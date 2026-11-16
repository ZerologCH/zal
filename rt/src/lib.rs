//! zal-rt: runtime for natively compiled ZAL programs (`zal native`).
//!
//! Same execution model as the VM: one thread, cooperative tasks, structured
//! cancellation, tasks that can be awaited many times, per-call capability
//! checks and budgets. Generated code is plain Rust using these primitives.

use std::cell::RefCell;

mod alloc;
pub use alloc::ZalAlloc;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::cmp::Reverse;
use std::future::Future;
use std::hash::{BuildHasherDefault, Hasher};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};

pub use serde_json::Value as Json;

// ------------------------------------------------------------------ values

pub type Str = Rc<str>;
pub type List<T> = Rc<Vec<T>>;
pub type Map<T> = Rc<HashMap<Str, T, BuildHasherDefault<FxHasher>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Dur(pub i64);

/// Runtime error: aborts the run (overflow, out-of-bounds, denied capability…).
#[derive(Clone, Debug, PartialEq)]
pub struct Trap(pub String);

pub fn trap<T>(m: impl Into<String>) -> Result<T, Trap> {
    Err(Trap(m.into()))
}

pub type Fut<T> = Pin<Box<dyn Future<Output = Result<T, Trap>>>>;

/// Same hasher as the VM (FxHash with a finalizer).
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        let mut h = self.0;
        let mut rest = bytes;
        while rest.len() >= 8 {
            let w = u64::from_le_bytes(rest[..8].try_into().unwrap());
            h = (h.rotate_left(5) ^ w).wrapping_mul(K);
            rest = &rest[8..];
        }
        // Tail (0..7 bytes): one word assembled from at most two reads.
        if !rest.is_empty() {
            let n = rest.len();
            let w = if n >= 4 {
                let lo = u32::from_le_bytes(rest[..4].try_into().unwrap()) as u64;
                let hi = u32::from_le_bytes(rest[n - 4..].try_into().unwrap()) as u64;
                lo | (hi << ((n - 4) * 8))
            } else {
                // 1..3 bytes: first, middle and last byte.
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
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^ (h >> 29)
    }
}

pub fn s(x: &str) -> Str {
    Rc::from(x)
}

pub fn add_i(a: i64, b: i64) -> Result<i64, Trap> {
    a.checked_add(b).ok_or_else(|| Trap("integer overflow in +".into()))
}
pub fn sub_i(a: i64, b: i64) -> Result<i64, Trap> {
    a.checked_sub(b).ok_or_else(|| Trap("integer - overflow or division by zero".into()))
}
pub fn mul_i(a: i64, b: i64) -> Result<i64, Trap> {
    a.checked_mul(b).ok_or_else(|| Trap("integer * overflow or division by zero".into()))
}
pub fn div_i(a: i64, b: i64) -> Result<i64, Trap> {
    a.checked_div(b).ok_or_else(|| Trap("integer / overflow or division by zero".into()))
}
pub fn rem_i(a: i64, b: i64) -> Result<i64, Trap> {
    a.checked_rem(b).ok_or_else(|| Trap("integer % overflow or division by zero".into()))
}
// Same as the VM: Dur `+` saturates, `-` traps on overflow.
pub fn add_d(a: Dur, b: Dur) -> Dur {
    Dur(a.0.saturating_add(b.0))
}
pub fn sub_d(a: Dur, b: Dur) -> Result<Dur, Trap> {
    a.0.checked_sub(b.0).map(Dur).ok_or_else(|| Trap("duration - overflow".into()))
}

/// Default value of a type, used by the mock host (as the VM's `zero`).
pub trait ZZero {
    fn zero() -> Self;
}
impl ZZero for i64 { fn zero() -> Self { 0 } }
impl ZZero for bool { fn zero() -> Self { false } }
impl ZZero for Str { fn zero() -> Self { Rc::from("") } }
impl ZZero for Dur { fn zero() -> Self { Dur(0) } }
impl ZZero for () { fn zero() -> Self {} }
impl<T> ZZero for List<T> { fn zero() -> Self { Rc::new(Vec::new()) } }
impl<T> ZZero for Map<T> { fn zero() -> Self { Rc::new(HashMap::default()) } }
impl<T> ZZero for Option<T> { fn zero() -> Self { None } }
impl<T: ZZero, E> ZZero for Result<T, E> { fn zero() -> Self { Ok(T::zero()) } }
impl<A: ZZero, B: ZZero> ZZero for (A, B) { fn zero() -> Self { (A::zero(), B::zero()) } }
impl<A: ZZero, B: ZZero, C: ZZero> ZZero for (A, B, C) { fn zero() -> Self { (A::zero(), B::zero(), C::zero()) } }

