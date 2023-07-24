#[derive(Clone, Debug, PartialEq)]
pub(super) enum UseTree {
    Glob,
    Name { name: Ident },
    Path { prefix: Ident, suffix: Box<UseTree> },
    Group { imports: Vec<UseTree> },
    Alias { name: Ident, alias: Ident },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct LetDecl {
    pub(super) name: Ident,
    pub(super) ty: Option<Type>,
    pub(super) init: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Block {
    pub(super) statements: Vec<Decl>,
    pub(super) final_expr: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Decl {
    Use {
        is_absolute: bool,
        use_tree: UseTree,
    },
    Let(LetDecl),
    Constraint(Expr),
    Fn {
        name: Ident,
        params: Vec<(Ident, Type)>,
        return_type: Type,
        body: Block,
    },
    Solve(SolveFunc),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Ident(pub(super) String);

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Type {
    Real,
    Int,
    Bool,
    String,
    Tuple(Vec<Type>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    Immediate(Immediate),
    Ident(Ident),
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    BinaryOp {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Call {
        name: Ident,
        args: Vec<Expr>,
    },
    Block(Block),
    If(IfExpr),
    Cond(CondExpr),
    Tuple(Vec<Expr>),
    TupleIndex {
        tuple: Box<Expr>,
        index: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum UnaryOp {
    Pos,
    Neg,
    Not,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum BinaryOp {
    Mul,
    Div,
    Add,
    Sub,
    Mod,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    Equal,
    NotEqual,
    LogicalAnd,
    LogicalOr,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Immediate {
    Real(f64),
    Int(i64),
    BigInt(num_bigint::BigInt),
    Bool(bool),
    String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SolveFunc {
    Satisfy,
    Minimize(Ident),
    Maximize(Ident),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct IfExpr {
    pub(super) condition: Box<Expr>,
    pub(super) then_block: Block,
    pub(super) else_block: Block,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct CondBranch {
    pub(super) condition: Box<Expr>,
    pub(super) result: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct CondExpr {
    pub(super) branches: Vec<CondBranch>,
    pub(super) else_result: Box<Expr>,
}
