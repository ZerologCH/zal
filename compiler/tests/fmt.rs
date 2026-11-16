//! `zal fmt`: idempotence and semantic preservation over every .zal file in the repo,
//! plus targeted layout and comment cases.

use std::path::{Path, PathBuf};

use zal_compiler::fmt::format_source;
use zal_compiler::{compile_file, compile_source};
use zal_ir::{Const, Program};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn zal_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "zal") {
                out.push(p);
            }
        }
    }
    let mut out = vec![];
    for d in ["examples", "zal-src", "tests/golden", "tests/differential", "docs/agents-examples", "benchmarks/workloads"] {
        walk(&repo().join(d), &mut out);
    }
    out.sort();
    out
}

fn fmt_file(p: &Path) -> Option<(String, String)> {
    let src = std::fs::read_to_string(p).unwrap();
    // Files that do not parse (golden error cases) are skipped; every other file must format.
    zal_compiler::parse_source(&src, "x.zal").ok()?;
    let out = format_source(&src, &p.display().to_string()).unwrap_or_else(|d| panic!("{}: {d}", p.display()));
    Some((src, out))
}

/// IR with the source-position data removed: formatting moves lines around.
fn normalized(mut p: Program) -> String {
    for f in &mut p.functions {
        f.lines.iter_mut().for_each(|l| *l = 0);
    }
    for c in &mut p.consts {
        // `assert cond` without message gets "assertion failed (line N)" from the parser.
        if let Const::Str(s) = c {
            if s.starts_with("assertion failed (line ") {
                *s = "assertion failed (line 0)".into();
            }
        }
    }
    p.to_json()
}

#[test]
fn corpus_is_not_empty() {
    let n = zal_files().iter().filter(|p| fmt_file(p).is_some()).count();
    eprintln!("formattable files: {n} of {}", zal_files().len());
    assert!(n >= 10, "only {n} formattable files found");
}

#[test]
fn idempotent_on_every_file() {
    for p in zal_files() {
        let Some((_, once)) = fmt_file(&p) else { continue };
        let twice = format_source(&once, "second.zal").unwrap_or_else(|d| panic!("{}: formatted output does not format: {d}", p.display()));
        assert_eq!(once, twice, "not idempotent: {}", p.display());
    }
}

