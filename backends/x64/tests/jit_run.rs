//! Isolation test: JIT-compile functions and run them with stub runtime
//! entry points, comparing against a reference mini-interpreter that shares
//! the exact op semantics. The JIT mixes inline fast paths with generic
//! fallbacks, so matching the reference exercises both.
//!
//! Values are the real `zal_vm::Value` (fixed `#[repr(C, u8)]` layout), so the
//! generated code reads and writes exactly the ABI the VM uses.

use std::cell::Cell;
use std::rc::Rc;

use zal_backend_x64::{jit_program, RtFns};
use zal_ir::{Const as CV, FnKind, Function, Op, Program, Ty};
use zal_vm::native::{ZalCtx, ZalValue, ZAL_EXIT_RET, ZAL_EXIT_TRAP, ZAL_JUMP, ZAL_TRAP_ADD, ZAL_TRAP_MUL};
use zal_vm::Value;

// ---- shared op semantics (reference and stub both call this) ----------------

enum StepR {
    Next,
    Jump(u32),
    Exit(u32),
    Trap(u32),
}

/// Execute `func.code[pc]` on a `Value` stack. Mirrors the VM for the subset
/// these tests use.
fn step(prog: &Program, func: &Function, pc: usize, stack: &mut Vec<Value>, base: usize) -> StepR {
    let op = func.code[pc];
    macro_rules! pop {
        () => {
            stack.pop().unwrap()
        };
    }
    match op {
        Op::Const(c) => {
            let v = match &prog.consts[c as usize] {
                CV::Int(i) => Value::Int(*i),
                CV::Bool(b) => Value::Bool(*b),
                CV::Dur(d) => Value::Dur(*d),
                CV::Str(s) => Value::Str(Rc::from(s.as_str())),
            };
            stack.push(v);
            StepR::Next
        }
        Op::Unit => {
            stack.push(Value::Unit);
            StepR::Next
        }
        Op::LoadLocal(i) => {
            stack.push(stack[base + i as usize].clone());
            StepR::Next
        }
        Op::StoreLocal(i) => {
            let v = pop!();
            stack[base + i as usize] = v;
            StepR::Next
        }
        Op::IncLocal(i, k) => match &stack[base + i as usize] {
            Value::Int(x) => match x.checked_add(k as i64) {
                Some(r) => {
                    stack[base + i as usize] = Value::Int(r);
                    StepR::Next
                }
                None => StepR::Trap(ZAL_TRAP_ADD),
            },
            _ => panic!("IncLocal on non-int"),
        },
        Op::Pop => {
            pop!();
            StepR::Next
        }
        Op::Add | Op::Sub | Op::Mul => {
            let b = pop!();
            let a = pop!();
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => {
                    let (r, code) = match op {
                        Op::Add => (x.checked_add(y), zal_vm::native::ZAL_TRAP_ADD),
                        Op::Sub => (x.checked_sub(y), zal_vm::native::ZAL_TRAP_SUB),
                        _ => (x.checked_mul(y), ZAL_TRAP_MUL),
                    };
                    match r {
                        Some(r) => {
                            stack.push(Value::Int(r));
                            StepR::Next
                        }
                        None => StepR::Trap(code),
                    }
                }
                (Value::Str(x), Value::Str(y)) if matches!(op, Op::Add) => {
                    stack.push(Value::Str(Rc::from(format!("{x}{y}").as_str())));
                    StepR::Next
                }
                (a, b) => panic!("bad arith operands {a:?} {b:?}"),
            }
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Eq | Op::Ne => {
            let b = pop!();
            let a = pop!();
            let r = match op {
                Op::Eq => a == b,
                Op::Ne => a != b,
                _ => {
                    let (x, y) = match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => (*x, *y),
                        _ => panic!("cmp of {a:?} {b:?}"),
                    };
                    match op {
                        Op::Lt => x < y,
                        Op::Le => x <= y,
                        Op::Gt => x > y,
                        _ => x >= y,
                    }
                }
            };
            stack.push(Value::Bool(r));
            StepR::Next
        }
        Op::Jump(t) => StepR::Jump(t),
        Op::JumpIfFalse(t) => {
            if let Value::Bool(false) = pop!() {
                StepR::Jump(t)
            } else {
                StepR::Next
            }
        }
        Op::JumpIfTrue(t) => {
            if let Value::Bool(true) = pop!() {
                StepR::Jump(t)
            } else {
                StepR::Next
            }
        }
        Op::JumpIfNotLt(t) => {
            let b = pop!();
            let a = pop!();
            let lt = match (a, b) {
                (Value::Int(x), Value::Int(y)) => x < y,
                (a, b) => panic!("JumpIfNotLt {a:?} {b:?}"),
            };
            if lt {
                StepR::Next
            } else {
                StepR::Jump(t)
            }
        }
        Op::Ret => StepR::Exit(ZAL_EXIT_RET),
        other => panic!("op {other:?} not in test subset"),
    }
}

