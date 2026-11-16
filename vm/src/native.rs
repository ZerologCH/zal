//! Native-code ABI (RFC 0002): the contract between the VM runtime and the
//! machine-code backends (Cranelift "A", own x86-64 "B").
//!
//! A backend compiles each IR function into a re-entrant body
//! `uint32_t zal_fn_<i>(ZalCtx *ctx, uint32_t entry_pc)`. The VM's dispatcher
//! calls it instead of interpreting the function; the body executes ops
//! (inline fast paths or the generic path [`zal_rt_op`]) until it must leave
//! and returns one of the `ZAL_EXIT_*` codes.

use std::ffi::c_void;
use std::mem::offset_of;
use std::rc::Rc;

use zal_ir::Program;

use crate::value::Value;

/// `ZalValue` in the C ABI: [`Value`] with its fixed `#[repr(C, u8)]` layout.
pub type ZalValue = Value;

/// The op at `ctx->pc` pushed a new frame; the dispatcher runs the callee and
/// later re-enters the caller at its saved resume pc.
pub const ZAL_EXIT_CALL: u32 = 1;
/// The function returned to a caller frame; the result is on the stack.
pub const ZAL_EXIT_RET: u32 = 2;
/// The fiber blocks (host call, await, sleep, preemption tick, slot wait);
/// the frame pc is saved in the runtime.
pub const ZAL_EXIT_SUSPEND: u32 = 3;
/// A runtime error is recorded in the runtime state.
pub const ZAL_EXIT_TRAP: u32 = 4;
/// The fiber's outermost function returned (no caller frame left); the
/// result is held by the runtime, not on the stack.
pub const ZAL_EXIT_DONE: u32 = 5;
/// `zal_rt_op` only: continue at `ctx->pc` (a taken branch).
pub const ZAL_JUMP: u32 = 6;
/// `zal_rt_op` only: continue at `pc + 1`.
pub const ZAL_NEXT: u32 = 0;

/// Trap codes for [`zal_rt_trap`] (messages equal the interpreter's).
pub const ZAL_TRAP_ADD: u32 = 1;
pub const ZAL_TRAP_SUB: u32 = 2;
pub const ZAL_TRAP_MUL: u32 = 3;
pub const ZAL_TRAP_DIV: u32 = 4;
pub const ZAL_TRAP_MOD: u32 = 5;

/// Value tags (RFC 0002 §3). Tags 0, 1, 2 and 4 are plain (no ownership).
pub const TAG_UNIT: u8 = 0;
pub const TAG_INT: u8 = 1;
pub const TAG_BOOL: u8 = 2;
pub const TAG_STR: u8 = 3;
pub const TAG_DUR: u8 = 4;
pub const TAG_LIST: u8 = 5;
pub const TAG_MAP: u8 = 6;
/// Byte offset of the payload inside a value. Owned payloads (`Str`, `List`,
/// `Map`) are `Rc` pointers to the allocation header (strong count, weak
/// count, then the data); machine code treats them as opaque and copies/drops
/// them only through `zal_rt_clone`/`zal_rt_drop`.
pub const VALUE_PAYLOAD_OFFSET: usize = 8;
/// Size of one value on the operand stack.
pub const VALUE_SIZE: usize = 24;

/// True when a value with this tag may be copied/overwritten as raw bytes.
#[inline]
pub const fn tag_is_plain(tag: u8) -> bool {
    matches!(tag, TAG_UNIT | TAG_INT | TAG_BOOL | TAG_DUR)
}

/// Execution context shared with machine code (RFC 0002 §2).
#[repr(C)]
#[derive(Debug)]
pub struct ZalCtx {
    /// Opaque runtime state; machine code must not touch it.
    pub vm: *mut c_void,
    /// Base of the fiber's operand stack buffer (re-read after runtime calls).
    pub stack: *mut ZalValue,
    /// Number of live values on the stack; top is `stack[sp - 1]`.
    pub sp: u64,
    /// Index of local 0 of the current frame.
    pub base: u64,
    /// Jump target / resume pc written by the runtime.
    pub pc: u32,
    /// Index of the current function.
    pub func: u32,
    /// Preemption counter (runtime-owned).
    pub slice: u32,
}

pub const CTX_OFF_VM: usize = offset_of!(ZalCtx, vm);
pub const CTX_OFF_STACK: usize = offset_of!(ZalCtx, stack);
pub const CTX_OFF_SP: usize = offset_of!(ZalCtx, sp);
pub const CTX_OFF_BASE: usize = offset_of!(ZalCtx, base);
pub const CTX_OFF_PC: usize = offset_of!(ZalCtx, pc);
pub const CTX_OFF_FUNC: usize = offset_of!(ZalCtx, func);
pub const CTX_OFF_SLICE: usize = offset_of!(ZalCtx, slice);

/// A compiled function body: `uint32_t zal_fn_<i>(ZalCtx *ctx, uint32_t entry_pc)`.
pub type NativeFn = unsafe extern "C" fn(*mut ZalCtx, u32) -> u32;

/// Back edges and calls between deadline/preemption checks (the
/// interpreter's slice). Inline code counts taken backward branches in
/// `ctx->slice` and calls `zal_rt_tick` only when it reaches this value.
pub const ZAL_SLICE: u32 = 512;

