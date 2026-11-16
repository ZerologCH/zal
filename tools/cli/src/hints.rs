//! Fix hints for `zal check --json`, derived from the diagnostic text and the AST.
//! The human-readable messages stay untouched; this module only reads them.

use zal_compiler::ast::*;
use zal_compiler::Diag;

const BUILTINS: &[&str] = &[
    "len", "push", "get", "set", "remove", "range", "map_new", "map_with_capacity", "map_get", "map_set", "map_has", "map_keys", "str", "print", "join", "await_any", "cancel", "sleep",
    "json_str", "json_get_str", "now_ms",
];
const BUILTIN_TYPES: &[&str] = &["Int", "Bool", "Str", "Unit", "Dur", "List", "Map", "Option", "Result", "Task"];

pub fn hint(d: &Diag, mods: &[Module]) -> Option<String> {
    let m = d.msg.as_str();
    let q = quoted(m);
    let q0 = q.first().map(String::as_str).unwrap_or("");

    if let Some(rest) = m.split_once("; add `requires ").map(|x| x.1) {
        // "calling `x` needs capability `e`; add `requires e` to `f`"
        return Some(format!("add `requires {}` to `{}`", rest.split('`').next().unwrap_or(""), rest.rsplit('`').nth(1).unwrap_or("")));
    }
    if m.starts_with("calling `") && m.contains("is pure; only a `task`") && q.len() >= 3 {
        let f = q[2].trim_start_matches("fn ");
        return Some(format!("change `fn {f}` to `task {f}` and add `requires {}`", q[1]));
    }
    if m.contains("is pure and cannot declare `requires`") {
        let f = q0.trim_start_matches("fn ");
        return Some(format!("change `fn {f}` to `task {f}`, or remove the `requires` clause"));
    }
    if m.starts_with("only a `workflow` can declare a budget") {
        return Some("change the function to a `workflow`, or remove the `budget` clause".into());
    }
    if m.starts_with("unknown effect kind in") || m.starts_with("unknown extern kind") {
        return Some("use one of the effect kinds: tool, net, file, secret, worker, deploy".into());
    }
    if m.starts_with("unknown duration unit") {
        return Some("use one of the duration units: ms, s, min, h (e.g. `30s`)".into());
    }
    if m.starts_with("cannot assign to `") {
        if m.contains("declare it with `let mut`") {
            return Some(format!("declare it with `let mut {q0} = ...`"));
        }
        return Some("copy the value into a new variable inside the block, or return it from the block and assign outside".into());
    }
    if m.starts_with("unknown variable `") {
        return near(q0, &scope_names(d, mods)).map(|n| format!("did you mean `{n}`?"));
    }
    if m.starts_with("unknown name `") {
        return near(q0, &scope_names(d, mods)).map(|n| format!("did you mean `{n}`?"));
    }
    if m.starts_with("unknown function `") {
        let mut names = BUILTINS.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        names.extend(func_names(mods));
        return near(q0, &names).map(|n| format!("did you mean `{n}`?"));
    }
    if m.starts_with("unknown type `") {
        let mut names = BUILTIN_TYPES.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        names.extend(type_names(mods));
        return Some(near(q0, &names).map(|n| format!("did you mean `{n}`?")).unwrap_or_else(|| format!("declare it with `type {q0} = {{ ... }}` or `enum {q0} {{ ... }}`")));
    }
    if m.starts_with("unknown record type `") || m.starts_with("unknown enum `") {
        let names = type_names(mods);
        return near(q0, &names).map(|n| format!("did you mean `{n}`?")).or_else(|| Some(format!("declare it first; declared types: {}", names.join(", "))));
    }
    if m.starts_with("record `") && m.contains("has no field `") && q.len() >= 2 {
        let fields = record_fields(mods, q0);
        let list = format!("fields of `{q0}`: {}", fields.join(", "));
        return Some(match near(&q[1], &fields) {
            Some(n) => format!("did you mean `{n}`? ({list})"),
            None => list,
        });
    }
    if m.starts_with("missing field `") && q.len() >= 2 {
        return Some(format!("add `{}: <value>` to the `{}` literal", q[0], q[1]));
    }
    if m.starts_with("field `") && m.ends_with("given twice") {
        return Some(format!("remove the duplicate `{q0}` field"));
    }
    if m.starts_with("enum `") && m.contains("has no variant `") && q.len() >= 2 {
        let vs = enum_variants(mods, q0);
        let list = format!("variants of `{q0}`: {}", vs.join(", "));
        return Some(match near(&q[1], &vs) {
            Some(n) => format!("did you mean `{q0}.{n}`? ({list})"),
            None => list,
        });
    }
    if m.starts_with("match on `") && m.contains("is not exhaustive") {
        return Some(non_exhaustive(d, mods, q0));
    }
    if m.starts_with("unreachable match arm") {
        return Some("remove the arm, or move it above the catch-all (`_` or a binding)".into());
    }
    if m.starts_with("`?` needs the enclosing function to return Result") || m.contains("`?` needs the enclosing function to return Result") {
        return Some("declare the enclosing function as `-> Result<_, E>` with the same error type E (or convert the error with `match`)".into());
    }
    if m.contains("`?` on Option needs the enclosing function to return Option") {
        return Some("declare the enclosing function as `-> Option<_>`".into());
    }
    if m.contains("a `retry` body must produce a Result") {
        return Some("make the last expression of the retry body a Result, e.g. a call that returns `Result<T, E>`".into());
    }
    if m.starts_with("`break`/`continue` outside a loop") {
        return Some("move it inside a `while`/`for` body, or use `return`".into());
    }
    if m.contains("is declared twice") {
        return Some("rename or remove one of the declarations (all top-level names share one namespace)".into());
    }
    if m.starts_with("`") && m.contains(" takes ") && m.contains(" argument(s), got ") {
        return Some(match signature(mods, q0) {
            Some(s) => format!("signature: {s}"),
            None => "match the number of arguments in the declaration".into(),
        });
    }
    if let Some((_, tail)) = m.split_once(": expected `") {
        // "<what>: expected `A`, found `B`"
        let (a, b) = (q_after(tail, 0), q.last().cloned().unwrap_or_default());
        let a = a.unwrap_or_default();
        return Some(conversion(&a, &b));
    }
    if let Some(t) = m.strip_prefix("expected `") {
        let tok = t.split('`').next().unwrap_or("");
        return Some(format!("insert `{tok}` here (or fix the preceding token)"));
    }
    if m.starts_with("a ZAL file starts with `module") {
        return Some("add `module <name>` as the first line (name = path under the import root, e.g. `module core.util`)".into());
    }
    if m.starts_with("expected a declaration") {
        return Some("top level allows only: type, enum, extern, fn, task, workflow, test (check for an unbalanced `}` above)".into());
    }
    if m.starts_with("unknown escape") {
        return Some("valid string escapes: \\n \\t \\\" \\\\".into());
    }
    if m.starts_with("unterminated string") {
        return Some("close the string with `\"` on the same line (strings cannot span lines; use \\n)".into());
    }
    if m.starts_with("empty `parallel`") || m.starts_with("empty `race`") {
        return Some("add at least one branch expression".into());
    }
    None
}

