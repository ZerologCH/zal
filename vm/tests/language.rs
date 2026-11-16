// End-to-end language tests: source → compiler → IR → VM.
// (Plain comments: language_native*.rs include this file inside a module.)

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zal_vm::host::{MockHost, MockTool, PolicyBroker};
// Glob on purpose: language_native*.rs shadow `Vm` with a wrapper that runs
// every function through the native ABI.
#[allow(unused_imports)]
use zal_vm::*;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn compile(src: &str) -> zal_ir::Program {
    zal_compiler::compile_source(src, "test.zal").unwrap_or_else(|ds| panic!("{}", ds.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n")))
}

fn run(src: &str, entry: &str, args: Vec<Value>, host: MockHost, grants: &[&str]) -> Result<Value, String> {
    let p = compile(src);
    let mut vm = Vm::new(&p, host, PolicyBroker::new(grants.iter().map(|s| s.to_string()).collect()));
    vm.run(entry, args).map_err(|e| e.msg)
}

fn tool(latency_ms: u64) -> MockTool {
    MockTool { latency: Duration::from_millis(latency_ms), ..Default::default() }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        v => panic!("expected Int, got {v}"),
    }
}

#[test]
fn golden_compile_errors() {
    let dir = root().join("tests/golden/errors");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let path = e.unwrap().path();
        let src = std::fs::read_to_string(&path).unwrap();
        let want = src.lines().next().and_then(|l| l.strip_prefix("// expect-error: ")).unwrap_or_else(|| panic!("{} lacks expect-error", path.display()));
        match zal_compiler::compile_source(&src, "t.zal") {
            Ok(_) => panic!("{} compiled but should fail with: {want}", path.display()),
            Err(ds) => {
                let got = ds.iter().map(|d| d.msg.clone()).collect::<Vec<_>>().join(" | ");
                assert!(got.contains(want), "{}:\n  want: {want}\n  got:  {got}", path.display());
            }
        }
        n += 1;
    }
    assert!(n >= 10, "expected golden cases, found {n}");
}

#[test]
fn examples_compile() {
    for f in ["examples/build_verify.zal", "examples/dag_pipeline.zal"] {
        let p = root().join(f);
        zal_compiler::compile_file(&p, &[root().join("zal-src"), p.parent().unwrap().to_path_buf()])
            .unwrap_or_else(|ds| panic!("{f}: {}", ds[0]));
    }
}

#[test]
fn ir_roundtrips_through_json() {
    let p = compile("module t\nfn f(x: Int) -> Int { x * 2 + 1 }");
    let q = zal_ir::Program::from_json(&p.to_json()).unwrap();
    let mut vm = Vm::new(&q, MockHost::default(), PolicyBroker::new(vec![]));
    assert_eq!(int(&vm.run("f", vec![Value::Int(20)]).unwrap()), 41);
}

#[test]
fn ir_version_mismatch_is_refused() {
    let p = compile("module t\nfn f() -> Int { 1 }");
    let s = p.to_json().replace("\"ir_version\": 1", "\"ir_version\": 999");
    assert!(zal_ir::Program::from_json(&s).unwrap_err().contains("not supported"));
}

#[test]
fn control_flow_and_data() {
    let src = r#"
module t
type P = { name: Str, n: Int }
enum Shape { Dot, Sq(Int) }
fn area(s: Shape) -> Int { match s { Shape.Dot => 0, Shape.Sq(k) => k * k } }
fn f() -> Int {
    let mut total = 0
    for i in range(10) {
        if i % 2 == 0 { continue }
        if i > 7 { break }
        total = total + i
    }
    let mut xs: List<Int> = []
    let mut j = 0
    while j < 5 { xs = push(xs, j)
        j = j + 1 }
    let m = map_set(map_new(), "a", P { name: "x", n: 4 })
    let p = match map_get(m, "a") { Some(p) => p.n, None => 0 }
    total + len(xs) + p + area(Shape.Sq(3)) + area(Shape.Dot)
}"#;
    // 1+3+5+7 = 16, +5, +4, +9
    assert_eq!(int(&run(src, "f", vec![], MockHost::default(), &[]).unwrap()), 34);
}

#[test]
fn try_and_ensure_propagate_errors() {
    let src = r#"
module t
fn half(x: Int) -> Result<Int, Str> {
    ensure x % 2 == 0, "odd: " + str(x)
    Ok(x / 2)
}
fn quarter(x: Int) -> Result<Int, Str> { let h = half(x)?
    half(h) }
fn f(x: Int) -> Str { match quarter(x) { Ok(v) => str(v), Err(e) => e } }
"#;
    let r = |x| match run(src, "f", vec![Value::Int(x)], MockHost::default(), &[]).unwrap() {
        Value::Str(s) => s.to_string(),
        v => panic!("{v}"),
    };
    assert_eq!(r(12), "3");
    assert_eq!(r(6), "odd: 3");
    assert_eq!(r(5), "odd: 5");
}