/// `builtin` codes for [`zal_rt_upd`] (`zal_ir::Builtin` declaration order).
pub const ZAL_BUILTIN_PUSH: u32 = zal_ir::Builtin::Push as u32;
pub const ZAL_BUILTIN_SET: u32 = zal_ir::Builtin::Set as u32;
pub const ZAL_BUILTIN_REMOVE: u32 = zal_ir::Builtin::Remove as u32;
pub const ZAL_BUILTIN_MAP_SET: u32 = zal_ir::Builtin::MapSet as u32;

/// Generic path: execute `functions[ctx->func].code[pc]` with exactly the
/// interpreter's semantics. Returns `ZAL_NEXT` (continue at `pc + 1`),
/// `ZAL_JUMP` (continue at `ctx->pc`) or an exit code to return from the body.
/// Afterwards `ctx->stack`, `sp` and `base` are current; `ctx->pc` and
/// `ctx->func` are current after `ZAL_JUMP` and every exit code.
///
/// # Safety
/// `ctx` must be the context the dispatcher passed to the running body.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_op(ctx: *mut ZalCtx, pc: u32) -> u32 {
    crate::rt::op(ctx, pc)
}

/// Slice check for a taken backward branch to `pc` (the target = resume pc).
/// v2 callers increment `ctx->slice` themselves and call this only when it
/// reached `ZAL_SLICE`; v1 callers that call on every back edge (without
/// incrementing) get the same accounting. No effect besides the slice check:
/// returns 0, or `ZAL_EXIT_SUSPEND` / `ZAL_EXIT_TRAP` with `ctx->pc` = `pc`.
///
/// # Safety
/// As for [`zal_rt_op`].
#[no_mangle]
pub unsafe extern "C" fn zal_rt_tick(ctx: *mut ZalCtx, pc: u32) -> u32 {
    crate::rt::tick(ctx, pc)
}

/// Record the trap `code` (`ZAL_TRAP_*`) for the op at `pc`; returns `ZAL_EXIT_TRAP`.
///
/// # Safety
/// As for [`zal_rt_op`].
#[no_mangle]
pub unsafe extern "C" fn zal_rt_trap(ctx: *mut ZalCtx, pc: u32, code: u32) -> u32 {
    crate::rt::trap(ctx, pc, code)
}

/// `Call(callee, argc)` at `pc`: push the callee's frame (arguments are the
/// top `argc` values), count the call tick and check the depth limit. Returns
/// `ZAL_EXIT_CALL` (ctx now describes the callee at pc 0; return it from the
/// body), or `ZAL_EXIT_SUSPEND` / `ZAL_EXIT_TRAP`. The caller is re-entered at
/// `pc + 1` after the callee returns.
///
/// # Safety
/// As for [`zal_rt_op`].
#[no_mangle]
pub unsafe extern "C" fn zal_rt_call(ctx: *mut ZalCtx, pc: u32, callee: u32, argc: u32) -> u32 {
    crate::rt::call(ctx, pc, callee, argc)
}

/// `Ret` without decoding the op (same as `zal_rt_op` on a `Ret`): returns
/// `ZAL_EXIT_RET` (ctx describes the caller at its resume pc) or
/// `ZAL_EXIT_DONE` (the fiber's outermost function returned).
///
/// # Safety
/// As for [`zal_rt_op`]; the result is the top of the stack.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_ret(ctx: *mut ZalCtx) -> u32 {
    crate::rt::ret(ctx)
}

/// Write a clone of `*src` into the uninitialised slot `*dst`.
///
/// # Safety
/// `src` must point to a live value, `dst` to writable uninitialised memory.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_clone(dst: *mut ZalValue, src: *const ZalValue) {
    dst.write((*src).clone());
}

/// Drop the value in place (the slot is uninitialised afterwards).
///
/// # Safety
/// `v` must point to a live value that nothing else drops.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_drop(v: *mut ZalValue) {
    std::ptr::drop_in_place(v);
}

// ---- Typed helpers (RFC 0002 v2 §3) --------------------------------------
//
// No frame or pc bookkeeping. Pointers point into the operand stack or the
// locals. A `dst` is uninitialised on entry, or it is the very pointer of the
// value being read (then that value is consumed: dropped after reading).
// Nonzero results mean "not applicable": nothing was changed, take the
// generic path (which produces the interpreter's trap where there is one).

/// Clone with the plain tags copied inline (the derived clone is out of line).
#[inline(always)]
unsafe fn clone_val(v: *const Value) -> Value {
    if tag_is_plain(*(v as *const u8)) {
        v.read()
    } else {
        (*v).clone()
    }
}

/// Overwrite a live slot; plain old values need no drop call.
#[inline(always)]
unsafe fn assign(dst: *mut Value, v: Value) {
    if !tag_is_plain(*(dst as *const u8)) {
        std::ptr::drop_in_place(dst);
    }
    dst.write(v);
}

/// Write `v` into `dst`, consuming `*src` first when `dst` aliases it.
#[inline(always)]
unsafe fn put(dst: *mut ZalValue, src: *const ZalValue, v: Value) {
    if dst as *const ZalValue == src {
        std::ptr::drop_in_place(dst);
    }
    dst.write(v);
}

/// Length of a List, Map or Str (in chars), or -1.
///
/// # Safety
/// `v` points to a live value.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_len(v: *const ZalValue) -> i64 {
    match &*v {
        Value::List(xs) => xs.len() as i64,
        Value::Map(m) => m.len() as i64,
        Value::Str(s) => s.chars().count() as i64,
        _ => -1,
    }
}

