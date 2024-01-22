use crate::{
    error::CompileError,
    expr::Immediate,
    intent::{
        intermediate::{Expr, ExprKey, Var, VarKey},
        IntermediateIntent,
    },
    span::{empty_span, Span, Spanned},
    types::{Path, PrimitiveKind, Type},
};
use std::collections::HashMap;

/// Scalarize an array by converting it into `n` decision variables where `n` is the total size of
/// the array (taking into account multi-dimensional arrays0. These new variables represent the
/// individual elements of the array. The names of the individual elements are chosen to be
/// `<array-name>[<index>]..[<index>]`.
///
/// For example, this 2D array:
///
/// ```yurt
/// let a: int[3][2];
/// ```
///
/// becomes
///
/// ```yurt
/// let a[0][0]: int;
/// let a[1][0]: int;
/// let a[2][0]: int;
/// let a[0][1]: int;
/// let a[1][1]: int;
/// let a[2][1]: int;
/// ```
///
/// The above is not valid Yurt, of course, because the square brackets are not allowed in
/// identifiers, but internally, this is fine and helps make the loopkup quite easy.
fn scalarize_array(
    ii: &mut IntermediateIntent,
    key: VarKey,
    name: &String,
    ty: &Type,
    range: ExprKey,
    span: &Span,
) -> Result<(), CompileError> {
    match ty {
        Type::Array { .. }
        | Type::Primitive {
            kind: PrimitiveKind::Int | PrimitiveKind::Real | PrimitiveKind::Bool,
            ..
        } => {
            let range = ii.exprs.get(range).expect("expr key guaranteed to exist");
            match range.evaluate(ii, &HashMap::new()) {
                Ok(Immediate::Int(val)) if val > 0 => {
                    for i in 0..val {
                        let new_var = Var {
                            name: format!("{name}[{i}]"),
                            ty: Some(ty.clone()),
                            span: span.clone(),
                        };
                        let new_var_key = ii.vars.insert(new_var.clone());

                        // Recurse for arrays of arrays
                        if let Type::Array {
                            ty: inner_ty,
                            range: inner_range,
                            ..
                        } = ty
                        {
                            scalarize_array(
                                ii,
                                new_var_key,
                                &new_var.name,
                                inner_ty,
                                *inner_range,
                                span,
                            )?;
                        }
                    }
                    ii.vars.remove(key);
                    Ok(())
                }
                Ok(_) => Err(CompileError::InvalidConstArrayLength {
                    span: range.span().clone(),
                }),
                _ => Err(CompileError::NonConstArrayLength {
                    span: range.span().clone(),
                }),
            }
        }
        _ => {
            // Eventually, this will go away. Hence why it's an internal error for the time being
            Err(CompileError::Internal {
                msg: "only arrays of ints, reals, and bools are currently supported",
                span: empty_span(),
            })
        }
    }
}

/// Scalarize an array access by converting it to a simple path expression that looks like
/// `<array-name>[<index>]..[<index>]`.
///
/// For example, this array element access:
///
/// ```yurt
/// constraint a[2][3] == 3; // here, `a[2][3]` is an `Expr::ArrayElementAccess { .. }`
/// ```
///
/// becomes
///
/// ```yurt
/// constraint a[2][3] == 3; // here, `a[2][3]` is an `Expr::PathByName( .. )`
/// ```
///
/// This matches the name of variable `a[2][3]` introduced when array `a` is scalaried in `fn
/// scalarize_array(..)`
fn scalarize_array_access(
    ii: &mut IntermediateIntent,
    key: ExprKey,
    array: ExprKey,
    index: ExprKey,
    span: &Span,
) -> Result<Path, CompileError> {
    let index = ii.exprs.get(index).expect("expr key guaranteed to exist");
    let index_value = index.evaluate(ii, &HashMap::new());
    let index_span = index.span().clone();
    let array = ii.exprs.get(array).expect("expr key guaranteed to exist");
    macro_rules! handle_array_access {
        ($path: expr) => {{
            // Try to evaluate the index using compile-time evaluation
            // Index must be a non-negative integer
            match &index_value {
                Ok(Immediate::Int(val)) if *val >= 0 => {
                    let path = format!("{}[{val}]", $path);
                    *ii.exprs
                        .get_mut(key)
                        .expect("key guaranteed to exist in the map!") =
                        Expr::PathByName(path.clone(), span.clone());
                    Ok(path)
                }
                Ok(_) => Err(CompileError::InvalidConstArrayIndex { span: index_span }),
                _ => Err(CompileError::NonConstArrayIndex { span: index_span }),
            }
        }};
    }

    match &array {
        Expr::PathByName(path, _) => {
            handle_array_access!(path)
        }
        Expr::PathByKey(path_key, _) => {
            handle_array_access!(&ii.vars[*path_key].name)
        }
        Expr::ArrayElementAccess {
            array: array_inner,
            index: index_inner,
            ..
        } => {
            handle_array_access!(scalarize_array_access(
                ii,
                key,
                *array_inner,
                *index_inner,
                span
            )?)
        }
        // Now this does not catch paths that do not represent arrays just yet. That is,
        // you could still index into a variable of type `int` or even a type name. Once we
        // have a type checker and expressions hold types, then we can improve this check.
        _ => Err(CompileError::CannotIndexIntoValue {
            span: array.span().clone(),
            index_span: index.span().clone(),
        }),
    }
}

/// Scalarize arrays by converting each array of size `n` into `n` new decision variable that
/// represent the individual elements of the array. The names of the individual elements are chosen
/// to be `<array-name>[<index>]..[<index>]`.
///
/// For example, consider the following:
///
/// ```yurt
/// let a: int[3];
///
/// constraint a[2] == 3;
/// ```
///
/// this becomes
///
/// ```yurt
/// let a[0]: int;
/// let a[1]: int;
/// let a[2]: int;
///
/// constraint a[2]: int; // here, `a[2]` is an `Expr::PathByName( .. )`
/// ```
///
/// The above is not valid Yurt, of course, because the square brackets are not allowed in
/// identifiers, but internally, this is fine and helps make the loopkup quite easy.
pub(crate) fn scalarize(ii: &mut IntermediateIntent) -> Result<(), CompileError> {
    // First, convert decision variables that are arrays into `n` new decision variables that
    // represent the individual elements of the array, where `n` is the length of the array
    ii.vars
        .iter()
        .filter_map(|(key, var)| {
            // Only collect arrays
            if let Some(Type::Array { ty, range, span }) = &var.ty {
                Some((key, var.name.clone(), ty.clone(), *range, span.clone()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .iter()
        .try_for_each(|(key, name, ty, range, span)| {
            scalarize_array(ii, *key, name, ty, *range, span)
        })?;

    // Next, change each array element access into its scalarized variable
    ii.exprs
        .iter()
        .filter_map(|(key, expr)| {
            // Only collect array element accesses
            if let Expr::ArrayElementAccess { array, index, span } = expr {
                Some((key, *array, *index, span.clone()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .iter()
        .try_for_each(|(key, array, index, span)| {
            scalarize_array_access(ii, *key, *array, *index, span)?;
            //dbg!(ii.exprs.get(*key).unwrap());
            Ok(())
        })
}
