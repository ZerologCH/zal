//! `zal` command line: check, build, ir, run, test.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

mod hints;

// Small-block free lists for the VM's many short-lived values (vm/src/alloc.rs).
#[global_allocator]
static ALLOC: zal_vm::alloc::ZalAlloc = zal_vm::alloc::ZalAlloc;

use zal_ir::{FnKind, Program};
use zal_vm::host::{JsonlHost, MockHost, PolicyBroker};
use zal_vm::value::{from_json, to_json};
use zal_vm::{Broker, Host, Vm};

const USAGE: &str = "zal 0.1 — ZeroLog Agent Language (stage-0)

usage:
  zal check <file.zal> [--json]              type/effect-check a program (--json: one diagnostic per line, with fix hints)
  zal fmt   <file.zal> [--check] [--write]   canonical formatting (stdout; --check: exit 1 if changes; --write: in place)
  zal build <file.zal> [-o out.zir.json]     compile to ZAL IR
  zal build <file.zal|.zir.json> --backend cranelift|x64 -o <bin>
                                             compile to machine code and link an executable (AOT)
  zal ir    <file.zal|file.zir.json>         print the IR listing
  zal run   <file.zal|file.zir.json> --entry <name> [--args '<json array>' | --args-file f.json]
            [--mock mock.json | --host '<command>'] [--policy policy.json] [--stats]
            [--backend vm|cranelift|x64]   (default vm; others compile to machine code first)
  zal test  <file.zal> [--mock mock.json]    run `test` blocks (grants each test its `requires`)
  zal native <file.zal> [-o dir]             compile to a native binary (AOT) with a bench harness
  zal bench <file.zal> --entry <name> --args-file <json> --mock m.json --policy p.json
            [--iters 30] [--warmup 5] [--label name] [--backend ...]   time repeated runs, print one JSON line

options:
  --root <dir>   extra import root (repeatable); defaults: file's dir, ./zal-src, $ZAL_PATH
";

struct Opts {
    cmd: String,
    file: PathBuf,
    entry: Option<String>,
    args: Option<String>,
    mock: Option<PathBuf>,
    host: Option<String>,
    policy: Option<PathBuf>,
    out: Option<PathBuf>,
    roots: Vec<PathBuf>,
    stats: bool,
    args_file: Option<PathBuf>,
    iters: usize,
    warmup: usize,
    label: Option<String>,
    check: bool,
    write: bool,
    json: bool,
    backend: Option<String>,
}

fn parse_opts() -> Result<Opts, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or(USAGE)?;
    if cmd == "--help" || cmd == "-h" {
        return Err(USAGE.into());
    }
    if cmd == "version" || cmd == "--version" {
        println!("zal 0.1.0 (IR v{})", zal_ir::IR_VERSION);
        std::process::exit(0);
    }
    let mut o = Opts { cmd, file: PathBuf::new(), entry: None, args: None, mock: None, host: None, policy: None, out: None, roots: vec![], stats: false, args_file: None, iters: 30, warmup: 5, label: None, check: false, write: false, json: false, backend: None };
    let mut file = None;
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--entry" => o.entry = Some(val()?),
            "--args" => o.args = Some(val()?),
            "--mock" => o.mock = Some(val()?.into()),
            "--host" => o.host = Some(val()?),
            "--policy" => o.policy = Some(val()?.into()),
            "-o" => o.out = Some(val()?.into()),
            "--root" => o.roots.push(val()?.into()),
            "--stats" => o.stats = true,
            "--args-file" => o.args_file = Some(val()?.into()),
            "--iters" => o.iters = val()?.parse().map_err(|_| "--iters needs a number")?,
            "--warmup" => o.warmup = val()?.parse().map_err(|_| "--warmup needs a number")?,
            "--label" => o.label = Some(val()?),
            "--check" => o.check = true,
            "--write" => o.write = true,
            "--json" => o.json = true,
            "--backend" => o.backend = Some(val()?),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            s => file = Some(PathBuf::from(s)),
        }
    }
    o.file = file.ok_or(format!("missing file\n\n{USAGE}"))?;
    if let Some(dir) = o.file.parent() {
        o.roots.push(if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir.to_path_buf() });
    }
    o.roots.push(PathBuf::from("zal-src"));
    if let Ok(p) = std::env::var("ZAL_PATH") {
        o.roots.extend(std::env::split_paths(&p));
    }
    Ok(o)
}