thread_local! {
    /// Nesting of eager first polls (see `spawn_future`).
    static EAGER: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static TICKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Call-depth guard, same limit as the VM (10 000 frames).
pub struct Depth;
impl Drop for Depth {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}
pub fn enter() -> Result<Depth, Trap> {
    // Calls tick like the VM does, so deep or mutual recursion without loops
    // still sees the budget deadline.
    tick()?;
    DEPTH.with(|d| {
        let n = d.get() + 1;
        if n > 10_000 {
            return trap("call depth limit exceeded (10000 frames)");
        }
        d.set(n);
        Ok(Depth)
    })
}

/// Loop back-edge: checks the budget deadline every 512 iterations (as the VM).
/// The counter bump is inlined at every loop; the check is out of line.
#[inline(always)]
pub fn tick() -> Result<(), Trap> {
    let n = TICKS.with(|t| {
        let n = t.get().wrapping_add(1);
        t.set(n);
        n
    });
    if n % 512 == 0 {
        tick_slow()
    } else {
        Ok(())
    }
}

#[cold]
#[inline(never)]
fn tick_slow() -> Result<(), Trap> {
    RT.with(|rt| {
        let rt = rt.borrow();
        if rt.deadline.is_some_and(|d| Instant::now() >= d) {
            return trap(format!("budget exceeded: timeout of {} ms", rt.timeout_ms));
        }
        Ok(())
    })
}

/// Unconditional deadline check (retry loops, which may run few iterations
/// but each one long).
pub fn check_deadline() -> Result<(), Trap> {
    RT.with(|rt| {
        let rt = rt.borrow();
        if rt.deadline.is_some_and(|d| Instant::now() >= d) {
            return trap(format!("budget exceeded: timeout of {} ms", rt.timeout_ms));
        }
        Ok(())
    })
}

/// `a + b + ...` over strings in one allocation (directly into the Rc).
pub fn concat(parts: &[&str]) -> Str {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut buf: Rc<[std::mem::MaybeUninit<u8>]> = Rc::new_uninit_slice(total);
    let dst = Rc::get_mut(&mut buf).expect("fresh Rc is unique");
    let mut at = 0;
    for p in parts {
        // SAFETY: `at + p.len() <= total` by construction; source and
        // destination do not overlap (fresh allocation).
        unsafe {
            std::ptr::copy_nonoverlapping(p.as_ptr(), dst.as_mut_ptr().add(at) as *mut u8, p.len());
        }
        at += p.len();
    }
    // SAFETY: every byte was written above, and the bytes are the
    // concatenation of valid UTF-8 strings, which is valid UTF-8.
    unsafe {
        let bytes: Rc<[u8]> = buf.assume_init();
        Rc::from_raw(Rc::into_raw(bytes) as *const str)
    }
}

/// `e?` without knowing statically whether `e` is a Result or an Option.
pub enum Flow<B, C> {
    Break(B),
    Continue(C),
}
pub struct ResErr<E>(pub E);
pub struct ResNone;
pub trait ZTry {
    type Out;
    type Res;
    fn branch(self) -> Flow<Self::Res, Self::Out>;
}
impl<T, E> ZTry for Result<T, E> {
    type Out = T;
    type Res = ResErr<E>;
    fn branch(self) -> Flow<ResErr<E>, T> {
        match self {
            Ok(v) => Flow::Continue(v),
            Err(e) => Flow::Break(ResErr(e)),
        }
    }
}
impl<T> ZTry for Option<T> {
    type Out = T;
    type Res = ResNone;
    fn branch(self) -> Flow<ResNone, T> {
        match self {
            Some(v) => Flow::Continue(v),
            None => Flow::Break(ResNone),
        }
    }
}
pub trait FromRes<R> {
    fn from_res(r: R) -> Self;
}
impl<T, E> FromRes<ResErr<E>> for Result<T, E> {
    fn from_res(r: ResErr<E>) -> Self {
        Err(r.0)
    }
}
impl<T> FromRes<ResNone> for Option<T> {
    fn from_res(_: ResNone) -> Self {
        None
    }
}

pub trait ZLen {
    fn zlen(&self) -> i64;
}
impl<T> ZLen for List<T> {
    fn zlen(&self) -> i64 {
        self.len() as i64
    }
}
impl<T> ZLen for Map<T> {
    fn zlen(&self) -> i64 {
        self.len() as i64
    }
}
impl ZLen for Str {
    fn zlen(&self) -> i64 {
        self.chars().count() as i64
    }
}

pub fn index<T: Clone>(xs: &List<T>, i: i64) -> Result<T, Trap> {
    match usize::try_from(i).ok().and_then(|i| xs.get(i)) {
        Some(v) => Ok(v.clone()),
        None => trap(format!("index {i} out of bounds (length {})", xs.len())),
    }
}
/// `xs[i]` by reference, for field access and reads (no refcount traffic).
pub fn index_ref<T>(xs: &List<T>, i: i64) -> Result<&T, Trap> {
    match usize::try_from(i).ok().and_then(|i| xs.get(i)) {
        Some(v) => Ok(v),
        None => trap(format!("index {i} out of bounds (length {})", xs.len())),
    }
}

pub fn get<T: Clone>(xs: &List<T>, i: i64) -> Option<T> {
    usize::try_from(i).ok().and_then(|i| xs.get(i)).cloned()
}
pub fn push<T: Clone>(mut xs: List<T>, x: T) -> List<T> {
    Rc::make_mut(&mut xs).push(x);
    xs
}
pub fn set<T: Clone>(mut xs: List<T>, i: i64, x: T) -> Result<List<T>, Trap> {
    let len = xs.len();
    match usize::try_from(i).ok().filter(|i| *i < len) {
        Some(i) => {
            Rc::make_mut(&mut xs)[i] = x;
            Ok(xs)
        }
        None => trap(format!("set: index {i} out of bounds (length {len})")),
    }
}
pub fn remove<T: Clone>(mut xs: List<T>, i: i64) -> Result<List<T>, Trap> {
    let len = xs.len();
    match usize::try_from(i).ok().filter(|i| *i < len) {
        Some(i) => {
            Rc::make_mut(&mut xs).remove(i);
            Ok(xs)
        }
        None => trap(format!("remove: index {i} out of bounds (length {len})")),
    }
}
/// In place `xs = push(xs, x)` (`xs` is the variable itself, no placeholder).
#[inline]
pub fn push_mut<T: Clone>(xs: &mut List<T>, x: T) {
    Rc::make_mut(xs).push(x);
}
/// In place `xs = set(xs, i, x)`.
#[inline]
pub fn set_mut<T: Clone>(xs: &mut List<T>, i: i64, x: T) -> Result<(), Trap> {
    let len = xs.len();
    match usize::try_from(i).ok().filter(|i| *i < len) {
        Some(i) => {
            Rc::make_mut(xs)[i] = x;
            Ok(())
        }
        None => trap(format!("set: index {i} out of bounds (length {len})")),
    }
}
/// In place `xs = remove(xs, i)`.
#[inline]
pub fn remove_mut<T: Clone>(xs: &mut List<T>, i: i64) -> Result<(), Trap> {
    let len = xs.len();
    match usize::try_from(i).ok().filter(|i| *i < len) {
        Some(i) => {
            Rc::make_mut(xs).remove(i);
            Ok(())
        }
        None => trap(format!("remove: index {i} out of bounds (length {len})")),
    }
}
/// In place `m = map_set(m, k, v)`.
#[inline]
pub fn map_set_mut<T: Clone>(m: &mut Map<T>, k: Str, v: T) {
    Rc::make_mut(m).insert(k, v);
}
/// In place: `xs = set(xs, i, push(xs[i], v))` for a list of lists.
pub fn push_at<T: Clone>(xs: &mut List<List<T>>, i: i64, v: T) -> Result<(), Trap> {
    let len = xs.len();
    match usize::try_from(i).ok().filter(|i| *i < len) {
        Some(k) => {
            Rc::make_mut(&mut Rc::make_mut(xs)[k]).push(v);
            Ok(())
        }
        None => trap(format!("index {i} out of bounds (length {len})")),
    }
}

pub fn range(n: i64) -> List<i64> {
    Rc::new((0..n.max(0)).collect())
}
pub fn list<T>(v: Vec<T>) -> List<T> {
    Rc::new(v)
}
pub fn map_new<T>() -> Map<T> {
    Rc::new(HashMap::default())
}
pub fn map_with_capacity<T>(n: i64) -> Map<T> {
    Rc::new(HashMap::with_capacity_and_hasher(n.clamp(0, 1 << 24) as usize, Default::default()))
}
pub fn map_get<T: Clone>(m: &Map<T>, k: &Str) -> Option<T> {
    m.get(k).cloned()
}
pub fn map_set<T: Clone>(mut m: Map<T>, k: Str, v: T) -> Map<T> {
    Rc::make_mut(&mut m).insert(k, v);
    m
}
pub fn map_has<T>(m: &Map<T>, k: &Str) -> bool {
    m.contains_key(k)
}
pub fn map_keys<T>(m: &Map<T>) -> List<Str> {
    let mut k: Vec<Str> = m.keys().cloned().collect();
    k.sort();
    Rc::new(k)
}
pub fn json_str(x: &Str) -> Str {
    Rc::from(serde_json::to_string(&**x).unwrap_or_default())
}
pub fn json_get_str(doc: &Str, field: &Str) -> Option<Str> {
    serde_json::from_str::<Json>(doc).ok()?.get(&**field)?.as_str().map(Rc::from)
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `str(x)`: same text as the VM's display of the value.
pub trait ZStr {
    fn zstr(&self) -> String;
}
impl ZStr for i64 {
    fn zstr(&self) -> String {
        self.to_string()
    }
}
impl ZStr for bool {
    fn zstr(&self) -> String {
        self.to_string()
    }
}
impl ZStr for Str {
    fn zstr(&self) -> String {
        self.to_string()
    }
}
impl ZStr for Dur {
    fn zstr(&self) -> String {
        format!("{}ms", self.0)
    }
}
impl ZStr for () {
    fn zstr(&self) -> String {
        "()".into()
    }
}
impl<T: ZStr> ZStr for List<T> {
    fn zstr(&self) -> String {
        format!("[{}]", self.iter().map(|x| x.zstr()).collect::<Vec<_>>().join(", "))
    }
}
impl<T: ZStr> ZStr for Option<T> {
    fn zstr(&self) -> String {
        match self {
            None => "#0".into(),
            Some(x) => format!("#1({})", x.zstr()),
        }
    }
}
impl<T: ZStr, E: ZStr> ZStr for Result<T, E> {
    fn zstr(&self) -> String {
        match self {
            Ok(x) => format!("#0({})", x.zstr()),
            Err(e) => format!("#1({})", e.zstr()),
        }
    }
}
impl<T: ZStr> ZStr for Map<T> {
    fn zstr(&self) -> String {
        let mut keys: Vec<&Str> = self.keys().collect();
        keys.sort();
        format!("{{{}}}", keys.iter().map(|k| format!("{}: {}", k, self[*k].zstr())).collect::<Vec<_>>().join(", "))
    }
}
impl<A: ZStr, B: ZStr> ZStr for (A, B) {
    fn zstr(&self) -> String {
        format!("({}, {})", self.0.zstr(), self.1.zstr())
    }
}
impl<A: ZStr, B: ZStr, C: ZStr> ZStr for (A, B, C) {
    fn zstr(&self) -> String {
        format!("({}, {}, {})", self.0.zstr(), self.1.zstr(), self.2.zstr())
    }
}
impl<T> ZStr for Task<T> {
    fn zstr(&self) -> String {
        format!("<task {}>", self.id)
    }
}

pub fn to_str<T: ZStr>(x: &T) -> Str {
    Rc::from(x.zstr())
}

// ------------------------------------------------------------------ JSON at the boundary

pub trait FromJson: Sized {
    fn from_json(j: &Json) -> Result<Self, String>;
}
pub trait ToJson {
    fn to_json(&self) -> Json;
}
fn bad<T>(what: &str, j: &Json) -> Result<T, String> {
    Err(format!("JSON {j} is not a {what}"))
}
impl FromJson for i64 {
    fn from_json(j: &Json) -> Result<Self, String> {
        j.as_i64().map_or_else(|| bad("Int", j), Ok)
    }
}
impl FromJson for bool {
    fn from_json(j: &Json) -> Result<Self, String> {
        j.as_bool().map_or_else(|| bad("Bool", j), Ok)
    }
}
impl FromJson for Str {
    fn from_json(j: &Json) -> Result<Self, String> {
        j.as_str().map_or_else(|| bad("Str", j), |x| Ok(Rc::from(x)))
    }
}
impl FromJson for Dur {
    fn from_json(j: &Json) -> Result<Self, String> {
        j.as_i64().map_or_else(|| bad("Dur", j), |x| Ok(Dur(x)))
    }
}
impl FromJson for () {
    fn from_json(_: &Json) -> Result<Self, String> {
        Ok(())
    }
}
impl<T: FromJson> FromJson for List<T> {
    fn from_json(j: &Json) -> Result<Self, String> {
        let a = j.as_array().ok_or_else(|| format!("JSON {j} is not a List"))?;
        Ok(Rc::new(a.iter().map(T::from_json).collect::<Result<_, _>>()?))
    }
}
impl<T: FromJson> FromJson for Map<T> {
    fn from_json(j: &Json) -> Result<Self, String> {
        let o = j.as_object().ok_or_else(|| format!("JSON {j} is not a Map"))?;
        let mut m = HashMap::default();
        for (k, v) in o {
            m.insert(Rc::from(k.as_str()), T::from_json(v)?);
        }
        Ok(Rc::new(m))
    }
}
impl<T: FromJson> FromJson for Option<T> {
    fn from_json(j: &Json) -> Result<Self, String> {
        if j.is_null() {
            Ok(None)
        } else {
            Ok(Some(T::from_json(j)?))
        }
    }
}
impl<T: FromJson, E: FromJson> FromJson for Result<T, E> {
    fn from_json(j: &Json) -> Result<Self, String> {
        if let Some(v) = j.get("ok") {
            Ok(Ok(T::from_json(v)?))
        } else if let Some(v) = j.get("err") {
            Ok(Err(E::from_json(v)?))
        } else {
            bad("Result", j)
        }
    }
}
impl ToJson for i64 {
    fn to_json(&self) -> Json {
        Json::from(*self)
    }
}
impl ToJson for bool {
    fn to_json(&self) -> Json {
        Json::from(*self)
    }
}
impl ToJson for Str {
    fn to_json(&self) -> Json {
        Json::from(&**self)
    }
}
impl ToJson for Dur {
    fn to_json(&self) -> Json {
        Json::from(self.0)
    }
}
impl ToJson for () {
    fn to_json(&self) -> Json {
        Json::Null
    }
}
impl<T: ToJson> ToJson for List<T> {
    fn to_json(&self) -> Json {
        Json::Array(self.iter().map(|x| x.to_json()).collect())
    }
}
impl<T: ToJson> ToJson for Map<T> {
    fn to_json(&self) -> Json {
        let mut keys: Vec<&Str> = self.keys().collect();
        keys.sort();
        Json::Object(keys.into_iter().map(|k| (k.to_string(), self[k].to_json())).collect())
    }
}
impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self) -> Json {
        match self {
            None => Json::Null,
            Some(x) => x.to_json(),
        }
    }
}
impl<T: ToJson, E: ToJson> ToJson for Result<T, E> {
    fn to_json(&self) -> Json {
        match self {
            Ok(x) => serde_json::json!({ "ok": x.to_json() }),
            Err(e) => serde_json::json!({ "err": e.to_json() }),
        }
    }
}
impl<A: ToJson, B: ToJson> ToJson for (A, B) {
    fn to_json(&self) -> Json {
        Json::Array(vec![self.0.to_json(), self.1.to_json()])
    }
}
impl<A: ToJson, B: ToJson, C: ToJson> ToJson for (A, B, C) {
    fn to_json(&self) -> Json {
        Json::Array(vec![self.0.to_json(), self.1.to_json(), self.2.to_json()])
    }
}
impl<A: FromJson, B: FromJson> FromJson for (A, B) {
    fn from_json(j: &Json) -> Result<Self, String> {
        match j.as_array().map(|a| a.as_slice()) {
            Some([a, b]) => Ok((A::from_json(a)?, B::from_json(b)?)),
            _ => bad("2-tuple", j),
        }
    }
}
impl<A: FromJson, B: FromJson, C: FromJson> FromJson for (A, B, C) {
    fn from_json(j: &Json) -> Result<Self, String> {
        match j.as_array().map(|a| a.as_slice()) {
            Some([a, b, c]) => Ok((A::from_json(a)?, B::from_json(b)?, C::from_json(c)?)),
            _ => bad("3-tuple", j),
        }
    }
}

