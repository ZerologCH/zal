//! Runtime side of the native ABI: the session behind `ZalCtx::vm`, the lean
//! generic path of `zal_rt_op`, calls, returns and traps (RFC 0002 v2).
//!
//! The lean path executes the ops that touch neither the scheduler nor the
//! frame stack directly on `ctx->stack`, without setting up the interpreter
//! loop. It decides before mutating anything; every case it does not cover
//! (other ops, unexpected operand types, traps, the slice limit on a backward
//! branch) goes to the interpreter's own code for that op (`Session::op`), so
//! messages, traces and corner cases stay the interpreter's.

use std::ffi::c_void;
use std::rc::Rc;

use zal_ir::{Builtin, Op, Program};

use crate::native::*;
use crate::{Frame, RunError, Value};

/// What `ZalCtx::vm` points to while machine code runs. Not generic: the
/// `Vm<H, B>`-specific slow paths are function pointers.
#[repr(C)]
pub(crate) struct Session {
    /// Slow generic path: one op through `exec_loop::<MODE_ONE>`.
    pub op: unsafe fn(*mut ZalCtx, u32) -> u32,
    /// Slice limit reached: deadline check and fair preemption.
    pub tick: unsafe fn(*mut ZalCtx, u32) -> u32,
    pub prog: *const Program,
    pub consts: *const Value,
    pub max_stack: *const u32,
    /// Per function: [locals, verified operand depth + 1].
    pub fninfo: *const [u32; 2],
    /// Per function: local slots that may own a value (null = all).
    pub drop_slots: *const Box<[u16]>,
    pub native: *const Option<NativeFn>,
    pub vm: *mut c_void,
    pub fid: u32,
    /// `zal run --profile` counts ops in the interpreter: no lean path then.
    pub profiling: bool,
    pub stack: *mut Vec<Value>,
    pub frames: *mut Vec<Frame>,
    pub ops: u64,
    pub err: Option<RunError>,
    pub done: Option<Value>,
}

#[inline(always)]
pub(crate) unsafe fn session<'a>(ctx: *mut ZalCtx) -> &'a mut Session {
    &mut *((*ctx).vm as *mut Session)
}

/// Point `ctx` at the current stack buffer and top frame after the runtime ran.
#[inline]
pub(crate) unsafe fn sync_ctx(ctx: *mut ZalCtx, stack: &mut Vec<Value>, frames: &[Frame]) {
    let c = &mut *ctx;
    c.stack = stack.as_mut_ptr();
    c.sp = stack.len() as u64;
    if let Some(fr) = frames.last() {
        c.base = fr.base as u64;
        c.pc = fr.pc as u32;
        c.func = fr.func;
    }
}

/// "Not handled here": take the interpreter's path for this op.
const SLOW: u32 = u32::MAX;

/// `zal_rt_op`: decode once; `Ret`/`Call` go straight to their small
/// routines, the other covered ops to the lean path, the rest to the
/// interpreter's code.
#[inline]
pub(crate) unsafe fn op(ctx: *mut ZalCtx, pc: u32) -> u32 {
    let s = session(ctx);
    if !s.profiling {
        let prog = &*s.prog;
        let func = prog.functions.get_unchecked((*ctx).func as usize);
        debug_assert!((pc as usize) < func.code.len());
        let op = *func.code.get_unchecked(pc as usize);
        let r = match op {
            Op::Ret => return ret(ctx),
            Op::Call(fi, argc) => return call(ctx, pc, fi, argc as u32),
            // Scheduler ops: the lean path never covers them.
            Op::Spawn(..) | Op::Extern(..) | Op::Await | Op::AwaitAll(_) | Op::Builtin(Builtin::AwaitAny, _) => SLOW,
            _ => lean_op(ctx, pc, op),
        };
        if r != SLOW {
            return r;
        }
    }
    (s.op)(ctx, pc)
}