#[test]
fn parallel_branches_overlap() {
    let src = r#"
module t
extern tool slow(x: Int) -> Int
task f() -> Int requires tool.slow {
    let (a, b, c) = parallel { slow(1), slow(2), slow(3) }
    a + b + c
}"#;
    let mut h = MockHost::default();
    h.tools.insert("slow".into(), MockTool { echo: true, ..tool(50) });
    let t0 = Instant::now();
    assert_eq!(int(&run(src, "f", vec![], h, &["tool.slow"]).unwrap()), 6);
    let el = t0.elapsed();
    assert!(el < Duration::from_millis(120), "branches ran sequentially: {el:?}");
}

#[test]
fn race_returns_first_and_cancels_rest() {
    let src = r#"
module t
extern tool fast(x: Int) -> Int
extern tool slow(x: Int) -> Int
task f() -> Int requires tool.fast, tool.slow { race { slow(1), fast(2) } }
"#;
    let mut h = MockHost::default();
    h.tools.insert("fast".into(), MockTool { echo: true, ..tool(10) });
    h.tools.insert("slow".into(), MockTool { echo: true, ..tool(500) });
    let t0 = Instant::now();
    assert_eq!(int(&run(src, "f", vec![], h, &["tool.fast", "tool.slow"]).unwrap()), 2);
    assert!(t0.elapsed() < Duration::from_millis(300), "loser was not cancelled");
}

#[test]
fn timeout_yields_none_and_some() {
    let src = r#"
module t
extern tool slow(x: Int) -> Int
task f(limit: Dur) -> Int requires tool.slow {
    match timeout(limit) { slow(7) } { Some(v) => v, None => -1 }
}"#;
    let mk = || {
        let mut h = MockHost::default();
        h.tools.insert("slow".into(), MockTool { echo: true, ..tool(100) });
        h
    };
    assert_eq!(int(&run(src, "f", vec![Value::Dur(20)], mk(), &["tool.slow"]).unwrap()), -1);
    assert_eq!(int(&run(src, "f", vec![Value::Dur(1000)], mk(), &["tool.slow"]).unwrap()), 7);
}

#[test]
fn retry_recovers_from_flaky_tool() {
    let src = r#"
module t
extern tool flaky(x: Int) -> Result<Int, Str>
task f(n: Int) -> Result<Int, Str> requires tool.flaky { retry(n) { flaky(5) } }
"#;
    let mk = || {
        let mut h = MockHost::default();
        h.tools.insert("flaky".into(), MockTool { echo: true, fail_first: 2, ..Default::default() });
        h
    };
    assert_eq!(run(src, "f", vec![Value::Int(3)], mk(), &["tool.flaky"]).unwrap(), Value::Variant(0, Some(Value::Int(5).into())));
    assert!(matches!(run(src, "f", vec![Value::Int(2)], mk(), &["tool.flaky"]).unwrap(), Value::Variant(1, _)));
}

#[test]
fn spawn_await_any_and_join() {
    let src = r#"
module t
extern tool work(x: Int) -> Int
task f() -> Int requires tool.work {
    let ts = [spawn work(30), spawn work(10), spawn work(20)]
    let (i, first) = await_any(ts)
    let all = join(ts)
    ensure_same(all[i], first)
    len(all) * 1000 + all[0] + all[1] + all[2]
}
fn ensure_same(a: Int, b: Int) -> Int { assert a == b, "await_any index/value mismatch"
    a }"#;
    let mut h = MockHost::default();
    h.tools.insert("work".into(), MockTool { echo: true, ..tool(10) });
    assert_eq!(int(&run(src, "f", vec![], h, &["tool.work"]).unwrap()), 3060);
}

#[test]
fn capability_denied_fails_closed() {
    let src = r#"
module t
extern tool deploy(x: Str) -> Result<Str, Str>
workflow w() -> Result<Str, Str> requires tool.deploy { deploy("prod") }
"#;
    let e = run(src, "w", vec![], MockHost::default(), &[]).unwrap_err();
    assert!(e.contains("preflight denied"), "{e}");
}