// ------------------------------------------------------------------ executor

type TaskFut = Pin<Box<dyn Future<Output = ()>>>;

trait Cancellable {
    fn mark_cancelled(&self);
}

impl<T> Cancellable for RefCell<Slot<T>> {
    fn mark_cancelled(&self) {
        let waiters = {
            let mut s = self.borrow_mut();
            s.cancelled = true;
            std::mem::take(&mut s.waiters)
        };
        for w in waiters {
            w.wake();
        }
    }
}

struct TaskEntry {
    fut: Option<TaskFut>,
    /// Children as (id, generation): an id is reused once its task is done.
    children: Vec<(usize, u32)>,
    /// Bumped on every reuse of the id; stale handles and child entries no
    /// longer match, so they can never cancel a newer task.
    gen: u32,
    /// The task's result slot, to mark it cancelled (shares the slot's allocation).
    on_cancel: Option<Rc<dyn Cancellable>>,
    queued: bool,
    done: bool,
    /// The run's root future: polled in place by `run`, never boxed as a task.
    root: bool,
    /// Call depth of this task while it is suspended (depth is per task, as per fiber in the VM).
    depth: u32,
}

#[derive(Default)]
struct Rt {
    tasks: Vec<TaskEntry>,
    /// Ids of finished tasks, reused by `spawn` (keeps the task table and the
    /// futures' memory small and cache-hot instead of growing per spawn).
    free: Vec<usize>,
    ready: VecDeque<usize>,
    timers: BinaryHeap<Reverse<(Instant, u64, usize)>>,
    timer_seq: u64,
    current: Option<usize>,
    // capabilities and budgets
    caps: Vec<bool>,
    effects: Vec<&'static str>,
    calls: u64,
    max_calls: Option<u64>,
    deadline: Option<Instant>,
    timeout_ms: i64,
    max_parallel: Option<u32>,
    inflight: u32,
    slot_waiters: VecDeque<Waker>,
    last_grants: Option<Vec<String>>,
    last_effects: Vec<&'static str>,
}

