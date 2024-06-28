use crate::{
    error::{CompileError, Error, ErrorEmitted, Handler},
    expr::{BinaryOp, Expr, Immediate, TupleAccess, UnaryOp},
    predicate::{
        ConstraintDecl, ExprKey, Predicate, PredicateInstance, Program, ProgramKind,
        State as StateVar,
    },
    span::empty_span,
    types::Type,
};
use essential_types::predicate::{Directive, Predicate as CompiledPredicate};
use state_asm::{
    Access, Alu, Constraint, Crypto, Op as StateRead, Pred, Stack, StateSlots, TotalControlFlow,
};

mod display;
#[cfg(test)]
mod tests;

#[derive(Debug, Default, Clone)]
pub struct CompiledProgram {
    pub kind: ProgramKind,
    pub names: Vec<String>,
    pub salt: [u8; 32],
    pub predicates: Vec<CompiledPredicate>,
}

impl CompiledProgram {
    pub const ROOT_PRED_NAME: &'static str = "";

    /// The root predicate is the one named `CompiledProgram::ROOT_PRED_NAME`
    pub fn root_predicate(&self) -> &CompiledPredicate {
        &self.predicates[self
            .names
            .iter()
            .position(|name| name == Self::ROOT_PRED_NAME)
            .unwrap()]
    }
}

/// Convert a `Program` into `CompiledProgram`
pub fn compile_program(
    handler: &Handler,
    program: &Program,
) -> Result<CompiledProgram, ErrorEmitted> {
    let mut names = Vec::new();
    let mut predicates = Vec::new();
    match program.kind {
        ProgramKind::Stateless => {
            let (name, pred) = program.preds.iter().next().unwrap();
            if let Ok(predicate) = handler.scope(|handler| predicate_to_asm(handler, pred)) {
                names.push(name.to_string());
                predicates.push(predicate);
            }
        }
        ProgramKind::Stateful => {
            for (name, pred) in program.preds.iter() {
                if name != Program::ROOT_PRED_NAME {
                    if let Ok(predicate) = handler.scope(|handler| predicate_to_asm(handler, pred))
                    {
                        names.push(name.to_string());
                        predicates.push(predicate);
                    }
                }
            }
        }
    }

    if handler.has_errors() {
        return Err(handler.cancel());
    }

    Ok(CompiledProgram {
        kind: program.kind.clone(),
        names,
        // Salt is not used by pint yet.
        salt: Default::default(),
        predicates,
    })
}

#[derive(Default)]
pub struct AsmBuilder {
    // Opcodes to read state.
    s_asm: Vec<Vec<StateRead>>,
    // Opcodes to specify constraints
    c_asm: Vec<Vec<Constraint>>,
}

#[derive(Debug)]
enum StorageKeyKind {
    Static(Vec<i64>),
    Dynamic(usize),
}

#[derive(Debug)]
struct StorageKey {
    kind: StorageKeyKind,
    is_extern: bool,
}

impl StorageKey {
    fn len(&self) -> usize {
        match &self.kind {
            StorageKeyKind::Static(key) => key.len(),
            StorageKeyKind::Dynamic(len) => *len,
        }
    }
}

/// Location of an expression:
/// 1. `Value` expressions are just raw values such as immediates or the outputs of binary ops.
/// 2. `DecisionVar` expressions refer to expressions that require the `DecisionVar` or
///    `DecisionVarRange` opcodes. The `usize` here is the index of the decision variable. We will
///    need to update this
///    variant to also include an offset once we stop scalarizing.
/// 3. `Transient` expressions refer to expressions that require the `Transient` opcode. The
///    `Option<String>` is the optional name of the pathway variable. If `None`, just use
///    `ThisPathway`. The `usize` is the transient key length.
/// 4. `State` expressions refer to expressions that require the `State` or `StateRange` opcodes.
///    The `bool` is the "delta": are we referring to the current or the next state?
enum Location {
    Value,
    DecisionVar(usize),
    Transient(Option<String>, usize),
    State(bool),
}