#[test]
fn wildcard_request_is_still_checked_per_call() {
    let src = r#"
module t
extern tool deploy(x: Str) -> Result<Str, Str>
extern tool build(x: Str) -> Result<Str, Str>
workflow w() -> Result<Str, Str> requires tool.* { build("x")?
    deploy("prod") }
"#;
    let mut h = MockHost::default();
    h.record = true;
    let e = run(src, "w", vec![], h, &["tool.build"]).unwrap_err();
    assert!(e.contains("capability denied for `deploy`"), "{e}");
}

#[test]
fn budget_limits_tool_calls() {
    let src = r#"
module t
extern tool ping(x: Int) -> Int
workflow w() -> Int requires tool.ping budget { max_tool_calls: 3 } {
    let mut s = 0
    for i in range(5) { s = s + ping(i) }
    s
}"#;
    let e = run(src, "w", vec![], MockHost::default(), &["tool.ping"]).unwrap_err();
    assert!(e.contains("more than 3 tool calls"), "{e}");
}

#[test]
fn budget_timeout_aborts_run() {
    let src = r#"
module t
extern tool slow(x: Int) -> Int
workflow w() -> Int requires tool.slow budget { timeout: 30ms } { slow(1) }
"#;
    let mut h = MockHost::default();
    h.tools.insert("slow".into(), tool(500));
    let t0 = Instant::now();
    let e = run(src, "w", vec![], h, &["tool.slow"]).unwrap_err();
    assert!(e.contains("budget exceeded: timeout"), "{e}");
    assert!(t0.elapsed() < Duration::from_millis(300));
}

#[test]
fn max_parallel_applies_backpressure() {
    let src = r#"
module t
extern tool slow(x: Int) -> Int
workflow w() -> Int requires tool.slow budget { max_parallel: 2 } {
    let (a, b, c, d) = parallel { slow(1), slow(1), slow(1), slow(1) }
    a + b + c + d
}"#;
    let p = compile(src);
    let mut h = MockHost::default();
    h.tools.insert("slow".into(), MockTool { echo: true, ..tool(20) });
    let mut vm = Vm::new(&p, h, PolicyBroker::new(vec!["tool.slow".into()]));
    let t0 = Instant::now();
    assert_eq!(int(&vm.run("w", vec![]).unwrap()), 4);
    assert_eq!(vm.stats.max_inflight, 2);
    assert!(vm.stats.deferred >= 2);
    assert!(t0.elapsed() >= Duration::from_millis(38), "4 calls at 2-wide should take two rounds");
}

#[test]
fn awaiting_a_cancelled_task_traps() {
    let src = r#"
module t
task f() -> Int {
    let t = spawn { sleep(1ms)
        1 }
    cancel(t)
    await t
}"#;
    let e = run(src, "f", vec![], MockHost::default(), &[]).unwrap_err();
    assert!(e.contains("cancelled"), "{e}");
}

#[test]
fn runtime_errors_carry_a_trace() {
    let src = "module t\nfn g(xs: List<Int>) -> Int { xs[5] }\nfn f() -> Int { g([1, 2]) }";
    let p = compile(src);
    let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
    let e = vm.run("f", vec![]).unwrap_err();
    assert!(e.msg.contains("out of bounds"));
    assert_eq!(e.trace[0], "g (line 2)");
    assert_eq!(e.trace[1], "f (line 3)");
}

#[test]
fn integer_overflow_traps() {
    let e = run("module t\nfn f(x: Int) -> Int { x * x }", "f", vec![Value::Int(i64::MAX)], MockHost::default(), &[]).unwrap_err();
    assert!(e.contains("overflow"), "{e}");
}