/// Clone `list[i]` into `dst`: 0, or 1 when out of bounds / not a list.
///
/// # Safety
/// See the helper conventions above.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_index(dst: *mut ZalValue, list: *const ZalValue, i: i64) -> u32 {
    let v = match &*list {
        Value::List(xs) => match usize::try_from(i).ok().and_then(|i| xs.get(i)) {
            Some(v) => clone_val(v),
            None => return 1,
        },
        _ => return 1,
    };
    put(dst, list, v);
    0
}

/// `ForNext` on the locals `slot3[0..3]` = [list, Int index, loop variable]:
/// 0 = advanced (variable = element, index + 1), 1 = exhausted, 2 = generic.
///
/// # Safety
/// `slot3` points to three live values.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_for_next(slot3: *mut ZalValue) -> u32 {
    let i = match &*slot3.add(1) {
        Value::Int(i) => *i as usize,
        _ => return 2,
    };
    let x = match &*slot3 {
        Value::List(xs) => match xs.get(i) {
            Some(x) => x as *const Value,
            None => return 1,
        },
        _ => return 2,
    };
    // Clone before dropping the old loop variable (the list in slot 0 keeps
    // the element alive either way).
    let v = clone_val(x);
    assign(slot3.add(2), v);
    slot3.add(1).write(Value::Int(i as i64 + 1));
    0
}

/// In-place `UpdLocal` on `*local`: `builtin` is `ZAL_BUILTIN_PUSH` (args
/// [x]), `ZAL_BUILTIN_REMOVE` ([i]), `ZAL_BUILTIN_SET` ([i, x]) or
/// `ZAL_BUILTIN_MAP_SET` ([key, value]); `args` points at the first (deepest)
/// argument. 0 = done, the arguments were consumed (moved: drop `sp` by the
/// argument count without dropping them); 2 = generic, nothing changed.
///
/// # Safety
/// `local` and the arguments are live values.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_upd(local: *mut ZalValue, args: *mut ZalValue, builtin: u32) -> u32 {
    match (builtin, &mut *local) {
        (ZAL_BUILTIN_PUSH, Value::List(xs)) => {
            crate::value::list_push(xs, args.read());
        }
        (ZAL_BUILTIN_REMOVE, Value::List(xs)) => {
            let Value::Int(i) = &*args else { return 2 };
            let Some(i) = usize::try_from(*i).ok().filter(|i| *i < xs.len()) else { return 2 };
            Rc::make_mut(xs).remove(i);
        }
        (ZAL_BUILTIN_SET, Value::List(xs)) => {
            let Value::Int(i) = &*args else { return 2 };
            let Some(i) = usize::try_from(*i).ok().filter(|i| *i < xs.len()) else { return 2 };
            Rc::make_mut(xs)[i] = args.add(1).read();
        }
        (ZAL_BUILTIN_MAP_SET, Value::Map(m)) => {
            let Value::Str(_) = &*args else { return 2 };
            let Value::Str(k) = args.read() else { unreachable!() };
            Rc::make_mut(m).insert(k, args.add(1).read());
        }
        _ => return 2,
    }
    0
}

/// Variant tag (Bool: 0/1), or 0xFFFF when `v` is neither.
///
/// # Safety
/// `v` points to a live value.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_tag(v: *const ZalValue) -> u32 {
    match &*v {
        Value::Variant(t, _) => *t as u32,
        Value::Bool(b) => *b as u32,
        _ => 0xFFFF,
    }
}

/// Clone the payload of a variant into `dst`: 0, or 2 (no payload / not a variant).
///
/// # Safety
/// See the helper conventions above.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_payload(dst: *mut ZalValue, v: *const ZalValue) -> u32 {
    let p = match &*v {
        Value::Variant(_, Some(p)) => (**p).clone(),
        _ => return 2,
    };
    put(dst, v, p);
    0
}

/// Clone field `i` of a record into `dst`: 0, or 2 (not a record / no such field).
///
/// # Safety
/// See the helper conventions above.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_field(dst: *mut ZalValue, rec: *const ZalValue, i: u32) -> u32 {
    let x = match &*rec {
        Value::Record(xs) => match xs.get(i as usize) {
            Some(x) => clone_val(x),
            None => return 2,
        },
        _ => return 2,
    };
    put(dst, rec, x);
    0
}

/// Look `key` (Str) up in `map`: 0 = found (value cloned into `dst`),
/// 1 = missing (`dst` untouched), 2 = generic.
///
/// # Safety
/// See the helper conventions above (`dst` must not alias `key`).
#[no_mangle]
pub unsafe extern "C" fn zal_rt_map_get(dst: *mut ZalValue, map: *const ZalValue, key: *const ZalValue) -> u32 {
    let got = match (&*map, &*key) {
        (Value::Map(m), Value::Str(k)) => m.get(k).cloned(),
        _ => return 2,
    };
    match got {
        Some(v) => {
            put(dst, map, v);
            0
        }
        None => 1,
    }
}

/// Clone constant `const_index` of the running program into uninitialised `dst`.
///
/// # Safety
/// `ctx` is the running context and the index is in range (verified IR).
#[no_mangle]
pub unsafe extern "C" fn zal_rt_const(dst: *mut ZalValue, const_index: u32, ctx: *const ZalCtx) {
    let s = &*((*ctx).vm as *const crate::rt::Session);
    dst.write((*s.consts.add(const_index as usize)).clone());
}

