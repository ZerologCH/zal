//! Type, effect and capability checking fused with bytecode emission.
//!
//! One pass per function: every expression is typed (with unification for
//! generic builtins and constructors) and lowered to stack bytecode at the same
//! time. Concurrent blocks (`parallel`, `race`, `timeout`, `retry`, `spawn`) are
//! lambda-lifted into their own functions that capture outer locals *by value*;
//! assigning to a captured variable is a compile error, so ZAL programs have no
//! shared mutable state between fibers by construction.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::Diag;
use zal_ir::{self as ir, Builtin, Const, FnKind, Op, Ty};

type CResult<T> = Result<T, Diag>;

struct FnSig {
    idx: u32,
    kind: FuncKind,
    params: Vec<Ty>,
    ret: Ty,
    effects: Vec<String>,
}

struct ExternSig {
    idx: u32,
    params: Vec<Ty>,
    ret: Ty,
    effect: String,
    readonly: bool,
}

struct Local {
    name: String,
    slot: u16,
    ty: Ty,
    mutable: bool,
}

struct Loop {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct FnCtx {
    name: String,
    kind: FnKind,
    scopes: Vec<Vec<Local>>,
    next: u16,
    max: u16,
    code: Vec<Op>,
    lines: Vec<u32>,
    effects: Vec<String>,
    ret: Ty,
    loops: Vec<Loop>,
    lifted: u32,
    params: Vec<(String, Ty)>,
    /// Every slot declaration: (slot, type, hidden temp without a real type).
    decls: Vec<(u16, Ty, bool)>,
}

const RECOGNIZED_EFFECT_KINDS: &[&str] = &["tool", "net", "file", "secret", "worker", "deploy"];

pub struct Compiler {
    file: String,
    records: HashMap<String, Vec<(String, Ty)>>,
    record_order: Vec<String>,
    enums: HashMap<String, Vec<(String, Option<Ty>)>>,
    enum_order: Vec<String>,
    externs: HashMap<String, ExternSig>,
    extern_out: Vec<ir::Extern>,
    funcs: HashMap<String, FnSig>,
    out: Vec<Option<ir::Function>>,
    consts: Vec<Const>,
    const_ix: HashMap<String, u32>,
    subst: Vec<Option<Ty>>,
    ctx: Vec<FnCtx>,
    pending_take: Option<u16>,
    /// Type of every source expression (by address), for the native backend.
    pub types: HashMap<usize, Ty>,
}

fn effect_allowed(declared: &[String], eff: &str) -> bool {
    declared.iter().any(|d| {
        d == eff || (d.ends_with(".*") && eff.starts_with(&d[..d.len() - 1]))
    })
}

pub fn compile(modules: &[Module]) -> Result<ir::Program, Vec<Diag>> {
    compile_opts(modules, std::env::var("ZAL_NO_FUSE").is_err())
}

/// `fuse = false` keeps the raw bytecode; used by differential tests.
pub fn compile_opts(modules: &[Module], fuse_ops: bool) -> Result<ir::Program, Vec<Diag>> {
    compile_typed(modules, fuse_ops).map(|(p, _)| p)
}

/// Compile and also return the resolved type of every expression (by address
/// into `modules`), which the native backend needs.
pub fn compile_typed(modules: &[Module], fuse_ops: bool) -> Result<(ir::Program, HashMap<usize, Ty>), Vec<Diag>> {
    let mut c = Compiler {
        file: String::new(),
        records: HashMap::new(),
        record_order: vec![],
        enums: HashMap::new(),
        enum_order: vec![],
        externs: HashMap::new(),
        extern_out: vec![],
        funcs: HashMap::new(),
        out: vec![],
        consts: vec![],
        const_ix: HashMap::new(),
        subst: vec![],
        ctx: vec![],
        pending_take: None,
        types: HashMap::new(),
    };
    let mut errors = Vec::new();
    if let Err(e) = c.declare(modules) {
        return Err(vec![e]);
    }
    for m in modules {
        c.file = m.file.clone();
        for item in &m.items {
            if let Item::Func(f) = item {
                if let Err(e) = c.function(f) {
                    c.ctx.clear();
                    errors.push(e);
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut functions: Vec<ir::Function> = c.out.into_iter().map(|f| f.expect("all declared functions compiled")).collect();
    if fuse_ops {
        for f in &mut functions {
            let (code, lines) = fuse(&f.code, &f.lines, &c.consts, &f.slot_tags);
            f.code = move_last_reads(&code, &f.slot_tags);
            f.lines = lines;
        }
    }
    // Types left open by inference only matter for display; close them as Unit.
    let subst = c.subst;
    for f in &mut functions {
        f.ret = close(&subst, &f.ret);
        for p in &mut f.params {
            p.1 = close(&subst, &p.1);
        }
    }
    let types: HashMap<usize, Ty> = c.types.iter().map(|(k, t)| (*k, close(&subst, t))).collect();
    let mut prog = ir::Program {
            ir_version: ir::IR_VERSION,
            module: modules.first().map(|m| m.name.clone()).unwrap_or_default(),
            consts: c.consts,
            functions,
            externs: c.extern_out,
            records: c.record_order.iter().map(|n| ir::Record { name: n.clone(), fields: c.records[n].clone() }).collect(),
            enums: c.enum_order.iter().map(|n| ir::Enum { name: n.clone(), variants: c.enums[n].clone() }).collect(),
    };
    let seal = ir::tag_seal(&prog);
    for f in &mut prog.functions {
        f.slot_seal = seal;
    }
    Ok((prog, types))
}

fn close(subst: &[Option<Ty>], t: &Ty) -> Ty {
    match t {
        Ty::Var(v) => match &subst[*v as usize] {
            Some(t) => close(subst, t),
            None => Ty::Unit,
        },
        Ty::List(a) => Ty::List(Box::new(close(subst, a))),
        Ty::Map(a) => Ty::Map(Box::new(close(subst, a))),
        Ty::Option(a) => Ty::Option(Box::new(close(subst, a))),
        Ty::Task(a) => Ty::Task(Box::new(close(subst, a))),
        Ty::Result(a, b) => Ty::Result(Box::new(close(subst, a)), Box::new(close(subst, b))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| close(subst, t)).collect()),
        t => t.clone(),
    }
}

/// Names referenced in an expression tree (over-approximation of free variables).
pub(crate) fn names_in_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Var(n) => {
            out.insert(n.clone());
        }
        ExprKind::Call { args, .. } => args.iter().for_each(|a| names_in_expr(a, out)),
        ExprKind::Field(a, _) | ExprKind::TupleIdx(a, _) | ExprKind::Neg(a) | ExprKind::Not(a) => names_in_expr(a, out),
        ExprKind::Try(a) | ExprKind::Spawn(a) | ExprKind::Await(a) => names_in_expr(a, out),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
            names_in_expr(a, out);
            names_in_expr(b, out);
        }
        ExprKind::If { cond, then, els } => {
            names_in_expr(cond, out);
            names_in_block(then, out);
            if let Some(e) = els {
                names_in_expr(e, out);
            }
        }
        ExprKind::Match { scrut, arms } => {
            names_in_expr(scrut, out);
            arms.iter().for_each(|(_, a)| names_in_expr(a, out));
        }
        ExprKind::Block(b) => names_in_block(b, out),
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Parallel(es) | ExprKind::Race(es) => {
            es.iter().for_each(|a| names_in_expr(a, out))
        }
        ExprKind::Record { fields, .. } => fields.iter().for_each(|(_, a)| names_in_expr(a, out)),
        ExprKind::Timeout { dur, body } => {
            names_in_expr(dur, out);
            names_in_block(body, out);
        }
        ExprKind::Retry { attempts, backoff, body } => {
            names_in_expr(attempts, out);
            if let Some(b) = backoff {
                names_in_expr(b, out);
            }
            names_in_block(body, out);
        }
        _ => {}
    }
}

pub(crate) fn names_in_block(b: &Block, out: &mut HashSet<String>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { value, .. } => names_in_expr(value, out),
            Stmt::Assign { name, value, .. } => {
                out.insert(name.clone());
                names_in_expr(value, out);
            }
            Stmt::Expr(e) => names_in_expr(e, out),
            Stmt::Return(Some(e), _) => names_in_expr(e, out),
            Stmt::While { cond, body, .. } => {
                names_in_expr(cond, out);
                names_in_block(body, out);
            }
            Stmt::For { iter, body, .. } => {
                names_in_expr(iter, out);
                names_in_block(body, out);
            }
            Stmt::Ensure { cond, err, .. } | Stmt::Assert { cond, msg: err, .. } => {
                names_in_expr(cond, out);
                names_in_expr(err, out);
            }
            _ => {}
        }
    }
    if let Some(t) = &b.tail {
        names_in_expr(t, out);
    }
}

