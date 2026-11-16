//! IR v1 function -> Cranelift IR.
//!
//! Shape of every generated body (RFC 0002 §1):
//!
//! ```text
//! entry(ctx, entry_pc): load stack/sp/base; jump dispatch(entry_pc)
//! dispatch(pc):         br_table pc -> blocks[0..n], default bad
//! blocks[leader]:       a region: straight-line code from a leader up to the
//!                       next leader, ops inline with a type/value mirror
//! blocks[other]:        a step block: that op through `zal_rt_op`, then on to
//!                       blocks[pc+1] (only reached by dispatch/redispatch)
//! exit(r):              return r
//! ```
//!
//! Leaders are pc 0, every static branch target and every pc after an op that
//! ends straight-line flow (unconditional jump, an op without inline code).
//!
//! Inside a region the generator keeps a *mirror*: for the top operand slots
//! and for locals it remembers the tag (a constant, or an SSA value already
//! loaded) and the payload word as an SSA value. Ops read operands from the
//! mirror instead of memory and skip tag guards whose outcome is known.
//! Memory stays canonical (every push and local store is written through), so
//! any runtime call, slow path, exit or region boundary needs no flush beyond
//! `sp`, and every block can be entered with memory state alone.
//!
//! A guarded op whose operand types are not known emits `guard ? fast : slow`;
//! the slow path runs the op through `zal_rt_op` and rejoins the fast path,
//! passing the result's tag and payload as block parameters.
//!
//! `stack`, `sp` and `base` live in frontend variables; `sp` is written back
//! before every runtime call that reads ctx and all three are re-read after it.

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    self, types, AbiParam, Block, BlockArg, FuncRef, InstBuilder, JumpTableData, MemFlagsData, Signature, Value,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use zal_ir::{Const, Op, Program};

use crate::abi::*;

pub(crate) struct Imports {
    op: FuncId,
    tick: FuncId,
    trap: FuncId,
    clone: FuncId,
    drop: FuncId,
    v2: Option<V2<FuncId>>,
    v3: Option<V3<FuncId>>,
    /// Data layout for inline access to owned values (addendum v4); only for
    /// JIT code running in the process that measured it.
    pub(crate) lay: Option<DataLayout>,
    /// Address of `zal_vm::native::jit_constants` (JIT only, with `lay`).
    pub(crate) kconst: Option<usize>,
    /// `zal_ir::tag_seal` of the program being compiled (0 = not computed).
    pub(crate) seal: u64,
    /// `zal_rt_list_one` (JIT with inline data access only).
    pub(crate) list_one: Option<FuncId>,
}

/// RFC 0002 v3 entry points.
#[derive(Clone, Copy)]
struct V3<T> {
    make: T,
    builtin: T,
    map_get_jump: T,
}

fn declare_v3<M: Module>(m: &mut M) -> Result<V3<FuncId>, String> {
    let ptr = m.target_config().pointer_type();
    let i32t = types::I32;
    let mut d = |name: &str, params: &[ir::Type]| -> Result<FuncId, String> {
        let mut sig = m.make_signature();
        sig.params.extend(params.iter().map(|t| AbiParam::new(*t)));
        sig.returns.push(AbiParam::new(i32t));
        m.declare_function(name, Linkage::Import, &sig).map_err(|e| format!("declare {name}: {e}"))
    };
    Ok(V3 {
        make: d("zal_rt_make", &[ptr, i32t, i32t])?,
        builtin: d("zal_rt_builtin", &[ptr, i32t, i32t])?,
        map_get_jump: d("zal_rt_map_get_jump", &[ptr, i32t])?,
    })
}

/// Addendum v2 entry points.
#[derive(Clone, Copy)]
struct V2<T> {
    len: T,
    index: T,
    for_next: T,
    upd: T,
    tag: T,
    payload: T,
    field: T,
    map_get: T,
    konst: T,
    concat: T,
    call: T,
    ret: T,
}

impl<T: Copy> V2<T> {
    fn map<U>(self, mut f: impl FnMut(T) -> U) -> V2<U> {
        V2 {
            len: f(self.len),
            index: f(self.index),
            for_next: f(self.for_next),
            upd: f(self.upd),
            tag: f(self.tag),
            payload: f(self.payload),
            field: f(self.field),
            map_get: f(self.map_get),
            konst: f(self.konst),
            concat: f(self.concat),
            call: f(self.call),
            ret: f(self.ret),
        }
    }
}

fn declare_v2<M: Module>(m: &mut M) -> Result<V2<FuncId>, String> {
    let ptr = m.target_config().pointer_type();
    let i32t = types::I32;
    let mut d = |name: &str, params: &[ir::Type], ret: Option<ir::Type>| -> Result<FuncId, String> {
        let mut sig = m.make_signature();
        sig.params.extend(params.iter().map(|t| AbiParam::new(*t)));
        sig.returns.extend(ret.map(AbiParam::new));
        m.declare_function(name, Linkage::Import, &sig).map_err(|e| format!("declare {name}: {e}"))
    };
    Ok(V2 {
        len: d("zal_rt_len", &[ptr], Some(types::I64))?,
        index: d("zal_rt_index", &[ptr, ptr, types::I64], Some(i32t))?,
        for_next: d("zal_rt_for_next", &[ptr], Some(i32t))?,
        upd: d("zal_rt_upd", &[ptr, ptr, i32t], Some(i32t))?,
        tag: d("zal_rt_tag", &[ptr], Some(i32t))?,
        payload: d("zal_rt_payload", &[ptr, ptr], Some(i32t))?,
        field: d("zal_rt_field", &[ptr, ptr, i32t], Some(i32t))?,
        map_get: d("zal_rt_map_get", &[ptr, ptr, ptr], Some(i32t))?,
        konst: d("zal_rt_const", &[ptr, i32t, ptr], None)?,
        concat: d("zal_rt_concat", &[ptr, i32t], Some(i32t))?,
        call: d("zal_rt_call", &[ptr, i32t, i32t, i32t], Some(i32t))?,
        ret: d("zal_rt_ret", &[ptr], Some(i32t))?,
    })
}


/// This backend uses the addendum v2 runtime entry points (kept as a flag so
/// tests and reports can state it).
pub const RT_V2: bool = true;

pub(crate) fn body_signature<M: Module>(m: &M) -> Signature {
    let ptr = m.target_config().pointer_type();
    let mut s = m.make_signature();
    s.params.push(AbiParam::new(ptr));
    s.params.push(AbiParam::new(types::I32));
    s.returns.push(AbiParam::new(types::I32));
    s
}

pub(crate) fn declare_imports<M: Module>(m: &mut M) -> Result<Imports, String> {
    let ptr = m.target_config().pointer_type();
    let op_sig = body_signature(m);
    let mut trap_sig = body_signature(m);
    trap_sig.params.push(AbiParam::new(types::I32));
    let mut decl = |name: &str, sig: &Signature| {
        m.declare_function(name, Linkage::Import, sig).map_err(|e| format!("declare {name}: {e}"))
    };
    let op = decl("zal_rt_op", &op_sig)?;
    let tick = decl("zal_rt_tick", &op_sig)?;
    let trap = decl("zal_rt_trap", &trap_sig)?;
    let mut clone_sig = m.make_signature();
    clone_sig.params.push(AbiParam::new(ptr));
    clone_sig.params.push(AbiParam::new(ptr));
    let mut drop_sig = m.make_signature();
    drop_sig.params.push(AbiParam::new(ptr));
    let clone = m.declare_function("zal_rt_clone", Linkage::Import, &clone_sig).map_err(|e| e.to_string())?;
    let drop = m.declare_function("zal_rt_drop", Linkage::Import, &drop_sig).map_err(|e| e.to_string())?;
    let v2 = if RT_V2 { Some(declare_v2(m)?) } else { None };
    let v3 = if RT_V2 { Some(declare_v3(m)?) } else { None };
    Ok(Imports { op, tick, trap, clone, drop, v2, v3, lay: None, kconst: None, seal: 0, list_one: None })
}

/// Ops with inline code (for tests/reporting); all others use `zal_rt_op`.
pub fn has_fast_path(prog: &Program, op: &Op) -> bool {
    inline_capable(prog, op, RT_V2) || (RT_V2 && matches!(op, Op::Call(..) | Op::Ret))
}

/// Builtins that may block or touch the scheduler: always the generic path.
fn builtin_blocks(b: zal_ir::Builtin) -> bool {
    use zal_ir::Builtin::*;
    matches!(b, Print | Join | AwaitAny | Cancel | Sleep)
}

/// Ops emitted inside a region (no region break after them).
fn inline_capable(prog: &Program, op: &Op, v2: bool) -> bool {
    match *op {
        Op::Const(c) => v2 || !matches!(prog.consts.get(c as usize), Some(Const::Str(_)) | None),
        Op::LenLocal(_)
        | Op::IndexLL(..)
        | Op::Index
        | Op::ForNext(..)
        | Op::JumpIfNotTag(..)
        | Op::LoadPayload(_)
        | Op::MovePayload(..)
        | Op::LoadField(..)
        | Op::MapGetJump(..)
        | Op::ConcatN(_)
        | Op::MakeList(_)
        | Op::MakeTuple(_)
        | Op::MakeRecord(_)
        | Op::MakeVariant(..)
        | Op::TagIs(_)
        | Op::Not => v2,
        Op::Builtin(b, _) => v2 && !builtin_blocks(b),
        Op::UpdLocal(b, _) => v2 && matches!(b, zal_ir::Builtin::Push | zal_ir::Builtin::Set | zal_ir::Builtin::Remove | zal_ir::Builtin::MapSet),
        Op::Unit
        | Op::LoadLocal(_)
        | Op::LoadLocal2(..)
        | Op::TakeLocal(_)
        | Op::StoreLoad(_)
        | Op::StoreLocal(_)
        | Op::IncLocal(..)
        | Op::Pop
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Mod
        | Op::Neg
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge
        | Op::Eq
        | Op::Ne
        | Op::Jump(_)
        | Op::JumpIfFalse(_)
        | Op::JumpIfTrue(_)
        | Op::JumpIfNotLt(_) => true,
        _ => false,
    }
}

fn branch_target(op: &Op) -> Option<u32> {
    match *op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfTrue(t)
        | Op::JumpIfNotLt(t)
        | Op::ForNext(_, t)
        | Op::JumpIfNotTag(_, t)
        | Op::UnwrapOrJump(_, t)
        | Op::MapGetJump(_, t) => Some(t),
        _ => None,
    }
}

/// Region starts: pc 0, branch targets, and the pc after any op that ends
/// straight-line flow inside a region.
fn leaders(prog: &Program, code: &[Op], v2: bool) -> Vec<bool> {
    let n = code.len();
    let mut l = vec![false; n + 1];
    l[0] = true;
    for (pc, op) in code.iter().enumerate() {
        if let Some(t) = branch_target(op) {
            l[t as usize] = true;
        }
        let ends = matches!(op, Op::Jump(_) | Op::Ret | Op::Trap) || !inline_capable(prog, op, v2);
        if ends {
            l[pc + 1] = true;
        }
    }
    l.truncate(n);
    l
}

/// What the generator knows about a value's tag.
#[derive(Clone, Copy)]
enum Tag {
    Known(u8),
    /// Loaded into this SSA value (i8); type unknown at compile time.
    Dyn(Value),
}

/// Mirror of one value: tag plus the payload word at +8 as `i64`. The word
/// is exact for Known plain tags (Bool: 0/1); for Dyn tags it is the raw word.
#[derive(Clone, Copy)]
struct M {
    tag: Tag,
    w: Value,
}

/// What a failed guard does.
#[derive(Clone, Copy)]
enum Slow {
    Deopt,
    JoinBool,
}

/// Static outcome of a guard.
enum G {
    Yes,
    No,
    Dyn(Value),
}