/// `ConcatN(n)` on the top `n` stack values (all Str): replaces them with the
/// concatenation and updates `ctx->sp`. 0, or 2 (an operand is not a Str).
///
/// # Safety
/// `ctx` is the running context with at least `n` values on the stack.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_concat(ctx: *mut ZalCtx, n: u32) -> u32 {
    let c = &mut *ctx;
    let at = c.sp as usize - n as usize;
    let mut total = 0;
    for k in at..c.sp as usize {
        match &*c.stack.add(k) {
            Value::Str(x) => total += x.len(),
            _ => return 2,
        }
    }
    let mut out = String::with_capacity(total);
    for k in at..c.sp as usize {
        let Value::Str(x) = c.stack.add(k).read() else { unreachable!() };
        out.push_str(&x);
    }
    c.stack.add(at).write(Value::Str(Rc::from(out)));
    c.sp = at as u64 + 1;
    0
}

// ---- v3 helpers (RFC 0002 addendum v3) ------------------------------------

/// `kind` codes for [`zal_rt_make`].
pub const ZAL_MAKE_LIST: u32 = 0;
pub const ZAL_MAKE_TUPLE: u32 = 1;
pub const ZAL_MAKE_RECORD: u32 = 2;
/// `MakeVariant(tag, has)`: `n` = `tag | (has << 16)`; with `has` the payload
/// is the top stack value.
pub const ZAL_MAKE_VARIANT: u32 = 3;

/// `MakeList(n)` / `MakeTuple(n)` / `MakeRecord(n)` on the top `n` stack
/// values, or `MakeVariant` (see [`ZAL_MAKE_VARIANT`]); replaces them with the
/// aggregate and updates `ctx->sp`. 0, or 2 for an unknown `kind` (nothing changed).
///
/// # Safety
/// `ctx` is the running context with the operands on the stack.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_make(ctx: *mut ZalCtx, kind: u32, n: u32) -> u32 {
    let c = &mut *ctx;
    if kind == ZAL_MAKE_VARIANT {
        let (tag, has) = ((n & 0xFFFF) as u16, n >> 16 != 0);
        if has {
            let at = c.stack.add(c.sp as usize - 1);
            at.write(Value::Variant(tag, Some(Rc::new(at.read()))));
        } else {
            c.stack.add(c.sp as usize).write(Value::Variant(tag, None));
            c.sp += 1;
        }
        return 0;
    }
    if kind > ZAL_MAKE_RECORD {
        return 2;
    }
    let n = n as usize;
    let at = c.sp as usize - n;
    let v = if kind == ZAL_MAKE_LIST {
        let mut xs = Vec::with_capacity(n);
        std::ptr::copy_nonoverlapping(c.stack.add(at), xs.as_mut_ptr(), n);
        xs.set_len(n);
        Value::List(Rc::new(xs))
    } else {
        let xs = crate::value::rc_slice_take(c.stack.add(at), n);
        if kind == ZAL_MAKE_TUPLE {
            Value::Tuple(xs)
        } else {
            Value::Record(xs)
        }
    };
    c.stack.add(at).write(v);
    c.sp = at as u64 + 1;
    0
}

/// `Builtin(builtin, argc)` (`builtin` = `zal_ir::Builtin` index) for every
/// builtin that cannot block or touch the scheduler: Len, Push, Get, Set,
/// Remove, Range, MapNew, MapWithCapacity, MapGet, MapSet, MapHas, MapKeys,
/// ToStr, JsonStr, JsonGetStr, NowMs. Operates on the top `argc` stack values
/// and updates `ctx->sp`/`ctx->stack`. 0, or 2 = generic path (Print, Join,
/// AwaitAny, Cancel, Sleep, or operands the interpreter would trap on;
/// nothing changed).
///
/// # Safety
/// `ctx` is the running context with the operands on the stack.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_builtin(ctx: *mut ZalCtx, builtin: u32, argc: u32) -> u32 {
    crate::rt::builtin(ctx, builtin, argc)
}

/// `MapGetJump(local, _)`: the top two stack values are map and key (Str).
/// Consumes both; 0 = found (the value replaces local `local`), 1 = missing
/// (take the branch), 2 = generic (nothing changed).
///
/// # Safety
/// `ctx` is the running context with map and key on the stack.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_map_get_jump(ctx: *mut ZalCtx, local: u32) -> u32 {
    let c = &mut *ctx;
    let sp = c.sp as usize;
    let got = match (&*c.stack.add(sp - 2), &*c.stack.add(sp - 1)) {
        (Value::Map(m), Value::Str(k)) => m.get(k).cloned(),
        _ => return 2,
    };
    std::ptr::drop_in_place(c.stack.add(sp - 1));
    std::ptr::drop_in_place(c.stack.add(sp - 2));
    c.sp -= 2;
    match got {
        Some(v) => {
            *c.stack.add(c.base as usize + local as usize) = v;
            0
        }
        None => 1,
    }
}

// ---- Data layout for inline access (RFC 0002 addendum v4) ---------------

/// `Tuple` / `Record` / `Variant` tags (runtime-private in v1, layout below
/// is part of addendum v4 for JIT code only).
pub const TAG_TUPLE: u8 = 7;
pub const TAG_RECORD: u8 = 8;
pub const TAG_VARIANT: u8 = 9;
/// `Task(u32)`: no ownership (JIT code may copy and drop it raw, v4).
pub const TAG_TASK: u8 = 10;

