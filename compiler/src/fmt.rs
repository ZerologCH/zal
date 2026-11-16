//! Canonical formatter: AST → ZAL source (`zal fmt`).
//!
//! The parser keeps no comments and few positions, so comments are scanned
//! from the source text and re-attached by line number: a comment on its own
//! line goes above the next statement/item/arm/element that starts after it,
//! a trailing comment stays at the end of the line that starts on the same
//! source line, and comments before a closing brace stay inside the block
//! (closing lines come from brace matching on the token stream). Blank lines
//! between statements are preserved (at most one).
//!
//! Safety net: `format_source` re-parses its output and refuses to return it
//! unless (1) the canonical shape of the AST is unchanged, (2) the comment
//! texts are identical and in order and (3) a second pass changes nothing.

use std::collections::HashMap;

use crate::ast::*;
use crate::lexer::{lex, Tok, Token};
use crate::{parse_source, Diag};

const WIDTH: usize = 100;

/// Format `src`. Errors are parse/lex diagnostics, or an internal diagnostic
/// when the result could not be proven equivalent (the input stays untouched).
pub fn format_source(src: &str, file: &str) -> Result<String, Diag> {
    let mut cur = format_once(src, file)?;
    for _ in 0..3 {
        let next = format_once(&cur, file).map_err(|d| internal(file, format!("formatted output does not parse again ({d})")))?;
        if next == cur {
            return Ok(cur);
        }
        cur = next;
    }
    Err(internal(file, "formatting does not converge".into()))
}

fn internal(file: &str, msg: String) -> Diag {
    Diag { file: file.to_string(), line: 0, col: 0, msg: format!("fmt: internal error, file left unchanged: {msg}") }
}

fn format_once(src: &str, file: &str) -> Result<String, Diag> {
    let m = parse_source(src, file)?;
    let toks = lex(src, file)?;
    let comments = scan_comments(src);
    let texts: Vec<String> = comments.iter().map(|c| c.text.clone()).collect();
    let header = toks[0].line;
    let mut p = Printer::new(Some((toks, comments, src)));
    p.module(&m, header);
    let out = p.finish();

    let m2 = parse_source(&out, file).map_err(|d| internal(file, format!("formatted output does not parse ({d})")))?;
    if shape(&m) != shape(&m2) {
        return Err(internal(file, "program structure would change".into()));
    }
    let after: Vec<String> = scan_comments(&out).into_iter().map(|c| c.text).collect();
    if after != texts {
        return Err(internal(file, "comments would change".into()));
    }
    Ok(out)
}

/// Layout-independent rendering of an AST, used to compare programs.
fn shape(m: &Module) -> String {
    let mut p = Printer::new(None);
    p.module(m, 1);
    p.finish()
}

struct Comment {
    line: u32,
    text: String,
    trailing: bool,
    used: bool,
}

fn scan_comments(src: &str) -> Vec<Comment> {
    let mut out = Vec::new();
    for (i, l) in src.lines().enumerate() {
        let cs: Vec<char> = l.chars().collect();
        let (mut j, mut in_str) = (0, false);
        while j < cs.len() {
            let c = cs[j];
            if in_str {
                if c == '\\' {
                    j += 1;
                } else if c == '"' {
                    in_str = false;
                }
            } else if c == '"' {
                in_str = true;
            } else if c == '/' && cs.get(j + 1) == Some(&'/') {
                let text: String = cs[j..].iter().collect();
                let trailing = cs[..j].iter().any(|c| !c.is_whitespace());
                out.push(Comment { line: i as u32 + 1, text: text.trim_end().to_string(), trailing, used: false });
                break;
            }
            j += 1;
        }
    }
    out
}

struct Printer {
    out: String,
    fresh: bool,
    line_start: usize,
    indent: usize,
    /// Source lines whose trailing comment may end the current output line.
    trail: Vec<u32>,
    last_end: u32,
    block_start: bool,
    comments: Vec<Comment>,
    blank: Vec<bool>,
    toks: Vec<Token>,
    closes: HashMap<u32, u32>,
}