impl AsmBuilder {
    /// Given an `expr`, compile and calculate its `Location`. Only a "pointer" is produced or
    /// nothing if the expression is a raw value.
    fn compile_expr_pointer(
        handler: &Handler,
        asm: &mut Vec<Constraint>,
        expr: &ExprKey,
        pred: &Predicate,
    ) -> Result<Location, ErrorEmitted> {
        match &expr.get(pred) {
            // All of these are just values
            Expr::Error(_)
            | Expr::Immediate { .. }
            | Expr::Array { .. }
            | Expr::Tuple { .. }
            | Expr::BinaryOp { .. }
            | Expr::MacroCall { .. }
            | Expr::IntrinsicCall { .. }
            | Expr::Select { .. }
            | Expr::Cast { .. }
            | Expr::In { .. }
            | Expr::Range { .. }
            | Expr::Generator { .. }
            | Expr::StorageAccess { .. }
            | Expr::ExternalStorageAccess { .. }
            | Expr::Index { .. } => Ok(Location::Value),
            Expr::UnaryOp { op, expr, .. } => match op {
                UnaryOp::NextState => {
                    // Next state expressions produce state expressions (i.e. ones that require
                    // `State` or `StateRange`
                    Self::compile_expr_pointer(handler, asm, expr, pred)?;
                    Ok(Location::State(true))
                }
                _ => Ok(Location::Value),
            },
            Expr::PathByName(path, _) => Self::compile_path(handler, asm, path, pred),
            Expr::PathByKey(var_key, _) => {
                let path = &var_key.get(pred).name;
                Self::compile_path(handler, asm, path, pred)
            }
            Expr::TupleFieldAccess { tuple, field, .. } => {
                let location = Self::compile_expr_pointer(handler, asm, tuple, pred)?;
                match location {
                    Location::State(_) | Location::Transient(..) => {
                        // Offset calculation for state and transient data is the same
                        // Grab the fields of the tuple
                        let Type::Tuple { ref fields, .. } = tuple.get_ty(pred) else {
                            return Err(handler.emit_err(Error::Compile {
                                error: CompileError::Internal {
                                    msg: "type must exist and be a tuple type",
                                    span: empty_span(),
                                },
                            }));
                        };

                        // The field index is based on the type definition
                        let field_idx = match field {
                            TupleAccess::Index(idx) => *idx,
                            TupleAccess::Name(ident) => fields
                                .iter()
                                .position(|(field_name, _)| {
                                    field_name
                                        .as_ref()
                                        .map_or(false, |name| name.name == ident.name)
                                })
                                .expect("field name must exist, this was checked in type checking"),
                            TupleAccess::Error => {
                                return Err(handler.emit_err(Error::Compile {
                                    error: CompileError::Internal {
                                        msg: "unexpected TupleAccess::Error",
                                        span: empty_span(),
                                    },
                                }));
                            }
                        };

                        // This is the offset from the base key where the full tuple is stored.
                        let key_offset: usize = fields
                            .iter()
                            .take(field_idx)
                            .map(|(_, ty)| ty.storage_slots())
                            .sum();

                        // Now offset using `Add`
                        asm.push(Stack::Push(key_offset as i64).into());
                        asm.push(Alu::Add.into());
                        Ok(location)
                    }
                    Location::DecisionVar(_) => {
                        unimplemented!("we'll handle this when we stop scalarizing")
                    }
                    Location::Value => {
                        unimplemented!("we'll handle this eventually as a fallback option")
                    }
                }
            }
        }
    }

