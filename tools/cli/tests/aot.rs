//! AOT runtime and link step (RFC 0002 §5) without a code generator: the
//! "object" is C with an all-null function table (null = interpret), the
//! embedded IR and the counts. Linking it with libzal_aot.a must give an
//! executable that behaves exactly like `zal run`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Build libzal_aot.a in its own target dir (the outer cargo holds the lock on ours).
fn build_aot_lib() -> PathBuf {
    let target = repo().join("target/aot-test");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--release", "--quiet", "-p", "zal-aot"])
        .env("CARGO_TARGET_DIR", &target)
        .current_dir(repo())
        .status()
        .expect("cargo");
    assert!(st.success(), "building zal-aot failed");
    target.join("release/libzal_aot.a")
}

/// Fixed scratch dir inside the target tree, overwritten by every run.
fn scratch() -> PathBuf {
    let d = repo().join("target/aot-test/link");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn c_object(ir: &str, nfuncs: usize, dir: &Path) -> Vec<u8> {
    let bytes: Vec<String> = ir.bytes().map(|b| b.to_string()).collect();
    let src = format!(
        "#include <stdint.h>\nconst unsigned char zal_ir[] = {{{}}};\nconst uint64_t zal_ir_len = {};\nvoid *const zal_fn_table[{nfuncs}] = {{0}};\nconst uint32_t zal_fn_count = {nfuncs};\n",
        bytes.join(","),
        ir.len()
    );
    let c = dir.join("prog.c");
    std::fs::write(&c, src).unwrap();
    let o = dir.join("prog_c.o");
    let st = Command::new("cc").arg("-c").arg(&c).arg("-o").arg(&o).status().expect("cc");
    assert!(st.success());
    std::fs::read(&o).unwrap()
}

#[test]
fn aot_library_exports_runtime_and_links_into_an_executable() {
    let lib = build_aot_lib();

    // Symbols: the runtime defines main and zal_rt_*, and expects the object's exports.
    let nm = Command::new("nm").arg("-g").arg(&lib).output().expect("nm");
    let syms = String::from_utf8_lossy(&nm.stdout);
    let has = |kind: &str, name: &str| syms.lines().any(|l| l.ends_with(&format!(" {kind} {name}")));
    let mut want = vec!["main"];
    want.extend(zal_vm::native::runtime_symbols().iter().map(|s| s.0));
    for s in want {
        assert!(has("T", s), "libzal_aot.a lacks {s}");
    }
    for s in ["zal_ir", "zal_ir_len", "zal_fn_table", "zal_fn_count"] {
        assert!(has("U", s), "libzal_aot.a should import {s}");
    }

    // Link a program and compare with `zal run` (prints, host calls, result).
    let r = repo();
    let p = zal_compiler::compile_file(&r.join("examples/dag_pipeline.zal"), &[r.join("zal-src"), r.join("examples")]).unwrap();
    let dir = scratch();
    let obj = c_object(&p.to_json(), p.functions.len(), &dir);
    let bin = dir.join("dag");
    zal_cli::aot_link_with(&obj, &bin, &lib).unwrap();

    let wl = r.join("benchmarks/workloads");
    let common = |c: &mut Command| {
        c.args(["--entry", "run_graph", "--args-file"]).arg(wl.join("w2-small.json")).arg("--mock").arg(wl.join("w2.mock.json")).arg("--policy").arg(wl.join("w2.policy.json"));
    };
    let mut aot = Command::new(&bin);
    common(&mut aot);
    let mut vm = Command::new(env!("CARGO_BIN_EXE_zal"));
    vm.current_dir(&r).arg("run").arg(r.join("examples/dag_pipeline.zal"));
    common(&mut vm);
    let (a, v) = (aot.output().unwrap(), vm.output().unwrap());
    assert!(v.status.success(), "{}", String::from_utf8_lossy(&v.stderr));
    assert_eq!(a.status.code(), v.status.code(), "{}", String::from_utf8_lossy(&a.stderr));
    assert_eq!(String::from_utf8_lossy(&a.stdout), String::from_utf8_lossy(&v.stdout));

    // Errors behave like `zal run` too: message on stderr, exit 1.
    let a = Command::new(&bin).args(["--entry", "nope"]).output().unwrap();
    assert_eq!(a.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&a.stderr).contains("no function `nope`"));
}
