//! Every function of every differential/example program compiles (JIT and
//! object) without Cranelift verifier errors.

use std::path::{Path, PathBuf};

use zal_backend_cranelift::abi::*;
use zal_backend_cranelift::{jit_with as jit, object};

unsafe extern "C" fn op(_: *mut ZalCtx, _: u32) -> u32 {
    EXIT_TRAP
}
unsafe extern "C" fn trap(_: *mut ZalCtx, _: u32, _: u32) -> u32 {
    EXIT_TRAP
}
unsafe extern "C" fn clone(_: *mut u8, _: *const u8) {}
unsafe extern "C" fn drop_(_: *mut u8) {}

fn programs() -> Vec<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut v = Vec::new();
    for dir in ["tests/differential/progs", "examples"] {
        let Ok(rd) = std::fs::read_dir(repo.join(dir)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "zal") {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

#[test]
fn corpus_compiles() {
    #[allow(unused_mut)]
    let mut rt = vec![
        ("zal_rt_op", op as *const u8),
        ("zal_rt_tick", op as *const u8),
        ("zal_rt_trap", trap as *const u8),
        ("zal_rt_clone", clone as *const u8),
        ("zal_rt_drop", drop_ as *const u8),
    ];
    for name in ["zal_rt_len", "zal_rt_index", "zal_rt_for_next", "zal_rt_upd", "zal_rt_tag", "zal_rt_payload", "zal_rt_field", "zal_rt_map_get", "zal_rt_const", "zal_rt_concat", "zal_rt_call", "zal_rt_ret", "zal_rt_make", "zal_rt_builtin", "zal_rt_map_get_jump"] {
        rt.push((name, op as *const u8));
    }
    let roots = [Path::new(env!("CARGO_MANIFEST_DIR")).join("../../zal-src")];
    let (mut ok, mut funcs) = (0, 0);
    for p in programs() {
        let Ok(prog) = zal_compiler::compile_file(&p, &roots) else { continue };
        let code = jit(&prog, &rt).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert!(code.table().iter().all(Option::is_some));
        object(&prog).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        ok += 1;
        funcs += prog.functions.len();
    }
    eprintln!("compiled {ok} programs, {funcs} functions");
    assert!(ok > 20, "corpus too small: {ok}");
}