    /// Generates assembly for producing a storage key  where `expr` is stored.
    /// Returns a `StorageKey`. The `StorageKey` contains two values:
    /// - The kind of the storage key: static where the keys are known at compile-time or dynamic.
    /// - Whether the key is internal or external. External keys should be accessed using
    /// `StateReadKeyRangeExtern`.
    fn compile_state_key(
        handler: &Handler,
        s_asm: &mut Vec<StateRead>,
        expr: &ExprKey,
        pred: &Predicate,
    ) -> Result<StorageKey, ErrorEmitted> {
        let expr_ty = expr.get_ty(pred);
        match &expr.get(pred) {
            Expr::IntrinsicCall { name, args, .. } => {
                if name.name.ends_with("__storage_get") {
                    // Expecting a single argument that is an array of integers representing a key
                    assert_eq!(args.len(), 1);
                    let Some(key_size) = args[0].get_ty(pred).get_array_size() else {
                        return Err(handler.emit_err(Error::Compile {
                            error: CompileError::Internal {
                                msg: "unable to get key size",
                                span: empty_span(),
                            },
                        }));
                    };

                    let mut asm = Vec::new();
                    Self::compile_expr(handler, &mut asm, &args[0], pred)?;
                    s_asm.extend(asm.iter().map(|op| StateRead::Constraint(*op)));
                    Ok(StorageKey {
                        kind: StorageKeyKind::Dynamic(key_size as usize),
                        is_extern: false,
                    })
                } else if name.name.ends_with("__storage_get_extern") {
                    // Expecting two arguments:
                    // 1. An address that is a `b256`
                    // 2. A key: an array of integers
                    assert_eq!(args.len(), 2);
                    let Some(key_size) = args[1].get_ty(pred).get_array_size() else {
                        return Err(handler.emit_err(Error::Compile {
                            error: CompileError::Internal {
                                msg: "unable to get key size",
                                span: empty_span(),
                            },
                        }));
                    };

                    // First, get the contract address and the storage key
                    let mut asm = Vec::new();
                    Self::compile_expr(handler, &mut asm, &args[0], pred)?;
                    Self::compile_expr(handler, &mut asm, &args[1], pred)?;
                    s_asm.extend(asm.iter().map(|op| StateRead::Constraint(*op)));
                    Ok(StorageKey {
                        kind: StorageKeyKind::Dynamic(key_size as usize),
                        is_extern: true,
                    })
                } else {
                    unimplemented!("Other calls are currently not supported")
                }
            }
            Expr::StorageAccess(name, _) => {
                let storage = &pred
                    .storage
                    .as_ref()
                    .expect("a storage block must have been declared")
                    .0;

                // Get the index of the storage variable in the storage block declaration
                let storage_index = storage
                    .iter()
                    .position(|var| var.name.name == *name)
                    .expect("storage access should have been checked before");

                // This is the key. It's either the `storage_index` if the storage type primitive
                // or a map, or it's `[storage_index, 0]`. The `0` here is a placeholder for
                // offsets.
                let storage_var = &storage[storage_index];
                let key = if storage_var.ty.is_any_primitive() || storage_var.ty.is_map() {
                    s_asm.push(Stack::Push(storage_index as i64).into());
                    vec![storage_index as i64]
                } else {
                    s_asm.push(Stack::Push(storage_index as i64).into());
                    s_asm.push(Stack::Push(0).into());
                    vec![storage_index as i64, 0]
                };

                Ok(StorageKey {
                    kind: StorageKeyKind::Static(key),
                    is_extern: false,
                })
            }
            Expr::ExternalStorageAccess {
                interface_instance,
                name,
                ..
            } => {
                // Get the `interface_instance` declaration that the storage access refers to
                let interface_instance = &pred
                    .interface_instances
                    .iter()
                    .find(|e| e.name.to_string() == *interface_instance)
                    .expect("missing interface instance");

                // Compile the interface instance address
                let mut asm = Vec::new();
                Self::compile_expr(handler, &mut asm, &interface_instance.address, pred)?;
                s_asm.extend(asm.iter().map(|op| StateRead::Constraint(*op)));

                // Get the `interface` declaration that the storage access refers to
                let interface = &pred
                    .interfaces
                    .iter()
                    .find(|e| e.name.to_string() == *interface_instance.interface)
                    .expect("missing interface");

                // Get the index of the storage variable in the storage block declaration
                let storage = &interface
                    .storage
                    .as_ref()
                    .expect("a storage block must have been declared")
                    .0;

                let storage_index = storage
                    .iter()
                    .position(|var| var.name.name == *name)
                    .expect("storage access should have been checked before");

                // This is the key. It's either the `storage_index` if the storage type primitive
                // or a map, or it's `[storage_index, 0]`. The `0` here is a placeholder for
                // offsets.
                let storage_var = &storage[storage_index];
                let key = if storage_var.ty.is_any_primitive() || storage_var.ty.is_map() {
                    s_asm.push(Stack::Push(storage_index as i64).into());
                    vec![storage_index as i64]
                } else {
                    s_asm.push(Stack::Push(storage_index as i64).into());
                    s_asm.push(Stack::Push(0).into());
                    vec![storage_index as i64, 0]
                };

                // This is external!
                Ok(StorageKey {
                    kind: StorageKeyKind::Static(key),
                    is_extern: true,
                })
            }
            Expr::Index { expr, index, .. } => {
                // Compile the key corresponding to `expr`
                let storage_key = Self::compile_state_key(handler, s_asm, expr, pred)?;

                // Compile the index
                let mut asm = vec![];
                Self::compile_expr(handler, &mut asm, index, pred)?;
                s_asm.extend(asm.iter().copied().map(StateRead::from));
                let mut key_length = storage_key.len() + index.get_ty(pred).size();
                if !(expr_ty.is_any_primitive() || expr_ty.is_map()) {
                    s_asm.push(StateRead::from(Stack::Push(0)));
                    key_length += 1;
                }
                Ok(StorageKey {
                    kind: StorageKeyKind::Dynamic(key_length),
                    is_extern: storage_key.is_extern,
                })
            }
            Expr::TupleFieldAccess { tuple, field, .. } => {
                // Compile the key corresponding to `tuple`
                let StorageKey { kind, is_extern } =
                    Self::compile_state_key(handler, s_asm, tuple, pred)?;

                // Grab the fields of the tuple
                let Type::Tuple { ref fields, .. } = tuple.get_ty(pred) else {
                    return Err(handler.emit_err(Error::Compile {
                        error: CompileError::Internal {
                            msg: "type must exist and be a tuple type",
                            span: empty_span(),
                        },
                    }));
                };

                // The field index is based on the type definition
                let field_idx = match field {
                    TupleAccess::Index(idx) => *idx,
                    TupleAccess::Name(ident) => fields
                        .iter()
                        .position(|(field_name, _)| {
                            field_name
                                .as_ref()
                                .map_or(false, |name| name.name == ident.name)
                        })
                        .expect("field name must exist, this was checked in type checking"),
                    TupleAccess::Error => {
                        return Err(handler.emit_err(Error::Compile {
                            error: CompileError::Internal {
                                msg: "unexpected TupleAccess::Error",
                                span: empty_span(),
                            },
                        }));
                    }
                };

                // This is the offset from the base key where the full tuple is stored.
                let key_offset: usize = fields
                    .iter()
                    .take(field_idx)
                    .map(|(_, ty)| ty.storage_slots())
                    .sum();

                // Increment the last word on the stack by `key_offset`. This works fine for the
                // static case because all static keys start at zero (at least for now). For
                // dynamic keys, this is not accurate due to a potential overflow. I'm going to
                // keep this for now though so that we can keep things going, but we need a proper
                // solution (`Add4` opcode, or storage reads with offset, or even a whole new
                // storage design that uses b-trees.)
                Ok(StorageKey {
                    kind: match kind {
                        StorageKeyKind::Dynamic(size) => {
                            // Increment the last word on the stack by `key_offset`
                            s_asm.push(Stack::Push(key_offset as i64).into());
                            s_asm.push(Alu::Add.into());
                            StorageKeyKind::Dynamic(size)
                        }
                        StorageKeyKind::Static(mut key) => {
                            // Remove the last word on the stack and increment it by `key_offset`.
                            s_asm.push(Stack::Pop.into());
                            let len = key.len();
                            key[len - 1] += key_offset as i64;
                            s_asm.push(Stack::Push(key[len - 1]).into());
                            StorageKeyKind::Static(key)
                        }
                    },
                    is_extern,
                })
            }
            _ => unreachable!("there really shouldn't be anything else at this stage"),
        }
    }