enum Part<'a> {
    T(String),
    E(&'a Expr, bool),
    B(&'a Block),
}

impl Printer {
    fn new(info: Option<(Vec<Token>, Vec<Comment>, &str)>) -> Self {
        let (toks, comments, blank) = match info {
            Some((t, c, src)) => (t, c, src.lines().map(|l| l.trim().is_empty()).collect::<Vec<_>>()),
            None => (vec![], vec![], vec![]),
        };
        let mut closes: HashMap<u32, u32> = HashMap::new();
        let mut stack = Vec::new();
        for t in &toks {
            match t.tok {
                Tok::Sym("{") => stack.push(t.line),
                Tok::Sym("}") => {
                    if let Some(o) = stack.pop() {
                        let e = closes.entry(o).or_insert(t.line);
                        *e = (*e).max(t.line);
                    }
                }
                _ => {}
            }
        }
        Printer { out: String::new(), fresh: true, line_start: 0, indent: 0, trail: vec![], last_end: 0, block_start: false, comments, blank, toks, closes }
    }

    fn finish(mut self) -> String {
        self.flush(u32::MAX);
        let mut s = self.out.trim_end().to_string();
        s.push('\n');
        s
    }

    // ---- low-level output -------------------------------------------------

    fn col(&self) -> usize {
        if self.fresh {
            self.indent * 4
        } else {
            self.out[self.line_start..].chars().count()
        }
    }

    fn fits(&self, s: &str) -> bool {
        self.col() + s.chars().count() <= WIDTH
    }

    fn w(&mut self, s: &str) {
        if self.fresh {
            for _ in 0..self.indent {
                self.out.push_str("    ");
            }
            self.fresh = false;
        }
        self.out.push_str(s);
    }

    fn nl(&mut self) {
        let lines = std::mem::take(&mut self.trail);
        if !self.fresh {
            for l in lines {
                if let Some(c) = self.comments.iter_mut().find(|c| !c.used && c.trailing && c.line == l) {
                    c.used = true;
                    let t = c.text.clone();
                    self.out.push(' ');
                    self.out.push_str(&t);
                    break;
                }
            }
        }
        self.out.push('\n');
        self.fresh = true;
        self.line_start = self.out.len();
    }

    fn blank_line(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
            self.line_start = self.out.len();
        }
    }

    // ---- source facts -----------------------------------------------------

    fn gap(&self, a: u32, b: u32) -> bool {
        a < b && ((a + 1)..b).any(|l| self.blank.get(l as usize - 1).copied().unwrap_or(false))
    }

    fn close_of(&self, open: u32) -> u32 {
        self.closes.get(&open).copied().unwrap_or(open)
    }

    fn comment_in(&self, a: u32, b: u32) -> bool {
        self.comments.iter().any(|c| c.line >= a && c.line <= b)
    }

    /// Start lines of the comma-separated elements of the first `open` bracket at or after `line`.
    fn elems(&self, line: u32, open: &'static str) -> Vec<u32> {
        let Some(i) = self.toks.iter().position(|t| t.line >= line && t.tok == Tok::Sym(open)) else { return vec![] };
        let (mut depth, mut out, mut at_start) = (0i32, vec![], true);
        for t in &self.toks[i..] {
            match t.tok {
                Tok::Sym("(") | Tok::Sym("{") | Tok::Sym("[") | Tok::Sym("<") => {
                    depth += 1;
                    if depth == 1 {
                        continue;
                    }
                }
                Tok::Sym(")") | Tok::Sym("}") | Tok::Sym("]") | Tok::Sym(">") => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Sym(",") if depth == 1 => {
                    at_start = true;
                    continue;
                }
                _ => {}
            }
            if at_start && depth >= 1 {
                out.push(t.line);
                at_start = false;
            }
        }
        out
    }

    // ---- comment-aware line starts ---------------------------------------

    fn flush(&mut self, before: u32) {
        while let Some(i) = self.comments.iter().position(|c| !c.used && c.line < before) {
            self.comments[i].used = true;
            let (line, text) = (self.comments[i].line, self.comments[i].text.clone());
            if self.gap(self.last_end, line) && !self.block_start {
                self.blank_line();
            }
            self.block_start = false;
            self.w(&text);
            self.trail.clear();
            self.nl();
            self.last_end = line;
        }
    }

    /// Begin a new output line for a node starting on source line `start`.
    fn unit(&mut self, start: u32, force_blank: bool) {
        if force_blank && !self.block_start {
            self.blank_line();
        }
        self.flush(start);
        if self.gap(self.last_end, start) && !self.block_start {
            self.blank_line();
        }
        self.block_start = false;
        self.trail = vec![start];
    }

    fn open_body(&mut self, line: u32) {
        self.indent += 1;
        self.block_start = true;
        self.last_end = line;
    }

    fn close_body(&mut self, close: u32) {
        self.flush(close);
        self.indent -= 1;
        self.block_start = false;
    }

    // ---- positions --------------------------------------------------------

    fn start_expr(&self, e: &Expr) -> u32 {
        match &e.kind {
            ExprKind::Binary(_, l, _) => self.start_expr(l),
            ExprKind::Field(x, _) | ExprKind::TupleIdx(x, _) | ExprKind::Index(x, _) | ExprKind::Try(x) => self.start_expr(x),
            _ => e.line,
        }
    }

    fn start_stmt(&self, s: &Stmt) -> u32 {
        match s {
            Stmt::Let { line, .. }
            | Stmt::Assign { line, .. }
            | Stmt::While { line, .. }
            | Stmt::For { line, .. }
            | Stmt::Ensure { line, .. }
            | Stmt::Assert { line, .. } => *line,
            Stmt::Return(_, l) | Stmt::Break(l) | Stmt::Continue(l) => *l,
            Stmt::Expr(e) => self.start_expr(e),
        }
    }

    fn end_block(&self, b: &Block) -> u32 {
        self.close_of(b.line).max(b.line)
    }

    fn end_stmt(&self, s: &Stmt) -> u32 {
        match s {
            Stmt::Let { value, line, .. } | Stmt::Assign { value, line, .. } => self.end_expr(value).max(*line),
            Stmt::Expr(e) => self.end_expr(e),
            Stmt::Return(e, l) => e.as_ref().map_or(*l, |e| self.end_expr(e).max(*l)),
            Stmt::While { cond, body, line } => self.end_expr(cond).max(self.end_block(body)).max(*line),
            Stmt::For { iter, body, line, .. } => self.end_expr(iter).max(self.end_block(body)).max(*line),
            Stmt::Ensure { cond, err, line } => self.end_expr(cond).max(self.end_expr(err)).max(*line),
            Stmt::Assert { cond, msg, line } => self.end_expr(cond).max(self.end_expr(msg)).max(*line),
            Stmt::Break(l) | Stmt::Continue(l) => *l,
        }
    }

    fn end_expr(&self, e: &Expr) -> u32 {
        let mut m = e.line;
        let mut up = |x: u32| m = m.max(x);
        match &e.kind {
            ExprKind::Call { args: es, .. } | ExprKind::List(es) | ExprKind::Tuple(es) => es.iter().for_each(|x| up(self.end_expr(x))),
            ExprKind::Field(x, _) | ExprKind::TupleIdx(x, _) | ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Try(x) | ExprKind::Spawn(x) | ExprKind::Await(x) => up(self.end_expr(x)),
            ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
                up(self.end_expr(a));
                up(self.end_expr(b));
            }
            ExprKind::If { cond, then, els } => {
                up(self.end_expr(cond));
                up(self.end_block(then));
                if let Some(x) = els {
                    up(self.end_expr(x));
                }
            }
            ExprKind::Match { scrut, arms } => {
                up(self.end_expr(scrut));
                arms.iter().for_each(|(_, b)| up(self.end_expr(b)));
                up(self.close_of(e.line));
            }
            ExprKind::Block(b) => up(self.end_block(b)),
            ExprKind::Record { fields, .. } => fields.iter().for_each(|(_, v)| up(self.end_expr(v))),
            ExprKind::Parallel(es) | ExprKind::Race(es) => {
                es.iter().for_each(|x| up(self.end_expr(x)));
                up(self.close_of(e.line));
            }
            ExprKind::Timeout { dur, body } => {
                up(self.end_expr(dur));
                up(self.end_block(body));
            }
            ExprKind::Retry { attempts, backoff, body } => {
                up(self.end_expr(attempts));
                if let Some(b) = backoff {
                    up(self.end_expr(b));
                }
                up(self.end_block(body));
            }
            _ => {}
        }
        m
    }

    // ---- module and items -------------------------------------------------

    fn module(&mut self, m: &Module, header: u32) {
        self.unit(header, false);
        self.w(&format!("module {}", m.name));
        self.nl();
        self.last_end = header;
        if !m.imports.is_empty() {
            self.blank_line();
            for (name, line) in &m.imports {
                self.unit(*line, false);
                self.w(&format!("import {name}"));
                self.last_end = *line;
                self.nl();
            }
        }
        let mut prev: Option<&Item> = None;
        for it in &m.items {
            let tight = prev.is_some_and(|p| self.tight(p, it));
            self.unit(item_line(it), !tight);
            self.item(it);
            self.nl();
            prev = Some(it);
        }
    }

    fn tight(&self, a: &Item, b: &Item) -> bool {
        let same = matches!((a, b), (Item::Record { .. }, Item::Record { .. }) | (Item::Enum { .. }, Item::Enum { .. }) | (Item::Extern { .. }, Item::Extern { .. }));
        same && self.one_line(a) && self.one_line(b) && !self.gap(item_line(a), item_line(b))
    }

    fn one_line(&self, it: &Item) -> bool {
        match it {
            Item::Extern { .. } => true,
            Item::Record { line, .. } | Item::Enum { line, .. } => self.close_of(*line) == *line,
            Item::Func(_) => false,
        }
    }

    fn item(&mut self, it: &Item) {
        match it {
            Item::Record { name, fields, line } => {
                let items: Vec<String> = fields.iter().map(|(f, t)| format!("{f}: {}", ty_str(t))).collect();
                self.braced_decl(&format!("type {name} = "), &items, *line);
            }
            Item::Enum { name, variants, line } => {
                let items: Vec<String> = variants.iter().map(|(v, t)| t.as_ref().map_or(v.clone(), |t| format!("{v}({})", ty_str(t)))).collect();
                self.braced_decl(&format!("enum {name} "), &items, *line);
            }
            Item::Extern { kind, name, params, ret, readonly, line } => {
                let mut tail = format!(" -> {}", ty_str(ret));
                if *readonly {
                    tail.push_str(" readonly");
                }
                self.signature(&format!("extern {kind} {name}"), params, &tail, *line);
            }
            Item::Func(f) => self.func(f),
        }
        self.last_end = self.item_end(it);
    }

    fn item_end(&self, it: &Item) -> u32 {
        match it {
            Item::Func(f) => self.end_block(&f.body),
            other => self.close_of(item_line(other)).max(item_line(other)),
        }
    }

    /// `type N = { a: T }` / `enum E { A, B }` on one line when it fits, one element per line otherwise.
    fn braced_decl(&mut self, head: &str, items: &[String], line: u32) {
        let cl = self.close_of(line);
        let inline = if items.is_empty() { "{}".to_string() } else { format!("{{ {} }}", items.join(", ")) };
        let blocked = cl > line && self.comment_in(line, cl - 1);
        if !blocked && self.fits(&format!("{head}{inline}")) {
            self.w(&format!("{head}{inline}"));
            return;
        }
        let lines = self.elems(line, "{");
        self.w(&format!("{head}{{"));
        self.nl();
        self.open_body(line);
        for (i, it) in items.iter().enumerate() {
            let at = if lines.len() == items.len() { lines[i] } else { line };
            self.unit(at, false);
            self.w(&format!("{it},"));
            self.last_end = at;
            self.nl();
        }
        self.close_body(cl);
        self.w("}");
        self.trail.push(cl);
    }

    /// `prefix(a: T, b: U)tail`, one parameter per line when too long.
    fn signature(&mut self, prefix: &str, params: &[(String, TypeExpr)], tail: &str, line: u32) {
        let ps: Vec<String> = params.iter().map(|(n, t)| format!("{n}: {}", ty_str(t))).collect();
        let one = format!("{prefix}({}){tail}", ps.join(", "));
        if ps.is_empty() || self.fits(&one) {
            self.w(&one);
            return;
        }
        let lines = self.elems(line, "(");
        self.w(&format!("{prefix}("));
        self.nl();
        self.open_body(line);
        for (i, p) in ps.iter().enumerate() {
            let at = if lines.len() == ps.len() { lines[i] } else { line };
            self.unit(at, false);
            self.w(&format!("{p},"));
            self.last_end = at;
            self.nl();
        }
        self.close_body(self.last_end);
        self.w(&format!("){tail}"));
    }

    fn func(&mut self, f: &FuncDecl) {
        let has_clauses = !f.requires.is_empty() || !f.budget.is_empty();
        let tail = f.ret.as_ref().map_or(String::new(), |t| format!(" -> {}", ty_str(t)));
        match f.kind {
            FuncKind::Test => self.w(&format!("test {}", quote(&f.name))),
            k => {
                let kw = match k {
                    FuncKind::Fn => "fn",
                    FuncKind::Task => "task",
                    _ => "workflow",
                };
                self.signature(&format!("{kw} {}", f.name), &f.params, &tail, f.line);
            }
        }
        if !has_clauses {
            self.w(" ");
            self.block(&f.body, true);
            return;
        }
        self.nl();
        self.indent += 1;
        let mut lines: Vec<String> = vec![];
        for r in &f.requires {
            match lines.last_mut() {
                Some(l) if self.indent * 4 + l.len() + 2 + r.len() <= WIDTH => {
                    l.push_str(", ");
                    l.push_str(r);
                }
                _ => lines.push(format!("requires {r}")),
            }
        }
        for l in lines {
            self.w(&l);
            self.nl();
        }
        if !f.budget.is_empty() {
            self.w("budget { ");
            for (i, (k, v)) in f.budget.iter().enumerate() {
                if i > 0 {
                    self.w(", ");
                }
                self.w(&format!("{k}: "));
                self.expr(v, false);
            }
            self.w(" }");
            self.nl();
        }
        self.indent -= 1;
        self.trail = vec![f.body.line];
        self.block(&f.body, true);
    }

    // ---- blocks and statements -------------------------------------------

    fn block(&mut self, b: &Block, force_multi: bool) {
        if let Some(s) = self.inline_block(b) {
            if (!force_multi || s == "{}") && self.fits(&s) {
                self.w(&s);
                return;
            }
        }
        self.w("{");
        self.nl();
        self.open_body(b.line);
        let n = b.stmts.len();
        for (i, s) in b.stmts.iter().enumerate() {
            self.unit(self.start_stmt(s), false);
            self.stmt(s);
            if i + 1 == n && b.tail.is_none() && semi_needed(s) {
                self.w(";");
            }
            self.last_end = self.end_stmt(s);
            self.nl();
        }
        if let Some(t) = &b.tail {
            self.unit(self.start_expr(t), false);
            self.expr_stmt(t);
            self.last_end = self.end_expr(t);
            self.nl();
        }
        let cl = self.close_of(b.line);
        self.close_body(cl);
        self.w("}");
        self.trail.push(cl);
    }

    fn inline_block(&self, b: &Block) -> Option<String> {
        let cl = self.close_of(b.line);
        if cl > b.line && self.comment_in(b.line, cl - 1) {
            return None;
        }
        let n = b.stmts.len() + b.tail.is_some() as usize;
        match n {
            0 => Some("{}".into()),
            1 => {
                let item = match &b.tail {
                    Some(t) => {
                        if !leaf_expr(t) {
                            return None;
                        }
                        self.inl(t, false)?
                    }
                    None => {
                        let s = &b.stmts[0];
                        if !leaf_stmt(s) {
                            return None;
                        }
                        let mut x = self.parts_inl(&stmt_parts(s))?;
                        if semi_needed(s) {
                            x.push(';');
                        }
                        x
                    }
                };
                Some(format!("{{ {item} }}"))
            }
            _ => None,
        }
    }

    /// A `match` used as a statement or block value always gets one arm per line.
    fn expr_stmt(&mut self, e: &Expr) {
        if matches!(e.kind, ExprKind::Match { .. }) {
            self.expr_multi(e, false);
        } else {
            self.expr(e, false);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        if let Stmt::Expr(e) = s {
            return self.expr_stmt(e);
        }
        for p in stmt_parts(s) {
            match p {
                Part::T(t) => self.w(&t),
                Part::E(e, head) => self.expr(e, head),
                Part::B(b) => self.block(b, false),
            }
        }
    }

    fn parts_inl(&self, parts: &[Part]) -> Option<String> {
        let mut s = String::new();
        for p in parts {
            match p {
                Part::T(t) => s.push_str(t),
                Part::E(e, head) => s.push_str(&self.inl(e, *head)?),
                Part::B(b) => s.push_str(&self.inline_block(b)?),
            }
        }
        Some(s)
    }

    // ---- expressions ------------------------------------------------------

    /// Single-line rendering, or `None` when the expression needs several lines.
    /// `head`: inside an `if`/`while`/`for`/`match` head, where a record literal needs parentheses.
    fn inl(&self, e: &Expr, head: bool) -> Option<String> {
        let list = |es: &[Expr], h: bool| -> Option<String> { es.iter().map(|x| self.inl(x, h)).collect::<Option<Vec<_>>>().map(|v| v.join(", ")) };
        let multiline_comment = |this: &Self| {
            let (a, b) = (this.start_expr(e), this.end_expr(e));
            a < b && this.comment_in(a, b)
        };
        Some(match &e.kind {
            ExprKind::Int(n) => n.to_string(),
            ExprKind::Str(s) => quote(s),
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Dur(ms) => dur_str(*ms),
            ExprKind::Unit => "()".into(),
            ExprKind::Var(n) => n.clone(),
            ExprKind::Path(p) => p.join("."),
            ExprKind::Call { path, args } => {
                if multiline_comment(self) {
                    return None;
                }
                format!("{}({})", path.join("."), list(args, head)?)
            }
            ExprKind::Field(x, f) => format!("{}.{f}", self.target(x, head)?),
            ExprKind::TupleIdx(x, i) => format!("{}.{i}", self.target(x, head)?),
            ExprKind::Index(x, i) => format!("{}[{}]", self.target(x, head)?, self.inl(i, head)?),
            ExprKind::Try(x) => format!("{}?", self.target(x, head)?),
            ExprKind::Binary(op, l, r) => {
                let p = op_prec(*op);
                format!("{} {} {}", self.operand(l, p, false, head)?, op_str(*op), self.operand(r, p, true, head)?)
            }
            ExprKind::Neg(x) => format!("-{}", self.unary_operand(x, head)?),
            ExprKind::Not(x) => format!("!{}", self.unary_operand(x, head)?),
            ExprKind::Await(x) => format!("await {}", self.unary_operand(x, head)?),
            ExprKind::Spawn(x) => format!("spawn {}", self.unary_operand(x, head)?),
            ExprKind::If { cond, then, els } => {
                let mut s = format!("if {} {}", self.inl(cond, true)?, self.inline_block(then)?);
                if let Some(x) = els {
                    s.push_str(" else ");
                    match &x.kind {
                        ExprKind::Block(b) => s.push_str(&self.inline_block(b)?),
                        _ => s.push_str(&self.inl(x, false)?),
                    }
                }
                s
            }
            ExprKind::Match { scrut, arms } => {
                let cl = self.close_of(e.line);
                if (cl > e.line && self.comment_in(e.line, cl - 1)) || arms.len() > 3 {
                    return None;
                }
                let mut parts = vec![];
                for (p, b) in arms {
                    if matches!(b.kind, ExprKind::Block(_)) || !leaf_expr(b) {
                        return None;
                    }
                    parts.push(format!("{} => {}", pat_str(p), self.inl(b, false)?));
                }
                if parts.is_empty() {
                    format!("match {} {{}}", self.inl(scrut, true)?)
                } else {
                    format!("match {} {{ {} }}", self.inl(scrut, true)?, parts.join(", "))
                }
            }
            ExprKind::Block(b) => self.inline_block(b)?,
            ExprKind::List(es) => {
                if multiline_comment(self) {
                    return None;
                }
                format!("[{}]", list(es, head)?)
            }
            ExprKind::Tuple(es) => {
                if multiline_comment(self) {
                    return None;
                }
                if es.len() == 1 {
                    format!("({},)", self.inl(&es[0], false)?)
                } else {
                    format!("({})", list(es, false)?)
                }
            }
            ExprKind::Record { name, fields } => {
                if multiline_comment(self) {
                    return None;
                }
                let fs = fields.iter().map(|(f, v)| self.inl(v, false).map(|v| format!("{f}: {v}"))).collect::<Option<Vec<_>>>()?;
                let s = if fs.is_empty() { format!("{name} {{}}") } else { format!("{name} {{ {} }}", fs.join(", ")) };
                if head {
                    format!("({s})")
                } else {
                    s
                }
            }
            ExprKind::Parallel(_) | ExprKind::Race(_) => return None,
            ExprKind::Timeout { dur, body } => format!("timeout({}) {}", self.inl(dur, head)?, self.inline_block(body)?),
            ExprKind::Retry { attempts, backoff, body } => {
                let b = match backoff {
                    Some(b) => format!(", backoff: {}", self.inl(b, head)?),
                    None => String::new(),
                };
                format!("retry({}{b}) {}", self.inl(attempts, head)?, self.inline_block(body)?)
            }
        })
    }

    fn operand(&self, c: &Expr, p: u8, rhs: bool, head: bool) -> Option<String> {
        if let ExprKind::Binary(op, ..) = &c.kind {
            let q = op_prec(*op);
            if q < p || (rhs && q == p) {
                return Some(format!("({})", self.inl(c, false)?));
            }
        }
        self.inl(c, head)
    }

    fn unary_operand(&self, c: &Expr, head: bool) -> Option<String> {
        if matches!(c.kind, ExprKind::Binary(..)) {
            return Some(format!("({})", self.inl(c, false)?));
        }
        self.inl(c, head)
    }

    fn target(&self, c: &Expr, head: bool) -> Option<String> {
        if postfix_parens(c) {
            return Some(format!("({})", self.inl(c, false)?));
        }
        self.inl(c, head)
    }

    /// Write `e`, on one line when it fits, wrapped or split over lines otherwise.
    fn expr(&mut self, e: &Expr, head: bool) {
        if let Some(s) = self.inl(e, head) {
            if self.fits(&s) || !wrappable(e) {
                self.w(&s);
                return;
            }
        }
        self.expr_multi(e, head);
    }

    fn operand_multi(&mut self, c: &Expr, p: u8, rhs: bool, head: bool) {
        if let ExprKind::Binary(op, ..) = &c.kind {
            let q = op_prec(*op);
            if q < p || (rhs && q == p) {
                self.w("(");
                self.expr(c, false);
                self.w(")");
                return;
            }
        }
        self.expr(c, head);
    }

    /// `a + b + c` that does not fit: pieces are packed greedily, continuation lines start
    /// with the operator (a `-` stays at the end of the line, the parser would take a line
    /// starting with `-` for a new statement).
    fn binary_chain(&mut self, e: &Expr, head: bool) {
        let ExprKind::Binary(op0, ..) = &e.kind else { return };
        let p = op_prec(*op0);
        let mut rest: Vec<(BinOp, &Expr)> = vec![];
        let mut first = e;
        while let ExprKind::Binary(op, l, r) = &first.kind {
            if op_prec(*op) != p {
                break;
            }
            rest.push((*op, r));
            first = l;
        }
        rest.reverse();
        self.operand_multi(first, p, false, head);
        self.indent += 1;
        for (op, r) in rest {
            let o = op_str(op);
            match self.operand(r, p, true, head) {
                Some(x) if op == BinOp::Sub => {
                    let t = format!(" - {x}");
                    if self.col() + t.chars().count() <= WIDTH {
                        self.w(&t);
                    } else {
                        self.w(" -");
                        self.nl();
                        self.w(&x);
                    }
                }
                Some(x) => {
                    let t = format!(" {o} {x}");
                    if self.col() + t.chars().count() <= WIDTH {
                        self.w(&t);
                    } else {
                        self.nl();
                        self.w(&format!("{o} {x}"));
                    }
                }
                None => {
                    self.w(&format!(" {o} "));
                    self.operand_multi(r, p, true, head);
                }
            }
        }
        self.indent -= 1;
    }

    fn paren_multi(&mut self, c: &Expr, paren: bool, head: bool) {
        if paren {
            self.w("(");
            self.expr(c, false);
            self.w(")");
        } else {
            self.expr(c, head);
        }
    }

    fn expr_multi(&mut self, e: &Expr, head: bool) {
        match &e.kind {
            ExprKind::Binary(..) => self.binary_chain(e, head),
            ExprKind::Neg(x) => self.prefixed("-", x, head),
            ExprKind::Not(x) => self.prefixed("!", x, head),
            ExprKind::Await(x) => self.prefixed("await ", x, head),
            ExprKind::Spawn(x) => self.prefixed("spawn ", x, head),
            ExprKind::Field(x, f) => {
                self.paren_multi(x, postfix_parens(x), head);
                self.w(&format!(".{f}"));
            }
            ExprKind::TupleIdx(x, i) => {
                self.paren_multi(x, postfix_parens(x), head);
                self.w(&format!(".{i}"));
            }
            ExprKind::Try(x) => {
                self.paren_multi(x, postfix_parens(x), head);
                self.w("?");
            }
            ExprKind::Index(x, i) => {
                self.paren_multi(x, postfix_parens(x), head);
                self.w("[");
                self.expr(i, head);
                self.w("]");
            }
            ExprKind::Call { path, args } if args.len() == 1 && matches!(args[0].kind, ExprKind::Call { .. } | ExprKind::Record { .. } | ExprKind::List(_) | ExprKind::Tuple(_)) && wrappable(&args[0]) => {
                self.w(&format!("{}(", path.join(".")));
                self.expr(&args[0], head);
                self.w(")");
            }
            ExprKind::Call { path, args } => {
                let a: Vec<&Expr> = args.iter().collect();
                self.wrapped(&format!("{}(", path.join(".")), ")", &a, head, None);
            }
            ExprKind::List(es) => {
                let a: Vec<&Expr> = es.iter().collect();
                self.wrapped("[", "]", &a, head, None);
            }
            ExprKind::Tuple(es) => {
                let a: Vec<&Expr> = es.iter().collect();
                self.wrapped("(", ")", &a, false, None);
            }
            ExprKind::Record { name, fields } => {
                if head {
                    self.w("(");
                }
                let a: Vec<&Expr> = fields.iter().map(|(_, v)| v).collect();
                let names: Vec<&str> = fields.iter().map(|(f, _)| f.as_str()).collect();
                self.wrapped(&format!("{name} {{"), "}", &a, false, Some(&names));
                if head {
                    self.w(")");
                }
            }
            ExprKind::Block(b) => self.block(b, true),
            ExprKind::If { cond, then, els } => {
                self.w("if ");
                self.expr(cond, true);
                self.w(" ");
                self.block(then, true);
                let mut next = els.as_deref();
                while let Some(x) = next {
                    match &x.kind {
                        ExprKind::If { cond, then, els } => {
                            self.w(" else if ");
                            self.expr(cond, true);
                            self.w(" ");
                            self.block(then, true);
                            next = els.as_deref();
                        }
                        ExprKind::Block(b) => {
                            self.w(" else ");
                            self.block(b, true);
                            next = None;
                        }
                        _ => next = None,
                    }
                }
            }
            ExprKind::Match { scrut, arms } => {
                self.w("match ");
                self.expr(scrut, true);
                if arms.is_empty() {
                    self.w(" {}");
                    return;
                }
                self.w(" {");
                self.nl();
                self.open_body(e.line);
                for (p, b) in arms {
                    self.unit(self.start_expr(b), false);
                    self.w(&format!("{} => ", pat_str(p)));
                    self.expr(b, false);
                    self.last_end = self.end_expr(b);
                    self.nl();
                }
                let cl = self.close_of(e.line);
                self.close_body(cl);
                self.w("}");
                self.trail.push(cl);
            }
            ExprKind::Parallel(es) | ExprKind::Race(es) => {
                self.w(if matches!(e.kind, ExprKind::Parallel(_)) { "parallel" } else { "race" });
                if es.is_empty() {
                    self.w(" {}");
                    return;
                }
                self.w(" {");
                self.nl();
                self.open_body(e.line);
                for b in es {
                    self.unit(self.start_expr(b), false);
                    self.expr(b, false);
                    self.last_end = self.end_expr(b);
                    self.nl();
                }
                let cl = self.close_of(e.line);
                self.close_body(cl);
                self.w("}");
                self.trail.push(cl);
            }
            ExprKind::Timeout { dur, body } => {
                self.w("timeout(");
                self.expr(dur, head);
                self.w(") ");
                self.block(body, false);
            }
            ExprKind::Retry { attempts, backoff, body } => {
                self.w("retry(");
                self.expr(attempts, head);
                if let Some(b) = backoff {
                    self.w(", backoff: ");
                    self.expr(b, head);
                }
                self.w(") ");
                self.block(body, false);
            }
            _ => {
                let s = self.inl(e, head).unwrap_or_default();
                self.w(&s);
            }
        }
    }

    fn prefixed(&mut self, op: &str, x: &Expr, head: bool) {
        self.w(op);
        self.paren_multi(x, matches!(x.kind, ExprKind::Binary(..)), head);
    }

    /// `open` elements `close`, one element per line with trailing commas.
    fn wrapped(&mut self, open: &str, close: &str, items: &[&Expr], head: bool, names: Option<&[&str]>) {
        if items.is_empty() {
            self.w(&format!("{open}{close}"));
            return;
        }
        self.w(open);
        self.nl();
        self.open_body(self.start_expr(items[0]));
        for (i, a) in items.iter().enumerate() {
            self.unit(self.start_expr(a), false);
            if let Some(n) = names {
                self.w(&format!("{}: ", n[i]));
            }
            self.expr(a, head);
            self.w(",");
            self.last_end = self.end_expr(a);
            self.nl();
        }
        self.indent -= 1;
        self.block_start = false;
        self.w(close);
    }
}

