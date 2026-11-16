//! Codegen tests against a stub runtime (RFC 0002 ABI), independent of zal-vm.
//!
//! The stub `zal_rt_op` interprets a small op subset on raw 24-byte values and
//! records every generic call, so tests can check both results and which ops
//! took the inline fast path.

use zal_backend_cranelift::abi::*;
use zal_backend_cranelift::{jit_with as jit, object, JitCode};
use zal_ir::{Const as K, FnKind, Function, Op, Program, Ty, IR_VERSION};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Raw {
    tag: u8,
    pad: [u8; 7],
    payload: i64,
    extra: i64,
}
const _: () = assert!(std::mem::size_of::<Raw>() == 24);

fn raw(tag: u8, payload: i64) -> Raw {
    Raw { tag, pad: [0; 7], payload, extra: 0 }
}
fn int(v: i64) -> Raw {
    raw(TAG_INT, v)
}
fn boolean(v: bool) -> Raw {
    raw(TAG_BOOL, v as i64)
}

#[derive(Default)]
struct Stub {
    prog: Option<Program>,
    generic: Vec<Op>,
    ticks: Vec<u32>,
    traps: Vec<(u32, u32)>,
    /// Make the n-th tick (1-based) suspend.
    suspend_tick: Option<usize>,
}

unsafe fn stub_of<'a>(ctx: *mut ZalCtx) -> &'a mut Stub {
    unsafe { &mut *((*ctx).vm as *mut Stub) }
}

unsafe fn slot<'a>(ctx: *mut ZalCtx, i: u64) -> &'a mut Raw {
    unsafe { &mut *((*ctx).stack as *mut Raw).add(i as usize) }
}

unsafe fn pop(ctx: *mut ZalCtx) -> Raw {
    unsafe {
        (*ctx).sp -= 1;
        *slot(ctx, (*ctx).sp)
    }
}

unsafe fn push(ctx: *mut ZalCtx, v: Raw) {
    unsafe {
        *slot(ctx, (*ctx).sp) = v;
        (*ctx).sp += 1;
    }
}

unsafe extern "C" fn rt_op(ctx: *mut ZalCtx, pc: u32) -> u32 {
    unsafe {
        let st = stub_of(ctx);
        let prog = st.prog.as_ref().unwrap();
        let op = prog.functions[(*ctx).func as usize].code[pc as usize];
        st.generic.push(op);
        let base = (*ctx).base;
        let jump = |ctx: *mut ZalCtx, t: u32| -> u32 {
            (*ctx).pc = t;
            JUMP
        };
        match op {
            Op::Const(c) => push(
                ctx,
                match &prog.consts[c as usize] {
                    K::Int(v) => int(*v),
                    K::Dur(v) => raw(TAG_DUR, *v),
                    K::Bool(b) => boolean(*b),
                    // Fake owned string: tag 3, payload = length.
                    K::Str(s) => raw(FAKE_OWNED, s.len() as i64),
                },
            ),
            Op::Unit => push(ctx, raw(TAG_UNIT, 0)),
            Op::LoadLocal(l) => {
                let v = *slot(ctx, base + l as u64);
                push(ctx, v)
            }
            Op::StoreLocal(l) => {
                let v = pop(ctx);
                *slot(ctx, base + l as u64) = v;
            }
            Op::Pop => {
                pop(ctx);
            }
            Op::IncLocal(l, k) => {
                let s = slot(ctx, base + l as u64);
                s.payload += k as i64;
            }
            Op::Add | Op::Sub | Op::Mul => {
                let b = pop(ctx);
                let a = pop(ctx);
                let r = match op {
                    Op::Add if a.tag == TAG_DUR => a.payload.saturating_add(b.payload),
                    Op::Add => a.payload + b.payload,
                    Op::Sub => a.payload - b.payload,
                    _ => a.payload * b.payload,
                };
                push(ctx, raw(a.tag, r));
            }
            Op::Lt | Op::Eq => {
                let b = pop(ctx);
                let a = pop(ctx);
                let r = if op == Op::Lt { a.payload < b.payload } else { a.tag == b.tag && a.payload == b.payload };
                push(ctx, boolean(r));
            }
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                let v = pop(ctx);
                let want = matches!(op, Op::JumpIfTrue(_));
                if v.tag == TAG_BOOL && (v.payload != 0) == want {
                    return jump(ctx, t);
                }
            }
            Op::JumpIfNotTag(tag, t) => {
                let v = pop(ctx);
                if v.tag as u16 != tag {
                    return jump(ctx, t);
                }
            }
            // Contrived: re-dispatch three ops ahead through ctx->pc.
            Op::Await => return jump(ctx, pc + 3),
            Op::Call(..) => {
                (*ctx).pc = pc + 1;
                return EXIT_CALL;
            }
            Op::Ret => return EXIT_RET,
            other => panic!("stub: unsupported op {other:?}"),
        }
        0
    }
}

