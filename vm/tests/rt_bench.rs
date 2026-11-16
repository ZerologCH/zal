//! Microbenchmarks of the native runtime entry points (RFC 0002 v2).
//! `cargo test --release -p zal-vm --test rt_bench -- --ignored --nocapture`

use std::hint::black_box;
use std::mem::MaybeUninit;
use std::rc::Rc;
use std::time::Instant;

use zal_vm::host::{MockHost, PolicyBroker};
use zal_vm::native::*;
use zal_vm::value::{str_val, ZMap};
use zal_vm::{NativeFn, Value, Vm};

fn best_ns<F: FnMut()>(reps: usize, mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_nanos() as f64);
    }
    best
}

fn run_ns(p: &zal_ir::Program, entry: &str, n: i64, table: Option<Vec<Option<NativeFn>>>) -> f64 {
    best_ns(7, || {
        let mut vm = Vm::new(p, MockHost::default(), PolicyBroker::new(vec![]));
        if let Some(t) = &table {
            vm.set_native(t.clone());
        }
        black_box(vm.run(entry, vec![Value::Int(n)]).unwrap());
    })
}

fn ops_of(p: &zal_ir::Program, entry: &str, n: i64) -> u64 {
    let mut vm = Vm::new(p, MockHost::default(), PolicyBroker::new(vec![]));
    vm.profile = Some(Default::default());
    vm.run(entry, vec![Value::Int(n)]).unwrap();
    vm.stats.ops
}

#[test]
#[ignore]
fn bench_entry_points() {
    let src = "module b
fn one() -> Int { 1 }
fn plain(n: Int) -> Int { let mut s = 0\n let mut i = 0\n while i < n { s = s + i % 7\n i = i + 1 }\n s }
fn calls(n: Int) -> Int { let mut s = 0\n let mut i = 0\n while i < n { s = s + one()\n i = i + 1 }\n s }
fn adds(n: Int) -> Int { let mut s = 0\n let mut i = 0\n while i < n { s = s + 1\n i = i + 1 }\n s }";
    let p = zal_compiler::compile_source(src, "b.zal").unwrap();
    let n = 2_000_000;

    // zal_rt_op: every op of a scalar loop through the generic path vs interpreted.
    let ops = ops_of(&p, "plain", n) as f64;
    let vm = run_ns(&p, "plain", n, None);
    let gen = run_ns(&p, "plain", n, Some(fake::table_generic(&p)));
    println!("zal_rt_op: interpreter {:.2} ns/op, all-generic body {:.2} ns/op, overhead {:.2} ns/op ({} ops)", vm / ops, gen / ops, (gen - vm) / ops, ops);

    // Call + return, native -> native (fake_body: zal_rt_call, Const, lean Ret).
    let c = run_ns(&p, "calls", n, Some(fake::table(&p)));
    let a = run_ns(&p, "adds", n, Some(fake::table(&p)));
    println!("call+return native->native: {:.2} ns per call (incl. the callee's Const+Ret ops)", (c - a) / n as f64);
    let ci = run_ns(&p, "calls", n, None);
    let ai = run_ns(&p, "adds", n, None);
    println!("call+return interpreter:    {:.2} ns per call", (ci - ai) / n as f64);

    // Typed helpers in isolation.
    const K: usize = 1_000_000;
    let l = Value::List(Rc::new((0..100).map(Value::Int).collect()));
    let s = str_val("hello world");
    let m = Value::Map(Rc::new((0..50).map(|i| (Rc::from(format!("k{i}").as_str()), Value::Int(i))).collect::<ZMap>()));
    let key = str_val("k17");
    let v = Value::Variant(1, Some(Rc::new(Value::Int(3))));
    let r = Value::Record(Rc::from(vec![Value::Int(1), Value::Int(2)]));
    let per = |ns: f64| ns / K as f64;
    unsafe {
        let t = best_ns(5, || {
            for _ in 0..K {
                black_box(zal_rt_len(black_box(&l)));
            }
        });
        println!("zal_rt_len(list)      {:.2} ns", per(t));
        let t = best_ns(5, || {
            for _ in 0..K {
                black_box(zal_rt_len(black_box(&s)));
            }
        });
        println!("zal_rt_len(str 11ch)  {:.2} ns", per(t));
        let mut d = MaybeUninit::<Value>::uninit();
        let t = best_ns(5, || {
            for i in 0..K {
                zal_rt_index(d.as_mut_ptr(), black_box(&l), (i % 100) as i64);
            }
        });
        println!("zal_rt_index          {:.2} ns", per(t));
        let t = best_ns(5, || {
            for _ in 0..K {
                zal_rt_map_get(d.as_mut_ptr(), black_box(&m), black_box(&key));
            }
        });
        println!("zal_rt_map_get        {:.2} ns", per(t));
        let t = best_ns(5, || {
            for _ in 0..K {
                black_box(zal_rt_tag(black_box(&v)));
            }
        });
        println!("zal_rt_tag            {:.2} ns", per(t));
        let t = best_ns(5, || {
            for _ in 0..K {
                zal_rt_payload(d.as_mut_ptr(), black_box(&v));
            }
        });
        println!("zal_rt_payload        {:.2} ns", per(t));
        let t = best_ns(5, || {
            for _ in 0..K {
                zal_rt_field(d.as_mut_ptr(), black_box(&r), 1);
            }
        });
        println!("zal_rt_field          {:.2} ns", per(t));
        let t = best_ns(5, || {
            let mut slot = [l.clone(), Value::Int(0), Value::Unit];
            for _ in 0..K {
                if zal_rt_for_next(slot.as_mut_ptr()) != 0 {
                    slot[1] = Value::Int(0);
                }
            }
        });
        println!("zal_rt_for_next       {:.2} ns", per(t));
        let t = best_ns(5, || {
            let mut x = Value::List(Rc::new(Vec::with_capacity(K)));
            for i in 0..K {
                let mut a = [Value::Int(i as i64)];
                zal_rt_upd(&mut x, a.as_mut_ptr(), ZAL_BUILTIN_PUSH);
                std::mem::forget(a);
            }
            black_box(x);
        });
        println!("zal_rt_upd(push)      {:.2} ns", per(t));
        let t = best_ns(5, || {
            for _ in 0..K {
                zal_rt_clone(d.as_mut_ptr(), black_box(&l));
                zal_rt_drop(d.as_mut_ptr());
            }
        });
        println!("zal_rt_clone+drop     {:.2} ns", per(t));
    }
}