thread_local! {
    static RT: RefCell<Rt> = RefCell::new(Rt::default());
    /// Fast path for `authorize`: granted effects (first 64) and whether any
    /// budget needs checking; kept in sync by `configure`.
    static FAST_CAPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FAST_NO_BUDGET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAST_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Entry (address of its declared-effects list) already preflighted under
    /// the current grants; cleared by `configure`.
    static PREFLIGHT_OK: std::cell::Cell<(usize, usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0, 0)) };
}

fn waker_for(id: usize) -> Waker {
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    fn wake(p: *const ()) {
        wake_by_ref(p)
    }
    fn wake_by_ref(p: *const ()) {
        let id = p as usize;
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if let Some(t) = rt.tasks.get_mut(id) {
                if !t.queued && !t.done && (t.fut.is_some() || t.root) {
                    t.queued = true;
                    rt.ready.push_back(id);
                }
            }
        })
    }
    fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    // Only ever used on the runtime's own thread.
    unsafe { Waker::from_raw(RawWaker::new(id as *const (), &VTABLE)) }
}

struct Slot<T> {
    result: Option<Result<T, Trap>>,
    cancelled: bool,
    waiters: Vec<Waker>,
}

/// Handle of a spawned task; can be awaited any number of times.
pub struct Task<T> {
    id: usize,
    gen: u32,
    slot: Rc<RefCell<Slot<T>>>,
}

