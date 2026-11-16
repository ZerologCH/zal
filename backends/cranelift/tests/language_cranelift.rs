//! Re-runs the VM's whole language suite (`vm/tests/language.rs`) with every
//! function executed as Cranelift machine code through the native dispatcher.

mod cl_vm {
    use std::ops::{Deref, DerefMut};

    use zal_vm::{Broker, Host};

    /// A VM with the Cranelift table installed. Field order matters: the VM
    /// drops before the code it may call.
    pub struct Vm<'p, H: Host, B: Broker>(zal_vm::Vm<'p, H, B>, #[allow(dead_code)] Option<zal_backend_cranelift::JitCode>);

    impl<'p, H: Host, B: Broker> Vm<'p, H, B> {
        pub fn new(p: &'p zal_ir::Program, host: H, broker: B) -> Self {
            let mut vm = zal_vm::Vm::new(p, host, broker);
            // IR that fails verification is refused by the backend; the VM
            // then reports it on run like it does without native code.
            let code = zal_backend_cranelift::jit(p).ok();
            if let Some(c) = &code {
                vm.set_native(c.table().to_vec());
            }
            Vm(vm, code)
        }
    }

    impl<'p, H: Host, B: Broker> Deref for Vm<'p, H, B> {
        type Target = zal_vm::Vm<'p, H, B>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<'p, H: Host, B: Broker> DerefMut for Vm<'p, H, B> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
}

mod language {
    // language.rs resolves files from the vm crate's manifest dir.
    #[allow(unused_macros)]
    macro_rules! env {
        ("CARGO_MANIFEST_DIR") => {
            concat!(std::env!("CARGO_MANIFEST_DIR"), "/../../vm")
        };
    }
    #[allow(unused_imports)]
    use super::cl_vm::Vm;
    include!("../../../vm/tests/language.rs");
}

/// Traps inside nested native frames report the interpreter's message and trace.
#[test]
fn cranelift_traps_like_the_interpreter() {
    use zal_vm::host::{MockHost, PolicyBroker};
    let src = "module t\nfn g(x: Int) -> Int { let mut i = 0\n while i < 3 { i = i + 1 }\n x + i }\nfn h(x: Int) -> Int { g(x) * 2 }\nfn f(x: Int) -> Int { h(x) + h(1) }";
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    let code = zal_backend_cranelift::jit(&p).unwrap();
    let mut out = vec![];
    for native in [false, true] {
        for arg in [5, i64::MAX - 1] {
            let mut vm = zal_vm::Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
            if native {
                vm.set_native(code.table().to_vec());
            }
            out.push(match vm.run("f", vec![zal_vm::Value::Int(arg)]) {
                Ok(v) => v.to_string(),
                Err(e) => e.to_string(),
            });
        }
    }
    assert_eq!(out[0], "24");
    assert!(out[1].contains("integer overflow in +") && out[1].contains("at g (line"), "{}", out[1]);
    assert_eq!(out[..2], out[2..]);
}

/// Inline `Index` (computed Int index), `Neg`, `Div` and `Mod`: same results
/// and the same trap messages as the interpreter, including the runtime
/// fallbacks (out of bounds, zero divisor, MIN / -1).
#[test]
fn cranelift_index_neg_div_mod_like_the_interpreter() {
    use zal_ir::Op;
    use zal_vm::host::{MockHost, PolicyBroker};
    use zal_vm::Value;
    let src = "module t
fn ix(xs: List<Int>, i: Int) -> Int { xs[i + 0] }
fn ixs(xs: List<Str>, i: Int) -> Str { xs[i + 0] + \"!\" }
fn tmp(a: Str, b: Str, i: Int) -> Str { [a, b][i + 0] }
fn ng(x: Int) -> Int { -x }
fn dv(a: Int, b: Int) -> Int { a / b }
fn md(a: Int, b: Int) -> Int { a % b }";
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    // The cases must exercise the generic ops, not fused forms.
    let has = |f: &str, want: fn(&Op) -> bool| p.functions.iter().find(|x| x.name == f).unwrap().code.iter().any(want);
    assert!(has("ix", |o| matches!(o, Op::Index)));
    assert!(has("tmp", |o| matches!(o, Op::Index)));
    assert!(has("ng", |o| matches!(o, Op::Neg)));
    assert!(has("dv", |o| matches!(o, Op::Div)));
    assert!(has("md", |o| matches!(o, Op::Mod)));
    let code = zal_backend_cranelift::jit(&p).unwrap();
    let ints = |v: &[i64]| Value::List(std::rc::Rc::new(v.iter().map(|x| Value::Int(*x)).collect()));
    let strs = |v: &[&str]| Value::List(std::rc::Rc::new(v.iter().map(|x| Value::Str((*x).into())).collect()));
    let s = |x: &str| Value::Str(x.into());
    let cases: Vec<(&str, Vec<Value>)> = vec![
        ("ix", vec![ints(&[10, 20, 30]), Value::Int(0)]),
        ("ix", vec![ints(&[10, 20, 30]), Value::Int(2)]),
        ("ix", vec![ints(&[10, 20, 30]), Value::Int(3)]),
        ("ix", vec![ints(&[10, 20, 30]), Value::Int(-1)]),
        ("ix", vec![ints(&[]), Value::Int(0)]),
        ("ixs", vec![strs(&["a", "bb"]), Value::Int(1)]),
        ("ixs", vec![strs(&["a", "bb"]), Value::Int(2)]),
        ("tmp", vec![s("x"), s("y"), Value::Int(0)]),
        ("tmp", vec![s("x"), s("y"), Value::Int(1)]),
        ("tmp", vec![s("x"), s("y"), Value::Int(5)]),
        ("ng", vec![Value::Int(7)]),
        ("ng", vec![Value::Int(-7)]),
        ("ng", vec![Value::Int(i64::MIN)]),
        ("ng", vec![Value::Int(i64::MAX)]),
        ("dv", vec![Value::Int(7), Value::Int(2)]),
        ("dv", vec![Value::Int(-7), Value::Int(2)]),
        ("dv", vec![Value::Int(7), Value::Int(-2)]),
        ("dv", vec![Value::Int(5), Value::Int(0)]),
        ("dv", vec![Value::Int(i64::MIN), Value::Int(-1)]),
        ("dv", vec![Value::Int(i64::MIN), Value::Int(1)]),
        ("md", vec![Value::Int(7), Value::Int(2)]),
        ("md", vec![Value::Int(-7), Value::Int(2)]),
        ("md", vec![Value::Int(7), Value::Int(-2)]),
        ("md", vec![Value::Int(5), Value::Int(0)]),
        ("md", vec![Value::Int(i64::MIN), Value::Int(-1)]),
    ];
    for (f, args) in cases {
        let mut out = vec![];
        for native in [false, true] {
            let mut vm = zal_vm::Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
            if native {
                vm.set_native(code.table().to_vec());
            }
            out.push(match vm.run(f, args.clone()) {
                Ok(v) => v.to_string(),
                Err(e) => e.to_string(),
            });
        }
        assert_eq!(out[0], out[1], "{f}({args:?})");
    }
}
