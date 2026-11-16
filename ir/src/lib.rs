//! ZAL IR v1: the versioned, source-independent execution format.
//!
//! A `Program` is what `zal build` writes and what the VM loads. It carries
//! bytecode per function plus the static facts (effects, extern signatures,
//! budgets) the host needs to authorize a run *before* executing anything.

use serde::{Deserialize, Serialize};

pub const IR_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Const {
    Int(i64),
    Str(String),
    Bool(bool),
    /// Milliseconds.
    Dur(i64),
}

/// Types as seen by the checker and by host marshalling.
/// `Var` only exists during checking of generic builtins and never reaches the IR.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Int,
    Bool,
    Str,
    Dur,
    Unit,
    List(Box<Ty>),
    Map(Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Tuple(Vec<Ty>),
    Task(Box<Ty>),
    /// Nominal record, resolved through `Program::records`.
    Record(String),
    /// Nominal enum, resolved through `Program::enums`.
    Enum(String),
    Var(u32),
    /// Bottom type of `return`, traps and diverging blocks.
    Never,
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Int => write!(f, "Int"),
            Ty::Bool => write!(f, "Bool"),
            Ty::Str => write!(f, "Str"),
            Ty::Dur => write!(f, "Dur"),
            Ty::Unit => write!(f, "Unit"),
            Ty::List(t) => write!(f, "List<{t}>"),
            Ty::Map(t) => write!(f, "Map<{t}>"),
            Ty::Option(t) => write!(f, "Option<{t}>"),
            Ty::Result(a, b) => write!(f, "Result<{a}, {b}>"),
            Ty::Tuple(ts) => {
                write!(f, "(")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{t}")?;
                }
                write!(f, ")")
            }
            Ty::Task(t) => write!(f, "Task<{t}>"),
            Ty::Record(n) | Ty::Enum(n) => write!(f, "{n}"),
            Ty::Var(v) => write!(f, "?{v}"),
            Ty::Never => write!(f, "Never"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    Len,
    Push,
    Get,
    Set,
    Remove,
    Range,
    MapNew,
    MapGet,
    MapSet,
    MapHas,
    MapKeys,
    ToStr,
    Print,
    Join,
    AwaitAny,
    Cancel,
    Sleep,
    JsonStr,
    JsonGetStr,
    NowMs,
    MapWithCapacity,
}