fn item_line(it: &Item) -> u32 {
    match it {
        Item::Record { line, .. } | Item::Enum { line, .. } | Item::Extern { line, .. } => *line,
        Item::Func(f) => f.line,
    }
}

fn stmt_parts(s: &Stmt) -> Vec<Part<'_>> {
    use Part::*;
    match s {
        Stmt::Let { names, tuple, mutable, ty, value, .. } => {
            let mut h = String::from("let ");
            if *mutable {
                h.push_str("mut ");
            }
            if *tuple {
                h.push_str(&format!("({})", names.join(", ")));
            } else {
                h.push_str(&names[0]);
            }
            if let Some(t) = ty {
                h.push_str(&format!(": {}", ty_str(t)));
            }
            h.push_str(" = ");
            vec![T(h), E(value, false)]
        }
        Stmt::Assign { name, value, .. } => vec![T(format!("{name} = ")), E(value, false)],
        Stmt::Expr(e) => vec![E(e, false)],
        Stmt::Return(None, _) => vec![T("return".into())],
        Stmt::Return(Some(e), _) => vec![T("return ".into()), E(e, false)],
        Stmt::While { cond, body, .. } => vec![T("while ".into()), E(cond, true), T(" ".into()), B(body)],
        Stmt::For { var, iter, body, .. } => vec![T(format!("for {var} in ")), E(iter, true), T(" ".into()), B(body)],
        Stmt::Ensure { cond, err, .. } => vec![T("ensure ".into()), E(cond, false), T(", ".into()), E(err, false)],
        Stmt::Assert { cond, msg, line } => {
            // The parser fills in this message when none is written; leave it out again.
            let auto = matches!(&msg.kind, ExprKind::Str(s) if *s == format!("assertion failed (line {line})"));
            if auto {
                vec![T("assert ".into()), E(cond, false)]
            } else {
                vec![T("assert ".into()), E(cond, false), T(", ".into()), E(msg, false)]
            }
        }
        Stmt::Break(_) => vec![T("break".into())],
        Stmt::Continue(_) => vec![T("continue".into())],
    }
}