fn load(o: &Opts) -> Result<Program, String> {
    if o.file.extension().is_some_and(|e| e == "json") {
        let s = std::fs::read_to_string(&o.file).map_err(|e| format!("{}: {e}", o.file.display()))?;
        return Program::from_json(&s);
    }
    zal_compiler::compile_file(&o.file, &o.roots).map_err(|ds| ds.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n"))
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn run_with<H: Host, B: Broker>(prog: &Program, o: &Opts, host: H, broker: B) -> Result<(), String> {
    let entry = o.entry.as_deref().ok_or("--entry <name> is required")?;
    let (_, f) = prog.function(entry).ok_or(format!("no function `{entry}`"))?;
    let text = match &o.args_file {
        Some(f) => read(f)?,
        None => o.args.clone().unwrap_or_else(|| "[]".into()),
    };
    let raw: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("--args: {e}"))?;
    let arr = raw.as_array().ok_or("--args must be a JSON array")?;
    if arr.len() != f.params.len() {
        return Err(format!("`{entry}` takes {} argument(s), got {}", f.params.len(), arr.len()));
    }
    let args = arr.iter().zip(&f.params).map(|(j, (_, t))| from_json(j, t, prog)).collect::<Result<Vec<_>, _>>()?;
    let ret = f.ret.clone();
    // Compiled before the timed run; the module owns the code while the VM runs.
    let jit = match o.backend.as_deref() {
        None | Some("vm") => None,
        Some(name) => Some(zal_cli::backend(name)?.jit(prog)?),
    };
    let mut vm = Vm::new(prog, host, broker);
    if let Some(m) = &jit {
        vm.set_native(m.table());
    }
    vm.echo_print = true;
    if std::env::var("ZAL_PROFILE").is_ok() {
        vm.profile = Some(Default::default());
    }
    let t0 = Instant::now();
    let r = vm.run(entry, args);
    let el = t0.elapsed();
    if o.stats {
        eprintln!(
            "stats: {:.3} ms, {} ops (counted with ZAL_PROFILE=1), {} fibers, {} host calls, max in-flight {}, deferred {}",
            el.as_secs_f64() * 1e3,
            vm.stats.ops,
            vm.stats.fibers,
            vm.stats.host_calls,
            vm.stats.max_inflight,
            vm.stats.deferred
        );
    }
    if let Some(p) = &vm.profile {
        let mut pairs: Vec<_> = p.iter().filter(|((f, _), _)| *f == u32::MAX).collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("profile (top op pairs):");
        for ((_, k), n) in pairs.iter().take(20) {
            eprintln!("  {:>10}  {k}", n);
        }
        let mut by_fn: std::collections::HashMap<u32, u64> = Default::default();
        for ((f, _), n) in p.iter().filter(|((f, _), _)| *f != u32::MAX) {
            *by_fn.entry(*f).or_default() += n;
        }
        let mut rows: Vec<_> = by_fn.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        eprintln!("profile (ops per function):");
        for (f, n) in rows.iter().take(12) {
            eprintln!("  {:>10}  {}", n, prog.functions[*f as usize].name);
        }
        let mut ops: Vec<_> = p.iter().filter(|((f, _), _)| *f != u32::MAX).collect();
        ops.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("profile (top ops):");
        for ((f, k), n) in ops.iter().take(15) {
            eprintln!("  {:>10}  {:<12} {}", n, k, prog.functions[*f as usize].name);
        }
    }
    let v = r.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&to_json(&v, &ret, prog)).unwrap());
    Ok(())
}

