//! Untyped syntax tree produced by the parser.

#[derive(Clone, Debug)]
pub struct Module {
    pub name: String,
    pub file: String,
    pub imports: Vec<(String, u32)>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub enum Item {
    Record { name: String, fields: Vec<(String, TypeExpr)>, line: u32 },
    Enum { name: String, variants: Vec<(String, Option<TypeExpr>)>, line: u32 },
    /// `readonly`: no observable side effects; may run concurrently and out of order.
    Extern { kind: String, name: String, params: Vec<(String, TypeExpr)>, ret: TypeExpr, readonly: bool, line: u32 },
    Func(FuncDecl),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncKind {
    Fn,
    Task,
    Workflow,
    Test,
}

#[derive(Clone, Debug)]
pub struct FuncDecl {
    pub kind: FuncKind,
    pub name: String,
    pub params: Vec<(String, TypeExpr)>,
    pub ret: Option<TypeExpr>,
    pub requires: Vec<String>,
    pub budget: Vec<(String, Expr)>,
    pub body: Block,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub enum TypeExpr {
    Name(String, Vec<TypeExpr>, u32),
    Tuple(Vec<TypeExpr>, u32),
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let { names: Vec<String>, tuple: bool, mutable: bool, ty: Option<TypeExpr>, value: Expr, line: u32 },
    Assign { name: String, value: Expr, line: u32 },
    Expr(Expr),
    Return(Option<Expr>, u32),
    While { cond: Expr, body: Block, line: u32 },
    For { var: String, iter: Expr, body: Block, line: u32 },
    Ensure { cond: Expr, err: Expr, line: u32 },
    Assert { cond: Expr, msg: Expr, line: u32 },
    Break(u32),
    Continue(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Debug)]
pub enum Pattern {
    /// `Ok(x)`, `None`, `Color.Red`, `Color.Rgb(v)`; binding `_` is allowed.
    Variant { path: Vec<String>, bind: Option<String> },
    Int(i64),
    Str(String),
    Bool(bool),
    Bind(String),
    Wildcard,
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Str(String),
    Bool(bool),
    Dur(i64),
    Unit,
    Var(String),
    /// `Enum.Variant` without payload.
    Path(Vec<String>),
    Call { path: Vec<String>, args: Vec<Expr> },
    Field(Box<Expr>, String),
    TupleIdx(Box<Expr>, u16),
    Index(Box<Expr>, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    If { cond: Box<Expr>, then: Block, els: Option<Box<Expr>> },
    Match { scrut: Box<Expr>, arms: Vec<(Pattern, Expr)> },
    Block(Block),
    List(Vec<Expr>),
    Tuple(Vec<Expr>),
    Record { name: String, fields: Vec<(String, Expr)> },
    Try(Box<Expr>),
    Parallel(Vec<Expr>),
    Race(Vec<Expr>),
    Timeout { dur: Box<Expr>, body: Block },
    Retry { attempts: Box<Expr>, backoff: Option<Box<Expr>>, body: Block },
    Spawn(Box<Expr>),
    Await(Box<Expr>),
}