fn conversion(expected: &str, found: &str) -> String {
    let base = format!("expected `{expected}`, found `{found}`");
    let tip = if expected == "Str" && found == "Int" {
        " — convert with `str(x)`"
    } else if expected.starts_with("Option<") && !found.starts_with("Option<") {
        " — wrap the value in `Some(...)`"
    } else if expected.starts_with("Result<") && !found.starts_with("Result<") {
        " — wrap the value in `Ok(...)` (or `Err(...)`)"
    } else if found.starts_with("Result<") && !expected.starts_with("Result<") {
        " — unwrap with `?` or `match`"
    } else if found.starts_with("Option<") && !expected.starts_with("Option<") {
        " — unwrap with `match` (`Some(x) => ...`)"
    } else if found.starts_with("Task<") {
        " — use `await` on the task"
    } else {
        ""
    };
    format!("{base}{tip}")
}

fn non_exhaustive(d: &Diag, mods: &[Module], ty: &str) -> String {
    let generic = "add the missing cases or a final `_ =>` arm".to_string();
    // Candidate universe from the scrutinee type named in the message.
    let head = ty.split('<').next().unwrap_or(ty);
    let universe: Vec<(String, String)> = match head {
        "Option" => vec![("Some".into(), "Some".into()), ("None".into(), "None".into())],
        "Result" => vec![("Ok".into(), "Ok".into()), ("Err".into(), "Err".into())],
        "Bool" => vec![("true".into(), "true".into()), ("false".into(), "false".into())],
        e => enum_variants(mods, e).into_iter().map(|v| (v.clone(), format!("{e}.{v}"))).collect(),
    };
    if universe.is_empty() {
        return generic;
    }
    let mut best: Option<Vec<String>> = None;
    for m in mods.iter().filter(|m| m.file == d.file) {
        for_each_match(m, &mut |line, arms| {
            if line != d.line {
                return;
            }
            let covered: Vec<String> = arms
                .iter()
                .filter_map(|p| match p {
                    Pattern::Variant { path, .. } => path.last().cloned(),
                    Pattern::Bool(b) => Some(b.to_string()),
                    _ => None,
                })
                .collect();
            let missing: Vec<String> = universe.iter().filter(|(v, _)| !covered.contains(v)).map(|(_, full)| full.clone()).collect();
            if !missing.is_empty() && best.is_none() {
                best = Some(missing);
            }
        });
    }
    match best {
        Some(ms) => format!("add arms for: {} (or a final `_ =>` arm)", ms.join(", ")),
        None => format!("cases of `{ty}`: {}; {generic}", universe.iter().map(|(_, f)| f.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

// ---- name collection ------------------------------------------------------

fn func_names(mods: &[Module]) -> Vec<String> {
    let mut v = vec![];
    for m in mods {
        for it in &m.items {
            match it {
                Item::Func(f) if f.kind != FuncKind::Test => v.push(f.name.clone()),
                Item::Extern { name, .. } => v.push(name.clone()),
                _ => {}
            }
        }
    }
    v
}

fn type_names(mods: &[Module]) -> Vec<String> {
    mods.iter()
        .flat_map(|m| m.items.iter())
        .filter_map(|it| match it {
            Item::Record { name, .. } | Item::Enum { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn record_fields(mods: &[Module], rec: &str) -> Vec<String> {
    mods.iter()
        .flat_map(|m| m.items.iter())
        .find_map(|it| match it {
            Item::Record { name, fields, .. } if name == rec => Some(fields.iter().map(|(f, _)| f.clone()).collect()),
            _ => None,
        })
        .unwrap_or_default()
}

fn enum_variants(mods: &[Module], en: &str) -> Vec<String> {
    mods.iter()
        .flat_map(|m| m.items.iter())
        .find_map(|it| match it {
            Item::Enum { name, variants, .. } if name == en => Some(variants.iter().map(|(v, _)| v.clone()).collect()),
            _ => None,
        })
        .unwrap_or_default()
}

fn signature(mods: &[Module], name: &str) -> Option<String> {
    let ps = |p: &[(String, TypeExpr)]| p.iter().map(|(n, t)| format!("{n}: {}", ty(t))).collect::<Vec<_>>().join(", ");
    mods.iter().flat_map(|m| m.items.iter()).find_map(|it| match it {
        Item::Func(f) if f.name == name => Some(format!("{name}({})", ps(&f.params))),
        Item::Extern { name: n, params, .. } if n == name => Some(format!("{name}({})", ps(params))),
        _ => None,
    })
}

fn ty(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Name(n, a, _) if a.is_empty() => n.clone(),
        TypeExpr::Name(n, a, _) => format!("{n}<{}>", a.iter().map(ty).collect::<Vec<_>>().join(", ")),
        TypeExpr::Tuple(ts, _) => format!("({})", ts.iter().map(ty).collect::<Vec<_>>().join(", ")),
    }
}

/// Names visible near the diagnostic: the enclosing function's parameters,
/// `let`/`for`/pattern bindings, and every top-level function.
fn scope_names(d: &Diag, mods: &[Module]) -> Vec<String> {
    let mut names = func_names(mods);
    for m in mods.iter().filter(|m| m.file == d.file) {
        let mut funcs: Vec<&FuncDecl> = m.items.iter().filter_map(|i| if let Item::Func(f) = i { Some(f) } else { None }).collect();
        funcs.sort_by_key(|f| f.line);
        if let Some(f) = funcs.iter().rev().find(|f| f.line <= d.line) {
            names.extend(f.params.iter().map(|(n, _)| n.clone()));
            collect_block(&f.body, &mut names);
        }
    }
    names
}

fn collect_block(b: &Block, out: &mut Vec<String>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { names, value, .. } => {
                out.extend(names.iter().cloned());
                collect_expr(value, out);
            }
            Stmt::For { var, iter, body, .. } => {
                out.push(var.clone());
                collect_expr(iter, out);
                collect_block(body, out);
            }
            Stmt::While { cond, body, .. } => {
                collect_expr(cond, out);
                collect_block(body, out);
            }
            Stmt::Assign { value, .. } => collect_expr(value, out),
            Stmt::Expr(e) => collect_expr(e, out),
            Stmt::Return(Some(e), _) => collect_expr(e, out),
            Stmt::Ensure { cond, err, .. } => {
                collect_expr(cond, out);
                collect_expr(err, out);
            }
            Stmt::Assert { cond, msg, .. } => {
                collect_expr(cond, out);
                collect_expr(msg, out);
            }
            _ => {}
        }
    }
    if let Some(t) = &b.tail {
        collect_expr(t, out);
    }
}

fn collect_expr(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Match { scrut, arms } => {
            collect_expr(scrut, out);
            for (p, b) in arms {
                match p {
                    Pattern::Variant { bind: Some(n), .. } | Pattern::Bind(n) => out.push(n.clone()),
                    _ => {}
                }
                collect_expr(b, out);
            }
        }
        ExprKind::Block(b) | ExprKind::Timeout { body: b, .. } | ExprKind::Retry { body: b, .. } => collect_block(b, out),
        ExprKind::If { cond, then, els } => {
            collect_expr(cond, out);
            collect_block(then, out);
            if let Some(x) = els {
                collect_expr(x, out);
            }
        }
        ExprKind::Call { args: es, .. } | ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Parallel(es) | ExprKind::Race(es) => es.iter().for_each(|x| collect_expr(x, out)),
        ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
            collect_expr(a, out);
            collect_expr(b, out);
        }
        ExprKind::Field(x, _) | ExprKind::TupleIdx(x, _) | ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Try(x) | ExprKind::Spawn(x) | ExprKind::Await(x) => collect_expr(x, out),
        ExprKind::Record { fields, .. } => fields.iter().for_each(|(_, v)| collect_expr(v, out)),
        _ => {}
    }
}

fn for_each_match(m: &Module, f: &mut dyn FnMut(u32, Vec<&Pattern>)) {
    fn blk(b: &Block, f: &mut dyn FnMut(u32, Vec<&Pattern>)) {
        for s in &b.stmts {
            match s {
                Stmt::Let { value, .. } | Stmt::Assign { value, .. } => ex(value, f),
                Stmt::Expr(e) => ex(e, f),
                Stmt::Return(Some(e), _) => ex(e, f),
                Stmt::While { cond, body, .. } | Stmt::For { iter: cond, body, .. } => {
                    ex(cond, f);
                    blk(body, f);
                }
                Stmt::Ensure { cond, err, .. } => {
                    ex(cond, f);
                    ex(err, f);
                }
                Stmt::Assert { cond, msg, .. } => {
                    ex(cond, f);
                    ex(msg, f);
                }
                _ => {}
            }
        }
        if let Some(t) = &b.tail {
            ex(t, f);
        }
    }
    fn ex(e: &Expr, f: &mut dyn FnMut(u32, Vec<&Pattern>)) {
        match &e.kind {
            ExprKind::Match { scrut, arms } => {
                f(e.line, arms.iter().map(|(p, _)| p).collect());
                ex(scrut, f);
                arms.iter().for_each(|(_, b)| ex(b, f));
            }
            ExprKind::Block(b) | ExprKind::Timeout { body: b, .. } | ExprKind::Retry { body: b, .. } => blk(b, f),
            ExprKind::If { cond, then, els } => {
                ex(cond, f);
                blk(then, f);
                if let Some(x) = els {
                    ex(x, f);
                }
            }
            ExprKind::Call { args: es, .. } | ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Parallel(es) | ExprKind::Race(es) => es.iter().for_each(|x| ex(x, f)),
            ExprKind::Binary(_, a, b) | ExprKind::Index(a, b) => {
                ex(a, f);
                ex(b, f);
            }
            ExprKind::Field(x, _) | ExprKind::TupleIdx(x, _) | ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Try(x) | ExprKind::Spawn(x) | ExprKind::Await(x) => ex(x, f),
            ExprKind::Record { fields, .. } => fields.iter().for_each(|(_, v)| ex(v, f)),
            _ => {}
        }
    }
    for it in &m.items {
        if let Item::Func(func) = it {
            blk(&func.body, f);
        }
    }
}

// ---- text helpers ---------------------------------------------------------

/// Backtick-quoted fragments of a message, in order.
fn quoted(s: &str) -> Vec<String> {
    s.split('`').skip(1).step_by(2).map(String::from).collect()
}

fn q_after(s: &str, n: usize) -> Option<String> {
    // `s` starts right after the opening backtick of the first quoted item.
    let mut parts = s.split('`');
    parts.nth(n * 2).map(String::from)
}

/// Closest candidate by edit distance, if it is close enough to be a plausible typo.
fn near(name: &str, candidates: &[String]) -> Option<String> {
    let limit = match name.chars().count() {
        0 | 1 => 0,
        2..=3 => 1,
        4..=8 => 2,
        _ => 3,
    };
    candidates
        .iter()
        .filter(|c| c.as_str() != name)
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, c)| (*d, c.len()))
        .map(|(_, c)| c.clone())
}

/// Levenshtein distance where swapping two adjacent letters counts once (typical typo).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use zal_compiler::{load_modules, compile_file};
    use std::path::PathBuf;

    /// Compile an in-memory program that must fail; return the first diagnostic and its hint.
    fn hint_for(src: &str) -> (String, Option<String>) {
        let dir = std::env::temp_dir().join(format!("zal-hints-{}-{}", std::process::id(), src.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let f: PathBuf = dir.join("t.zal");
        std::fs::write(&f, src).unwrap();
        let roots = vec![dir.clone()];
        let ds = compile_file(&f, &roots).unwrap_err();
        let mods = load_modules(&f, &roots).unwrap_or_default();
        let r = (ds[0].msg.clone(), hint(&ds[0], &mods));
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn missing_capability() {
        let (_, h) = hint_for("module t\nextern tool build(r: Str) -> Result<Str, Str>\nworkflow w() -> Result<Str, Str> { build(\"x\") }\n");
        assert_eq!(h.unwrap(), "add `requires tool.build` to `w`");
    }

    #[test]
    fn unknown_variable_suggests_nearest_name() {
        let (_, h) = hint_for("module t\nfn f(count: Int) -> Int { cuont + 1 }\n");
        assert_eq!(h.unwrap(), "did you mean `count`?");
    }

    #[test]
    fn unknown_function_suggests_builtin() {
        let (_, h) = hint_for("module t\nfn f(xs: List<Int>) -> Int { lenn(xs) }\n");
        assert_eq!(h.unwrap(), "did you mean `len`?");
    }

    #[test]
    fn non_exhaustive_lists_missing_variants() {
        let (_, h) = hint_for("module t\nenum Color { Red, Green, Blue }\nfn f(c: Color) -> Int { match c { Color.Red => 1 } }\n");
        let h = h.unwrap();
        assert!(h.contains("Color.Green") && h.contains("Color.Blue") && !h.contains("Color.Red,"), "{h}");
        let (_, h) = hint_for("module t\nfn f(o: Option<Int>) -> Int { match o { Some(x) => x } }\n");
        assert!(h.unwrap().contains("None"));
    }

    #[test]
    fn type_mismatch_shows_expected_and_found() {
        let (m, h) = hint_for("module t\nfn f(n: Int) -> Str { n }\n");
        assert!(m.contains("expected `Str`, found `Int`"), "{m}");
        let h = h.unwrap();
        assert!(h.contains("expected `Str`, found `Int`") && h.contains("str(x)"), "{h}");
    }

    #[test]
    fn unknown_field_lists_fields() {
        let (_, h) = hint_for("module t\ntype A = { name: Str }\nfn f(a: A) -> Str { a.nmae }\n");
        let h = h.unwrap();
        assert!(h.contains("did you mean `name`?"), "{h}");
    }

    #[test]
    fn immutable_assign() {
        let (_, h) = hint_for("module t\nfn f() -> Int {\n    let x = 1\n    x = 2\n    x\n}\n");
        assert_eq!(h.unwrap(), "declare it with `let mut x = ...`");
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(near("lenn", &["len".into(), "push".into()]).as_deref(), Some("len"));
        assert_eq!(near("zzzzzz", &["len".into()]), None);
    }
}