    /// Generates assembly for an `ExprKey`. Returns the number of opcodes used to express `expr`
    fn compile_expr(
        handler: &Handler,
        asm: &mut Vec<Constraint>,
        expr: &ExprKey,
        pred: &Predicate,
    ) -> Result<usize, ErrorEmitted> {
        let old_asm_len = asm.len();
        let expr_ty = expr.get_ty(pred);
        match Self::compile_expr_pointer(handler, asm, expr, pred)? {
            Location::Value => {
                Self::compile_value_expr(handler, asm, expr, pred)?;
            }
            Location::DecisionVar(len) => {
                if len == 1 {
                    asm.push(Access::DecisionVar.into());
                } else {
                    asm.push(Stack::Push(0).into()); // index
                    asm.push(Stack::Push(len as i64).into()); // len
                    asm.push(Access::DecisionVarRange.into());
                }
            }
            Location::Transient(pathway, len) => {
                if let Some(pathway_var_name) = pathway {
                    let var_index = pred
                        .vars()
                        .filter(|(_, var)| !var.is_pub)
                        .position(|(_, var)| var.name == pathway_var_name);
                    asm.push(Stack::Push(len as i64).into()); // key length
                    asm.push(Stack::Push(var_index.unwrap() as i64).into()); // slot
                    asm.push(Access::DecisionVar.into());
                    asm.push(Constraint::Access(Access::Transient));
                } else {
                    asm.push(Stack::Push(len as i64).into()); // key length
                    asm.push(Constraint::Access(Access::ThisPathway));
                    asm.push(Constraint::Access(Access::Transient));
                }
            }
            Location::State(next_state) => {
                let slots = expr_ty.storage_slots();
                if slots == 1 {
                    asm.push(Stack::Push(next_state as i64).into());
                    asm.push(Access::State.into());
                } else {
                    asm.push(Stack::Push(slots as i64).into());
                    asm.push(Stack::Push(next_state as i64).into());
                    asm.push(Access::StateRange.into());
                }
            }
        }
        Ok(asm.len() - old_asm_len)
    }

