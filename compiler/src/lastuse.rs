//! Last-use analysis for the native backend: which variable reads may move
//! the value instead of cloning it.
//!
//! Every read of a ZAL variable is a clone in the generated Rust (refcount
//! traffic on a separate cache line for every string, list and record). A
//! read that is the variable's last use can move instead. The analysis is
//! conservative on purpose:
//! - a read moves only if the name occurs exactly once in its whole top-level
//!   statement (so no borrow of the same variable can be alive at the move);
//! - inside loops, names that are live around the back edge never move;
//!   names declared fresh in the loop body (first occurrence is the binding)
//!   are per-iteration and may move;
//! - bodies that become their own tasks (spawn, parallel, race, timeout)
//!   capture copies; nothing inside them is marked, and their captured names
//!   count as uses.
//!
//! The result is a set of `Expr` addresses (`ExprKind::Var` nodes).

use std::collections::HashSet;

use crate::ast::*;

pub fn moves(body: &Block) -> HashSet<usize> {
    let mut a = A { moves: HashSet::new() };
    a.block(body, &HashSet::new());
    a.moves
}

struct A {
    moves: HashSet<usize>,
}

fn addr(e: &Expr) -> usize {
    e as *const Expr as usize
}

/// Every variable name read anywhere in an expression (including nested
/// blocks and task bodies).
fn uses_expr(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Var(n) => out.push(n.clone()),
        ExprKind::Call { args, .. } | ExprKind::List(args) | ExprKind::Tuple(args) | ExprKind::Parallel(args) | ExprKind::Race(args) => {
            for a in args {
                uses_expr(a, out);
            }
        }
        ExprKind::Field(a, _) | ExprKind::TupleIdx(a, _) | ExprKind::Neg(a) | ExprKind::Not(a) | ExprKind::Try(a) | ExprKind::Spawn(a) | ExprKind::Await(a) => uses_expr(a, out),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
            uses_expr(a, out);
            uses_expr(b, out);
        }
        ExprKind::If { cond, then, els } => {
            uses_expr(cond, out);
            uses_block(then, out);
            if let Some(e) = els {
                uses_expr(e, out);
            }
        }
        ExprKind::Match { scrut, arms } => {
            uses_expr(scrut, out);
            for (_, e) in arms {
                uses_expr(e, out);
            }
        }
        ExprKind::Block(b) => uses_block(b, out),
        ExprKind::Timeout { dur, body } => {
            uses_expr(dur, out);
            uses_block(body, out);
        }
        ExprKind::Retry { attempts, backoff, body } => {
            uses_expr(attempts, out);
            if let Some(b) = backoff {
                uses_expr(b, out);
            }
            uses_block(body, out);
        }
        ExprKind::Record { fields, .. } => {
            for (_, e) in fields {
                uses_expr(e, out);
            }
        }
        _ => {}
    }
}

