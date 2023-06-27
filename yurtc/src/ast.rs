#[derive(Clone, Debug, PartialEq)]
pub(super) struct VarStatement {
    pub(super) name: Ident,
    pub(super) ty: Option<Type>,
    pub(super) init: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct LetStatement {
    pub(super) name: Ident,
    pub(super) ty: Option<Type>,
    pub(super) init: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct CodeBlock {
    pub(super) statements: Vec<Decl>,
    pub(super) final_expr: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Decl {
    Var(VarStatement),
    Let(LetStatement),
    Constraint(Expr),
    Fn {
        name: Ident,
        params: Vec<(Ident, Type)>,
        return_type: Type,
        body: CodeBlock,
    },
    Solve(SolveFunc),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Ident(pub(super) String);

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Type {
    Real,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    Immediate(Immediate),
    Ident(Ident),
    BinaryOp {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Call {
        name: Ident,
        args: Vec<Expr>,
    },
    CodeBlock(CodeBlock),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum BinaryOp {
    Mul,

    LessThan,
    GreaterThan,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Immediate {
    Real(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SolveFunc {
    Satisfy,
    Minimize(Ident),
    Maximize(Ident),
}