enum LiftBody<'a> {
    Expr(&'a Expr),
    Block(&'a Block),
}

impl Compiler {
    fn err<T>(&self, line: u32, msg: impl Into<String>) -> CResult<T> {
        Err(Diag { file: self.file.clone(), line, col: 0, msg: msg.into() })
    }

    // ---------- declarations ----------

    fn declare(&mut self, modules: &[Module]) -> CResult<()> {
        let mut seen: HashSet<String> = HashSet::new();
        // Pass 1: nominal type names, so fields may reference later types.
        for m in modules {
            self.file = m.file.clone();
            for item in &m.items {
                let (name, line) = match item {
                    Item::Record { name, line, .. } | Item::Enum { name, line, .. } => (name, *line),
                    Item::Extern { name, line, .. } => (name, *line),
                    Item::Func(f) if f.kind != FuncKind::Test => (&f.name, f.line),
                    Item::Func(_) => continue,
                };
                if !seen.insert(name.clone()) {
                    return self.err(line, format!("`{name}` is declared twice"));
                }
                match item {
                    Item::Record { name, .. } => {
                        self.records.insert(name.clone(), vec![]);
                        self.record_order.push(name.clone());
                    }
                    Item::Enum { name, .. } => {
                        self.enums.insert(name.clone(), vec![]);
                        self.enum_order.push(name.clone());
                    }
                    _ => {}
                }
            }
        }
        // Pass 2: bodies of types and signatures.
        for m in modules {
            self.file = m.file.clone();
            for item in &m.items {
                match item {
                    Item::Record { name, fields, line } => {
                        let mut fs: Vec<(String, Ty)> = Vec::new();
                        for (f, te) in fields {
                            if fs.iter().any(|(n, _)| n == f) {
                                return self.err(*line, format!("field `{f}` repeated in `{name}`"));
                            }
                            fs.push((f.clone(), self.texpr(te)?));
                        }
                        self.records.insert(name.clone(), fs);
                    }
                    Item::Enum { name, variants, .. } => {
                        let mut vs = Vec::new();
                        for (v, te) in variants {
                            vs.push((v.clone(), te.as_ref().map(|t| self.texpr(t)).transpose()?));
                        }
                        self.enums.insert(name.clone(), vs);
                    }
                    Item::Extern { kind, name, params, ret, readonly, line } => {
                        if !RECOGNIZED_EFFECT_KINDS.contains(&kind.as_str()) {
                            return self.err(*line, format!("unknown extern kind `{kind}` (use one of {})", RECOGNIZED_EFFECT_KINDS.join(", ")));
                        }
                        let ps: Vec<(String, Ty)> = params.iter().map(|(n, t)| Ok((n.clone(), self.texpr(t)?))).collect::<CResult<_>>()?;
                        let ret = self.texpr(ret)?;
                        let effect = format!("{kind}.{name}");
                        let idx = self.extern_out.len() as u32;
                        self.externs.insert(name.clone(), ExternSig { idx, params: ps.iter().map(|p| p.1.clone()).collect(), ret: ret.clone(), effect: effect.clone(), readonly: *readonly });
                        self.extern_out.push(ir::Extern { name: name.clone(), effect, params: ps, ret, readonly: *readonly });
                    }
                    Item::Func(f) => {
                        let params: Vec<Ty> = f.params.iter().map(|(_, t)| self.texpr(t)).collect::<CResult<_>>()?;
                        let ret = match &f.ret {
                            Some(t) => self.texpr(t)?,
                            None => Ty::Unit,
                        };
                        if f.kind == FuncKind::Fn && !f.requires.is_empty() {
                            return self.err(f.line, format!("`fn {}` is pure and cannot declare `requires`; make it a `task`", f.name));
                        }
                        if f.kind != FuncKind::Workflow && !f.budget.is_empty() {
                            return self.err(f.line, "only a `workflow` can declare a budget");
                        }
                        for r in &f.requires {
                            let kind = r.split('.').next().unwrap_or("");
                            if !RECOGNIZED_EFFECT_KINDS.contains(&kind) {
                                return self.err(f.line, format!("unknown effect kind in `requires {r}`"));
                            }
                        }
                        let idx = self.out.len() as u32;
                        self.out.push(None);
                        if f.kind != FuncKind::Test {
                            self.funcs.insert(f.name.clone(), FnSig { idx, kind: f.kind, params, ret, effects: f.requires.clone() });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn texpr(&self, te: &TypeExpr) -> CResult<Ty> {
        match te {
            TypeExpr::Tuple(ts, _) => Ok(Ty::Tuple(ts.iter().map(|t| self.texpr(t)).collect::<CResult<_>>()?)),
            TypeExpr::Name(n, args, line) => {
                let arity = |k: usize| -> CResult<()> {
                    if args.len() != k {
                        self.err(*line, format!("`{n}` takes {k} type argument(s), got {}", args.len()))
                    } else {
                        Ok(())
                    }
                };
                let a = |i: usize| self.texpr(&args[i]).map(Box::new);
                Ok(match n.as_str() {
                    "Int" => { arity(0)?; Ty::Int }
                    "Bool" => { arity(0)?; Ty::Bool }
                    "Str" => { arity(0)?; Ty::Str }
                    "Dur" => { arity(0)?; Ty::Dur }
                    "Unit" => { arity(0)?; Ty::Unit }
                    "List" => { arity(1)?; Ty::List(a(0)?) }
                    "Map" => { arity(1)?; Ty::Map(a(0)?) }
                    "Option" => { arity(1)?; Ty::Option(a(0)?) }
                    "Task" => { arity(1)?; Ty::Task(a(0)?) }
                    "Result" => { arity(2)?; Ty::Result(a(0)?, a(1)?) }
                    _ if self.records.contains_key(n) => { arity(0)?; Ty::Record(n.clone()) }
                    _ if self.enums.contains_key(n) => { arity(0)?; Ty::Enum(n.clone()) }
                    _ => return self.err(*line, format!("unknown type `{n}`")),
                })
            }
        }
    }

    // ---------- type inference ----------

    fn fresh(&mut self) -> Ty {
        self.subst.push(None);
        Ty::Var(self.subst.len() as u32 - 1)
    }

    fn prune(&self, t: &Ty) -> Ty {
        match t {
            Ty::Var(v) => match &self.subst[*v as usize] {
                Some(t) => self.prune(t),
                None => t.clone(),
            },
            Ty::List(a) => Ty::List(Box::new(self.prune(a))),
            Ty::Map(a) => Ty::Map(Box::new(self.prune(a))),
            Ty::Option(a) => Ty::Option(Box::new(self.prune(a))),
            Ty::Task(a) => Ty::Task(Box::new(self.prune(a))),
            Ty::Result(a, b) => Ty::Result(Box::new(self.prune(a)), Box::new(self.prune(b))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.prune(t)).collect()),
            t => t.clone(),
        }
    }

    fn occurs(&self, v: u32, t: &Ty) -> bool {
        match self.prune(t) {
            Ty::Var(w) => v == w,
            Ty::List(a) | Ty::Map(a) | Ty::Option(a) | Ty::Task(a) => self.occurs(v, &a),
            Ty::Result(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            Ty::Tuple(ts) => ts.iter().any(|t| self.occurs(v, t)),
            _ => false,
        }
    }

    fn unify(&mut self, a: &Ty, b: &Ty, line: u32, what: &str) -> CResult<()> {
        let (a, b) = (self.prune(a), self.prune(b));
        let ok = match (&a, &b) {
            (Ty::Never, _) | (_, Ty::Never) => true,
            (Ty::Var(x), Ty::Var(y)) if x == y => true,
            (Ty::Var(x), t) | (t, Ty::Var(x)) => {
                if self.occurs(*x, t) {
                    return self.err(line, format!("{what}: infinite type"));
                }
                self.subst[*x as usize] = Some(t.clone());
                true
            }
            (Ty::List(x), Ty::List(y)) | (Ty::Map(x), Ty::Map(y)) | (Ty::Option(x), Ty::Option(y)) | (Ty::Task(x), Ty::Task(y)) => {
                return self.unify(x, y, line, what);
            }
            (Ty::Result(a1, b1), Ty::Result(a2, b2)) => {
                self.unify(a1, a2, line, what)?;
                return self.unify(b1, b2, line, what);
            }
            (Ty::Tuple(x), Ty::Tuple(y)) if x.len() == y.len() => {
                for (p, q) in x.iter().zip(y.iter()) {
                    self.unify(p, q, line, what)?;
                }
                true
            }
            _ => a == b,
        };
        if ok {
            Ok(())
        } else {
            self.err(line, format!("{what}: expected `{}`, found `{}`", a, b))
        }
    }

    /// Unify two branch types where either may diverge.
    fn join(&mut self, a: &Ty, b: &Ty, line: u32, what: &str) -> CResult<Ty> {
        let (pa, pb) = (self.prune(a), self.prune(b));
        if pa == Ty::Never {
            return Ok(pb);
        }
        if pb == Ty::Never {
            return Ok(pa);
        }
        self.unify(&pa, &pb, line, what)?;
        Ok(self.prune(&pa))
    }

    // ---------- emission helpers ----------

    fn cx(&mut self) -> &mut FnCtx {
        self.ctx.last_mut().expect("inside a function")
    }

    fn emit(&mut self, op: Op, line: u32) -> usize {
        let c = self.cx();
        c.code.push(op);
        c.lines.push(line);
        c.code.len() - 1
    }

    fn here(&mut self) -> usize {
        self.cx().code.len()
    }

    fn patch(&mut self, at: usize, target: usize) {
        let t = target as u32;
        let c = self.cx();
        c.code[at] = match c.code[at] {
            Op::Jump(_) => Op::Jump(t),
            Op::JumpIfFalse(_) => Op::JumpIfFalse(t),
            Op::JumpIfTrue(_) => Op::JumpIfTrue(t),
            other => other,
        };
    }

    fn konst(&mut self, k: Const) -> u32 {
        let key = format!("{:?}", k);
        if let Some(i) = self.const_ix.get(&key) {
            return *i;
        }
        self.consts.push(k);
        let i = self.consts.len() as u32 - 1;
        self.const_ix.insert(key, i);
        i
    }

    fn push_scope(&mut self) {
        self.cx().scopes.push(vec![]);
    }

    fn pop_scope(&mut self) {
        let c = self.cx();
        // Slots are not reused after a scope ends: every declaration keeps
        // its own slot, so its static type (`slot_tags`) holds for the whole
        // function and fused match temporaries stay unique.
        c.scopes.pop();
    }

    fn declare_local(&mut self, name: &str, ty: Ty, mutable: bool) -> u16 {
        let c = self.cx();
        let slot = c.next;
        c.next += 1;
        c.max = c.max.max(c.next);
        c.decls.push((slot, ty.clone(), name.starts_with('$')));
        c.scopes.last_mut().unwrap().push(Local { name: name.to_string(), slot, ty, mutable });
        slot
    }

    fn temp(&mut self) -> u16 {
        self.declare_local("$tmp", Ty::Unit, true)
    }

    /// A hidden temporary whose static type is known (for-loop slots).
    fn temp_typed(&mut self, ty: Ty) -> u16 {
        let s = self.declare_local("$tmp", ty, true);
        self.cx().decls.last_mut().unwrap().2 = false;
        s
    }

    /// `ir::Function::slot_tags`: a slot gets a tag only when every
    /// declaration of it has the same, fully resolved tag.
    fn slot_tags(&self, c: &FnCtx) -> Vec<u16> {
        fn tag(t: &Ty) -> Option<u16> {
            Some(match t {
                Ty::Unit => 0,
                Ty::Int => 1,
                Ty::Bool => 2,
                Ty::Str => 3,
                Ty::Dur => 4,
                Ty::List(_) => 5,
                Ty::Map(_) => 6,
                Ty::Tuple(_) => 7,
                Ty::Record(_) => 8,
                Ty::Option(_) | Ty::Result(..) | Ty::Enum(_) => 9,
                Ty::Task(_) => 10,
                Ty::Var(_) | Ty::Never => return None,
            })
        }
        let enc = |t: &Ty| -> u16 {
            let t = self.prune(t);
            match tag(&t) {
                None => 0xFFFF,
                Some(v) => {
                    let elem = match &t {
                        Ty::List(e) => tag(&self.prune(e)).unwrap_or(0xFF),
                        // Record: index into Program::records (field types).
                        Ty::Record(n) => self.record_order.iter().position(|r| r == n).filter(|i| *i < 0xFF).map_or(0xFF, |i| i as u16),
                        _ => 0xFF,
                    };
                    v | (elem << 8)
                }
            }
        };
        let mut out = vec![0xFFFEu16; c.max as usize];
        for (slot, ty, hidden) in &c.decls {
            let e = if *hidden { 0xFFFF } else { enc(ty) };
            let o = &mut out[*slot as usize];
            *o = if *o == 0xFFFE || *o == e { e } else { 0xFFFF };
        }
        for o in out.iter_mut() {
            if *o == 0xFFFE {
                *o = 0xFFFF;
            }
        }
        out
    }

    fn lookup(&self, name: &str) -> Option<(u16, Ty, bool)> {
        let c = self.ctx.last()?;
        for s in c.scopes.iter().rev() {
            for l in s.iter().rev() {
                if l.name == name {
                    return Some((l.slot, l.ty.clone(), l.mutable));
                }
            }
        }
        None
    }

    fn check_effects(&self, needed: &[String], line: u32, callee: &str) -> CResult<()> {
        let c = self.ctx.last().unwrap();
        for e in needed {
            if !effect_allowed(&c.effects, e) {
                let hint = match c.kind {
                    FnKind::Fn => format!("`fn {}` is pure; only a `task` or `workflow` may use effects", c.name),
                    _ => format!("add `requires {e}` to `{}`", c.name.split('$').next().unwrap_or(&c.name)),
                };
                return self.err(line, format!("calling `{callee}` needs capability `{e}`; {hint}"));
            }
        }
        Ok(())
    }

    // ---------- functions ----------

    fn function(&mut self, f: &FuncDecl) -> CResult<()> {
        let (idx, params, ret, effects) = match self.funcs.get(&f.name) {
            Some(s) if f.kind != FuncKind::Test => (s.idx, s.params.clone(), s.ret.clone(), s.effects.clone()),
            _ => {
                // Tests are not callable; find their reserved slot by position.
                let idx = self.out.iter().position(|o| o.is_none()).unwrap() as u32;
                (idx, vec![], Ty::Unit, f.requires.clone())
            }
        };
        let kind = match f.kind {
            FuncKind::Fn => FnKind::Fn,
            FuncKind::Task => FnKind::Task,
            FuncKind::Workflow => FnKind::Workflow,
            FuncKind::Test => FnKind::Test,
        };
        let budget = self.budget(f)?;
        let named: Vec<(String, Ty)> = f.params.iter().map(|p| p.0.clone()).zip(params.iter().cloned()).collect();
        self.ctx.push(FnCtx {
            name: f.name.clone(),
            kind,
            scopes: vec![vec![]],
            next: 0,
            max: 0,
            code: vec![],
            lines: vec![],
            effects: effects.clone(),
            ret: ret.clone(),
            loops: vec![],
            lifted: 0,
            params: named.clone(),
            decls: vec![],
        });
        for (n, t) in &named {
            self.declare_local(n, t.clone(), false);
        }
        let t = self.block(&f.body)?;
        self.unify(&ret, &t, f.body.line, &format!("return value of `{}`", f.name))?;
        self.emit(Op::Ret, f.line);
        let c = self.ctx.pop().unwrap();
        let slot_tags = self.slot_tags(&c);
        self.out[idx as usize] = Some(ir::Function {
            name: f.name.clone(),
            kind,
            params: c.params,
            ret,
            locals: c.max,
            code: c.code,
            lines: c.lines,
            effects,
            budget,
            slot_tags,
            slot_seal: 0,
        });
        Ok(())
    }

    fn budget(&self, f: &FuncDecl) -> CResult<Option<ir::Budget>> {
        if f.budget.is_empty() {
            return Ok(None);
        }
        let mut b = ir::Budget::default();
        let mut seen = HashSet::new();
        for (k, e) in &f.budget {
            if !seen.insert(k.as_str()) {
                return self.err(e.line, format!("budget `{k}` given twice"));
            }
            match (k.as_str(), &e.kind) {
                ("timeout", ExprKind::Dur(ms)) if *ms > 0 => b.timeout_ms = Some(*ms),
                ("max_parallel", ExprKind::Int(n)) if *n > 0 && u32::try_from(*n).is_ok() => b.max_parallel = Some(*n as u32),
                ("max_tool_calls", ExprKind::Int(n)) if *n > 0 => b.max_tool_calls = Some(*n as u64),
                ("timeout" | "max_parallel" | "max_tool_calls", _) => {
                    return self.err(e.line, format!("budget `{k}` needs a positive literal"))
                }
                _ => return self.err(e.line, format!("unknown budget key `{k}` (timeout, max_parallel, max_tool_calls)")),
            }
        }
        Ok(Some(b))
    }

    fn lift(&mut self, body: LiftBody, line: u32) -> CResult<(u32, Vec<u16>, Ty)> {
        let mut names = HashSet::new();
        match body {
            LiftBody::Expr(e) => names_in_expr(e, &mut names),
            LiftBody::Block(b) => names_in_block(b, &mut names),
        }
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        let mut caps = Vec::new();
        for n in names {
            if let Some((slot, ty, _)) = self.lookup(&n) {
                caps.push((n, slot, ty));
            }
        }
        let parent = self.ctx.last_mut().unwrap();
        parent.lifted += 1;
        let name = format!("{}$lift{}", parent.name, parent.lifted);
        let effects = parent.effects.clone();
        let ret = self.fresh();
        let params: Vec<(String, Ty)> = caps.iter().map(|(n, _, t)| (n.clone(), t.clone())).collect();
        self.ctx.push(FnCtx {
            name: name.clone(),
            kind: FnKind::Lifted,
            scopes: vec![vec![]],
            next: 0,
            max: 0,
            code: vec![],
            lines: vec![],
            effects: effects.clone(),
            ret: ret.clone(),
            loops: vec![],
            lifted: 0,
            params: params.clone(),
            decls: vec![],
        });
        for (n, t) in &params {
            self.declare_local(n, t.clone(), false);
        }
        let t = match body {
            LiftBody::Expr(e) => self.expr(e)?,
            LiftBody::Block(b) => self.block(b)?,
        };
        self.unify(&ret, &t, line, "concurrent block")?;
        self.emit(Op::Ret, line);
        let c = self.ctx.pop().unwrap();
        let slot_tags = self.slot_tags(&c);
        let idx = self.out.len() as u32;
        let rt = self.prune(&ret);
        self.out.push(Some(ir::Function {
            name,
            kind: FnKind::Lifted,
            params: c.params,
            ret: rt.clone(),
            locals: c.max,
            code: c.code,
            lines: c.lines,
            effects,
            budget: None,
            slot_tags,
            slot_seal: 0,
        }));
        Ok((idx, caps.iter().map(|c| c.1).collect(), rt))
    }

    fn spawn_lifted(&mut self, body: LiftBody, line: u32) -> CResult<Ty> {
        let (idx, caps, t) = self.lift(body, line)?;
        // `spawn f(a, b)` with plain variables as arguments: the lifted body is
        // only `LoadLocal(param)...; Call(f, n); Ret`. Spawn `f` directly with
        // the captured values (same values, same order; reading a captured
        // variable has no effect), which saves a frame and a call per task.
        if let Some(Some(func)) = self.out.get(idx as usize) {
            let code = &func.code;
            let n = code.len();
            if n >= 2 && code[n - 1] == Op::Ret {
                if let Op::Call(f, argc) = code[n - 2] {
                    let loads: Option<Vec<u16>> = code[..n - 2]
                        .iter()
                        .map(|o| match *o {
                            Op::LoadLocal(p) if (p as usize) < caps.len() => Some(caps[p as usize]),
                            _ => None,
                        })
                        .collect();
                    if let Some(loads) = loads.filter(|l| l.len() == argc as usize) {
                        for s in &loads {
                            self.emit(Op::LoadLocal(*s), line);
                        }
                        self.emit(Op::Spawn(f, argc), line);
                        return Ok(t);
                    }
                }
            }
        }
        for s in &caps {
            self.emit(Op::LoadLocal(*s), line);
        }
        self.emit(Op::Spawn(idx, caps.len() as u16), line);
        Ok(t)
    }

    // ---------- statements ----------

    fn block(&mut self, b: &Block) -> CResult<Ty> {
        self.push_scope();
        let mut diverges = false;
        let stmts = self.autopar(&b.stmts);
        for s in stmts.iter() {
            diverges = self.stmt(s)?;
        }
        let t = match &b.tail {
            Some(e) => self.expr(e)?,
            None => {
                self.emit(Op::Unit, b.line);
                if diverges { Ty::Never } else { Ty::Unit }
            }
        };
        self.pop_scope();
        Ok(t)
    }

    /// Is `e` a call of a `readonly` extern, possibly followed by `?`?
    fn autopar<'s>(&self, stmts: &'s [Stmt]) -> Vec<std::borrow::Cow<'s, Stmt>> {
        let ro = |n: &str| self.externs.get(n).is_some_and(|x| x.readonly);
        autopar(stmts, &ro)
    }

    /// Compile a condition as control flow: returns the jumps to patch to the
    /// false target. `a && b` becomes two conditional jumps instead of a
    /// materialized Bool, which also lets `Lt; JumpIfFalse` fuse.
    fn cond_false(&mut self, e: &Expr, line: u32, what: &str) -> CResult<Vec<usize>> {
        if let ExprKind::Binary(BinOp::And, l, r) = &e.kind {
            let mut js = self.cond_false(l, line, what)?;
            js.extend(self.cond_false(r, line, what)?);
            return Ok(js);
        }
        let t = self.expr(e)?;
        self.unify(&Ty::Bool, &t, line, what)?;
        Ok(vec![self.emit(Op::JumpIfFalse(0), line)])
    }

    /// Compile a block for its effects only: leaves nothing on the stack.
    fn block_stmt(&mut self, b: &Block) -> CResult<()> {
        self.push_scope();
        let stmts = self.autopar(&b.stmts);
        for s in stmts.iter() {
            self.stmt(s)?;
        }
        if let Some(e) = &b.tail {
            self.expr_stmt(e)?;
        }
        self.pop_scope();
        Ok(())
    }

    /// An expression used as a statement (value discarded). Takes the source
    /// expression by reference so its recorded types stay keyed to the AST.
    fn expr_stmt(&mut self, e: &Expr) -> CResult<bool> {
        match &e.kind {
            ExprKind::If { cond, then, els: None } => {
                let jfs = self.cond_false(cond, e.line, "if condition")?;
                self.block_stmt(then)?;
                let end = self.here();
                for j in jfs {
                    self.patch(j, end);
                }
                Ok(false)
            }
            ExprKind::Match { scrut, arms } => {
                self.matche_mode(scrut, arms, e.line, true)?;
                Ok(false)
            }
            _ => {
                let t = self.expr(e)?;
                self.emit(Op::Pop, e.line);
                Ok(self.prune(&t) == Ty::Never)
            }
        }
    }

    /// Returns true when the statement unconditionally leaves the block.
    fn stmt(&mut self, s: &Stmt) -> CResult<bool> {
        match s {
            Stmt::Let { names, tuple, mutable, ty, value, line } => {
                let mut t = self.expr(value)?;
                if let Some(te) = ty {
                    let want = self.texpr(te)?;
                    self.unify(&want, &t, *line, "let binding")?;
                    t = want;
                }
                if *tuple {
                    let parts: Vec<Ty> = (0..names.len()).map(|_| self.fresh()).collect();
                    self.unify(&Ty::Tuple(parts.clone()), &t, *line, "tuple destructuring")?;
                    // SplitTuple pushes the elements in order; store them back to front.
                    self.emit(Op::SplitTuple(names.len() as u16), *line);
                    let slots: Vec<u16> = names.iter().enumerate().map(|(i, n)| self.declare_local(n, parts[i].clone(), *mutable)).collect();
                    for slot in slots.iter().rev() {
                        self.emit(Op::StoreLocal(*slot), *line);
                    }
                } else {
                    let slot = self.declare_local(&names[0], t, *mutable);
                    self.emit(Op::StoreLocal(slot), *line);
                }
                Ok(false)
            }
            Stmt::Assign { name, value, line } => {
                let Some((slot, ty, mutable)) = self.lookup(name) else {
                    return self.err(*line, format!("unknown variable `{name}`"));
                };
                if !mutable {
                    let captured = self.ctx.last().unwrap().kind == FnKind::Lifted;
                    let why = if captured { " (variables captured by a concurrent block are read-only copies)" } else { "; declare it with `let mut`" };
                    return self.err(*line, format!("cannot assign to `{name}`{why}"));
                }
                // `x = push(x, ..)` and friends: evaluate the other arguments into
                // temporaries first, then *move* x into the call so its container
                // stays uniquely owned and is updated in place instead of copied.
                if let ExprKind::Call { path, args } = &value.kind {
                    let in_place = matches!(path.as_slice(), [f] if ["push", "set", "remove", "map_set"].contains(&f.as_str()));
                    if in_place && matches!(args.first(), Some(Expr { kind: ExprKind::Var(v), .. }) if v == name) {
                        self.push_scope();
                        let mut new_args = vec![args[0].clone()];
                        for (i, a) in args[1..].iter().enumerate() {
                            let simple = matches!(&a.kind, ExprKind::Int(_) | ExprKind::Str(_) | ExprKind::Bool(_))
                                || matches!(&a.kind, ExprKind::Var(v) if v != name);
                            if simple {
                                new_args.push(a.clone());
                                continue;
                            }
                            let t = self.expr(a)?;
                            let tmp = format!("$arg{i}");
                            let s = self.declare_local(&tmp, t, false);
                            self.emit(Op::StoreLocal(s), a.line);
                            new_args.push(Expr { kind: ExprKind::Var(tmp), line: a.line });
                        }
                        self.pending_take = Some(slot);
                        let call = Expr { kind: ExprKind::Call { path: path.clone(), args: new_args }, line: value.line };
                        let t = self.expr(&call)?;
                        self.pending_take = None;
                        self.unify(&ty, &t, *line, &format!("assignment to `{name}`"))?;
                        self.emit(Op::StoreLocal(slot), *line);
                        self.pop_scope();
                        return Ok(false);
                    }
                }
                let t = self.expr(value)?;
                self.unify(&ty, &t, *line, &format!("assignment to `{name}`"))?;
                self.emit(Op::StoreLocal(slot), *line);
                Ok(false)
            }
            Stmt::Expr(e) => self.expr_stmt(e),
            Stmt::Return(e, line) => {
                let t = match e {
                    Some(e) => self.expr(e)?,
                    None => {
                        self.emit(Op::Unit, *line);
                        Ty::Unit
                    }
                };
                let ret = self.ctx.last().unwrap().ret.clone();
                self.unify(&ret, &t, *line, "return value")?;
                self.emit(Op::Ret, *line);
                Ok(true)
            }
            Stmt::While { cond, body, line } => {
                let start = self.here();
                let jfs = self.cond_false(cond, *line, "while condition")?;
                self.cx().loops.push(Loop { breaks: vec![], continues: vec![] });
                self.block_stmt(body)?;
                self.emit(Op::Jump(start as u32), *line);
                let end = self.here();
                for j in jfs {
                    self.patch(j, end);
                }
                let lp = self.cx().loops.pop().unwrap();
                for b in lp.breaks {
                    self.patch(b, end);
                }
                for c in lp.continues {
                    self.patch(c, start);
                }
                Ok(false)
            }
            Stmt::For { var, iter, body, line } => {
                let it = self.expr(iter)?;
                let elem = self.fresh();
                self.unify(&Ty::List(Box::new(elem.clone())), &it, *line, "for loop over")?;
                self.push_scope();
                // list, index and element occupy three consecutive slots for ForNext.
                let list = self.temp_typed(Ty::List(Box::new(elem.clone())));
                self.emit(Op::StoreLocal(list), *line);
                let i = self.temp_typed(Ty::Int);
                let zero = self.konst(Const::Int(0));
                self.emit(Op::Const(zero), *line);
                self.emit(Op::StoreLocal(i), *line);
                let v = self.declare_local(var, elem, false);
                debug_assert!(i == list + 1 && v == list + 2);
                let top = self.emit(Op::ForNext(list, 0), *line);
                self.cx().loops.push(Loop { breaks: vec![], continues: vec![] });
                self.block_stmt(body)?;
                self.emit(Op::Jump(top as u32), *line);
                let end = self.here();
                self.cx().code[top] = Op::ForNext(list, end as u32);
                let lp = self.cx().loops.pop().unwrap();
                for b in lp.breaks {
                    self.patch(b, end);
                }
                for c in lp.continues {
                    self.patch(c, top);
                }
                self.pop_scope();
                Ok(false)
            }
            Stmt::Ensure { cond, err, line } => {
                let ct = self.expr(cond)?;
                self.unify(&Ty::Bool, &ct, *line, "ensure condition")?;
                let jt = self.emit(Op::JumpIfTrue(0), *line);
                let et = self.expr(err)?;
                let ok = self.fresh();
                let ret = self.ctx.last().unwrap().ret.clone();
                self.unify(&ret, &Ty::Result(Box::new(ok), Box::new(et)), *line, "`ensure` needs the function to return Result<_, E> where E is the error's type")?;
                self.emit(Op::MakeVariant(1, true), *line);
                self.emit(Op::Ret, *line);
                let end = self.here();
                self.patch(jt, end);
                Ok(false)
            }
            Stmt::Assert { cond, msg, line } => {
                let ct = self.expr(cond)?;
                self.unify(&Ty::Bool, &ct, *line, "assert condition")?;
                let jt = self.emit(Op::JumpIfTrue(0), *line);
                let mt = self.expr(msg)?;
                self.unify(&Ty::Str, &mt, *line, "assert message")?;
                self.emit(Op::Trap, *line);
                let end = self.here();
                self.patch(jt, end);
                Ok(false)
            }
            Stmt::Break(line) | Stmt::Continue(line) => {
                if self.ctx.last().unwrap().loops.is_empty() {
                    return self.err(*line, "`break`/`continue` outside a loop (loops do not cross concurrent blocks)");
                }
                let j = self.emit(Op::Jump(0), *line);
                let lp = self.cx().loops.last_mut().unwrap();
                if matches!(s, Stmt::Break(_)) {
                    lp.breaks.push(j);
                } else {
                    lp.continues.push(j);
                }
                Ok(true)
            }
        }
    }

    // ---------- expressions ----------

    fn expr(&mut self, e: &Expr) -> CResult<Ty> {
        let t = self.expr_inner(e)?;
        self.types.insert(e as *const Expr as usize, t.clone());
        Ok(t)
    }

    fn expr_inner(&mut self, e: &Expr) -> CResult<Ty> {
        let line = e.line;
        match &e.kind {
            ExprKind::Int(n) => {
                let k = self.konst(Const::Int(*n));
                self.emit(Op::Const(k), line);
                Ok(Ty::Int)
            }
            ExprKind::Str(s) => {
                let k = self.konst(Const::Str(s.clone()));
                self.emit(Op::Const(k), line);
                Ok(Ty::Str)
            }
            ExprKind::Bool(b) => {
                let k = self.konst(Const::Bool(*b));
                self.emit(Op::Const(k), line);
                Ok(Ty::Bool)
            }
            ExprKind::Dur(ms) => {
                let k = self.konst(Const::Dur(*ms));
                self.emit(Op::Const(k), line);
                Ok(Ty::Dur)
            }
            ExprKind::Unit => {
                self.emit(Op::Unit, line);
                Ok(Ty::Unit)
            }
            ExprKind::Var(n) => {
                if let Some((slot, ty, _)) = self.lookup(n) {
                    if self.pending_take == Some(slot) {
                        self.pending_take = None;
                        self.emit(Op::TakeLocal(slot), line);
                    } else {
                        self.emit(Op::LoadLocal(slot), line);
                    }
                    return Ok(ty);
                }
                if n == "None" {
                    self.emit(Op::MakeVariant(0, false), line);
                    let t = self.fresh();
                    return Ok(Ty::Option(Box::new(t)));
                }
                self.err(line, format!("unknown variable `{n}`"))
            }
            ExprKind::Path(path) => {
                let (en, tag, payload) = self.variant_of(path, line)?;
                if payload.is_some() {
                    return self.err(line, format!("variant `{}` carries a value; write `{}(...)`", path.join("."), path.join(".")));
                }
                self.emit(Op::MakeVariant(tag, false), line);
                Ok(Ty::Enum(en))
            }
            ExprKind::Call { path, args } => self.call(path, args, line),
            ExprKind::Field(obj, f) => {
                let t = self.expr(obj)?;
                match self.prune(&t) {
                    Ty::Record(r) => {
                        let fields = &self.records[&r];
                        match fields.iter().position(|(n, _)| n == f) {
                            Some(i) => {
                                let ft = fields[i].1.clone();
                                self.emit(Op::FieldGet(i as u16), line);
                                Ok(ft)
                            }
                            None => self.err(line, format!("record `{r}` has no field `{f}`")),
                        }
                    }
                    Ty::Var(_) => self.err(line, format!("cannot infer the record type for `.{f}`; add a type annotation")),
                    t => self.err(line, format!("`{t}` has no fields")),
                }
            }
            ExprKind::TupleIdx(obj, i) => {
                let t = self.expr(obj)?;
                match self.prune(&t) {
                    Ty::Tuple(ts) if (*i as usize) < ts.len() => {
                        self.emit(Op::TupleGet(*i), line);
                        Ok(ts[*i as usize].clone())
                    }
                    t => self.err(line, format!("`{t}` has no element .{i}")),
                }
            }
            ExprKind::Index(obj, idx) => {
                let t = self.expr(obj)?;
                let elem = self.fresh();
                self.unify(&Ty::List(Box::new(elem.clone())), &t, line, "indexing")?;
                let it = self.expr(idx)?;
                self.unify(&Ty::Int, &it, line, "list index")?;
                self.emit(Op::Index, line);
                Ok(elem)
            }
            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, line),
            ExprKind::Neg(a) => {
                let t = self.expr(a)?;
                match self.prune(&t) {
                    Ty::Int | Ty::Dur => {
                        self.emit(Op::Neg, line);
                        Ok(t)
                    }
                    t => self.err(line, format!("cannot negate `{t}`")),
                }
            }
            ExprKind::Not(a) => {
                let t = self.expr(a)?;
                self.unify(&Ty::Bool, &t, line, "`!` operand")?;
                self.emit(Op::Not, line);
                Ok(Ty::Bool)
            }
            ExprKind::If { cond, then, els } => {
                let jfs = self.cond_false(cond, line, "if condition")?;
                let tt = self.block(then)?;
                match els {
                    Some(els) => {
                        let jend = self.emit(Op::Jump(0), line);
                        let l = self.here();
                        for j in &jfs {
                            self.patch(*j, l);
                        }
                        let et = self.expr(els)?;
                        let end = self.here();
                        self.patch(jend, end);
                        self.join(&tt, &et, line, "if/else branches")
                    }
                    None => {
                        self.emit(Op::Pop, line);
                        let jend = self.emit(Op::Jump(0), line);
                        let l = self.here();
                        for j in &jfs {
                            self.patch(*j, l);
                        }
                        let end = self.here();
                        self.patch(jend, end);
                        self.emit(Op::Unit, line);
                        Ok(Ty::Unit)
                    }
                }
            }
            ExprKind::Match { scrut, arms } => self.matche(scrut, arms, line),
            ExprKind::Block(b) => self.block(b),
            ExprKind::List(es) => {
                let elem = self.fresh();
                for x in es {
                    let t = self.expr(x)?;
                    self.unify(&elem, &t, x.line, "list element")?;
                }
                self.emit(Op::MakeList(es.len() as u16), line);
                Ok(Ty::List(Box::new(elem)))
            }
            ExprKind::Tuple(es) => {
                let mut ts = Vec::new();
                for x in es {
                    ts.push(self.expr(x)?);
                }
                self.emit(Op::MakeTuple(es.len() as u16), line);
                Ok(Ty::Tuple(ts))
            }
            ExprKind::Record { name, fields } => {
                let Some(decl) = self.records.get(name).cloned() else {
                    return self.err(line, format!("unknown record type `{name}`"));
                };
                for (f, _) in fields {
                    if !decl.iter().any(|(n, _)| n == f) {
                        return self.err(line, format!("record `{name}` has no field `{f}`"));
                    }
                    if fields.iter().filter(|(n, _)| n == f).count() > 1 {
                        return self.err(line, format!("field `{f}` given twice"));
                    }
                }
                for (f, ft) in &decl {
                    let Some((_, v)) = fields.iter().find(|(n, _)| n == f) else {
                        return self.err(line, format!("missing field `{f}` in `{name}`"));
                    };
                    let t = self.expr(v)?;
                    self.unify(ft, &t, v.line, &format!("field `{f}`"))?;
                }
                self.emit(Op::MakeRecord(decl.len() as u16), line);
                Ok(Ty::Record(name.clone()))
            }
            ExprKind::Try(inner) => {
                let t = self.expr(inner)?;
                let tmp = self.temp_typed(t.clone());
                self.emit(Op::StoreLocal(tmp), line);
                let ret = self.ctx.last().unwrap().ret.clone();
                let (fail_tag, ok_t) = match self.prune(&t) {
                    Ty::Result(a, b) => {
                        let any = self.fresh();
                        self.unify(&ret, &Ty::Result(Box::new(any), b.clone()), line, "`?` needs the enclosing function to return Result with the same error type")?;
                        (1u16, *a)
                    }
                    Ty::Option(a) => {
                        let any = self.fresh();
                        self.unify(&ret, &Ty::Option(Box::new(any)), line, "`?` on Option needs the enclosing function to return Option")?;
                        (0u16, *a)
                    }
                    t => return self.err(line, format!("`?` applies to Result or Option, not `{t}`")),
                };
                self.emit(Op::LoadLocal(tmp), line);
                self.emit(Op::TagIs(fail_tag), line);
                let jf = self.emit(Op::JumpIfFalse(0), line);
                self.emit(Op::LoadLocal(tmp), line);
                self.emit(Op::Ret, line);
                let ok = self.here();
                self.patch(jf, ok);
                self.emit(Op::LoadLocal(tmp), line);
                self.emit(Op::Payload, line);
                Ok(ok_t)
            }
            ExprKind::Parallel(es) => {
                if es.is_empty() {
                    return self.err(line, "empty `parallel` block");
                }
                let mut ts = Vec::new();
                for x in es {
                    ts.push(self.spawn_lifted(LiftBody::Expr(x), x.line)?);
                }
                self.emit(Op::AwaitAll(es.len() as u16), line);
                Ok(Ty::Tuple(ts))
            }
            ExprKind::Race(es) => {
                if es.is_empty() {
                    return self.err(line, "empty `race` block");
                }
                let t = self.fresh();
                for x in es {
                    let bt = self.spawn_lifted(LiftBody::Expr(x), x.line)?;
                    self.unify(&t, &bt, x.line, "race branches")?;
                }
                self.emit(Op::Race(es.len() as u16), line);
                Ok(t)
            }
            ExprKind::Timeout { dur, body } => {
                let t = self.spawn_lifted(LiftBody::Block(body), line)?;
                let dt = self.expr(dur)?;
                self.unify(&Ty::Dur, &dt, line, "timeout duration")?;
                self.emit(Op::AwaitTimeout, line);
                Ok(Ty::Option(Box::new(t)))
            }
            ExprKind::Retry { attempts, backoff, body } => {
                let at = self.expr(attempts)?;
                self.unify(&Ty::Int, &at, line, "retry attempts")?;
                self.push_scope();
                let a = self.temp();
                self.emit(Op::StoreLocal(a), line);
                let i = self.temp();
                let zero = self.konst(Const::Int(0));
                let one = self.konst(Const::Int(1));
                self.emit(Op::Const(zero), line);
                self.emit(Op::StoreLocal(i), line);
                let (fidx, caps, t) = self.lift(LiftBody::Block(body), line)?;
                let (ok, er) = (self.fresh(), self.fresh());
                self.unify(&Ty::Result(Box::new(ok), Box::new(er)), &t, line, "a `retry` body must produce a Result")?;
                let r = self.temp();
                let top = self.here();
                for s in &caps {
                    self.emit(Op::LoadLocal(*s), line);
                }
                self.emit(Op::Call(fidx, caps.len() as u16), line);
                self.emit(Op::StoreLocal(r), line);
                self.emit(Op::LoadLocal(i), line);
                self.emit(Op::Const(one), line);
                self.emit(Op::Add, line);
                self.emit(Op::StoreLocal(i), line);
                self.emit(Op::LoadLocal(r), line);
                self.emit(Op::TagIs(0), line);
                let j_ok = self.emit(Op::JumpIfTrue(0), line);
                self.emit(Op::LoadLocal(i), line);
                self.emit(Op::LoadLocal(a), line);
                self.emit(Op::Lt, line);
                let j_out = self.emit(Op::JumpIfFalse(0), line);
                if let Some(b) = backoff {
                    let bt = self.expr(b)?;
                    self.unify(&Ty::Dur, &bt, line, "retry backoff")?;
                    self.emit(Op::Builtin(Builtin::Sleep, 1), line);
                    self.emit(Op::Pop, line);
                }
                self.emit(Op::Jump(top as u32), line);
                let done = self.here();
                self.patch(j_ok, done);
                self.patch(j_out, done);
                self.emit(Op::LoadLocal(r), line);
                self.pop_scope();
                Ok(t)
            }
            ExprKind::Spawn(inner) => {
                let t = self.spawn_lifted(LiftBody::Expr(inner), line)?;
                Ok(Ty::Task(Box::new(t)))
            }
            ExprKind::Await(inner) => {
                let t = self.expr(inner)?;
                let v = self.fresh();
                self.unify(&Ty::Task(Box::new(v.clone())), &t, line, "`await` operand")?;
                self.emit(Op::Await, line);
                Ok(v)
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, line: u32) -> CResult<Ty> {
        if matches!(op, BinOp::And | BinOp::Or) {
            let lt = self.expr(l)?;
            self.unify(&Ty::Bool, &lt, line, "logical operand")?;
            let j = if op == BinOp::And { self.emit(Op::JumpIfFalse(0), line) } else { self.emit(Op::JumpIfTrue(0), line) };
            let rt = self.expr(r)?;
            self.unify(&Ty::Bool, &rt, line, "logical operand")?;
            let jend = self.emit(Op::Jump(0), line);
            let short = self.here();
            self.patch(j, short);
            let k = self.konst(Const::Bool(op == BinOp::Or));
            self.emit(Op::Const(k), line);
            let end = self.here();
            self.patch(jend, end);
            return Ok(Ty::Bool);
        }
        // `a + b + c` over strings: push every operand, then one ConcatN.
        if op == BinOp::Add {
            let mut parts = Vec::new();
            flatten_add(l, &mut parts);
            flatten_add(r, &mut parts);
            if parts.len() > 2 {
                let first = self.expr(parts[0])?;
                if self.prune(&first) == Ty::Str {
                    for p in &parts[1..] {
                        let t = self.expr(p)?;
                        self.unify(&Ty::Str, &t, line, "string concatenation")?;
                    }
                    self.emit(Op::ConcatN(parts.len() as u16), line);
                    return Ok(Ty::Str);
                }
                // Not a string chain: fold left-to-right like the generic path.
                let mut acc = first;
                for p in &parts[1..] {
                    let t = self.expr(p)?;
                    self.unify(&acc, &t, line, "operands")?;
                    let pt = self.prune(&acc);
                    if !matches!(pt, Ty::Int | Ty::Dur) {
                        return self.err(line, format!("operator Add is not defined for `{pt}`"));
                    }
                    self.emit(Op::Add, line);
                    acc = pt;
                }
                return Ok(acc);
            }
        }
        let lt = self.expr(l)?;
        let rt = self.expr(r)?;
        self.unify(&lt, &rt, line, "operands")?;
        let t = self.prune(&lt);
        let (code, ok, res) = match op {
            BinOp::Add => (Op::Add, matches!(t, Ty::Int | Ty::Dur | Ty::Str), t.clone()),
            BinOp::Sub => (Op::Sub, matches!(t, Ty::Int | Ty::Dur), t.clone()),
            BinOp::Mul => (Op::Mul, t == Ty::Int, t.clone()),
            BinOp::Div => (Op::Div, t == Ty::Int, t.clone()),
            BinOp::Mod => (Op::Mod, t == Ty::Int, t.clone()),
            BinOp::Eq => (Op::Eq, !matches!(t, Ty::Task(_)), Ty::Bool),
            BinOp::Ne => (Op::Ne, !matches!(t, Ty::Task(_)), Ty::Bool),
            BinOp::Lt => (Op::Lt, matches!(t, Ty::Int | Ty::Dur | Ty::Str), Ty::Bool),
            BinOp::Le => (Op::Le, matches!(t, Ty::Int | Ty::Dur | Ty::Str), Ty::Bool),
            BinOp::Gt => (Op::Gt, matches!(t, Ty::Int | Ty::Dur | Ty::Str), Ty::Bool),
            BinOp::Ge => (Op::Ge, matches!(t, Ty::Int | Ty::Dur | Ty::Str), Ty::Bool),
            BinOp::And | BinOp::Or => unreachable!(),
        };
        if !ok {
            return self.err(line, format!("operator {:?} is not defined for `{t}`", op));
        }
        self.emit(code, line);
        Ok(res)
    }

    fn variant_of(&self, path: &[String], line: u32) -> CResult<(String, u16, Option<Ty>)> {
        let [en, vn] = path else {
            return self.err(line, format!("unknown name `{}`", path.join(".")));
        };
        let Some(vs) = self.enums.get(en) else {
            return self.err(line, format!("unknown enum `{en}`"));
        };
        match vs.iter().position(|(n, _)| n == vn) {
            Some(i) => Ok((en.clone(), i as u16, vs[i].1.clone())),
            None => self.err(line, format!("enum `{en}` has no variant `{vn}`")),
        }
    }

    fn call(&mut self, path: &[String], args: &[Expr], line: u32) -> CResult<Ty> {
        if path.len() == 2 {
            let (en, tag, payload) = self.variant_of(path, line)?;
            let Some(pt) = payload else {
                return self.err(line, format!("variant `{}` carries no value", path.join(".")));
            };
            if args.len() != 1 {
                return self.err(line, "a variant takes exactly one value");
            }
            let t = self.expr(&args[0])?;
            self.unify(&pt, &t, line, "variant payload")?;
            self.emit(Op::MakeVariant(tag, true), line);
            return Ok(Ty::Enum(en));
        }
        let name = path[0].as_str();
        match name {
            "Some" | "Ok" | "Err" => {
                if args.len() != 1 {
                    return self.err(line, format!("`{name}` takes one value"));
                }
                let t = self.expr(&args[0])?;
                let (tag, ty) = match name {
                    "Some" => (1, Ty::Option(Box::new(t))),
                    "Ok" => (0, Ty::Result(Box::new(t), Box::new(self.fresh()))),
                    _ => (1, Ty::Result(Box::new(self.fresh()), Box::new(t))),
                };
                self.emit(Op::MakeVariant(tag, true), line);
                return Ok(ty);
            }
            _ => {}
        }
        if let Some(sig) = self.funcs.get(name) {
            let (idx, params, ret, effects, kind) = (sig.idx, sig.params.clone(), sig.ret.clone(), sig.effects.clone(), sig.kind);
            if kind == FuncKind::Test {
                return self.err(line, "tests cannot be called");
            }
            self.check_effects(&effects, line, name)?;
            self.args_against(name, &params, args, line)?;
            self.emit(Op::Call(idx, args.len() as u16), line);
            return Ok(ret);
        }
        if let Some(sig) = self.externs.get(name) {
            let (idx, params, ret, effect) = (sig.idx, sig.params.clone(), sig.ret.clone(), sig.effect.clone());
            self.check_effects(&[effect], line, name)?;
            self.args_against(name, &params, args, line)?;
            self.emit(Op::Extern(idx, args.len() as u16), line);
            return Ok(ret);
        }
        self.builtin(name, args, line)
    }

    fn args_against(&mut self, name: &str, params: &[Ty], args: &[Expr], line: u32) -> CResult<()> {
        if params.len() != args.len() {
            return self.err(line, format!("`{name}` takes {} argument(s), got {}", params.len(), args.len()));
        }
        for (i, (p, a)) in params.iter().zip(args).enumerate() {
            let t = self.expr(a)?;
            self.unify(p, &t, a.line, &format!("argument {} of `{name}`", i + 1))?;
        }
        Ok(())
    }

    fn builtin(&mut self, name: &str, args: &[Expr], line: u32) -> CResult<Ty> {
        // Signatures with placeholders Var(0..) instantiated fresh per call.
        let v = |i| Box::new(Ty::Var(i));
        let (b, params, ret): (Builtin, Vec<Ty>, Ty) = match name {
            "push" => (Builtin::Push, vec![Ty::List(v(0)), Ty::Var(0)], Ty::List(v(0))),
            "get" => (Builtin::Get, vec![Ty::List(v(0)), Ty::Int], Ty::Option(v(0))),
            "set" => (Builtin::Set, vec![Ty::List(v(0)), Ty::Int, Ty::Var(0)], Ty::List(v(0))),
            "remove" => (Builtin::Remove, vec![Ty::List(v(0)), Ty::Int], Ty::List(v(0))),
            "range" => (Builtin::Range, vec![Ty::Int], Ty::List(Box::new(Ty::Int))),
            "map_new" => (Builtin::MapNew, vec![], Ty::Map(v(0))),
            "map_with_capacity" => (Builtin::MapWithCapacity, vec![Ty::Int], Ty::Map(v(0))),
            "map_get" => (Builtin::MapGet, vec![Ty::Map(v(0)), Ty::Str], Ty::Option(v(0))),
            "map_set" => (Builtin::MapSet, vec![Ty::Map(v(0)), Ty::Str, Ty::Var(0)], Ty::Map(v(0))),
            "map_has" => (Builtin::MapHas, vec![Ty::Map(v(0)), Ty::Str], Ty::Bool),
            "map_keys" => (Builtin::MapKeys, vec![Ty::Map(v(0))], Ty::List(Box::new(Ty::Str))),
            "str" => (Builtin::ToStr, vec![Ty::Var(0)], Ty::Str),
            "print" => (Builtin::Print, vec![Ty::Str], Ty::Unit),
            "join" => (Builtin::Join, vec![Ty::List(Box::new(Ty::Task(v(0))))], Ty::List(v(0))),
            "await_any" => (Builtin::AwaitAny, vec![Ty::List(Box::new(Ty::Task(v(0))))], Ty::Tuple(vec![Ty::Int, Ty::Var(0)])),
            "cancel" => (Builtin::Cancel, vec![Ty::Task(v(0))], Ty::Unit),
            "sleep" => (Builtin::Sleep, vec![Ty::Dur], Ty::Unit),
            "json_str" => (Builtin::JsonStr, vec![Ty::Str], Ty::Str),
            "json_get_str" => (Builtin::JsonGetStr, vec![Ty::Str, Ty::Str], Ty::Option(Box::new(Ty::Str))),
            "now_ms" => (Builtin::NowMs, vec![], Ty::Int),
            "len" => {
                if args.len() != 1 {
                    return self.err(line, "`len` takes one argument");
                }
                let t = self.expr(&args[0])?;
                return match self.prune(&t) {
                    Ty::List(_) | Ty::Map(_) | Ty::Str => {
                        self.emit(Op::Builtin(Builtin::Len, 1), line);
                        Ok(Ty::Int)
                    }
                    t => self.err(line, format!("`len` needs a List, Map or Str, not `{t}`")),
                };
            }
            _ => return self.err(line, format!("unknown function `{name}`")),
        };
        let fresh: Vec<Ty> = (0..3).map(|_| self.fresh()).collect();
        let inst = |t: &Ty| -> Ty { instantiate(t, &fresh) };
        let params: Vec<Ty> = params.iter().map(inst).collect();
        let ret = inst(&ret);
        self.args_against(name, &params, args, line)?;
        self.emit(Op::Builtin(b, args.len() as u16), line);
        Ok(ret)
    }

    fn matche(&mut self, scrut: &Expr, arms: &[(Pattern, Expr)], line: u32) -> CResult<Ty> {
        self.matche_mode(scrut, arms, line, false)
    }

    /// `as_stmt`: the match value is discarded, so arms leave nothing on the
    /// stack (no Unit/Pop pair per arm).
    fn matche_mode(&mut self, scrut: &Expr, arms: &[(Pattern, Expr)], line: u32, as_stmt: bool) -> CResult<Ty> {
        // A plain local variable is matched in place; anything else goes
        // through a temporary.
        let direct = match &scrut.kind {
            ExprKind::Var(n) => self.lookup(n),
            _ => None,
        };
        let (st, tmp) = match direct {
            Some((slot, ty, _)) => {
                self.types.insert(scrut as *const Expr as usize, ty.clone());
                self.push_scope();
                (ty, slot)
            }
            None => {
                let st = self.expr(scrut)?;
                self.push_scope();
                let tmp = self.temp_typed(st.clone());
                self.emit(Op::StoreLocal(tmp), line);
                (st, tmp)
            }
        };
        let st = self.prune(&st);
        let mut result = Ty::Never;
        let mut ends = Vec::new();
        let mut covered: HashSet<u16> = HashSet::new();
        let mut catch_all = false;
        let n_variants = match &st {
            Ty::Option(_) | Ty::Result(_, _) => Some(2usize),
            Ty::Enum(en) => Some(self.enums[en].len()),
            Ty::Bool => Some(2),
            _ => None,
        };
        for (arm_ix, (pat, body)) in arms.iter().enumerate() {
            let last_arm = arm_ix + 1 == arms.len();
            if catch_all {
                return self.err(body.line, "unreachable match arm after a catch-all pattern");
            }
            self.push_scope();
            let mut jf = None;
            match pat {
                Pattern::Wildcard => catch_all = true,
                Pattern::Bind(n) => {
                    catch_all = true;
                    self.emit(Op::LoadLocal(tmp), line);
                    let s = self.declare_local(n, st.clone(), false);
                    self.emit(Op::StoreLocal(s), line);
                }
                Pattern::Int(_) | Pattern::Str(_) | Pattern::Bool(_) => {
                    let (k, t, tag) = match pat {
                        Pattern::Int(n) => (Const::Int(*n), Ty::Int, None),
                        Pattern::Str(s) => (Const::Str(s.clone()), Ty::Str, None),
                        Pattern::Bool(b) => (Const::Bool(*b), Ty::Bool, Some(*b as u16)),
                        _ => unreachable!(),
                    };
                    self.unify(&t, &st, line, "match pattern")?;
                    if let Some(tag) = tag {
                        covered.insert(tag);
                    }
                    self.emit(Op::LoadLocal(tmp), line);
                    let ki = self.konst(k);
                    self.emit(Op::Const(ki), line);
                    self.emit(Op::Eq, line);
                    jf = Some(self.emit(Op::JumpIfFalse(0), line));
                }
                Pattern::Variant { path, bind } => {
                    let (tag, payload) = self.pattern_variant(&st, path, line)?;
                    // The arm that completes exhaustiveness needs no test: its
                    // variant is the only one left.
                    let only_left = last_arm && !covered.contains(&tag) && n_variants.is_some_and(|n| covered.len() + 1 == n);
                    covered.insert(tag);
                    if only_left {
                        catch_all = true;
                    } else {
                        self.emit(Op::LoadLocal(tmp), line);
                        self.emit(Op::TagIs(tag), line);
                        jf = Some(self.emit(Op::JumpIfFalse(0), line));
                    }
                    match (bind, payload) {
                        (Some(b), Some(pt)) => {
                            self.emit(Op::LoadLocal(tmp), line);
                            self.emit(Op::Payload, line);
                            let s = self.declare_local(b, pt, false);
                            self.emit(Op::StoreLocal(s), line);
                        }
                        (Some(_), None) => return self.err(line, format!("`{}` carries no value to bind", path.join("."))),
                        (None, Some(_)) => return self.err(line, format!("`{}` carries a value; bind it as `{}(x)` or `{}(_)`", path.join("."), path.join("."), path.join("."))),
                        (None, None) => {}
                    }
                }
            }
            let bt = if as_stmt {
                match &body.kind {
                    ExprKind::Block(b) => {
                        self.block_stmt(b)?;
                        Ty::Unit
                    }
                    _ => {
                        self.expr_stmt(body)?;
                        Ty::Unit
                    }
                }
            } else {
                self.expr(body)?
            };
            if !as_stmt {
                result = self.join(&result, &bt, body.line, "match arms")?;
            }
            self.pop_scope();
            ends.push(self.emit(Op::Jump(0), line));
            if let Some(j) = jf {
                let next = self.here();
                self.patch(j, next);
            }
        }
        let exhaustive = catch_all || n_variants.is_some_and(|n| covered.len() >= n);
        if !exhaustive {
            return self.err(line, format!("match on `{st}` is not exhaustive; add the missing cases or `_ =>`"));
        }
        if !catch_all {
            let k = self.konst(Const::Str("internal: no match arm taken".into()));
            self.emit(Op::Const(k), line);
            self.emit(Op::Trap, line);
        }
        let end = self.here();
        for j in ends {
            self.patch(j, end);
        }
        self.pop_scope();
        Ok(result)
    }

    fn pattern_variant(&mut self, st: &Ty, path: &[String], line: u32) -> CResult<(u16, Option<Ty>)> {
        let last = path.last().unwrap().as_str();
        match (st, path.len()) {
            (Ty::Option(t), 1) => match last {
                "None" => Ok((0, None)),
                "Some" => Ok((1, Some((**t).clone()))),
                _ => self.err(line, format!("`{last}` is not a variant of Option")),
            },
            (Ty::Result(a, b), 1) => match last {
                "Ok" => Ok((0, Some((**a).clone()))),
                "Err" => Ok((1, Some((**b).clone()))),
                _ => self.err(line, format!("`{last}` is not a variant of Result")),
            },
            (Ty::Enum(en), _) => {
                if path.len() == 2 && path[0] != *en {
                    return self.err(line, format!("pattern `{}` does not belong to `{en}`", path.join(".")));
                }
                let full = [en.clone(), last.to_string()];
                let (_, tag, p) = self.variant_of(&full, line)?;
                Ok((tag, p))
            }
            (Ty::Var(_), _) => self.err(line, "cannot infer the matched type; add a type annotation"),
            (t, _) => self.err(line, format!("pattern `{}` does not fit `{t}`", path.join("."))),
        }
    }
}

/// `LoadLocal a` whose value is never read again (a is dead after the op on
/// every path) becomes `TakeLocal a`: the value moves instead of being
/// cloned now and dropped when the slot is overwritten or the frame returns.
/// Plain slots (Unit/Int/Bool/Dur/Task by static type) are left alone: their
/// copy is free and the move would add a store. Same length, same targets.
/// Liveness is conservative: writes that happen on one path only
/// (ForNext, UnwrapOrJump, MapGetJump) kill nothing.
pub fn move_last_reads(code: &[Op], slot_tags: &[u16]) -> Vec<Op> {
    let last = last_reads(code, slot_tags);
    code.iter().zip(&last).map(|(op, &l)| match *op {
        Op::LoadLocal(a) if l => Op::TakeLocal(a),
        o => o,
    }).collect()
}

/// Per op: true for a `LoadLocal a` of a non-plain slot that is dead after
/// the op on every path (see `move_last_reads`).
fn last_reads(code: &[Op], slot_tags: &[u16]) -> Vec<bool> {
    let n = code.len();
    let nl = code.iter().flat_map(|o| op_locals(*o)).map(|l| l as usize + 1).max().unwrap_or(0);
    if nl == 0 {
        return vec![false; n];
    }
    let words = nl.div_ceil(64);
    // (uses, defs) per op.
    let ud = |op: Op| -> (Vec<u16>, Vec<u16>) {
        match op {
            Op::StoreLocal(a) | Op::StoreLoad(a) => (vec![], vec![a]),
            Op::TakeLocal(a) => (vec![a], vec![a]),
            Op::MovePayload(a, b) => (vec![a], vec![b]),
            Op::UnwrapOrJump(..) | Op::MapGetJump(..) => (vec![], vec![]),
            Op::ForNext(b, _) => (vec![b, b + 1, b + 2], vec![]),
            o => (op_locals(o), vec![]),
        }
    };
    let succ = |pc: usize| -> Vec<usize> {
        let mut v = Vec::with_capacity(2);
        match code[pc] {
            Op::Ret | Op::Trap => {}
            Op::Jump(t) => v.push(t as usize),
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfNotLt(t) | Op::ForNext(_, t) | Op::JumpIfNotTag(_, t) | Op::UnwrapOrJump(_, t) | Op::MapGetJump(_, t) => {
                v.push(t as usize);
                v.push(pc + 1);
            }
            _ => v.push(pc + 1),
        }
        v.retain(|&s| s < n);
        v
    };
    let set = |b: &mut [u64], l: u16| b[l as usize / 64] |= 1 << (l % 64);
    let clr = |b: &mut [u64], l: u16| b[l as usize / 64] &= !(1u64 << (l % 64));
    let mut live_in = vec![vec![0u64; words]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for pc in (0..n).rev() {
            let mut out = vec![0u64; words];
            for s in succ(pc) {
                for (o, i) in out.iter_mut().zip(&live_in[s]) {
                    *o |= *i;
                }
            }
            let (u, d) = ud(code[pc]);
            for l in d {
                clr(&mut out, l);
            }
            for l in u {
                set(&mut out, l);
            }
            if out != live_in[pc] {
                live_in[pc] = out;
                changed = true;
            }
        }
    }
    let plain = |l: u16| match slot_tags.get(l as usize) {
        Some(&t) => matches!(t & 0xFF, 0 | 1 | 2 | 4 | 10),
        None => true,
    };
    let mut out = vec![false; n];
    for pc in 0..n {
        let Op::LoadLocal(a) = code[pc] else { continue };
        if plain(a) {
            continue;
        }
        out[pc] = !succ(pc).iter().any(|&s| live_in[s][a as usize / 64] >> (a % 64) & 1 == 1);
    }
    out
}

/// Locals an op reads or writes.
fn op_locals(op: Op) -> Vec<u16> {
    match op {
        Op::LoadLocal(a) | Op::TakeLocal(a) | Op::StoreLocal(a) | Op::StoreLoad(a) | Op::IncLocal(a, _) | Op::LoadField(a, _)
        | Op::LoadPayload(a) | Op::LenLocal(a) | Op::UpdLocal(_, a) | Op::UnwrapOrJump(a, _) | Op::MapGetJump(a, _) => vec![a],
        Op::LoadLocal2(a, b) | Op::IndexLL(a, b) | Op::MovePayload(a, b) => vec![a, b],
        Op::ForNext(b, _) => vec![b, b + 1, b + 2],
        _ => vec![],
    }
}

fn flatten_add<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match &e.kind {
        ExprKind::Binary(BinOp::Add, l, r) => {
            flatten_add(l, out);
            flatten_add(r, out);
        }
        _ => out.push(e),
    }
}

fn instantiate(t: &Ty, fresh: &[Ty]) -> Ty {
    match t {
        Ty::Var(i) => fresh[*i as usize].clone(),
        Ty::List(a) => Ty::List(Box::new(instantiate(a, fresh))),
        Ty::Map(a) => Ty::Map(Box::new(instantiate(a, fresh))),
        Ty::Option(a) => Ty::Option(Box::new(instantiate(a, fresh))),
        Ty::Task(a) => Ty::Task(Box::new(instantiate(a, fresh))),
        Ty::Result(a, b) => Ty::Result(Box::new(instantiate(a, fresh)), Box::new(instantiate(b, fresh))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| instantiate(t, fresh)).collect()),
        t => t.clone(),
    }
}

/// `TakeLocal a; <argument loads>; Builtin(push|set|remove|map_set); StoreLocal a`
/// becomes `<argument loads>; UpdLocal(b, a)`: the container in local `a` is
/// updated in place. The argument loads must not touch `a` and must push
/// exactly the remaining arguments; no jump may land inside the sequence.
fn update_in_place(code: &[Op], lines: &[u32]) -> (Vec<Op>, Vec<u32>) {
    let mut target = vec![false; code.len() + 1];
    for op in code {
        match *op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::ForNext(_, t) => target[t as usize] = true,
            _ => {}
        }
    }
    let mut keep = vec![true; code.len()];
    let mut repl: Vec<Option<Op>> = vec![None; code.len()];
    let mut i = 0;
    while i < code.len() {
        if let Op::TakeLocal(a) = code[i] {
            let mut j = i + 1;
            let mut pushed = 0u16;
            let mut ok = true;
            while j < code.len() && !target[j] {
                match code[j] {
                    Op::LoadLocal(x) if x != a => pushed += 1,
                    Op::Const(_) | Op::Unit => pushed += 1,
                    _ => break,
                }
                j += 1;
            }
            if j + 1 < code.len() && !target[j] && !target[j + 1] {
                if let (Op::Builtin(b, argc), Op::StoreLocal(dst)) = (code[j], code[j + 1]) {
                    let fits = matches!(b, Builtin::Push | Builtin::Set | Builtin::Remove | Builtin::MapSet);
                    if fits && dst == a && argc == pushed + 1 && ok {
                        keep[i] = false;
                        repl[j] = Some(Op::UpdLocal(b, a));
                        keep[j + 1] = false;
                        i = j + 2;
                        continue;
                    }
                }
            }
            ok = false;
            let _ = ok;
        }
        i += 1;
    }
    let mut map = vec![0u32; code.len() + 1];
    let mut out = vec![];
    let mut out_lines = vec![];
    for k in 0..code.len() {
        map[k] = out.len() as u32;
        if keep[k] {
            out.push(repl[k].unwrap_or(code[k]));
            out_lines.push(lines[k]);
        }
    }
    map[code.len()] = out.len() as u32;
    for op in &mut out {
        *op = match *op {
            Op::Jump(t) => Op::Jump(map[t as usize]),
            Op::JumpIfFalse(t) => Op::JumpIfFalse(map[t as usize]),
            Op::JumpIfTrue(t) => Op::JumpIfTrue(map[t as usize]),
            Op::ForNext(b, t) => Op::ForNext(b, map[t as usize]),
            o => o,
        };
    }
    (out, out_lines)
}

/// Peephole pass: fuse common op sequences into superinstructions. A sequence
/// is only fused when none of its inner ops is a jump target.
pub fn fuse(code: &[Op], lines: &[u32], consts: &[Const], slot_tags: &[u16]) -> (Vec<Op>, Vec<u32>) {
    let (code, lines) = update_in_place(code, lines);
    let (code, lines) = (code.as_slice(), lines.as_slice());
    // A last read stays a separate LoadLocal so `move_last_reads` can make
    // it a move (LoadLocal2 has no move form).
    let last = last_reads(code, slot_tags);
    let mut target = vec![false; code.len() + 1];
    for op in code {
        match *op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::ForNext(_, t) | Op::JumpIfNotTag(_, t) => target[t as usize] = true,
            _ => {}
        }
    }
    let free = |i: usize, n: usize| i + n <= code.len() && (1..n).all(|k| !target[i + k]);
    let mut out = Vec::with_capacity(code.len());
    let mut out_lines = Vec::with_capacity(code.len());
    let mut map = vec![0u32; code.len() + 1];
    let mut i = 0;
    while i < code.len() {
        let (op, n) = match code[i..] {
            [Op::LoadLocal(a), Op::Const(c), Op::Add, Op::StoreLocal(b), ..] if a == b && free(i, 4) => match consts[c as usize] {
                Const::Int(k) if i32::try_from(k).is_ok() => (Op::IncLocal(a, k as i32), 4),
                _ => (code[i], 1),
            },
            // Keep `LoadLocal a; FieldGet/Payload/Len` fusable (the fused op reads
            // the local in place: no clone of the aggregate on the stack).
            [Op::StoreLocal(a), Op::LoadLocal(b), Op::FieldGet(_) | Op::Payload | Op::Builtin(Builtin::Len, 1), ..] if a == b && free(i, 3) => (code[i], 1),
            [Op::StoreLocal(a), Op::LoadLocal(b), ..] if a == b && free(i, 2) => (Op::StoreLoad(a), 2),
            [Op::Lt, Op::JumpIfFalse(t), ..] if free(i, 2) => (Op::JumpIfNotLt(t), 2),
            [Op::TagIs(t), Op::JumpIfFalse(j), ..] if free(i, 2) => (Op::JumpIfNotTag(t, j), 2),
            [Op::LoadLocal(a), Op::LoadLocal(b), Op::Index, ..] if free(i, 3) => (Op::IndexLL(a, b), 3),
            [Op::LoadLocal(a), Op::FieldGet(f), ..] if free(i, 2) => (Op::LoadField(a, f), 2),
            [Op::LoadLocal(a), Op::Payload, Op::StoreLocal(b), ..] if free(i, 3) => (Op::MovePayload(a, b), 3),
            [Op::LoadLocal(a), Op::Payload, ..] if free(i, 2) => (Op::LoadPayload(a), 2),
            [Op::LoadLocal(a), Op::Builtin(Builtin::Len, 1), ..] if free(i, 2) => (Op::LenLocal(a), 2),
            [Op::LoadLocal(_), Op::LoadLocal(_), Op::Builtin(Builtin::Len, 1), ..] => (code[i], 1),
            [Op::LoadLocal(a), Op::LoadLocal(b), ..] if free(i, 2) && !last[i] && !last[i + 1] => (Op::LoadLocal2(a, b), 2),
            _ => (code[i], 1),
        };
        for k in 0..n {
            map[i + k] = out.len() as u32;
        }
        out.push(op);
        out_lines.push(lines[i]);
        i += n;
    }
    map[code.len()] = out.len() as u32;
    for op in &mut out {
        *op = match *op {
            Op::Jump(t) => Op::Jump(map[t as usize]),
            Op::JumpIfFalse(t) => Op::JumpIfFalse(map[t as usize]),
            Op::JumpIfTrue(t) => Op::JumpIfTrue(map[t as usize]),
            Op::JumpIfNotLt(t) => Op::JumpIfNotLt(map[t as usize]),
            Op::ForNext(b, t) => Op::ForNext(b, map[t as usize]),
            Op::JumpIfNotTag(g, t) => Op::JumpIfNotTag(g, map[t as usize]),
            o => o,
        };
    }
    // Match on a temporary Option: StoreLoad(t); JumpIfNotTag(1, j); MovePayload(t, r)
    // where `t` appears nowhere else → UnwrapOrJump(r, j); with a preceding
    // map_get → MapGetJump(r, j). Rebuilt with a fresh index map.
    let uses = |slot: u16, code: &[Op]| code.iter().filter(|o| op_locals(**o).contains(&slot)).count();
    let mut i = 0;
    let mut out2: Vec<Op> = Vec::with_capacity(out.len());
    let mut lines2: Vec<u32> = Vec::with_capacity(out.len());
    let mut tmap = vec![0u32; out.len() + 1];
    let mut target2 = vec![false; out.len() + 1];
    for op in &out {
        match *op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfNotLt(t) | Op::ForNext(_, t) | Op::JumpIfNotTag(_, t) => target2[t as usize] = true,
            _ => {}
        }
    }
    while i < out.len() {
        let pat = match out[i..] {
            [Op::Builtin(Builtin::MapGet, 2), Op::StoreLoad(t), Op::JumpIfNotTag(1, j), Op::MovePayload(t2, r), ..]
                if t == t2 && !target2[i + 1] && !target2[i + 2] && !target2[i + 3] && uses(t, &out) == 2 => Some((Op::MapGetJump(r, j), 4)),
            [Op::StoreLoad(t), Op::JumpIfNotTag(1, j), Op::MovePayload(t2, r), ..]
                if t == t2 && !target2[i + 1] && !target2[i + 2] && uses(t, &out) == 2 => Some((Op::UnwrapOrJump(r, j), 3)),
            _ => None,
        };
        let (op, n) = pat.unwrap_or((out[i], 1));
        for k in 0..n {
            tmap[i + k] = out2.len() as u32;
        }
        out2.push(op);
        lines2.push(out_lines[i]);
        i += n;
    }
    tmap[out.len()] = out2.len() as u32;
    for op in &mut out2 {
        *op = match *op {
            Op::Jump(t) => Op::Jump(tmap[t as usize]),
            Op::JumpIfFalse(t) => Op::JumpIfFalse(tmap[t as usize]),
            Op::JumpIfTrue(t) => Op::JumpIfTrue(tmap[t as usize]),
            Op::JumpIfNotLt(t) => Op::JumpIfNotLt(tmap[t as usize]),
            Op::ForNext(b, t) => Op::ForNext(b, tmap[t as usize]),
            Op::JumpIfNotTag(g, t) => Op::JumpIfNotTag(g, tmap[t as usize]),
            Op::UnwrapOrJump(r, t) => Op::UnwrapOrJump(r, tmap[t as usize]),
            Op::MapGetJump(r, t) => Op::MapGetJump(r, tmap[t as usize]),
            o => o,
        };
    }
    let mut out = out2;
    let out_lines = lines2;
    // Jump threading: a jump to an unconditional jump goes straight to its end.
    let resolve = |mut t: u32, code: &[Op]| {
        for _ in 0..16 {
            match code.get(t as usize) {
                Some(Op::Jump(u)) if *u != t => t = *u,
                _ => break,
            }
        }
        t
    };
    let snapshot = out.clone();
    for op in &mut out {
        *op = match *op {
            Op::Jump(t) => Op::Jump(resolve(t, &snapshot)),
            Op::JumpIfFalse(t) => Op::JumpIfFalse(resolve(t, &snapshot)),
            Op::JumpIfTrue(t) => Op::JumpIfTrue(resolve(t, &snapshot)),
            Op::JumpIfNotLt(t) => Op::JumpIfNotLt(resolve(t, &snapshot)),
            Op::JumpIfNotTag(g, t) => Op::JumpIfNotTag(g, resolve(t, &snapshot)),
            Op::UnwrapOrJump(r, t) => Op::UnwrapOrJump(r, resolve(t, &snapshot)),
            Op::MapGetJump(r, t) => Op::MapGetJump(r, resolve(t, &snapshot)),
            o => o,
        };
    }
    (out, out_lines)
}

pub(crate) fn readonly_call<'e>(e: &'e Expr, readonly: &dyn Fn(&str) -> bool) -> Option<&'e Expr> {
    let call = match &e.kind {
        ExprKind::Try(inner) => inner.as_ref(),
        _ => e,
    };
    match &call.kind {
        ExprKind::Call { path, .. } if path.len() == 1 && readonly(&path[0]) => Some(call),
        _ => None,
    }
}

