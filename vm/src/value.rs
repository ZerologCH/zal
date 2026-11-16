//! Runtime values and JSON marshalling at the host boundary.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use serde_json::{json, Map as JMap, Value as J};
use zal_ir::{Program, Ty};

/// FxHash-style hasher: map keys are short trusted-by-construction strings
/// inside one run, so a fast non-cryptographic hash is fine. Iteration order
/// is never observable: `map_keys`, display and JSON sort by key.
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Same as zal-rt's FxHasher: whole words, tail from at most two reads.
        const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        let mut h = self.0;
        let mut rest = bytes;
        while rest.len() >= 8 {
            let w = u64::from_le_bytes(rest[..8].try_into().unwrap());
            h = (h.rotate_left(5) ^ w).wrapping_mul(K);
            rest = &rest[8..];
        }
        if !rest.is_empty() {
            let n = rest.len();
            let w = if n >= 4 {
                let lo = u32::from_le_bytes(rest[..4].try_into().unwrap()) as u64;
                let hi = u32::from_le_bytes(rest[n - 4..].try_into().unwrap()) as u64;
                lo | (hi << ((n - 4) * 8))
            } else {
                (rest[0] as u64) | ((rest[n / 2] as u64) << 8) | ((rest[n - 1] as u64) << 16)
            };
            h = (h.rotate_left(5) ^ w ^ ((n as u64) << 56)).wrapping_mul(K);
        }
        self.0 = h;
    }
    fn write_u8(&mut self, i: u8) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn finish(&self) -> u64 {
        // Fold high bits into low ones: the multiply step alone leaves the low
        // bits weak, and the table index comes from the low bits.
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^ (h >> 29)
    }
}

pub type ZMap = HashMap<Rc<str>, Value, BuildHasherDefault<FxHasher>>;

pub fn sorted_entries(m: &ZMap) -> Vec<(&Rc<str>, &Value)> {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| a.0.cmp(b.0));
    v
}

/// Runtime value. The layout is part of the native ABI (RFC 0002 §3): tag
/// byte at offset 0 in declaration order (Unit=0, Int=1, Bool=2, Str=3,
/// Dur=4, List=5, Map=6, then the runtime-private variants), payload at +8,
/// 24 bytes in total. Never reorder or insert variants before `Map`.
#[derive(Clone, Debug)]
#[repr(C, u8)]
pub enum Value {
    Unit,
    Int(i64),
    Bool(bool),
    Str(Rc<str>),
    Dur(i64),
    List(Rc<Vec<Value>>),
    Map(Rc<ZMap>),
    /// Fixed-size aggregates: one allocation (count header + elements).
    Tuple(Rc<[Value]>),
    Record(Rc<[Value]>),
    Variant(u16, Option<Rc<Value>>),
    Task(u32),
}

impl PartialEq for Value {
    fn eq(&self, o: &Value) -> bool {
        use Value::*;
        match (self, o) {
            (Unit, Unit) => true,
            (Int(a), Int(b)) | (Dur(a), Dur(b)) => a == b,
            (Bool(a), Bool(b)) => a == b,
            (Str(a), Str(b)) => a == b,
            (List(a), List(b)) => a == b,
            (Tuple(a), Tuple(b)) | (Record(a), Record(b)) => a == b,
            (Map(a), Map(b)) => a == b,
            (Variant(t1, p1), Variant(t2, p2)) => t1 == t2 && p1 == p2,
            (Task(a), Task(b)) => a == b,
            _ => false,
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Unit => write!(f, "()"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Dur(ms) => write!(f, "{ms}ms"),
            Value::List(_) | Value::Tuple(_) | Value::Record(_) => {
                let xs: &[Value] = match self {
                    Value::List(xs) => xs,
                    Value::Tuple(xs) | Value::Record(xs) => xs,
                    _ => unreachable!(),
                };
                let open = if matches!(self, Value::List(_)) { "[" } else { "(" };
                let close = if matches!(self, Value::List(_)) { "]" } else { ")" };
                write!(f, "{open}")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "{close}")
            }
            Value::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in sorted_entries(m).into_iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            Value::Variant(t, None) => write!(f, "#{t}"),
            Value::Variant(t, Some(p)) => write!(f, "#{t}({p})"),
            Value::Task(id) => write!(f, "<task {id}>"),
        }
    }
}