    /// Generates assembly for an `ExprKey` that is a `Location::Value`. Returns the number of
    /// opcodes used to express `expr`
    fn compile_value_expr(
        handler: &Handler,
        asm: &mut Vec<Constraint>,
        expr: &ExprKey,
        pred: &Predicate,
    ) -> Result<usize, ErrorEmitted> {
        fn compile_immediate(asm: &mut Vec<Constraint>, imm: &Immediate) {
            match imm {
                Immediate::Int(val) => asm.push(Stack::Push(*val).into()),

                Immediate::B256(val) => {
                    asm.push(Stack::Push(val[0] as i64).into());
                    asm.push(Stack::Push(val[1] as i64).into());
                    asm.push(Stack::Push(val[2] as i64).into());
                    asm.push(Stack::Push(val[3] as i64).into());
                }

                Immediate::Array(elements) => {
                    for element in elements {
                        compile_immediate(asm, element);
                    }
                }

                Immediate::Tuple(fields) => {
                    for (_, field) in fields {
                        compile_immediate(asm, field);
                    }
                }

                Immediate::Error
                | Immediate::Nil
                | Immediate::Real(_)
                | Immediate::Bool(_)
                | Immediate::String(_) => {
                    unimplemented!("other literal types are not yet supported")
                }
            }
        }

        let old_asm_len = asm.len();

        match &expr.get(pred) {
            Expr::Immediate { value, .. } => compile_immediate(asm, value),
            Expr::Array { elements, .. } => {
                for element in elements {
                    Self::compile_expr(handler, asm, element, pred)?;
                }
            }
            Expr::Tuple { fields, .. } => {
                for (_, field) in fields {
                    Self::compile_expr(handler, asm, field, pred)?;
                }
            }
            Expr::BinaryOp { op, lhs, rhs, .. } => {
                let lhs_len = Self::compile_expr(handler, asm, lhs, pred)?;
                let rhs_len = Self::compile_expr(handler, asm, rhs, pred)?;
                match op {
                    BinaryOp::Add => asm.push(Alu::Add.into()),
                    BinaryOp::Sub => asm.push(Alu::Sub.into()),
                    BinaryOp::Mul => asm.push(Alu::Mul.into()),
                    BinaryOp::Div => asm.push(Alu::Div.into()),
                    BinaryOp::Mod => asm.push(Alu::Mod.into()),
                    BinaryOp::Equal => {
                        let type_size = lhs.get_ty(pred).size();
                        if type_size == 1 {
                            asm.push(Pred::Eq.into());
                        } else {
                            asm.push(Stack::Push(type_size as i64).into());
                            asm.push(Pred::EqRange.into());
                        }
                    }
                    BinaryOp::NotEqual => {
                        asm.push(Pred::Eq.into());
                        asm.push(Pred::Not.into());
                    }
                    BinaryOp::LessThanOrEqual => asm.push(Pred::Lte.into()),
                    BinaryOp::LessThan => asm.push(Pred::Lt.into()),
                    BinaryOp::GreaterThanOrEqual => {
                        asm.push(Pred::Gte.into());
                    }
                    BinaryOp::GreaterThan => asm.push(Pred::Gt.into()),
                    BinaryOp::LogicalAnd => {
                        // Short-circuit AND. Using `JumpForwardIf`, converts `x && y` to:
                        // if !x { false } else { y }

                        // Location right before the `lhs` opcodes
                        let lhs_position = asm.len() - rhs_len - lhs_len;

                        // Location right before the `rhs` opcodes
                        let rhs_position = asm.len() - rhs_len;

                        // Push `false` before `lhs` opcodes. This is the result of the `AND`
                        // operation if `lhs` is false.
                        asm.insert(lhs_position, Stack::Push(0).into());

                        // Then push the number of instructions to skip over if the `lhs` is true.
                        // That's `rhs_len + 2` because we're going to add to add `Pop` later and
                        // we want to skip over that AND all the `rhs` opcodes
                        asm.insert(lhs_position + 1, Stack::Push(rhs_len as i64 + 2).into());

                        // Now, invert `lhs` to get the jump condition which is `!lhs`
                        asm.insert(rhs_position + 2, Pred::Not.into());

                        // Then, add the `JumpForwardIf` instruction after the `rhs` opcodes and
                        // the two newly added opcodes. The `lhs` is the condition.
                        asm.insert(
                            rhs_position + 3,
                            Constraint::TotalControlFlow(TotalControlFlow::JumpForwardIf),
                        );

                        // Finally, insert a ` Pop`. The point here is that if the jump condition
                        // (i.e. `!lhs`) is false, then we want to remove the `true` we push on the
                        // stack above.
                        asm.insert(rhs_position + 4, Stack::Pop.into());
                    }
                    BinaryOp::LogicalOr => {
                        // Short-circuit OR. Using `JumpForwardIf`, converts `x || y` to:
                        // if x { true } else { y }

                        // Location right before the `lhs` opcodes
                        let lhs_position = asm.len() - rhs_len - lhs_len;

                        // Location right before the `rhs` opcodes
                        let rhs_position = asm.len() - rhs_len;

                        // Push `true` before `lhs` opcodes. This is the result of the `OR`
                        // operation if `lhs` is true.
                        asm.insert(lhs_position, Stack::Push(1).into());

                        // Then push the number of instructions to skip over if the `lhs` is true.
                        // That's `rhs_len + 2` because we're going to add to add `Pop` later and
                        // we want to skip over that AND all the `rhs` opcodes
                        asm.insert(lhs_position + 1, Stack::Push(rhs_len as i64 + 2).into());

                        // Now add the `JumpForwardIf` instruction after the `rhs` opcodes and the
                        // two newly added opcodes. The `lhs` is the condition.
                        asm.insert(
                            rhs_position + 2,
                            Constraint::TotalControlFlow(TotalControlFlow::JumpForwardIf),
                        );

                        // Then, insert a ` Pop`. The point here is that if the jump condition
                        // (i.e. `lhs`) is false, then we want to remove the `true` we push on the
                        // stack above.
                        asm.insert(rhs_position + 3, Stack::Pop.into());
                    }
                }
            }
            Expr::UnaryOp { op, expr, .. } => {
                match op {
                    UnaryOp::Not => {
                        Self::compile_expr(handler, asm, expr, pred)?;
                        asm.push(Pred::Not.into());
                    }
                    UnaryOp::NextState => {
                        return Err(handler.emit_err(Error::Compile {
                            error: CompileError::Internal {
                                msg: "unexpected next state expression",
                                span: empty_span(),
                            },
                        }));
                    }
                    UnaryOp::Neg => {
                        // Push `0` (i.e. `lhs`) before the `expr` (i.e. `rhs`) opcodes. Then
                        // subtract `lhs` - `rhs` to negate the value.
                        asm.insert(asm.len()-1, Stack::Push(0).into());
                        Self::compile_expr(handler, asm, expr, pred)?;
                        asm.push(Alu::Sub.into())
                    }
                    UnaryOp::Error => unreachable!("unexpected Unary::Error"),
                }
            }
            Expr::StorageAccess(_, _) | Expr::ExternalStorageAccess { .. } => {
                return Err(handler.emit_err(Error::Compile {
                    error: CompileError::Internal {
                        msg: "unexpected storage access",
                        span: empty_span(),
                    },
                }));
            }
            Expr::IntrinsicCall { name, args, span } => match &name.name[..] {
                // Access ops
                "__mut_keys_len" => {
                    assert!(args.is_empty());
                    asm.push(Constraint::Access(Access::MutKeysLen));
                }

                "__mut_keys_contains" => {
                    assert_eq!(args.len(), 1);

                    let mut_key = args[0];
                    let mut_key_type = &mut_key.get_ty(pred);

                    // Check that the mutable key is an array of integers
                    let el_ty = mut_key_type.get_array_el_type().unwrap();
                    assert!(el_ty.is_int());

                    // Compile the mut key argument, insert its length, and then insert the
                    // `Sha256` opcode
                    Self::compile_expr(handler, asm, &mut_key, pred)?;
                    asm.push(Constraint::Stack(Stack::Push(mut_key_type.size() as i64)));
                    asm.push(Constraint::Access(Access::MutKeysContains));
                }

                "__this_address" => {
                    assert!(args.is_empty());
                    asm.push(Constraint::Access(Access::ThisAddress));
                }

                "__this_set_address" => {
                    assert!(args.is_empty());
                    asm.push(Constraint::Access(Access::ThisContractAddress));
                }

                "__this_pathway" => {
                    assert!(args.is_empty());
                    asm.push(Constraint::Access(Access::ThisPathway));
                }

                // Crypto ops
                "__sha256" => {
                    assert_eq!(args.len(), 1);

                    let data = args[0];
                    let data_type = &data.get_ty(pred);

                    // Compile the data argument, insert its length, and then insert the `Sha256`
                    // opcode
                    Self::compile_expr(handler, asm, &data, pred)?;
                    asm.push(Constraint::Stack(Stack::Push(data_type.size() as i64)));
                    asm.push(Constraint::Crypto(Crypto::Sha256));
                }

                "__verify_ed25519" => {
                    assert_eq!(args.len(), 3);

                    let data = args[0];
                    let signature = args[1];
                    let public_key = args[2];

                    let data_type = &data.get_ty(pred);
                    let signature_type = &signature.get_ty(pred);
                    let public_key_type = &public_key.get_ty(pred);

                    // Check argument types:
                    // - `data_type` can be anything, so nothing to check
                    // - `signature_type` must be a `{ b256, b256 }`
                    // - `public_key_type` must be a `b256`
                    let fields = signature_type
                        .get_tuple_fields()
                        .expect("expecting a tuple here");
                    assert!(fields.len() == 2 && fields[0].1.is_b256() && fields[1].1.is_b256());
                    assert!(public_key_type.is_b256());

                    // Compile all arguments separately and then insert the `VerifyEd25519` opcode
                    Self::compile_expr(handler, asm, &data, pred)?;
                    asm.push(Constraint::Stack(Stack::Push(data_type.size() as i64)));
                    Self::compile_expr(handler, asm, &signature, pred)?;
                    Self::compile_expr(handler, asm, &public_key, pred)?;
                    asm.push(Constraint::Crypto(Crypto::VerifyEd25519));
                }

                "__recover_secp256k1" => {
                    assert_eq!(args.len(), 2);

                    let data_hash = args[0];
                    let signature = args[1];

                    let data_hash_type = &data_hash.get_ty(pred);
                    let signature_type = &signature.get_ty(pred);

                    // Check argument types:
                    // - `data_hash_type` must be a `b256`
                    // - `signature_type` must be a `{ b256, b256, int }`
                    assert!(data_hash_type.is_b256());
                    let fields = signature_type
                        .get_tuple_fields()
                        .expect("expecting a tuple here");
                    assert!(
                        fields.len() == 3
                            && fields[0].1.is_b256()
                            && fields[1].1.is_b256()
                            && fields[2].1.is_int()
                    );

                    // Compile all arguments separately and then insert the `VerifyEd25519` opcode
                    Self::compile_expr(handler, asm, &data_hash, pred)?;
                    Self::compile_expr(handler, asm, &signature, pred)?;
                    asm.push(Constraint::Crypto(Crypto::RecoverSecp256k1));
                }

                "__state_len" => {
                    assert_eq!(args.len(), 1);

                    // Check argument:
                    // - `state_var` must be a path to a state var or a "next state" expression
                    assert!(match args[0].try_get(pred) {
                        Some(Expr::PathByName(name, _))
                            if pred.states().any(|(_, state)| state.name == *name) =>
                            true,
                        Some(Expr::PathByKey(var_key, _))
                            if pred
                                .states()
                                .any(|(_, state)| state.name == var_key.get(pred).name) =>
                            true,
                        Some(Expr::UnaryOp {
                            op: UnaryOp::NextState,
                            ..
                        }) => true,
                        _ => false,
                    });

                    Self::compile_expr(handler, asm, &args[0], pred)?;

                    // After compiling a path to a state var or a "next state" expression, we
                    // expect that the last opcode is a `State` or a `StateRange`. Pop that and
                    // replace it with `StateLen` since we're after the state length here and not
                    // the actual state.
                    assert!(matches!(
                        asm.last(),
                        Some(&Constraint::Access(Access::State | Access::StateRange))
                    ));
                    asm.pop();

                    asm.push(Constraint::Access(Access::StateLen));
                }

                _ => {
                    return Err(handler.emit_err(Error::Compile {
                        error: CompileError::Internal {
                            msg: "Unexpected intrinsic name",
                            span: span.clone(),
                        },
                    }));
                }
            },
            Expr::Select {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                if then_expr.can_panic(pred) || else_expr.can_panic(pred) {
                    // We need to short circuit these with control flow to avoid potential panics.
                    // The 'else' is put before the 'then' since it's easier to jump-if-true.
                    //
                    // This jump to 'then' will get updated with the proper distance below.
                    let to_then_jump_idx = asm.len();
                    asm.push(Constraint::Stack(Stack::Push(-1)));
                    Self::compile_expr(handler, asm, condition, pred)?;
                    asm.push(Constraint::TotalControlFlow(
                        TotalControlFlow::JumpForwardIf,
                    ));

                    // Compile the 'else' selection, update the prior jump.  We need to jump over
                    // the size of 'else` plus 3 instructions it uses to jump the 'then'.
                    let else_size = Self::compile_expr(handler, asm, else_expr, pred)?;
                    asm[to_then_jump_idx] = Constraint::Stack(Stack::Push(else_size as i64 + 4));

                    // This (unconditional) jump over 'then' will also get updated.
                    let to_end_jump_idx = asm.len();
                    asm.push(Constraint::Stack(Stack::Push(-1)));
                    asm.push(Constraint::Stack(Stack::Push(1)));
                    asm.push(Constraint::TotalControlFlow(
                        TotalControlFlow::JumpForwardIf,
                    ));

                    // Compile the 'then' selection, update the prior jump.
                    let then_size = Self::compile_expr(handler, asm, then_expr, pred)?;
                    asm[to_end_jump_idx] = Constraint::Stack(Stack::Push(then_size as i64 + 1));
                } else {
                    // Alternatively, evaluate both options and use ASM `select` to choose one.
                    let type_size = then_expr.get_ty(pred).size();
                    Self::compile_expr(handler, asm, else_expr, pred)?;
                    Self::compile_expr(handler, asm, then_expr, pred)?;
                    if type_size == 1 {
                        Self::compile_expr(handler, asm, condition, pred)?;
                        asm.push(Constraint::Stack(Stack::Select));
                    } else {
                        asm.push(Constraint::Stack(Stack::Push(type_size as i64)));
                        Self::compile_expr(handler, asm, condition, pred)?;
                        asm.push(Constraint::Stack(Stack::SelectRange));
                    }
                }
            }
            Expr::PathByName(..)
            | Expr::PathByKey(..)
            | Expr::Error(_)
            | Expr::MacroCall { .. }
            | Expr::Index { .. }
            | Expr::TupleFieldAccess { .. }
            | Expr::Cast { .. }
            | Expr::In { .. }
            | Expr::Range { .. }
            | Expr::Generator { .. } => {
                return Err(handler.emit_err(Error::Compile {
                    error: CompileError::Internal {
                        msg: "Unexpected expression during assembly generation",
                        span: empty_span(),
                    },
                }));
            }
        }
        Ok(asm.len() - old_asm_len)
    }

