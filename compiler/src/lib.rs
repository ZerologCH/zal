//! ZAL stage-0 compiler: source → AST → typed/effect-checked → ZAL IR v1.

pub mod ast;
pub mod check;
pub mod fmt;
pub mod lexer;
pub mod lastuse;
pub mod native;
pub mod parser;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Diag {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub msg: String,
}

impl std::fmt::Display for Diag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.col > 0 {
            write!(f, "{}:{}:{}: error: {}", self.file, self.line, self.col, self.msg)
        } else {
            write!(f, "{}:{}: error: {}", self.file, self.line, self.msg)
        }
    }
}

pub fn parse_source(src: &str, file: &str) -> Result<ast::Module, Diag> {
    let toks = lexer::lex(src, file)?;
    parser::parse(&toks, file)
}

/// Compile a single in-memory source (no imports). Used by tests and benchmarks.
pub fn compile_source(src: &str, file: &str) -> Result<zal_ir::Program, Vec<Diag>> {
    let m = parse_source(src, file).map_err(|e| vec![e])?;
    if let Some((imp, line)) = m.imports.first() {
        return Err(vec![Diag { file: file.into(), line: *line, col: 0, msg: format!("import `{imp}` needs a file path; use compile_file") }]);
    }
    check::compile(&[m])
}

/// Load `path` and, transitively, every `import a.b.c` from `<root>/a/b/c.zal`
/// for the first root that has it. The entry module comes first.
pub fn load_modules(path: &Path, roots: &[PathBuf]) -> Result<Vec<ast::Module>, Diag> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = vec![(path.to_path_buf(), None::<(String, String, u32)>)];
    while let Some((p, from)) = queue.pop() {
        let src = std::fs::read_to_string(&p).map_err(|e| match &from {
            Some((imp, file, line)) => Diag { file: file.clone(), line: *line, col: 0, msg: format!("cannot read import `{imp}` ({}): {e}", p.display()) },
            None => Diag { file: p.display().to_string(), line: 0, col: 0, msg: format!("cannot read: {e}") },
        })?;
        let m = parse_source(&src, &p.display().to_string())?;
        if let Some((imp, file, line)) = &from {
            if &m.name != imp {
                return Err(Diag { file: file.clone(), line: *line, col: 0, msg: format!("`{}` declares `module {}`, expected `{imp}`", p.display(), m.name) });
            }
        }
        if !seen.insert(m.name.clone()) {
            continue;
        }
        for (imp, line) in m.imports.iter().rev() {
            if seen.contains(imp) {
                continue;
            }
            let rel: PathBuf = imp.split('.').collect::<PathBuf>().with_extension("zal");
            let found = roots.iter().map(|r| r.join(&rel)).find(|c| c.exists());
            match found {
                Some(c) => queue.push((c, Some((imp.clone(), m.file.clone(), *line)))),
                None => {
                    return Err(Diag { file: m.file.clone(), line: *line, col: 0, msg: format!("import `{imp}` not found (looked for {} under {:?})", rel.display(), roots) })
                }
            }
        }
        out.push(m);
    }
    Ok(out)
}

pub fn compile_file(path: &Path, roots: &[PathBuf]) -> Result<zal_ir::Program, Vec<Diag>> {
    let mods = load_modules(path, roots).map_err(|e| vec![e])?;
    check::compile(&mods)
}

/// Like `compile_file`, choosing whether the peephole pass runs.
pub fn compile_file_opts(path: &Path, roots: &[PathBuf], fuse: bool) -> Result<zal_ir::Program, Vec<Diag>> {
    let mods = load_modules(path, roots).map_err(|e| vec![e])?;
    check::compile_opts(&mods, fuse)
}

/// Compile to native code: returns the generated Rust source (see `native`).
pub fn native_source(path: &Path, roots: &[PathBuf]) -> Result<String, Vec<Diag>> {
    let mods = load_modules(path, roots).map_err(|e| vec![e])?;
    let (prog, types) = check::compile_typed(&mods, false)?;
    native::generate(&mods, &prog, &types)
        .map(|n| n.source)
        .map_err(|m| vec![Diag { file: path.display().to_string(), line: 0, col: 0, msg: m }])
}