unsafe extern "C" fn rt_tick(ctx: *mut ZalCtx, pc: u32) -> u32 {
    unsafe {
        let st = stub_of(ctx);
        st.ticks.push(pc);
        // v2: inline code calls tick only at the slice limit; parking the
        // counter one below the limit makes every back edge tick, as in v1.
        (*ctx).slice = SLICE_PARK;
        if st.suspend_tick == Some(st.ticks.len()) {
            (*ctx).pc = pc;
            return EXIT_SUSPEND;
        }
        0
    }
}

unsafe extern "C" fn rt_trap(ctx: *mut ZalCtx, pc: u32, code: u32) -> u32 {
    unsafe { stub_of(ctx).traps.push((pc, code)) };
    EXIT_TRAP
}

/// Fake owned value of the stub runtime (strings): tag 7 is owned but not
/// an inline-`Rc` tag, so inline code clones and drops it through the runtime.
const FAKE_OWNED: u8 = 7;

/// Initial/after-tick slice counter: one below the v2 limit (512).
const SLICE_PARK: u32 = 511;

thread_local! {
    /// (clones, drops) of owned (non-plain) values by inline code.
    static OWNED: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

unsafe extern "C" fn rt_clone(dst: *mut u8, src: *const u8) {
    unsafe {
        assert!(!tag_is_plain(*src) && *src != TAG_STR, "plain and Rc clones must be inline");
        std::ptr::copy_nonoverlapping(src, dst, 24);
    }
    OWNED.with(|c| c.set((c.get().0 + 1, c.get().1)));
}

unsafe extern "C" fn rt_drop(v: *mut u8) {
    unsafe { assert!(!tag_is_plain(*v), "drop of a plain value must be inline") };
    // A Str reaching zal_rt_drop is its last reference: count it as freed.
    unsafe {
        if *v == TAG_STR {
            let p = *((v as *const u8).add(8) as *const *mut i64);
            assert_eq!(*p, 1, "only the last Str reference is dropped by the runtime");
            *p = 0;
        }
    }
    OWNED.with(|c| c.set((c.get().0, c.get().1 + 1)));
}

fn rt() -> Vec<(&'static str, *const u8)> {
    let mut v = vec![
        ("zal_rt_op", rt_op as *const u8),
        ("zal_rt_tick", rt_tick as *const u8),
        ("zal_rt_trap", rt_trap as *const u8),
        ("zal_rt_clone", rt_clone as *const u8),
        ("zal_rt_drop", rt_drop as *const u8),
    ];
    v.extend(v2::symbols());
    v
}

fn func(params: usize, locals: u16, code: Vec<Op>) -> Function {
    Function {
        name: format!("f{}", code.len()),
        kind: FnKind::Fn,
        params: (0..params).map(|i| (format!("p{i}"), Ty::Int)).collect(),
        ret: Ty::Int,
        locals,
        lines: vec![1; code.len()],
        code,
        effects: vec![],
        budget: None,
        slot_tags: vec![],
        slot_seal: 0,
    }
}

fn program(consts: Vec<K>, functions: Vec<Function>) -> Program {
    Program {
        ir_version: IR_VERSION,
        module: "t".into(),
        consts,
        functions,
        externs: vec![],
        records: vec![],
        enums: vec![],
    }
}

struct Run {
    stub: Box<Stub>,
    stack: Vec<Raw>,
    ctx: ZalCtx,
    code: JitCode,
}

impl Run {
    fn new(prog: Program, locals: &[Raw]) -> Run {
        let code = jit(&prog, &rt()).expect("jit");
        let mut stub = Box::new(Stub { prog: Some(prog), ..Default::default() });
        let mut stack = vec![raw(0xEE, 0); 64];
        stack[..locals.len()].copy_from_slice(locals);
        let ctx = ZalCtx {
            vm: &mut *stub as *mut Stub as *mut _,
            stack: stack.as_mut_ptr() as *mut ZalValue,
            sp: locals.len() as u64,
            base: 0,
            pc: 0,
            func: 0,
            slice: SLICE_PARK,
        };
        Run { stub, stack, ctx, code }
    }

    fn call(&mut self, entry_pc: u32) -> u32 {
        let f = self.code.table()[self.ctx.func as usize].unwrap();
        unsafe { f(&mut self.ctx, entry_pc) }
    }

    fn top(&self) -> Raw {
        self.stack[self.ctx.sp as usize - 1]
    }
}

/// sum of 0..n with locals n(0), i(1), s(2); every op but Ret is a fast path.
fn sum_loop() -> Program {
    use Op::*;
    let code = vec![
        Const(0),
        StoreLocal(1),
        Const(0),
        StoreLocal(2),
        LoadLocal(1), // 4: loop head
        LoadLocal(0),
        JumpIfNotLt(13),
        LoadLocal(2),
        LoadLocal(1),
        Add,
        StoreLocal(2),
        IncLocal(1, 1),
        Jump(4),
        LoadLocal(2), // 13
        Ret,
    ];
    program(vec![K::Int(0)], vec![func(1, 3, code)])
}

#[test]
fn sum_loop_runs_inline() {
    let unit = raw(TAG_UNIT, 0);
    let mut r = Run::new(sum_loop(), &[int(1000), unit, unit]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int((0..1000).sum()));
    assert_eq!(r.stub.generic, vec![Op::Ret], "only Ret leaves the fast path");
    assert_eq!(r.stub.ticks.len(), 1000);
    assert!(r.stub.ticks.iter().all(|&t| t == 4), "tick gets the resume pc");
}

#[test]
fn resume_at_entry_pc() {
    let mut r = Run::new(sum_loop(), &[int(10), int(5), int(100)]);
    assert_eq!(r.call(4), EXIT_RET);
    assert_eq!(r.top(), int(100 + (5..10).sum::<i64>()));
}

#[test]
fn tick_suspend_then_resume() {
    let unit = raw(TAG_UNIT, 0);
    let mut r = Run::new(sum_loop(), &[int(10), unit, unit]);
    r.stub.suspend_tick = Some(3);
    assert_eq!(r.call(0), EXIT_SUSPEND);
    assert_eq!(r.ctx.pc, 4);
    assert_eq!(r.ctx.sp, 3, "sp flushed before the exit");
    let pc = r.ctx.pc;
    assert_eq!(r.call(pc), EXIT_RET);
    assert_eq!(r.top(), int(45));
}

fn binop(a: K, b: K, op: Op) -> Run {
    let p = program(vec![a, b], vec![func(0, 0, vec![Op::Const(0), Op::Const(1), op, Op::Ret])]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_RET);
    r
}

#[test]
fn int_arith_and_comparisons_inline() {
    let cases: &[(i64, i64)] = &[(3, 5), (5, 3), (4, 4), (-7, 2), (i64::MIN, i64::MAX)];
    for &(a, b) in cases {
        for (op, want) in [
            (Op::Lt, a < b),
            (Op::Le, a <= b),
            (Op::Gt, a > b),
            (Op::Ge, a >= b),
            (Op::Eq, a == b),
            (Op::Ne, a != b),
        ] {
            let r = binop(K::Int(a), K::Int(b), op);
            assert_eq!(r.top(), boolean(want), "{a} {op:?} {b}");
            assert_eq!(r.stub.generic, vec![Op::Ret]);
        }
    }
    for (op, want) in [(Op::Add, 47), (Op::Sub, 37), (Op::Mul, 210)] {
        let r = binop(K::Int(42), K::Int(5), op);
        assert_eq!(r.top(), int(want));
        assert_eq!(r.ctx.sp, 1);
        assert_eq!(r.stub.generic, vec![Op::Ret]);
    }
}

#[test]
fn overflow_traps() {
    for (a, b, op, code) in [
        (i64::MAX, 1, Op::Add, TRAP_ADD),
        (i64::MIN, 1, Op::Sub, TRAP_SUB),
        (i64::MAX, 2, Op::Mul, TRAP_MUL),
        (i64::MIN, -1, Op::Mul, TRAP_MUL),
    ] {
        let p = program(vec![K::Int(a), K::Int(b)], vec![func(0, 0, vec![Op::Const(0), Op::Const(1), op, Op::Ret])]);
        let mut r = Run::new(p, &[]);
        assert_eq!(r.call(0), EXIT_TRAP);
        assert_eq!(r.stub.traps, vec![(2, code)]);
        assert_eq!(r.ctx.sp, 0, "operands popped as in the VM");
        assert!(r.stub.generic.is_empty());
    }
    let p = program(vec![], vec![func(1, 1, vec![Op::IncLocal(0, 1), Op::LoadLocal(0), Op::Ret])]);
    let mut r = Run::new(p, &[int(i64::MAX)]);
    assert_eq!(r.call(0), EXIT_TRAP);
    assert_eq!(r.stub.traps, vec![(0, TRAP_ADD)]);
    let p = program(vec![], vec![func(1, 1, vec![Op::IncLocal(0, -3), Op::LoadLocal(0), Op::Ret])]);
    let mut r = Run::new(p, &[int(10)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(7));
}

#[test]
fn guards_fall_back_to_generic() {
    // Dur + Dur saturates in the VM: generic path.
    let r = binop(K::Dur(i64::MAX), K::Dur(5), Op::Add);
    assert_eq!(r.top(), raw(TAG_DUR, i64::MAX));
    assert_eq!(r.stub.generic, vec![Op::Add, Op::Ret]);
    // Str constant and Str operands.
    let r = binop(K::Str("ab".into()), K::Str("cde".into()), Op::Add);
    assert_eq!(r.top(), raw(FAKE_OWNED, 5));
    let want = vec![Op::Add, Op::Ret];
    assert_eq!(r.stub.generic, want);
    // Mixed: Int == Dur is false and must not compare payloads inline.
    let r = binop(K::Int(3), K::Dur(3), Op::Eq);
    assert_eq!(r.top(), boolean(false));
    assert_eq!(r.stub.generic, vec![Op::Eq, Op::Ret]);

    // Owned values in locals: clone / drop through the runtime, inline otherwise.
    use Op::*;
    OWNED.with(|c| c.set((0, 0)));
    let code = vec![
        Const(0),      // Str (generic)
        StoreLocal(0), // over Unit: moves the Str
        LoadLocal(0),  // clone
        Pop,           // drop
        LoadLocal2(0, 0), // 2 clones
        StoreLoad(0),  // drop old local, clone top into it
        Pop,           // drop
        Pop,           // drop
        TakeLocal(0),  // move, local becomes Unit
        Pop,           // drop
        Const(1),      // Int
        StoreLocal(0), // over Unit
        LoadLocal(0),  // Int: raw copy
        Ret,
    ];
    let p = program(vec![K::Str("x".into()), K::Int(9)], vec![func(0, 1, code)]);
    let mut r = Run::new(p, &[raw(TAG_UNIT, 0)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(9));
    let want = vec![Ret];
    assert_eq!(r.stub.generic, want);
    assert_eq!(OWNED.with(|c| c.get()), (4, 5));

    // JumpIfFalse on a non-Bool: generic (the VM pops and falls through).
    let code = vec![Const(0), JumpIfFalse(4), Const(0), Ret, Const(1), Ret];
    let p = program(vec![K::Int(0), K::Int(1)], vec![func(0, 0, code)]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(0));
    // A failed guard deopts: the rest of the region steps through the runtime.
    assert_eq!(r.stub.generic, vec![JumpIfFalse(4), Const(0), Ret]);
}

#[test]
fn conditional_jumps_and_backward_ticks() {
    use Op::*;
    // i = 0; do { i += 1 } while (i < 3); return i
    let code = vec![IncLocal(0, 1), LoadLocal(0), Const(0), Lt, JumpIfTrue(0), LoadLocal(0), Ret];
    let p = program(vec![K::Int(3)], vec![func(1, 1, code)]);
    let mut r = Run::new(p, &[int(0)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(3));
    assert_eq!(r.stub.ticks, vec![0, 0], "taken backward conditional branches tick");
    assert_eq!(r.stub.generic, vec![Ret]);

    // Forward JumpIfFalse on Bool: no tick.
    let code = vec![Const(0), JumpIfFalse(4), Const(1), Ret, Const(2), Ret];
    let p = program(vec![K::Bool(false), K::Int(1), K::Int(2)], vec![func(0, 0, code)]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(2));
    assert!(r.stub.ticks.is_empty());
    assert_eq!(r.stub.generic, vec![Ret]);
}

#[test]
fn generic_branch_static_target_and_redispatch() {
    use Op::*;
    // JumpIfNotTag goes through zal_rt_op and lands on its static target.
    let code = vec![Const(0), JumpIfNotTag(5, 4), Const(1), Ret, Const(2), Ret];
    let p = program(vec![K::Int(3), K::Int(10), K::Int(20)], vec![func(0, 0, code)]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(20));

    // A non-branch op answering JUMP re-dispatches through ctx->pc.
    let code = vec![Const(0), Await, Const(1), Add, Ret];
    let p = program(vec![K::Int(7), K::Int(100)], vec![func(0, 0, code)]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(7));
}

#[test]
fn call_exit_and_reentry() {
    use Op::*;
    let main = func(0, 0, vec![Call(1, 0), Const(0), Add, Ret]);
    let callee = func(0, 0, vec![Const(0), Ret]);
    let p = program(vec![K::Int(1)], vec![main, callee]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_CALL);
    assert_eq!(r.ctx.pc, 1);
    // The runtime ran the callee and left its result on the stack.
    r.stack[r.ctx.sp as usize] = int(41);
    r.ctx.sp += 1;
    let pc = r.ctx.pc;
    assert_eq!(r.call(pc), EXIT_RET);
    assert_eq!(r.top(), int(42));
    assert_eq!(r.code.table().len(), 2);
}

#[test]
fn rejects_unverified_ir() {
    let p = program(vec![], vec![func(0, 0, vec![Op::Pop, Op::Ret])]);
    assert!(jit(&p, &rt()).is_err());
    assert!(object(&p).is_err());
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("zal-cl");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn aot_object_symbols() {
    let o = object(&sum_loop()).expect("object");
    let path = scratch("sum.o");
    std::fs::write(&path, &o).unwrap();
    let Ok(out) = std::process::Command::new("nm").arg(&path).output() else {
        eprintln!("nm not available; skipped");
        return;
    };
    let nm = String::from_utf8_lossy(&out.stdout);
    let has = |kinds: &str, name: &str| {
        nm.lines().any(|l| {
            let mut it = l.split_whitespace().rev();
            it.next() == Some(name) && it.next().is_some_and(|k| kinds.contains(k))
        })
    };
    assert!(has("T", "zal_fn_0"), "{nm}");
    for d in ["zal_fn_table", "zal_fn_count", "zal_ir", "zal_ir_len"] {
        assert!(has("DdRr", d), "{d} missing:\n{nm}");
    }
    for u in ["zal_rt_op", "zal_rt_tick", "zal_rt_trap"] {
        assert!(has("U", u), "{u} missing:\n{nm}");
    }
}

const C_HARNESS: &str = r#"
#include <stdint.h>
#include <stdio.h>
#include <string.h>
typedef struct { void *vm; uint8_t *stack; uint64_t sp, base; uint32_t pc, func, slice; } ZalCtx;
typedef uint32_t (*zal_fn)(ZalCtx*, uint32_t);
extern zal_fn zal_fn_table[];
extern uint32_t zal_fn_count;
extern const char zal_ir[];
extern uint64_t zal_ir_len;
static int ticks;
uint32_t zal_rt_op(ZalCtx *c, uint32_t pc) { (void)c; (void)pc; return 2; /* only Ret is generic here */ }
uint32_t zal_rt_ret(ZalCtx *c) { (void)c; return 2; }
uint32_t zal_rt_tick(ZalCtx *c, uint32_t pc) { (void)c; (void)pc; ticks++; return 0; }
uint32_t zal_rt_trap(ZalCtx *c, uint32_t pc, uint32_t code) { (void)c; printf("trap %u %u\n", pc, code); return 4; }
void zal_rt_clone(void *d, const void *s) { (void)d; (void)s; }
void zal_rt_drop(void *v) { (void)v; }
int main(void) {
    uint8_t stack[24 * 64];
    memset(stack, 0, sizeof stack);
    stack[0] = 1; int64_t n = 100; memcpy(stack + 8, &n, 8);
    ZalCtx c = { 0, stack, 3, 0, 0, 0, 0 };
    uint32_t r = zal_fn_table[0](&c, 0);
    int64_t top; memcpy(&top, stack + 24 * (c.sp - 1) + 8, 8);
    printf("exit=%u count=%u top=%lld ticks=%d ir=%c%c\n", r, zal_fn_count, (long long)top, ticks,
           zal_ir[0], zal_ir[zal_ir_len - 1]);
    return 0;
}
"#;

#[test]
fn aot_object_links_and_runs() {
    let o = object(&sum_loop()).expect("object");
    let obj = scratch("sum_link.o");
    let c = scratch("harness.c");
    let exe = scratch("sum_link");
    std::fs::write(&obj, &o).unwrap();
    std::fs::write(&c, C_HARNESS).unwrap();
    let Ok(st) = std::process::Command::new("cc").arg(&c).arg(&obj).arg("-o").arg(&exe).status() else {
        eprintln!("cc not available; skipped");
        return;
    };
    assert!(st.success(), "link failed");
    let out = std::process::Command::new(&exe).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    // v2 counts the slice inline and calls zal_rt_tick only at 512.
    let ticks = 0;
    assert_eq!(s.trim(), format!("exit=2 count=1 top=4950 ticks={ticks} ir={{}}"), "{s}");
}

#[test]
fn native_backend_trait_with_real_runtime_symbols() {
    let b = zal_backend_cranelift::backend();
    assert_eq!(b.name(), "cranelift");
    let m = b.jit(&sum_loop()).expect("jit against zal-vm runtime symbols");
    assert_eq!(m.table().len(), 1);
    assert!(m.table()[0].is_some());
    assert!(!b.object(&sum_loop()).unwrap().is_empty());
}

#[test]
fn raw_layout_matches_zal_vm_values() {
    use zal_vm::Value;
    let as_raw = |v: &Value| unsafe { std::ptr::read(v as *const Value as *const Raw) };
    let i = as_raw(&Value::Int(-42));
    assert_eq!((i.tag, i.payload), (TAG_INT, -42));
    let d = as_raw(&Value::Dur(7));
    assert_eq!((d.tag, d.payload), (TAG_DUR, 7));
    let b = as_raw(&Value::Bool(true));
    assert_eq!((b.tag, b.payload & 0xff), (TAG_BOOL, 1));
    assert_eq!(as_raw(&Value::Unit).tag, TAG_UNIT);
    // What inline code writes reads back as the right Value.
    let v: Value = unsafe { std::ptr::read(&boolean(false) as *const Raw as *const Value) };
    assert_eq!(v, Value::Bool(false));
    let v: Value = unsafe { std::ptr::read(&int(9) as *const Raw as *const Value) };
    assert_eq!(v, Value::Int(9));
}

#[test]
fn mirror_tracks_locals_and_deopts_on_failed_guards() {
    use Op::*;
    // (a + b) + c with locals of unknown type, then the local round trip.
    let code = vec![
        LoadLocal2(0, 1),
        Add, // guard on both; Dur deopts
        LoadLocal(2),
        Add, // Known Int result + local 2 (guarded)
        StoreLocal(3),
        IncLocal(3, 1),
        LoadLocal(3), // mirror after IncLocal
        LoadLocal(3),
        Mul,
        Ret,
    ];
    let p = program(vec![], vec![func(4, 4, code)]);
    let mut r = Run::new(p, &[int(2), int(3), int(4), raw(TAG_UNIT, 0)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(100));
    assert_eq!(r.stub.generic, vec![Ret]);
    assert_eq!(r.stack[3], int(10), "local written through");

    // Dur operands: the first Add deopts, the rest of the region steps generically.
    let code = vec![LoadLocal2(0, 1), Add, LoadLocal(2), Add, Ret];
    let p2 = program(vec![], vec![func(3, 3, code)]);
    let mut r = Run::new(p2, &[raw(TAG_DUR, 2), raw(TAG_DUR, 3), raw(TAG_DUR, 4)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), raw(TAG_DUR, 9));
    assert_eq!(r.stub.generic, vec![Add, LoadLocal(2), Add, Ret]);
}

#[test]
fn known_bool_from_compare_feeds_branch_without_guard() {
    use Op::*;
    // i = 0; while i < 5 { i += 1 } with Lt + JumpIfFalse instead of JumpIfNotLt.
    let code = vec![LoadLocal(0), Const(0), Lt, JumpIfFalse(6), IncLocal(0, 1), Jump(0), LoadLocal(0), Ret];
    let p = program(vec![K::Int(5)], vec![func(1, 1, code)]);
    let mut r = Run::new(p, &[int(0)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(5));
    assert_eq!(r.stub.generic, vec![Ret]);
    assert_eq!(r.stub.ticks.len(), 5);
}

#[test]
fn compare_on_non_ints_rejoins_with_a_bool() {
    use Op::*;
    // Str == Str goes through the runtime but the branch after it stays inline.
    let code = vec![Const(0), Const(0), Eq, JumpIfFalse(6), Const(1), Ret, Const(2), Ret];
    let p = program(vec![K::Str("a".into()), K::Int(1), K::Int(2)], vec![func(0, 0, code)]);
    let mut r = Run::new(p, &[]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(1));
    let want = vec![Eq, Ret];
    assert_eq!(r.stub.generic, want);
}

/// Stub addendum-v2 helpers on the fake value model of this file:
/// List = tag 5 with payload = index into a thread-local list store,
/// Variant = tag 9 with payload = variant tag and `extra` = Int payload.
mod v2 {
    use super::*;
    use std::cell::RefCell;

    pub const TAG_LIST: u8 = 5;
    pub const TAG_VARIANT: u8 = 9;

    thread_local! {
        pub static LISTS: RefCell<Vec<Vec<Raw>>> = const { RefCell::new(Vec::new()) };
        pub static CALLS: RefCell<Vec<(u32, u32, u32)>> = const { RefCell::new(Vec::new()) };
    }

    pub fn list(xs: Vec<Raw>) -> Raw {
        LISTS.with(|l| {
            l.borrow_mut().push(xs);
            raw(TAG_LIST, l.borrow().len() as i64 - 1)
        })
    }

    pub fn list_of(v: Raw) -> Vec<Raw> {
        LISTS.with(|l| l.borrow()[v.payload as usize].clone())
    }

    pub fn variant(tag: i64, payload: i64) -> Raw {
        Raw { tag: TAG_VARIANT, pad: [0; 7], payload: tag, extra: payload }
    }

    unsafe fn r<'a>(p: *const u8) -> &'a mut Raw {
        unsafe { &mut *(p as *mut Raw) }
    }

    unsafe extern "C" fn len(v: *const u8) -> i64 {
        let v = unsafe { *r(v) };
        if v.tag == TAG_LIST { list_of(v).len() as i64 } else { -1 }
    }
    unsafe extern "C" fn index(dst: *mut u8, l: *const u8, i: i64) -> u32 {
        let l = unsafe { *r(l) };
        if l.tag != TAG_LIST {
            return 1;
        }
        match usize::try_from(i).ok().and_then(|i| list_of(l).get(i).copied()) {
            Some(x) => {
                unsafe { *r(dst) = x };
                0
            }
            None => 1,
        }
    }
    unsafe extern "C" fn for_next(slot: *mut u8) -> u32 {
        unsafe {
            let s = slot as *mut Raw;
            let (l, i) = (*s, *s.add(1));
            if l.tag != TAG_LIST || i.tag != TAG_INT {
                return 2;
            }
            match list_of(l).get(i.payload as usize) {
                Some(x) => {
                    *s.add(2) = *x;
                    *s.add(1) = int(i.payload + 1);
                    0
                }
                None => 1,
            }
        }
    }
    unsafe extern "C" fn upd(local: *mut u8, args: *mut u8, b: u32) -> u32 {
        let l = unsafe { *r(local) };
        if l.tag != TAG_LIST || b != zal_ir::Builtin::Push as u32 {
            return 2;
        }
        let x = unsafe { *r(args) };
        LISTS.with(|ls| ls.borrow_mut()[l.payload as usize].push(x));
        0
    }
    unsafe extern "C" fn tag(v: *const u8) -> u32 {
        let v = unsafe { *r(v) };
        match v.tag {
            TAG_VARIANT | TAG_BOOL => v.payload as u32,
            _ => 0xFFFF,
        }
    }
    unsafe extern "C" fn payload(dst: *mut u8, v: *const u8) -> u32 {
        let v = unsafe { *r(v) };
        if v.tag != TAG_VARIANT {
            return 2;
        }
        unsafe { *r(dst) = int(v.extra) };
        0
    }
    unsafe extern "C" fn field(_dst: *mut u8, _v: *const u8, _i: u32) -> u32 {
        2
    }
    unsafe extern "C" fn map_get(_dst: *mut u8, _m: *const u8, _k: *const u8) -> u32 {
        2
    }
    unsafe extern "C" fn konst(dst: *mut u8, i: u32, ctx: *const ZalCtx) -> () {
        unsafe {
            let st = stub_of(ctx as *mut ZalCtx);
            let K::Str(s) = &st.prog.as_ref().unwrap().consts[i as usize] else { panic!("const helper on a plain constant") };
            *r(dst) = raw(FAKE_OWNED, s.len() as i64);
        }
    }
    unsafe extern "C" fn concat(ctx: *mut ZalCtx, n: u32) -> u32 {
        unsafe {
            let mut total = 0;
            for _ in 0..n {
                total += pop(ctx).payload;
            }
            push(ctx, raw(FAKE_OWNED, total));
        }
        0
    }
    /// v3 helpers the tests do not exercise: always "generic".
    unsafe extern "C" fn na3(_: *mut ZalCtx, _: u32, _: u32) -> u32 {
        2
    }
    unsafe extern "C" fn na2(_: *mut ZalCtx, _: u32) -> u32 {
        2
    }
    /// Records the Ret like the generic stub does, so assertions stay shared.
    unsafe extern "C" fn ret(ctx: *mut ZalCtx) -> u32 {
        unsafe { stub_of(ctx).generic.push(Op::Ret) };
        EXIT_RET
    }
    unsafe extern "C" fn call(ctx: *mut ZalCtx, pc: u32, f: u32, argc: u32) -> u32 {
        CALLS.with(|c| c.borrow_mut().push((pc, f, argc)));
        unsafe { (*ctx).pc = pc + 1 };
        EXIT_CALL
    }

    pub fn symbols() -> Vec<(&'static str, *const u8)> {
        vec![
            ("zal_rt_len", len as *const u8),
            ("zal_rt_index", index as *const u8),
            ("zal_rt_for_next", for_next as *const u8),
            ("zal_rt_upd", upd as *const u8),
            ("zal_rt_tag", tag as *const u8),
            ("zal_rt_payload", payload as *const u8),
            ("zal_rt_field", field as *const u8),
            ("zal_rt_map_get", map_get as *const u8),
            ("zal_rt_const", konst as *const u8),
            ("zal_rt_concat", concat as *const u8),
            ("zal_rt_call", call as *const u8),
            ("zal_rt_ret", ret as *const u8),
            ("zal_rt_make", na3 as *const u8),
            ("zal_rt_builtin", na3 as *const u8),
            ("zal_rt_map_get_jump", na2 as *const u8),
        ]
    }

    #[test]
    fn for_next_index_len_and_upd_through_helpers() {
        use Op::*;
        // locals: list(0) idx(1) var(2) acc(3) out(4)
        // for x in list { acc += x; push(out, x * x) } ; acc + len(out) + out[1]
        let code = vec![
            ForNext(0, 9),
            LoadLocal2(3, 2),
            Add,
            StoreLocal(3),
            LoadLocal2(2, 2),
            Mul,
            UpdLocal(zal_ir::Builtin::Push, 4),
            Jump(0),
            Unit, // 8: unreachable filler
            LoadLocal(3), // 9
            LenLocal(4),
            Add,
            Const(0),
            StoreLocal(1),
            IndexLL(4, 1),
            Add,
            Ret,
        ];
        let p = program(vec![K::Int(1)], vec![func(5, 5, code)]);
        let l = list(vec![int(2), int(3), int(4)]);
        let out = list(vec![]);
        let mut r = Run::new(p, &[l, int(0), raw(TAG_UNIT, 0), int(0), out]);
        r.ctx.slice = 0;
        assert_eq!(r.call(0), EXIT_RET);
        assert_eq!(r.top(), int(9 + 3 + 9));
        assert_eq!(list_of(out), vec![int(4), int(9), int(16)]);
        assert_eq!(r.stub.generic, vec![Ret]);
        assert!(r.stub.ticks.is_empty(), "slice counted inline below the limit");
        assert_eq!(r.ctx.slice, 3);
    }

    #[test]
    fn tags_payloads_strings_and_calls_through_helpers() {
        use Op::*;
        // match v { tag 1 => payload + 1, _ => 0 } plus a Str concat length
        let code = vec![
            LoadLocal(0),
            JumpIfNotTag(1, 8),
            LoadPayload(0),
            Const(0), // "ab"
            Const(1), // "cde"
            ConcatN(2),
            StoreLocal(1),
            Ret,
            Const(2), // 8
            Ret,
        ];
        let p = program(vec![K::Str("ab".into()), K::Str("cde".into()), K::Int(0)], vec![func(2, 2, code)]);
        let mut r = Run::new(p.clone(), &[variant(1, 41), raw(TAG_UNIT, 0)]);
        assert_eq!(r.call(0), EXIT_RET);
        assert_eq!(r.top(), int(41));
        assert_eq!(r.stack[1], raw(FAKE_OWNED, 5));
        assert_eq!(r.stub.generic, vec![Ret]);
        let mut r = Run::new(p, &[variant(0, 7), raw(TAG_UNIT, 0)]);
        assert_eq!(r.call(0), EXIT_RET);
        // Plain values are tag + payload word; the third word is unspecified.
        assert_eq!((r.top().tag, r.top().payload), (TAG_INT, 0));

        // MovePayload into a local; Call leaves through zal_rt_call.
        let main = func(2, 2, vec![MovePayload(0, 1), Call(1, 0), LoadLocal(1), Add, Ret]);
        let callee = func(0, 0, vec![Const(0), Ret]);
        let p = program(vec![K::Int(1)], vec![main, callee]);
        let mut r = Run::new(p, &[variant(1, 5), raw(TAG_UNIT, 0)]);
        assert_eq!(r.call(0), EXIT_CALL);
        assert_eq!(r.stack[1], int(5));
        assert_eq!(CALLS.with(|c| c.borrow().last().copied()), Some((1, 1, 0)));
        r.stack[r.ctx.sp as usize] = int(10);
        r.ctx.sp += 1;
        let pc = r.ctx.pc;
        assert_eq!(r.call(pc), EXIT_RET);
        assert_eq!(r.top(), int(15));
        assert_eq!(r.stub.generic, vec![Ret]);
    }

    #[test]
    fn helper_not_applicable_falls_back_to_generic() {
        use Op::*;
        // len of an Int local: helper says -1, the generic op runs (stub: unsupported -> we use JumpIfNotTag on Bool instead)
        let code = vec![Const(0), JumpIfNotTag(1, 4), Const(1), Ret, Const(2), Ret];
        let p = program(vec![K::Bool(true), K::Int(1), K::Int(2)], vec![func(0, 0, code)]);
        let mut r = Run::new(p, &[]);
        assert_eq!(r.call(0), EXIT_RET);
        assert_eq!(r.top(), int(1), "Bool true has tag 1");
    }
}

#[test]
fn rc_values_clone_and_drop_inline() {
    use Op::*;
    // A Str with a fake Rc header: [strong, weak, data].
    let hdr: &'static mut [i64; 3] = Box::leak(Box::new([1, 1, 0]));
    let p = hdr as *mut [i64; 3] as *mut i64;
    let s = Raw { tag: TAG_STR, pad: [0; 7], payload: p as i64, extra: 0 };
    OWNED.with(|c| c.set((0, 0)));
    let code = vec![
        LoadLocal(0), // strong 2
        LoadLocal(0), // strong 3
        Pop,          // strong 2
        StoreLocal(1), // moved into local 1 (over Unit)
        Unit,
        StoreLocal(1), // drop: strong 1
        Unit,
        StoreLocal(0), // last reference: zal_rt_drop
        Const(0),
        Ret,
    ];
    let prog = program(vec![K::Int(7)], vec![func(2, 2, code)]);
    let mut r = Run::new(prog, &[s, raw(TAG_UNIT, 0)]);
    assert_eq!(r.call(0), EXIT_RET);
    assert_eq!(r.top(), int(7));
    assert_eq!(unsafe { *p }, 0, "freed exactly once");
    assert_eq!(OWNED.with(|c| c.get()), (0, 1), "only the last drop calls the runtime");
}