    /// Compile a path expression. Assumes that each path expressions corresponds to a decision
    /// variable or a state variable.
    fn compile_path(
        handler: &Handler,
        asm: &mut Vec<Constraint>,
        path: &String,
        pred: &Predicate,
    ) -> Result<Location, ErrorEmitted> {
        let var_index = pred
            .vars()
            .filter(|(_, var)| !var.is_pub)
            .position(|(_, var)| &var.name == path);

        let pub_var_index = pred
            .vars()
            .filter(|(_, var)| var.is_pub)
            .position(|(_, var)| &var.name == path);

        let storage_index = pred.states().position(|(_, state)| &state.name == path);

        if let Some(var_index) = var_index {
            let var_key = pred.vars().find(|(_, var)| &var.name == path).unwrap().0;
            let var_ty_size = var_key.get_ty(pred).size();
            asm.push(Stack::Push(var_index as i64).into()); // slot
            Ok(Location::DecisionVar(var_ty_size))
        } else if let Some(pub_var_index) = pub_var_index {
            let var_key = pred.vars().find(|(_, var)| &var.name == path).unwrap().0;
            asm.push(Stack::Push(pub_var_index as i64).into());
            if var_key.get_ty(pred).is_any_primitive() {
                Ok(Location::Transient(None, 1))
            } else {
                asm.push(Stack::Push(0).into()); // placeholder for offsets
                Ok(Location::Transient(None, 2))
            }
        } else if let Some(storage_index) = storage_index {
            let slot_index: usize = pred
                .states()
                .take(storage_index)
                .map(|(state_key, _)| state_key.get_ty(pred).storage_slots())
                .sum();

            asm.push(Stack::Push(slot_index as i64).into());

            Ok(Location::State(false))
        } else {
            // try external vars by looking through all available predicate instances and their
            // corresponding interfaces
            for PredicateInstance {
                name,
                interface_instance,
                predicate: predicate_name,
                ..
            } in &pred.predicate_instances
            {
                let Some(interface_instance) = pred
                    .interface_instances
                    .iter()
                    .find(|e| e.name.to_string() == *interface_instance)
                else {
                    continue;
                };

                let Some(interface) = pred
                    .interfaces
                    .iter()
                    .find(|e| e.name.to_string() == *interface_instance.interface)
                else {
                    continue;
                };

                let Some(predicate_interface) = interface
                    .predicate_interfaces
                    .iter()
                    .find(|e| e.name.to_string() == *predicate_name.to_string())
                else {
                    continue;
                };

                let Some(transient_index) = predicate_interface
                    .vars
                    .iter()
                    .position(|var| name.to_string() + "::" + &var.name.name == *path)
                else {
                    continue;
                };

                let Some(var) = predicate_interface
                    .vars
                    .iter()
                    .find(|var| name.to_string() + "::" + &var.name.name == *path)
                else {
                    continue;
                };

                asm.push(Stack::Push(transient_index as i64).into());

                if !var.ty.is_any_primitive() {
                    asm.push(Stack::Push(0).into()); // placeholder for offsets
                    return Ok(Location::Transient(
                        Some("__".to_owned() + &name.to_string() + "_pathway"),
                        2,
                    ));
                } else {
                    return Ok(Location::Transient(
                        Some("__".to_owned() + &name.to_string() + "_pathway"),
                        1,
                    ));
                }
            }
            return Err(handler.emit_err(Error::Compile {
                error: CompileError::Internal {
                    msg: "unable to find external pub var",
                    span: empty_span(),
                },
            }));
        }
    }

