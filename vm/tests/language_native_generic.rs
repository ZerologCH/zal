//! Re-runs the whole language suite with every function (each op through zal_rt_op: lean path and fallbacks) executed through the native ABI
//! (RFC 0002) by the test-only fake backend `zal_vm::native::fake`: native
//! dispatch, `zal_rt_op`/`tick`/`trap`/`clone`/`drop` and every exit code.

mod fake_vm {
    use std::ops::{Deref, DerefMut};

    use zal_vm::{Broker, Host};

    pub struct Vm<'p, H: Host, B: Broker>(zal_vm::Vm<'p, H, B>);

    impl<'p, H: Host, B: Broker> Vm<'p, H, B> {
        pub fn new(p: &'p zal_ir::Program, host: H, broker: B) -> Self {
            let mut vm = zal_vm::Vm::new(p, host, broker);
            vm.set_native(zal_vm::native::fake::table_generic(p));
            Vm(vm)
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
    #[allow(unused_imports)]
    use super::fake_vm::Vm;
    include!("language.rs");
}

/// The suite above must really go through machine-code-shaped bodies, and a
/// trap inside nested native frames must report the interpreter's message and trace.
#[test]
fn native_path_runs_and_traps_like_the_interpreter() {
    use zal_vm::host::{MockHost, PolicyBroker};
    let src = "module t\nfn g(x: Int) -> Int { let mut i = 0\n while i < 3 { i = i + 1 }\n x + i }\nfn h(x: Int) -> Int { g(x) * 2 }\nfn f(x: Int) -> Int { h(x) + h(1) }";
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    let before = zal_vm::native::fake::entries();
    let mut out = vec![];
    for native in [false, true] {
        for arg in [5, i64::MAX - 1] {
            let mut vm = zal_vm::Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
            if native {
                vm.set_native(zal_vm::native::fake::table_generic(&p));
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
    assert!(zal_vm::native::fake::entries() > before);
}