struct Gen<'a, 'b> {
    b: &'b mut FunctionBuilder<'a>,
    prog: &'b Program,
    code: &'b [Op],
    ptr: ir::Type,
    ctx: Value,
    stack: Variable,
    sp: Variable,
    /// Address of local 0 (see `reload`).
    base: Variable,
    /// Address of the first free operand slot.
    spp: Variable,
    blocks: Vec<Block>,
    leader: Vec<bool>,
    redispatch: Block,
    /// step(pc): op `pc` through `zal_rt_op`, then dispatch(pc + 1).
    step: Block,
    exit: Block,
    bad: Block,
    rt_op: FuncRef,
    rt_tick: FuncRef,
    rt_trap: FuncRef,
    rt_clone: FuncRef,
    rt_drop: FuncRef,
    v2: Option<V2<FuncRef>>,
    v3: Option<V3<FuncRef>>,
    lay: Option<DataLayout>,
    kconst: Option<usize>,
    list_one: Option<FuncRef>,
    /// `slot_tags` of this function (empty unless inline data access is on).
    stags: Vec<u16>,
    /// Mirror of the top operand slots (last = top); `None` = only in memory.
    vs: Vec<Option<M>>,
    /// Mirror of locals.
    locs: HashMap<u16, M>,
    /// Ops after the current one that a fused emission consumed.
    skip: usize,
    /// Lazy operand slots: Known plain values pushed in this region whose
    /// memory slot is not written yet, as (slot index relative to the
    /// region's entry sp, value). Written by `sync` before anything reads
    /// operand memory and by `emit_pending` on every edge out of the region.
    pending: Vec<(i64, M)>,
    /// sp relative to the region entry (compile time).
    depth: i64,
    /// Lazy pushes enabled (`ZAL_CL_NO_LAZY` turns them off).
    lazy: bool,
    /// Emitting a side arm that rejoins: push eagerly, and `sync` stores
    /// without forgetting (the other arm has not written the slots).
    in_arm: bool,
    /// Static operand depth before each op (verified IR). With it, sp is a
    /// compile-time quantity: operand slot = base + (`sd0` + `depth` - k),
    /// and the `sp`/`spp`/`stack` variables are not used at all, which keeps
    /// registers free across the whole function.
    stat: Option<Vec<Option<u32>>>,
    /// Number of locals of this function.
    nloc: i64,
    /// Slot index (from base) of sp at the start of the current region.
    sd0: i64,
}

/// Value tag of a static type (RFC 0002 §3 numbering; Variant 9, Task 10).
fn ty_tag(t: &zal_ir::Ty) -> Option<u8> {
    use zal_ir::Ty;
    Some(match t {
        Ty::Unit => 0,
        Ty::Int => 1,
        Ty::Bool => 2,
        Ty::Str => 3,
        Ty::Dur => 4,
        Ty::List(_) => 5,
        Ty::Map(_) => 6,
        Ty::Tuple(_) => 7,
        Ty::Record(_) => 8,
        Ty::Option(_) | Ty::Result(..) | Ty::Enum(_) => 9,
        Ty::Task(_) => 10,
        Ty::Var(_) | Ty::Never => return None,
    })
}

fn mem() -> MemFlagsData {
    MemFlagsData::trusted()
}

impl Gen<'_, '_> {
    // ---- ctx and addressing ----

    /// Re-read `stack`/`sp`/`base` from ctx. The generator keeps two derived
    /// pointers: `base` holds the address of local 0 and `spp` the address of
    /// the first free slot, so every slot address is a constant offset
    /// (folded into the load/store) instead of a multiply and two adds.
    fn reload(&mut self) {
        if self.stat.is_some() {
            let s = self.b.ins().load(self.ptr, mem(), self.ctx, CTX_STACK);
            let base = self.b.ins().load(types::I64, mem(), self.ctx, CTX_BASE);
            let lp = self.addr_of(s, base);
            self.b.def_var(self.base, lp);
            return;
        }
        let s = self.b.ins().load(self.ptr, mem(), self.ctx, CTX_STACK);
        let sp = self.b.ins().load(types::I64, mem(), self.ctx, CTX_SP);
        let base = self.b.ins().load(types::I64, mem(), self.ctx, CTX_BASE);
        let lp = self.addr_of(s, base);
        let spp = self.addr_of(s, sp);
        self.b.def_var(self.stack, s);
        self.b.def_var(self.sp, sp);
        self.b.def_var(self.base, lp);
        self.b.def_var(self.spp, spp);
    }

    fn addr_of(&mut self, s: Value, idx: Value) -> Value {
        let off = self.b.ins().imul_imm_s(idx, VALUE_SIZE);
        let off = if self.ptr == types::I64 { off } else { self.b.ins().ireduce(self.ptr, off) };
        self.b.ins().iadd(s, off)
    }

    fn flush(&mut self) {
        self.emit_pending();
        if self.stat.is_some() {
            let base = self.b.ins().load(types::I64, mem(), self.ctx, CTX_BASE);
            let sp = self.b.ins().iadd_imm_s(base, self.sd0 + self.depth);
            self.b.ins().store(mem(), sp, self.ctx, CTX_SP);
            return;
        }
        let sp = self.b.use_var(self.sp);
        self.b.ins().store(mem(), sp, self.ctx, CTX_SP);
    }

    /// Address of `stack[sp - k]` (k = 1: top of stack, k = 0: first free slot).
    fn top(&mut self, k: i64) -> Value {
        if k >= 1 && !self.pending.is_empty() {
            self.sync();
        }
        if self.stat.is_some() {
            let lp = self.b.use_var(self.base);
            return self.b.ins().iadd_imm_s(lp, (self.sd0 + self.depth - k) * VALUE_SIZE);
        }
        let spp = self.b.use_var(self.spp);
        if k == 0 { spp } else { self.b.ins().iadd_imm_s(spp, -k * VALUE_SIZE) }
    }

    fn local(&mut self, l: u16) -> Value {
        let lp = self.b.use_var(self.base);
        if l == 0 { lp } else { self.b.ins().iadd_imm_s(lp, l as i64 * VALUE_SIZE) }
    }

    fn bump_sp(&mut self, d: i64) {
        self.depth += d;
        let dep = self.depth;
        self.pending.retain(|(i, _)| *i < dep);
        if self.stat.is_some() {
            return;
        }
        let sp = self.b.use_var(self.sp);
        let sp = self.b.ins().iadd_imm_s(sp, d);
        self.b.def_var(self.sp, sp);
        let spp = self.b.use_var(self.spp);
        let spp = self.b.ins().iadd_imm_s(spp, d * VALUE_SIZE);
        self.b.def_var(self.spp, spp);
    }

    fn copy_value(&mut self, dst: Value, src: Value) {
        for off in [0, 8, 16] {
            let w = self.b.ins().load(types::I64, mem(), src, off);
            self.b.ins().store(mem(), w, dst, off);
        }
    }

    // ---- lazy operand slots ----