#[test]
fn in_place_updates_do_not_copy() {
    // 200k pushes would take minutes if every push cloned the list.
    let src = "module t\nfn f() -> Int { let mut xs: List<Int> = []\n for i in range(200000) { xs = push(xs, i + len(xs)) }\n len(xs) }";
    let t0 = Instant::now();
    assert_eq!(int(&run(src, "f", vec![], MockHost::default(), &[]).unwrap()), 200_000);
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn zal_scheduler_runs_dag_in_dependency_order() {
    let p = zal_compiler::compile_file(&root().join("examples/dag_pipeline.zal"), &[root().join("zal-src")]).unwrap();
    let mut h = MockHost::default();
    h.record = true;
    h.tools.insert("dispatch".into(), MockTool { echo: true, echo_arg: 1, ..Default::default() });
    let mut vm = Vm::new(&p, h, PolicyBroker::new(vec!["tool.dispatch".into()]));
    let v = vm.run("pipeline", vec![]).unwrap();
    assert_eq!(v, Value::Variant(0, Some(zal_vm::value::str_val("preview").into())));
    assert_eq!(vm.stats.host_calls, 5);
}

#[test]
fn fuzz_compiler_never_panics() {
    // Deterministic mutation fuzzing: delete, duplicate and swap byte ranges of
    // real programs. The compiler must return diagnostics, never panic.
    let seeds: Vec<String> = ["examples/build_verify.zal", "examples/dag_pipeline.zal", "zal-src/scheduler/dag.zal"]
        .iter()
        .map(|f| std::fs::read_to_string(root().join(f)).unwrap())
        .collect();
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut next = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n.max(1) as u64) as usize
    };
    let mut compiled = 0;
    for i in 0..3000 {
        let mut b: Vec<char> = seeds[i % seeds.len()].chars().collect();
        for _ in 0..1 + next(4) {
            let (a, len) = (next(b.len()), 1 + next(12));
            let end = (a + len).min(b.len());
            match next(3) {
                0 => {
                    b.drain(a..end);
                }
                1 => {
                    let chunk: Vec<char> = b[a..end].to_vec();
                    let at = next(b.len());
                    b.splice(at..at, chunk);
                }
                _ => {
                    let c = b"{}()[]<>,:;.=+-*/%!?\"x0_\n"[next(25)] as char;
                    if a < b.len() {
                        b[a] = c;
                    }
                }
            }
        }
        let src: String = b.into_iter().collect();
        let r = std::panic::catch_unwind(|| zal_compiler::compile_source(&src.replace("import scheduler.dag", ""), "fuzz.zal").is_ok());
        match r {
            Ok(ok) => compiled += ok as usize,
            Err(_) => panic!("compiler panicked on mutation {i}:\n{src}"),
        }
    }
    eprintln!("fuzz: {compiled}/3000 mutants still compiled");
}

#[test]
fn jsonl_host_protocol_roundtrip() {
    if std::process::Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not installed; skipping");
        return;
    }
    let root = root();
    let p = zal_compiler::compile_file(&root.join("examples/hosts/jsonl_demo.zal"), &[]).unwrap();
    let host = zal_vm::host::JsonlHost::spawn(&format!("node {}", root.join("examples/hosts/echo-host.mjs").display())).unwrap();
    let mut vm = Vm::new(&p, host, PolicyBroker::new(vec!["tool.upper".into()]));
    let v = vm.run("demo", vec![]).unwrap();
    assert_eq!(v, Value::Variant(0, Some(zal_vm::value::str_val("left+right").into())));
}

#[test]
fn value_and_op_sizes_stay_small() {
    // Guard against accidental growth of the two hottest types.
    eprintln!("Value={} Op={}", std::mem::size_of::<Value>(), std::mem::size_of::<zal_ir::Op>());
    assert!(std::mem::size_of::<Value>() <= 24);
    assert!(std::mem::size_of::<zal_ir::Op>() <= 8);
}

#[test]
fn differential_fused_vs_raw_bytecode() {
    // The peephole pass must not change behaviour: run the ZAL scheduler on a
    // real workload with and without fusion and compare result and work done.
    let root = root();
    let wl: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("benchmarks/workloads/w2-medium.json")).unwrap()).unwrap();
    let mut outs = vec![];
    for fuse in [true, false] {
        let p = zal_compiler::compile_file_opts(&root.join("examples/dag_pipeline.zal"), &[root.join("zal-src")], fuse).unwrap();
        let (_, f) = p.function("run_graph").unwrap();
        let args: Vec<Value> = wl.as_array().unwrap().iter().zip(&f.params).map(|(j, (_, t))| zal_vm::value::from_json(j, t, &p).unwrap()).collect();
        let mut h = MockHost::default();
        h.checksum_arg = Some(2);
        h.tools.insert("dispatch".into(), MockTool { echo: true, echo_arg: 1, ..Default::default() });
        let mut vm = Vm::new(&p, h, PolicyBroker::new(vec!["tool.dispatch".into()]));
        let v = vm.run("run_graph", args).unwrap();
        outs.push((v.to_string(), vm.host.checksum, vm.stats.host_calls));
    }
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0].1, 24180);
}

#[test]
fn map_hasher_spreads_similar_keys() {
    // Regression: a hasher with weak low bits made sequential keys collide and
    // a 1M-insert loop 13x slower. Bucket indices come from the low bits.
    use std::hash::{BuildHasher, BuildHasherDefault};
    let b = BuildHasherDefault::<zal_vm::value::FxHasher>::default();
    let mut buckets = std::collections::HashSet::new();
    for i in 0..4096 {
        buckets.insert(b.hash_one(format!("n{i}").as_str()) & 4095);
    }
    assert!(buckets.len() > 2400, "only {} of 4096 buckets used", buckets.len());
}