/// Automatic parallelization of independent read-only calls.
///
/// 1. Consecutive `let x = ro_call(..)[?]` whose arguments do not use any
///    name bound earlier in the run become `spawn`s, awaited in the
///    original order (`let x = (await t)[?]`).
/// 2. `for v in xs { acc = push(acc, ro_call(..v..)[?]) }` spawns every
///    call first and collects the results in list order.
///
/// Results, their order and `?` semantics are unchanged; an early failure
/// cancels the speculative siblings (structured concurrency). Only calls
/// declared `readonly` qualify, so no observable effect is reordered.
pub(crate) fn autopar<'s>(stmts: &'s [Stmt], readonly: &dyn Fn(&str) -> bool) -> Vec<std::borrow::Cow<'s, Stmt>> {
    use std::borrow::Cow;
    let mut out: Vec<Cow<'s, Stmt>> = Vec::with_capacity(stmts.len());
    let mut i = 0;
    let mut counter = 0usize;
    let mut fresh = |p: &str| {
        counter += 1;
        format!("${p}{}_{}", counter, stmts.len())
    };
    while i < stmts.len() {
        // Pattern 2: loop over a list collecting read-only results.
        if let Stmt::For { var, iter, body, line } = &stmts[i] {
            if body.tail.is_none() && body.stmts.len() == 1 {
                if let Stmt::Assign { name: acc, value, .. } = &body.stmts[0] {
                    if let ExprKind::Call { path, args } = &value.kind {
                        if path.len() == 1 && path[0] == "push" && args.len() == 2 && matches!(&args[0].kind, ExprKind::Var(a) if a == acc) {
                            if let Some(call) = readonly_call(&args[1], readonly) {
                                let mut used = HashSet::new();
                                names_in_expr(call, &mut used);
                                if !used.contains(acc) {
                                    let l = *line;
                                    let ts = fresh("tasks");
                                    let t = fresh("t");
                                    let ex = |k: ExprKind| Expr { kind: k, line: l };
                                    out.push(Cow::Owned(Stmt::Let { names: vec![ts.clone()], tuple: false, mutable: true, ty: None, value: ex(ExprKind::List(vec![])), line: l }));
                                    out.push(Cow::Owned(Stmt::For {
                                        var: var.clone(),
                                        iter: iter.clone(),
                                        body: Block { stmts: vec![Stmt::Assign { name: ts.clone(), value: ex(ExprKind::Call { path: vec!["push".into()], args: vec![ex(ExprKind::Var(ts.clone())), ex(ExprKind::Spawn(Box::new(call.clone())))] }), line: l }], tail: None, line: l },
                                        line: l,
                                    }));
                                    let mut got = ex(ExprKind::Await(Box::new(ex(ExprKind::Var(t.clone())))));
                                    if matches!(args[1].kind, ExprKind::Try(_)) {
                                        got = ex(ExprKind::Try(Box::new(got)));
                                    }
                                    out.push(Cow::Owned(Stmt::For {
                                        var: t.clone(),
                                        iter: ex(ExprKind::Var(ts.clone())),
                                        body: Block { stmts: vec![Stmt::Assign { name: acc.clone(), value: ex(ExprKind::Call { path: vec!["push".into()], args: vec![ex(ExprKind::Var(acc.clone())), got] }), line: l }], tail: None, line: l },
                                        line: l,
                                    }));
                                    i += 1;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }
        // Pattern 1: a run of independent read-only lets.
        let mut run: Vec<&Stmt> = vec![];
        let mut bound: HashSet<String> = HashSet::new();
        let mut j = i;
        while j < stmts.len() {
            let Stmt::Let { names, tuple: false, value, .. } = &stmts[j] else { break };
            let Some(call) = readonly_call(value, readonly) else { break };
            let mut used = HashSet::new();
            names_in_expr(call, &mut used);
            if used.iter().any(|n| bound.contains(n)) || bound.contains(&names[0]) {
                break;
            }
            bound.insert(names[0].clone());
            run.push(&stmts[j]);
            j += 1;
        }
        if run.len() < 2 {
            out.push(Cow::Borrowed(&stmts[i]));
            i += 1;
            continue;
        }
        let mut awaits = vec![];
        for st in &run {
            let Stmt::Let { names, mutable, ty, value, line, .. } = st else { unreachable!() };
            let l = *line;
            let call = readonly_call(value, readonly).unwrap().clone();
            let t = fresh("p");
            out.push(Cow::Owned(Stmt::Let { names: vec![t.clone()], tuple: false, mutable: false, ty: None, value: Expr { kind: ExprKind::Spawn(Box::new(call)), line: l }, line: l }));
            let mut got = Expr { kind: ExprKind::Await(Box::new(Expr { kind: ExprKind::Var(t), line: l })), line: l };
            if matches!(value.kind, ExprKind::Try(_)) {
                got = Expr { kind: ExprKind::Try(Box::new(got)), line: l };
            }
            awaits.push(Stmt::Let { names: names.clone(), tuple: false, mutable: *mutable, ty: ty.clone(), value: got, line: l });
        }
        out.extend(awaits.into_iter().map(Cow::Owned));
        i = j;
    }
    out
}