/// Where machine code finds the data of owned values, measured in this very
/// process (see [`data_layout`]). All offsets are bytes.
///
/// - Every owned payload word at value+8 points to an `Rc` allocation:
///   strong count at +0, weak count at +8, data from +16.
/// - List (`Rc<Vec<Value>>`): the `Vec`'s buffer pointer, capacity and
///   length are words at `list_ptr` / `list_cap` / `list_len` from the
///   allocation start; elements are 24-byte values.
/// - Str, Tuple, Record (`Rc<str>`, `Rc<[Value]>`): the value holds a fat
///   pointer; the element/byte count is the word at value+`fat_len`, the
///   allocation pointer the word at value+`fat_ptr`; data at allocation+16.
/// - Variant: `u16` tag at value+8, `Option<Rc<Value>>` at value+16 (0 = no
///   payload; else the payload value is at allocation+16).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataLayout {
    pub list_ptr: i32,
    pub list_cap: i32,
    pub list_len: i32,
    pub fat_ptr: i32,
    pub fat_len: i32,
    pub variant_tag: i32,
    pub variant_ptr: i32,
}

/// The data layout of this build, or `None` if it does not have the shape
/// [`DataLayout`] describes (then JIT code must use the helpers). Only valid
/// for code that runs in this process: AOT objects, which link a runtime
/// built separately, must not depend on it.
pub fn data_layout() -> Option<DataLayout> {
    static L: std::sync::OnceLock<Option<DataLayout>> = std::sync::OnceLock::new();
    *L.get_or_init(probe_layout)
}

fn probe_layout() -> Option<DataLayout> {
    unsafe fn word(p: *const u8, off: usize) -> usize {
        (p.add(off) as *const usize).read_unaligned()
    }
    unsafe {
        // List: distinct capacity and length, find the three words.
        let mut v: Vec<Value> = Vec::with_capacity(7);
        v.push(Value::Int(11));
        v.push(Value::Int(12));
        v.push(Value::Int(13));
        let buf = v.as_ptr() as usize;
        let cap = v.capacity();
        let list = Value::List(Rc::new(v));
        let lv = &list as *const Value as *const u8;
        if *lv != TAG_LIST {
            return None;
        }
        let alloc = word(lv, 8) as *const u8;
        if word(alloc, 0) != 1 || word(alloc, 8) != 1 {
            return None;
        }
        let find = |want: usize| (16..40).step_by(8).find(|&o| word(alloc, o) == want);
        let (list_ptr, list_cap, list_len) = (find(buf)?, find(cap)?, find(3)?);
        if cap == 3 || buf == 3 || cap == buf {
            return None;
        }
        // Record: fat pointer; the count is 2.
        let rec = Value::Record(Rc::from(vec![Value::Int(21), Value::Int(22)]));
        let rv = &rec as *const Value as *const u8;
        if *rv != TAG_RECORD {
            return None;
        }
        let (fat_ptr, fat_len) = if word(rv, 16) == 2 { (8, 16) } else if word(rv, 8) == 2 { (16, 8) } else { return None };
        let ra = word(rv, fat_ptr) as *const u8;
        if word(ra, 0) != 1 || *ra.add(16) != TAG_INT || word(ra, 24) != 21 || word(ra, 40 + 8) != 22 {
            return None;
        }
        // Str uses the same fat-pointer shape.
        let s = Value::Str(Rc::from("abc"));
        let sv = &s as *const Value as *const u8;
        if *sv != TAG_STR || word(sv, fat_len) != 3 || *(word(sv, fat_ptr) as *const u8).add(16) != b'a' {
            return None;
        }
        // Variant: u16 tag at +8, payload pointer at +16 (null = none).
        let var = Value::Variant(0x1234, Some(Rc::new(Value::Int(77))));
        let vv = &var as *const Value as *const u8;
        let none = Value::Variant(0x4321, None);
        let nv = &none as *const Value as *const u8;
        if *vv != TAG_VARIANT || (vv.add(8) as *const u16).read() != 0x1234 || (nv.add(8) as *const u16).read() != 0x4321 || word(nv, 16) != 0 {
            return None;
        }
        let va = word(vv, 16) as *const u8;
        if va.is_null() || word(va, 0) != 1 || *va.add(16) != TAG_INT || word(va, 24) != 77 {
            return None;
        }
        // Tuple shares the record representation.
        let t = Value::Tuple(Rc::from(vec![Value::Unit]));
        if *(&t as *const Value as *const u8) != TAG_TUPLE {
            return None;
        }
        let task = Value::Task(5);
        if *(&task as *const Value as *const u8) != TAG_TASK || (((&task as *const Value as *const u8).add(8)) as *const u32).read() != 5 {
            return None;
        }
        Some(DataLayout {
            list_ptr: list_ptr as i32,
            list_cap: list_cap as i32,
            list_len: list_len as i32,
            fat_ptr: fat_ptr as i32,
            fat_len: fat_len as i32,
            variant_tag: 8,
            variant_ptr: 16,
        })
    }
}