fn uses_stmt(s: &Stmt, out: &mut Vec<String>) {
    match s {
        Stmt::Let { value, .. } => uses_expr(value, out),
        Stmt::Assign { name, value, .. } => {
            // The in-place forms (`x = push(x, ..)`) read the old value too.
            out.push(name.clone());
            uses_expr(value, out);
        }
        Stmt::Expr(e) => uses_expr(e, out),
        Stmt::Return(e, _) => {
            if let Some(e) = e {
                uses_expr(e, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            uses_expr(cond, out);
            uses_block(body, out);
        }
        Stmt::For { iter, body, .. } => {
            uses_expr(iter, out);
            uses_block(body, out);
        }
        Stmt::Ensure { cond, err, .. } => {
            uses_expr(cond, out);
            uses_expr(err, out);
        }
        Stmt::Assert { cond, msg, .. } => {
            uses_expr(cond, out);
            uses_expr(msg, out);
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn uses_block(b: &Block, out: &mut Vec<String>) {
    for s in &b.stmts {
        uses_stmt(s, out);
    }
    if let Some(t) = &b.tail {
        uses_expr(t, out);
    }
}

/// Names bound fresh at the top level of a loop body before any read of them
/// (they cannot carry a value around the back edge).
fn fresh_in(body: &Block) -> HashSet<String> {
    let mut seen_read: HashSet<String> = HashSet::new();
    let mut fresh = HashSet::new();
    for s in &body.stmts {
        let mut u = vec![];
        uses_stmt(s, &mut u);
        if let Stmt::Let { names, .. } = s {
            // The value is evaluated before the names are bound.
            seen_read.extend(u);
            for n in names {
                if !seen_read.contains(n) {
                    fresh.insert(n.clone());
                }
            }
        } else {
            seen_read.extend(u);
        }
    }
    fresh
}

impl A {
    /// `live` = names read after this block (on some path).
    fn block(&mut self, b: &Block, live: &HashSet<String>) {
        let mut live = live.clone();
        if let Some(t) = &b.tail {
            self.top(t, &live, None);
            let mut u = vec![];
            uses_expr(t, &mut u);
            live.extend(u);
        }
        for s in b.stmts.iter().rev() {
            self.stmt(s, &mut live);
        }
    }

    fn stmt(&mut self, s: &Stmt, live: &mut HashSet<String>) {
        match s {
            Stmt::Let { names, value, .. } => {
                for n in names {
                    live.remove(n);
                }
                self.top(value, live, None);
                let mut u = vec![];
                uses_expr(value, &mut u);
                live.extend(u);
            }
            Stmt::Assign { name, value, .. } => {
                // The target's old value dies here; the in-place codegen forms
                // borrow it mutably, so a read of `name` in the value never moves.
                live.remove(name);
                self.top(value, live, Some(name));
                let mut u = vec![];
                uses_expr(value, &mut u);
                live.extend(u);
                live.insert(name.clone());
            }
            Stmt::Expr(e) => {
                self.top(e, live, None);
                let mut u = vec![];
                uses_expr(e, &mut u);
                live.extend(u);
            }
            Stmt::Return(e, _) => {
                live.clear();
                if let Some(e) = e {
                    self.top(e, live, None);
                    let mut u = vec![];
                    uses_expr(e, &mut u);
                    live.extend(u);
                }
            }
            Stmt::While { cond, body, .. } => {
                let carried = self.carried(Some(cond), body, live);
                self.block(body, &carried);
                let mut u = vec![];
                uses_expr(cond, &mut u);
                uses_block(body, &mut u);
                live.extend(u);
            }
            Stmt::For { var, iter, body, .. } => {
                let mut carried = self.carried(None, body, live);
                carried.remove(var);
                self.block(body, &carried);
                let mut u = vec![];
                uses_block(body, &mut u);
                u.retain(|n| n != var);
                uses_expr(iter, &mut u);
                live.extend(u);
            }
            Stmt::Ensure { cond, err, .. } | Stmt::Assert { cond, msg: err, .. } => {
                let mut u = vec![];
                uses_expr(cond, &mut u);
                uses_expr(err, &mut u);
                live.extend(u);
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }

    /// Names live at the end of a loop body: everything after the loop, and
    /// everything the loop reads that is not freshly bound per iteration.
    fn carried(&self, cond: Option<&Expr>, body: &Block, after: &HashSet<String>) -> HashSet<String> {
        let mut u = vec![];
        if let Some(c) = cond {
            uses_expr(c, &mut u);
        }
        uses_block(body, &mut u);
        let fresh = fresh_in(body);
        let mut c: HashSet<String> = after.clone();
        c.extend(u.into_iter().filter(|n| !fresh.contains(n)));
        c
    }

    /// A top-level expression of a statement: mark single-occurrence reads of
    /// names that are dead afterwards, then descend into nested blocks.
    fn top(&mut self, e: &Expr, live: &HashSet<String>, target: Option<&String>) {
        let mut u = vec![];
        uses_expr(e, &mut u);
        let mut count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for n in &u {
            *count.entry(n.as_str()).or_default() += 1;
        }
        let single: HashSet<String> = count
            .iter()
            .filter(|(n, c)| **c == 1 && !live.contains(**n) && target.is_none_or(|t| t != **n))
            .map(|(n, _)| n.to_string())
            .collect();
        self.mark(e, &single, live);
    }

    /// Marks direct reads (not inside task bodies); nested blocks get their
    /// own statement-level analysis with the names live after this expression.
    fn mark(&mut self, e: &Expr, single: &HashSet<String>, live: &HashSet<String>) {
        match &e.kind {
            ExprKind::Var(n) => {
                if single.contains(n) {
                    self.moves.insert(addr(e));
                }
            }
            ExprKind::Call { args, .. } | ExprKind::List(args) | ExprKind::Tuple(args) => {
                for a in args {
                    self.mark(a, single, live);
                }
            }
            ExprKind::Field(..) | ExprKind::TupleIdx(..) => {} // place access, no clone of the base
            ExprKind::Neg(a) | ExprKind::Not(a) | ExprKind::Try(a) | ExprKind::Await(a) => self.mark(a, single, live),
            ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
                self.mark(a, single, live);
                self.mark(b, single, live);
            }
            ExprKind::Record { fields, .. } => {
                for (_, f) in fields {
                    self.mark(f, single, live);
                }
            }
            ExprKind::If { cond, then, els } => {
                self.mark(cond, single, live);
                self.block(then, live);
                if let Some(x) = els {
                    self.branch(x, live);
                }
            }
            ExprKind::Match { scrut, arms } => {
                self.mark(scrut, single, live);
                for (_, x) in arms {
                    self.branch(x, live);
                }
            }
            ExprKind::Block(b) => self.block(b, live),
            // Task bodies and retried bodies: no moves inside (copies / re-run).
            _ => {}
        }
    }

    fn branch(&mut self, e: &Expr, live: &HashSet<String>) {
        match &e.kind {
            ExprKind::Block(b) => self.block(b, live),
            _ => self.top(e, live, None),
        }
    }
}