    /// Store every lazy slot without forgetting them: for edges that leave
    /// the region, so the continuing flow keeps its state.
    fn emit_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        for (i, m) in self.pending.clone() {
            let at = if self.stat.is_some() {
                let lp = self.b.use_var(self.base);
                self.b.ins().iadd_imm_s(lp, (self.sd0 + i) * VALUE_SIZE)
            } else {
                let spp = self.b.use_var(self.spp);
                self.b.ins().iadd_imm_s(spp, (i - self.depth) * VALUE_SIZE)
            };
            self.store_plain(at, m);
        }
    }

    /// Store every lazy slot and forget them (continuing flow reads memory).
    fn sync(&mut self) {
        self.emit_pending();
        if !self.in_arm {
            self.pending.clear();
        }
    }

    /// Push the Known plain value `m` as a new operand slot (memory and sp;
    /// the mirror is the caller's business).
    fn push_slot(&mut self, m: M) {
        if self.lazy && !self.in_arm {
            self.pending.push((self.depth, m));
        } else {
            let at = self.top(0);
            self.store_plain(at, m);
        }
        self.bump_sp(1);
    }

    /// Ops that keep lazy slots lazy: they read operands only from the
    /// mirror and call nothing that reads operand memory. Every other op
    /// starts with canonical operand memory. (Lazy values held across an
    /// op's internal calls were measured worse than storing them.)
    fn lazy_ok(&self, op: &Op) -> bool {
        match *op {
            Op::Const(c) => matches!(self.prog.consts.get(c as usize), Some(Const::Int(_) | Const::Dur(_) | Const::Bool(_))),
            Op::Unit
            | Op::LoadLocal(_)
            | Op::LoadLocal2(..)
            | Op::StoreLocal(_)
            | Op::StoreLoad(_)
            | Op::IncLocal(..)
            | Op::Pop
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::Eq
            | Op::Ne
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::JumpIfTrue(_)
            | Op::JumpIfNotLt(_) => true,
            Op::LenLocal(a) => self.lay.is_some() && self.static_tag(a) == Some(TAG_LIST),
            _ => false,
        }
    }

    /// Proven sp slot index (from base) before op `pc`.
    fn static_sp(&self, pc: usize) -> Option<i64> {
        let d = (*self.stat.as_ref()?.get(pc)?)?;
        Some(self.nloc + d as i64)
    }

    /// Start emitting at op `pc` with canonical memory.
    fn begin(&mut self, pc: usize) {
        self.clear();
        self.sd0 = self.static_sp(pc).unwrap_or(self.nloc);
    }

    /// Compile-time sp change that the runtime made (static mode only; the
    /// dynamic mode learns it from `reload`).
    fn adjust_static(&mut self, d: i64) {
        if self.stat.is_some() {
            debug_assert!(self.pending.is_empty());
            self.depth += d;
        }
    }

    /// After a helper that pushed/popped through ctx and succeeded: sp is
    /// the proven depth of the fall-through op `pc + 1`.
    fn after_rt(&mut self, pc: usize) {
        if let Some(sp) = self.static_sp(pc + 1) {
            debug_assert!(self.pending.is_empty());
            self.depth = sp - self.sd0;
        }
    }

    // ---- mirror ----

    fn clear(&mut self) {
        self.vs.clear();
        self.locs.clear();
        self.pending.clear();
        self.depth = 0;
    }

    fn peek(&self, k: usize) -> Option<M> {
        let n = self.vs.len();
        if k <= n { self.vs[n - k] } else { None }
    }

    fn set_peek(&mut self, k: usize, m: M) {
        while self.vs.len() < k {
            self.vs.insert(0, None);
        }
        let n = self.vs.len();
        self.vs[n - k] = Some(m);
    }

    fn load_m(&mut self, addr: Value) -> M {
        let t = self.b.ins().load(types::I8, mem(), addr, 0);
        let w = self.b.ins().load(types::I64, mem(), addr, VAL_PAYLOAD);
        M { tag: Tag::Dyn(t), w }
    }

    /// Operand `k` from the top (1 = top), loaded once and then mirrored.
    fn operand(&mut self, k: usize) -> M {
        if let Some(m) = self.peek(k) {
            return m;
        }
        let at = self.top(k as i64);
        let m = self.load_m(at);
        self.set_peek(k, m);
        m
    }

    fn local_m(&mut self, l: u16) -> M {
        if let Some(m) = self.locs.get(&l) {
            return *m;
        }
        let at = self.local(l);
        let m = match self.static_tag(l) {
            Some(t) => self.known_at(at, t),
            None => self.load_m(at),
        };
        self.locs.insert(l, m);
        m
    }

    /// Static tag of local `l` from the checker (`slot_tags`), JIT only.
    fn static_tag(&self, l: u16) -> Option<u8> {
        let v = *self.stags.get(l as usize)?;
        let t = (v & 0xFF) as u8;
        (t != 0xFF).then_some(t)
    }

    /// Static element tag of the List in local `l`.
    fn static_elem(&self, l: u16) -> Option<u8> {
        let v = *self.stags.get(l as usize)?;
        let t = (v >> 8) as u8;
        ((v & 0xFF) as u8 == TAG_LIST && t != 0xFF).then_some(t)
    }

    /// Move the top operand (raw) to `dst`; a known plain mirror needs only
    /// the tag and the word.
    fn move_top_to(&mut self, dst: Value) {
        match self.peek(1) {
            Some(m @ M { tag: Tag::Known(t), .. }) if tag_is_plain(t) => self.store_plain(dst, m),
            _ => {
                let src = self.top(1);
                self.copy_value(dst, src);
            }
        }
    }

    /// Drop the old value of local `l` before it is overwritten. Without a
    /// mirror, a static tag still helps: the slot holds Unit (never stored)
    /// or a value of that tag.
    fn drop_local_old(&mut self, l: u16, old: Option<M>, at: Value) {
        match (old, self.static_tag(l)) {
            (None, Some(t)) if (self.plain_mask() >> t) & 1 == 1 => {}
            (None, Some(t)) => {
                let tg = self.b.ins().load(types::I8, mem(), at, 0);
                let set = self.b.ins().icmp_imm_u(IntCC::NotEqual, tg, TAG_UNIT as i64);
                let d = self.b.create_block();
                let done = self.b.create_block();
                self.b.ins().brif(set, d, &[], done, &[]);
                self.b.switch_to_block(d);
                self.drop_elem(at, Some(t));
                self.b.ins().jump(done, &[]);
                self.b.switch_to_block(done);
            }
            _ => self.drop_owned(old, at),
        }
    }

    /// Static tag of field `f` of the Record in local `l`.
    fn static_field(&self, l: u16, f: u16) -> Option<u8> {
        let v = *self.stags.get(l as usize)?;
        if (v & 0xFF) as u8 != TAG_RECORD || (v >> 8) == 0xFF {
            return None;
        }
        let r = self.prog.records.get((v >> 8) as usize)?;
        ty_tag(&r.fields.get(f as usize)?.1)
    }

    /// Mirror of a value at `addr` whose tag is statically `t` (word loaded;
    /// Bool words cleaned to 0/1).
    fn known_at(&mut self, addr: Value, t: u8) -> M {
        let w = self.b.ins().load(types::I64, mem(), addr, VAL_PAYLOAD);
        let w = if t == TAG_BOOL {
            let b = self.b.ins().ireduce(types::I8, w);
            self.b.ins().uextend(types::I64, b)
        } else {
            w
        };
        M { tag: Tag::Known(t), w }
    }

    /// Clone `src` (an element whose tag may be statically known) into `dst`.
    fn clone_elem(&mut self, dst: Value, src: Value, et: Option<u8>) -> M {
        match et {
            Some(t) => {
                let m = self.known_at(src, t);
                self.clone_into(dst, src, Some(m));
                m
            }
            None => self.clone_out(dst, src),
        }
    }

    /// Drop an element whose tag may be statically known.
    fn drop_elem(&mut self, at: Value, et: Option<u8>) {
        match et {
            Some(t) => {
                let w = self.b.ins().iconst(types::I64, 0);
                self.drop_owned(Some(M { tag: Tag::Known(t), w }), at);
            }
            None => self.drop_owned(None, at),
        }
    }

    fn pop_n(&mut self, n: usize) {
        for _ in 0..n {
            self.vs.pop();
        }
        self.bump_sp(-(n as i64));
    }

    fn known(&mut self, tag: u8, w: Value) -> M {
        let _ = self;
        M { tag: Tag::Known(tag), w }
    }

    /// Write a Known plain value (tag + payload word) to `addr`.
    fn store_plain(&mut self, addr: Value, m: M) {
        let Tag::Known(t) = m.tag else { unreachable!("store_plain of a dynamic tag") };
        // Whole first word (tag + zero padding): later 8-byte loads of the
        // slot are forwarded from this store instead of stalling.
        let tv = self.b.ins().iconst(types::I64, t as i64);
        self.b.ins().store(mem(), tv, addr, 0);
        self.b.ins().store(mem(), m.w, addr, VAL_PAYLOAD);
    }

    fn push_known(&mut self, tag: u8, w: Value) {
        let m = self.known(tag, w);
        self.push_slot(m);
        self.vs.push(Some(m));
    }

    fn is_tag(&mut self, m: M, t: u8) -> G {
        match m.tag {
            Tag::Known(k) if k == t => G::Yes,
            Tag::Known(_) => G::No,
            Tag::Dyn(v) => G::Dyn(self.b.ins().icmp_imm_u(IntCC::Equal, v, t as i64)),
        }
    }

    /// Tags copied and dropped raw: PLAIN_MASK, plus Task with a v4 layout.
    fn plain_mask(&self) -> u32 {
        if self.lay.is_some() { PLAIN_MASK | (1 << TAG_TASK) } else { PLAIN_MASK }
    }

    fn plain_bit(&mut self, tag: Value) -> Value {
        let tag = self.b.ins().uextend(types::I32, tag);
        let pm = self.plain_mask() as i64;
        let mask = self.b.ins().iconst(types::I32, pm);
        let bit = self.b.ins().ushr(mask, tag);
        self.b.ins().band_imm_u(bit, 1)
    }

    /// Plain tags (no ownership): Unit, Int, Bool, Dur = bit `tag` of PLAIN_MASK.
    fn is_plain(&mut self, m: Option<M>, addr: Value) -> G {
        match m.map(|m| m.tag) {
            Some(Tag::Known(t)) => {
                if (self.plain_mask() >> t) & 1 == 1 { G::Yes } else { G::No }
            }
            Some(Tag::Dyn(v)) => G::Dyn(self.plain_bit(v)),
            None => {
                let t = self.b.ins().load(types::I8, mem(), addr, 0);
                G::Dyn(self.plain_bit(t))
            }
        }
    }

    /// The mirror after a guard proved `m` has tag `t` (Bool words cleaned).
    fn refine(&mut self, m: M, t: u8) -> M {
        let w = match (m.tag, t) {
            (Tag::Dyn(_), TAG_BOOL) => {
                let b = self.b.ins().ireduce(types::I8, m.w);
                self.b.ins().uextend(types::I64, b)
            }
            _ => m.w,
        };
        M { tag: Tag::Known(t), w }
    }

    /// Tag value of `m` (or loaded from `addr`).
    fn tag_of(&mut self, m: Option<M>, addr: Value) -> Value {
        match m.map(|m| m.tag) {
            Some(Tag::Known(t)) => self.b.ins().iconst(types::I8, t as i64),
            Some(Tag::Dyn(v)) => v,
            None => self.b.ins().load(types::I8, mem(), addr, 0),
        }
    }

    /// Tags whose payload word is an `Rc` pointer to the allocation header
    /// (strong count first): Str, List, Map (RFC 0002 §3), plus Tuple and
    /// Record when the data layout (addendum v4) puts their pointer there.
    fn rc_mask(&self) -> u32 {
        match self.lay {
            Some(l) if l.fat_ptr == VAL_PAYLOAD => RC_MASK | (1 << TAG_TUPLE) | (1 << TAG_RECORD),
            _ => RC_MASK,
        }
    }

    /// Bit `tag` of `rc_mask()`.
    fn rc_bit(&mut self, tag: Value) -> Value {
        let tag = self.b.ins().uextend(types::I32, tag);
        let mk = self.rc_mask() as i64;
        let mask = self.b.ins().iconst(types::I32, mk);
        let bit = self.b.ins().ushr(mask, tag);
        self.b.ins().band_imm_u(bit, 1)
    }

    /// Inline `Rc` drop: strong > 1 just decrements; the last reference
    /// goes through `zal_rt_drop`, which frees.
    fn rc_drop(&mut self, addr: Value) {
        let p = self.b.ins().load(self.ptr, mem(), addr, VAL_PAYLOAD);
        self.rc_dec_at(addr, p);
    }

    /// Decrement the strong count at `p` (an Rc allocation owned by the value
    /// at `addr`); the last reference drops the whole value via the runtime.
    fn rc_dec_at(&mut self, addr: Value, p: Value) {
        let strong = self.b.ins().load(types::I64, mem(), p, 0);
        let shared = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, strong, 1);
        let dec_b = self.b.create_block();
        let last_b = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().brif(shared, dec_b, &[], last_b, &[]);
        self.b.switch_to_block(dec_b);
        let s1 = self.b.ins().iadd_imm_s(strong, -1);
        self.b.ins().store(mem(), s1, p, 0);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(last_b);
        self.b.ins().call(self.rt_drop, &[addr]);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    /// Inline `Rc` clone: raw copy and strong += 1.
    fn rc_clone(&mut self, dst: Value, src: Value) {
        self.copy_value(dst, src);
        let p = self.b.ins().load(self.ptr, mem(), src, VAL_PAYLOAD);
        let strong = self.b.ins().load(types::I64, mem(), p, 0);
        let s1 = self.b.ins().iadd_imm_s(strong, 1);
        self.b.ins().store(mem(), s1, p, 0);
    }

    /// Variant payload pointer (0 = none), addendum v4 layout.
    fn variant_ptr(&mut self, addr: Value) -> Value {
        let off = self.lay.expect("layout").variant_ptr;
        self.b.ins().load(self.ptr, mem(), addr, off)
    }

    /// Inline Variant clone: raw copy, payload strong += 1 when present.
    fn variant_clone(&mut self, dst: Value, src: Value) {
        self.copy_value(dst, src);
        let p = self.variant_ptr(src);
        let has = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().brif(p, has, &[], done, &[]);
        self.b.switch_to_block(has);
        let strong = self.b.ins().load(types::I64, mem(), p, 0);
        let s1 = self.b.ins().iadd_imm_s(strong, 1);
        self.b.ins().store(mem(), s1, p, 0);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    /// Inline Variant drop: nothing without payload, else an Rc decrement.
    fn variant_drop(&mut self, addr: Value) {
        let p = self.variant_ptr(addr);
        let has = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().brif(p, has, &[], done, &[]);
        self.b.switch_to_block(has);
        self.rc_dec_at(addr, p);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    /// Owned value: inline for Rc tags (and Variant with a data layout),
    /// `zal_rt_clone`/`zal_rt_drop` otherwise.
    fn owned_op(
        &mut self,
        m: Option<M>,
        addr: Value,
        inline: impl FnOnce(&mut Self),
        variant: impl FnOnce(&mut Self),
        call: impl FnOnce(&mut Self),
    ) {
        let has_var = self.lay.is_some();
        if let Some(Tag::Known(t)) = m.map(|m| m.tag) {
            if (self.rc_mask() >> t) & 1 == 1 {
                inline(self)
            } else if has_var && t == TAG_VARIANT {
                variant(self)
            } else {
                call(self)
            }
            return;
        }
        let tag = self.tag_of(m, addr);
        let rc = self.rc_bit(tag);
        let rc_b = self.b.create_block();
        let call_b = self.b.create_block();
        let done = self.b.create_block();
        self.b.ins().brif(rc, rc_b, &[], call_b, &[]);
        self.b.switch_to_block(rc_b);
        inline(self);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(call_b);
        if has_var {
            let is_var = self.b.ins().icmp_imm_u(IntCC::Equal, tag, TAG_VARIANT as i64);
            let var_b = self.b.create_block();
            let call2 = self.b.create_block();
            self.b.ins().brif(is_var, var_b, &[], call2, &[]);
            self.b.switch_to_block(var_b);
            variant(self);
            self.b.ins().jump(done, &[]);
            self.b.switch_to_block(call2);
        }
        call(self);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    fn drop_call(&mut self, m: Option<M>, addr: Value) {
        let drop = self.rt_drop;
        self.owned_op(m, addr, |g| g.rc_drop(addr), |g| g.variant_drop(addr), |g| {
            g.b.ins().call(drop, &[addr]);
        });
    }

    fn clone_call(&mut self, dst: Value, src: Value, m: Option<M>) {
        let clone = self.rt_clone;
        self.owned_op(m, src, |g| g.rc_clone(dst, src), |g| g.variant_clone(dst, src), |g| {
            g.b.ins().call(clone, &[dst, src]);
        });
    }

    /// Drop the value at `addr` (mirror `m`) if it owns anything.
    fn drop_owned(&mut self, m: Option<M>, addr: Value) {
        match self.is_plain(m, addr) {
            G::Yes => {}
            G::No => self.drop_call(m, addr),
            G::Dyn(p) => {
                let call_b = self.b.create_block();
                let done = self.b.create_block();
                self.b.ins().brif(p, done, &[], call_b, &[]);
                self.b.switch_to_block(call_b);
                self.drop_call(m, addr);
                self.b.ins().jump(done, &[]);
                self.b.switch_to_block(done);
            }
        }
    }

    /// `*dst = clone(*src)` into an uninitialised slot. Clone and drop never
    /// touch ctx or reallocate the stack, so no flush/reload around them.
    fn clone_into(&mut self, dst: Value, src: Value, m: Option<M>) {
        match self.is_plain(m, src) {
            G::Yes => match m {
                Some(m) => self.store_plain(dst, m),
                None => self.copy_value(dst, src),
            },
            G::No => self.clone_call(dst, src, m),
            G::Dyn(p) => {
                let raw_b = self.b.create_block();
                let call_b = self.b.create_block();
                let done = self.b.create_block();
                self.b.ins().brif(p, raw_b, &[], call_b, &[]);
                self.b.switch_to_block(raw_b);
                match m {
                    // Plain: tag byte and payload word are the whole value.
                    Some(M { tag: Tag::Dyn(t), w }) => {
                        let t64 = self.b.ins().uextend(types::I64, t);
                        self.b.ins().store(mem(), t64, dst, 0);
                        self.b.ins().store(mem(), w, dst, VAL_PAYLOAD);
                    }
                    _ => self.copy_value(dst, src),
                }
                self.b.ins().jump(done, &[]);
                self.b.switch_to_block(call_b);
                self.clone_call(dst, src, m);
                self.b.ins().jump(done, &[]);
                self.b.switch_to_block(done);
            }
        }
    }

    // ---- control ----

    fn pc_block(&self, pc: usize) -> Block {
        if pc < self.blocks.len() { self.blocks[pc] } else { self.bad }
    }

    fn exit_with(&mut self, r: Value) {
        self.b.ins().jump(self.exit, &[BlockArg::Value(r)]);
    }

    /// Taken branch from op `pc` to `t` (a leader); backward edges (t <= pc,
    /// the VM's test after its pc increment) go through `zal_rt_tick`, which
    /// gets the target as resume pc.
    fn goto(&mut self, pc: usize, t: u32) {
        self.emit_pending();
        let target = self.pc_block(t as usize);
        if (t as usize) > pc {
            self.b.ins().jump(target, &[]);
            return;
        }
        if self.v2.is_some() {
            // Addendum v2 §1: count inline, call the runtime only at the limit.
            let sl = self.b.ins().load(types::I32, mem(), self.ctx, CTX_SLICE);
            let sl = self.b.ins().iadd_imm_s(sl, 1);
            self.b.ins().store(mem(), sl, self.ctx, CTX_SLICE);
            let due = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, sl, ZAL_SLICE as i64);
            let tick_b = self.b.create_block();
            self.b.ins().brif(due, tick_b, &[], target, &[]);
            self.b.switch_to_block(tick_b);
        }
        self.flush();
        let tv = self.b.ins().iconst(types::I32, t as i64);
        let call = self.b.ins().call(self.rt_tick, &[self.ctx, tv]);
        let r = self.b.inst_results(call)[0];
        self.reload();
        let leave = self.b.create_block();
        self.b.ins().brif(r, leave, &[], target, &[]);
        self.b.switch_to_block(leave);
        self.exit_with(r);
    }

    fn trap(&mut self, pc: usize, code: u32) {
        self.flush();
        let p = self.b.ins().iconst(types::I32, pc as i64);
        let c = self.b.ins().iconst(types::I32, code as i64);
        let call = self.b.ins().call(self.rt_trap, &[self.ctx, p, c]);
        let r = self.b.inst_results(call)[0];
        self.exit_with(r);
    }

    /// Call `zal_rt_op` for op `pc`; on 0 continue at `cont` (built by the
    /// caller after reload), on 6 at the static target or via ctx->pc, else exit.
    fn rt_op_call(&mut self, pc: usize, cont: impl FnOnce(&mut Self)) {
        // The op changes operand memory and the flow continues: no lazy slots.
        self.sync();
        self.flush();
        let p = self.b.ins().iconst(types::I32, pc as i64);
        let call = self.b.ins().call(self.rt_op, &[self.ctx, p]);
        let r = self.b.inst_results(call)[0];
        self.reload();
        let not_zero = self.b.create_block();
        let zero = self.b.create_block();
        self.b.ins().brif(r, not_zero, &[], zero, &[]);
        self.b.switch_to_block(not_zero);
        let jumped = match branch_target(&self.code[pc]) {
            Some(t) => self.pc_block(t as usize),
            None => self.redispatch,
        };
        let is_jump = self.b.ins().icmp_imm_u(IntCC::Equal, r, JUMP as i64);
        let leave = self.b.create_block();
        self.b.ins().brif(is_jump, jumped, &[], leave, &[]);
        self.b.switch_to_block(leave);
        self.exit_with(r);
        self.b.switch_to_block(zero);
        cont(self);
    }

    /// Op `pc` through the runtime (shared step block), then on through
    /// dispatch(pc+1). Ends a region; memory is canonical, so no flush here.
    fn generic(&mut self, pc: usize) {
        // Static mode: the shared step block cannot know sp, so store it here.
        if self.stat.is_some() {
            self.flush();
        } else {
            self.emit_pending();
        }
        let p = self.b.ins().iconst(types::I32, pc as i64);
        self.b.ins().jump(self.step, &[BlockArg::Value(p)]);
    }

    /// Replace every mirror entry whose tag is the dynamic value `v` by the
    /// refined entry (a guard proved that tag).
    fn refine_all(&mut self, v: Value, t: u8) {
        let same = |m: &M| matches!(m.tag, Tag::Dyn(x) if x == v);
        let vs: Vec<Option<M>> = self.vs.clone();
        for (i, e) in vs.into_iter().enumerate() {
            if let Some(m) = e.filter(same) {
                self.vs[i] = Some(self.refine(m, t));
            }
        }
        let ls: Vec<(u16, M)> = self.locs.iter().map(|(k, m)| (*k, *m)).filter(|(_, m)| same(m)).collect();
        for (k, m) in ls {
            let r = self.refine(m, t);
            self.locs.insert(k, r);
        }
    }

    /// Guarded inline op on operands `checks` (mirror, required tag).
    /// `fast` gets the refined operands, emits the inline code (memory
    /// writes, sp) into an open block, leaves it open and returns the pushed
    /// result, if any; `pops` operand slots are consumed.
    ///
    /// When a guard fails at run time: `Slow::Deopt` runs the op through
    /// `zal_rt_op` and continues in the step chain (blocks[pc+1]), so the
    /// fast continuation keeps exact, refined state; `Slow::JoinBool` (ops
    /// whose result is a Bool either way) rejoins with the Bool as a block
    /// parameter. Returns false when the region ended here.
    fn guarded(&mut self, pc: usize, checks: &[(M, u8)], pops: usize, slow: Slow, fast: impl FnOnce(&mut Self, &[M]) -> Option<M>) -> bool {
        let mut conds = Vec::new();
        let mut never = false;
        for &(m, t) in checks {
            match self.is_tag(m, t) {
                G::Yes => {}
                G::No => never = true,
                G::Dyn(c) => conds.push((c, m, t)),
            }
        }
        if never {
            return match slow {
                Slow::Deopt => {
                    self.generic(pc);
                    false
                }
                Slow::JoinBool => {
                    let join = self.b.create_block();
                    self.b.append_block_param(join, types::I64);
                    self.slow_bool(pc, join, 1 - pops as i64);
                    self.b.switch_to_block(join);
                    self.after_bool_join(join, pops);
                    true
                }
            };
        }
        let refined: Vec<M> = checks.iter().map(|&(m, t)| self.refine(m, t)).collect();
        if conds.is_empty() {
            let r = fast(self, &refined);
            for _ in 0..pops {
                self.vs.pop();
            }
            if let Some(r) = r {
                self.vs.push(Some(r));
            }
            return true;
        }
        // Both sides of a dynamic guard start from canonical memory.
        self.sync();
        let mut ok = conds[0].0;
        for &(c, _, _) in &conds[1..] {
            ok = self.b.ins().band(ok, c);
        }
        let fast_b = self.b.create_block();
        let slow_b = self.b.create_block();
        self.b.ins().brif(ok, fast_b, &[], slow_b, &[]);
        match slow {
            Slow::Deopt => {
                self.b.switch_to_block(slow_b);
                self.generic(pc);
                self.b.switch_to_block(fast_b);
                for &(_, m, t) in &conds {
                    if let Tag::Dyn(v) = m.tag {
                        self.refine_all(v, t);
                    }
                }
                let refined: Vec<M> = checks.iter().map(|&(m, t)| self.refine(m, t)).collect();
                let r = fast(self, &refined);
                for _ in 0..pops {
                    self.vs.pop();
                }
                if let Some(r) = r {
                    self.vs.push(Some(r));
                }
                true
            }
            Slow::JoinBool => {
                let join = self.b.create_block();
                self.b.append_block_param(join, types::I64);
                self.b.switch_to_block(fast_b);
                // sp after the join is the fast arm's (the runtime leaves the same).
                let pre = self.depth;
                let saved = (self.vs.clone(), self.locs.clone());
                self.in_arm = true;
                let r = fast(self, &refined).expect("JoinBool op pushes a Bool");
                self.in_arm = false;
                (self.vs, self.locs) = saved;
                self.b.ins().jump(join, &[BlockArg::Value(r.w)]);
                self.b.switch_to_block(slow_b);
                // The slow arm runs the op from its operands (pre-op sp).
                let post = self.depth;
                self.depth = pre;
                self.slow_bool(pc, join, post - pre);
                self.b.switch_to_block(join);
                self.after_bool_join(join, pops);
                true
            }
        }
    }

    /// Slow path of a Bool-producing op: runtime, then the join with the
    /// result's payload byte (the runtime writes only the byte).
    fn slow_bool(&mut self, pc: usize, join: Block, adj: i64) {
        self.rt_op_call(pc, |g| {
            g.adjust_static(adj);
            let at = g.top(1);
            let b = g.b.ins().load(types::I8, mem(), at, VAL_PAYLOAD);
            let w = g.b.ins().uextend(types::I64, b);
            g.b.ins().jump(join, &[BlockArg::Value(w)]);
        });
    }

    fn after_bool_join(&mut self, join: Block, pops: usize) {
        for _ in 0..pops {
            self.vs.pop();
        }
        let w = self.b.block_params(join)[0];
        self.vs.push(Some(M { tag: Tag::Known(TAG_BOOL), w }));
    }

    // ---- ops ----

    fn load_local(&mut self, l: u16) {
        let m = self.local_m(l);
        if let Tag::Known(t) = m.tag {
            if tag_is_plain(t) && self.lazy {
                self.push_known(t, m.w);
                return;
            }
        }
        let src = self.local(l);
        let dst = self.top(0);
        self.clone_into(dst, src, Some(m));
        self.vs.push(Some(m));
        self.bump_sp(1);
    }

    /// Store the top (moved, any tag) into local `l`, dropping the old value.
    fn store_local(&mut self, l: u16, keep_top: bool) {
        let e = self.peek(1);
        let old = self.locs.get(&l).copied();
        let dst = self.local(l);
        self.drop_local_old(l, old, dst);
        if let Some(m @ M { tag: Tag::Known(t), .. }) = e {
            if tag_is_plain(t) && self.lazy {
                // Plain: copy from the mirror, no operand memory involved.
                let dst = self.local(l);
                self.store_plain(dst, m);
                self.locs.insert(l, m);
                if !keep_top {
                    self.pop_n(1);
                }
                return;
            }
        }
        let dst = self.local(l);
        let src = self.top(1);
        if keep_top {
            self.clone_into(dst, src, e);
        } else {
            match e {
                Some(m @ M { tag: Tag::Known(t), .. }) if tag_is_plain(t) => self.store_plain(dst, m),
                _ => self.copy_value(dst, src),
            }
        }
        match e {
            Some(m) => {
                self.locs.insert(l, m);
            }
            None => {
                self.locs.remove(&l);
            }
        }
        if !keep_top {
            self.pop_n(1);
        }
    }

    /// Move local `l` to the top (any tag), leaving Unit.
    fn take_local(&mut self, l: u16) {
        let m = self.locs.get(&l).copied();
        let src = self.local(l);
        let dst = self.top(0);
        match m {
            Some(m @ M { tag: Tag::Known(t), .. }) if tag_is_plain(t) => self.store_plain(dst, m),
            _ => self.copy_value(dst, src),
        }
        let unit = self.b.ins().iconst(types::I64, TAG_UNIT as i64);
        self.b.ins().store(mem(), unit, src, 0);
        self.vs.push(m);
        self.bump_sp(1);
        let z = self.b.ins().iconst(types::I64, 0);
        let u = self.known(TAG_UNIT, z);
        self.locs.insert(l, u);
    }

    fn next_is(&self, pc: usize, f: impl Fn(&Op) -> bool) -> bool {
        pc + 1 < self.code.len() && !self.leader[pc + 1] && f(&self.code[pc + 1])
    }

    /// LoadLocal, fused with a following Ret (move instead of clone: the
    /// frame dies) or TagIs (read the tag in place: no clone + drop).
    fn load_local_fused(&mut self, pc: usize, l: u16) -> bool {
        if self.next_is(pc, |o| matches!(o, Op::Ret)) {
            self.take_local(l);
            return true;
        }
        if let (Some(v2), true) = (self.v2, self.next_is(pc, |o| matches!(o, Op::TagIs(_)))) {
            let Op::TagIs(t) = self.code[pc + 1] else { unreachable!() };
            let at = self.local(l);
            let tg = self.call1(v2.tag, &[at]);
            let ok = self.b.ins().icmp_imm_u(IntCC::NotEqual, tg, TAG_NONE as i64);
            self.helper_ok(pc, ok);
            let hit = self.b.ins().icmp_imm_u(IntCC::Equal, tg, t as i64);
            let w = self.b.ins().uextend(types::I64, hit);
            self.push_known(TAG_BOOL, w);
            self.skip = 1;
            return true;
        }
        self.load_local(l);
        true
    }

    /// StoreLoad fused with a following JumpIfNotTag: move the top into the
    /// local and test the tag there (no clone + drop of the top).
    fn store_load_fused(&mut self, pc: usize, l: u16) -> bool {
        let Some(v2) = self.v2 else {
            self.store_local(l, true);
            return true;
        };
        if !self.next_is(pc, |o| matches!(o, Op::JumpIfNotTag(..))) {
            self.store_local(l, true);
            return true;
        }
        let Op::JumpIfNotTag(tag, t) = self.code[pc + 1] else { unreachable!() };
        let at = self.top(1);
        let tg = self.call1(v2.tag, &[at]);
        let ok = self.b.ins().icmp_imm_u(IntCC::NotEqual, tg, TAG_NONE as i64);
        self.helper_ok(pc, ok);
        self.store_local(l, false);
        let hit = self.b.ins().icmp_imm_u(IntCC::Equal, tg, tag as i64);
        let taken = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(hit, cont, &[], taken, &[]);
        self.b.switch_to_block(taken);
        self.goto(pc + 1, t);
        self.b.switch_to_block(cont);
        self.skip = 1;
        true
    }

    /// Emit op `pc` inside a region. Returns false when the op ended the region.
    fn emit_op(&mut self, pc: usize) -> bool {
        let op = self.code[pc];
        if !self.pending.is_empty() && !self.lazy_ok(&op) {
            self.sync();
        }
        match op {
            Op::Const(c) => match &self.prog.consts[c as usize] {
                Const::Int(v) => {
                    let v = self.b.ins().iconst(types::I64, *v);
                    self.push_known(TAG_INT, v);
                }
                Const::Dur(v) => {
                    let v = self.b.ins().iconst(types::I64, *v);
                    self.push_known(TAG_DUR, v);
                }
                Const::Bool(v) => {
                    let v = self.b.ins().iconst(types::I64, *v as i64);
                    self.push_known(TAG_BOOL, v);
                }
                Const::Str(_) if self.kconst.is_some() => {
                    self.push_kconst(c as usize, TAG_STR);
                }
                Const::Str(_) => {
                    let Some(v2) = self.v2 else {
                        self.generic(pc);
                        return false;
                    };
                    let dst = self.top(0);
                    let idx = self.b.ins().iconst(types::I32, c as i64);
                    self.b.ins().call(v2.konst, &[dst, idx, self.ctx]);
                    self.vs.push(None);
                    self.bump_sp(1);
                }
            },
            Op::Unit => {
                let z = self.b.ins().iconst(types::I64, 0);
                self.push_known(TAG_UNIT, z);
            }
            Op::LoadLocal(l) => return self.load_local_fused(pc, l),
            Op::LoadLocal2(a, b) => {
                self.load_local(a);
                self.load_local(b);
            }
            Op::TakeLocal(l) => self.take_local(l),
            Op::StoreLocal(l) => self.store_local(l, false),
            Op::StoreLoad(l) => return self.store_load_fused(pc, l),
            Op::Pop if matches!(self.peek(1), Some(M { tag: Tag::Known(t), .. }) if tag_is_plain(t)) => {
                self.pop_n(1);
            }
            Op::Pop => {
                let e = self.peek(1);
                let at = self.top(1);
                self.drop_owned(e, at);
                self.pop_n(1);
            }
            Op::IncLocal(l, k) => {
                let m = self.local_m(l);
                return self.guarded(pc, &[(m, TAG_INT)], 0, Slow::Deopt, |g, ms| {
                    let kv = g.b.ins().iconst(types::I64, k as i64);
                    let (r, of) = g.b.ins().sadd_overflow(ms[0].w, kv);
                    let ok_b = g.b.create_block();
                    let of_b = g.b.create_block();
                    g.b.ins().brif(of, of_b, &[], ok_b, &[]);
                    g.b.switch_to_block(of_b);
                    g.trap(pc, TRAP_ADD);
                    g.b.switch_to_block(ok_b);
                    let at = g.local(l);
                    g.b.ins().store(mem(), r, at, VAL_PAYLOAD);
                    let nm = g.known(TAG_INT, r);
                    g.locs.insert(l, nm);
                    None
                });
            }
            Op::Add | Op::Sub | Op::Mul => {
                let a = self.operand(2);
                let b = self.operand(1);
                return self.guarded(pc, &[(a, TAG_INT), (b, TAG_INT)], 2, Slow::Deopt, |g, ms| {
                    let (x, y) = (ms[0].w, ms[1].w);
                    let (r, of, code) = match op {
                        Op::Add => {
                            let (r, of) = g.b.ins().sadd_overflow(x, y);
                            (r, of, TRAP_ADD)
                        }
                        Op::Sub => {
                            let (r, of) = g.b.ins().ssub_overflow(x, y);
                            (r, of, TRAP_SUB)
                        }
                        _ => {
                            let (r, of) = g.b.ins().smul_overflow(x, y);
                            (r, of, TRAP_MUL)
                        }
                    };
                    let ok_b = g.b.create_block();
                    let of_b = g.b.create_block();
                    g.b.ins().brif(of, of_b, &[], ok_b, &[]);
                    g.b.switch_to_block(of_b);
                    // The VM has popped both (plain) operands when it traps.
                    let saved = (g.pending.clone(), g.depth);
                    g.bump_sp(-2);
                    g.trap(pc, code);
                    (g.pending, g.depth) = saved;
                    g.b.switch_to_block(ok_b);
                    let m = g.known(TAG_INT, r);
                    if g.lazy {
                        g.bump_sp(-2);
                        g.push_slot(m);
                    } else {
                        // The slot of `a` already holds the Int tag in memory.
                        let dst = g.top(2);
                        g.b.ins().store(mem(), r, dst, VAL_PAYLOAD);
                        g.bump_sp(-1);
                    }
                    Some(m)
                });
            }
            Op::Neg => {
                // Int only (the VM wraps MIN to MIN); Dur and the rest deopt.
                let a = self.operand(1);
                return self.guarded(pc, &[(a, TAG_INT)], 1, Slow::Deopt, |g, ms| {
                    let r = g.b.ins().ineg(ms[0].w);
                    let m = g.known(TAG_INT, r);
                    g.bump_sp(-1);
                    g.push_slot(m);
                    Some(m)
                });
            }
            Op::Div | Op::Mod => {
                let a = self.operand(2);
                let b = self.operand(1);
                return self.guarded(pc, &[(a, TAG_INT), (b, TAG_INT)], 2, Slow::Deopt, |g, ms| {
                    let (x, y) = (ms[0].w, ms[1].w);
                    // Zero divisor and MIN / -1 trap in the VM (checked_div /
                    // checked_rem): run those through the runtime for its message.
                    let zero = g.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    let min = g.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                    let m1 = g.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                    let ovf = g.b.ins().band(min, m1);
                    let bad = g.b.ins().bor(zero, ovf);
                    let ok = g.b.ins().bxor_imm_s(bad, 1);
                    g.helper_ok(pc, ok);
                    let r = if matches!(op, Op::Div) { g.b.ins().sdiv(x, y) } else { g.b.ins().srem(x, y) };
                    let m = g.known(TAG_INT, r);
                    g.bump_sp(-2);
                    g.push_slot(m);
                    Some(m)
                });
            }
            Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Eq | Op::Ne => {
                let cc = match op {
                    Op::Lt => IntCC::SignedLessThan,
                    Op::Le => IntCC::SignedLessThanOrEqual,
                    Op::Gt => IntCC::SignedGreaterThan,
                    Op::Ge => IntCC::SignedGreaterThanOrEqual,
                    Op::Eq => IntCC::Equal,
                    _ => IntCC::NotEqual,
                };
                let a = self.operand(2);
                let b = self.operand(1);
                return self.guarded(pc, &[(a, TAG_INT), (b, TAG_INT)], 2, Slow::JoinBool, |g, ms| {
                    let r = g.b.ins().icmp(cc, ms[0].w, ms[1].w);
                    let r = g.b.ins().uextend(types::I64, r);
                    let m = g.known(TAG_BOOL, r);
                    g.bump_sp(-2);
                    g.push_slot(m);
                    Some(m)
                });
            }
            Op::Jump(t) => {
                self.goto(pc, t);
                return false;
            }
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                let e = self.operand(1);
                return self.guarded(pc, &[(e, TAG_BOOL)], 1, Slow::Deopt, |g, ms| {
                    let v = ms[0].w;
                    g.bump_sp(-1);
                    let taken = g.b.create_block();
                    let cont = g.b.create_block();
                    if matches!(op, Op::JumpIfTrue(_)) {
                        g.b.ins().brif(v, taken, &[], cont, &[]);
                    } else {
                        g.b.ins().brif(v, cont, &[], taken, &[]);
                    }
                    g.b.switch_to_block(taken);
                    g.goto(pc, t);
                    g.b.switch_to_block(cont);
                    None
                });
            }
            Op::JumpIfNotLt(t) => {
                let a = self.operand(2);
                let b = self.operand(1);
                return self.guarded(pc, &[(a, TAG_INT), (b, TAG_INT)], 2, Slow::Deopt, |g, ms| {
                    g.bump_sp(-2);
                    let lt = g.b.ins().icmp(IntCC::SignedLessThan, ms[0].w, ms[1].w);
                    let taken = g.b.create_block();
                    let cont = g.b.create_block();
                    g.b.ins().brif(lt, cont, &[], taken, &[]);
                    g.b.switch_to_block(taken);
                    g.goto(pc, t);
                    g.b.switch_to_block(cont);
                    None
                });
            }
            _ => {
                if self.v2.is_some() {
                    return self.emit_v2(pc);
                }
                self.generic(pc);
                return false;
            }
        }
        true
    }

    fn call1(&mut self, f: FuncRef, args: &[Value]) -> Value {
        let c = self.b.ins().call(f, args);
        self.b.inst_results(c)[0]
    }

    /// Continue in a fresh block when `ok`, else run op `pc` through the
    /// runtime and on into the step chain (the helper had no effect).
    fn helper_ok(&mut self, pc: usize, ok: Value) {
        let fast_b = self.b.create_block();
        let slow_b = self.b.create_block();
        self.b.ins().brif(ok, fast_b, &[], slow_b, &[]);
        self.b.switch_to_block(slow_b);
        self.generic(pc);
        self.b.switch_to_block(fast_b);
    }

    fn is_zero(&mut self, r: Value) -> Value {
        self.b.ins().icmp_imm_u(IntCC::Equal, r, 0)
    }

    /// Ops through the addendum v2 helpers. Helpers answering "not
    /// applicable" leave memory untouched, so the fallback is the generic op.
    fn emit_v2(&mut self, pc: usize) -> bool {
        if self.lay.is_some() {
            if let Some(r) = self.emit_v4(pc) {
                return r;
            }
        }
        let v2 = self.v2.expect("v2 helpers");
        let v3 = self.v3.expect("v3 helpers");
        let op = self.code[pc];
        match op {
            Op::LenLocal(a) => {
                let at = self.local(a);
                let n = self.call1(v2.len, &[at]);
                let ok = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, n, 0);
                self.helper_ok(pc, ok);
                self.push_known(TAG_INT, n);
            }
            Op::IndexLL(a, b) => {
                let mb = self.local_m(b);
                return self.guarded(pc, &[(mb, TAG_INT)], 0, Slow::Deopt, |g, ms| {
                    let dst = g.top(0);
                    let list = g.local(a);
                    let r = g.call1(v2.index, &[dst, list, ms[0].w]);
                    let ok = g.is_zero(r);
                    g.helper_ok(pc, ok);
                    g.vs.push(None);
                    g.bump_sp(1);
                    None
                });
            }
            Op::LoadField(a, f) => {
                let dst = self.top(0);
                let rec = self.local(a);
                let fi = self.b.ins().iconst(types::I32, f as i64);
                let r = self.call1(v2.field, &[dst, rec, fi]);
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                self.vs.push(None);
                self.bump_sp(1);
            }
            Op::LoadPayload(a) => {
                let dst = self.top(0);
                let v = self.local(a);
                let r = self.call1(v2.payload, &[dst, v]);
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                self.vs.push(None);
                self.bump_sp(1);
            }
            Op::MovePayload(a, b) => {
                // Payload into the free slot above the top, then into local b.
                let tmp = self.top(0);
                let v = self.local(a);
                let r = self.call1(v2.payload, &[tmp, v]);
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                let old = self.locs.get(&b).copied();
                let dst = self.local(b);
                self.drop_owned(old, dst);
                let dst = self.local(b);
                let tmp = self.top(0);
                self.copy_value(dst, tmp);
                self.locs.remove(&b);
            }
            Op::ForNext(b, t) => {
                if b > u16::MAX - 2 {
                    self.generic(pc);
                    return false;
                }
                let at = self.local(b);
                let r = self.call1(v2.for_next, &[at]);
                let cont = self.b.create_block();
                let not0 = self.b.create_block();
                self.b.ins().brif(r, not0, &[], cont, &[]);
                self.b.switch_to_block(not0);
                let exhausted = self.b.ins().icmp_imm_u(IntCC::Equal, r, 1);
                let taken = self.b.create_block();
                let slow = self.b.create_block();
                self.b.ins().brif(exhausted, taken, &[], slow, &[]);
                self.b.switch_to_block(slow);
                self.generic(pc);
                self.b.switch_to_block(taken);
                self.goto(pc, t);
                self.b.switch_to_block(cont);
                self.locs.remove(&(b + 1));
                self.locs.remove(&(b + 2));
            }
            Op::UpdLocal(bi, a) => {
                let n: usize = match bi {
                    zal_ir::Builtin::Push | zal_ir::Builtin::Remove => 1,
                    _ => 2,
                };
                let local = self.local(a);
                let args = self.top(n as i64);
                let code = match bi {
                    zal_ir::Builtin::Push => zal_vm::native::ZAL_BUILTIN_PUSH,
                    zal_ir::Builtin::Set => zal_vm::native::ZAL_BUILTIN_SET,
                    zal_ir::Builtin::Remove => zal_vm::native::ZAL_BUILTIN_REMOVE,
                    _ => zal_vm::native::ZAL_BUILTIN_MAP_SET,
                };
                let code = self.b.ins().iconst(types::I32, code as i64);
                let r = self.call1(v2.upd, &[local, args, code]);
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                self.locs.remove(&a);
                self.pop_n(n);
            }
            Op::JumpIfNotTag(tag, t) => {
                let e = self.peek(1);
                let at = self.top(1);
                let tg = self.call1(v2.tag, &[at]);
                let ok = self.b.ins().icmp_imm_u(IntCC::NotEqual, tg, TAG_NONE as i64);
                self.helper_ok(pc, ok);
                let at = self.top(1);
                self.drop_owned(e, at);
                self.pop_n(1);
                let hit = self.b.ins().icmp_imm_u(IntCC::Equal, tg, tag as i64);
                let taken = self.b.create_block();
                let cont = self.b.create_block();
                self.b.ins().brif(hit, cont, &[], taken, &[]);
                self.b.switch_to_block(taken);
                self.goto(pc, t);
                self.b.switch_to_block(cont);
            }
            Op::MapGetJump(r, t) => {
                self.flush();
                let lv = self.b.ins().iconst(types::I32, r as i64);
                let res = self.call1(v3.map_get_jump, &[self.ctx, lv]);
                self.reload();
                let ok = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, res, 2);
                self.helper_ok(pc, ok);
                self.after_rt(pc);
                self.locs.remove(&r);
                for _ in 0..2 {
                    self.vs.pop();
                }
                let taken = self.b.create_block();
                let cont = self.b.create_block();
                self.b.ins().brif(res, taken, &[], cont, &[]);
                self.b.switch_to_block(taken);
                self.goto(pc, t);
                self.b.switch_to_block(cont);
            }
            Op::MakeList(n) | Op::MakeTuple(n) | Op::MakeRecord(n) | Op::MakeVariant(n, _) => {
                let (kind, arg, pops) = match op {
                    Op::MakeList(_) => (zal_vm::native::ZAL_MAKE_LIST, n as u32, n as usize),
                    Op::MakeTuple(_) => (zal_vm::native::ZAL_MAKE_TUPLE, n as u32, n as usize),
                    Op::MakeRecord(_) => (zal_vm::native::ZAL_MAKE_RECORD, n as u32, n as usize),
                    Op::MakeVariant(tag, has) => (zal_vm::native::ZAL_MAKE_VARIANT, tag as u32 | ((has as u32) << 16), has as usize),
                    _ => unreachable!(),
                };
                self.flush();
                let k = self.b.ins().iconst(types::I32, kind as i64);
                let a = self.b.ins().iconst(types::I32, arg as i64);
                let r = self.call1(v3.make, &[self.ctx, k, a]);
                self.reload();
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                self.after_rt(pc);
                for _ in 0..pops {
                    self.vs.pop();
                }
                self.vs.push(None);
            }
            Op::Builtin(bi, argc) if !builtin_blocks(bi) => {
                self.flush();
                let b = self.b.ins().iconst(types::I32, bi as u32 as i64);
                let a = self.b.ins().iconst(types::I32, argc as i64);
                let r = self.call1(v3.builtin, &[self.ctx, b, a]);
                self.reload();
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                self.after_rt(pc);
                for _ in 0..argc {
                    self.vs.pop();
                }
                self.vs.push(None);
            }
            Op::TagIs(t) => {
                let e = self.peek(1);
                let at = self.top(1);
                let tg = self.call1(v2.tag, &[at]);
                let ok = self.b.ins().icmp_imm_u(IntCC::NotEqual, tg, TAG_NONE as i64);
                self.helper_ok(pc, ok);
                let at = self.top(1);
                self.drop_owned(e, at);
                self.pop_n(1);
                let hit = self.b.ins().icmp_imm_u(IntCC::Equal, tg, t as i64);
                let w = self.b.ins().uextend(types::I64, hit);
                self.push_known(TAG_BOOL, w);
            }
            Op::Not => {
                let e = self.operand(1);
                return self.guarded(pc, &[(e, TAG_BOOL)], 1, Slow::Deopt, |g, ms| {
                    let r = g.b.ins().bxor_imm_u(ms[0].w, 1);
                    let m = g.known(TAG_BOOL, r);
                    let at = g.top(1);
                    g.store_plain(at, m);
                    Some(m)
                });
            }
            Op::ConcatN(n) => {
                self.flush();
                let nv = self.b.ins().iconst(types::I32, n as i64);
                let r = self.call1(v2.concat, &[self.ctx, nv]);
                self.reload();
                let ok = self.is_zero(r);
                self.helper_ok(pc, ok);
                self.after_rt(pc);
                for _ in 0..n {
                    self.vs.pop();
                }
                self.vs.push(None);
            }
            Op::Call(f, argc) => {
                self.flush();
                let p = self.b.ins().iconst(types::I32, pc as i64);
                let fv = self.b.ins().iconst(types::I32, f as i64);
                let av = self.b.ins().iconst(types::I32, argc as i64);
                let r = self.call1(v2.call, &[self.ctx, p, fv, av]);
                self.exit_with(r);
                return false;
            }
            Op::Ret => {
                self.flush();
                let r = self.call1(v2.ret, &[self.ctx]);
                self.exit_with(r);
                return false;
            }
            _ => {
                // Op without inline code: call the runtime here and continue
                // straight at the next block (no trip through the shared
                // step block and the dispatch table).
                let next = self.pc_block(pc + 1);
                self.rt_op_call(pc, |g| {
                    g.b.ins().jump(next, &[]);
                });
                return false;
            }
        }
        true
    }

    // ---- addendum v4: inline data access ----

    /// Clone the value at `src` into `dst`, returning its mirror (tag and
    /// word loaded once; later guards on the copy need no reload).
    fn clone_out(&mut self, dst: Value, src: Value) -> M {
        let m = self.load_m(src);
        self.clone_into(dst, src, Some(m));
        m
    }

    /// Push a clone of `jit_constants()[i]` (an Rc-owned value with tag `t`).
    fn push_kconst(&mut self, i: usize, t: u8) {
        let base = self.kconst.expect("constants");
        let src = self.b.ins().iconst(self.ptr, (base + i * VALUE_SIZE as usize) as i64);
        let dst = self.top(0);
        let w = self.b.ins().iconst(types::I64, 0);
        // Known tag only selects the inline Rc clone; the mirror stays unknown.
        self.clone_into(dst, src, Some(M { tag: Tag::Known(t), w }));
        self.vs.push(None);
        self.bump_sp(1);
    }

    /// Address of element `i` (i64) of the list whose Rc allocation is `p`.
    fn list_elem(&mut self, p: Value, i: Value) -> Value {
        let l = self.lay.expect("layout");
        let buf = self.b.ins().load(self.ptr, mem(), p, l.list_ptr);
        let off = self.b.ins().imul_imm_s(i, VALUE_SIZE);
        self.b.ins().iadd(buf, off)
    }

    fn list_len(&mut self, p: Value) -> Value {
        let l = self.lay.expect("layout");
        self.b.ins().load(types::I64, mem(), p, l.list_len)
    }

    /// Continue in a fresh block when `ok`, else run op `pc` generically
    /// (nothing has been changed yet).
    fn or_generic(&mut self, pc: usize, ok: Value) {
        self.helper_ok(pc, ok);
    }

    /// Address of field `f` of the record/tuple at `addr` whose allocation
    /// pointer is `p`; runs the op generically when `f` is out of range.
    fn agg_field(&mut self, pc: usize, addr: Value, p: Value, f: u16) -> Value {
        let l = self.lay.expect("layout");
        let n = self.b.ins().load(types::I64, mem(), addr, l.fat_len);
        let ok = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, n, f as i64);
        self.or_generic(pc, ok);
        self.b.ins().iadd_imm_s(p, 16 + f as i64 * VALUE_SIZE)
    }

    /// Allocation pointer of the record/tuple at `addr` (mirror word `w`).
    fn agg_ptr(&mut self, addr: Value, w: Value) -> Value {
        let l = self.lay.expect("layout");
        if l.fat_ptr == VAL_PAYLOAD { w } else { self.b.ins().load(self.ptr, mem(), addr, l.fat_ptr) }
    }

    /// Unique list (strong == weak == 1) with room for one more element.
    fn list_unique(&mut self, p: Value) -> Value {
        let strong = self.b.ins().load(types::I64, mem(), p, 0);
        let weak = self.b.ins().load(types::I64, mem(), p, 8);
        let both = self.b.ins().band(strong, weak);
        let either = self.b.ins().bor(strong, weak);
        // strong, weak >= 1 always: both == 1 && either == 1 <=> both are 1.
        let a = self.b.ins().icmp_imm_u(IntCC::Equal, both, 1);
        let b = self.b.ins().icmp_imm_u(IntCC::Equal, either, 1);
        self.b.ins().band(a, b)
    }

    /// `zal_rt_upd` for local `a` with `n` arguments on top (the helper copes
    /// with shared lists and growth); the generic op when not applicable.
    fn upd_helper(&mut self, pc: usize, bi: zal_ir::Builtin, a: u16, n: usize, join: Block) {
        let v2 = self.v2.expect("v2 helpers");
        let local = self.local(a);
        let args = self.top(n as i64);
        let code = match bi {
            zal_ir::Builtin::Push => zal_vm::native::ZAL_BUILTIN_PUSH,
            zal_ir::Builtin::Set => zal_vm::native::ZAL_BUILTIN_SET,
            zal_ir::Builtin::Remove => zal_vm::native::ZAL_BUILTIN_REMOVE,
            _ => zal_vm::native::ZAL_BUILTIN_MAP_SET,
        };
        let code = self.b.ins().iconst(types::I32, code as i64);
        let r = self.call1(v2.upd, &[local, args, code]);
        let ok = self.is_zero(r);
        self.helper_ok(pc, ok);
        self.b.ins().jump(join, &[]);
    }

    /// Ops with inline data access (addendum v4). `None`: not covered here.
    fn emit_v4(&mut self, pc: usize) -> Option<bool> {
        let op = self.code[pc];
        Some(match op {
            Op::Index => {
                // List[Int] from the stack: in bounds inline, else the runtime
                // (its trap message). The element replaces the list slot.
                let ml = self.operand(2);
                let mi = self.operand(1);
                self.guarded(pc, &[(ml, TAG_LIST), (mi, TAG_INT)], 2, Slow::Deopt, |g, ms| {
                    let (p, i) = (ms[0].w, ms[1].w);
                    let len = g.list_len(p);
                    let ok = g.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
                    g.or_generic(pc, ok);
                    let elem = g.list_elem(p, i);
                    // Clone into the spare slot first: dropping the list may free it.
                    let tmp = g.top(0);
                    let m = g.clone_out(tmp, elem);
                    let lat = g.top(2);
                    g.drop_owned(Some(ms[0]), lat);
                    let lat = g.top(2);
                    let tmp = g.top(0);
                    g.copy_value(lat, tmp);
                    g.bump_sp(-1);
                    Some(m)
                })
            }
            Op::MakeList(0) if self.kconst.is_some() => {
                self.push_kconst(self.prog.consts.len(), TAG_LIST);
                true
            }
            Op::ForNext(b, t) => {
                // Unverified IR may name locals at the top of the u16 range.
                if b > u16::MAX - 2 {
                    return None;
                }
                let ml = self.local_m(b);
                let mi = self.local_m(b + 1);
                self.guarded(pc, &[(ml, TAG_LIST), (mi, TAG_INT)], 0, Slow::Deopt, |g, ms| {
                    let (p, i) = (ms[0].w, ms[1].w);
                    let len = g.list_len(p);
                    let more = g.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
                    let cont = g.b.create_block();
                    let taken = g.b.create_block();
                    g.b.ins().brif(more, cont, &[], taken, &[]);
                    g.b.switch_to_block(taken);
                    g.goto(pc, t);
                    g.b.switch_to_block(cont);
                    let elem = g.list_elem(p, i);
                    let old = g.locs.get(&(b + 2)).copied();
                    let var = g.local(b + 2);
                    g.drop_local_old(b + 2, old, var);
                    let var = g.local(b + 2);
                    let et = g.static_elem(b);
                    let m = g.clone_elem(var, elem, et);
                    g.locs.insert(b + 2, m);
                    let i1 = g.b.ins().iadd_imm_s(i, 1);
                    let m = g.known(TAG_INT, i1);
                    let at = g.local(b + 1);
                    g.store_plain(at, m);
                    g.locs.insert(b + 1, m);
                    None
                })
            }
            Op::IndexLL(a, b) => {
                let ml = self.local_m(a);
                let mi = self.local_m(b);
                self.guarded(pc, &[(ml, TAG_LIST), (mi, TAG_INT)], 0, Slow::Deopt, |g, ms| {
                    let (p, i) = (ms[0].w, ms[1].w);
                    let len = g.list_len(p);
                    let ok = g.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
                    g.or_generic(pc, ok);
                    let elem = g.list_elem(p, i);
                    let dst = g.top(0);
                    let et = g.static_elem(a);
                    let m = g.clone_elem(dst, elem, et);
                    g.vs.push(Some(m));
                    g.bump_sp(1);
                    None
                })
            }
            Op::LenLocal(a) => {
                // List: inline; anything else: `zal_rt_len` (Map, Str).
                let ml = self.local_m(a);
                let v2 = self.v2.expect("v2 helpers");
                match self.is_tag(ml, TAG_LIST) {
                    G::Yes => {
                        let n = self.list_len(ml.w);
                        self.push_known(TAG_INT, n);
                    }
                    G::No => return None,
                    G::Dyn(c) => {
                        let inl = self.b.create_block();
                        let hlp = self.b.create_block();
                        let join = self.b.create_block();
                        self.b.append_block_param(join, types::I64);
                        self.b.ins().brif(c, inl, &[], hlp, &[]);
                        self.b.switch_to_block(inl);
                        let n = self.list_len(ml.w);
                        self.b.ins().jump(join, &[BlockArg::Value(n)]);
                        self.b.switch_to_block(hlp);
                        let at = self.local(a);
                        let n = self.call1(v2.len, &[at]);
                        let ok = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, n, 0);
                        self.helper_ok(pc, ok);
                        self.b.ins().jump(join, &[BlockArg::Value(n)]);
                        self.b.switch_to_block(join);
                        let n = self.b.block_params(join)[0];
                        self.push_known(TAG_INT, n);
                    }
                }
                true
            }
            Op::Builtin(zal_ir::Builtin::Len, 1) => {
                let e = self.operand(1);
                let ok = self.guarded(pc, &[(e, TAG_LIST)], 1, Slow::Deopt, |g, ms| {
                    let n = g.list_len(ms[0].w);
                    let at = g.top(1);
                    g.drop_owned(Some(ms[0]), at);
                    let m = g.known(TAG_INT, n);
                    let at = g.top(1);
                    g.store_plain(at, m);
                    Some(m)
                });
                ok
            }
            Op::LoadField(a, f) => {
                let mr = self.local_m(a);
                self.guarded(pc, &[(mr, TAG_RECORD)], 0, Slow::Deopt, |g, ms| {
                    let addr = g.local(a);
                    let p = g.agg_ptr(addr, ms[0].w);
                    let src = g.agg_field(pc, addr, p, f);
                    let dst = g.top(0);
                    let ft = g.static_field(a, f);
                    let m = g.clone_elem(dst, src, ft);
                    g.vs.push(Some(m));
                    g.bump_sp(1);
                    None
                })
            }
            Op::FieldGet(f) => {
                let e = self.operand(1);
                let ok = self.guarded(pc, &[(e, TAG_RECORD)], 1, Slow::Deopt, |g, ms| {
                    let addr = g.top(1);
                    let p = g.agg_ptr(addr, ms[0].w);
                    let src = g.agg_field(pc, addr, p, f);
                    // Field into the free slot, drop the record, move down.
                    let tmp = g.top(0);
                    g.clone_into(tmp, src, None);
                    let addr = g.top(1);
                    g.drop_owned(Some(ms[0]), addr);
                    let addr = g.top(1);
                    let tmp = g.top(0);
                    g.copy_value(addr, tmp);
                    None
                });
                if ok {
                    self.vs.push(None);
                }
                ok
            }
            Op::JumpIfNotTag(tag, t) => {
                let e = self.operand(1);
                self.guarded(pc, &[(e, TAG_VARIANT)], 0, Slow::Deopt, |g, _| {
                    let at = g.top(1);
                    let vt = g.b.ins().load(types::I16, mem(), at, g.lay.unwrap().variant_tag);
                    g.variant_drop(at);
                    g.pop_n(1);
                    let hit = g.b.ins().icmp_imm_u(IntCC::Equal, vt, tag as i64);
                    let taken = g.b.create_block();
                    let cont = g.b.create_block();
                    g.b.ins().brif(hit, cont, &[], taken, &[]);
                    g.b.switch_to_block(taken);
                    g.goto(pc, t);
                    g.b.switch_to_block(cont);
                    None
                })
            }
            Op::MovePayload(a, b) if a != b => {
                let mv = self.local_m(a);
                self.guarded(pc, &[(mv, TAG_VARIANT)], 0, Slow::Deopt, |g, _| {
                    let va = g.local(a);
                    let p = g.variant_ptr(va);
                    g.or_generic(pc, p);
                    let src = g.b.ins().iadd_imm_s(p, 16);
                    let old = g.locs.get(&b).copied();
                    let dst = g.local(b);
                    g.drop_local_old(b, old, dst);
                    let dst = g.local(b);
                    let pt = g.static_tag(b);
                    let m = g.clone_elem(dst, src, pt);
                    g.locs.insert(b, m);
                    None
                })
            }
            Op::LoadPayload(a) => {
                let mv = self.local_m(a);
                self.guarded(pc, &[(mv, TAG_VARIANT)], 0, Slow::Deopt, |g, _| {
                    let va = g.local(a);
                    let p = g.variant_ptr(va);
                    g.or_generic(pc, p);
                    let src = g.b.ins().iadd_imm_s(p, 16);
                    let dst = g.top(0);
                    let m = g.clone_out(dst, src);
                    g.vs.push(Some(m));
                    g.bump_sp(1);
                    None
                })
            }
            Op::UpdLocal(bi @ zal_ir::Builtin::Push, a) => {
                let ml = self.local_m(a);
                let ok = self.guarded(pc, &[(ml, TAG_LIST)], 0, Slow::Deopt, |g, ms| {
                    let p = ms[0].w;
                    let uniq = g.list_unique(p);
                    let len = g.list_len(p);
                    let cap = g.b.ins().load(types::I64, mem(), p, g.lay.unwrap().list_cap);
                    let room = g.b.ins().icmp(IntCC::UnsignedLessThan, len, cap);
                    let ok = g.b.ins().band(uniq, room);
                    let fast = g.b.create_block();
                    let slow = g.b.create_block();
                    let join = g.b.create_block();
                    g.b.ins().brif(ok, fast, &[], slow, &[]);
                    g.b.switch_to_block(slow);
                    // First push into the shared empty list: a fresh list.
                    if let (Some(f), Some(base)) = (g.list_one, g.kconst) {
                        let k = g.prog.consts.len();
                        // SAFETY: the JIT-owned constants outlive the code.
                        let rc = unsafe { ((base + k * VALUE_SIZE as usize + VAL_PAYLOAD as usize) as *const u64).read() };
                        let is_empty = g.b.ins().icmp_imm_u(IntCC::Equal, p, rc as i64);
                        let one = g.b.create_block();
                        let other = g.b.create_block();
                        g.b.ins().brif(is_empty, one, &[], other, &[]);
                        g.b.switch_to_block(one);
                        let local = g.local(a);
                        let x = g.top(1);
                        g.b.ins().call(f, &[local, x]);
                        g.b.ins().jump(join, &[]);
                        g.b.switch_to_block(other);
                    }
                    g.upd_helper(pc, bi, a, 1, join);
                    g.b.switch_to_block(fast);
                    let dst = g.list_elem(p, len);
                    g.move_top_to(dst);
                    let n1 = g.b.ins().iadd_imm_s(len, 1);
                    g.b.ins().store(mem(), n1, p, g.lay.unwrap().list_len);
                    g.b.ins().jump(join, &[]);
                    g.b.switch_to_block(join);
                    None
                });
                if ok {
                    self.locs.remove(&a);
                    self.pop_n(1);
                }
                ok
            }
            Op::UpdLocal(bi @ zal_ir::Builtin::Set, a) => {
                let ml = self.local_m(a);
                let mi = self.operand(2);
                let ok = self.guarded(pc, &[(ml, TAG_LIST), (mi, TAG_INT)], 0, Slow::Deopt, |g, ms| {
                    let (p, i) = (ms[0].w, ms[1].w);
                    let uniq = g.list_unique(p);
                    let len = g.list_len(p);
                    let inb = g.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
                    let ok = g.b.ins().band(uniq, inb);
                    let fast = g.b.create_block();
                    let slow = g.b.create_block();
                    let join = g.b.create_block();
                    g.b.ins().brif(ok, fast, &[], slow, &[]);
                    g.b.switch_to_block(slow);
                    g.upd_helper(pc, bi, a, 2, join);
                    g.b.switch_to_block(fast);
                    let dst = g.list_elem(p, i);
                    let et = g.static_elem(a);
                    g.drop_elem(dst, et);
                    let dst = g.list_elem(p, i);
                    g.move_top_to(dst);
                    g.b.ins().jump(join, &[]);
                    g.b.switch_to_block(join);
                    None
                });
                if ok {
                    self.locs.remove(&a);
                    self.pop_n(2);
                }
                ok
            }
            Op::UpdLocal(bi @ zal_ir::Builtin::Remove, a) => {
                let ml = self.local_m(a);
                let mi = self.operand(1);
                let ok = self.guarded(pc, &[(ml, TAG_LIST), (mi, TAG_INT)], 0, Slow::Deopt, |g, ms| {
                    // Unique list, index in range: drop the element and
                    // shift the tail down one slot (raw moves).
                    let (p, i) = (ms[0].w, ms[1].w);
                    let uniq = g.list_unique(p);
                    let len = g.list_len(p);
                    let inb = g.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
                    let ok = g.b.ins().band(uniq, inb);
                    let fast = g.b.create_block();
                    let slow = g.b.create_block();
                    let join = g.b.create_block();
                    g.b.ins().brif(ok, fast, &[], slow, &[]);
                    g.b.switch_to_block(slow);
                    g.upd_helper(pc, bi, a, 1, join);
                    g.b.switch_to_block(fast);
                    let at = g.list_elem(p, i);
                    let et = g.static_elem(a);
                    g.drop_elem(at, et);
                    let last = g.b.ins().iadd_imm_s(len, -1);
                    let lay = g.lay.unwrap();
                    g.b.ins().store(mem(), last, p, lay.list_len);
                    let end = g.list_elem(p, last);
                    let at = g.list_elem(p, i);
                    let lp = g.b.create_block();
                    g.b.append_block_param(lp, g.ptr);
                    let body = g.b.create_block();
                    g.b.ins().jump(lp, &[BlockArg::Value(at)]);
                    g.b.switch_to_block(lp);
                    let cur = g.b.block_params(lp)[0];
                    let more = g.b.ins().icmp(IntCC::UnsignedLessThan, cur, end);
                    g.b.ins().brif(more, body, &[], join, &[]);
                    g.b.switch_to_block(body);
                    let nx = g.b.ins().iadd_imm_s(cur, VALUE_SIZE);
                    if et.is_some_and(tag_is_plain) || et == Some(TAG_TASK) {
                        // Plain elements: tag byte and payload word are all.
                        let t = g.b.ins().load(types::I64, mem(), nx, 0);
                        let w = g.b.ins().load(types::I64, mem(), nx, VAL_PAYLOAD);
                        g.b.ins().store(mem(), t, cur, 0);
                        g.b.ins().store(mem(), w, cur, VAL_PAYLOAD);
                    } else {
                        g.copy_value(cur, nx);
                    }
                    g.b.ins().jump(lp, &[BlockArg::Value(nx)]);
                    g.b.switch_to_block(join);
                    None
                });
                if ok {
                    self.locs.remove(&a);
                    self.pop_n(1);
                }
                ok
            }
            _ => return None,
        })
    }

    /// Emit the region starting at leader `start`.
    fn emit_region(&mut self, start: usize) {
        self.begin(start);
        let mut pc = start;
        loop {
            self.skip = 0;
            if let Some(sp) = self.static_sp(pc) {
                // Compile-time sp must match the proof at every op boundary.
                debug_assert_eq!(self.sd0 + self.depth, sp, "static sp at pc {pc} (region {start})");
                if self.sd0 + self.depth != sp {
                    self.sync();
                    self.depth = sp - self.sd0;
                }
            }
            // Source location = op pc (profiling aid, ZAL_CL_SRCMAP).
            self.b.set_srcloc(ir::SourceLoc::new(pc as u32));
            if !self.emit_op(pc) {
                return;
            }
            pc += 1 + self.skip;
            if pc >= self.code.len() || self.leader[pc] {
                self.sync();
                let next = self.pc_block(pc);
                self.b.ins().jump(next, &[]);
                return;
            }
        }
    }
}