impl<T> Clone for Task<T> {
    fn clone(&self) -> Self {
        Task { id: self.id, gen: self.gen, slot: self.slot.clone() }
    }
}

impl<T> PartialEq for Task<T> {
    fn eq(&self, o: &Self) -> bool {
        Rc::ptr_eq(&self.slot, &o.slot)
    }
}

pub fn spawn<T: 'static>(f: Fut<T>) -> Task<T> {
    spawn_future(f)
}

/// Spawn any future (one allocation for the task itself).
pub fn spawn_future<T: 'static, F: Future<Output = Result<T, Trap>> + 'static>(f: F) -> Task<T> {
    let slot = Rc::new(RefCell::new(Slot { result: None, cancelled: false, waiters: vec![] }));
    let s2 = slot.clone();
    let wrapped: TaskFut = Box::pin(async move {
        let r = f.await;
        let waiters = {
            let mut s = s2.borrow_mut();
            s.result = Some(r);
            std::mem::take(&mut s.waiters)
        };
        for w in waiters {
            w.wake();
        }
    });
    let on_cancel: Rc<dyn Cancellable> = slot.clone();
    // Eager start: a task spawned by a running task is polled once right
    // away, under its own id and waker. One that finishes then (every host
    // call answered at once) never enters the ready queue. The nesting of
    // such inline polls is bounded so spawn chains cannot exhaust the stack.
    let eager = EAGER.with(|e| e.get()) < 32 && RT.with(|rt| rt.borrow().current.is_some());
    let mut wrapped = Some(wrapped);
    let (id, gen) = RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        let fut = if eager { None } else { wrapped.take() };
        let (id, gen) = match rt.free.pop() {
            Some(id) => {
                let t = &mut rt.tasks[id];
                t.gen = t.gen.wrapping_add(1);
                t.fut = fut;
                t.children.clear();
                t.on_cancel = Some(on_cancel);
                t.queued = !eager;
                t.done = false;
                t.depth = 0;
                (id, t.gen)
            }
            None => {
                rt.tasks.push(TaskEntry { fut, children: vec![], gen: 0, on_cancel: Some(on_cancel), queued: !eager, done: false, root: false, depth: 0 });
                (rt.tasks.len() - 1, 0)
            }
        };
        if let Some(p) = rt.current {
            rt.tasks[p].children.push((id, gen));
        }
        if !eager {
            rt.ready.push_back(id);
        }
        (id, gen)
    });
    if let Some(mut fut) = wrapped {
        let w = waker_for(id);
        let mut cx = Context::from_waker(&w);
        let parent = RT.with(|rt| rt.borrow_mut().current.replace(id));
        let parent_depth = DEPTH.with(|d| d.replace(0));
        EAGER.with(|e| e.set(e.get() + 1));
        let done = fut.as_mut().poll(&mut cx).is_ready();
        EAGER.with(|e| e.set(e.get() - 1));
        let depth = DEPTH.with(|d| d.replace(parent_depth));
        let children = RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            rt.current = parent;
            let t = &mut rt.tasks[id];
            if done {
                t.done = true;
                t.on_cancel = None;
                let c = std::mem::take(&mut t.children);
                rt.free.push(id);
                c
            } else {
                t.fut = Some(fut);
                t.depth = depth;
                vec![]
            }
        });
        // Structured concurrency, as in the run loop: a finished task takes
        // its unawaited children with it.
        for (c, g) in children {
            cancel_id(c, g);
        }
    }
    Task { id, gen, slot }
}