fn main_inner() -> Result<(), String> {
    let o = parse_opts()?;
    match o.cmd.as_str() {
        "check" if o.json => check_json(&o),
        "fmt" => fmt_cmd(&o),
        "check" => {
            let p = load(&o)?;
            let n = p.functions.iter().filter(|f| f.kind != FnKind::Lifted).count();
            println!("ok: module {} — {} functions, {} externs", p.module, n, p.externs.len());
            Ok(())
        }
        "build" if o.backend.as_deref().is_some_and(|b| b != "vm") => {
            let p = load(&o)?;
            p.verify()?;
            let out = o.out.clone().ok_or("--backend needs -o <binary>")?;
            let obj = zal_cli::backend(o.backend.as_deref().unwrap())?.object(&p)?;
            zal_cli::aot_link(&obj, &out)?;
            println!("wrote {}", out.display());
            Ok(())
        }
        "build" => {
            let p = load(&o)?;
            // Never write IR that the VM would refuse to load.
            p.verify()?;
            let out = o.out.clone().unwrap_or_else(|| o.file.with_extension("zir.json"));
            std::fs::write(&out, p.to_json()).map_err(|e| format!("{}: {e}", out.display()))?;
            println!("wrote {}", out.display());
            Ok(())
        }
        "ir" => {
            print!("{}", load(&o)?.disassemble());
            Ok(())
        }
        "run" => {
            let p = load(&o)?;
            // No policy file means no grants: fail closed.
            let broker = match &o.policy {
                Some(f) => PolicyBroker::from_json(&read(f)?)?,
                None => PolicyBroker::new(vec![]),
            };
            match (&o.host, &o.mock) {
                (Some(cmd), _) => run_with(&p, &o, JsonlHost::spawn(cmd)?, broker),
                (None, Some(m)) => run_with(&p, &o, MockHost::from_json(&read(m)?)?, broker),
                (None, None) => run_with(&p, &o, MockHost::default(), broker),
            }
        }
        "test" => {
            let p = load(&o)?;
            let mock_cfg = match &o.mock {
                Some(m) => Some(read(m)?),
                None => None,
            };
            let (mut pass, mut fail) = (0, 0);
            for f in p.functions.iter().filter(|f| f.kind == FnKind::Test) {
                let host = match &mock_cfg {
                    Some(c) => MockHost::from_json(c)?,
                    None => MockHost::default(),
                };
                let mut vm = Vm::new(&p, host, PolicyBroker::new(f.effects.clone()));
                match vm.run(&f.name, vec![]) {
                    Ok(_) => {
                        pass += 1;
                        println!("ok    {}", f.name);
                    }
                    Err(e) => {
                        fail += 1;
                        println!("FAIL  {}\n      {}", f.name, e.to_string().replace('\n', "\n      "));
                    }
                }
            }
            println!("\n{pass} passed, {fail} failed");
            if fail > 0 {
                Err(String::new())
            } else {
                Ok(())
            }
        }
        "bench" => bench(&o),
        "native" => native(&o),
        _ => Err(USAGE.into()),
    }
}

/// `zal check --json`: one JSON object per diagnostic, `{"ok":true,...}` on success.
fn check_json(o: &Opts) -> Result<(), String> {
    match zal_compiler::compile_file(&o.file, &o.roots) {
        Ok(p) => {
            let n = p.functions.iter().filter(|f| f.kind != FnKind::Lifted).count();
            println!("{}", serde_json::json!({"ok": true, "module": p.module, "functions": n, "externs": p.externs.len()}));
            Ok(())
        }
        Err(ds) => {
            let mods = zal_compiler::load_modules(&o.file, &o.roots).unwrap_or_default();
            // Written by hand to keep the documented key order.
            for d in &ds {
                println!(
                    "{{\"file\":{},\"line\":{},\"col\":{},\"severity\":\"error\",\"message\":{},\"hint\":{}}}",
                    serde_json::json!(d.file),
                    d.line,
                    d.col,
                    serde_json::json!(d.msg),
                    serde_json::json!(hints::hint(d, &mods))
                );
            }
            Err(String::new())
        }
    }
}

