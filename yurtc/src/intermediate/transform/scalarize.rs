use crate::{
    error::CompileError,
    expr::{BinaryOp, Immediate, TupleAccess},
    intermediate::{Expr, ExprKey, IntermediateIntent, Var, VarKey},
    span::{empty_span, Span, Spanned},
    types::{PrimitiveKind, Type},
};
use std::collections::{BTreeMap, HashMap};

macro_rules! iterate {
    ($continue_expr: expr, $msg: literal, $modified: ident) => {
        for loop_check in 0.. {
            if !$continue_expr {
                break;
            }

            $modified = true;

            if loop_check > 10_000 {
                return Err(CompileError::Internal {
                    msg: concat!("infinite loop in ", $msg),
                    span: empty_span(),
                });
            }
        }
    };

    ($continue_expr: expr, $msg: literal) => {
        let mut _modified = false;
        iterate!($continue_expr, $msg, _modified);
    };
}

pub(crate) fn scalarize(ii: &mut IntermediateIntent) -> Result<(), CompileError> {
    // Before we start, make sure all the array types have their sizes determined.
    fix_array_sizes(ii)?;

    iterate!(
        {
            let mut modified = false;
            modified |= scalarize_arrays(ii)?;
            modified |= scalarize_tuples(ii)?;
            modified
        },
        "scalarize()"
    );

    Ok(())
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
/// constraint a[2] == 3; // here, `a[2]` is an `Expr::PathByName( .. )`
/// ```
///
/// The above is not valid Yurt, of course, because the square brackets are not allowed in
/// identifiers, but internally, this is fine and helps make the lookup quite easy.
fn scalarize_arrays(ii: &mut IntermediateIntent) -> Result<bool, CompileError> {
    let mut modified = false;

    // Convert all comparisons (via `==` or `!=`) of arrays to element-by-element comparisons.
    iterate!(
        lower_array_compares(ii)?,
        "lower_array_compares()",
        modified
    );

    // Scalarize arrays one at a time.
    iterate!(scalarize_array(ii)?, "scalarize_array()", modified);

    Ok(modified)
}

fn fix_array_sizes(ii: &mut IntermediateIntent) -> Result<(), CompileError> {
    // Given a variable with an array type of unknown size and a range expression, determine the
    // array size and return a new array type.
    fn fix_array_size(
        ii: &mut IntermediateIntent,
        mut el_ty: Type,
        range_expr_key: ExprKey,
        array_ty_span: Span,
    ) -> Result<Type, CompileError> {
        if !(el_ty.is_array() || el_ty.is_int() || el_ty.is_real() || el_ty.is_bool()) {
            // Eventually, this will go away. Hence why it's an internal error for the time being.
            return Err(CompileError::Internal {
                msg: "only arrays of ints, reals, and bools are currently supported",
                span: empty_span(),
            });
        }

        // We have a nested array.  We need to fix its size first (if necessary) so that we can use
        // the new element type in the parent array.
        if el_ty.is_array() {
            let Some((inner_el_ty, inner_range_key, inner_size, inner_span)) =
                get_array_params(&el_ty)
            else {
                return Err(CompileError::Internal {
                    msg: "failed to get params for type we know is an array?",
                    span: el_ty.span().clone(),
                });
            };

            if inner_size.is_none() {
                el_ty = fix_array_size(
                    ii,
                    inner_el_ty.clone(),
                    *inner_range_key,
                    inner_span.clone(),
                )?;
            }
        }

        let range_expr = ii
            .exprs
            .get(range_expr_key)
            .expect("expr key guaranteed to exist");

        if let Expr::PathByName(path, _) = range_expr {
            // It's hopefully an enum for the range expression.
            if let Some(val) = ii.enums.iter().find_map(|enum_decl| {
                (&enum_decl.name.name == path).then_some(enum_decl.variants.len() as i64)
            }) {
                Ok(Type::Array {
                    ty: Box::new(el_ty),
                    range: range_expr_key,
                    size: Some(val),
                    span: array_ty_span,
                })
            } else {
                Err(CompileError::NonConstArrayLength {
                    span: range_expr.span().clone(),
                })
            }
        } else {
            match range_expr.evaluate(ii, &HashMap::new()) {
                Ok(Immediate::Int(val)) if val > 0 => Ok(Type::Array {
                    ty: Box::new(el_ty),
                    range: range_expr_key,
                    size: Some(val),
                    span: array_ty_span,
                }),
                Ok(_) => Err(CompileError::InvalidConstArrayLength {
                    span: range_expr.span().clone(),
                }),
                _ => Err(CompileError::NonConstArrayLength {
                    span: range_expr.span().clone(),
                }),
            }
        }
    }

    // Find all the vars or exprs (depending on how this macro is called) which are have array
    // types which are not yet fixed.  Save the var or expr key, the element type and the range
    // expression and then determine the size and save it back.

    macro_rules! update_types {
        ($iter: expr, $key_ty: ty, $types_map: expr) => {
            let candidates: Vec<($key_ty, Type, ExprKey, Span)> = $iter
                .filter_map(|(key, _)| {
                    $types_map.get(key).and_then(get_array_params).and_then(
                        |(el_ty, range, size, span)| {
                            // Only collect if size is None.
                            (size.is_none()).then(|| (key, el_ty.clone(), *range, span.clone()))
                        },
                    )
                })
                .collect();

            for (key, el_ty, range_expr_key, array_ty_span) in candidates {
                let fixed_ty = fix_array_size(ii, el_ty, range_expr_key, array_ty_span)?;
                if let Some(old_ty) = $types_map.get_mut(key) {
                    *old_ty = fixed_ty;
                }
            }
        };
    }

    update_types!(ii.vars.iter(), VarKey, ii.var_types);
    update_types!(ii.exprs.iter(), ExprKey, ii.expr_types);

    Ok(())
}

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
/// identifiers, but internally, this is fine and helps make the lookup quite easy.
fn scalarize_array(ii: &mut IntermediateIntent) -> Result<bool, CompileError> {
    // Find the next array variable to convert.
    let Some((array_var_key, el_ty, array_size, span)) =
        ii.var_types.iter().find_map(|(var_key, var_ty)| {
            get_array_params(var_ty).map(|(el_ty, _range, array_size, span)| {
                (var_key, el_ty.clone(), *array_size, span.clone())
            })
        })
    else {
        // No array vars found.
        return Ok(false);
    };

    let array_size = array_size.ok_or_else(|| CompileError::Internal {
        msg: "non-fixed array size found in scalarize_array()",
        span: span.clone(),
    })?;

    let array_name = ii
        .vars
        .get(array_var_key)
        .map(|var| var.name.clone())
        .ok_or_else(|| CompileError::Internal {
            msg: "missing name for array variable in scalarize_array()",
            span: span.clone(),
        })?;

    // Convert decision variables that are arrays into `n` new decision variables that represent
    // the individual elements of the array, where `n` is the length of the array.
    let new_var_keys = (0..array_size)
        .map(|idx| {
            let new_var = Var {
                name: format!("{array_name}[{idx}]"),
                span: span.clone(),
            };
            let new_var_key = ii.vars.insert(new_var);
            ii.var_types.insert(new_var_key, el_ty.clone());
            new_var_key
        })
        .collect::<Vec<_>>();

    // Change each array element access into its scalarized variable.
    scalarize_array_access(
        ii,
        &array_name,
        array_var_key,
        array_size,
        el_ty,
        &new_var_keys,
    )?;

    // Remove the old array variable.
    ii.vars.remove(array_var_key);
    ii.var_types.remove(array_var_key);

    Ok(true)
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
    array_var_name: &String,
    array_var_key: VarKey,
    array_size: i64,
    el_ty: Type,
    new_array_var_keys: &[VarKey],
) -> Result<(), CompileError> {
    // Gather all accesses into this specific array.
    let accesses: Vec<(ExprKey, ExprKey, Span)> = ii
        .exprs
        .iter()
        .filter_map(|(expr_key, expr)| {
            if let Expr::ArrayElementAccess { array, index, span } = expr {
                match ii.exprs.get(*array).expect("expr key guaranteed to exist") {
                    Expr::PathByName(path, _) if path == array_var_name => {
                        Some((expr_key, *index, span.clone()))
                    }

                    Expr::PathByKey(path_var_key, _) if *path_var_key == array_var_key => {
                        Some((expr_key, *index, span.clone()))
                    }

                    _ => None,
                }
            } else {
                None
            }
        })
        .collect();

    for (array_access_key, index_key, span) in accesses {
        let index_expr = ii
            .exprs
            .get(index_key)
            .expect("expr key guaranteed to exist");
        let index_span = index_expr.span().clone();
        let index_value = index_expr.evaluate(ii, &HashMap::new()).map_err(|_| {
            CompileError::NonConstArrayIndex {
                span: index_span.clone(),
            }
        })?;

        // Index must be an integer in range.
        match index_value {
            Immediate::Int(imm_val) => {
                if imm_val < 0 || imm_val >= array_size {
                    return Err(CompileError::ArrayIndexOutOfBounds { span: index_span });
                }

                let new_access_key = ii.exprs.insert(Expr::PathByKey(
                    new_array_var_keys[imm_val as usize],
                    span.clone(),
                ));
                ii.expr_types.insert(new_access_key, el_ty.clone());

                ii.replace_exprs(array_access_key, new_access_key);
                ii.exprs.remove(array_access_key);
                ii.expr_types.remove(array_access_key);
            }

            _ => {
                return Err(CompileError::InvalidConstArrayIndex { span: index_span });
            }
        }
    }

    Ok(())
}

fn get_array_params(ary_ty: &Type) -> Option<(&Type, &ExprKey, &Option<i64>, &Span)> {
    match ary_ty {
        Type::Alias { ty, .. } => get_array_params(ty),
        Type::Array {
            ty,
            range,
            size,
            span,
        } => Some((ty, range, size, span)),
        _ => None,
    }
}

fn lower_array_compares(ii: &mut IntermediateIntent) -> Result<bool, CompileError> {
    // Find comparisons between arrays and save the op details.
    let array_compare_ops = ii
        .exprs
        .iter()
        .filter_map(|(key, expr)| match expr {
            Expr::BinaryOp { op, lhs, rhs, span }
                if (*op == BinaryOp::Equal || *op == BinaryOp::NotEqual) =>
            {
                ii.expr_types.get(*lhs).and_then(get_array_params).and_then(
                    |(lhs_el_ty, _, lhs_opt_size, _)| {
                        ii.expr_types.get(*rhs).and_then(get_array_params).map(
                            |(_rhs_el_ty, _, rhs_opt_size, _)| {
                                // Save all the details.
                                (
                                    key,
                                    *op,
                                    *lhs,
                                    *lhs_opt_size,
                                    *rhs,
                                    *rhs_opt_size,
                                    lhs_el_ty.clone(),
                                    span.clone(),
                                )
                            },
                        )
                    },
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    if array_compare_ops.is_empty() {
        return Ok(false);
    }

    let get_array_size = |array_ty: &Option<i64>| {
        array_ty.ok_or_else(|| CompileError::Internal {
            msg: "array type in missing its size in lower_array_compares()",
            span: empty_span(),
        })
    };

    for (op_expr_key, op, lhs_array_key, lhs_opt_size, rhs_array_key, rhs_opt_size, el_ty, span) in
        array_compare_ops
    {
        let lhs_size = get_array_size(&lhs_opt_size)?;
        let rhs_size = get_array_size(&rhs_opt_size)?;

        if lhs_size != rhs_size {
            // This *should* be done by the type checker but we currently only evaluate the ranges
            // within those types as the first step in scalarisation, not at type check time.  This
            // may change in the future.
            return Err(CompileError::MismatchedArrayComparisonSizes {
                op: op.to_string(),
                lhs_size,
                rhs_size,
                span,
            });
        }

        // Pair up each element with an individual `op` operation and then chain them together
        // with a series of `&&` operations.  Twice we collect into a temporary Vec to avoid
        // borrowing problems with `ii.exprs`.
        let and_chain_expr_key = (0..lhs_size)
            .map(|idx| {
                let imm_idx_key = ii.exprs.insert(Expr::Immediate {
                    value: Immediate::Int(idx),
                    span: empty_span(),
                });

                ii.expr_types.insert(
                    imm_idx_key,
                    Type::Primitive {
                        kind: PrimitiveKind::Int,
                        span: span.clone(),
                    },
                );

                let lhs_access_expr_key = ii.exprs.insert(Expr::ArrayElementAccess {
                    array: lhs_array_key,
                    index: imm_idx_key,
                    span: span.clone(),
                });

                let rhs_access_expr_key = ii.exprs.insert(Expr::ArrayElementAccess {
                    array: rhs_array_key,
                    index: imm_idx_key,
                    span: span.clone(),
                });

                ii.expr_types.insert(lhs_access_expr_key, el_ty.clone());
                ii.expr_types.insert(rhs_access_expr_key, el_ty.clone());

                ii.exprs.insert(Expr::BinaryOp {
                    op,
                    lhs: lhs_access_expr_key,
                    rhs: rhs_access_expr_key,
                    span: span.clone(),
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .reduce(|acc, cmp_op_key| {
                let and_op_key = ii.exprs.insert(Expr::BinaryOp {
                    op: BinaryOp::LogicalAnd,
                    lhs: acc,
                    rhs: cmp_op_key,
                    span: span.clone(),
                });

                ii.expr_types.insert(
                    and_op_key,
                    Type::Primitive {
                        kind: PrimitiveKind::Bool,
                        span: span.clone(),
                    },
                );

                and_op_key
            })
            .expect("there must be 1 or more array elements");

        ii.expr_types.insert(
            and_chain_expr_key,
            Type::Primitive {
                kind: PrimitiveKind::Bool,
                span: span.clone(),
            },
        );

        ii.replace_exprs(op_expr_key, and_chain_expr_key);
        ii.exprs.remove(op_expr_key);
        ii.expr_types.remove(op_expr_key);
    }

    Ok(true)
}

/// Scalarize tuples by extracting the tuple fields out into their own decision variables.  The
/// new names are `<tuple-name>.<field-name>` if the fields are named, or `<tuple-name>.<index>` if
/// not.
///
/// E.g.,
///
/// ```yurt
/// let a: { x: int, real };
///
/// constraint a.x < 11;    // here `a.x` is a tuple-access expr.
/// ```
///
/// becomes
///
/// ```yurt
/// let a.x: int;
/// let a.1: real;
///
/// constraint a.x < 11;    // `a.x` is now a path expr, an illegal identifier usually.
///
/// ```
fn scalarize_tuples(ii: &mut IntermediateIntent) -> Result<bool, CompileError> {
    // First we need to lower any aggregate comparisons.  It is valid to use `==` or `!=` to
    // compare whole tuples, but we must split these comparisons into field-by-field ops before we
    // scalarise.

    let mut modified = false;

    // Do it in a loop so we can lower nested tuples.  I.e., we might create a new field-by-field
    // comparison between inner tuples and so they need to be lowered too.
    iterate!(
        lower_tuple_compares(ii)?,
        "lower_tuple_compares()",
        modified
    );

    // Split all tuple vars into their fields.  We accumulate a set of var_keys which need to be
    // removed once we're done.
    let mut old_tuple_vars = Vec::new();
    iterate!(
        split_tuple_vars(ii, &mut old_tuple_vars)?,
        "split_tuple_vars()",
        modified
    );
    for var_key in old_tuple_vars {
        ii.vars.remove(var_key);
        ii.var_types.remove(var_key);
    }

    Ok(modified)
}

fn lower_tuple_compares(ii: &mut IntermediateIntent) -> Result<bool, CompileError> {
    // Gather all the valid binary op exprs.
    let mut tuple_compare_ops = Vec::new();
    for (expr_key, expr) in &ii.exprs {
        if let Expr::BinaryOp { op, lhs, rhs, span } = expr {
            if (*op == BinaryOp::Equal || *op == BinaryOp::NotEqual)
                && ii
                    .expr_types
                    .get(*lhs)
                    .map(|lhs_ty| lhs_ty.is_tuple())
                    .unwrap_or(false)
            {
                // Type checking should ensure RHS is also a tuple.
                assert!(ii.expr_types.get(*rhs).unwrap().is_tuple());

                tuple_compare_ops.push((expr_key, *op, *lhs, *rhs, span.clone()));
            }
        }
    }

    let modified = !tuple_compare_ops.is_empty();

    for (expr_key, op, lhs_tuple_key, rhs_tuple_key, span) in tuple_compare_ops {
        // Get the tuple field names and types for both sides, as they might be the same type but
        // have different accessors.  We *could* just drop the names and always use indices but
        // this means our lowered identifiers will be less descriptive.

        let Some(lhs_fields) = ii
            .expr_types
            .get(lhs_tuple_key)
            .expect("failed to get tuple type in lower_tuple_compares()")
            .get_tuple_fields()
            .map(|fs| fs.to_vec())
        else {
            unreachable!("failed to get lhs tuple field types in lower_tuple_compares()");
        };
        let Some(rhs_fields) = ii
            .expr_types
            .get(rhs_tuple_key)
            .expect("failed to get tuple type in lower_tuple_compares()")
            .get_tuple_fields()
            .map(|fs| fs.to_vec())
        else {
            unreachable!("failed to get rhs tuple field types in lower_tuple_compares()");
        };

        // Replace the op with a chain of field-by-field comparisons.
        let mut new_field_compare_ops = Vec::new();

        // This can be a bit tricky since tuples with named fields can declare literals with
        // the fields in any order.  The rules say that two tuples-with-named-fields types are
        // only equivalent if all the fields are named.  This is managed by Type::eq() and type
        // checking has already checked this for us.
        //
        // So if ALL the fields for both tuples are named then we can use the names as
        // accessors, otherwise we will use indices.
        let use_named_accessors = lhs_fields.iter().all(|(name, _)| name.is_some())
            && rhs_fields.iter().all(|(name, _)| name.is_some());

        for (field_idx, (opt_field_name, field_ty)) in lhs_fields.iter().enumerate() {
            // For the LHS get the access by index, but use the name if its there.
            let lhs_field_access = opt_field_name
                .as_ref()
                .map(|field_name| TupleAccess::Name(field_name.clone()))
                .unwrap_or_else(|| TupleAccess::Index(field_idx));

            let rhs_field_access = if use_named_accessors {
                // We use the same named accessor as LHS.
                lhs_field_access.clone()
            } else {
                // Use the field at field_idx, but still by name if it has one.
                rhs_fields[field_idx]
                    .0
                    .as_ref()
                    .map(|field_name| TupleAccess::Name(field_name.clone()))
                    .unwrap_or_else(|| TupleAccess::Index(field_idx))
            };

            let lhs_access = ii.exprs.insert(Expr::TupleFieldAccess {
                tuple: lhs_tuple_key,
                field: lhs_field_access,
                span: span.clone(),
            });

            let rhs_access = ii.exprs.insert(Expr::TupleFieldAccess {
                tuple: rhs_tuple_key,
                field: rhs_field_access,
                span: span.clone(),
            });

            ii.expr_types.insert(lhs_access, field_ty.clone());
            ii.expr_types.insert(rhs_access, field_ty.clone());

            let field_compare_op = ii.exprs.insert(Expr::BinaryOp {
                op,
                lhs: lhs_access,
                rhs: rhs_access,
                span: span.clone(),
            });
            ii.expr_types.insert(
                field_compare_op,
                Type::Primitive {
                    kind: PrimitiveKind::Bool,
                    span: span.clone(),
                },
            );

            new_field_compare_ops.push(field_compare_op);
        }

        let and_chain_expr_key = new_field_compare_ops
            .into_iter()
            .reduce(|acc, compare_op_key| {
                let and_op_key = ii.exprs.insert(Expr::BinaryOp {
                    op: BinaryOp::LogicalAnd,
                    lhs: acc,
                    rhs: compare_op_key,
                    span: span.clone(),
                });

                ii.expr_types.insert(
                    and_op_key,
                    Type::Primitive {
                        kind: PrimitiveKind::Bool,
                        span: span.clone(),
                    },
                );

                and_op_key
            })
            .expect("there must be 1 or more tuple fields");

        ii.replace_exprs(expr_key, and_chain_expr_key);
        ii.exprs.remove(expr_key);
        ii.expr_types.remove(expr_key);
    }

    Ok(modified)
}

fn split_tuple_vars(
    ii: &mut IntermediateIntent,
    old_tuple_vars: &mut Vec<VarKey>,
) -> Result<bool, CompileError> {
    let mut new_vars = Vec::new();

    // Iterate for all the tuple vars and gather their fields into `new_vars`.
    for (var_key, Var { name, span }) in ii.vars.iter() {
        if old_tuple_vars.contains(&var_key) {
            // Already split; skip it.
            continue;
        }

        let Some(var_ty) = ii.var_types.get(var_key) else {
            return Err(CompileError::Internal {
                msg: "missing var type in split_tuple_vars",
                span: span.clone(),
            });
        };

        if let Some(fields) = var_ty.get_tuple_fields() {
            // We now know we have a tuple var and its field types.
            for (field_idx, (opt_field_name, field_ty)) in fields.iter().enumerate() {
                // Always save the numeric index name, and optionally the symbolic name.
                let new_idx_name = format!("{name}.{field_idx}");
                let new_sym_name = opt_field_name
                    .as_ref()
                    .map(|field_name| format!("{name}.{field_name}"));
                new_vars.push(((new_idx_name, new_sym_name), span.clone(), field_ty.clone()));
            }

            old_tuple_vars.push(var_key);
        }
    }

    if new_vars.is_empty() {
        return Ok(false);
    }

    let mut new_tuple_vars = BTreeMap::new();

    // Add all the new vars to the intermediate intent and memo the new key.
    for ((idx_name, opt_sym_name), span, field_ty) in new_vars {
        // Prefer the symbolic name if it's there.
        let new_var_key = ii.vars.insert(Var {
            name: opt_sym_name.as_ref().unwrap_or(&idx_name).clone(),
            span,
        });
        ii.var_types.insert(new_var_key, field_ty.clone());

        // Add both names to this map so we cover both potential uses below.
        if let Some(sym_name) = opt_sym_name {
            new_tuple_vars.insert(sym_name, (new_var_key, field_ty.clone()));
        }
        new_tuple_vars.insert(idx_name, (new_var_key, field_ty));
    }

    let mut new_accesses = Vec::new();

    // Iterate for each tuple access which is into a var and get the new var key.
    for (expr_key, expr) in ii.exprs.iter() {
        if let Expr::TupleFieldAccess { tuple, field, span } = expr {
            let mut push_new_access = |tuple_name: String| {
                // Work out the access name after the dot.
                let field_name = match field {
                    TupleAccess::Index(field_idx) => format!("{field_idx}"),
                    TupleAccess::Name(name) => name.name.clone(),
                    _ => unreachable!("tuple access error in split_tuple_vars()"),
                };

                let access_name = tuple_name + "." + &field_name;

                if let Some((new_tuple_var_key, new_tuple_var_ty)) =
                    new_tuple_vars.get(&access_name)
                {
                    // We've matched this tuple field access with one of the new split vars we
                    // created above.  Mark this expr to be replaced.
                    new_accesses.push((
                        expr_key,
                        *new_tuple_var_key,
                        new_tuple_var_ty,
                        span.clone(),
                    ));
                }
            };

            match ii.exprs.get(*tuple) {
                Some(Expr::PathByKey(var_key, _)) => push_new_access(
                    ii.vars
                        .get(*var_key)
                        .expect("missing var in split_tuple_vars()")
                        .name
                        .clone(),
                ),
                Some(Expr::PathByName(path, _)) => push_new_access(path.clone()),
                _ => {}
            }
        }
    }

    // Replace all the old tuple accesses with new PathByKey exprs.
    for (old_expr_key, new_var_key, new_tuple_var_ty, span) in new_accesses {
        let new_expr_key = ii.exprs.insert(Expr::PathByKey(new_var_key, span));
        ii.expr_types.insert(new_expr_key, new_tuple_var_ty.clone());

        ii.replace_exprs(old_expr_key, new_expr_key);
        ii.exprs.remove(old_expr_key);
        ii.expr_types.remove(old_expr_key);
    }

    Ok(true)
}