fn cancel_id(id: usize, gen: u32) {
    let (fut, children, on_cancel) = RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        let t = &mut rt.tasks[id];
        if t.done || t.gen != gen {
            return (None, vec![], None);
        }
        t.done = true;
        let r = (t.fut.take(), std::mem::take(&mut t.children), t.on_cancel.take());
        if !t.root {
            rt.free.push(id);
        }
        r
    });
    // Dropping the future cancels any in-flight host call it owns.
    drop(fut);
    for (c, g) in children {
        cancel_id(c, g);
    }
    if let Some(c) = on_cancel {
        c.mark_cancelled();
    }
}

pub fn cancel<T>(t: &Task<T>) {
    if t.slot.borrow().result.is_none() {
        cancel_id(t.id, t.gen);
    }
}

impl<T: Clone> Task<T> {
    pub fn wait(&self) -> TaskWait<T> {
        TaskWait { t: self.clone() }
    }
}

pub struct TaskWait<T> {
    t: Task<T>,
}

impl<T: Clone> Future for TaskWait<T> {
    type Output = Result<T, Trap>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = self.t.slot.borrow_mut();
        if let Some(r) = &s.result {
            return Poll::Ready(r.clone());
        }
        if s.cancelled {
            return Poll::Ready(trap("awaited a cancelled task"));
        }
        s.waiters.push(cx.waker().clone());
        Poll::Pending
    }
}

/// `await_any(ts)`: index and value of the first finished task (cancelled ones skipped).
pub fn await_any<T: Clone>(ts: &List<Task<T>>) -> impl Future<Output = Result<(i64, T), Trap>> {
    let ts = ts.clone();
    std::future::poll_fn(move |cx| {
        if ts.is_empty() {
            return Poll::Ready(trap("await_any on an empty list"));
        }
        let mut live = false;
        for (i, t) in ts.iter().enumerate() {
            let s = t.slot.borrow();
            if let Some(r) = &s.result {
                return Poll::Ready(r.clone().map(|v| (i as i64, v)));
            }
            if !s.cancelled {
                live = true;
            }
        }
        if !live {
            return Poll::Ready(trap("await_any: every task in the list was cancelled"));
        }
        for t in ts.iter() {
            t.slot.borrow_mut().waiters.push(cx.waker().clone());
        }
        Poll::Pending
    })
}

pub async fn join<T: Clone>(ts: List<Task<T>>) -> Result<List<T>, Trap> {
    let mut out = Vec::with_capacity(ts.len());
    for t in ts.iter() {
        if t.slot.borrow().cancelled {
            return trap("joined a cancelled task");
        }
        out.push(t.wait().await.map_err(|e| if e.0 == "awaited a cancelled task" { Trap("joined a cancelled task".into()) } else { e })?);
    }
    Ok(Rc::new(out))
}

/// Await every task of a wide `parallel` (one suspension point for the caller).
pub async fn wait_all<T: Clone>(ts: Vec<Task<T>>) -> Result<Vec<T>, Trap> {
    let mut out = Vec::with_capacity(ts.len());
    for t in &ts {
        out.push(t.wait().await?);
    }
    Ok(out)
}

/// First finished of the given tasks wins; the others are cancelled.
pub async fn race<T: Clone + 'static>(ts: Vec<Task<T>>) -> Result<T, Trap> {
    let list = Rc::new(ts);
    let (i, v) = await_any(&list).await?;
    for (k, t) in list.iter().enumerate() {
        if k as i64 != i {
            cancel(t);
        }
    }
    Ok(v)
}

pub fn sleep(d: Dur) -> impl Future<Output = Result<(), Trap>> {
    let deadline = Instant::now() + Duration::from_millis(d.0.max(0) as u64);
    let mut armed = false;
    std::future::poll_fn(move |_cx| {
        if Instant::now() >= deadline {
            return Poll::Ready(Ok(()));
        }
        if !armed {
            armed = true;
            RT.with(|rt| {
                let mut rt = rt.borrow_mut();
                let id = rt.current.expect("sleep inside a task");
                rt.timer_seq += 1;
                let seq = rt.timer_seq;
                rt.timers.push(Reverse((deadline, seq, id)));
            });
        }
        Poll::Pending
    })
}

/// `timeout(d) { body }`: Some(v) if the task finishes in time, else cancel and None.
pub async fn timeout<T: Clone + 'static>(d: Dur, t: Task<T>) -> Result<Option<T>, Trap> {
    let deadline = Instant::now() + Duration::from_millis(d.0.max(0) as u64);
    let mut armed = false;
    let r = std::future::poll_fn(|cx| {
        {
            let mut s = t.slot.borrow_mut();
            if let Some(r) = &s.result {
                return Poll::Ready(Some(r.clone()));
            }
            if s.cancelled {
                return Poll::Ready(None);
            }
            s.waiters.push(cx.waker().clone());
        }
        if armed && Instant::now() >= deadline {
            return Poll::Ready(None);
        }
        if !armed {
            armed = true;
            RT.with(|rt| {
                let mut rt = rt.borrow_mut();
                let id = rt.current.expect("timeout inside a task");
                rt.timer_seq += 1;
                let seq = rt.timer_seq;
                rt.timers.push(Reverse((deadline, seq, id)));
            });
        }
        Poll::Pending
    })
    .await;
    match r {
        Some(v) => v.map(Some),
        None => {
            cancel(&t);
            Ok(None)
        }
    }
}