#[test]
fn verifier_rejects_malformed_bytecode() {
    // The interpreter skips bounds checks because of this proof, so broken
    // IR must never reach it.
    let base = compile("module t\nfn f(x: Int) -> Int { x + 1 }");
    let cases: Vec<(&str, Box<dyn Fn(&mut zal_ir::Program)>)> = vec![
        ("underflow", Box::new(|p| p.functions[0].code.insert(0, zal_ir::Op::Pop))),
        ("local out of range", Box::new(|p| p.functions[0].code[0] = zal_ir::Op::LoadLocal(99))),
        ("bad jump", Box::new(|p| { let n = p.functions[0].code.len(); p.functions[0].code[n - 1] = zal_ir::Op::Jump(999) })),
        ("runs off the end", Box::new(|p| { p.functions[0].code.pop(); p.functions[0].lines.pop(); })),
        ("bad constant", Box::new(|p| p.functions[0].code.insert(0, zal_ir::Op::Const(999)))),
    ];
    for (name, mutate) in cases {
        let mut p = base.clone();
        mutate(&mut p);
        if p.functions[0].lines.len() != p.functions[0].code.len() {
            let n = p.functions[0].code.len();
            p.functions[0].lines.resize(n, 1);
        }
        let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
        let e = vm.run("f", vec![Value::Int(1)]).unwrap_err();
        assert!(e.msg.starts_with("IR verify"), "{name}: {}", e.msg);
    }
}

const AUTOPAR: &str = r#"
module t
extern tool read(path: Str) -> Result<Str, Str> readonly
extern tool write(path: Str) -> Result<Str, Str>
task independent() -> Result<Str, Str> requires tool.read {
    let a = read("a")?
    let b = read("b")?
    let c = read("c")?
    let d = read("d")?
    Ok(a + b + c + d)
}
task dependent() -> Result<Str, Str> requires tool.read {
    let a = read("a")?
    let b = read(a)?
    Ok(b)
}
task writes() -> Result<Str, Str> requires tool.write {
    let a = write("a")?
    let b = write("b")?
    Ok(a + b)
}
task looped(files: List<Str>) -> Result<Int, Str> requires tool.read {
    let mut acc: List<Str> = []
    for f in files { acc = push(acc, read(f)?) }
    let mut total = 0
    for x in acc { total = total + len(x) }
    Ok(total * 100 + len(acc))
}
task order(files: List<Str>) -> Result<Str, Str> requires tool.read {
    let mut acc: List<Str> = []
    for f in files { acc = push(acc, read(f)?) }
    let mut s = ""
    for x in acc { s = s + x }
    Ok(s)
}
"#;

fn ro_host(latency: u64, fail_first: u32) -> MockHost {
    let mut h = MockHost::default();
    h.record = true;
    for t in ["read", "write"] {
        h.tools.insert(t.into(), MockTool { echo: true, latency: Duration::from_millis(latency), fail_first, ..Default::default() });
    }
    h
}

fn timed(entry: &str, args: Vec<Value>, host: MockHost) -> (Result<Value, String>, Duration) {
    let p = compile(AUTOPAR);
    let mut vm = Vm::new(&p, host, PolicyBroker::new(vec!["tool.read".into(), "tool.write".into()]));
    let t = Instant::now();
    let r = vm.run(entry, args).map_err(|e| e.msg);
    (r, t.elapsed())
}

#[test]
fn autopar_runs_independent_readonly_calls_concurrently() {
    let (r, el) = timed("independent", vec![], ro_host(40, 0));
    assert_eq!(r.unwrap(), Value::Variant(0, Some(zal_vm::value::str_val("abcd").into())));
    assert!(el < Duration::from_millis(120), "4 × 40 ms ran sequentially: {el:?}");
}

#[test]
fn autopar_keeps_dependent_and_effectful_calls_sequential() {
    let (_, el) = timed("dependent", vec![], ro_host(40, 0));
    assert!(el >= Duration::from_millis(78), "dependent call must wait: {el:?}");
    let (_, el) = timed("writes", vec![], ro_host(40, 0));
    assert!(el >= Duration::from_millis(78), "non-readonly calls must stay ordered: {el:?}");
}

#[test]
fn autopar_loop_collects_in_list_order_concurrently() {
    let files: Vec<Value> = (0..8).map(|i| zal_vm::value::str_val(&format!("f{i}"))).collect();
    let list = || Value::List(std::rc::Rc::new(files.clone()));
    let (r, el) = timed("looped", vec![list()], ro_host(40, 0));
    assert_eq!(r.unwrap(), Value::Variant(0, Some(Value::Int(16 * 100 + 8).into())));
    assert!(el < Duration::from_millis(160), "8 × 40 ms ran sequentially: {el:?}");
    let (r, _) = timed("order", vec![list()], ro_host(0, 0));
    assert_eq!(r.unwrap(), Value::Variant(0, Some(zal_vm::value::str_val("f0f1f2f3f4f5f6f7").into())));
}

