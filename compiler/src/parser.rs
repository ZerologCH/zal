//! Recursive-descent parser for ZAL v0.1. Grammar: docs/language/grammar.md.

use crate::ast::*;
use crate::lexer::{Tok, Token};
use crate::Diag;

pub struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    file: String,
    /// Inside `if`/`while`/`for`/`match` heads `Name {` opens the body, not a record literal.
    no_record: bool,
    /// Current nesting of expressions, blocks, types and patterns.
    depth: u32,
}

/// Deepest nesting the parser accepts. Parsing and every later pass recurse
/// once per level; without a bound a hostile source overflows the stack.
pub const MAX_NESTING: u32 = 512;

type PResult<T> = Result<T, Diag>;

pub fn parse(toks: &[Token], file: &str) -> PResult<Module> {
    let mut p = Parser { toks, pos: 0, file: file.to_string(), no_record: false, depth: 0 };
    p.module()
}

impl<'a> Parser<'a> {
    /// Runs `f` one nesting level deeper, or fails beyond `MAX_NESTING`.
    fn nest<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.depth >= MAX_NESTING {
            return self.err(format!("nesting deeper than {MAX_NESTING} levels"));
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }
    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }
    fn nl_before(&self) -> bool {
        self.toks[self.pos].nl_before
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        let t = &self.toks[self.pos];
        Err(Diag { file: self.file.clone(), line: t.line, col: t.col, msg: msg.into() })
    }
    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Sym(x) if *x == s)
    }
    fn is_kw(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Kw(x) if *x == s)
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, s: &str) -> bool {
        if self.is_kw(s) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_sym(&mut self, s: &str) -> PResult<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            self.err(format!("expected `{s}`, found {}", describe(self.peek())))
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            t => self.err(format!("expected a name, found {}", describe(&t))),
        }
    }
    fn dotted(&mut self) -> PResult<String> {
        let mut s = self.ident()?;
        while self.eat_sym(".") {
            s.push('.');
            s.push_str(&self.ident()?);
        }
        Ok(s)
    }

    fn module(&mut self) -> PResult<Module> {
        if !self.eat_kw("module") {
            return self.err("a ZAL file starts with `module <name>`");
        }
        let name = self.dotted()?;
        let mut imports = Vec::new();
        let mut items = Vec::new();
        while self.is_kw("import") {
            let line = self.line();
            self.bump();
            imports.push((self.dotted()?, line));
        }
        while *self.peek() != Tok::Eof {
            items.push(self.item()?);
        }
        Ok(Module { name, file: self.file.clone(), imports, items })
    }

    fn item(&mut self) -> PResult<Item> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Kw("type") => {
                self.bump();
                let name = self.ident()?;
                self.expect_sym("=")?;
                self.expect_sym("{")?;
                let mut fields = Vec::new();
                while !self.is_sym("}") {
                    let f = self.ident()?;
                    self.expect_sym(":")?;
                    fields.push((f, self.ty()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("}")?;
                Ok(Item::Record { name, fields, line })
            }
            Tok::Kw("enum") => {
                self.bump();
                let name = self.ident()?;
                self.expect_sym("{")?;
                let mut variants = Vec::new();
                while !self.is_sym("}") {
                    let v = self.ident()?;
                    let payload = if self.eat_sym("(") {
                        let t = self.ty()?;
                        self.expect_sym(")")?;
                        Some(t)
                    } else {
                        None
                    };
                    variants.push((v, payload));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("}")?;
                Ok(Item::Enum { name, variants, line })
            }
            Tok::Kw("extern") => {
                self.bump();
                let kind = self.ident()?;
                let name = self.ident()?;
                let params = self.params()?;
                self.expect_sym("->")?;
                let ret = self.ty()?;
                // Contextual qualifier, only directly after the return type.
                let readonly = matches!(self.peek(), Tok::Ident(w) if w == "readonly") && !self.nl_before();
                if readonly {
                    self.bump();
                }
                Ok(Item::Extern { kind, name, params, ret, readonly, line })
            }
            Tok::Kw(k @ ("fn" | "task" | "workflow")) => {
                self.bump();
                let kind = match k {
                    "fn" => FuncKind::Fn,
                    "task" => FuncKind::Task,
                    _ => FuncKind::Workflow,
                };
                let name = self.ident()?;
                let params = self.params()?;
                let ret = if self.eat_sym("->") { Some(self.ty()?) } else { None };
                self.func_rest(kind, name, params, ret, line)
            }
            // `test` is contextual so tools may be called `test`.
            Tok::Ident(t) if t == "test" && matches!(self.peek_at(1), Tok::Str(_)) => {
                self.bump();
                let name = match self.bump() {
                    Tok::Str(s) => s,
                    _ => return self.err("expected test name string"),
                };
                self.func_rest(FuncKind::Test, name, vec![], None, line)
            }
            t => self.err(format!("expected a declaration (type, enum, extern, fn, task, workflow, test), found {}", describe(&t))),
        }
    }

    fn func_rest(
        &mut self,
        kind: FuncKind,
        name: String,
        params: Vec<(String, TypeExpr)>,
        ret: Option<TypeExpr>,
        line: u32,
    ) -> PResult<Item> {
        let mut requires = Vec::new();
        let mut budget = Vec::new();
        loop {
            if self.eat_kw("requires") {
                loop {
                    let mut e = self.ident()?;
                    self.expect_sym(".")?;
                    if self.eat_sym("*") {
                        e.push_str(".*");
                    } else {
                        e.push('.');
                        e.push_str(&self.ident()?);
                    }
                    requires.push(e);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            } else if self.eat_kw("budget") {
                self.expect_sym("{")?;
                while !self.is_sym("}") {
                    let k = if self.eat_kw("timeout") { "timeout".to_string() } else { self.ident()? };
                    self.expect_sym(":")?;
                    budget.push((k, self.expr()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("}")?;
            } else {
                break;
            }
        }
        let body = self.block()?;
        Ok(Item::Func(FuncDecl { kind, name, params, ret, requires, budget, body, line }))
    }

    fn params(&mut self) -> PResult<Vec<(String, TypeExpr)>> {
        self.expect_sym("(")?;
        let mut ps = Vec::new();
        while !self.is_sym(")") {
            let n = self.ident()?;
            self.expect_sym(":")?;
            ps.push((n, self.ty()?));
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(ps)
    }

    fn ty(&mut self) -> PResult<TypeExpr> {
        self.nest(Self::ty_inner)
    }

    fn ty_inner(&mut self) -> PResult<TypeExpr> {
        let line = self.line();
        if self.eat_sym("(") {
            let mut ts = Vec::new();
            while !self.is_sym(")") {
                ts.push(self.ty()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            return Ok(TypeExpr::Tuple(ts, line));
        }
        let name = self.ident()?;
        let mut args = Vec::new();
        if self.eat_sym("<") {
            loop {
                args.push(self.ty()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(">")?;
        }
        Ok(TypeExpr::Name(name, args, line))
    }

    fn block(&mut self) -> PResult<Block> {
        self.nest(Self::block_inner)
    }

    fn block_inner(&mut self) -> PResult<Block> {
        let line = self.line();
        self.expect_sym("{")?;
        let saved = self.no_record;
        self.no_record = false;
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.is_sym("}") {
            if *self.peek() == Tok::Eof {
                return self.err("unclosed block");
            }
            if self.eat_sym(";") {
                continue;
            }
            match self.stmt()? {
                Stmt::Expr(e) if self.is_sym("}") && !matches!(e.kind, ExprKind::Unit) => {
                    tail = Some(Box::new(e));
                }
                s => stmts.push(s),
            }
        }
        self.expect_sym("}")?;
        self.no_record = saved;
        Ok(Block { stmts, tail, line })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Kw("let") => {
                self.bump();
                let mutable = self.eat_kw("mut");
                let (names, tuple) = if self.eat_sym("(") {
                    let mut ns = Vec::new();
                    while !self.is_sym(")") {
                        ns.push(self.ident()?);
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym(")")?;
                    (ns, true)
                } else {
                    (vec![self.ident()?], false)
                };
                let ty = if self.eat_sym(":") { Some(self.ty()?) } else { None };
                self.expect_sym("=")?;
                let value = self.expr()?;
                Ok(Stmt::Let { names, tuple, mutable, ty, value, line })
            }
            Tok::Kw("return") => {
                self.bump();
                if self.is_sym("}") || self.nl_before() {
                    Ok(Stmt::Return(None, line))
                } else {
                    Ok(Stmt::Return(Some(self.expr()?), line))
                }
            }
            Tok::Kw("while") => {
                self.bump();
                let cond = self.head_expr()?;
                let body = self.block()?;
                Ok(Stmt::While { cond, body, line })
            }
            Tok::Kw("for") => {
                self.bump();
                let var = self.ident()?;
                if !self.eat_kw("in") {
                    return self.err("expected `in`");
                }
                let iter = self.head_expr()?;
                let body = self.block()?;
                Ok(Stmt::For { var, iter, body, line })
            }
            Tok::Kw("ensure") => {
                self.bump();
                let cond = self.expr()?;
                self.expect_sym(",")?;
                let err = self.expr()?;
                Ok(Stmt::Ensure { cond, err, line })
            }
            Tok::Kw("assert") => {
                self.bump();
                let cond = self.expr()?;
                let msg = if self.eat_sym(",") {
                    self.expr()?
                } else {
                    Expr { kind: ExprKind::Str(format!("assertion failed (line {line})")), line }
                };
                Ok(Stmt::Assert { cond, msg, line })
            }
            Tok::Kw("break") => {
                self.bump();
                Ok(Stmt::Break(line))
            }
            Tok::Kw("continue") => {
                self.bump();
                Ok(Stmt::Continue(line))
            }
            Tok::Ident(name) if matches!(self.peek_at(1), Tok::Sym("=")) => {
                self.bump();
                self.bump();
                let value = self.expr()?;
                Ok(Stmt::Assign { name, value, line })
            }
            _ => Ok(Stmt::Expr(self.expr()?)),
        }
    }

    fn head_expr(&mut self) -> PResult<Expr> {
        let saved = self.no_record;
        self.no_record = true;
        let e = self.expr();
        self.no_record = saved;
        e
    }

    pub fn expr(&mut self) -> PResult<Expr> {
        self.binary(0)
    }

    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let (op, prec) = match self.peek() {
                Tok::Sym("||") => (BinOp::Or, 1),
                Tok::Sym("&&") => (BinOp::And, 2),
                Tok::Sym("==") => (BinOp::Eq, 3),
                Tok::Sym("!=") => (BinOp::Ne, 3),
                Tok::Sym("<") => (BinOp::Lt, 4),
                Tok::Sym("<=") => (BinOp::Le, 4),
                Tok::Sym(">") => (BinOp::Gt, 4),
                Tok::Sym(">=") => (BinOp::Ge, 4),
                Tok::Sym("+") => (BinOp::Add, 5),
                Tok::Sym("-") => (BinOp::Sub, 5),
                Tok::Sym("*") => (BinOp::Mul, 6),
                Tok::Sym("/") => (BinOp::Div, 6),
                Tok::Sym("%") => (BinOp::Mod, 6),
                _ => break,
            };
            if prec < min_prec || (op == BinOp::Sub && self.nl_before()) {
                break;
            }
            let line = self.line();
            self.bump();
            let rhs = self.binary(prec + 1)?;
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        self.nest(Self::unary_inner)
    }

    fn unary_inner(&mut self) -> PResult<Expr> {
        let line = self.line();
        if self.eat_sym("-") {
            return Ok(Expr { kind: ExprKind::Neg(Box::new(self.unary()?)), line });
        }
        if self.eat_sym("!") {
            return Ok(Expr { kind: ExprKind::Not(Box::new(self.unary()?)), line });
        }
        if self.eat_kw("await") {
            return Ok(Expr { kind: ExprKind::Await(Box::new(self.unary()?)), line });
        }
        if self.eat_kw("spawn") {
            return Ok(Expr { kind: ExprKind::Spawn(Box::new(self.unary()?)), line });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            let line = self.line();
            if self.is_sym("?") {
                self.bump();
                e = Expr { kind: ExprKind::Try(Box::new(e)), line };
            } else if self.is_sym(".") {
                self.bump();
                match self.bump() {
                    Tok::Ident(f) => e = Expr { kind: ExprKind::Field(Box::new(e), f), line },
                    Tok::Int(n) => e = Expr { kind: ExprKind::TupleIdx(Box::new(e), n as u16), line },
                    t => return self.err(format!("expected field name after `.`, found {}", describe(&t))),
                }
            } else if self.is_sym("[") && !self.nl_before() {
                self.bump();
                let idx = self.expr()?;
                self.expect_sym("]")?;
                e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(idx)), line };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn args(&mut self) -> PResult<Vec<Expr>> {
        self.expect_sym("(")?;
        let mut args = Vec::new();
        while !self.is_sym(")") {
            args.push(self.expr()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(args)
    }

    /// Comma- or newline-separated expressions inside `{ }` (parallel/race).
    fn expr_list_block(&mut self) -> PResult<Vec<Expr>> {
        self.expect_sym("{")?;
        let saved = self.no_record;
        self.no_record = false;
        let mut es = Vec::new();
        while !self.is_sym("}") {
            es.push(self.expr()?);
            self.eat_sym(",");
        }
        self.expect_sym("}")?;
        self.no_record = saved;
        Ok(es)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let line = self.line();
        let kind = match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                ExprKind::Int(n)
            }
            Tok::Dur(n) => {
                self.bump();
                ExprKind::Dur(n)
            }
            Tok::Str(s) => {
                self.bump();
                ExprKind::Str(s)
            }
            Tok::Kw("true") => {
                self.bump();
                ExprKind::Bool(true)
            }
            Tok::Kw("false") => {
                self.bump();
                ExprKind::Bool(false)
            }
            Tok::Sym("(") => {
                self.bump();
                if self.eat_sym(")") {
                    ExprKind::Unit
                } else {
                    let saved = self.no_record;
                    self.no_record = false;
                    let first = self.expr()?;
                    let k = if self.eat_sym(",") {
                        let mut es = vec![first];
                        while !self.is_sym(")") {
                            es.push(self.expr()?);
                            if !self.eat_sym(",") {
                                break;
                            }
                        }
                        ExprKind::Tuple(es)
                    } else {
                        self.no_record = saved;
                        self.expect_sym(")")?;
                        return Ok(first);
                    };
                    self.no_record = saved;
                    self.expect_sym(")")?;
                    k
                }
            }
            Tok::Sym("[") => {
                self.bump();
                let mut es = Vec::new();
                while !self.is_sym("]") {
                    es.push(self.expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym("]")?;
                ExprKind::List(es)
            }
            Tok::Sym("{") => ExprKind::Block(self.block()?),
            Tok::Kw("if") => return self.if_expr(),
            Tok::Kw("match") => {
                self.bump();
                let scrut = self.head_expr()?;
                self.expect_sym("{")?;
                let saved = self.no_record;
                self.no_record = false;
                let mut arms = Vec::new();
                while !self.is_sym("}") {
                    let pat = self.pattern()?;
                    self.expect_sym("=>")?;
                    // `return` / `break` / `continue` may stand directly as an
                    // arm (common in agent-written code); it becomes `{ stmt }`.
                    let body = if self.is_kw("return") || self.is_kw("break") || self.is_kw("continue") {
                        let line = self.line();
                        let st = self.stmt()?;
                        Expr { kind: ExprKind::Block(Block { stmts: vec![st], tail: None, line }), line }
                    } else {
                        self.expr()?
                    };
                    arms.push((pat, body));
                    self.eat_sym(",");
                }
                self.expect_sym("}")?;
                self.no_record = saved;
                ExprKind::Match { scrut: Box::new(scrut), arms }
            }
            Tok::Kw("parallel") => {
                self.bump();
                ExprKind::Parallel(self.expr_list_block()?)
            }
            Tok::Kw("race") => {
                self.bump();
                ExprKind::Race(self.expr_list_block()?)
            }
            Tok::Kw("timeout") => {
                self.bump();
                self.expect_sym("(")?;
                let dur = self.expr()?;
                self.expect_sym(")")?;
                ExprKind::Timeout { dur: Box::new(dur), body: self.block()? }
            }
            Tok::Kw("retry") => {
                self.bump();
                self.expect_sym("(")?;
                let attempts = self.expr()?;
                let mut backoff = None;
                if self.eat_sym(",") {
                    let k = self.ident()?;
                    if k != "backoff" {
                        return self.err("retry accepts only `backoff: <Dur>`");
                    }
                    self.expect_sym(":")?;
                    backoff = Some(Box::new(self.expr()?));
                }
                self.expect_sym(")")?;
                ExprKind::Retry { attempts: Box::new(attempts), backoff, body: self.block()? }
            }
            Tok::Ident(name) => {
                self.bump();
                let mut path = vec![name];
                // `Enum.Variant` paths: only when the head is capitalized.
                while path[0].starts_with(char::is_uppercase)
                    && self.is_sym(".")
                    && matches!(self.peek_at(1), Tok::Ident(_))
                {
                    self.bump();
                    path.push(self.ident()?);
                }
                if self.is_sym("(") && !self.nl_before() {
                    let args = self.args()?;
                    ExprKind::Call { path, args }
                } else if self.is_sym("{")
                    && !self.no_record
                    && path.len() == 1
                    && path[0].starts_with(char::is_uppercase)
                {
                    self.bump();
                    let mut fields = Vec::new();
                    while !self.is_sym("}") {
                        let f = self.ident()?;
                        let v = if self.eat_sym(":") {
                            self.expr()?
                        } else {
                            Expr { kind: ExprKind::Var(f.clone()), line: self.line() }
                        };
                        fields.push((f, v));
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym("}")?;
                    ExprKind::Record { name: path.remove(0), fields }
                } else if path.len() > 1 {
                    ExprKind::Path(path)
                } else {
                    ExprKind::Var(path.remove(0))
                }
            }
            t => return self.err(format!("expected an expression, found {}", describe(&t))),
        };
        Ok(Expr { kind, line })
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let line = self.line();
        self.bump();
        let cond = self.head_expr()?;
        let then = self.block()?;
        let els = if self.eat_kw("else") {
            if self.is_kw("if") {
                Some(Box::new(self.if_expr()?))
            } else {
                let b = self.block()?;
                let l = b.line;
                Some(Box::new(Expr { kind: ExprKind::Block(b), line: l }))
            }
        } else {
            None
        };
        Ok(Expr { kind: ExprKind::If { cond: Box::new(cond), then, els }, line })
    }

    fn pattern(&mut self) -> PResult<Pattern> {
        self.nest(Self::pattern_inner)
    }

    fn pattern_inner(&mut self) -> PResult<Pattern> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(Pattern::Int(n))
            }
            Tok::Sym("-") => {
                self.bump();
                match self.bump() {
                    Tok::Int(n) => Ok(Pattern::Int(-n)),
                    _ => self.err("expected integer"),
                }
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Pattern::Str(s))
            }
            Tok::Kw("true") => {
                self.bump();
                Ok(Pattern::Bool(true))
            }
            Tok::Kw("false") => {
                self.bump();
                Ok(Pattern::Bool(false))
            }
            Tok::Ident(n) if n == "_" => {
                self.bump();
                Ok(Pattern::Wildcard)
            }
            Tok::Ident(n) => {
                self.bump();
                let mut path = vec![n];
                while self.eat_sym(".") {
                    path.push(self.ident()?);
                }
                let is_variant = path.len() > 1 || path[0].starts_with(char::is_uppercase);
                if !is_variant {
                    return Ok(Pattern::Bind(path.remove(0)));
                }
                let bind = if self.eat_sym("(") {
                    let b = self.ident()?;
                    self.expect_sym(")")?;
                    Some(b)
                } else {
                    None
                };
                Ok(Pattern::Variant { path, bind })
            }
            t => self.err(format!("expected a pattern, found {}", describe(&t))),
        }
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(s) => format!("name `{s}`"),
        Tok::Int(n) => format!("integer `{n}`"),
        Tok::Str(_) => "string".into(),
        Tok::Dur(_) => "duration".into(),
        Tok::Kw(k) => format!("keyword `{k}`"),
        Tok::Sym(s) => format!("`{s}`"),
        Tok::Eof => "end of file".into(),
    }
}