/// Translate IR function `fidx` into `func` (signature already set).
pub(crate) fn translate<M: Module>(
    m: &mut M,
    imp: &Imports,
    prog: &Program,
    fidx: usize,
    depths: Option<&[Option<u32>]>,
    func: &mut ir::Function,
    fctx: &mut FunctionBuilderContext,
) {
    let ptr = m.target_config().pointer_type();
    let tcfg = m.target_config();
    let rt_op = m.declare_func_in_func(imp.op, func);
    let rt_tick = m.declare_func_in_func(imp.tick, func);
    let rt_trap = m.declare_func_in_func(imp.trap, func);
    let rt_clone = m.declare_func_in_func(imp.clone, func);
    let rt_drop = m.declare_func_in_func(imp.drop, func);
    let v2_refs = imp.v2.map(|v| v.map(|id| m.declare_func_in_func(id, func)));
    let v3_refs = imp.v3.map(|v| V3 {
        make: m.declare_func_in_func(v.make, func),
        builtin: m.declare_func_in_func(v.builtin, func),
        map_get_jump: m.declare_func_in_func(v.map_get_jump, func),
    });
    let list_one_ref = imp.list_one.map(|id| m.declare_func_in_func(id, func));
    let code = &prog.functions[fidx].code;

    let mut b = FunctionBuilder::new(func, fctx);
    let stack = b.declare_var(ptr);
    let sp = b.declare_var(types::I64);
    let base = b.declare_var(ptr);
    let spp = b.declare_var(ptr);

    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    let dispatch = b.create_block();
    b.append_block_param(dispatch, types::I32);
    let redispatch = b.create_block();
    let step = b.create_block();
    b.append_block_param(step, types::I32);
    let exit = b.create_block();
    b.append_block_param(exit, types::I32);
    let bad = b.create_block();
    let blocks: Vec<Block> = (0..code.len()).map(|_| b.create_block()).collect();

    b.switch_to_block(entry);
    let ctx = b.block_params(entry)[0];
    let entry_pc = b.block_params(entry)[1];

    let mut g = Gen {
        b: &mut b,
        prog,
        code,
        ptr,
        ctx,
        stack,
        sp,
        base,
        spp,
        blocks,
        leader: leaders(prog, code, imp.v2.is_some()),
        redispatch,
        step,
        exit,
        bad,
        rt_op,
        rt_tick,
        rt_trap,
        rt_clone,
        rt_drop,
        v2: v2_refs,
        v3: v3_refs,
        lay: if imp.v2.is_some() { imp.lay } else { None },
        kconst: if imp.v2.is_some() && imp.lay.is_some() { imp.kconst } else { None },
        list_one: if imp.v2.is_some() && imp.lay.is_some() { list_one_ref } else { None },
        // Slot tags only with a valid seal (compiled in this process, unchanged).
        stags: if imp.v2.is_some()
            && imp.lay.is_some()
            && std::env::var("ZAL_NO_SLOT_TAGS").is_err()
            && imp.seal != 0
            && prog.functions[fidx].slot_seal == imp.seal
        {
            prog.functions[fidx].slot_tags.clone()
        } else {
            Vec::new()
        },
        vs: Vec::new(),
        locs: HashMap::new(),
        skip: 0,
        pending: Vec::new(),
        depth: 0,
        lazy: imp.v2.is_some() && std::env::var("ZAL_CL_NO_LAZY").is_err(),
        in_arm: false,
        stat: depths
            .filter(|d| imp.v2.is_some() && d.len() == code.len() && std::env::var_os("ZAL_CL_NO_STATIC").is_none())
            .map(|d| d.to_vec()),
        nloc: prog.functions[fidx].locals as i64,
        sd0: 0,
    };
    g.reload();
    g.b.ins().jump(dispatch, &[BlockArg::Value(entry_pc)]);

    // dispatch(pc): every op is a possible resume point.
    g.b.switch_to_block(dispatch);
    let pcv = g.b.block_params(dispatch)[0];
    let def = g.b.func.dfg.block_call(bad, &[]);
    let targets: Vec<_> = g.blocks.clone().into_iter().map(|blk| g.b.func.dfg.block_call(blk, &[])).collect();
    let jt = g.b.create_jump_table(JumpTableData::new(def, &targets));
    g.b.ins().br_table(pcv, jt);

    // redispatch: a generic op answered JUMP without a static target.
    g.b.switch_to_block(redispatch);
    let p = g.b.ins().load(types::I32, mem(), ctx, CTX_PC);
    g.b.ins().jump(dispatch, &[BlockArg::Value(p)]);

    // step(pc): one op through the runtime; 0 -> dispatch(pc+1), 6 -> ctx->pc.
    g.b.switch_to_block(step);
    let sp_pc = g.b.block_params(step)[0];
    if g.stat.is_none() {
        g.flush();
    }
    let call = g.b.ins().call(g.rt_op, &[ctx, sp_pc]);
    let r = g.b.inst_results(call)[0];
    g.reload();
    let zero = g.b.create_block();
    let nz = g.b.create_block();
    g.b.ins().brif(r, nz, &[], zero, &[]);
    g.b.switch_to_block(zero);
    let np = g.b.ins().iadd_imm_s(sp_pc, 1);
    g.b.ins().jump(dispatch, &[BlockArg::Value(np)]);
    g.b.switch_to_block(nz);
    let is_jump = g.b.ins().icmp_imm_u(IntCC::Equal, r, JUMP as i64);
    let leave = g.b.create_block();
    g.b.ins().brif(is_jump, redispatch, &[], leave, &[]);
    g.b.switch_to_block(leave);
    g.exit_with(r);

    g.b.switch_to_block(exit);
    let r = g.b.block_params(exit)[0];
    g.b.ins().return_(&[r]);

    // Unreachable for verified IR (pc out of range / control off the end).
    g.b.switch_to_block(bad);
    let t = g.b.ins().iconst(types::I32, EXIT_TRAP as i64);
    g.b.ins().return_(&[t]);

    for pc in 0..code.len() {
        let blk = g.blocks[pc];
        g.b.switch_to_block(blk);
        if g.leader[pc] {
            g.emit_region(pc);
        } else {
            // Entered only by dispatch: memory state, so step generically.
            g.b.set_srcloc(ir::SourceLoc::new(pc as u32));
            g.begin(pc);
            g.generic(pc);
        }
    }

    b.seal_all_blocks();
    b.finalize(tcfg);
}