#[test]
fn autopar_preserves_early_failure() {
    // The first read fails: the function returns that error, like sequential code.
    let (r, _) = timed("independent", vec![], ro_host(10, 1));
    assert!(matches!(r.unwrap(), Value::Variant(1, _)));
}

#[test]
fn fuzz_ir_verifier_guards_unchecked_execution() {
    // The interpreter skips bounds checks on the operand stack and locals
    // because the verifier proved them safe. Mutate real programs' bytecode at
    // random; whatever the verifier accepts is executed (with a time budget).
    // In debug builds the interpreter's debug_asserts turn any unproven access
    // into a panic, so a verifier gap fails this test instead of hiding.
    let sources: Vec<String> = ["examples/build_verify.zal", "examples/dag_pipeline.zal", "zal-src/scheduler/dag.zal"]
        .iter()
        .map(|f| f.to_string())
        .collect();
    let mut progs = vec![];
    for f in &sources {
        let p = root().join(f);
        progs.push(zal_compiler::compile_file(&p, &[root().join("zal-src"), p.parent().unwrap().to_path_buf()]).unwrap());
    }
    let mut x: u64 = 0xDEADBEEFCAFEF00D;
    let mut rnd = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n.max(1) as u64) as usize
    };
    let (mut accepted, mut rejected) = (0, 0);
    for i in 0..1500 {
        let mut p = progs[i % progs.len()].clone();
        let fi = rnd(p.functions.len());
        let f = &mut p.functions[fi];
        if f.code.is_empty() {
            continue;
        }
        let at = rnd(f.code.len());
        let l = f.locals;
        let n = f.code.len() as u32;
        // Plausible-looking mutations: tweak operands, swap ops, insert/delete.
        let ops = [
            zal_ir::Op::Pop, zal_ir::Op::Add, zal_ir::Op::LoadLocal(rnd(l as usize + 2) as u16), zal_ir::Op::StoreLocal(rnd(l as usize + 2) as u16),
            zal_ir::Op::Jump(rnd(n as usize + 2) as u32), zal_ir::Op::JumpIfFalse(rnd(n as usize + 2) as u32), zal_ir::Op::MakeTuple(rnd(4) as u16),
            zal_ir::Op::TupleGet(rnd(3) as u16), zal_ir::Op::Ret, zal_ir::Op::IndexLL(rnd(l as usize + 2) as u16, rnd(l as usize + 2) as u16),
            zal_ir::Op::ForNext(rnd(l as usize + 2) as u16, rnd(n as usize + 2) as u32), zal_ir::Op::UpdLocal(zal_ir::Builtin::Push, rnd(l as usize + 2) as u16),
            zal_ir::Op::SplitTuple(rnd(4) as u16), zal_ir::Op::Unit, zal_ir::Op::LoadLocal2(rnd(l as usize + 2) as u16, rnd(l as usize + 2) as u16),
            zal_ir::Op::Builtin(zal_ir::Builtin::Len, rnd(3) as u16), zal_ir::Op::Builtin(zal_ir::Builtin::MapSet, rnd(4) as u16),
            zal_ir::Op::Builtin(zal_ir::Builtin::Push, rnd(3) as u16), zal_ir::Op::FieldGet(rnd(4) as u16), zal_ir::Op::LoadField(rnd(l as usize + 2) as u16, rnd(4) as u16),
            zal_ir::Op::ForNext(u16::MAX - rnd(2) as u16, rnd(n as usize + 2) as u32), zal_ir::Op::JumpIfTrue(rnd(n as usize + 2) as u32),
        ];
        match rnd(3) {
            0 => f.code[at] = ops[rnd(ops.len())],
            1 => {
                f.code.insert(at, ops[rnd(ops.len())]);
                f.lines.insert(at, 1);
            }
            _ => {
                if f.code.len() > 1 {
                    f.code.remove(at);
                    f.lines.remove(at);
                }
            }
        }
        if p.verify().is_err() {
            rejected += 1;
            continue;
        }
        accepted += 1;
        // Run every entry-like function with zero-valued args under a short budget.
        let entry = p.functions.iter().find(|f| f.params.is_empty() && f.kind != zal_ir::FnKind::Lifted).map(|f| f.name.clone());
        if let Some(e) = entry {
            let mut h = MockHost::default();
            h.tools.insert("model".into(), MockTool { suffix: Some(":ok".into()), ..Default::default() });
            let mut vm = Vm::new(&p, h, PolicyBroker::new(vec!["tool.*".into(), "worker.*".into()]));
            // Mutated jumps make endless loops; bound every run.
            vm.set_time_limit(Duration::from_millis(30));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run(&e, vec![])));
            if let Err(panic) = r {
                let msg = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                // Type confusion in accepted IR may hit "internal:" traps (fine),
                // but a debug_assert / index panic means the verifier let an
                // unsafe access through.
                panic!("mutant {i} passed the verifier but panicked: {msg}");
            }
        }
    }
    eprintln!("ir fuzz: {accepted} accepted, {rejected} rejected");
    assert!(accepted > 50 && rejected > 50);
}