/// Clone with the plain tags copied inline (the derived clone is out of line).
#[inline(always)]
unsafe fn clone_fast(v: *const Value) -> Value {
    if tag_is_plain(*(v as *const u8)) {
        v.read()
    } else {
        (*v).clone()
    }
}

#[inline(always)]
unsafe fn top(c: &ZalCtx, k: usize) -> *mut Value {
    c.stack.add(c.sp as usize - 1 - k)
}

#[inline(always)]
unsafe fn local(c: &ZalCtx, s: u16) -> *mut Value {
    c.stack.add(c.base as usize + s as usize)
}

#[inline(always)]
unsafe fn push(c: &mut ZalCtx, v: Value) {
    c.stack.add(c.sp as usize).write(v);
    c.sp += 1;
}

#[inline(always)]
unsafe fn pop(c: &mut ZalCtx) -> Value {
    c.sp -= 1;
    c.stack.add(c.sp as usize).read()
}

/// Move the top `n` values into a Vec (what `stack.split_off` does).
#[inline(always)]
unsafe fn take_n(c: &mut ZalCtx, n: usize) -> Vec<Value> {
    let mut v = Vec::with_capacity(n);
    c.sp -= n as u64;
    std::ptr::copy_nonoverlapping(c.stack.add(c.sp as usize), v.as_mut_ptr(), n);
    v.set_len(n);
    v
}