/// Values JIT code may clone inline (addendum v4): `prog.consts` in order,
/// then one empty List (index `prog.consts.len()`). Values are immutable, so
/// sharing these with the VM's own copies is unobservable (`MakeList(0)`
/// becomes a reference to the shared empty list; the first push copies it
/// into a fresh buffer, as for any shared list). The owner keeps the box
/// alive while the code runs; clones taken from it stay valid after that.
pub fn jit_constants(prog: &Program) -> Box<[Value]> {
    let mut v: Vec<Value> = prog
        .consts
        .iter()
        .map(|c| match c {
            zal_ir::Const::Int(n) => Value::Int(*n),
            zal_ir::Const::Str(s) => Value::Str(Rc::from(s.as_str())),
            zal_ir::Const::Bool(b) => Value::Bool(*b),
            zal_ir::Const::Dur(ms) => Value::Dur(*ms),
        })
        .collect();
    v.push(Value::List(Rc::new(Vec::new())));
    v.into_boxed_slice()
}

/// `UpdLocal(push)` on a local holding a shared empty list (`MakeList(0)`
/// from `jit_constants`): replace it by a fresh list holding `*x` (moved).
/// Same result as the copy-on-write push, without the generic path.
///
/// # Safety
/// `local` holds a List, `x` a live value that is moved out.
#[no_mangle]
pub unsafe extern "C" fn zal_rt_list_one(local: *mut ZalValue, x: *mut ZalValue) {
    let mut v = Vec::with_capacity(4);
    v.push(x.read());
    let old = local.replace(Value::List(Rc::new(v)));
    drop(old);
}