#[test]
fn formatting_preserves_the_compiled_program() {
    let tmp = std::env::temp_dir().join(format!("zal-fmt-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut compared = 0;
    for p in zal_files() {
        let Some((_, out)) = fmt_file(&p) else { continue };
        let roots = vec![p.parent().unwrap().to_path_buf(), repo().join("zal-src")];
        let Ok(orig) = compile_file(&p, &roots) else { continue };
        let f = tmp.join("formatted.zal");
        std::fs::write(&f, &out).unwrap();
        let fmtd = compile_file(&f, &roots).unwrap_or_else(|ds| panic!("{}: formatted source does not compile: {}", p.display(), ds[0]));
        assert_eq!(normalized(orig), normalized(fmtd), "IR differs after formatting {}", p.display());
        compared += 1;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("compared IR of {compared} files");
    assert!(compared >= 4, "only {compared} files compiled");
}

#[test]
fn repo_sources_that_were_already_canonical_stay_put() {
    for rel in ["examples/build_verify.zal", "zal-src/scheduler/dag.zal"] {
        let p = repo().join(rel);
        let (src, out) = fmt_file(&p).unwrap();
        assert_eq!(src, out, "{rel} should be a fixed point");
    }
}

fn f(src: &str) -> String {
    format_source(src, "t.zal").unwrap_or_else(|d| panic!("{d}\n--- source ---\n{src}"))
}

fn semantic_same(src: &str) {
    let out = f(src);
    let a = compile_source(src, "t.zal").unwrap_or_else(|d| panic!("original does not compile: {}", d[0]));
    let b = compile_source(&out, "t.zal").unwrap_or_else(|d| panic!("formatted does not compile: {}\n{out}", d[0]));
    assert_eq!(normalized(a), normalized(b), "IR changed:\n{out}");
    assert_eq!(f(&out), out, "not idempotent:\n{out}");
}

#[test]
fn comments_survive_in_place() {
    let src = r#"// header
module t

// about f
fn f(x: Int) -> Int {   // trailing on signature
    // leading
    let a = x + 1 // trailing on let

    // before tail
    a * 2
    // dangling at block end
}

fn g() -> Int {
    // only a comment
}
// at end of file
"#;
    let out = f(src);
    for c in ["// header", "// about f", "// trailing on signature", "// leading", "// trailing on let", "// before tail", "// dangling at block end", "// only a comment", "// at end of file"] {
        assert_eq!(out.matches(c).count(), 1, "{c} lost or duplicated:\n{out}");
    }
    assert!(out.contains("fn f(x: Int) -> Int { // trailing on signature"), "{out}");
    assert!(out.contains("let a = x + 1 // trailing on let\n\n"), "blank line after let kept:\n{out}");
    let tail = out.find("a * 2").unwrap();
    assert!(out.find("// dangling at block end").unwrap() > tail, "{out}");
    assert!(out.find("// dangling at block end").unwrap() < out[tail..].find("\n}\n").unwrap() + tail, "dangling comment stays inside the block:\n{out}");
    assert_eq!(f(&out), out);
}

#[test]
fn comment_markers_inside_strings_are_not_comments() {
    let out = f("module t\nfn f() -> Str { \"a // b\" }\n");
    assert!(out.contains("\"a // b\""));
    assert_eq!(out.matches("//").count(), 1);
}

#[test]
fn strings_keep_their_escapes() {
    let src = "module t\nfn f() -> Str { \"a\\nb\\t\\\"q\\\" \\\\ end\" }\n";
    semantic_same(src);
    assert!(f(src).contains(r#""a\nb\t\"q\" \\ end""#));
}

#[test]
fn precedence_and_parentheses() {
    semantic_same("module t\nfn f(a: Int, b: Int, c: Int) -> Int { (a + b) * c - (a - b) - (b - c) }\n");
    semantic_same("module t\nfn f(a: Bool, b: Bool, c: Bool) -> Bool { (a || b) && c || !(a && b) }\n");
    semantic_same("module t\nfn f(a: Int) -> Int { -(a + 1) + - -a }\n");
    let out = f("module t\nfn f(a: Int, b: Int, c: Int) -> Int { a - (b - c) }\n");
    assert!(out.contains("a - (b - c)"), "{out}");
    let out = f("module t\nfn f(a: Int, b: Int, c: Int) -> Int { (a - b) - c }\n");
    assert!(out.contains("a - b - c"), "{out}");
}

#[test]
fn record_literal_in_head_keeps_parentheses() {
    let src = "module t\ntype P = { x: Int }\nfn f(p: P) -> Int {\n    if p == (P { x: 1 }) { 1 } else { 2 }\n}\n";
    semantic_same(src);
    assert!(f(src).contains("(P { x: 1 })"));
}

#[test]
fn trailing_semicolon_keeps_the_block_unit_valued() {
    let src = "module t\nfn g() -> Int { 1 }\nfn f() { g(); }\n";
    semantic_same(src);
    assert!(f(src).contains("g();"), "{}", f(src));
}

#[test]
fn clauses_go_on_their_own_lines() {
    let src = "module t\nextern tool b(x: Str) -> Result<Str, Str>\nworkflow w() -> Result<Str, Str> requires tool.b budget { timeout: 90s, max_parallel: 2 } { b(\"x\") }\n";
    let out = f(src);
    assert!(out.contains("workflow w() -> Result<Str, Str>\n    requires tool.b\n    budget { timeout: 90s, max_parallel: 2 }\n{\n    b(\"x\")\n}\n"), "{out}");
    semantic_same(src);
}

#[test]
fn durations_use_the_largest_exact_unit() {
    let out = f("module t\nworkflow w() requires tool.* budget { timeout: 60s, max_parallel: 2 } { sleep(1000ms); sleep(90s); sleep(2h) }\n");
    assert!(out.contains("timeout: 1min"), "{out}");
    assert!(out.contains("sleep(1s)") && out.contains("sleep(90s)") && out.contains("sleep(2h)"), "{out}");
}

#[test]
fn long_lines_wrap_and_stay_valid() {
    let src = "module t
fn long(alpha_value: Int, beta_value: Int, gamma_value: Int, delta_value: Int, epsilon_value: Int) -> Int { alpha_value }
fn h(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int) -> Int { a }
fn g() -> Int {
    let xs = [1000000, 2000000, 3000000, 4000000, 5000000, 6000000, 7000000, 8000000, 9000000, 1000001]
    let s = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" + \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\" + \"cccccccccccccccccccccccccc\" + \"dddddddddddddddddddd\"
    h(1000000, 2000000, 3000000, 4000000, 5000000, long(1000000, 2000000, 3000000, 4000000, 5000000)) + len(xs) + len(s)
}
";
    semantic_same(src);
    let out = f(src);
    assert!(out.lines().all(|l| l.chars().count() <= 110), "{out}");
}

#[test]
fn match_if_parallel_layouts() {
    let src = "module t
enum C { R, G, B(Int) }
extern tool a(x: Int) -> Result<Int, Str>
workflow w(c: C) -> Result<Int, Str> requires tool.a {
    let (x, y) = parallel { a(1) a(2) }
    let v = x? + y?
    match c { C.R => Ok(1), C.G => Ok(2), C.B(n) => { if n > 1 { Ok(n) } else { Err(\"small\") } } }
}
";
    semantic_same(src);
    let out = f(src);
    assert!(out.contains("parallel {\n        a(1)\n        a(2)\n    }"), "{out}");
}

#[test]
fn leading_minus_line_stays_a_new_statement() {
    semantic_same("module t\nfn f(a: Int) -> Int {\n    let x = a\n    -1\n}\n");
}

#[test]
fn tests_and_imports_roundtrip() {
    semantic_same("module t\nfn f() -> Int { 1 }\ntest \"one\" { assert f() == 1 }\ntest \"two\" { assert f() == 1, \"msg\" }\n");
}

#[test]
fn parse_errors_are_reported_not_formatted() {
    let e = format_source("module t\nfn f( {", "t.zal").unwrap_err();
    assert!(e.msg.contains("expected"), "{}", e.msg);
}