/// Ordering of two comparable operands (Int/Int, Dur/Dur, Str/Str).
#[inline(always)]
fn ord(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => Some(x.cmp(y)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[inline(always)]
fn len_of(v: &Value) -> Option<usize> {
    match v {
        Value::List(xs) => Some(xs.len()),
        Value::Map(m) => Some(m.len()),
        Value::Str(s) => Some(s.chars().count()),
        _ => None,
    }
}

#[inline(always)]
fn clone_payload(p: Rc<Value>) -> Value {
    Rc::try_unwrap(p).unwrap_or_else(|p| (*p).clone())
}

#[inline(never)]
unsafe fn lean_op(ctx: *mut ZalCtx, pc: u32, op: Op) -> u32 {
    let s = session(ctx);
    let c = &mut *ctx;

    // A taken branch to `t`: backward branches count a tick; at the slice
    // limit the interpreter's path does it (checked before any mutation).
    macro_rules! branch_ok {
        ($t:expr) => {
            !($t <= pc && c.slice + 1 >= ZAL_SLICE)
        };
    }
    macro_rules! taken {
        ($t:expr) => {{
            if $t <= pc {
                c.slice += 1;
            }
            c.pc = $t;
            ZAL_JUMP
        }};
    }

    match op {
        Op::Const(i) => push(c, clone_fast(s.consts.add(i as usize))),
        Op::Unit => push(c, Value::Unit),
        Op::LoadLocal(a) => push(c, clone_fast(local(c, a))),
        Op::TakeLocal(a) => {
            let v = std::mem::replace(&mut *local(c, a), Value::Unit);
            push(c, v)
        }
        Op::StoreLocal(a) => {
            let v = pop(c);
            *local(c, a) = v;
        }
        Op::Pop => drop(pop(c)),
        Op::Add => {
            let r = match (&*top(c, 1), &*top(c, 0)) {
                (Value::Int(x), Value::Int(y)) => match x.checked_add(*y) {
                    Some(r) => Value::Int(r),
                    None => return SLOW,
                },
                (Value::Dur(x), Value::Dur(y)) => Value::Dur(x.saturating_add(*y)),
                (Value::Str(x), Value::Str(y)) => {
                    let mut st = String::with_capacity(x.len() + y.len());
                    st.push_str(x);
                    st.push_str(y);
                    Value::Str(Rc::from(st))
                }
                _ => return SLOW,
            };
            drop(pop(c));
            *top(c, 0) = r;
        }
        Op::Sub | Op::Mul | Op::Div | Op::Mod => {
            let f: fn(i64, i64) -> Option<i64> = match op {
                Op::Sub => i64::checked_sub,
                Op::Mul => i64::checked_mul,
                Op::Div => i64::checked_div,
                _ => i64::checked_rem,
            };
            let r = match (&*top(c, 1), &*top(c, 0)) {
                (Value::Int(x), Value::Int(y)) => match f(*x, *y) {
                    Some(r) => Value::Int(r),
                    None => return SLOW,
                },
                (Value::Dur(x), Value::Dur(y)) => match f(*x, *y) {
                    Some(r) => Value::Dur(r),
                    None => return SLOW,
                },
                _ => return SLOW,
            };
            c.sp -= 1; // both operands are plain
            top(c, 0).write(r);
        }
        Op::Neg => match &*top(c, 0) {
            Value::Int(x) => *top(c, 0) = Value::Int(x.checked_neg().unwrap_or(i64::MIN)),
            Value::Dur(x) => *top(c, 0) = Value::Dur(-x),
            _ => return SLOW,
        },
        Op::Not => match &*top(c, 0) {
            Value::Bool(b) => *top(c, 0) = Value::Bool(!b),
            _ => return SLOW,
        },
        Op::Eq | Op::Ne => {
            let b = pop(c);
            let a = pop(c);
            push(c, Value::Bool((a == b) == (op == Op::Eq)));
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let Some(o) = ord(&*top(c, 1), &*top(c, 0)) else { return SLOW };
            let r = match op {
                Op::Lt => o.is_lt(),
                Op::Le => o.is_le(),
                Op::Gt => o.is_gt(),
                _ => o.is_ge(),
            };
            drop(pop(c));
            *top(c, 0) = Value::Bool(r);
        }
        Op::Jump(t) => {
            if !branch_ok!(t) {
                return SLOW;
            }
            return taken!(t);
        }
        Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
            let want = matches!(op, Op::JumpIfTrue(_));
            let hit = matches!(&*top(c, 0), Value::Bool(b) if *b == want);
            if hit && !branch_ok!(t) {
                return SLOW;
            }
            drop(pop(c));
            if hit {
                return taken!(t);
            }
        }
        Op::MakeTuple(n) => {
            c.sp -= n as u64;
            let xs = crate::value::rc_slice_take(c.stack.add(c.sp as usize), n as usize);
            push(c, Value::Tuple(xs))
        }
        Op::MakeRecord(n) => {
            c.sp -= n as u64;
            let xs = crate::value::rc_slice_take(c.stack.add(c.sp as usize), n as usize);
            push(c, Value::Record(xs))
        }
        Op::MakeList(n) => {
            let xs = take_n(c, n as usize);
            push(c, Value::List(Rc::new(xs)))
        }
        Op::TupleGet(i) | Op::FieldGet(i) => {
            let v = match (op, &*top(c, 0)) {
                (Op::TupleGet(_), Value::Tuple(xs)) | (Op::FieldGet(_), Value::Record(xs)) => match xs.get(i as usize) {
                    Some(x) => x.clone(),
                    None => return SLOW,
                },
                _ => return SLOW,
            };
            *top(c, 0) = v;
        }
        Op::Index => {
            let v = match (&*top(c, 1), &*top(c, 0)) {
                (Value::List(xs), Value::Int(i)) => match usize::try_from(*i).ok().and_then(|i| xs.get(i)) {
                    Some(v) => v.clone(),
                    None => return SLOW,
                },
                _ => return SLOW,
            };
            c.sp -= 1; // the index is plain
            *top(c, 0) = v;
        }
        Op::IncLocal(a, k) => {
            let l = local(c, a);
            match &*l {
                Value::Int(x) => match x.checked_add(k as i64) {
                    Some(r) => l.write(Value::Int(r)),
                    None => return SLOW,
                },
                _ => return SLOW,
            }
        }
        Op::LoadLocal2(a, b) => {
            let x = clone_fast(local(c, a));
            let y = clone_fast(local(c, b));
            push(c, x);
            push(c, y);
        }
        Op::JumpIfNotLt(t) => {
            let Some(o) = ord(&*top(c, 1), &*top(c, 0)) else { return SLOW };
            let hit = !o.is_lt();
            if hit && !branch_ok!(t) {
                return SLOW;
            }
            drop(pop(c));
            drop(pop(c));
            if hit {
                return taken!(t);
            }
        }
        Op::StoreLoad(a) => {
            let v = (*top(c, 0)).clone();
            *local(c, a) = v;
        }
        Op::SplitTuple(n) => {
            if !matches!(&*top(c, 0), Value::Tuple(xs) if xs.len() == n as usize) {
                return SLOW;
            }
            let Value::Tuple(xs) = pop(c) else { unreachable!() };
            crate::value::rc_slice_each(xs, |x| push(c, x));
        }
        Op::JumpIfNotTag(tag, t) => {
            let hit = match &*top(c, 0) {
                Value::Variant(x, _) => *x != tag,
                Value::Bool(b) => *b as u16 != tag,
                _ => return SLOW,
            };
            if hit && !branch_ok!(t) {
                return SLOW;
            }
            drop(pop(c));
            if hit {
                return taken!(t);
            }
        }
        Op::IndexLL(a, b) => {
            let v = match (&*local(c, a), &*local(c, b)) {
                (Value::List(xs), Value::Int(i)) => match usize::try_from(*i).ok().and_then(|i| xs.get(i)) {
                    Some(v) => v.clone(),
                    None => return SLOW,
                },
                _ => return SLOW,
            };
            push(c, v);
        }
        Op::LoadField(a, f) => {
            let v = match &*local(c, a) {
                Value::Record(xs) => match xs.get(f as usize) {
                    Some(x) => x.clone(),
                    None => return SLOW,
                },
                _ => return SLOW,
            };
            push(c, v);
        }
        Op::LoadPayload(a) => {
            let v = match &*local(c, a) {
                Value::Variant(_, Some(p)) => (**p).clone(),
                _ => return SLOW,
            };
            push(c, v);
        }
        Op::MovePayload(a, b) => {
            let v = match &*local(c, a) {
                Value::Variant(_, Some(p)) => (**p).clone(),
                _ => return SLOW,
            };
            *local(c, b) = v;
        }
        Op::UnwrapOrJump(r, t) => {
            let hit = match &*top(c, 0) {
                Value::Variant(1, Some(_)) => false,
                Value::Variant(_, _) => true,
                _ => return SLOW,
            };
            if hit && !branch_ok!(t) {
                return SLOW;
            }
            let v = pop(c);
            if hit {
                drop(v);
                return taken!(t);
            }
            let Value::Variant(_, Some(p)) = v else { unreachable!() };
            *local(c, r) = clone_payload(p);
        }
        Op::MapGetJump(r, t) => {
            let got = match (&*top(c, 1), &*top(c, 0)) {
                (Value::Map(m), Value::Str(k)) => m.get(k).cloned(),
                _ => return SLOW,
            };
            if got.is_none() && !branch_ok!(t) {
                return SLOW;
            }
            drop(pop(c));
            drop(pop(c));
            match got {
                Some(v) => *local(c, r) = v,
                None => return taken!(t),
            }
        }
        Op::LenLocal(a) => {
            let Some(n) = len_of(&*local(c, a)) else { return SLOW };
            push(c, Value::Int(n as i64));
        }
        Op::UpdLocal(b, a) => {
            let argc = match b {
                Builtin::Push | Builtin::Remove => 1,
                Builtin::Set | Builtin::MapSet => 2,
                _ => return SLOW,
            };
            if zal_rt_upd(local(c, a), c.stack.add(c.sp as usize - argc), b as u32) != 0 {
                return SLOW;
            }
            c.sp -= argc as u64;
        }
        Op::ConcatN(n) => {
            if zal_rt_concat(ctx, n as u32) != 0 {
                return SLOW;
            }
        }
        Op::ForNext(b, exit) => {
            let slot = local(c, b);
            match zal_rt_for_next_peek(slot) {
                0 => {
                    zal_rt_for_next(slot);
                }
                1 => {
                    if !branch_ok!(exit) {
                        return SLOW;
                    }
                    return taken!(exit);
                }
                _ => return SLOW,
            }
        }
        Op::MakeVariant(tag, has) => {
            let p = if has { Some(Rc::new(pop(c))) } else { None };
            push(c, Value::Variant(tag, p));
        }
        Op::TagIs(tag) => {
            let r = match &*top(c, 0) {
                Value::Variant(t, _) => *t == tag,
                Value::Bool(b) => *b as u16 == tag,
                _ => return SLOW,
            };
            *top(c, 0) = Value::Bool(r);
        }
        Op::Payload => {
            if !matches!(&*top(c, 0), Value::Variant(_, Some(_))) {
                return SLOW;
            }
            let Value::Variant(_, Some(p)) = pop(c) else { unreachable!() };
            push(c, clone_payload(p));
        }
        Op::Builtin(Builtin::Len, _) => {
            let Some(n) = len_of(&*top(c, 0)) else { return SLOW };
            *top(c, 0) = Value::Int(n as i64);
        }
        Op::Builtin(Builtin::Push, _) => {
            let Value::List(_) = &*top(c, 1) else { return SLOW };
            let x = pop(c);
            let Value::List(xs) = &mut *top(c, 0) else { unreachable!() };
            crate::value::list_push(xs, x);
        }
        Op::Builtin(Builtin::MapGet, _) | Op::Builtin(Builtin::MapHas, _) => {
            let r = match (&*top(c, 1), &*top(c, 0)) {
                (Value::Map(m), Value::Str(k)) => {
                    if matches!(op, Op::Builtin(Builtin::MapHas, _)) {
                        Value::Bool(m.contains_key(k))
                    } else {
                        match m.get(k) {
                            Some(v) => Value::Variant(1, Some(Rc::new(v.clone()))),
                            None => Value::Variant(0, None),
                        }
                    }
                }
                _ => return SLOW,
            };
            drop(pop(c));
            *top(c, 0) = r;
        }
        Op::Builtin(Builtin::MapSet, _) => {
            if !matches!((&*top(c, 2), &*top(c, 1)), (Value::Map(_), Value::Str(_))) {
                return SLOW;
            }
            let v = pop(c);
            let Value::Str(k) = pop(c) else { unreachable!() };
            let Value::Map(m) = &mut *top(c, 0) else { unreachable!() };
            Rc::make_mut(m).insert(k, v);
        }
        Op::Builtin(b, argc) => {
            if builtin(ctx, b as u32, argc as u32) != 0 {
                return SLOW;
            }
        }
        _ => return SLOW,
    }
    ZAL_NEXT
}

/// `ForNext` precheck without mutation: 0 = would advance, 1 = exhausted, 2 = generic.
#[inline(always)]
unsafe fn zal_rt_for_next_peek(slot3: *const Value) -> u32 {
    match (&*slot3, &*slot3.add(1)) {
        (Value::List(xs), Value::Int(i)) => {
            if (*i as usize) < xs.len() {
                0
            } else {
                1
            }
        }
        _ => 2,
    }
}

/// `zal_rt_call`: the interpreter's `Call` (frame push, tick, depth limit).
/// Works on `ctx` directly: the fiber's `Vec` length is only brought up to
/// date at exits to the scheduler (the dispatcher sets it from `ctx->sp`).
#[inline]
pub(crate) unsafe fn call(ctx: *mut ZalCtx, pc: u32, callee: u32, argc: u32) -> u32 {
    let s = session(ctx);
    let frames = &mut *s.frames;
    let c = &mut *ctx;
    let [locals, need] = *s.fninfo.add(callee as usize);
    let sp = c.sp as usize;
    let nb = sp - argc as usize;
    let want = nb + (locals as usize).max(argc as usize);
    let stack = &mut *s.stack;
    // Room for the callee's locals and its verified operand depth.
    if stack.capacity() < want + need as usize {
        grow(c, stack, sp, want + need as usize);
    }
    for k in sp..want {
        c.stack.add(k).write(Value::Unit);
    }
    let n = frames.len();
    frames.get_unchecked_mut(n - 1).pc = pc as usize + 1;
    if n == frames.capacity() {
        grow_frames(frames);
    }
    frames.as_mut_ptr().add(n).write(Frame { func: callee, pc: 0, base: nb });
    frames.set_len(n + 1);
    c.sp = want as u64;
    c.base = nb as u64;
    c.func = callee;
    c.pc = 0;
    c.slice += 1;
    if c.slice >= ZAL_SLICE || n >= 10_000 {
        return call_slow(ctx, n);
    }
    ZAL_EXIT_CALL
}

#[cold]
#[inline(never)]
unsafe fn grow(c: &mut ZalCtx, stack: &mut Vec<Value>, sp: usize, cap: usize) {
    stack.set_len(sp);
    stack.reserve(cap - sp);
    c.stack = stack.as_mut_ptr();
}

#[cold]
#[inline(never)]
fn grow_frames(frames: &mut Vec<Frame>) {
    frames.reserve(frames.len().max(16));
}

/// Call tick at the slice limit, then the depth limit, in the interpreter's order.
#[cold]
#[inline(never)]
unsafe fn call_slow(ctx: *mut ZalCtx, n: usize) -> u32 {
    let s = session(ctx);
    if (*ctx).slice >= ZAL_SLICE {
        let r = (s.tick)(ctx, 0);
        if r != 0 {
            return r;
        }
    }
    if n >= 10_000 {
        let stack = &mut *s.stack;
        stack.set_len((*ctx).sp as usize);
        s.err = Some(RunError { msg: "call depth limit exceeded (10000 frames)".into(), trace: vec![] });
        return ZAL_EXIT_TRAP;
    }
    ZAL_EXIT_CALL
}

/// `Ret`: pop the result, drop the frame's locals, resume the caller.
#[inline]
pub(crate) unsafe fn ret(ctx: *mut ZalCtx) -> u32 {
    let s = session(ctx);
    let frames = &mut *s.frames;
    let c = &mut *ctx;
    let (sp, base) = (c.sp as usize, c.base as usize);
    let v = c.stack.add(sp - 1).read();
    // Drop the frame's locals and temporaries (what `truncate(base)` does).
    // With trusted slot tags, statically plain locals are skipped.
    let mut from = base;
    if !s.drop_slots.is_null() {
        let locals = (*s.fninfo.add(c.func as usize))[0] as usize;
        let end = (base + locals).min(sp - 1);
        for &l in (*s.drop_slots.add(c.func as usize)).iter() {
            let k = base + l as usize;
            if k < end {
                let p = c.stack.add(k);
                if !tag_is_plain(*(p as *const u8)) {
                    drop_slot(p);
                }
            }
        }
        from = end;
    }
    for k in from..sp - 1 {
        let p = c.stack.add(k);
        if !tag_is_plain(*(p as *const u8)) {
            drop_slot(p);
        }
    }
    let n = frames.len() - 1;
    frames.set_len(n);
    if n == 0 {
        return ret_done(ctx, v);
    }
    // The caller's frame reserved room for its own operands.
    c.stack.add(base).write(v);
    c.sp = base as u64 + 1;
    let fr = frames.get_unchecked(n - 1);
    c.base = fr.base as u64;
    c.pc = fr.pc as u32;
    c.func = fr.func;
    ZAL_EXIT_RET
}

#[inline(never)]
unsafe fn drop_slot(p: *mut Value) {
    std::ptr::drop_in_place(p);
}

#[cold]
#[inline(never)]
unsafe fn ret_done(ctx: *mut ZalCtx, v: Value) -> u32 {
    let s = session(ctx);
    s.done = Some(v);
    (*ctx).sp = (*ctx).base;
    ZAL_EXIT_DONE
}

/// `zal_rt_trap`: record an arithmetic trap of the op at `pc`.
pub(crate) unsafe fn trap(ctx: *mut ZalCtx, pc: u32, code: u32) -> u32 {
    let s = session(ctx);
    let stack = &mut *s.stack;
    let frames = &mut *s.frames;
    stack.set_len((*ctx).sp as usize);
    // As the interpreter's `fail!`: the frame pc is one past the failing op.
    frames.last_mut().unwrap().pc = pc as usize + 1;
    let msg = match code {
        ZAL_TRAP_ADD => "integer overflow in +".to_string(),
        ZAL_TRAP_SUB => "integer - overflow or division by zero".to_string(),
        ZAL_TRAP_MUL => "integer * overflow or division by zero".to_string(),
        ZAL_TRAP_DIV => "integer / overflow or division by zero".to_string(),
        ZAL_TRAP_MOD => "integer % overflow or division by zero".to_string(),
        c => format!("internal: unknown trap code {c}"),
    };
    s.err = Some(RunError { msg, trace: vec![] });
    ZAL_EXIT_TRAP
}

/// `zal_rt_tick`: works for callers that count the slice themselves (v2:
/// call only at the limit) and for v1 callers that call on every back edge.
#[inline]
pub(crate) unsafe fn tick(ctx: *mut ZalCtx, pc: u32) -> u32 {
    let c = &mut *ctx;
    if c.slice < ZAL_SLICE {
        c.slice += 1;
        if c.slice < ZAL_SLICE {
            return 0;
        }
    }
    let s = session(ctx);
    (s.tick)(ctx, pc)
}

/// `zal_rt_builtin`: non-blocking builtins with the interpreter's results;
/// every operand check happens before the first change.
pub(crate) unsafe fn builtin(ctx: *mut ZalCtx, code: u32, argc: u32) -> u32 {
    use Builtin as B;
    const ALL: [Builtin; 21] = [
        B::Len, B::Push, B::Get, B::Set, B::Remove, B::Range, B::MapNew, B::MapGet, B::MapSet, B::MapHas, B::MapKeys,
        B::ToStr, B::Print, B::Join, B::AwaitAny, B::Cancel, B::Sleep, B::JsonStr, B::JsonGetStr, B::NowMs, B::MapWithCapacity,
    ];
    let Some(&b) = ALL.get(code as usize) else { return 2 };
    debug_assert_eq!(b as u32, code);
    if argc != b.arity() as u32 {
        return 2;
    }
    let c = &mut *ctx;
    let t = |k: usize| &*c.stack.add(c.sp as usize - 1 - k);
    let in_range = |xs: &Vec<Value>, i: &Value| matches!(i, Value::Int(i) if usize::try_from(*i).is_ok_and(|i| i < xs.len()));
    // Check the operands as the interpreter's paths do (a mismatch traps
    // there, or behaves specially: leave it to the generic path).
    let ok = match b {
        B::Len => len_of(t(0)).is_some(),
        B::Push => matches!(t(1), Value::List(_)),
        B::Get => matches!((t(1), t(0)), (Value::List(_), Value::Int(_))),
        B::Set => matches!(t(2), Value::List(xs) if in_range(xs, t(1))),
        B::Remove => matches!(t(1), Value::List(xs) if in_range(xs, t(0))),
        B::Range | B::MapWithCapacity => matches!(t(0), Value::Int(_)),
        B::MapNew | B::NowMs => true,
        B::MapGet | B::MapHas => matches!((t(1), t(0)), (Value::Map(_), Value::Str(_))),
        B::MapSet => matches!((t(2), t(1)), (Value::Map(_), Value::Str(_))),
        B::MapKeys => matches!(t(0), Value::Map(_)),
        B::ToStr => true,
        B::JsonStr => matches!(t(0), Value::Str(_)),
        B::JsonGetStr => matches!((t(1), t(0)), (Value::Str(_), Value::Str(_))),
        B::Print | B::Join | B::AwaitAny | B::Cancel | B::Sleep => false,
    };
    if !ok {
        return 2;
    }
    let some = |v: Value| Value::Variant(1, Some(Rc::new(v)));
    let none = || Value::Variant(0, None);
    match b {
        B::Len => {
            let n = len_of(t(0)).unwrap_unchecked();
            *top(c, 0) = Value::Int(n as i64);
        }
        B::Push => {
            let x = pop(c);
            let Value::List(xs) = &mut *top(c, 0) else { unreachable!() };
            crate::value::list_push(xs, x);
        }
        B::Get => {
            let Value::Int(i) = pop(c) else { unreachable!() };
            let Value::List(xs) = &*top(c, 0) else { unreachable!() };
            let r = usize::try_from(i).ok().and_then(|i| xs.get(i)).cloned();
            *top(c, 0) = match r {
                Some(v) => some(v),
                None => none(),
            };
        }
        B::Set => {
            let x = pop(c);
            let Value::Int(i) = pop(c) else { unreachable!() };
            let Value::List(xs) = &mut *top(c, 0) else { unreachable!() };
            Rc::make_mut(xs)[i as usize] = x;
        }
        B::Remove => {
            let Value::Int(i) = pop(c) else { unreachable!() };
            let Value::List(xs) = &mut *top(c, 0) else { unreachable!() };
            Rc::make_mut(xs).remove(i as usize);
        }
        B::Range => {
            let Value::Int(k) = *top(c, 0) else { unreachable!() };
            top(c, 0).write(Value::List(Rc::new((0..k.max(0)).map(Value::Int).collect())));
        }
        B::MapNew => push(c, Value::Map(Rc::new(crate::value::ZMap::default()))),
        B::MapWithCapacity => {
            let Value::Int(n) = *top(c, 0) else { unreachable!() };
            let m = crate::value::ZMap::with_capacity_and_hasher(n.clamp(0, 1 << 24) as usize, Default::default());
            top(c, 0).write(Value::Map(Rc::new(m)));
        }
        B::MapGet | B::MapHas => {
            let Value::Str(k) = pop(c) else { unreachable!() };
            let Value::Map(m) = &*top(c, 0) else { unreachable!() };
            let r = if b == B::MapHas {
                Value::Bool(m.contains_key(&k))
            } else {
                match m.get(&k) {
                    Some(v) => some(v.clone()),
                    None => none(),
                }
            };
            *top(c, 0) = r;
        }
        B::MapSet => {
            let v = pop(c);
            let Value::Str(k) = pop(c) else { unreachable!() };
            let Value::Map(m) = &mut *top(c, 0) else { unreachable!() };
            Rc::make_mut(m).insert(k, v);
        }
        B::MapKeys => {
            let Value::Map(m) = &*top(c, 0) else { unreachable!() };
            let mut keys: Vec<Rc<str>> = m.keys().cloned().collect();
            keys.sort();
            *top(c, 0) = Value::List(Rc::new(keys.into_iter().map(Value::Str).collect()));
        }
        B::ToStr => {
            let s = (*top(c, 0)).to_string();
            *top(c, 0) = Value::Str(Rc::from(s));
        }
        B::JsonStr => {
            let Value::Str(x) = &*top(c, 0) else { unreachable!() };
            let s = serde_json::to_string(&**x).unwrap_or_default();
            *top(c, 0) = Value::Str(Rc::from(s));
        }
        B::JsonGetStr => {
            let Value::Str(field) = pop(c) else { unreachable!() };
            let Value::Str(doc) = &*top(c, 0) else { unreachable!() };
            let got = serde_json::from_str::<serde_json::Value>(doc).ok().and_then(|j| j.get(&*field).and_then(|v| v.as_str()).map(|s| s.to_string()));
            *top(c, 0) = match got {
                Some(v) => some(Value::Str(Rc::from(v))),
                None => none(),
            };
        }
        B::NowMs => {
            let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
            push(c, Value::Int(ms));
        }
        B::Print | B::Join | B::AwaitAny | B::Cancel | B::Sleep => unreachable!(),
    }
    0
}
