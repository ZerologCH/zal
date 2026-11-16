//! Library side of the `zal` CLI: machine-code backend registry and the AOT
//! link step (RFC 0002 §5-§6).

use std::path::{Path, PathBuf};

use zal_ir::Program;
use zal_vm::native::{JitModule, NativeBackend};

/// Names `--backend` accepts besides `vm` (built or not).
pub const BACKEND_NAMES: [&str; 2] = ["cranelift", "x64"];

/// Every backend compiled into this binary. Streams A and B register here.
pub fn backends() -> Vec<Box<dyn NativeBackend>> {
    #[allow(unused_mut)]
    let mut v: Vec<Box<dyn NativeBackend>> = vec![Box::new(FakeBackend)];
    #[cfg(feature = "cranelift")]
    v.push(zal_backend_cranelift::backend());
    #[cfg(feature = "x64")]
    v.push(zal_backend_x64::backend());
    v
}

/// The backend called `name`, or a message saying why there is none.
pub fn backend(name: &str) -> Result<Box<dyn NativeBackend>, String> {
    if let Some(b) = backends().into_iter().find(|b| b.name() == name) {
        return Ok(b);
    }
    if BACKEND_NAMES.contains(&name) {
        Err(format!("backend not built: `{name}` (rebuild zal with `--features {name}`)"))
    } else {
        Err(format!("unknown backend `{name}` (vm, {})", BACKEND_NAMES.join(", ")))
    }
}

/// Hidden test backend `fake`: no code generation, every function runs the
/// generic-path body of `zal_vm::native::fake` (proves the ABI end to end,
/// including the differential runner's JIT mode).
pub struct FakeBackend;

impl NativeBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn jit(&self, p: &Program) -> Result<JitModule, String> {
        Ok(JitModule::new(zal_vm::native::fake::table(p), Box::new(())))
    }
    fn object(&self, _p: &Program) -> Result<Vec<u8>, String> {
        Err("the fake backend has no object output".into())
    }
}

/// Where `libzal_aot.a` is: `$ZAL_AOT_LIB`, else next to the running `zal`
/// (cargo puts both in `target/<profile>/`), else the repo's release dir.
pub fn aot_lib() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("ZAL_AOT_LIB") {
        return Ok(PathBuf::from(p));
    }
    let mut cands = vec![];
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        cands.push(dir.join("libzal_aot.a"));
        if let Some(up) = dir.parent() {
            cands.push(up.join("libzal_aot.a")); // test binaries live in target/<profile>/deps
        }
    }
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/libzal_aot.a"));
    cands
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| "libzal_aot.a not found: run `cargo build --release -p zal-aot` or set ZAL_AOT_LIB".into())
}

/// Write `object` next to `out` and link it with the AOT runtime into an
/// executable: `cc out.o libzal_aot.a -lpthread -ldl -lm -o out`.
pub fn aot_link(object: &[u8], out: &Path) -> Result<(), String> {
    aot_link_with(object, out, &aot_lib()?)
}

pub fn aot_link_with(object: &[u8], out: &Path, lib: &Path) -> Result<(), String> {
    let obj = out.with_extension("o");
    std::fs::write(&obj, object).map_err(|e| format!("{}: {e}", obj.display()))?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let o = std::process::Command::new(&cc)
        .arg(&obj)
        .arg(lib)
        .args(["-lpthread", "-ldl", "-lm", "-o"])
        .arg(out)
        .output()
        .map_err(|e| format!("cannot run {cc}: {e}"))?;
    if !o.status.success() {
        return Err(format!("link failed ({cc}): {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(())
}