/// Name and address of every runtime entry point, for JIT symbol resolution.
pub fn runtime_symbols() -> [(&'static str, *const u8); 21] {
    [
        ("zal_rt_op", zal_rt_op as *const u8),
        ("zal_rt_tick", zal_rt_tick as *const u8),
        ("zal_rt_trap", zal_rt_trap as *const u8),
        ("zal_rt_call", zal_rt_call as *const u8),
        ("zal_rt_ret", zal_rt_ret as *const u8),
        ("zal_rt_clone", zal_rt_clone as *const u8),
        ("zal_rt_drop", zal_rt_drop as *const u8),
        ("zal_rt_len", zal_rt_len as *const u8),
        ("zal_rt_index", zal_rt_index as *const u8),
        ("zal_rt_for_next", zal_rt_for_next as *const u8),
        ("zal_rt_upd", zal_rt_upd as *const u8),
        ("zal_rt_tag", zal_rt_tag as *const u8),
        ("zal_rt_payload", zal_rt_payload as *const u8),
        ("zal_rt_field", zal_rt_field as *const u8),
        ("zal_rt_map_get", zal_rt_map_get as *const u8),
        ("zal_rt_const", zal_rt_const as *const u8),
        ("zal_rt_concat", zal_rt_concat as *const u8),
        ("zal_rt_make", zal_rt_make as *const u8),
        ("zal_rt_builtin", zal_rt_builtin as *const u8),
        ("zal_rt_map_get_jump", zal_rt_map_get_jump as *const u8),
        ("zal_rt_list_one", zal_rt_list_one as *const u8),
    ]
}

/// Machine code produced by a JIT backend. Owns the executable memory (the
/// `keep` box) for as long as the table may be called: keep it alive while
/// any `Vm` holding the table runs.
pub struct JitModule {
    table: Vec<Option<NativeFn>>,
    _keep: Box<dyn std::any::Any>,
}

impl JitModule {
    /// `table[i]` is the body of function `i` (None = interpret it); `keep`
    /// owns whatever backs the code (mapped pages, a `cranelift_jit::JITModule`, ...).
    pub fn new(table: Vec<Option<NativeFn>>, keep: Box<dyn std::any::Any>) -> Self {
        JitModule { table, _keep: keep }
    }

    /// The function table for [`crate::Vm::set_native`].
    pub fn table(&self) -> Vec<Option<NativeFn>> {
        self.table.clone()
    }
}

/// A machine-code backend. Streams A and B implement this in their crates and
/// export `pub fn backend() -> Box<dyn NativeBackend>`.
pub trait NativeBackend {
    /// Name used by `--backend` (`cranelift`, `x64`).
    fn name(&self) -> &'static str;
    /// Compile `p` into executable memory.
    fn jit(&self, p: &Program) -> Result<JitModule, String>;
    /// Compile `p` into an ELF relocatable object with the AOT exports of
    /// RFC 0002 §5 (`zal_fn_<i>`, `zal_fn_table`, `zal_fn_count`, `zal_ir`, `zal_ir_len`).
    fn object(&self, p: &Program) -> Result<Vec<u8>, String>;
}

/// Test-only "backend" without code generation. `fake_body` walks a
/// function's ops like generated code would: inline fast paths (plain
/// loads/stores, Int add, Bool branches, v2 inline slice counting on back
/// edges), the typed helpers, `zal_rt_call`, everything else through
/// `zal_rt_op`, following `ZAL_JUMP` via `ctx->pc`. `generic_body` sends every
/// op through `zal_rt_op` (the lean path and its fallbacks). The function
/// index comes from `ctx->func`, which a real backend knows statically.
#[doc(hidden)]
pub mod fake {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zal_ir::{Builtin, Op};

    static ENTRIES: AtomicU64 = AtomicU64::new(0);

    /// How often any fake body was entered (process-wide), so tests can prove
    /// the native path ran.
    pub fn entries() -> u64 {
        ENTRIES.load(Ordering::Relaxed)
    }

    /// Every function runs through `fake_body`.
    pub fn table(p: &Program) -> Vec<Option<NativeFn>> {
        vec![Some(fake_body as NativeFn); p.functions.len()]
    }

    /// Even-numbered functions through `generic_body`, odd ones interpreted:
    /// every interpreter/native transition of calls and returns.
    pub fn table_mixed(p: &Program) -> Vec<Option<NativeFn>> {
        (0..p.functions.len()).map(|i| if i % 2 == 0 { Some(generic_body as NativeFn) } else { None }).collect()
    }

    /// Every function through `generic_body`.
    pub fn table_generic(p: &Program) -> Vec<Option<NativeFn>> {
        vec![Some(generic_body as NativeFn); p.functions.len()]
    }

    #[inline(always)]
    unsafe fn tag(v: *const ZalValue) -> u8 {
        *(v as *const u8)
    }

    #[inline(always)]
    unsafe fn int_at(v: *const ZalValue) -> i64 {
        *((v as *const u8).add(VALUE_PAYLOAD_OFFSET) as *const i64)
    }

    unsafe fn code<'a>(ctx: *mut ZalCtx) -> &'a [Op] {
        let s = &*((*ctx).vm as *const crate::rt::Session);
        let prog = &*s.prog;
        &prog.functions[(*ctx).func as usize].code
    }

    /// Taken branch from `pc` to `t` with v2 inline slice counting.
    #[inline(always)]
    unsafe fn branch(ctx: *mut ZalCtx, pc: u32, t: u32) -> u32 {
        if t <= pc {
            (*ctx).slice += 1;
            if (*ctx).slice >= ZAL_SLICE {
                return zal_rt_tick(ctx, t);
            }
        }
        0
    }

    /// # Safety
    /// Called by the dispatcher only, with its context.
    pub unsafe extern "C" fn generic_body(ctx: *mut ZalCtx, entry_pc: u32) -> u32 {
        // Plain load+store, no locked RMW: an exact count does not matter.
        ENTRIES.store(ENTRIES.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        let mut pc = entry_pc;
        loop {
            match zal_rt_op(ctx, pc) {
                ZAL_NEXT => pc += 1,
                ZAL_JUMP => pc = (*ctx).pc,
                exit => return exit,
            }
        }
    }

    /// # Safety
    /// Called by the dispatcher only, with its context.
    pub unsafe extern "C" fn fake_body(ctx: *mut ZalCtx, entry_pc: u32) -> u32 {
        // Plain load+store, no locked RMW: an exact count does not matter.
        ENTRIES.store(ENTRIES.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        let code = code(ctx);
        let mut pc = entry_pc;
        loop {
            let c = &mut *ctx;
            let sp = c.sp as usize;
            let r = match code[pc as usize] {
                Op::Unit => {
                    c.stack.add(sp).write(Value::Unit);
                    c.sp += 1;
                    ZAL_NEXT
                }
                Op::Const(i) => {
                    zal_rt_const(c.stack.add(sp), i, ctx);
                    (*ctx).sp += 1;
                    ZAL_NEXT
                }
                Op::LoadLocal(s) => {
                    let src = c.stack.add(c.base as usize + s as usize);
                    let dst = c.stack.add(sp);
                    if tag_is_plain(tag(src)) {
                        std::ptr::copy_nonoverlapping(src, dst, 1);
                    } else {
                        zal_rt_clone(dst, src);
                    }
                    c.sp += 1;
                    ZAL_NEXT
                }
                Op::StoreLocal(s) => {
                    let dst = c.stack.add(c.base as usize + s as usize);
                    if !tag_is_plain(tag(dst)) {
                        zal_rt_drop(dst);
                    }
                    c.sp -= 1;
                    std::ptr::copy_nonoverlapping(c.stack.add(c.sp as usize), dst, 1);
                    ZAL_NEXT
                }
                Op::Pop => {
                    let v = c.stack.add(sp - 1);
                    if !tag_is_plain(tag(v)) {
                        zal_rt_drop(v);
                    }
                    c.sp -= 1;
                    ZAL_NEXT
                }
                Op::Add if tag(c.stack.add(sp - 1)) == TAG_INT && tag(c.stack.add(sp - 2)) == TAG_INT => {
                    let (a, b) = (int_at(c.stack.add(sp - 2)), int_at(c.stack.add(sp - 1)));
                    match a.checked_add(b) {
                        Some(r) => {
                            c.sp -= 1;
                            c.stack.add(sp - 2).write(Value::Int(r));
                            ZAL_NEXT
                        }
                        None => return zal_rt_trap(ctx, pc, ZAL_TRAP_ADD),
                    }
                }
                Op::IncLocal(s, k) if tag(c.stack.add(c.base as usize + s as usize)) == TAG_INT => {
                    let at = c.stack.add(c.base as usize + s as usize);
                    match int_at(at).checked_add(k as i64) {
                        Some(r) => {
                            at.write(Value::Int(r));
                            ZAL_NEXT
                        }
                        None => return zal_rt_trap(ctx, pc, ZAL_TRAP_ADD),
                    }
                }
                Op::JumpIfFalse(t) if tag(c.stack.add(sp - 1)) == TAG_BOOL => {
                    c.sp -= 1;
                    let b = *(c.stack.add(sp - 1) as *const u8).add(VALUE_PAYLOAD_OFFSET);
                    if b == 0 {
                        let r = branch(ctx, pc, t);
                        if r != 0 {
                            return r;
                        }
                        pc = t;
                        continue;
                    }
                    ZAL_NEXT
                }
                Op::Jump(t) => {
                    let r = branch(ctx, pc, t);
                    if r != 0 {
                        return r;
                    }
                    pc = t;
                    continue;
                }
                Op::Call(fi, argc) => zal_rt_call(ctx, pc, fi, argc as u32),
                Op::Ret => zal_rt_ret(ctx),
                Op::LenLocal(a) => {
                    let n = zal_rt_len(c.stack.add(c.base as usize + a as usize));
                    if n >= 0 {
                        c.stack.add(sp).write(Value::Int(n));
                        c.sp += 1;
                        ZAL_NEXT
                    } else {
                        zal_rt_op(ctx, pc)
                    }
                }
                Op::IndexLL(a, b) if tag(c.stack.add(c.base as usize + b as usize)) == TAG_INT => {
                    let i = int_at(c.stack.add(c.base as usize + b as usize));
                    if zal_rt_index(c.stack.add(sp), c.stack.add(c.base as usize + a as usize), i) == 0 {
                        c.sp += 1;
                        ZAL_NEXT
                    } else {
                        zal_rt_op(ctx, pc)
                    }
                }
                Op::Index if tag(c.stack.add(sp - 1)) == TAG_INT => {
                    // Result replaces the list slot (dst aliases the list: consumed).
                    let i = int_at(c.stack.add(sp - 1));
                    if zal_rt_index(c.stack.add(sp - 2), c.stack.add(sp - 2), i) == 0 {
                        c.sp -= 1;
                        ZAL_NEXT
                    } else {
                        zal_rt_op(ctx, pc)
                    }
                }
                Op::ForNext(b, exit) => match zal_rt_for_next(c.stack.add(c.base as usize + b as usize)) {
                    0 => ZAL_NEXT,
                    1 => {
                        let r = branch(ctx, pc, exit);
                        if r != 0 {
                            return r;
                        }
                        pc = exit;
                        continue;
                    }
                    _ => zal_rt_op(ctx, pc),
                },
                Op::UpdLocal(b, a) => {
                    let (code, argc) = match b {
                        Builtin::Push => (ZAL_BUILTIN_PUSH, 1),
                        Builtin::Remove => (ZAL_BUILTIN_REMOVE, 1),
                        Builtin::Set => (ZAL_BUILTIN_SET, 2),
                        Builtin::MapSet => (ZAL_BUILTIN_MAP_SET, 2),
                        _ => (u32::MAX, 0),
                    };
                    if argc > 0 && zal_rt_upd(c.stack.add(c.base as usize + a as usize), c.stack.add(sp - argc), code) == 0 {
                        c.sp -= argc as u64;
                        ZAL_NEXT
                    } else {
                        zal_rt_op(ctx, pc)
                    }
                }
                Op::JumpIfNotTag(t, target) => {
                    let tg = zal_rt_tag(c.stack.add(sp - 1));
                    if tg == 0xFFFF {
                        zal_rt_op(ctx, pc)
                    } else {
                        c.sp -= 1;
                        zal_rt_drop(c.stack.add(sp - 1));
                        if tg != t as u32 {
                            let r = branch(ctx, pc, target);
                            if r != 0 {
                                return r;
                            }
                            pc = target;
                            continue;
                        }
                        ZAL_NEXT
                    }
                }
                Op::LoadPayload(a) => {
                    if zal_rt_payload(c.stack.add(sp), c.stack.add(c.base as usize + a as usize)) == 0 {
                        c.sp += 1;
                        ZAL_NEXT
                    } else {
                        zal_rt_op(ctx, pc)
                    }
                }
                Op::LoadField(a, f) => {
                    if zal_rt_field(c.stack.add(sp), c.stack.add(c.base as usize + a as usize), f as u32) == 0 {
                        c.sp += 1;
                        ZAL_NEXT
                    } else {
                        zal_rt_op(ctx, pc)
                    }
                }
                Op::MapGetJump(r, t) => match zal_rt_map_get_jump(ctx, r as u32) {
                    0 => ZAL_NEXT,
                    1 => {
                        let r = branch(ctx, pc, t);
                        if r != 0 {
                            return r;
                        }
                        pc = t;
                        continue;
                    }
                    _ => zal_rt_op(ctx, pc),
                },
                Op::MakeList(n) | Op::MakeTuple(n) | Op::MakeRecord(n) => {
                    let kind = match code[pc as usize] {
                        Op::MakeList(_) => ZAL_MAKE_LIST,
                        Op::MakeTuple(_) => ZAL_MAKE_TUPLE,
                        _ => ZAL_MAKE_RECORD,
                    };
                    zal_rt_make(ctx, kind, n as u32)
                }
                Op::MakeVariant(tag, has) => zal_rt_make(ctx, ZAL_MAKE_VARIANT, tag as u32 | ((has as u32) << 16)),
                Op::Builtin(b, argc) => match zal_rt_builtin(ctx, b as u32, argc as u32) {
                    0 => ZAL_NEXT,
                    _ => zal_rt_op(ctx, pc),
                },
                Op::ConcatN(n) => match zal_rt_concat(ctx, n as u32) {
                    0 => ZAL_NEXT,
                    _ => zal_rt_op(ctx, pc),
                },
                _ => zal_rt_op(ctx, pc),
            };
            match r {
                ZAL_NEXT => pc += 1,
                ZAL_JUMP => pc = (*ctx).pc,
                exit => return exit,
            }
        }
    }
}