// ------------------------------------------------------------------ capabilities and budgets

/// Configure a run: grants from the policy (exact or `kind.*`), declared effects
/// per extern index, and the entry workflow's budget.
pub fn configure(effects: &[&'static str], grants: &[String], timeout_ms: Option<i64>, max_parallel: Option<u32>, max_calls: Option<u64>) {
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        rt.effects = effects.to_vec();
        rt.caps = effects
            .iter()
            .map(|e| grants.iter().any(|g| g == e || (g.ends_with(".*") && e.starts_with(&g[..g.len() - 1]))))
            .collect();
        rt.calls = 0;
        rt.max_calls = max_calls;
        rt.timeout_ms = timeout_ms.unwrap_or(0);
        rt.deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms.max(0) as u64));
        rt.max_parallel = max_parallel;
        rt.inflight = 0;
        let mut mask = 0u64;
        for (i, c) in rt.caps.iter().enumerate().take(64) {
            if *c {
                mask |= 1 << i;
            }
        }
        FAST_CAPS.with(|m| m.set(mask));
        FAST_NO_BUDGET.with(|b| b.set(max_calls.is_none() && timeout_ms.is_none() && rt.effects.len() <= 64));
        FAST_CALLS.with(|c| c.set(0));
        // The preflight decision depends only on effects and grants.
        if rt.last_grants.as_deref() != Some(grants) || rt.last_effects != effects {
            PREFLIGHT_OK.with(|p| p.set((0, 0, 0, 0)));
            rt.last_grants = Some(grants.to_vec());
            rt.last_effects = effects.to_vec();
        }
    })
}

/// Preflight: every concrete effect the entry declares must be granted.
pub fn preflight(declared: &[&str], grants: &[String]) -> Result<(), Trap> {
    // Same entry, same grants since the last `configure`: same decision.
    // Keyed by both slices (address and length): a different grant list or
    // a sub-slice must not reuse an earlier decision.
    let key = (declared.as_ptr() as usize, declared.len(), grants.as_ptr() as usize, grants.len());
    if key.0 != 0 && PREFLIGHT_OK.with(|p| p.get()) == key {
        return Ok(());
    }
    for e in declared {
        if e.ends_with(".*") {
            continue;
        }
        if !grants.iter().any(|g| g == e || (g.ends_with(".*") && e.starts_with(&g[..g.len() - 1]))) {
            return trap(format!("preflight denied: capability `{e}` not granted"));
        }
    }
    PREFLIGHT_OK.with(|p| p.set(key));
    Ok(())
}

/// Authorization and budget check for one host call (fail closed).
pub fn authorize(effect: usize, name: &str) -> Result<(), Trap> {
    // Fast path: no budgets to count, decision from the granted-effect mask.
    if FAST_NO_BUDGET.with(|b| b.get()) {
        FAST_CALLS.with(|c| c.set(c.get() + 1));
        if effect < 64 && FAST_CAPS.with(|m| m.get()) & (1 << effect) != 0 {
            return Ok(());
        }
    }
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        rt.calls += 1;
        if let Some(m) = rt.max_calls {
            if rt.calls > m {
                return trap(format!("budget exceeded: more than {m} tool calls"));
            }
        }
        if rt.deadline.is_some_and(|d| Instant::now() >= d) {
            return trap(format!("budget exceeded: timeout of {} ms", rt.timeout_ms));
        }
        if !rt.caps.get(effect).copied().unwrap_or(false) {
            let e = rt.effects.get(effect).copied().unwrap_or("?");
            return trap(format!("capability denied for `{name}`: capability `{e}` not granted"));
        }
        Ok(())
    })
}

/// In-flight slot for one host call (budget `max_parallel`); released on drop.
pub struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        // try_with: a permit can be dropped during thread teardown (after a trap).
        let w = RT
            .try_with(|rt| {
                let mut rt = rt.borrow_mut();
                rt.inflight = rt.inflight.saturating_sub(1);
                // Wake every waiter: the front one may belong to a cancelled
                // task, and waking only it would strand the live ones. Losers
                // re-register on their next poll.
                std::mem::take(&mut rt.slot_waiters)
            })
            .unwrap_or_default();
        for w in w {
            w.wake();
        }
    }
}

/// True when a `max_parallel` budget is active (host calls need a slot).
pub fn limited() -> bool {
    RT.with(|rt| rt.borrow().max_parallel.is_some())
}

/// In-flight slot; without a `max_parallel` budget this completes immediately.
pub fn permit() -> impl Future<Output = Permit> {
    std::future::poll_fn(|cx| {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if rt.max_parallel.is_none_or(|m| rt.inflight < m) {
                rt.inflight += 1;
                Poll::Ready(Permit)
            } else {
                rt.slot_waiters.push_back(cx.waker().clone());
                Poll::Pending
            }
        })
    })
}

// ------------------------------------------------------------------ run loop

thread_local! {
    static IDLE: RefCell<Option<Box<dyn FnMut() -> bool>>> = RefCell::new(None);
}