impl Builtin {
    /// Exact operand count; the VM pops this many values without checks.
    pub fn arity(self) -> u16 {
        use Builtin::*;
        match self {
            MapNew | NowMs => 0,
            Len | Range | MapKeys | ToStr | Print | Join | AwaitAny | Cancel | Sleep | JsonStr | MapWithCapacity => 1,
            Push | Get | Remove | MapGet | MapHas | JsonGetStr => 2,
            Set | MapSet => 3,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Const(u32),
    Unit,
    LoadLocal(u16),
    /// Move a local out (leaving Unit) so copy-on-write containers stay uniquely owned.
    TakeLocal(u16),
    StoreLocal(u16),
    Pop,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Not,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Jump(u32),
    JumpIfFalse(u32),
    JumpIfTrue(u32),
    /// Direct call of a ZAL function: pops `argc` args.
    Call(u32, u16),
    Builtin(Builtin, u16),
    /// Host call through the broker: suspends the fiber until the host answers.
    Extern(u32, u16),
    Ret,
    MakeTuple(u16),
    TupleGet(u16),
    MakeRecord(u16),
    FieldGet(u16),
    MakeList(u16),
    /// List element by Int index; traps when out of bounds.
    Index,
    /// Loop step over locals base (list), base+1 (Int index), base+2 (element):
    /// if index < len, load the element, advance the index and fall through;
    /// otherwise jump to the target.
    ForNext(u16, u32),
    // Superinstructions produced by the peephole pass (never by hand):
    /// `LoadLocal a; Const k; Add; StoreLocal a` with an Int constant.
    IncLocal(u16, i32),
    /// `LoadLocal a; LoadLocal b`.
    LoadLocal2(u16, u16),
    /// `Lt; JumpIfFalse t`.
    JumpIfNotLt(u32),
    /// `StoreLocal a; LoadLocal a`.
    StoreLoad(u16),
    /// Concatenate the top `n` Str values in one allocation (compiled from `a + b + ...`).
    ConcatN(u16),
    /// Pop a tuple of `n` and push its elements in order.
    SplitTuple(u16),
    /// `TagIs t; JumpIfFalse j`.
    JumpIfNotTag(u16, u32),
    /// `LoadLocal a; LoadLocal b; Index`.
    IndexLL(u16, u16),
    /// `LoadLocal a; FieldGet f`.
    LoadField(u16, u16),
    /// `LoadLocal a; Payload`.
    LoadPayload(u16),
    /// `LoadLocal a; Payload; StoreLocal b`.
    MovePayload(u16, u16),
    /// Pop an Option/Result-Ok-shaped variant: tag 1 (Some) stores the payload
    /// into local `r` and falls through; otherwise jump. (Fused match on a
    /// temporary that nothing else reads.)
    UnwrapOrJump(u16, u32),
    /// Pop key and map; if present store the value into local `r`, else jump.
    /// `map_get` + `UnwrapOrJump` without allocating the Option.
    MapGetJump(u16, u32),
    /// `LoadLocal a; Builtin(Len)`.
    LenLocal(u16),
    /// In-place `a = b(a, args...)` for push/set/remove/map_set: pops the other
    /// arguments, updates the container in local `a`.
    UpdLocal(Builtin, u16),
    /// Enum / Option / Result constructor. Option: None=0, Some=1. Result: Ok=0, Err=1.
    MakeVariant(u16, bool),
    /// Pop a variant, push Bool (tag matches).
    TagIs(u16),
    /// Replace the variant on top of stack with its payload.
    Payload,
    /// Start a child fiber running function `fn` with `argc` args; push Task handle.
    Spawn(u32, u16),
    Await,
    /// Await `n` handles (stack order) and push a tuple of their results.
    AwaitAll(u16),
    /// Await the first of `n` handles, cancel the others, push its result.
    Race(u16),
    /// Stack: handle, dur. Push Some(v) if done in time, else cancel and push None.
    AwaitTimeout,
    /// Pop a Str and abort the whole run with it.
    Trap,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    Fn,
    Task,
    Workflow,
    /// Compiler-generated body of a `parallel`/`race`/`timeout`/`retry`/`spawn` block.
    Lifted,
    Test,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Budget {
    pub timeout_ms: Option<i64>,
    pub max_parallel: Option<u32>,
    pub max_tool_calls: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Function {
    pub name: String,
    pub kind: FnKind,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    pub locals: u16,
    pub code: Vec<Op>,
    /// Source line per op, for diagnostics and traces.
    pub lines: Vec<u32>,
    /// Effects this function may exercise, transitively (checked statically).
    pub effects: Vec<String>,
    pub budget: Option<Budget>,
    /// Static value tag per local slot, from the type checker (optional):
    /// low byte = value tag (RFC 0002 §3 numbering, Variant = 9, Task = 10),
    /// high byte = element tag for a List; 0xFF = unknown. A known tag holds
    /// for every *read* of the slot (a slot may still be Unit before its
    /// first store). Empty = no information. Only used by JIT code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slot_tags: Vec<u16>,
    /// `tag_seal` of this function when the compiler produced `slot_tags`
    /// in this process; 0 otherwise. Never serialized: IR loaded from a file,
    /// or changed after compilation, has no valid seal, so machine code never
    /// trusts slot tags it cannot vouch for.
    #[serde(skip)]
    pub slot_seal: u64,
}

/// Keyed hash (per-process random key) over the whole program as
/// serialized (code, types, constants, externs, records and the slot tags;
/// `slot_seal` itself is not serialized). Slot tags are only sound when every
/// function is the checker's output: one changed function can hand others
/// values of other types. Unforgeable across processes.
pub fn tag_seal(prog: &Program) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    static KEY: std::sync::OnceLock<std::collections::hash_map::RandomState> = std::sync::OnceLock::new();
    let mut h = KEY.get_or_init(std::collections::hash_map::RandomState::new).build_hasher();
    prog.to_json().hash(&mut h);
    h.finish() | 1
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Extern {
    pub name: String,
    /// The capability the host must grant for every call, e.g. `tool.build`.
    pub effect: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    /// Declared free of observable side effects (may run concurrently).
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Enum {
    pub name: String,
    pub variants: Vec<(String, Option<Ty>)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Program {
    pub ir_version: u32,
    pub module: String,
    pub consts: Vec<Const>,
    pub functions: Vec<Function>,
    pub externs: Vec<Extern>,
    pub records: Vec<Record>,
    pub enums: Vec<Enum>,
}

impl Program {
    pub fn function(&self, name: &str) -> Option<(u32, &Function)> {
        self.functions
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name)
            .map(|(i, f)| (i as u32, f))
    }

    pub fn record(&self, name: &str) -> Option<&Record> {
        self.records.iter().find(|r| r.name == name)
    }

    pub fn enum_def(&self, name: &str) -> Option<&Enum> {
        self.enums.iter().find(|e| e.name == name)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("IR serializes")
    }

    pub fn from_json(s: &str) -> Result<Program, String> {
        let p: Program = serde_json::from_str(s).map_err(|e| format!("invalid IR: {e}"))?;
        if p.ir_version != IR_VERSION {
            return Err(format!(
                "IR version {} not supported (this VM runs {})",
                p.ir_version, IR_VERSION
            ));
        }
        Ok(p)
    }

    /// Verify every function's bytecode and return its maximum operand-stack
    /// depth. The VM relies on this proof to skip bounds checks: operand
    /// depth never underflows, never exceeds the returned maximum, is the same
    /// on every path into an instruction, every local/constant/function index
    /// is in range, and control never runs off the end of a function.
    pub fn verify(&self) -> Result<Vec<u32>, String> {
        self.verify_depths().map(|(max, _)| max)
    }

    /// As `verify`, plus the proven operand depth before every op of every
    /// function (`None` = unreachable op). Native backends use it to address
    /// operand slots at constant offsets from the frame base.
    pub fn verify_depths(&self) -> Result<(Vec<u32>, Vec<Vec<Option<u32>>>), String> {
        // The host dispatches by extern name but the broker authorizes by
        // effect, so the effect must be bound to the name: `<kind>.<name>`.
        // Otherwise a crafted IR file could call `deploy` under `tool.read`.
        const KINDS: &[&str] = &["tool", "net", "file", "secret", "worker", "deploy"];
        for x in &self.externs {
            let ok = x.effect.split_once('.').is_some_and(|(k, n)| KINDS.contains(&k) && n == x.name);
            if !ok {
                return Err(format!("IR verify: extern `{}` has effect `{}`; expected `<kind>.{}`", x.name, x.effect, x.name));
            }
        }
        // Declared effects of functions must be well-formed requests.
        for f in &self.functions {
            for e in &f.effects {
                let ok = e.split_once('.').is_some_and(|(k, n)| KINDS.contains(&k) && !n.is_empty());
                if !ok {
                    return Err(format!("IR verify: function `{}` declares malformed effect `{e}`", f.name));
                }
            }
        }
        // Effects are a static contract: a function may only reach externs (and
        // callees) whose effects it declares. The broker re-checks each call, but
        // a crafted IR file must not slip a call past its declared surface.
        let covers = |decl: &[String], e: &str| decl.iter().any(|d| d == e || (d.ends_with(".*") && e.starts_with(&d[..d.len() - 1])));
        for f in &self.functions {
            for op in &f.code {
                let need: Vec<&str> = match *op {
                    Op::Extern(e, _) => self.externs.get(e as usize).map(|x| vec![x.effect.as_str()]).unwrap_or_default(),
                    Op::Call(g, _) | Op::Spawn(g, _) => self.functions.get(g as usize).map(|g| g.effects.iter().map(String::as_str).collect()).unwrap_or_default(),
                    _ => continue,
                };
                if let Some(e) = need.into_iter().find(|e| !covers(&f.effects, e)) {
                    return Err(format!("IR verify: `{}` reaches effect `{e}` it does not declare", f.name));
                }
            }
        }
        let mut out = Vec::with_capacity(self.functions.len());
        let mut all = Vec::with_capacity(self.functions.len());
        for (fi, f) in self.functions.iter().enumerate() {
            let err = |pc: usize, m: String| format!("IR verify: {} (#{fi}) at {pc}: {m}", f.name);
            let n = f.code.len();
            if n == 0 {
                return Err(err(0, "empty function".into()));
            }
            if f.lines.len() != n {
                return Err(err(0, "line table length mismatch".into()));
            }
            let locals = f.locals as usize;
            if locals < f.params.len() {
                return Err(err(0, "fewer locals than parameters".into()));
            }
            let mut depth: Vec<Option<u32>> = vec![None; n];
            let mut work = vec![(0usize, 0u32)];
            let mut max = 0u32;
            while let Some((pc, d)) = work.pop() {
                if pc >= n {
                    return Err(err(pc, "control runs off the end".into()));
                }
                match depth[pc] {
                    Some(x) if x == d => continue,
                    Some(x) => return Err(err(pc, format!("stack depth {d} meets {x}"))),
                    None => depth[pc] = Some(d),
                }
                let op = f.code[pc];
                let local = |i: u16| -> Result<(), String> {
                    if (i as usize) < locals { Ok(()) } else { Err(err(pc, format!("local {i} out of range"))) }
                };
                let target = |t: u32| -> Result<usize, String> {
                    if (t as usize) < n { Ok(t as usize) } else { Err(err(pc, format!("jump target {t} out of range"))) }
                };
                let callee = |idx: u32, argc: u16| -> Result<(), String> {
                    let g = self.functions.get(idx as usize).ok_or_else(|| err(pc, format!("function {idx} out of range")))?;
                    if g.params.len() != argc as usize { return Err(err(pc, format!("call of {} with {argc} args", g.name))); }
                    Ok(())
                };
                // (pops, pushes, successors)
                let (pops, pushes): (u32, u32) = match op {
                    Op::Const(c) => {
                        if (c as usize) >= self.consts.len() { return Err(err(pc, format!("constant {c} out of range"))); }
                        (0, 1)
                    }
                    Op::Unit => (0, 1),
                    Op::LoadLocal(i) | Op::TakeLocal(i) => { local(i)?; (0, 1) }
                    Op::StoreLocal(i) => { local(i)?; (1, 0) }
                    Op::StoreLoad(i) => { local(i)?; (1, 1) }
                    Op::IncLocal(i, _) => { local(i)?; (0, 0) }
                    Op::LoadLocal2(a, b) => { local(a)?; local(b)?; (0, 2) }
                    Op::ForNext(b, _) => { local(b)?; local(b.checked_add(2).ok_or_else(|| err(pc, "loop slot overflow".into()))?)?; (0, 0) }
                    Op::Pop => (1, 0),
                    Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Index => (2, 1),
                    Op::Neg | Op::Not | Op::TagIs(_) | Op::Payload | Op::TupleGet(_) | Op::FieldGet(_) | Op::Await => (1, 1),
                    Op::Jump(_) => (0, 0),
                    Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => (1, 0),
                    Op::JumpIfNotLt(_) => (2, 0),
                    Op::Call(g, argc) | Op::Spawn(g, argc) => { callee(g, argc)?; (argc as u32, 1) }
                    Op::Extern(e, argc) => {
                        let x = self.externs.get(e as usize).ok_or_else(|| err(pc, format!("extern {e} out of range")))?;
                        if x.params.len() != argc as usize { return Err(err(pc, "extern arity".into())); }
                        (argc as u32, 1)
                    }
                    Op::Builtin(b, argc) => {
                        if b.arity() != argc { return Err(err(pc, format!("builtin {b:?} with {argc} args, expects {}", b.arity()))); }
                        (argc as u32, 1)
                    }
                    Op::MakeTuple(k) | Op::MakeRecord(k) | Op::MakeList(k) | Op::AwaitAll(k) | Op::Race(k) | Op::ConcatN(k) => (k as u32, 1),
                    Op::MakeVariant(_, has) => (has as u32, 1),
                    Op::AwaitTimeout => (2, 1),
                    Op::Ret | Op::Trap => (1, 0),
                    Op::JumpIfNotTag(_, _) => (1, 0),
                    Op::SplitTuple(k) => (1, k as u32),
                    Op::IndexLL(a, b) => { local(a)?; local(b)?; (0, 1) }
                    Op::LoadField(a, _) | Op::LoadPayload(a) | Op::LenLocal(a) => { local(a)?; (0, 1) }
                    Op::MovePayload(a, b) => { local(a)?; local(b)?; (0, 0) }
                    Op::UnwrapOrJump(r, _) => { local(r)?; (1, 0) }
                    Op::MapGetJump(r, _) => { local(r)?; (2, 0) }
                    Op::UpdLocal(b, a) => {
                        local(a)?;
                        let n = match b { Builtin::Push | Builtin::Remove => 1, Builtin::Set | Builtin::MapSet => 2, _ => return Err(err(pc, "UpdLocal on unsupported builtin".into())) };
                        (n, 0)
                    }
                };
                if d < pops {
                    return Err(err(pc, format!("stack underflow ({d} < {pops})")));
                }
                let nd = d - pops + pushes;
                // Re-executing ops (await, race, timeout, builtins that block) keep
                // their operands while suspended: the peak includes them.
                max = max.max(d.max(nd));
                match op {
                    Op::Ret | Op::Trap => {}
                    Op::Jump(t) => work.push((target(t)?, nd)),
                    Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfNotLt(t) | Op::ForNext(_, t) | Op::JumpIfNotTag(_, t) | Op::UnwrapOrJump(_, t) | Op::MapGetJump(_, t) => {
                        work.push((target(t)?, nd));
                        work.push((pc + 1, nd));
                    }
                    _ => work.push((pc + 1, nd)),
                }
            }
            out.push(max);
            all.push(depth);
        }
        Ok((out, all))
    }

    /// Human-readable listing for `zal ir`.
    pub fn disassemble(&self) -> String {
        let mut s = format!("; module {} (ZAL IR v{})\n", self.module, self.ir_version);
        for e in &self.externs {
            let ps: Vec<String> = e.params.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            s += &format!("extern {}({}) -> {}  ; effect {}\n", e.name, ps.join(", "), e.ret, e.effect);
        }
        for (i, f) in self.functions.iter().enumerate() {
            let ps: Vec<String> = f.params.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            s += &format!(
                "\n#{i} {:?} {}({}) -> {}  locals={} effects=[{}]\n",
                f.kind,
                f.name,
                ps.join(", "),
                f.ret,
                f.locals,
                f.effects.join(", ")
            );
            if let Some(b) = &f.budget {
                s += &format!("  budget {:?}\n", b);
            }
            for (pc, op) in f.code.iter().enumerate() {
                let extra = match op {
                    Op::Const(c) => format!("  ; {:?}", self.consts[*c as usize]),
                    Op::Call(t, _) | Op::Spawn(t, _) => {
                        format!("  ; {}", self.functions[*t as usize].name)
                    }
                    Op::Extern(t, _) => format!("  ; {}", self.externs[*t as usize].name),
                    _ => String::new(),
                };
                s += &format!("  {pc:4}  {:?}{extra}\n", op);
            }
        }
        s
    }
}
