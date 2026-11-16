//! The whole language suite with every function running as x64 machine code
//! (JIT table installed via `Vm::set_native`), like vm/tests/language_native.rs.

mod x64_vm {
    use std::ops::{Deref, DerefMut};
    use zal_vm::{Broker, Host};

    pub struct Vm<'p, H: Host, B: Broker>(zal_vm::Vm<'p, H, B>, #[allow(dead_code)] Option<zal_vm::native::JitModule>);

    impl<'p, H: Host, B: Broker> Vm<'p, H, B> {
        pub fn new(p: &'p zal_ir::Program, host: H, broker: B) -> Self {
            let mut vm = zal_vm::Vm::new(p, host, broker);
            // Unverifiable IR is refused by the backend; the VM then reports
            // the verifier error itself, as the suite expects.
            let m = zal_backend_x64::backend().jit(p);
            if let Ok(m) = &m {
                vm.set_native(m.table());
            }
            let m = m.ok();
            // Field order: the Vm drops before the code it calls into.
            Vm(vm, m)
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
    use super::x64_vm::Vm;
    include!(concat!(env!("OUT_DIR"), "/language.rs"));
}

/// Nested native frames, a backward loop (tick) and an overflow trap inside
/// x64 code report exactly what the interpreter reports.
#[test]
fn x64_runs_and_traps_like_the_interpreter() {
    use zal_vm::host::{MockHost, PolicyBroker};
    let src = "module t\nfn g(x: Int) -> Int { let mut i = 0\n while i < 3 { i = i + 1 }\n x + i }\nfn h(x: Int) -> Int { g(x) * 2 }\nfn f(x: Int) -> Int { h(x) + h(1) }";
    let p = zal_compiler::compile_source(src, "t.zal").unwrap();
    let m = zal_backend_x64::backend().jit(&p).unwrap();
    let mut out = vec![];
    for native in [false, true] {
        for arg in [5, i64::MAX - 1] {
            let mut vm = zal_vm::Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
            if native {
                vm.set_native(m.table());
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