/// A last `expr` without a tail would turn into the block value; `;` keeps it a statement.
fn semi_needed(s: &Stmt) -> bool {
    matches!(s, Stmt::Expr(e) if !matches!(e.kind, ExprKind::Unit))
}

fn leaf_stmt(s: &Stmt) -> bool {
    match s {
        Stmt::While { .. } | Stmt::For { .. } => false,
        Stmt::Let { value, .. } | Stmt::Assign { value, .. } => leaf_expr(value),
        Stmt::Expr(e) => leaf_expr(e),
        Stmt::Return(e, _) => e.as_ref().is_none_or(leaf_expr),
        Stmt::Ensure { cond, err, .. } => leaf_expr(cond) && leaf_expr(err),
        Stmt::Assert { cond, msg, .. } => leaf_expr(cond) && leaf_expr(msg),
        Stmt::Break(_) | Stmt::Continue(_) => true,
    }
}

/// No nested control flow or blocks: safe to keep on one line inside `{ }`.
fn leaf_expr(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::If { .. } | ExprKind::Match { .. } | ExprKind::Block(_) | ExprKind::Parallel(_) | ExprKind::Race(_) | ExprKind::Timeout { .. } | ExprKind::Retry { .. } => false,
        ExprKind::Call { args: es, .. } | ExprKind::List(es) | ExprKind::Tuple(es) => es.iter().all(leaf_expr),
        ExprKind::Field(x, _) | ExprKind::TupleIdx(x, _) | ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Try(x) | ExprKind::Spawn(x) | ExprKind::Await(x) => leaf_expr(x),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => leaf_expr(a) && leaf_expr(b),
        ExprKind::Record { fields, .. } => fields.iter().all(|(_, v)| leaf_expr(v)),
        _ => true,
    }
}