/// One run of the native call loop, for `valgrind --tool=callgrind`.
#[test]
#[ignore]
fn profile_native_calls() {
    let src = "module b\nfn one() -> Int { 1 }\nfn calls(n: Int) -> Int { let mut s = 0\n let mut i = 0\n while i < n { s = s + one()\n i = i + 1 }\n s }";
    let p = zal_compiler::compile_source(src, "b.zal").unwrap();
    let table: Vec<Option<NativeFn>> = vec![Some(one_body as NativeFn), Some(calls_body as NativeFn)];
    run_ns(&p, "calls", 300_000, Some(table));
}

// Hand-written bodies shaped like ideal generated code for
//   #0 one() -> Int { 1 }            : Const(0); Ret
//   #1 calls(n): while i < n { s = s + one(); i = i + 1 }; s
// so the per-iteration time is the runtime's call+return cost plus a few
// inline instructions (no fake-body decoding).
unsafe fn int(c: &ZalCtx, at: usize) -> i64 {
    match &*c.stack.add(at) {
        Value::Int(x) => *x,
        _ => unreachable!(),
    }
}

unsafe extern "C" fn one_body(ctx: *mut ZalCtx, _pc: u32) -> u32 {
    let c = &mut *ctx;
    c.stack.add(c.sp as usize).write(Value::Int(1));
    c.sp += 1;
    zal_rt_ret(ctx)
}

unsafe extern "C" fn calls_body(ctx: *mut ZalCtx, entry: u32) -> u32 {
    let c = &mut *ctx;
    let b = c.base as usize;
    if entry == 0 {
        *c.stack.add(b + 1) = Value::Int(0);
        *c.stack.add(b + 2) = Value::Int(0);
    } else {
        // pc 8: Add; StoreLocal(1); IncLocal(2, 1); Jump(4) (back edge)
        let r = int(c, c.sp as usize - 1) + int(c, c.sp as usize - 2);
        c.sp -= 2;
        c.stack.add(b + 1).write(Value::Int(r));
        c.stack.add(b + 2).write(Value::Int(int(c, b + 2) + 1));
        c.slice += 1;
        if c.slice >= ZAL_SLICE {
            let r = zal_rt_tick(ctx, 4);
            if r != 0 {
                return r;
            }
        }
    }
    let c = &mut *ctx;
    if int(c, b + 2) < int(c, b) {
        // pc 6, 7: LoadLocal(1); Call(0, 0)
        c.stack.add(c.sp as usize).write(Value::Int(int(c, b + 1)));
        c.sp += 1;
        return zal_rt_call(ctx, 7, 0, 0);
    }
    c.stack.add(c.sp as usize).write(Value::Int(int(c, b + 1)));
    c.sp += 1;
    zal_rt_ret(ctx)
}

#[test]
#[ignore]
fn bench_native_call_return() {
    let src = "module b\nfn one() -> Int { 1 }\nfn calls(n: Int) -> Int { let mut s = 0\n let mut i = 0\n while i < n { s = s + one()\n i = i + 1 }\n s }";
    let p = zal_compiler::compile_source(src, "b.zal").unwrap();
    assert!(matches!(p.functions[1].code[7], zal_ir::Op::Call(0, 0)) && matches!(p.functions[1].code[13], zal_ir::Op::Ret));
    let n = 5_000_000;
    let table: Vec<Option<NativeFn>> = vec![Some(one_body as NativeFn), Some(calls_body as NativeFn)];
    let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
    vm.set_native(table.clone());
    assert_eq!(vm.run("calls", vec![Value::Int(1000)]).unwrap(), Value::Int(1000));
    let t = run_ns(&p, "calls", n, Some(table));
    let ti = run_ns(&p, "calls", n, None);
    println!("native->native call+return (zal_rt_call + Ret via zal_rt_op + dispatcher): {:.2} ns per iteration; interpreter {:.2} ns per iteration", t / n as f64, ti / n as f64);
}
