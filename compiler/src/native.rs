//! Native backend: type-checked ZAL → Rust source on top of `zal-rt`, compiled
//! ahead of time. The VM stays the reference semantics; this backend keeps the
//! same observable behaviour (values, traps, structured concurrency, per-call
//! capability checks, budgets) with static types and no interpreter.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::ast::*;
use crate::check::{names_in_block, names_in_expr};
use zal_ir::{Program, Ty};

pub struct Native {
    pub source: String,
}

struct Gen<'a> {
    prog: &'a Program,
    types: &'a HashMap<usize, Ty>,
    funcs: HashMap<String, FuncKind>,
    externs: HashMap<String, usize>,
    enums: HashSet<String>,
    scopes: Vec<HashSet<String>>,
    in_async: bool,
    uniq: usize,
    record_of: HashMap<String, Vec<String>>,
    /// >0 while generating compiler-made statements: their addresses are not
    /// keys of the type table (a lookup could hit a stale entry).
    untyped: usize,
    /// Interned string literals: text -> index of a thread-local `Str`.
    lits: HashMap<String, usize>,
    /// True where the function-level literal table `__lits` is in scope
    /// (not inside spawned tasks, which must be 'static).
    lits_local: bool,
    /// Loop variables bound by reference (`for v in xs.iter()`): reading
    /// them must not touch the element's refcount (a write to a cold line).
    refs: HashSet<String>,
    /// Variable reads that are last uses (see lastuse.rs): moved, not cloned.
    moves: HashSet<usize>,
}

type R<T> = Result<T, String>;

/// True if `name` is rebound or assigned anywhere inside `b` (then a loop
/// variable of that name must be an owned copy).
fn rebinds(b: &Block, name: &str) -> bool {
    fn pat(p: &Pattern, n: &str) -> bool {
        match p {
            Pattern::Bind(x) => x == n,
            Pattern::Variant { bind: Some(x), .. } => x == n,
            _ => false,
        }
    }
    fn expr(e: &Expr, n: &str) -> bool {
        match &e.kind {
            ExprKind::Call { args, .. } | ExprKind::List(args) | ExprKind::Tuple(args) | ExprKind::Parallel(args) | ExprKind::Race(args) => {
                args.iter().any(|a| expr(a, n))
            }
            ExprKind::Field(a, _) | ExprKind::TupleIdx(a, _) | ExprKind::Neg(a) | ExprKind::Not(a) | ExprKind::Try(a) | ExprKind::Spawn(a) | ExprKind::Await(a) => expr(a, n),
            ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => expr(a, n) || expr(b, n),
            ExprKind::If { cond, then, els } => expr(cond, n) || block(then, n) || els.as_ref().is_some_and(|e| expr(e, n)),
            ExprKind::Match { scrut, arms } => expr(scrut, n) || arms.iter().any(|(p, e)| pat(p, n) || expr(e, n)),
            ExprKind::Block(b) | ExprKind::Timeout { body: b, .. } => block(b, n),
            ExprKind::Retry { attempts, backoff, body } => expr(attempts, n) || backoff.as_ref().is_some_and(|e| expr(e, n)) || block(body, n),
            ExprKind::Record { fields, .. } => fields.iter().any(|(_, e)| expr(e, n)),
            _ => false,
        }
    }
    fn block(b: &Block, n: &str) -> bool {
        b.stmts.iter().any(|s| match s {
            Stmt::Let { names, value, .. } => names.iter().any(|x| x == n) || expr(value, n),
            Stmt::Assign { name, value, .. } => name == n || expr(value, n),
            Stmt::Expr(e) => expr(e, n),
            Stmt::Return(e, _) => e.as_ref().is_some_and(|e| expr(e, n)),
            Stmt::While { cond, body, .. } => expr(cond, n) || block(body, n),
            Stmt::For { var, iter, body, .. } => var == n || expr(iter, n) || block(body, n),
            Stmt::Ensure { cond, err, .. } => expr(cond, n) || expr(err, n),
            Stmt::Assert { cond, msg, .. } => expr(cond, n) || expr(msg, n),
            Stmt::Break(_) | Stmt::Continue(_) => false,
        }) || b.tail.as_ref().is_some_and(|e| expr(e, n))
    }
    block(b, name)
}

fn ident(n: &str) -> String {
    format!("v_{}", n.replace('$', "t_"))
}

pub fn rty(t: &Ty) -> String {
    match t {
        Ty::Int => "i64".into(),
        Ty::Bool => "bool".into(),
        Ty::Str => "Str".into(),
        Ty::Dur => "Dur".into(),
        Ty::Unit | Ty::Never | Ty::Var(_) => "()".into(),
        Ty::List(a) => format!("List<{}>", rty(a)),
        Ty::Map(a) => format!("Map<{}>", rty(a)),
        Ty::Option(a) => format!("Option<{}>", rty(a)),
        Ty::Result(a, b) => format!("Result<{}, {}>", rty(a), rty(b)),
        Ty::Tuple(ts) => format!("({})", ts.iter().map(|t| rty(t) + ",").collect::<String>()),
        Ty::Task(a) => format!("Task<{}>", rty(a)),
        Ty::Record(n) => format!("R_{n}"),
        Ty::Enum(n) => format!("E_{n}"),
    }
}


impl<'a> Gen<'a> {
    fn ty_of(&self, e: &Expr) -> Option<&Ty> {
        if self.untyped > 0 {
            return None;
        }
        self.types.get(&(e as *const Expr as usize))
    }

    fn fresh(&mut self, p: &str) -> String {
        self.uniq += 1;
        format!("__{p}{}", self.uniq)
    }

    fn bind(&mut self, n: &str) {
        self.scopes.last_mut().unwrap().insert(n.to_string());
    }