#[test]
fn crafted_ir_cannot_rebind_an_extern_to_another_effect() {
    // An IR file that calls `deploy` but labels it with the granted `tool.read`
    // effect must be refused before anything runs.
    let src = "module t\nextern tool read(p: Str) -> Result<Str, Str>\nextern deploy prod(p: Str) -> Result<Str, Str>\nworkflow w() -> Result<Str, Str> requires deploy.prod { prod(\"x\") }";
    let mut p = compile(src);
    let i = p.externs.iter().position(|x| x.name == "prod").unwrap();
    p.externs[i].effect = "tool.read".into();
    for f in &mut p.functions {
        f.effects = vec!["tool.read".into()];
    }
    let mut h = MockHost::default();
    h.record = true;
    let mut vm = Vm::new(&p, h, PolicyBroker::new(vec!["tool.read".into()]));
    let e = vm.run("w", vec![]).unwrap_err();
    assert!(e.msg.contains("expected `<kind>.prod`"), "{}", e.msg);
    assert!(vm.host.log.is_empty(), "no host call may happen");
}

#[test]
fn return_break_continue_directly_in_match_arms() {
    let src = r#"
module t
fn first_even(xs: List<Int>) -> Option<Int> {
    for x in xs {
        match x % 2 {
            0 => return Some(x),
            _ => continue,
        }
    }
    None
}
fn f() -> Int {
    let mut n = 0
    while true {
        match n {
            5 => break,
            _ => { n = n + 1 }
        }
    }
    n * 10 + match first_even([1, 3, 4, 6]) { Some(v) => v, None => 0 }
}"#;
    assert_eq!(int(&run(src, "f", vec![], MockHost::default(), &[]).unwrap()), 54);
}

#[test]
fn verifier_checks_builtin_arity() {
    // The VM pops a builtin's operands unchecked; a lying argc in crafted IR
    // would read below the frame.
    let mut p = compile("module t\nfn f(xs: List<Int>) -> Int { len(xs) }");
    let f = p.functions.iter_mut().find(|f| f.name == "f").unwrap();
    for op in &mut f.code {
        match op {
            zal_ir::Op::Builtin(b, _) => *op = zal_ir::Op::Builtin(*b, 0),
            zal_ir::Op::LenLocal(_) => *op = zal_ir::Op::Builtin(zal_ir::Builtin::Len, 0),
            _ => {}
        }
    }
    let e = p.verify().unwrap_err();
    assert!(e.contains("expects 1"), "{e}");
}

#[test]
fn split_of_a_wrong_sized_tuple_traps() {
    // Host-supplied arguments are not type-checked; a short tuple must trap,
    // not underrun the unchecked stack.
    let src = "module t\nfn f(t: (Int, Int, Int)) -> Int { let (a, b, c) = t\n a + b + c }";
    for xs in [vec![Value::Int(1), Value::Int(2)], vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]] {
        let e = run(src, "f", vec![Value::Tuple(std::rc::Rc::from(xs))], MockHost::default(), &[]).unwrap_err();
        assert!(e.contains("split of"), "{e}");
    }
    assert_eq!(int(&run(src, "f", vec![Value::Tuple(std::rc::Rc::from(vec![Value::Int(1), Value::Int(2), Value::Int(3)]))], MockHost::default(), &[]).unwrap()), 6);
}

#[test]
fn field_index_out_of_range_traps() {
    let src = "module t\ntype P = { a: Int, b: Int }\nfn f(p: P) -> Int { p.b }";
    let e = run(src, "f", vec![Value::Record(std::rc::Rc::from(vec![Value::Int(1)]))], MockHost::default(), &[]).unwrap_err();
    assert!(e.contains("field index"), "{e}");
}