pub fn str_val(s: &str) -> Value {
    Value::Str(Rc::from(s))
}

/// Default value of a type (used by the mock host).
pub fn zero(ty: &Ty, p: &Program) -> Value {
    match ty {
        Ty::Int => Value::Int(0),
        Ty::Bool => Value::Bool(false),
        Ty::Str => str_val(""),
        Ty::Dur => Value::Dur(0),
        Ty::List(_) => Value::List(Rc::new(vec![])),
        Ty::Map(_) => Value::Map(Rc::new(ZMap::default())),
        Ty::Option(_) => Value::Variant(0, None),
        Ty::Result(a, _) => Value::Variant(0, Some(Rc::new(zero(a, p)))),
        Ty::Tuple(ts) => Value::Tuple(ts.iter().map(|t| zero(t, p)).collect()),
        // Crafted IR may name types it does not define: degrade, never panic.
        Ty::Record(n) => match p.record(n) {
            Some(r) => Value::Record(r.fields.iter().map(|(_, t)| zero(t, p)).collect()),
            None => Value::Unit,
        },
        Ty::Enum(n) => match p.enum_def(n).and_then(|e| e.variants.first()) {
            Some(v) => Value::Variant(0, v.1.as_ref().map(|t| Rc::new(zero(t, p)))),
            None => Value::Unit,
        },
        _ => Value::Unit,
    }
}

pub fn to_json(v: &Value, ty: &Ty, p: &Program) -> J {
    match (v, ty) {
        (Value::Unit, _) => J::Null,
        (Value::Int(n), _) | (Value::Dur(n), _) => json!(n),
        (Value::Bool(b), _) => json!(b),
        (Value::Str(s), _) => json!(&**s),
        (Value::List(xs), Ty::List(t)) => J::Array(xs.iter().map(|x| to_json(x, t, p)).collect()),
        (Value::Tuple(xs), Ty::Tuple(ts)) => J::Array(xs.iter().zip(ts).map(|(x, t)| to_json(x, t, p)).collect()),
        (Value::Map(m), Ty::Map(t)) => J::Object(sorted_entries(m).into_iter().map(|(k, x)| (k.to_string(), to_json(x, t, p))).collect()),
        (Value::Record(xs), Ty::Record(n)) if p.record(n).is_some() => {
            let r = p.record(n).unwrap();
            J::Object(r.fields.iter().zip(xs.iter()).map(|((f, t), x)| (f.clone(), to_json(x, t, p))).collect())
        }
        (Value::Variant(0, _), Ty::Option(_)) => J::Null,
        (Value::Variant(_, Some(x)), Ty::Option(t)) => to_json(x, t, p),
        (Value::Variant(tag, Some(x)), Ty::Result(a, b)) => {
            if *tag == 0 {
                json!({ "ok": to_json(x, a, p) })
            } else {
                json!({ "err": to_json(x, b, p) })
            }
        }
        (Value::Variant(tag, payload), Ty::Enum(n)) if p.enum_def(n).is_some_and(|e| (*tag as usize) < e.variants.len()) => {
            let (name, pt) = &p.enum_def(n).unwrap().variants[*tag as usize];
            match (payload, pt) {
                (Some(x), Some(t)) => json!({ "variant": name, "value": to_json(x, t, p) }),
                _ => json!(name),
            }
        }
        (v, _) => json!(v.to_string()),
    }
}