fn reference(prog: &Program, fi: usize, args: &[Value]) -> Result<Value, u32> {
    let func = &prog.functions[fi];
    let mut stack = args.to_vec();
    stack.resize(func.locals as usize, Value::Unit);
    let mut pc = 0usize;
    loop {
        match step(prog, func, pc, &mut stack, 0) {
            StepR::Next => pc += 1,
            StepR::Jump(t) => pc = t as usize,
            StepR::Exit(_) => return Ok(stack.pop().unwrap()),
            StepR::Trap(c) => return Err(c),
        }
    }
}

// ---- stub runtime shared with the JIT through a thread-local ----------------

struct RunState {
    prog: *const Program,
    func: usize,
    trapped: Cell<Option<u32>>,
}

thread_local! {
    static STATE: Cell<*const RunState> = const { Cell::new(std::ptr::null()) };
}

/// Read the operand stack as owned `Value`s (cloning), run one op, write back.
extern "C" fn stub_op(ctx: *mut ZalCtx, pc: u32) -> u32 {
    let st = unsafe { &*STATE.with(|s| s.get()) };
    let prog = unsafe { &*st.prog };
    let func = &prog.functions[st.func];
    let c = unsafe { &mut *ctx };
    let slots = unsafe { std::slice::from_raw_parts_mut(c.stack, 4096) };
    let base = c.base as usize;
    let old_sp = c.sp as usize;
    // Move the live values out of the slots (they are logically uninit now).
    let mut stack: Vec<Value> = (0..old_sp).map(|i| unsafe { std::ptr::read(&slots[i]) }).collect();
    let r = step(prog, func, pc as usize, &mut stack, base);
    let new_sp = stack.len();
    // Move the new stack back, then refill any vacated slots so every slot
    // stays a valid, droppable value.
    for (i, v) in stack.into_iter().enumerate() {
        unsafe { std::ptr::write(&mut slots[i], v) };
    }
    for s in slots.iter_mut().take(old_sp).skip(new_sp) {
        unsafe { std::ptr::write(s, Value::Unit) };
    }
    c.sp = new_sp as u64;
    match r {
        StepR::Next => 0,
        StepR::Jump(t) => {
            c.pc = t;
            ZAL_JUMP
        }
        StepR::Exit(code) => code,
        StepR::Trap(code) => {
            st.trapped.set(Some(code));
            ZAL_EXIT_TRAP
        }
    }
}

extern "C" fn stub_tick(_ctx: *mut ZalCtx, _target: u32) -> u32 {
    0
}

extern "C" fn stub_trap(_ctx: *mut ZalCtx, _pc: u32, code: u32) -> u32 {
    let st = unsafe { &*STATE.with(|s| s.get()) };
    st.trapped.set(Some(code));
    ZAL_EXIT_TRAP
}

fn rt() -> RtFns {
    RtFns::default()
        .with("zal_rt_op", stub_op as *const () as usize)
        .with("zal_rt_tick", stub_tick as *const () as usize)
        .with("zal_rt_trap", stub_trap as *const () as usize)
}

/// JIT-run `fi` with `args`; Ok(result) or Err(trap code).
fn jit_run(prog: &Program, fi: usize, args: &[Value]) -> Result<Value, u32> {
    let m = jit_program(prog, &rt()).unwrap();
    let table = m.table();
    let f = table[fi].unwrap();
    let func = &prog.functions[fi];
    let mut slots: Vec<ZalValue> = Vec::with_capacity(4096);
    for a in args {
        slots.push(a.clone());
    }
    while slots.len() < 4096 {
        slots.push(Value::Unit);
    }
    let state = RunState { prog, func: fi, trapped: Cell::new(None) };
    STATE.with(|s| s.set(&state));
    let mut ctx = ZalCtx {
        vm: std::ptr::null_mut(),
        stack: slots.as_mut_ptr(),
        sp: func.locals as u64,
        base: 0,
        pc: 0,
        func: fi as u32,
        slice: 0,
    };
    let code = unsafe { f(&mut ctx, 0) };
    STATE.with(|s| s.set(std::ptr::null()));
    // Clone the result out so the slot buffer still owns every value and drops
    // cleanly when it goes out of scope.
    if let Some(c) = state.trapped.get() {
        assert_eq!(code, ZAL_EXIT_TRAP);
        Err(c)
    } else {
        assert_eq!(code, ZAL_EXIT_RET, "expected a clean return");
        Ok(slots[ctx.sp as usize - 1].clone())
    }
}

// ---- builders ---------------------------------------------------------------

fn func(name: &str, nparams: usize, locals: u16, code: Vec<Op>) -> Function {
    Function {
        name: name.into(),
        kind: FnKind::Fn,
        params: (0..nparams).map(|i| (format!("p{i}"), Ty::Int)).collect(),
        ret: Ty::Int,
        locals,
        lines: vec![0; code.len()],
        code,
        effects: vec![],
        budget: None,
        slot_tags: vec![],
        slot_seal: 0,
    }
}