    fn is_local(&self, n: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(n))
    }

    fn block(&mut self, b: &Block) -> R<String> {
        self.scopes.push(HashSet::new());
        let mut s = String::from("{\n");
        // Same automatic parallelization of read-only calls as the VM backend.
        let ro: HashSet<String> = self.prog.externs.iter().filter(|x| x.readonly).map(|x| x.name.clone()).collect();
        let stmts = crate::check::autopar(&b.stmts, &|n: &str| ro.contains(n));
        for st in stmts.iter() {
            let made = matches!(st, std::borrow::Cow::Owned(_));
            if made {
                self.untyped += 1;
            }
            let r = self.stmt(st);
            if made {
                self.untyped -= 1;
            }
            s += &r?;
            s.push('\n');
        }
        let diverges = matches!(b.stmts.last(), Some(Stmt::Return(..) | Stmt::Break(_) | Stmt::Continue(_)));
        match &b.tail {
            Some(e) => s += &self.expr(e)?,
            None if diverges => {}
            None => s += "()",
        }
        s += "\n}";
        self.scopes.pop();
        Ok(s)
    }

    fn stmt(&mut self, st: &Stmt) -> R<String> {
        Ok(match st {
            Stmt::Let { names, tuple, mutable, value, .. } => {
                let v = self.expr(value)?;
                let m = if *mutable { "mut " } else { "" };
                let ann = match self.ty_of(value) {
                    Some(t) if !*tuple => format!(": {}", rty(t)),
                    _ => String::new(),
                };
                let out = if *tuple {
                    let pats: Vec<String> = names.iter().map(|n| format!("{m}{}", ident(n))).collect();
                    format!("let ({},) = {v};", pats.join(", "))
                } else {
                    format!("let {m}{}{ann} = {v};", ident(&names[0]))
                };
                for n in names {
                    self.bind(n);
                }
                out
            }
            Stmt::Assign { name, value, .. } => {
                // `x = push(x, ..)` and friends: move x out instead of cloning,
                // so the container stays uniquely owned (in-place update).
                if let ExprKind::Call { path, args } = &value.kind {
                    // `x = set(x, i, push(x[i], v))`: append to the inner list in place.
                    if path.len() == 1 && path[0] == "set" && args.len() == 3 && matches!(&args[0].kind, ExprKind::Var(a) if a == name) {
                        if let (ExprKind::Var(i1), ExprKind::Call { path: p2, args: a2 }) = (&args[1].kind, &args[2].kind) {
                            if p2.len() == 1 && p2[0] == "push" && a2.len() == 2 {
                                if let ExprKind::Index(base, idx) = &a2[0].kind {
                                    let same_base = matches!(&base.kind, ExprKind::Var(b) if b == name);
                                    let same_idx = matches!(&idx.kind, ExprKind::Var(i2) if i2 == i1);
                                    let mut used = HashSet::new();
                                    names_in_expr(&a2[1], &mut used);
                                    if same_base && same_idx && !used.contains(name) {
                                        let v = self.expr(&a2[1])?;
                                        return Ok(format!("{{ let __v = {v}; zal_rt::push_at(&mut {}, {}.clone(), __v)?; }}", ident(name), ident(i1)));
                                    }
                                }
                            }
                        }
                    }
                    let f = path[0].as_str();
                    if path.len() == 1 && ["push", "set", "remove", "map_set"].contains(&f) && matches!(&args[0].kind, ExprKind::Var(a) if a == name) {
                        let mut pre = String::new();
                        let mut rest = vec![];
                        for a in &args[1..] {
                            let t = self.fresh("a");
                            write!(pre, "let {t} = {}; ", self.expr(a)?).unwrap();
                            rest.push(t);
                        }
                        // Mutate the variable in place: no placeholder value.
                        let x = ident(name);
                        let call = match f {
                            "push" => format!("zal_rt::push_mut(&mut {x}, {})", rest[0]),
                            "set" => format!("zal_rt::set_mut(&mut {x}, {}, {})?", rest[0], rest[1]),
                            "remove" => format!("zal_rt::remove_mut(&mut {x}, {})?", rest[0]),
                            _ => format!("zal_rt::map_set_mut(&mut {x}, {}, {})", rest[0], rest[1]),
                        };
                        return Ok(format!("{{ {pre}{call}; }}"));
                    }
                }
                format!("{} = {};", ident(name), self.expr(value)?)
            }
            Stmt::Expr(e) => format!("{};", self.expr(e)?),
            Stmt::Return(e, _) => match e {
                Some(e) => format!("return Ok({});", self.expr(e)?),
                None => "return Ok(());".into(),
            },
            Stmt::While { cond, body, .. } => format!("while {} {{ zal_rt::tick()?; {} }}", self.expr(cond)?, self.block(body)?),
            Stmt::For { var, iter, body, .. } => {
                let it = self.fresh("it");
                let i = match &iter.kind {
                    ExprKind::Field(..) | ExprKind::Index(..) => format!("{}.clone()", self.borrow(iter)?),
                    _ => self.expr(iter)?,
                };
                self.scopes.push(HashSet::new());
                self.bind(var);
                // Read-only loop variables borrow the element.
                let by_ref = !rebinds(body, var);
                let shadowed = if by_ref { !self.refs.insert(var.clone()) } else { self.refs.remove(var) };
                let b = self.block(body);
                if by_ref && !shadowed {
                    self.refs.remove(var);
                } else if !by_ref && shadowed {
                    self.refs.insert(var.clone());
                }
                let b = b?;
                self.scopes.pop();
                let iter = if by_ref { "iter()" } else { "iter().cloned()" };
                format!("{{ let {it} = {i}; for {} in {it}.{iter} {{ zal_rt::tick()?; {b} }} }}", ident(var))
            }
            Stmt::Ensure { cond, err, .. } => format!("if !({}) {{ return Ok(Err({})); }}", self.expr(cond)?, self.expr(err)?),
            Stmt::Assert { cond, msg, .. } => format!("if !({}) {{ return Err(zal_rt::Trap(({}).to_string())); }}", self.expr(cond)?, self.expr(msg)?),
            Stmt::Break(_) => "break;".into(),
            Stmt::Continue(_) => "continue;".into(),
        })
    }

    fn need_async(&self, what: &str) -> R<()> {
        if self.in_async {
            Ok(())
        } else {
            Err(format!("native backend: `{what}` inside a pure `fn` is not supported"))
        }
    }

    /// `spawn` of an expression or block: captured locals are cloned into the task.
    fn spawn_of(&mut self, names: HashSet<String>, body: String, ty: Option<&Ty>) -> String {
        // `body` was generated with lits_local=false by the callers below.
        let mut caps: Vec<String> = names.into_iter().filter(|n| self.is_local(n)).collect();
        caps.sort();
        let clones: String = caps.iter().map(|n| format!("let {0} = {0}.clone(); ", ident(n))).collect();
        let t = ty.map(rty).unwrap_or_else(|| "_".into());
        format!("{{ {clones}zal_rt::spawn_future::<{t}, _>(async move {{ Ok::<{t}, zal_rt::Trap>({body}) }}) }}")
    }

    fn expr(&mut self, e: &Expr) -> R<String> {
        let ty = self.ty_of(e).cloned();
        Ok(match &e.kind {
            ExprKind::Int(n) => format!("{n}i64"),
            ExprKind::Str(s) => {
                // Literals are built once per thread and shared (refcount only).
                let n = self.lits.len();
                let k = *self.lits.entry(s.clone()).or_insert(n);
                if self.lits_local {
                    format!("__lits.l{k}.clone()")
                } else {
                    format!("LITS.with(|l| l.l{k}.clone())")
                }
            }
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Dur(ms) => format!("zal_rt::Dur({ms})"),
            ExprKind::Unit => "()".into(),
            ExprKind::Var(n) => {
                if n == "None" && !self.is_local(n) {
                    "None".into()
                } else if self.is_local(n) && !self.refs.contains(n) && self.moves.contains(&(e as *const Expr as usize)) {
                    ident(n)
                } else {
                    format!("{}.clone()", ident(n))
                }
            }
            ExprKind::Path(p) => format!("E_{}::{}", p[0], p[1]),
            ExprKind::Call { path, args } => self.call(path, args, ty.as_ref())?,
            ExprKind::Field(o, f) => format!("{}.f_{f}.clone()", self.place(o)?),
            ExprKind::TupleIdx(o, i) => format!("{}.{i}.clone()", self.place(o)?),
            ExprKind::Index(l, i) => format!("zal_rt::index({}, {})?", self.borrow(l)?, self.expr(i)?),
            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, ty.as_ref())?,
            ExprKind::Neg(a) => match self.ty_of(a) {
                Some(Ty::Dur) => format!("zal_rt::Dur(-({}).0)", self.expr(a)?),
                _ => format!("({}).checked_neg().unwrap_or(i64::MIN)", self.expr(a)?),
            },
            ExprKind::Not(a) => format!("(!{})", self.expr(a)?),
            ExprKind::If { cond, then, els } => {
                let c = self.expr(cond)?;
                let t = self.block(then)?;
                match els {
                    Some(x) => format!("(if {c} {t} else {{ {} }})", self.expr(x)?),
                    None => format!("{{ if {c} {{ {t}; }} }}"),
                }
            }
            ExprKind::Match { scrut, arms } => self.matche(scrut, arms)?,
            ExprKind::Block(b) => self.block(b)?,
            ExprKind::List(es) => {
                if es.is_empty() {
                    let el = match &ty {
                        Some(Ty::List(t)) => rty(t),
                        _ => "_".into(),
                    };
                    format!("zal_rt::list(Vec::<{el}>::new())")
                } else {
                    let xs: Vec<String> = es.iter().map(|x| self.expr(x)).collect::<R<_>>()?;
                    format!("zal_rt::list(vec![{}])", xs.join(", "))
                }
            }
            ExprKind::Tuple(es) => {
                let xs: Vec<String> = es.iter().map(|x| self.expr(x)).collect::<R<_>>()?;
                format!("({},)", xs.join(", "))
            }
            ExprKind::Record { name, fields } => {
                let order = self.record_of[name].clone();
                let mut parts = vec![];
                for f in &order {
                    let v = &fields.iter().find(|(n, _)| n == f).unwrap().1;
                    parts.push(format!("f_{f}: {}", self.expr(v)?));
                }
                format!("R_{name}(std::rc::Rc::new(D_{name} {{ {} }}))", parts.join(", "))
            }
            ExprKind::Try(inner) => {
                let v = self.expr(inner)?;
                match self.ty_of(inner) {
                    Some(Ty::Option(_)) => format!("(match {v} {{ Some(x) => x, None => return Ok(None) }})"),
                    Some(Ty::Result(..)) => format!("(match {v} {{ Ok(x) => x, Err(e) => return Ok(Err(e)) }})"),
                    // Compiler-generated nodes (autopar) carry no recorded type.
                    _ => format!("(match zal_rt::ZTry::branch({v}) {{ zal_rt::Flow::Continue(x) => x, zal_rt::Flow::Break(r) => return Ok(zal_rt::FromRes::from_res(r)) }})"),
                }
            }
            ExprKind::Parallel(es) if es.len() > 8 => {
                // Wide fan-out: one task per branch. Inline-joining dozens of
                // futures makes one huge state machine that rustc cannot build
                // in reasonable time or memory.
                self.need_async("parallel")?;
                let mut s = String::from("{ ");
                let mut ws = vec![];
                for x in es {
                    let t = self.fresh("t");
                    // A task call with call-free arguments: evaluate the arguments
                    // here and spawn the function's own future (one future type
                    // per function instead of one closure type per branch).
                    if let ExprKind::Call { path, args } = &x.kind {
                        let is_task = path.len() == 1 && matches!(self.funcs.get(&path[0]), Some(FuncKind::Task | FuncKind::Workflow));
                        let mut called = HashSet::new();
                        args.iter().for_each(|a| calls_in_expr(a, &mut called));
                        let pure_args = called.iter().all(|c| !self.funcs.contains_key(c) && !self.externs.contains_key(c));
                        if is_task && pure_args {
                            let (_, irf) = self.prog.function(&path[0]).unwrap();
                            let rt = rty(&irf.ret);
                            let a: Vec<String> = args.iter().map(|a| self.expr(a)).collect::<R<_>>()?;
                            write!(s, "let {t} = zal_rt::spawn::<{rt}>(Box::pin(f_{}({}))); ", path[0], a.join(", ")).unwrap();
                            ws.push(format!("{t}.wait().await?"));
                            continue;
                        }
                    }
                    let mut names = HashSet::new();
                    names_in_expr(x, &mut names);
                    let body = self.spawned(|g| g.expr(x))?;
                    let sp = self.spawn_of(names, body, self.ty_of(x).cloned().as_ref());
                    write!(s, "let {t} = {sp}; ").unwrap();
                    ws.push(format!("{t}.wait().await?"));
                }
                // Same-typed branches: one await for all of them, so the enclosing
                // state machine gets one suspension point instead of one per branch.
                let tys: Vec<Option<String>> = es.iter().map(|x| self.ty_of(x).map(rty)).collect();
                if tys.iter().all(|t| t.is_some() && *t == tys[0]) {
                    let handles: Vec<String> = ws.iter().map(|w| w.trim_end_matches(".wait().await?").to_string()).collect();
                    let n = handles.len();
                    write!(s, "let mut __all = zal_rt::wait_all(vec![{}]).await?.into_iter(); ", handles.join(", ")).unwrap();
                    let parts: Vec<String> = (0..n).map(|_| "__all.next().unwrap()".to_string()).collect();
                    return Ok(s + &format!("({},) }}", parts.join(", ")));
                }
                s + &format!("({},) }}", ws.join(", "))
            }
            ExprKind::Parallel(es) => {
                // Branches are awaited right here, so they run as concurrently
                // polled futures inside this task (no task per branch). They
                // borrow locals instead of cloning them; dropping the task drops
                // (cancels) every branch.
                self.need_async("parallel")?;
                let mut s = String::from("{ ");
                let mut polls = String::new();
                let mut done = vec![];
                let mut outs = vec![];
                for (k, x) in es.iter().enumerate() {
                    let body = self.with_async(|g| g.expr(x))?;
                    let t = self.ty_of(x).map(rty).unwrap_or_else(|| "_".into());
                    write!(s, "let mut __f{k} = std::pin::pin!(async {{ Ok::<{t}, zal_rt::Trap>({body}) }}); let mut __r{k}: Option<Result<{t}, zal_rt::Trap>> = None; ").unwrap();
                    write!(polls, "if __r{k}.is_none() {{ if let std::task::Poll::Ready(v) = __f{k}.as_mut().poll(__cx) {{ __r{k} = Some(v); }} }} ").unwrap();
                    done.push(format!("__r{k}.is_some()"));
                    outs.push(format!("__r{k}.take().unwrap()?"));
                }
                write!(s, "std::future::poll_fn(|__cx| {{ use std::future::Future; {polls}if {} {{ std::task::Poll::Ready(()) }} else {{ std::task::Poll::Pending }} }}).await; ({},) }}", done.join(" && "), outs.join(", ")).unwrap();
                s
            }
            ExprKind::Race(es) => {
                self.need_async("race")?;
                let mut sps = vec![];
                for x in es {
                    let mut names = HashSet::new();
                    names_in_expr(x, &mut names);
                    let body = self.spawned(|g| g.expr(x))?;
                    sps.push(self.spawn_of(names, body, self.ty_of(x).cloned().as_ref()));
                }
                format!("zal_rt::race(vec![{}]).await?", sps.join(", "))
            }
            ExprKind::Timeout { dur, body } => {
                self.need_async("timeout")?;
                let mut names = HashSet::new();
                names_in_block(body, &mut names);
                let inner = match &ty {
                    Some(Ty::Option(t)) => Some((**t).clone()),
                    _ => None,
                };
                let b = self.spawned(|g| g.block(body))?;
                let sp = self.spawn_of(names, b, inner.as_ref());
                format!("zal_rt::timeout({}, {sp}).await?", self.expr(dur)?)
            }
            ExprKind::Retry { attempts, backoff, body } => {
                self.need_async("retry")?;
                let n = self.fresh("n");
                let i = self.fresh("i");
                let r = self.fresh("r");
                let b = self.with_async(|g| g.block(body))?;
                let t = ty.as_ref().map(rty).unwrap_or_else(|| "_".into());
                let sleep = match backoff {
                    Some(d) => format!("zal_rt::sleep({}).await?;", self.expr(d)?),
                    None => String::new(),
                };
                format!(
                    "{{ let {n} = {}; let mut {i} = 0i64; loop {{ zal_rt::check_deadline()?; let {r}: {t} = (async {{ Ok::<{t}, zal_rt::Trap>({b}) }}).await?; {i} += 1; if {r}.is_ok() || {i} >= {n} {{ break {r}; }} {sleep} }} }}",
                    self.expr(attempts)?
                )
            }
            ExprKind::Spawn(x) => {
                self.need_async("spawn")?;
                let mut names = HashSet::new();
                names_in_expr(x, &mut names);
                let body = self.spawned(|g| g.expr(x))?;
                self.spawn_of(names, body, self.ty_of(x).cloned().as_ref())
            }
            ExprKind::Await(x) => {
                self.need_async("await")?;
                format!("({}).wait().await?", self.expr(x)?)
            }
        })
    }

    /// A borrow of an expression: plain locals are borrowed in place (no
    /// refcount traffic), anything else is evaluated into a temporary.
    fn borrow(&mut self, e: &Expr) -> R<String> {
        match &e.kind {
            ExprKind::Var(n) if self.refs.contains(n) => return Ok(format!("({})", ident(n))),
            ExprKind::Var(n) if self.is_local(n) => return Ok(format!("(&{})", ident(n))),
            ExprKind::Field(o, f) => return Ok(format!("(&{}.f_{f})", self.place(o)?)),
            ExprKind::Index(l, i) => return Ok(format!("(zal_rt::index_ref({}, {})?)", self.borrow(l)?, self.expr(i)?)),
            _ => {}
        }
        Ok(format!("(&{})", self.expr(e)?))
    }

    /// The place of an expression for field/tuple access (no clone of the base).
    fn place(&mut self, e: &Expr) -> R<String> {
        match &e.kind {
            ExprKind::Var(n) if self.refs.contains(n) => return Ok(format!("(*{})", ident(n))),
            ExprKind::Var(n) if self.is_local(n) => return Ok(ident(n)),
            ExprKind::Index(l, i) => return Ok(format!("(zal_rt::index_ref({}, {})?)", self.borrow(l)?, self.expr(i)?)),
            ExprKind::Field(o, f) => return Ok(format!("({}.f_{f})", self.place(o)?)),
            _ => {}
        }
        Ok(format!("({})", self.expr(e)?))
    }

    /// Generate a body that becomes its own task ('static: no borrowed locals).
    fn spawned<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        let was = self.lits_local;
        self.lits_local = false;
        // Captured variables are owned copies inside the task.
        let refs = std::mem::take(&mut self.refs);
        let r = self.with_async(f);
        self.refs = refs;
        self.lits_local = was;
        r
    }

    fn with_async<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        let was = self.in_async;
        self.in_async = true;
        let r = f(self);
        self.in_async = was;
        r
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, whole: Option<&Ty>) -> R<String> {
        // Arithmetic: the operands have the result's type (nested chains are
        // not recorded individually by the checker).
        let lt = match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => whole.cloned().or_else(|| self.ty_of(l).cloned()),
            _ => self.ty_of(l).cloned(),
        };
        if op == BinOp::Add && lt == Some(Ty::Str) {
            let mut parts = vec![];
            flatten_add(l, &mut parts);
            flatten_add(r, &mut parts);
            let mut xs: Vec<String> = vec![];
            for p in &parts {
                match &p.kind {
                    ExprKind::Str(lit_text) => xs.push(format!("{lit_text:?}")),
                    ExprKind::Var(n) if self.refs.contains(n) => xs.push(format!("&**{}", ident(n))),
                    ExprKind::Var(n) if self.is_local(n) => xs.push(format!("&*{}", ident(n))),
                    _ => xs.push(format!("&*{}", self.expr(p)?)),
                }
            }
            return Ok(format!("zal_rt::concat(&[{}])", xs.join(", ")));
        }
        if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
            let (a, b) = (self.borrow(l)?, self.borrow(r)?);
            let o = match op {
                BinOp::Eq => "==",
                BinOp::Ne => "!=",
                BinOp::Lt => "<",
                BinOp::Le => "<=",
                BinOp::Gt => ">",
                _ => ">=",
            };
            return Ok(format!("({a} {o} {b})"));
        }
        let (a, b) = (self.expr(l)?, self.expr(r)?);
        Ok(match (op, lt) {
            (BinOp::And, _) => format!("({a} && {b})"),
            (BinOp::Or, _) => format!("({a} || {b})"),
            (BinOp::Add, Some(Ty::Dur)) => format!("zal_rt::add_d({a}, {b})"),
            (BinOp::Sub, Some(Ty::Dur)) => format!("zal_rt::sub_d({a}, {b})?"),
            (BinOp::Add, _) => format!("zal_rt::add_i({a}, {b})?"),
            (BinOp::Sub, _) => format!("zal_rt::sub_i({a}, {b})?"),
            (BinOp::Mul, _) => format!("zal_rt::mul_i({a}, {b})?"),
            (BinOp::Div, _) => format!("zal_rt::div_i({a}, {b})?"),
            (BinOp::Mod, _) => format!("zal_rt::rem_i({a}, {b})?"),
            (BinOp::Eq, _) => format!("({a} == {b})"),
            (BinOp::Ne, _) => format!("({a} != {b})"),
            (BinOp::Lt, _) => format!("({a} < {b})"),
            (BinOp::Le, _) => format!("({a} <= {b})"),
            (BinOp::Gt, _) => format!("({a} > {b})"),
            (BinOp::Ge, _) => format!("({a} >= {b})"),
        })
    }

    fn call(&mut self, path: &[String], args: &[Expr], ty: Option<&Ty>) -> R<String> {
        // Builtins that only read an argument get a borrow of it.
        let by_ref: &[usize] = match (path.len(), path[0].as_str()) {
            (1, "len" | "get" | "map_keys" | "str" | "json_str" | "await_any" | "cancel") => &[0],
            (1, "map_get" | "map_has" | "json_get_str") => &[0, 1],
            _ => &[],
        };
        if !by_ref.is_empty() && !self.funcs.contains_key(&path[0]) && !self.externs.contains_key(&path[0]) {
            let mut b = vec![];
            for (i, x) in args.iter().enumerate() {
                b.push(if by_ref.contains(&i) { self.borrow(x)? } else { self.expr(x)? });
            }
            let r = |i: usize| b[i].clone();
            return Ok(match path[0].as_str() {
                "len" => format!("zal_rt::ZLen::zlen({})", r(0)),
                "get" => format!("zal_rt::get({}, {})", r(0), r(1)),
                "map_keys" => format!("zal_rt::map_keys({})", r(0)),
                "str" => format!("zal_rt::to_str({})", r(0)),
                "json_str" => format!("zal_rt::json_str({})", r(0)),
                "await_any" => {
                    self.need_async("await_any")?;
                    format!("zal_rt::await_any({}).await?", r(0))
                }
                "cancel" => format!("{{ zal_rt::cancel({}); }}", r(0)),
                "map_get" => format!("zal_rt::map_get({}, {})", r(0), r(1)),
                "map_has" => format!("zal_rt::map_has({}, {})", r(0), r(1)),
                _ => format!("zal_rt::json_get_str({}, {})", r(0), r(1)),
            });
        }
        let a: Vec<String> = args.iter().map(|x| self.expr(x)).collect::<R<_>>()?;
        if path.len() == 2 {
            return Ok(format!("E_{}::{}({})", path[0], path[1], a.join(", ")));
        }
        let name = path[0].as_str();
        if let Some(kind) = self.funcs.get(name).copied() {
            return Ok(match kind {
                FuncKind::Fn => format!("f_{name}({})?", a.join(", ")),
                _ => {
                    self.need_async(name)?;
                    format!("f_{name}({}).await?", a.join(", "))
                }
            });
        }
        if self.externs.contains_key(name) {
            self.need_async(name)?;
            return Ok(format!("x_{name}({}).await?", a.join(", ")));
        }
        let r = |i: usize| a[i].clone();
        Ok(match name {
            "Some" => format!("Some({})", r(0)),
            "Ok" => format!("Ok({})", r(0)),
            "Err" => format!("Err({})", r(0)),
            "len" => format!("zal_rt::ZLen::zlen(&{})", r(0)),
            "push" => format!("zal_rt::push({}, {})", r(0), r(1)),
            "set" => format!("zal_rt::set({}, {}, {})?", r(0), r(1), r(2)),
            "remove" => format!("zal_rt::remove({}, {})?", r(0), r(1)),
            "get" => format!("zal_rt::get(&{}, {})", r(0), r(1)),
            "range" => format!("zal_rt::range({})", r(0)),
            "map_new" => {
                let t = match ty {
                    Some(Ty::Map(t)) => rty(t),
                    _ => "_".into(),
                };
                format!("zal_rt::map_new::<{t}>()")
            }
            "map_with_capacity" => {
                let t = match ty {
                    Some(Ty::Map(t)) => rty(t),
                    _ => "_".into(),
                };
                format!("zal_rt::map_with_capacity::<{t}>({})", r(0))
            }
            "map_get" => format!("zal_rt::map_get(&{}, &{})", r(0), r(1)),
            "map_set" => format!("zal_rt::map_set({}, {}, {})", r(0), r(1), r(2)),
            "map_has" => format!("zal_rt::map_has(&{}, &{})", r(0), r(1)),
            "map_keys" => format!("zal_rt::map_keys(&{})", r(0)),
            "str" => format!("zal_rt::to_str(&{})", r(0)),
            "print" => format!("{{ println!(\"{{}}\", {}); }}", r(0)),
            "json_str" => format!("zal_rt::json_str(&{})", r(0)),
            "json_get_str" => format!("zal_rt::json_get_str(&{}, &{})", r(0), r(1)),
            "now_ms" => "zal_rt::now_ms()".into(),
            "join" => {
                self.need_async("join")?;
                format!("zal_rt::join({}).await?", r(0))
            }
            "await_any" => {
                self.need_async("await_any")?;
                format!("zal_rt::await_any(&{}).await?", r(0))
            }
            "cancel" => format!("{{ zal_rt::cancel(&{}); }}", r(0)),
            "sleep" => {
                self.need_async("sleep")?;
                format!("zal_rt::sleep({}).await?", r(0))
            }
            other => return Err(format!("native backend: unknown function `{other}`")),
        })
    }

    fn pattern(&self, p: &Pattern, scrut: &Ty) -> String {
        match p {
            Pattern::Wildcard => "_".into(),
            Pattern::Bind(n) => ident(n),
            Pattern::Int(n) => format!("{n}i64"),
            Pattern::Str(s) => format!("{s:?}"),
            Pattern::Bool(b) => b.to_string(),
            Pattern::Variant { path, bind } => {
                let last = path.last().unwrap();
                let b = bind.as_ref().map(|n| if n == "_" { "_".to_string() } else { ident(n) });
                let head = match scrut {
                    Ty::Enum(en) => format!("E_{en}::{last}"),
                    _ => last.clone(),
                };
                match b {
                    Some(b) => format!("{head}({b})"),
                    None if matches!(scrut, Ty::Enum(_)) && self.variant_has_payload(scrut, last) => format!("{head}(_)"),
                    None => head,
                }
            }
        }
    }

    /// Scrutinee type from the patterns, for nodes without a recorded type.
    fn infer_scrut(&self, arms: &[(Pattern, Expr)]) -> Ty {
        for (p, _) in arms {
            match p {
                Pattern::Str(_) => return Ty::Str,
                Pattern::Int(_) => return Ty::Int,
                Pattern::Bool(_) => return Ty::Bool,
                Pattern::Variant { path, .. } => {
                    let last = path.last().unwrap().as_str();
                    match last {
                        "Some" | "None" => return Ty::Option(Box::new(Ty::Unit)),
                        "Ok" | "Err" => return Ty::Result(Box::new(Ty::Unit), Box::new(Ty::Unit)),
                        _ => {
                            if path.len() == 2 {
                                return Ty::Enum(path[0].clone());
                            }
                            if let Some(e) = self.prog.enums.iter().find(|e| e.variants.iter().any(|(n, _)| n == last)) {
                                return Ty::Enum(e.name.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ty::Unit
    }

    fn variant_has_payload(&self, t: &Ty, v: &str) -> bool {
        match t {
            Ty::Enum(en) => self.prog.enum_def(en).is_some_and(|e| e.variants.iter().any(|(n, p)| n == v && p.is_some())),
            _ => false,
        }
    }

    fn matche(&mut self, scrut: &Expr, arms: &[(Pattern, Expr)]) -> R<String> {
        let st = match self.ty_of(scrut).cloned() {
            Some(t) => t,
            None => self.infer_scrut(arms),
        };
        let sv = self.expr(scrut)?;
        let head = if st == Ty::Str { format!("&*{sv}") } else { sv };
        let mut s = format!("(match {head} {{\n");
        for (p, body) in arms {
            self.scopes.push(HashSet::new());
            if let Pattern::Bind(n) = p {
                self.bind(n);
            }
            if let Pattern::Variant { bind: Some(n), .. } = p {
                self.bind(n);
            }
            let pat = self.pattern(p, &st);
            let b = self.expr(body)?;
            self.scopes.pop();
            writeln!(s, "{pat} => {{ {b} }}").unwrap();
        }
        Ok(s + "})")
    }
}

fn calls_in_block(b: &Block, out: &mut HashSet<String>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { value: e, .. } | Stmt::Assign { value: e, .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => calls_in_expr(e, out),
            Stmt::While { cond, body, .. } => {
                calls_in_expr(cond, out);
                calls_in_block(body, out);
            }
            Stmt::For { iter, body, .. } => {
                calls_in_expr(iter, out);
                calls_in_block(body, out);
            }
            Stmt::Ensure { cond, err, .. } | Stmt::Assert { cond, msg: err, .. } => {
                calls_in_expr(cond, out);
                calls_in_expr(err, out);
            }
            _ => {}
        }
    }
    if let Some(t) = &b.tail {
        calls_in_expr(t, out);
    }
}

fn calls_in_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Call { path, args } => {
            if path.len() == 1 {
                out.insert(path[0].clone());
            }
            args.iter().for_each(|a| calls_in_expr(a, out));
        }
        ExprKind::Field(a, _) | ExprKind::TupleIdx(a, _) | ExprKind::Neg(a) | ExprKind::Not(a) | ExprKind::Try(a) | ExprKind::Spawn(a) | ExprKind::Await(a) => calls_in_expr(a, out),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
            calls_in_expr(a, out);
            calls_in_expr(b, out);
        }
        ExprKind::If { cond, then, els } => {
            calls_in_expr(cond, out);
            calls_in_block(then, out);
            if let Some(e) = els {
                calls_in_expr(e, out);
            }
        }
        ExprKind::Match { scrut, arms } => {
            calls_in_expr(scrut, out);
            arms.iter().for_each(|(_, a)| calls_in_expr(a, out));
        }
        ExprKind::Block(b) => calls_in_block(b, out),
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Parallel(es) | ExprKind::Race(es) => es.iter().for_each(|a| calls_in_expr(a, out)),
        ExprKind::Record { fields, .. } => fields.iter().for_each(|(_, a)| calls_in_expr(a, out)),
        ExprKind::Timeout { dur, body } => {
            calls_in_expr(dur, out);
            calls_in_block(body, out);
        }
        ExprKind::Retry { attempts, backoff, body } => {
            calls_in_expr(attempts, out);
            if let Some(b) = backoff {
                calls_in_expr(b, out);
            }
            calls_in_block(body, out);
        }
        _ => {}
    }
}

fn flatten_add<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &e.kind {
        ExprKind::Binary(BinOp::Add, l, r) => {
            flatten_add(l, out);
            flatten_add(r, out);
        }
        _ => out.push(e),
    }
}

/// Generate the Rust program: types, host trait, mock host, functions, entry
/// dispatch and a benchmark `main`.
pub fn generate(modules: &[Module], prog: &Program, types: &HashMap<usize, Ty>) -> Result<Native, String> {
    let mut g = Gen {
        prog,
        types,
        funcs: HashMap::new(),
        externs: HashMap::new(),
        enums: HashSet::new(),
        scopes: vec![],
        in_async: false,
        uniq: 0,
        record_of: HashMap::new(),
        untyped: 0,
        lits: HashMap::new(),
        lits_local: false,
        refs: HashSet::new(),
        moves: HashSet::new(),
    };
    for m in modules {
        for it in &m.items {
            match it {
                Item::Func(f) if f.kind != FuncKind::Test => {
                    g.funcs.insert(f.name.clone(), f.kind);
                }
                Item::Record { name, fields, .. } => {
                    g.record_of.insert(name.clone(), fields.iter().map(|f| f.0.clone()).collect());
                }
                Item::Enum { name, .. } => {
                    g.enums.insert(name.clone());
                }
                _ => {}
            }
        }
    }
    for (i, x) in prog.externs.iter().enumerate() {
        g.externs.insert(x.name.clone(), i);
    }
    let mut out = String::new();
    out += "#![allow(unused, non_camel_case_types, non_snake_case, unreachable_code, unused_parens, clippy::all)]\n";
    out += "// Generated by `zal native`. Do not edit.\nuse std::rc::Rc;\nuse std::cell::{Cell, RefCell};\nuse std::collections::HashMap;\nuse zal_rt::{Str, List, Map, Dur, Task, Trap, Fut, FromJson, ToJson, ZStr, Json};\n\n";

    // Records: a cheap-to-clone handle around shared immutable data.
    for r in &prog.records {
        let fields: Vec<String> = r.fields.iter().map(|(n, t)| format!("pub f_{n}: {}", rty(t))).collect();
        writeln!(out, "#[derive(Clone, PartialEq, Debug)]\npub struct D_{0} {{ {1} }}\n#[derive(Clone, PartialEq, Debug)]\npub struct R_{0}(pub Rc<D_{0}>);\nimpl std::ops::Deref for R_{0} {{ type Target = D_{0}; fn deref(&self) -> &D_{0} {{ &self.0 }} }}", r.name, fields.join(", ")).unwrap();
        let from: Vec<String> = r.fields.iter().map(|(n, _)| format!("f_{n}: FromJson::from_json(o.get({n:?}).ok_or(\"missing field {n}\")?)?")).collect();
        let to: Vec<String> = r.fields.iter().map(|(n, _)| format!("({n:?}.to_string(), self.f_{n}.to_json())")).collect();
        let disp: Vec<String> = r.fields.iter().map(|(n, _)| format!("self.f_{n}.zstr()")).collect();
        writeln!(out, "impl FromJson for R_{0} {{ fn from_json(j: &Json) -> Result<Self, String> {{ let o = j.as_object().ok_or(\"expected object\")?; Ok(R_{0}(Rc::new(D_{0} {{ {1} }}))) }} }}", r.name, from.join(", ")).unwrap();
        writeln!(out, "impl ToJson for R_{0} {{ fn to_json(&self) -> Json {{ Json::Object(vec![{1}].into_iter().collect()) }} }}", r.name, to.join(", ")).unwrap();
        writeln!(out, "impl ZStr for R_{0} {{ fn zstr(&self) -> String {{ format!(\"({{}})\", vec![{1}].join(\", \")) }} }}", r.name, disp.join(", ")).unwrap();
        let zs: Vec<String> = r.fields.iter().map(|(n, _)| format!("f_{n}: zal_rt::ZZero::zero()")).collect();
        writeln!(out, "impl zal_rt::ZZero for R_{0} {{ fn zero() -> Self {{ R_{0}(Rc::new(D_{0} {{ {1} }})) }} }}\n", r.name, zs.join(", ")).unwrap();
    }
    for en in &prog.enums {
        let vs: Vec<String> = en.variants.iter().map(|(n, p)| match p {
            Some(t) => format!("{n}({})", rty(t)),
            None => n.clone(),
        }).collect();
        writeln!(out, "#[derive(Clone, PartialEq, Debug)]\npub enum E_{} {{ {} }}", en.name, vs.join(", ")).unwrap();
        let disp: Vec<String> = en.variants.iter().enumerate().map(|(i, (n, p))| match p {
            Some(_) => format!("E_{}::{n}(x) => format!(\"#{i}({{}})\", x.zstr())", en.name),
            None => format!("E_{}::{n} => \"#{i}\".to_string()", en.name),
        }).collect();
        let to: Vec<String> = en.variants.iter().map(|(n, p)| match p {
            Some(_) => format!("E_{}::{n}(x) => serde_json::json!({{\"variant\": {n:?}, \"value\": x.to_json()}})", en.name),
            None => format!("E_{}::{n} => Json::from({n:?})", en.name),
        }).collect();
        writeln!(out, "impl ZStr for E_{0} {{ fn zstr(&self) -> String {{ match self {{ {1} }} }} }}", en.name, disp.join(", ")).unwrap();
        writeln!(out, "impl ToJson for E_{0} {{ fn to_json(&self) -> Json {{ match self {{ {1} }} }} }}", en.name, to.join(", ")).unwrap();
        let from: Vec<String> = en.variants.iter().map(|(n, p)| match p {
            Some(_) => format!("{n:?} => Ok(E_{}::{n}(FromJson::from_json(payload.ok_or(\"missing value\")?)?))", en.name),
            None => format!("{n:?} => Ok(E_{}::{n})", en.name),
        }).collect();
        writeln!(out, "impl FromJson for E_{0} {{ fn from_json(j: &Json) -> Result<Self, String> {{ let (name, payload) = match j {{ Json::String(s) => (s.as_str(), None), Json::Object(o) => (o.get(\"variant\").and_then(|v| v.as_str()).ok_or(\"expected variant\")?, o.get(\"value\")), _ => return Err(format!(\"JSON {{j}} is not a {0}\")) }}; match name {{ {1}, _ => Err(format!(\"unknown variant {{name}}\")) }} }} }}", en.name, from.join(", ")).unwrap();
        let first = &en.variants[0];
        let z = match &first.1 { Some(_) => format!("E_{}::{}(zal_rt::ZZero::zero())", en.name, first.0), None => format!("E_{}::{}", en.name, first.0) };
        writeln!(out, "impl zal_rt::ZZero for E_{0} {{ fn zero() -> Self {{ {1} }} }}\n", en.name, z).unwrap();
    }

    // Host ABI: one typed method per extern; every call is authorized first.
    let effects: Vec<String> = prog.externs.iter().map(|x| format!("{:?}", x.effect)).collect();
    writeln!(out, "pub const EFFECTS: &[&str] = &[{}];", effects.join(", ")).unwrap();
    let names: Vec<String> = prog.externs.iter().map(|x| format!("{:?}", x.name)).collect();
    writeln!(out, "pub const EXTERN_NAMES: &[&str] = &[{}];", names.join(", ")).unwrap();
    out += "pub trait Host {\n";
    for x in &prog.externs {
        let ps: Vec<String> = x.params.iter().enumerate().map(|(i, (_, t))| format!("a{i}: {}", rty(t))).collect();
        writeln!(out, "    fn x_{}(&self, {}) -> zal_rt::Reply<{}>;", x.name, ps.join(", "), rty(&x.ret)).unwrap();
    }
    out += "}\nthread_local! { pub static HOST: RefCell<Option<Rc<dyn Host>>> = RefCell::new(None); }\n";
    for (i, x) in prog.externs.iter().enumerate() {
        let ps: Vec<String> = x.params.iter().enumerate().map(|(i, (_, t))| format!("a{i}: {}", rty(t))).collect();
        let an: Vec<String> = (0..x.params.len()).map(|i| format!("a{i}")).collect();
        writeln!(
            out,
            "async fn x_{0}({1}) -> Result<{2}, Trap> {{ zal_rt::authorize({3}, {0:?})?; let _p = if zal_rt::limited() {{ Some(zal_rt::permit().await) }} else {{ None }}; match HOST.with(|h| h.borrow().as_ref().expect(\"host\").x_{0}({4})) {{ zal_rt::Reply::Now(v) => v, zal_rt::Reply::Later(f) => f.await }} }}",
            x.name, ps.join(", "), rty(&x.ret), i, an.join(", ")
        ).unwrap();
    }

    // Mock host for tests and benchmarks (same options as the VM's MockHost).
    out += "\n#[derive(Default, Clone)]\npub struct MockCfg { pub echo_arg: Option<usize>, pub suffix: Option<String>, pub result: Option<Json>, pub latency_ms: u64, pub latency_by_arg0: HashMap<String, u64>, pub fail_first: u64 }\n";
    out += "#[derive(Default)]\npub struct MockHost { pub cfg: HashMap<String, MockCfg>, pub slots: Vec<Option<MockCfg>>, pub calls: Vec<Cell<u64>>, pub checksum_arg: Option<usize>, pub checksum: Cell<u64> }\n";
    out += "impl MockHost { pub fn from_json(j: &Json) -> Self { let mut h = MockHost::default(); h.checksum_arg = j.get(\"checksum_arg\").and_then(|v| v.as_u64()).map(|v| v as usize); if let Some(t) = j.get(\"tools\").and_then(|t| t.as_object()) { for (k, c) in t { h.cfg.insert(k.clone(), MockCfg { echo_arg: c.get(\"echo_arg\").and_then(|v| v.as_u64()).map(|v| v as usize).or(if c.get(\"echo\").and_then(|v| v.as_bool()) == Some(true) { Some(0) } else { None }), suffix: c.get(\"suffix\").and_then(|v| v.as_str()).map(String::from), result: c.get(\"result\").cloned(), latency_ms: c.get(\"latency_ms\").and_then(|v| v.as_u64()).unwrap_or(0), latency_by_arg0: c.get(\"latency_by_arg0\").and_then(|v| v.as_object()).map(|o| o.iter().filter_map(|(k, v)| v.as_u64().map(|ms| (k.clone(), ms))).collect()).unwrap_or_default(), fail_first: c.get(\"fail_first\").and_then(|v| v.as_u64()).unwrap_or(0) }); } } h.slots = EXTERN_NAMES.iter().map(|n| h.cfg.get(*n).cloned()).collect(); h.calls = EXTERN_NAMES.iter().map(|_| Cell::new(0)).collect(); h } }\n";
    out += "impl Host for MockHost {\n";
    for (mi, x) in prog.externs.iter().enumerate() {
        let ps: Vec<String> = x.params.iter().enumerate().map(|(i, (_, t))| format!("a{i}: {}", rty(t))).collect();
        let (ok_ty, is_result) = match &x.ret {
            Ty::Result(a, _) => ((**a).clone(), true),
            t => (t.clone(), false),
        };
        let wrap = |v: &str| if is_result { format!("Ok({v})") } else { v.to_string() };
        let mut echo = String::new();
        for (i, (_, t)) in x.params.iter().enumerate() {
            if *t == ok_ty {
                write!(echo, "if c.echo_arg == Some({i}) {{ return finish(lat, {}); }} ", wrap(&format!("a{i}"))).unwrap();
            }
        }
        let mut sum = String::new();
        for (i, (_, t)) in x.params.iter().enumerate() {
            if *t == Ty::Str {
                write!(sum, "if self.checksum_arg == Some({i}) {{ self.checksum.set(self.checksum.get() + a{i}.len() as u64); }} ").unwrap();
            }
        }
        // First `fail_first` calls fail (Result externs only), like the VM mock.
        let fail = match &x.ret {
            Ty::Result(_, e) => {
                let err = if **e == Ty::Str { "zal_rt::s(\"mock failure\")".to_string() } else { "zal_rt::ZZero::zero()".to_string() };
                format!("if __n <= c.fail_first {{ return zal_rt::Reply::Now(Ok(Err({err}))); }} ")
            }
            _ => "if __n <= c.fail_first { return zal_rt::Reply::Now(Err(Trap(\"mock failure\".into()))); } ".to_string(),
        };
        let lat = if x.params.first().map(|p| &p.1) == Some(&Ty::Str) {
            "let lat = if c.latency_by_arg0.is_empty() { c.latency_ms } else { c.latency_by_arg0.get(&*a0).copied().unwrap_or(c.latency_ms) }; ".to_string()
        } else {
            "let lat = c.latency_ms; ".to_string()
        };
        let suffix = if ok_ty == Ty::Str && x.params.first().map(|p| &p.1) == Some(&Ty::Str) {
            format!("if let Some(sfx) = &c.suffix {{ return finish(lat, {}); }} ", wrap("zal_rt::concat(&[&*a0, sfx])"))
        } else {
            String::new()
        };
        writeln!(
            out,
            "    fn x_{0}(&self, {1}) -> zal_rt::Reply<{2}> {{ {sum}let __n = {{ let n = self.calls[{3}].get() + 1; self.calls[{3}].set(n); n }}; let c = match &self.slots[{3}] {{ Some(c) => c, None => return zal_rt::Reply::Now(Ok(<{2} as zal_rt::ZZero>::zero())) }}; {fail} fn finish<T: 'static>(ms: u64, v: T) -> zal_rt::Reply<T> {{ if ms == 0 {{ zal_rt::Reply::Now(Ok(v)) }} else {{ zal_rt::Reply::Later(Box::pin(async move {{ zal_rt::sleep(zal_rt::Dur(ms as i64)).await?; Ok(v) }})) }} }} {lat}{echo}{suffix}match &c.result {{ Some(j) => match <{2} as FromJson>::from_json(j) {{ Ok(v) => finish(lat, v), Err(e) => zal_rt::Reply::Now(Err(Trap(e))) }}, None => finish(lat, <{2} as zal_rt::ZZero>::zero()) }} }}",
            x.name, ps.join(", "), rty(&x.ret), mi
        ).unwrap();
    }
    out += "}\n\n";

    // Call graph: which functions can reach themselves.
    let mut calls: HashMap<String, HashSet<String>> = HashMap::new();
    for m in modules {
        for it in &m.items {
            if let Item::Func(f) = it {
                let mut c = HashSet::new();
                calls_in_block(&f.body, &mut c);
                calls.insert(f.name.clone(), c);
            }
        }
    }
    let mut recursive: HashSet<String> = HashSet::new();
    for start in calls.keys() {
        let mut stack: Vec<&String> = calls[start].iter().collect();
        let mut seen: HashSet<&String> = HashSet::new();
        while let Some(n) = stack.pop() {
            if n == start {
                recursive.insert(start.clone());
                break;
            }
            if seen.insert(n) {
                if let Some(next) = calls.get(n) {
                    stack.extend(next.iter());
                }
            }
        }
    }

    // Functions.
    for m in modules {
        for it in &m.items {
            let Item::Func(f) = it else { continue };
            if f.kind == FuncKind::Test {
                continue;
            }
            let (_, irf) = prog.function(&f.name).ok_or("missing function")?;
            let ps: Vec<String> = irf.params.iter().map(|(n, t)| format!("{}: {}", ident(n), rty(t))).collect();
            g.scopes = vec![irf.params.iter().map(|(n, _)| n.clone()).collect()];
            g.moves = crate::lastuse::moves(&f.body);
            let rt = rty(&irf.ret);
            g.lits_local = true;
            let pro_full = "let __lits = LITS.with(|l| l.clone());";
            if f.kind == FuncKind::Fn {
                g.in_async = false;
                let body = g.block(&f.body)?;
                let pro = if body.contains("__lits.") { pro_full } else { "" };
                writeln!(out, "pub fn f_{}({}) -> Result<{rt}, Trap> {{ let _depth = zal_rt::enter()?; {pro} Ok({body}) }}\n", f.name, ps.join(", ")).unwrap();
            } else {
                g.in_async = true;
                let body = g.block(&f.body)?;
                let pro = if body.contains("__lits.") { pro_full } else { "" };
                if recursive.contains(&f.name) {
                    // Recursive async functions need an indirection.
                    writeln!(out, "pub fn f_{}({}) -> Fut<{rt}> {{ Box::pin(async move {{ let _depth = zal_rt::enter()?; {pro} Ok({body}) }}) }}\n", f.name, ps.join(", ")).unwrap();
                } else {
                    writeln!(out, "pub async fn f_{}({}) -> Result<{rt}, Trap> {{ {pro} Ok({body}) }}\n", f.name, ps.join(", ")).unwrap();
                }
            }
        }
    }

    // Interned literals.
    let mut lits: Vec<(&String, &usize)> = g.lits.iter().collect();
    lits.sort_by_key(|(_, k)| **k);
    let fields: Vec<String> = lits.iter().map(|(_, k)| format!("l{k}: Str")).collect();
    let inits: Vec<String> = lits.iter().map(|(t, k)| format!("l{k}: zal_rt::s({t:?})")).collect();
    writeln!(out, "pub struct Lits {{ {} }}", fields.join(", ")).unwrap();
    writeln!(out, "thread_local! {{ static LITS: Rc<Lits> = Rc::new(Lits {{ {} }}); }}", inits.join(", ")).unwrap();

    // Entry dispatch: JSON args in, JSON result out; budgets of the entry apply.
    out += "/// Prepare an entry: arguments are converted once; each call of the returned\n/// closure starts a run with (refcount-)cloned arguments.\npub type Starter = Box<dyn Fn() -> Fut<Json>>;\npub fn entry(name: &str, args: &Json) -> Result<(Starter, Vec<&'static str>, Option<i64>, Option<u32>, Option<u64>), String> {\n    let a = args.as_array().ok_or(\"args must be an array\")?;\n    match name {\n";
    for m in modules {
        for it in &m.items {
            let Item::Func(f) = it else { continue };
            if !matches!(f.kind, FuncKind::Task | FuncKind::Workflow) {
                continue;
            }
            let (_, irf) = prog.function(&f.name).unwrap();
            let conv: Vec<String> = irf.params.iter().enumerate().map(|(i, (_, t))| format!("let p{i}: {} = FromJson::from_json(a.get({i}).ok_or(\"missing argument\")?)?;", rty(t))).collect();
            let use_: Vec<String> = (0..irf.params.len()).map(|i| format!("p{i}.clone()")).collect();
            let effs: Vec<String> = irf.effects.iter().map(|e| format!("{e:?}")).collect();
            let b = irf.budget.clone().unwrap_or_default();
            let opt = |o: Option<String>| o.map(|v| format!("Some({v})")).unwrap_or_else(|| "None".into());
            writeln!(
                out,
                "        {0:?} => {{ if a.len() != {1} {{ return Err(\"`{0}` takes {1} argument(s)\".into()); }} {2} let start: Starter = Box::new(move || {{ let f = f_{0}({3}); Box::pin(async move {{ let v = f.await?; zal_rt::mark_end(); Ok(v.to_json()) }}) }}); Ok((start, vec![{4}], {5}, {6}, {7})) }}",
                f.name,
                irf.params.len(),
                conv.join(" "),
                use_.join(", "),
                effs.join(", "),
                opt(b.timeout_ms.map(|v| v.to_string())),
                opt(b.max_parallel.map(|v| v.to_string())),
                opt(b.max_tool_calls.map(|v| v.to_string())),
            ).unwrap();
        }
    }
    out += "        other => Err(format!(\"no entry `{other}`\")),\n    }\n}\n";
    Ok(Native { source: out })
}