/// Install the host's idle hook: called whenever no task is ready, before the
/// executor waits for timers. It returns true when it did work that may have
/// woken tasks (for example a group commit releasing the writers).
pub fn set_idle(f: Option<Box<dyn FnMut() -> bool>>) {
    IDLE.with(|i| *i.borrow_mut() = f);
}

fn run_idle() -> bool {
    // Taken out while it runs: the hook wakes tasks, which borrows the executor.
    let Some(mut f) = IDLE.with(|i| i.borrow_mut().take()) else { return false };
    let r = f();
    IDLE.with(|i| {
        let mut slot = i.borrow_mut();
        if slot.is_none() {
            *slot = Some(f);
        }
    });
    r
}

/// Run `f` to completion on this thread.
pub fn run<T: Clone + 'static>(f: Fut<T>) -> Result<T, Trap> {
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        rt.tasks.clear();
        rt.ready.clear();
        rt.timers.clear();
        rt.current = None;
    });
    // The root future is polled in place (task 0), not spawned.
    let mut root = f;
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        rt.free.clear();
        rt.tasks.push(TaskEntry { fut: None, children: vec![], gen: 0, on_cancel: None, queued: true, done: false, root: true, depth: 0 });
        rt.ready.push_back(0);
    });
    loop {
        let next = RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            let id = rt.ready.pop_front()?;
            rt.tasks[id].queued = false;
            rt.current = Some(id);
            Some((id, rt.tasks[id].fut.take()))
        });
        if let Some((0, _)) = next {
            let w = waker_for(0);
            let mut cx = Context::from_waker(&w);
            let d = RT.with(|rt| rt.borrow().tasks[0].depth);
            DEPTH.with(|x| x.set(d));
            let r = root.as_mut().poll(&mut cx);
            let d = DEPTH.with(|x| x.get());
            RT.with(|rt| {
                let mut rt = rt.borrow_mut();
                rt.current = None;
                rt.tasks[0].depth = d;
            });
            if let Poll::Ready(v) = r {
                // Structured concurrency: the run's unawaited tasks end with it.
                let children = RT.with(|rt| {
                    let mut rt = rt.borrow_mut();
                    rt.tasks[0].done = true;
                    std::mem::take(&mut rt.tasks[0].children)
                });
                for (c, g) in children {
                    cancel_id(c, g);
                }
                return v;
            }
            continue;
        }
        match next {
            Some((id, Some(mut fut))) => {
                let w = waker_for(id);
                let mut cx = Context::from_waker(&w);
                let d = RT.with(|rt| rt.borrow().tasks[id].depth);
                DEPTH.with(|x| x.set(d));
                let done = fut.as_mut().poll(&mut cx).is_ready();
                let d = DEPTH.with(|x| x.get());
                let children = RT.with(|rt| {
                    let mut rt = rt.borrow_mut();
                    rt.current = None;
                    let t = &mut rt.tasks[id];
                    t.depth = d;
                    if done {
                        t.done = true;
                        t.on_cancel = None;
                        let c = std::mem::take(&mut t.children);
                        rt.free.push(id);
                        Some(c)
                    } else {
                        if !t.done {
                            t.fut = Some(fut);
                        }
                        None
                    }
                });
                // Structured concurrency: a finished task takes its unawaited children with it.
                if let Some(cs) = children {
                    for (c, g) in cs {
                        cancel_id(c, g);
                    }
                }
            }
            Some((_, None)) => {}
            None => {
                // No task can run: let the host finish batched work first (a
                // group commit), which may wake tasks.
                if run_idle() {
                    continue;
                }
                let (deadline, timeout_ms) = RT.with(|rt| {
                    let rt = rt.borrow();
                    (rt.deadline, rt.timeout_ms)
                });
                let next_timer = RT.with(|rt| rt.borrow().timers.peek().map(|Reverse((t, _, _))| *t));
                let Some(t) = next_timer else {
                    return trap("deadlock: every task is waiting and nothing can wake them");
                };
                let wake_at = deadline.map_or(t, |d| d.min(t));
                let now = Instant::now();
                if wake_at > now {
                    std::thread::sleep(wake_at - now);
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return trap(format!("budget exceeded: timeout of {timeout_ms} ms"));
                }
                let now = Instant::now();
                RT.with(|rt| {
                    let mut rt = rt.borrow_mut();
                    let mut due = vec![];
                    while let Some(Reverse((t, _, id))) = rt.timers.peek().copied() {
                        if t > now {
                            break;
                        }
                        rt.timers.pop();
                        due.push(id);
                    }
                    for id in due {
                        if let Some(t) = rt.tasks.get_mut(id) {
                            if !t.queued && !t.done && (t.fut.is_some() || t.root) {
                                t.queued = true;
                                rt.ready.push_back(id);
                            }
                        }
                    }
                });
            }
        }
    }
}

/// A host answer: immediate (no allocation) or a future for real I/O.
pub enum Reply<T> {
    Now(Result<T, Trap>),
    Later(Fut<T>),
}

impl<T> Reply<T> {
    pub async fn get(self) -> Result<T, Trap> {
        match self {
            Reply::Now(v) => v,
            Reply::Later(f) => f.await,
        }
    }
}

thread_local! {
    static END: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Mark the moment the program's entry finished (before result conversion),
/// so harnesses measure the program, not JSON output.
pub fn mark_end() {
    END.with(|e| e.set(Some(Instant::now())));
}

pub fn take_end() -> Option<Instant> {
    END.with(|e| e.take())
}

pub fn ready<T: 'static>(v: Result<T, Trap>) -> Fut<T> {
    Box::pin(std::future::ready(v))
}