pub fn from_json(j: &J, ty: &Ty, p: &Program) -> Result<Value, String> {
    let bad = || format!("JSON {} does not match type {}", short(j), ty);
    Ok(match ty {
        Ty::Unit => Value::Unit,
        Ty::Int => Value::Int(j.as_i64().ok_or_else(bad)?),
        Ty::Dur => Value::Dur(j.as_i64().ok_or_else(bad)?),
        Ty::Bool => Value::Bool(j.as_bool().ok_or_else(bad)?),
        Ty::Str => str_val(j.as_str().ok_or_else(bad)?),
        Ty::List(t) => Value::List(Rc::new(j.as_array().ok_or_else(bad)?.iter().map(|x| from_json(x, t, p)).collect::<Result<_, _>>()?)),
        Ty::Tuple(ts) => {
            let a = j.as_array().filter(|a| a.len() == ts.len()).ok_or_else(bad)?;
            Value::Tuple(a.iter().zip(ts).map(|(x, t)| from_json(x, t, p)).collect::<Result<Vec<_>, _>>()?.into())
        }
        Ty::Map(t) => {
            let o = j.as_object().ok_or_else(bad)?;
            Value::Map(Rc::new(o.iter().map(|(k, x)| Ok((Rc::from(k.as_str()), from_json(x, t, p)?))).collect::<Result<_, String>>()?))
        }
        Ty::Record(n) => {
            let o = j.as_object().ok_or_else(bad)?;
            let r = p.record(n).ok_or_else(bad)?;
            let mut fs = Vec::new();
            for (f, t) in &r.fields {
                let x = o.get(f).ok_or_else(|| format!("missing field `{f}` for {n}"))?;
                fs.push(from_json(x, t, p)?);
            }
            Value::Record(fs.into())
        }
        Ty::Option(t) => {
            if j.is_null() {
                Value::Variant(0, None)
            } else {
                Value::Variant(1, Some(Rc::new(from_json(j, t, p)?)))
            }
        }
        Ty::Result(a, b) => {
            let o: &JMap<String, J> = j.as_object().ok_or_else(bad)?;
            if let Some(x) = o.get("ok") {
                Value::Variant(0, Some(Rc::new(from_json(x, a, p)?)))
            } else if let Some(x) = o.get("err") {
                Value::Variant(1, Some(Rc::new(from_json(x, b, p)?)))
            } else {
                return Err(bad());
            }
        }
        Ty::Enum(n) => {
            let e = p.enum_def(n).ok_or_else(bad)?;
            let (name, payload) = match j {
                J::String(s) => (s.as_str(), None),
                J::Object(o) => (o.get("variant").and_then(|v| v.as_str()).ok_or_else(bad)?, o.get("value")),
                _ => return Err(bad()),
            };
            let tag = e.variants.iter().position(|(v, _)| v == name).ok_or_else(bad)?;
            let pv = match (&e.variants[tag].1, payload) {
                (Some(t), Some(x)) => Some(Rc::new(from_json(x, t, p)?)),
                (None, None) => None,
                _ => return Err(bad()),
            };
            Value::Variant(tag as u16, pv)
        }
        _ => return Err(bad()),
    })
}

fn short(j: &J) -> String {
    let s = j.to_string();
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}

/// Move `n` values starting at `src` into a new `Rc<[Value]>` (a single
/// allocation; the source slots are left logically uninitialised).
///
/// # Safety
/// `src..src+n` are live values that the caller no longer drops.
pub(crate) unsafe fn rc_slice_take(src: *const Value, n: usize) -> Rc<[Value]> {
    let mut rc = Rc::<[Value]>::new_uninit_slice(n);
    let dst = Rc::get_mut(&mut rc).unwrap_unchecked();
    std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr() as *mut Value, n);
    rc.assume_init()
}

/// The elements of a tuple, moved out when this is the only reference.
pub(crate) fn rc_slice_each(mut xs: Rc<[Value]>, mut f: impl FnMut(Value)) {
    match Rc::get_mut(&mut xs) {
        Some(m) => {
            for x in m.iter_mut() {
                f(std::mem::replace(x, Value::Unit));
            }
        }
        None => {
            for x in xs.iter() {
                f(x.clone());
            }
        }
    }
}

/// `push` with copy-on-write. A shared list is copied into a buffer that
/// already has room for the new element (make_mut would copy with capacity =
/// length and then reallocate on the push itself).
#[inline]
pub(crate) fn list_push(xs: &mut Rc<Vec<Value>>, x: Value) {
    if let Some(v) = Rc::get_mut(xs) {
        v.push(x);
        return;
    }
    let mut v = Vec::with_capacity((xs.len() + 1).next_power_of_two().max(4));
    v.extend(xs.iter().cloned());
    v.push(x);
    *xs = Rc::new(v);
}