fn program(consts: Vec<CV>, functions: Vec<Function>) -> Program {
    Program { ir_version: 1, module: "test".into(), consts, functions, externs: vec![], records: vec![], enums: vec![] }
}

fn both(prog: &Program, fi: usize, args: &[Value]) {
    let want = reference(prog, fi, args);
    let got = jit_run(prog, fi, args);
    match (&got, &want) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "fn {fi} args {args:?}"),
        (Err(a), Err(b)) => assert_eq!(a, b, "fn {fi} args {args:?}"),
        _ => panic!("fn {fi} args {args:?}: jit {got:?} vs ref {want:?}"),
    }
}

#[test]
fn arithmetic_and_locals() {
    use Op::*;
    let p = program(
        vec![CV::Int(3)],
        vec![func("f", 2, 3, vec![LoadLocal(0), LoadLocal(1), Add, Const(0), Mul, StoreLocal(2), LoadLocal(2), LoadLocal(0), Sub, Ret])],
    );
    for (a, b) in [(2, 5), (-7, 7), (0, 0), (100, -1)] {
        both(&p, 0, &[Value::Int(a), Value::Int(b)]);
    }
}

#[test]
fn comparisons_and_branches() {
    use Op::*;
    let p = program(
        vec![CV::Int(1), CV::Int(2)],
        vec![func("f", 2, 2, vec![LoadLocal(0), LoadLocal(1), Lt, JumpIfFalse(6), Const(0), Ret, Const(1), Ret])],
    );
    for (a, b) in [(1, 2), (2, 1), (3, 3)] {
        both(&p, 0, &[Value::Int(a), Value::Int(b)]);
    }
}

#[test]
fn loop_sum_with_backward_jump() {
    use Op::*;
    let p = program(
        vec![CV::Int(0)],
        vec![func(
            "f",
            1,
            3,
            vec![
                Const(0), StoreLocal(1), Const(0), StoreLocal(2),
                LoadLocal(2), LoadLocal(0), JumpIfNotLt(16),
                LoadLocal(1), LoadLocal(2), Add, StoreLocal(1), IncLocal(2, 1), Jump(4),
                Unit, Pop, Unit, LoadLocal(1), Ret,
            ],
        )],
    );
    for n in [0, 1, 5, 50] {
        both(&p, 0, &[Value::Int(n)]);
    }
}

#[test]
fn integer_overflow_traps() {
    use Op::*;
    let p = program(vec![], vec![func("f", 2, 2, vec![LoadLocal(0), LoadLocal(1), Add, Ret])]);
    assert_eq!(jit_run(&p, 0, &[Value::Int(i64::MAX), Value::Int(1)]), Err(ZAL_TRAP_ADD));
    let p = program(vec![], vec![func("f", 2, 2, vec![LoadLocal(0), LoadLocal(1), Mul, Ret])]);
    assert_eq!(jit_run(&p, 0, &[Value::Int(i64::MAX), Value::Int(2)]), Err(ZAL_TRAP_MUL));
    let p = program(vec![], vec![func("f", 1, 1, vec![IncLocal(0, 1), LoadLocal(0), Ret])]);
    assert_eq!(jit_run(&p, 0, &[Value::Int(i64::MAX)]), Err(ZAL_TRAP_ADD));
}

#[test]
fn tag_guard_falls_back_to_generic() {
    use Op::*;
    // tag-3 strings must route Add and Eq through the generic path.
    let p = program(vec![CV::Str("ab".into()), CV::Str("cd".into())], vec![func("f", 0, 0, vec![Const(0), Const(1), Add, Ret])]);
    assert_eq!(jit_run(&p, 0, &[]), Ok(Value::Str(Rc::from("abcd"))));
}

#[test]
fn resume_via_entry_pc() {
    use Op::*;
    let p = program(vec![CV::Int(99)], vec![func("f", 1, 2, vec![Const(0), StoreLocal(1), LoadLocal(1), Ret])]);
    let m = jit_program(&p, &rt()).unwrap();
    let table = m.table();
    let f = table[0].unwrap();
    let mut slots: Vec<ZalValue> = vec![Value::Int(7), Value::Int(42)];
    while slots.len() < 4096 {
        slots.push(Value::Unit);
    }
    let state = RunState { prog: &p, func: 0, trapped: Cell::new(None) };
    STATE.with(|s| s.set(&state));
    let mut ctx = ZalCtx { vm: std::ptr::null_mut(), stack: slots.as_mut_ptr(), sp: 2, base: 0, pc: 0, func: 0, slice: 0 };
    let code = unsafe { f(&mut ctx, 2) }; // resume at pc 2, skipping the store
    STATE.with(|s| s.set(std::ptr::null()));
    assert_eq!(code, ZAL_EXIT_RET);
    assert_eq!(slots[ctx.sp as usize - 1].clone(), Value::Int(42));
}