#[test]
fn conditional_back_edge_cannot_escape_the_time_limit() {
    // Jump threading (or a crafted file) can close a loop with a conditional
    // jump; it must tick like an unconditional back edge.
    let mut p = compile("module t\nfn f() -> Int { 0 }");
    let c = p.consts.len() as u32;
    p.consts.push(zal_ir::Const::Bool(true));
    let f = p.functions.iter_mut().find(|f| f.name == "f").unwrap();
    f.code = vec![zal_ir::Op::Const(c), zal_ir::Op::JumpIfTrue(0), zal_ir::Op::Unit, zal_ir::Op::Ret];
    f.lines = vec![1; 4];
    p.verify().unwrap();
    let mut vm = Vm::new(&p, MockHost::default(), PolicyBroker::new(vec![]));
    vm.set_time_limit(Duration::from_millis(50));
    let t = Instant::now();
    let e = vm.run("f", vec![]).unwrap_err();
    assert!(e.msg.contains("timeout"), "{}", e.msg);
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn jsonl_host_garbage_fails_pending_calls_instead_of_hanging() {
    let src = "module t\nextern tool up(s: Str) -> Result<Str, Str>\nworkflow w() -> Result<Str, Str> requires tool.up { up(\"x\") }";
    let p = compile(src);
    for reply in ["not json", "{\"ok\":\"x\"}", "{\"id\":999,\"ok\":\"x\"}"] {
        let host = zal_vm::host::JsonlHost::spawn(&format!("read l; echo '{reply}'; sleep 10")).unwrap();
        let mut vm = Vm::new(&p, host, PolicyBroker::new(vec!["tool.up".into()]));
        let t = Instant::now();
        let r = vm.run("w", vec![]);
        assert!(t.elapsed() < Duration::from_secs(5), "{reply}: hung");
        let msg = match r {
            Ok(Value::Variant(1, Some(e))) => e.to_string(),
            Ok(v) => panic!("{reply}: unexpected {v}"),
            Err(e) => e.msg,
        };
        assert!(msg.contains("host protocol error"), "{reply}: {msg}");
    }
}

#[test]
fn budget_edge_cases_are_rejected() {
    for (src, want) in [
        ("module t\nworkflow w() -> Int budget { timeout: 1s, timeout: 2s } { 1 }", "given twice"),
        ("module t\nworkflow w() -> Int budget { max_parallel: 4294967296 } { 1 }", "positive literal"),
        ("module t\nworkflow w() -> Int budget { timeout: 9223372036854775807h } { 1 }", "out of range"),
    ] {
        let e = zal_compiler::compile_source(src, "t.zal").unwrap_err();
        let m = e.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n");
        assert!(m.contains(want), "{src}: {m}");
    }
}

#[test]
fn crafted_ir_cannot_reach_undeclared_effects() {
    // Stripping a helper's declared effects must not hide its extern call;
    // nor may a caller drop the effects of what it calls.
    let src = "module t\nextern tool rm(p: Str) -> Result<Str, Str>\ntask helper() -> Result<Str, Str> requires tool.rm { rm(\"/\") }\n\
               workflow w() -> Result<Str, Str> requires tool.rm { helper() }";
    let base = compile(src);
    for who in ["helper", "w"] {
        let mut p = base.clone();
        p.functions.iter_mut().find(|f| f.name == who).unwrap().effects.clear();
        let e = p.verify().unwrap_err();
        assert!(e.contains("does not declare"), "{who}: {e}");
    }
    base.verify().unwrap();
}

#[test]
fn policy_caps_bound_self_declared_budgets() {
    // A workflow declaring a generous (or no) budget still runs under the
    // host's ceilings.
    let src = "module t\nextern tool t(x: Int) -> Int\nworkflow w() -> Int requires tool.t budget { max_tool_calls: 1000 } {\n\
               let mut n = 0\n let mut i = 0\n while i < 10 { n = n + t(i)\n i = i + 1 }\n n }";
    let p = compile(src);
    let mut h = MockHost::default();
    h.tools.insert("t".into(), MockTool { echo: true, ..Default::default() });
    let b = PolicyBroker::from_json(r#"{"grant": ["tool.t"], "caps": {"max_tool_calls": 3}}"#).unwrap();
    let e = Vm::new(&p, h, b).run("w", vec![]).unwrap_err();
    assert!(e.msg.contains("more than 3 tool calls"), "{}", e.msg);
    assert!(PolicyBroker::from_json(r#"{"grant": [], "caps": {"max_parallel": 0}}"#).is_err());
    assert!(PolicyBroker::from_json(r#"{"grant": [], "caps": {"bogus": 1}}"#).is_err());
}
