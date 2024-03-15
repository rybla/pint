use crate::{
    error::CompileError,
    expr::{self, Expr, Ident},
    intermediate::{
        IntermediateIntent,
        SolveFunc::{self, *},
    },
    span::empty_span,
    types::{PrimitiveKind, Type},
};

pub(crate) fn canonicalize(ii: &mut IntermediateIntent) -> Result<(), CompileError> {
    canonicalize_directive(ii)?;

    Ok(())
}

/// Canonicalize the solve directive by transforming any maximize or minimize directive into a form
/// that is suitable for constraint-based mathematical solvers.
///
/// This function performs a transformation similar to the following example:
///
/// ```yurt
/// solve maximize <expr>;
/// ```
///
/// becomes:
///
/// ```yurt
/// let __objective: <type_of_expr>;
/// constraint __objective == <expr>;
/// solve maximize __objective;
/// ```
///
/// This transformation is necessary because while the `solve maximize <expr>` or `solve minimize <expr>`
/// form is convenient for the user, it is not in the proper form for the solvers. This function
/// therefore transforms the solve directive into a form that can be handled by the solver.
///
/// Note that the actual transformation may vary depending on the specific details of the solve directive
fn canonicalize_directive(ii: &mut IntermediateIntent) -> Result<(), CompileError> {
    let (solve_func, directive_span) = ii
        .directives
        .first()
        .ok_or_else(|| CompileError::MissingSolveDirective { span: empty_span() })?
        .clone();

    let directive_expr_key = match solve_func {
        Satisfy => return Ok(()), // we only need to transform a maximize or minimize directive
        Minimize(expr_key) | Maximize(expr_key) => expr_key,
    };

    let directive_expr_type = ii
        .expr_types
        .get(directive_expr_key)
        .ok_or_else(|| CompileError::Internal {
            msg: "invalid intermediate intent expression_types slotmap key",
            span: empty_span(),
        })?
        .clone();

    // create the new objective variable
    // let __objective: <type_of_expr>;
    let expr_type_clone = directive_expr_type.clone();
    let _ = ii.insert_var(
        "",
        None,
        &Ident {
            name: "__objective".to_string(),
            span: directive_span.clone(),
        },
        Some(directive_expr_type.clone()),
    )?;

    // update the directive expression to be the newly created objective variable
    // solve maximize __objective;
    let objective_expr_key = ii.exprs.insert(Expr::PathByName(
        "__objective".to_string(),
        directive_span.clone(),
    ));
    let _ = ii.expr_types.insert(objective_expr_key, expr_type_clone);

    let eq_expr_key = ii.exprs.insert(Expr::BinaryOp {
        op: expr::BinaryOp::Equal,
        lhs: objective_expr_key,
        rhs: directive_expr_key,
        span: directive_span.clone(),
    });
    ii.expr_types.insert(
        eq_expr_key,
        Type::Primitive {
            kind: PrimitiveKind::Bool,
            span: directive_span.clone(),
        },
    );
    ii.constraints.push((eq_expr_key, directive_span.clone()));

    let canonicalized_solve_func = match solve_func {
        Satisfy => return Ok(()),
        Minimize(_) => SolveFunc::Minimize(objective_expr_key),
        Maximize(_) => SolveFunc::Maximize(objective_expr_key),
    };
    let canonicalized_directive = (canonicalized_solve_func, directive_span.clone());
    ii.directives[0] = canonicalized_directive;

    Ok(())
}