/// `zal fmt`: print the canonical form; `--write` replaces the file, `--check` only reports.
fn fmt_cmd(o: &Opts) -> Result<(), String> {
    let src = read(&o.file)?;
    let out = zal_compiler::fmt::format_source(&src, &o.file.display().to_string()).map_err(|d| d.to_string())?;
    if o.check {
        if out == src {
            return Ok(());
        }
        return Err(format!("{}: would reformat (run `zal fmt --write`)", o.file.display()));
    }
    if o.write {
        if out != src {
            std::fs::write(&o.file, &out).map_err(|e| format!("{}: {e}", o.file.display()))?;
        }
        return Ok(());
    }
    print!("{out}");
    Ok(())
}

/// On-CPU time of this thread in ns (Linux schedstat); robust against a busy host.
fn cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

const NATIVE_MAIN: &str = r#"// Generated by `zal native`: benchmark/run harness around the compiled program.
mod program;
use program::*;

// The ZAL runtime allocator (size-class free lists, see zal-rt alloc.rs).
#[global_allocator]
static ALLOC: zal_rt::ZalAlloc = zal_rt::ZalAlloc;
use std::rc::Rc;
use std::time::Instant;

fn arg(a: &[String], k: &str) -> Option<String> {
    a.iter().position(|x| x == k).and_then(|i| a.get(i + 1).cloned())
}

fn main() {
    // zal_rt::enter allows 10000 nested calls; give synchronous native frames
    // a stack that holds them (reserved lazily), so deep recursion traps at
    // the limit instead of overflowing the 8 MiB main-thread stack.
    let h = std::thread::Builder::new().stack_size(1 << 30).spawn(real_main).expect("main thread");
    if h.join().is_err() {
        std::process::exit(101);
    }
}