/// Can `expr_multi` produce a better layout than one long line?
fn wrappable(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call { args, .. } => !args.is_empty(),
        ExprKind::List(es) | ExprKind::Tuple(es) => !es.is_empty(),
        ExprKind::Binary(..) => true,
        ExprKind::Record { fields, .. } => !fields.is_empty(),
        ExprKind::If { .. } | ExprKind::Match { .. } | ExprKind::Block(_) | ExprKind::Timeout { .. } | ExprKind::Retry { .. } | ExprKind::Parallel(_) | ExprKind::Race(_) => true,
        ExprKind::Field(x, _) | ExprKind::TupleIdx(x, _) | ExprKind::Index(x, _) | ExprKind::Try(x) | ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Await(x) | ExprKind::Spawn(x) => wrappable(x),
        _ => false,
    }
}

fn postfix_parens(c: &Expr) -> bool {
    matches!(c.kind, ExprKind::Binary(..) | ExprKind::Neg(_) | ExprKind::Not(_) | ExprKind::Await(_) | ExprKind::Spawn(_) | ExprKind::Int(_) | ExprKind::Dur(_))
}

fn op_prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne => 3,
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
        BinOp::Add | BinOp::Sub => 5,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 6,
    }
}

fn op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Or => "||",
        BinOp::And => "&&",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}

fn pat_str(p: &Pattern) -> String {
    match p {
        Pattern::Variant { path, bind } => match bind {
            Some(b) => format!("{}({b})", path.join(".")),
            None => path.join("."),
        },
        Pattern::Int(n) => n.to_string(),
        Pattern::Str(s) => quote(s),
        Pattern::Bool(b) => b.to_string(),
        Pattern::Bind(n) => n.clone(),
        Pattern::Wildcard => "_".into(),
    }
}

fn ty_str(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Name(n, args, _) if args.is_empty() => n.clone(),
        TypeExpr::Name(n, args, _) => format!("{n}<{}>", args.iter().map(ty_str).collect::<Vec<_>>().join(", ")),
        TypeExpr::Tuple(ts, _) => format!("({})", ts.iter().map(ty_str).collect::<Vec<_>>().join(", ")),
    }
}

fn dur_str(ms: i64) -> String {
    if ms != 0 && ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms != 0 && ms % 60_000 == 0 {
        format!("{}min", ms / 60_000)
    } else if ms != 0 && ms % 1_000 == 0 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