    /// Generates assembly for a given constraint
    fn compile_constraint(
        &mut self,
        handler: &Handler,
        expr: &ExprKey,
        pred: &Predicate,
    ) -> Result<(), ErrorEmitted> {
        let mut asm = Vec::new();
        Self::compile_expr(handler, &mut asm, expr, pred)?;
        self.c_asm.push(asm);
        Ok(())
    }

    /// Generates assembly for a given state read
    fn compile_state(
        &mut self,
        handler: &Handler,
        state: &StateVar,
        slot_idx: &mut u32,
        pred: &Predicate,
    ) -> Result<(), ErrorEmitted> {
        let mut s_asm: Vec<StateRead> = Vec::new();

        // First, get the storage key
        let storage_key = Self::compile_state_key(handler, &mut s_asm, &state.expr, pred)?;
        let key_len = match storage_key.kind {
            StorageKeyKind::Static(key) => key.len(),
            StorageKeyKind::Dynamic(size) => size,
        };

        let storage_slots = state.expr.get_ty(pred).storage_slots();
        s_asm.extend([
            Stack::Push(storage_slots as i64).into(),
            StateSlots::AllocSlots.into(),
            Stack::Push(key_len as i64).into(),       // key_len
            Stack::Push(storage_slots as i64).into(), // num_keys_to_read
            Stack::Push(0).into(),                    // slot_index
            if storage_key.is_extern {
                StateRead::KeyRangeExtern
            } else {
                StateRead::KeyRange
            },
            TotalControlFlow::Halt.into(),
        ]);
        self.s_asm.push(s_asm);

        *slot_idx += storage_slots as u32;
        Ok(())
    }
}

/// Converts a `crate::Predicate` into a `CompiledPredicate` which
/// includes generating assembly for the constraints and for state reads.
pub fn predicate_to_asm(
    handler: &Handler,
    pred: &Predicate,
) -> Result<CompiledPredicate, ErrorEmitted> {
    let mut builder = AsmBuilder::default();

    let mut slot_idx = 0;
    for (_, state) in pred.states() {
        let _ = builder.compile_state(handler, state, &mut slot_idx, pred);
    }

    for ConstraintDecl {
        expr: constraint, ..
    } in &pred.constraints
    {
        let _ = builder.compile_constraint(handler, constraint, pred);
    }

    if handler.has_errors() {
        return Err(handler.cancel());
    }

    Ok(CompiledPredicate {
        state_read: builder
            .s_asm
            .iter()
            .map(|s_asm| state_asm::to_bytes(s_asm.iter().copied()).collect())
            .collect(),
        constraints: builder
            .c_asm
            .iter()
            .map(|c_asm| constraint_asm::to_bytes(c_asm.iter().copied()).collect())
            .collect(),
        directive: Directive::Satisfy,
    })
}