fn real_main() {
    let a: Vec<String> = std::env::args().collect();
    let entry_name = arg(&a, "--entry").expect("--entry");
    let args_text = match arg(&a, "--args-file") {
        Some(f) => std::fs::read_to_string(f).expect("args file"),
        None => arg(&a, "--args").unwrap_or_else(|| "[]".into()),
    };
    let args: serde_json::Value = serde_json::from_str(&args_text).expect("args JSON");
    let mock: serde_json::Value = arg(&a, "--mock").map(|f| serde_json::from_str(&std::fs::read_to_string(f).expect("mock")).expect("mock JSON")).unwrap_or(serde_json::json!({}));
    let policy: serde_json::Value = arg(&a, "--policy")
        .map(|f| serde_json::from_str(&std::fs::read_to_string(f).expect("policy")).expect("policy JSON"))
        .unwrap_or(serde_json::Value::Null);
    let grants: Vec<String> = policy
        .get("grant")
        .and_then(|g| g.as_array())
        .map(|g| g.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // Host ceilings, as the VM's PolicyBroker applies them.
    let cap = |k: &str| policy.get("caps").and_then(|c| c.get(k)).and_then(|v| v.as_u64()).filter(|n| *n > 0);
    fn tighter<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        }
    }
    let iters: usize = arg(&a, "--iters").and_then(|v| v.parse().ok()).unwrap_or(1);
    let warmup: usize = arg(&a, "--warmup").and_then(|v| v.parse().ok()).unwrap_or(0);
    let label = arg(&a, "--label").unwrap_or_else(|| entry_name.clone());
    #[cfg(feature = "prof")]
    let guard = pprof::ProfilerGuardBuilder::default().frequency(2000).build().unwrap();
    let mut ms = vec![];
    let mut result = serde_json::Value::Null;
    let mut checksum = 0u64;
    let (start, effs, t, mp, mc) = match entry(&entry_name, &args) {
        Ok(x) => x,
        Err(e) => { eprintln!("{e}"); std::process::exit(2); }
    };
    let t = tighter(t, cap("timeout_ms").and_then(|n| i64::try_from(n).ok()));
    let mp = tighter(mp, cap("max_parallel").and_then(|n| u32::try_from(n).ok()));
    let mc = tighter(mc, cap("max_tool_calls"));
    for i in 0..warmup + iters {
        let host = Rc::new(MockHost::from_json(&mock));
        HOST.with(|h| *h.borrow_mut() = Some(host.clone()));
        let fut = start();
        zal_rt::configure(EFFECTS, &grants, t, mp, mc);
        let t0 = Instant::now();
        let r = zal_rt::preflight(&effs, &grants).and_then(|_| zal_rt::run(fut));
        // The program ends when its entry returns; JSON conversion of the
        // result is output handling and not timed (the baselines do not either).
        let end = zal_rt::take_end().unwrap_or_else(Instant::now);
        let el = end.duration_since(t0).as_secs_f64() * 1e3;
        match r {
            Ok(v) => result = v,
            Err(e) => { eprintln!("runtime error: {}", e.0); std::process::exit(1); }
        }
        checksum = host.checksum.get();
        if i >= warmup { ms.push(el); }
    }
    #[cfg(feature = "prof")]
    {
        let rep = guard.report().build().unwrap();
        let mut m: std::collections::HashMap<String, isize> = Default::default();
        let mut tot = 0;
        for (f, c) in rep.data.iter() {
            tot += *c;
            // Innermost inlined function with its source line, then its callers (ZAL_PROF_DEPTH).
            let depth: usize = std::env::var("ZAL_PROF_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
            let n = f.frames.first().map(|x| x.iter().take(depth).map(|s| format!("{}@{}:{}", s.name().chars().take(60).collect::<String>(), s.filename.as_ref().and_then(|p| p.file_name()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(), s.lineno.unwrap_or(0))).collect::<Vec<_>>().join(" < ")).unwrap_or_default();
            *m.entry(n).or_default() += *c;
        }
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (n, c) in v.iter().take(25) {
            eprintln!("{:5.1}%  {}", 100.0 * *c as f64 / tot as f64, n.chars().take(400).collect::<String>());
        }
    }
    if iters <= 1 && arg(&a, "--iters").is_none() {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        return;
    }
    let mut s = ms.clone();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |x: f64| s[((s.len() as f64 - 1.0) * x).round() as usize];
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    println!("{}", serde_json::json!({"impl": "zal-native", "workload": label, "result": result, "checksum": checksum, "iters": iters, "ms": ms, "p50": q(0.5), "p95": q(0.95), "mean": mean}));
}
"#;

/// `zal native`: generate a Cargo project for the program and build it.
fn native(o: &Opts) -> Result<(), String> {
    let src = zal_compiler::native_source(&o.file, &o.roots).map_err(|ds| ds.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n"))?;
    let repo = std::env::var("ZAL_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    let stem = o.file.file_stem().and_then(|s| s.to_str()).unwrap_or("program").replace(['.', '-'], "_");
    let dir = o.out.clone().unwrap_or_else(|| repo.join("target/zal-native").join(&stem));
    std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
    let rt = repo.join("rt").canonicalize().map_err(|e| format!("zal-rt not found: {e}"))?;
    let manifest = format!(
        "[package]\nname = \"{stem}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\nzal-rt = {{ path = {:?} }}\nserde_json = \"1\"\npprof = {{ version = \"0.14\", optional = true }}\n\n[features]\nprof = [\"dep:pprof\"]\n\n[profile.release]\nlto = \"thin\"\ncodegen-units = 1\n\n[profile.profiling]\ninherits = \"release\"\ndebug = 1\n",
        rt.display().to_string()
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("src/program.rs"), src).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("src/main.rs"), NATIVE_MAIN).map_err(|e| e.to_string())?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "--release", "--quiet"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", repo.join("target/native-build"))
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err(format!("native build failed (generated source: {})", dir.join("src/program.rs").display()));
    }
    let bin = repo.join("target/native-build/release").join(&stem);
    println!("{}", bin.display());
    Ok(())
}

fn bench(o: &Opts) -> Result<(), String> {
    let p = load(o)?;
    let entry = o.entry.as_deref().ok_or("--entry <name> is required")?;
    let (_, f) = p.function(entry).ok_or(format!("no function `{entry}`"))?;
    let raw = match &o.args_file {
        Some(file) => read(file)?,
        None => o.args.clone().unwrap_or_else(|| "[]".into()),
    };
    let j: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("args: {e}"))?;
    let arr = j.as_array().ok_or("args must be a JSON array")?;
    // Marshal once: the timed region is only the VM run, like the baselines.
    let args = arr.iter().zip(&f.params).map(|(j, (_, t))| from_json(j, t, &p)).collect::<Result<Vec<_>, _>>()?;
    let mock = read(o.mock.as_ref().ok_or("--mock is required")?)?;
    let policy = read(o.policy.as_ref().ok_or("--policy is required")?)?;
    let mut ms = Vec::new();
    let mut cpu = Vec::new();
    let mut result = serde_json::Value::Null;
    let mut checksum = 0;
    let jit = match o.backend.as_deref() {
        None | Some("vm") => None,
        Some(name) => Some(zal_cli::backend(name)?.jit(&p)?),
    };
    // ZAL_BENCH_FRESH_VM=1 restores the old harness (a new VM per iteration).
    // Default: one long-lived VM per bench, like the baselines keep their
    // runtime (tokio) and registry across iterations; buffers are reused.
    let fresh = std::env::var("ZAL_BENCH_FRESH_VM").is_ok_and(|v| v == "1");
    let mut vm = Vm::new(&p, MockHost::from_json(&mock)?, PolicyBroker::from_json(&policy)?);
    if let Some(m) = &jit {
        vm.set_native(m.table());
    }
    for i in 0..o.warmup + o.iters {
        if fresh {
            vm = Vm::new(&p, MockHost::from_json(&mock)?, PolicyBroker::from_json(&policy)?);
            if let Some(m) = &jit {
                vm.set_native(m.table());
            }
        }
        vm.host.checksum = 0;
        let c0 = cpu_ns();
        let t0 = Instant::now();
        let v = vm.run(entry, args.clone()).map_err(|e| e.to_string())?;
        let el = t0.elapsed().as_secs_f64() * 1e3;
        let cel = (cpu_ns() - c0) as f64 / 1e6;
        if i >= o.warmup {
            ms.push(el);
            cpu.push(cel);
        }
        result = to_json(&v, &f.ret, &p);
        checksum = vm.host.checksum;
    }
    let mut sorted = ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |x: f64| sorted[((sorted.len() as f64 - 1.0) * x).round() as usize];
    let mean = ms.iter().sum::<f64>() / ms.len().max(1) as f64;
    let mut cs = cpu.clone();
    cs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let cpu_p50 = cs[cs.len() / 2];
    let cpu_min = cs[0];
    let out = serde_json::json!({
        "impl": "zal",
        "workload": o.label.clone().unwrap_or_else(|| o.args_file.as_ref().map(|f| f.display().to_string()).unwrap_or_default()),
        "result": result,
        "checksum": checksum,
        "iters": o.iters,
        "ms": ms,
        "p50": q(0.5),
        "p95": q(0.95),
        "mean": mean,
        "cpu_p50": cpu_p50,
        "cpu_min": cpu_min,
        "cpu_mean": cpu.iter().sum::<f64>() / cpu.len() as f64,
    });
    println!("{out}");
    Ok(())
}

fn main() -> ExitCode {
    match main_inner() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            ExitCode::FAILURE
        }
    }
}
