use crate::{
    error::{Error, ErrorEmitted, Handler, ParseError},
    expr::{Expr, Ident, Immediate, MatchBranch, MatchElse},
    span::{empty_span, Span, Spanned},
    types::{EphemeralDecl, NewTypeDecl, Type, UnionDecl, UnionVariant},
};
use exprs::ExprsIter;
use pint_abi_types::{ContractABI, PredicateABI, VarABI};

use std::fmt::{self, Formatter};

use fxhash::FxHashMap;

mod analyse;
mod display;
mod exprs;
mod optimize;
mod states;
mod transform;
mod vars;

pub(crate) use display::{DisplayWithContract, DisplayWithPred};
pub use exprs::{ExprKey, Exprs};
pub use states::{State, StateKey, States};
pub use vars::{Var, VarKey, Vars};

slotmap::new_key_type! { pub struct PredKey; }
slotmap::new_key_type! { pub struct UnionKey; }
slotmap::new_key_type! { pub struct CallKey; }

/// A Contract is a collection of predicates and some global consts.
#[derive(Default, Debug, Clone)]
pub struct Contract {
    pub preds: slotmap::SlotMap<PredKey, Predicate>,

    pub exprs: Exprs,
    pub consts: FxHashMap<String, Const>,
    pub storage: Option<(Vec<StorageVar>, Span)>,
    pub interfaces: Vec<Interface>,

    pub unions: slotmap::SlotMap<UnionKey, UnionDecl>,
    pub new_types: Vec<NewTypeDecl>,

    removed_macro_calls: slotmap::SecondaryMap<ExprKey, Span>,
    pub(crate) symbols: SymbolTable,
}

pub struct CompileOptions {
    pub skip_optimize: bool,
    pub print_flat: bool,
}

impl Contract {
    pub fn compile(self, handler: &Handler, options: CompileOptions) -> Result<Self, ErrorEmitted> {
        let type_checked = handler.scope(|handler| self.type_check(handler))?;
        let flattened = handler.scope(|handler| type_checked.flatten(handler))?;

        if options.print_flat {
            println!("{flattened}");
        }

        if options.skip_optimize {
            Ok(flattened)
        } else {
            Ok(flattened.optimize(handler))
        }
    }

    /// An iterator for all expressions in a predicate.
    pub(crate) fn exprs(&self, pred_key: PredKey) -> ExprsIter {
        ExprsIter::new(self, pred_key)
    }

    /// Visit expression and every sub-expression with a function.
    fn visitor_from_key(
        &self,
        kind: VisitorKind,
        expr_key: ExprKey,
        f: &mut impl FnMut(ExprKey, &Expr),
    ) {
        let expr = expr_key.get(self);

        if kind == VisitorKind::DepthFirstParentsBeforeChildren {
            // Visit the parent before recursing.
            f(expr_key, expr);
        }

        match expr {
            Expr::Error(_)
            | Expr::Path(_, _)
            | Expr::StorageAccess { .. }
            | Expr::ExternalStorageAccess { .. }
            | Expr::MacroCall { .. }
            | Expr::Immediate { .. } => {}

            Expr::Array {
                elements,
                range_expr,
                ..
            } => {
                for element in elements {
                    self.visitor_from_key(kind, *element, f);
                }
                self.visitor_from_key(kind, *range_expr, f);
            }

            Expr::Tuple { fields, .. } => {
                for (_, field) in fields {
                    self.visitor_from_key(kind, *field, f);
                }
            }

            Expr::UnionVariant { value, .. } => {
                if let Some(value) = value {
                    self.visitor_from_key(kind, *value, f);
                }
            }

            Expr::UnaryOp { expr, .. } => self.visitor_from_key(kind, *expr, f),

            Expr::BinaryOp { lhs, rhs, .. } => {
                self.visitor_from_key(kind, *lhs, f);
                self.visitor_from_key(kind, *rhs, f);
            }

            Expr::IntrinsicCall { args, .. } => {
                for arg in args {
                    self.visitor_from_key(kind, *arg, f);
                }
            }

            Expr::Select {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                self.visitor_from_key(kind, *condition, f);
                self.visitor_from_key(kind, *then_expr, f);
                self.visitor_from_key(kind, *else_expr, f);
            }

            Expr::Match {
                match_expr,
                match_branches,
                else_branch,
                ..
            } => {
                self.visitor_from_key(kind, *match_expr, f);
                for MatchBranch {
                    constraints, expr, ..
                } in match_branches
                {
                    for c_expr in constraints {
                        self.visitor_from_key(kind, *c_expr, f);
                    }
                    self.visitor_from_key(kind, *expr, f);
                }
                if let Some(MatchElse { constraints, expr }) = else_branch {
                    for c_expr in constraints {
                        self.visitor_from_key(kind, *c_expr, f);
                    }
                    self.visitor_from_key(kind, *expr, f);
                }
            }

            Expr::Index { expr, index, .. } => {
                self.visitor_from_key(kind, *expr, f);
                self.visitor_from_key(kind, *index, f);
            }

            Expr::TupleFieldAccess { tuple, .. } => {
                self.visitor_from_key(kind, *tuple, f);
            }

            Expr::Cast { value, .. } => self.visitor_from_key(kind, *value, f),

            Expr::In {
                value, collection, ..
            } => {
                self.visitor_from_key(kind, *value, f);
                self.visitor_from_key(kind, *collection, f);
            }

            Expr::Range { lb, ub, .. } => {
                self.visitor_from_key(kind, *lb, f);
                self.visitor_from_key(kind, *ub, f);
            }

            Expr::Generator {
                gen_ranges,
                conditions,
                body,
                ..
            } => {
                for (_, range) in gen_ranges {
                    self.visitor_from_key(kind, *range, f);
                }
                for condition in conditions {
                    self.visitor_from_key(kind, *condition, f);
                }
                self.visitor_from_key(kind, *body, f);
            }

            Expr::UnionTag { union_expr, .. } | Expr::UnionValue { union_expr, .. } => {
                self.visitor_from_key(kind, *union_expr, f)
            }
        }

        if kind == VisitorKind::DepthFirstChildrenBeforeParents {
            // Visit the parent after recursing.
            f(expr_key, expr);
        }
    }

    pub(crate) fn visitor<F: FnMut(ExprKey, &Expr)>(
        &self,
        pred_key: PredKey,
        kind: VisitorKind,
        mut f: F,
    ) {
        for expr_key in self.root_set(pred_key) {
            self.visitor_from_key(kind, expr_key, &mut |k, e| f(k, e));
        }
    }

    pub fn root_set(&self, pred_key: PredKey) -> impl Iterator<Item = ExprKey> + '_ {
        self.preds.get(pred_key).unwrap().root_set()
    }

    pub fn replace_exprs(
        &mut self,
        pred_key: Option<PredKey>,
        old_expr: ExprKey,
        new_expr: ExprKey,
    ) {
        // Here we recursively replace any interior expr_keys.
        self.exprs
            .update_exprs(|_, expr| expr.replace_one_to_one(old_expr, new_expr));
        self.exprs
            .update_types(|_, expr_ty| expr_ty.replace_type_expr(old_expr, new_expr));

        // But we need to replace any 'root' set exprs too, such as those in consts values and
        // types, the new-type aliases and then any references in Predicates.
        self.consts
            .values_mut()
            .for_each(|Const { expr, decl_ty }| {
                if *expr == old_expr {
                    *expr = new_expr;
                }
                decl_ty.replace_type_expr(old_expr, new_expr);
            });

        if let Some((storage_vars, _)) = &mut self.storage {
            storage_vars
                .iter_mut()
                .for_each(|StorageVar { ty, .. }| ty.replace_type_expr(old_expr, new_expr));
        }

        self.new_types
            .iter_mut()
            .for_each(|NewTypeDecl { ty, .. }| ty.replace_type_expr(old_expr, new_expr));

        self.interfaces.iter_mut().for_each(
            |Interface {
                 storage,
                 predicate_interfaces,
                 ..
             }| {
                if let Some((storage_vars, _)) = storage {
                    storage_vars
                        .iter_mut()
                        .for_each(|StorageVar { ty, .. }| ty.replace_type_expr(old_expr, new_expr))
                }
                predicate_interfaces
                    .iter_mut()
                    .for_each(|PredicateInterface { vars, .. }| {
                        vars.iter_mut().for_each(|InterfaceVar { ty, .. }| {
                            ty.replace_type_expr(old_expr, new_expr)
                        });
                    });
            },
        );

        if let Some(pred_key) = pred_key {
            self.preds
                .get_mut(pred_key)
                .unwrap()
                .replace_exprs(old_expr, new_expr);
        }
    }

    // Apply a mutating closure to every single type in the contract.
    pub fn update_types(&mut self, f: impl Fn(&mut Type), skip_new_types: bool) {
        // Update every expression type in the contract.
        self.exprs.update_types(|_, expr_ty| f(expr_ty));

        // Loop for each predicate and update their var types and state types.
        self.preds
            .keys()
            .collect::<Vec<_>>()
            .iter()
            .for_each(|pred_key| {
                if let Some(pred) = self.preds.get_mut(*pred_key) {
                    pred.vars.update_types(|_, var_ty| f(var_ty));
                    pred.states.update_types(|_, state_ty| f(state_ty));
                }
            });

        // Update every declared const type.
        self.consts
            .values_mut()
            .for_each(|Const { decl_ty, .. }| f(decl_ty));

        // Update every union decl variant type.
        for UnionDecl { variants, .. } in self.unions.values_mut() {
            for variant in variants {
                if let Some(variant_ty) = &mut variant.ty {
                    f(variant_ty);
                }
            }
        }

        // Update every alias type.
        if !skip_new_types {
            for NewTypeDecl { ty, .. } in &mut self.new_types {
                f(ty);
            }
        }

        // Update every cast or union variant expression types.
        self.exprs.update_exprs(|_, expr| {
            if let Expr::Cast { ty, .. } = expr {
                f(ty);
            }

            if let Expr::UnionValue { variant_ty, .. } = expr {
                f(variant_ty);
            }
        });

        // Update every storage variable type.
        if let Some((storage_vars, _)) = self.storage.as_mut() {
            storage_vars.iter_mut().for_each(|StorageVar { ty, .. }| {
                f(ty);
            })
        }

        // Update every type found in interface instance decls.
        self.interfaces.iter_mut().for_each(
            |Interface {
                 storage,
                 predicate_interfaces,
                 ..
             }| {
                // Update every storage variable in the interface.
                if let Some((storage_vars, _)) = storage.as_mut() {
                    storage_vars
                        .iter_mut()
                        .for_each(|StorageVar { ty, .. }| f(ty));
                }

                // Update every decision variable in the interface.
                predicate_interfaces
                    .iter_mut()
                    .for_each(|predicate_interface| {
                        predicate_interface
                            .vars
                            .iter_mut()
                            .for_each(|InterfaceVar { ty, .. }| f(ty));
                    });
            },
        );
    }

    /// Generates a `ContractABI` given a `Contract`
    pub fn abi(&self, handler: &Handler) -> Result<ContractABI, ErrorEmitted> {
        Ok(ContractABI {
            predicates: self
                .preds
                .iter()
                .map(|(_, pred)| pred.abi(handler, self))
                .collect::<Result<_, _>>()?,
            storage: self
                .storage
                .as_ref()
                .map(|(storage, _)| {
                    storage
                        .iter()
                        .map(|StorageVar { name, ty, .. }| {
                            // The key of `ty` is either the `index` if the storage type is
                            // primitive or a map, or it's `[index, 0]`. The `0` here is a
                            // placeholder for offsets.
                            Ok(VarABI {
                                name: name.to_string(),
                                ty: ty.abi(handler, self)?,
                            })
                        })
                        .collect::<Result<_, _>>()
                })
                .unwrap_or(Ok(vec![]))?,
        })
    }

    pub fn expr_key_to_span(&self, expr_key: ExprKey) -> Span {
        expr_key
            .try_get(self)
            .map(|expr| expr.span().clone())
            .unwrap_or_else(empty_span)
    }

    pub fn add_removed_macro_call(&mut self, expr_key: ExprKey, span: Span) {
        self.removed_macro_calls.insert(expr_key, span);
    }

    pub fn is_removed_macro_call(&self, expr_key: ExprKey) -> bool {
        self.removed_macro_calls.contains_key(expr_key)
    }

    /// Returns a local `StorageVar` given a var name. Panics if anything goes wrong.
    pub fn storage_var(&self, name: &String) -> (usize, &StorageVar) {
        let storage = &self
            .storage
            .as_ref()
            .expect("a storage block must have been declared")
            .0;
        let storage_index = storage
            .iter()
            .position(|var| var.name.name == *name)
            .expect("storage access should have been checked before");
        (storage_index, &storage[storage_index])
    }

    /// Returns an external `StorageVar` given an interface name and a var name. Panics if anything
    /// goes wrong.
    pub fn external_storage_var(&self, interface: &String, name: &String) -> (usize, &StorageVar) {
        // Get the `interface` declaration that the storage access refers to
        let interface = &self
            .interfaces
            .iter()
            .find(|e| e.name.to_string() == *interface)
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

        (storage_index, &storage[storage_index])
    }

    pub(crate) fn root_array_range_exprs(&self) -> impl Iterator<Item = ExprKey> + '_ {
        // This currently only fetches array type range expressions.
        self.storage
            .iter()
            .flat_map(|(storage_vars, _)| {
                storage_vars
                    .iter()
                    .flat_map(|StorageVar { ty, .. }| ty.get_all_array_range_exprs())
            })
            .chain(
                self.interfaces
                    .iter()
                    .flat_map(
                        |Interface {
                             storage,
                             predicate_interfaces,
                             ..
                         }| {
                            storage
                                .iter()
                                .flat_map(|(storage_vars, _)| {
                                    storage_vars.iter().map(|StorageVar { ty, .. }| ty)
                                })
                                .chain(predicate_interfaces.iter().flat_map(
                                    |PredicateInterface { vars, .. }| {
                                        vars.iter().map(|InterfaceVar { ty, .. }| ty)
                                    },
                                ))
                        },
                    )
                    .flat_map(|ty| ty.get_all_array_range_exprs()),
            )
            .chain(
                self.new_types
                    .iter()
                    .flat_map(|NewTypeDecl { ty, .. }| ty.get_all_array_range_exprs()),
            )
            .chain(
                self.unions
                    .iter()
                    .flat_map(|(_key, UnionDecl { variants, .. })| {
                        variants.iter().flat_map(|UnionVariant { ty, .. }| ty)
                    })
                    .flat_map(|ty| ty.get_all_array_range_exprs()),
            )
            .chain(
                self.consts
                    .values()
                    .filter_map(|Const { decl_ty, .. }| decl_ty.get_array_range_expr()),
            )
    }
}

/// An in-progress predicate, possibly malformed or containing redundant information.  Designed to
/// be iterated upon and to be reduced to a [Predicate].
#[derive(Debug, Default, Clone)]
pub struct Predicate {
    pub name: String,

    pub vars: Vars,
    pub states: States,

    pub constraints: Vec<ConstraintDecl>,
    pub if_decls: Vec<IfDecl>,
    pub match_decls: Vec<MatchDecl>,

    pub ephemerals: Vec<EphemeralDecl>,

    // Each of the initialised variables.  Used by type inference.
    pub var_inits: slotmap::SecondaryMap<VarKey, ExprKey>,

    // CallKey is used in a secondary map in the parser context to access the actual call data.
    pub calls: slotmap::SlotMap<CallKey, String>,

    // A list of all availabe interface instances
    pub interface_instances: Vec<InterfaceInstance>,

    // A list of all availabe predicate instances
    pub predicate_instances: Vec<PredicateInstance>,

    pub symbols: SymbolTable,
}

impl Predicate {
    pub fn new(name: String) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }

    /// Generate a `PredicateABI` given a `Predicate`
    pub fn abi(
        &self,
        handler: &Handler,
        contract: &Contract,
    ) -> Result<PredicateABI, ErrorEmitted> {
        Ok(PredicateABI {
            name: self.name.clone(),
            vars: self
                .vars()
                .filter(|(_, var)| !var.is_pub)
                .map(|(var_key, _)| var_key.abi(handler, contract, self))
                .collect::<Result<_, _>>()?,
            pub_vars: self
                .vars()
                .filter(|(_, var)| var.is_pub)
                .map(|(var_key, Var { name, .. })| {
                    Ok(VarABI {
                        name: name.to_string(),
                        ty: {
                            let ty = var_key.get_ty(self);
                            ty.abi(handler, contract)?
                        },
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    pub fn insert_ephemeral(
        &mut self,
        mod_prefix: &str,
        name: &Ident,
        ty: Type,
    ) -> std::result::Result<(), ErrorEmitted> {
        let full_name = self
            .symbols
            .add_symbol_no_clash(mod_prefix, None, name, name.span.clone());

        if !self
            .ephemerals
            .iter()
            .any(|eph_decl| eph_decl.name == full_name)
        {
            self.ephemerals.push(EphemeralDecl {
                name: full_name,
                ty,
                span: name.span.clone(),
            });
        }

        Ok(())
    }

    pub fn replace_exprs(&mut self, old_expr: ExprKey, new_expr: ExprKey) {
        self.vars.update_types(|_var_key, var_ty| {
            var_ty.replace_type_expr(old_expr, new_expr);
        });

        self.states.update_types(|_state_key, state_ty| {
            state_ty.replace_type_expr(old_expr, new_expr);
        });

        self.states.update_states(|State { expr, .. }| {
            if *expr == old_expr {
                *expr = new_expr;
            }
        });

        self.constraints
            .iter_mut()
            .for_each(|ConstraintDecl { expr, .. }| {
                if *expr == old_expr {
                    *expr = new_expr;
                }
            });

        self.if_decls.iter_mut().for_each(|if_decl| {
            if_decl.replace_exprs(old_expr, new_expr);
        });

        self.var_inits.iter_mut().for_each(|(_, expr)| {
            if *expr == old_expr {
                *expr = new_expr;
            }
        });

        self.interface_instances
            .iter_mut()
            .for_each(|InterfaceInstance { address, .. }| {
                if *address == old_expr {
                    *address = new_expr;
                }
            });

        self.predicate_instances
            .iter_mut()
            .for_each(|PredicateInstance { address, .. }| {
                if let Some(ref mut address) = address {
                    if *address == old_expr {
                        *address = new_expr;
                    }
                }
            });
    }

    /// Return an iterator to the 'root set' of expressions, based on the constraints, states,
    /// interface instances, and predicate instances.
    fn root_set(&self) -> impl Iterator<Item = ExprKey> + '_ {
        self.constraints
            .iter()
            .map(|c| c.expr)
            .chain(self.states().map(|(_, state)| state.expr))
            .chain(self.interface_instances.iter().map(|ii| ii.address))
            .chain(self.predicate_instances.iter().filter_map(|pi| pi.address))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VisitorKind {
    DepthFirstChildrenBeforeParents,
    DepthFirstParentsBeforeChildren,
}

#[derive(Clone, Debug)]
pub struct Const {
    pub(crate) expr: ExprKey,
    pub(crate) decl_ty: Type,
}

#[derive(Clone, Debug)]
pub struct ConstraintDecl {
    pub expr: ExprKey,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum BlockStatement {
    Constraint(ConstraintDecl),
    If(IfDecl),
    Match(MatchDecl),
}

impl BlockStatement {
    fn replace_exprs(&mut self, old_expr: ExprKey, new_expr: ExprKey) {
        match self {
            BlockStatement::Constraint(ConstraintDecl { expr, .. }) => {
                if *expr == old_expr {
                    *expr = new_expr;
                }
            }

            BlockStatement::If(if_decl) => if_decl.replace_exprs(old_expr, new_expr),

            BlockStatement::Match(match_decl) => match_decl.replace_exprs(old_expr, new_expr),
        }
    }

    fn fmt_with_indent(
        &self,
        f: &mut Formatter,
        contract: &Contract,
        pred: &Predicate,
        indent: usize,
    ) -> fmt::Result {
        let indentation = " ".repeat(4 * indent);
        match self {
            Self::Constraint(constraint) => {
                writeln!(
                    f,
                    "{indentation}constraint {}",
                    contract.with_ctrct(constraint.expr)
                )
            }

            Self::If(if_decl) => if_decl.fmt_with_indent(f, contract, pred, indent),

            Self::Match(match_decl) => match_decl.fmt_with_indent(f, contract, pred, indent),
        }
    }

    /// Returns all the constraints in the `BlockStatement`
    fn get_constraints(&self) -> Vec<&ConstraintDecl> {
        let mut constraints = Vec::new();
        match self {
            BlockStatement::Constraint(constraint) => constraints.push(constraint),
            BlockStatement::If(if_decl) => constraints.extend(if_decl.get_constraints()),
            BlockStatement::Match(match_decl) => constraints.extend(match_decl.get_constraints()),
        }
        constraints
    }
}

#[derive(Clone, Debug)]
pub struct IfDecl {
    pub condition: ExprKey,
    pub then_block: Vec<BlockStatement>,
    pub else_block: Option<Vec<BlockStatement>>,
    pub span: Span,
}

impl IfDecl {
    fn replace_exprs(&mut self, old_expr: ExprKey, new_expr: ExprKey) {
        if self.condition == old_expr {
            self.condition = new_expr;
        }

        self.then_block
            .iter_mut()
            .for_each(|stmt| stmt.replace_exprs(old_expr, new_expr));

        if let Some(else_block) = &mut self.else_block {
            else_block
                .iter_mut()
                .for_each(|stmt| stmt.replace_exprs(old_expr, new_expr));
        }
    }

    fn fmt_with_indent(
        &self,
        f: &mut Formatter,
        contract: &Contract,
        pred: &Predicate,
        indent: usize,
    ) -> fmt::Result {
        let indentation = " ".repeat(4 * indent);
        writeln!(
            f,
            "{indentation}if {} {{",
            contract.with_ctrct(self.condition)
        )?;
        for block_statement in &self.then_block {
            block_statement.fmt_with_indent(f, contract, pred, indent + 1)?;
        }
        if let Some(else_block) = &self.else_block {
            writeln!(f, "{indentation}}} else {{")?;
            for block_statement in else_block {
                block_statement.fmt_with_indent(f, contract, pred, indent + 1)?;
            }
        }
        writeln!(f, "{indentation}}}")
    }

    /// Returns all the constraints in the `IfDecl`
    fn get_constraints(&self) -> Vec<&ConstraintDecl> {
        let mut constraints = Vec::new();
        for block_statement in &self.then_block {
            constraints.extend(block_statement.get_constraints());
        }
        if let Some(else_block) = self.else_block.as_ref() {
            for block_statement in else_block {
                constraints.extend(block_statement.get_constraints());
            }
        }
        constraints
    }
}

#[derive(Clone, Debug)]
pub struct MatchDecl {
    pub match_expr: ExprKey,
    pub match_branches: Vec<MatchDeclBranch>,
    pub else_branch: Option<Vec<BlockStatement>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct MatchDeclBranch {
    pub name: String,
    pub name_span: Span,
    pub binding: Option<Ident>,
    pub block: Vec<BlockStatement>,
}

impl MatchDecl {
    fn replace_exprs(&mut self, old_expr: ExprKey, new_expr: ExprKey) {
        if self.match_expr == old_expr {
            self.match_expr = new_expr;
        }

        for MatchDeclBranch { block, .. } in &mut self.match_branches {
            for stmt in block {
                stmt.replace_exprs(old_expr, new_expr);
            }
        }

        if let Some(else_block) = &mut self.else_branch {
            for stmt in else_block {
                stmt.replace_exprs(old_expr, new_expr);
            }
        }
    }

    fn fmt_with_indent(
        &self,
        f: &mut Formatter,
        contract: &Contract,
        pred: &Predicate,
        indent: usize,
    ) -> fmt::Result {
        let indentation = " ".repeat(4 * indent);
        let else_indentation = " ".repeat(4 * (indent + 1));

        writeln!(
            f,
            "{indentation}match {} {{",
            contract.with_ctrct(self.match_expr)
        )?;

        for match_branch in &self.match_branches {
            match_branch.fmt_with_indent(f, contract, pred, indent + 1)?;
        }

        if let Some(else_block_stmts) = &self.else_branch {
            writeln!(f, "{else_indentation}else => {{")?;
            for block_statement in else_block_stmts {
                block_statement.fmt_with_indent(f, contract, pred, indent + 2)?;
            }
            writeln!(f, "{else_indentation}}}")?;
        }

        writeln!(f, "{indentation}}}")
    }

    /// Returns all the constraints in the `MatchDecl`
    fn get_constraints(&self) -> Vec<&ConstraintDecl> {
        let mut constraints = Vec::new();
        for match_branch in &self.match_branches {
            for block_statement in &match_branch.block {
                constraints.extend(block_statement.get_constraints());
            }
        }
        constraints
    }
}

impl MatchDeclBranch {
    fn fmt_with_indent(
        &self,
        f: &mut Formatter,
        contract: &Contract,
        pred: &Predicate,
        indent: usize,
    ) -> fmt::Result {
        let indentation = " ".repeat(4 * indent);

        write!(f, "{indentation}{}", self.name)?;
        if let Some(id) = &self.binding {
            write!(f, "({})", id)?;
        }
        writeln!(f, " => {{")?;

        for block_statement in &self.block {
            block_statement.fmt_with_indent(f, contract, pred, indent + 1)?;
        }

        writeln!(f, "{indentation}}}")
    }
}

#[derive(Clone, Debug)]
pub struct StorageVar {
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

/// A a predicate interface that belong in an `Interface`.
#[derive(Clone, Debug)]
pub struct PredicateInterface {
    pub name: Ident,
    pub vars: Vec<InterfaceVar>,
    pub span: Span,
}

/// A declaration inside an `Interface`. This could either be a `storage` declaration or a
/// predicate interface declaration
#[derive(Clone, Debug)]
pub enum InterfaceDecl {
    StorageDecl((Vec<StorageVar>, Span)),
    PredicateInterface(PredicateInterface),
}

/// full interface to an external contract
#[derive(Clone, Debug)]
pub struct Interface {
    pub name: Ident,
    pub storage: Option<(Vec<StorageVar>, Span)>,
    pub predicate_interfaces: Vec<PredicateInterface>,
    pub span: Span,
}

/// A decision variable that lives inside a predicate interface. Unlike `Var`, the type here is not
/// optional
#[derive(Clone, Debug)]
pub struct InterfaceVar {
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

/// An interface instance that specifies an address
#[derive(Clone, Debug)]
pub struct InterfaceInstance {
    pub name: Ident,
    pub interface: String,
    pub address: ExprKey,
    pub span: Span,
}

/// A predicate instance that specifies an address
#[derive(Clone, Debug)]
pub struct PredicateInstance {
    pub name: Ident,
    pub interface_instance: Option<String>,
    pub predicate: Ident,
    pub address: Option<ExprKey>,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct SymbolTable {
    symbols: FxHashMap<String, Span>,
}

impl SymbolTable {
    pub fn add_symbol_no_clash(
        &mut self,
        mod_prefix: &str,
        local_scope: Option<&str>,
        name: &Ident,
        span: Span,
    ) -> String {
        let full_name = Self::make_full_symbol(mod_prefix, local_scope, name);
        self.symbols.entry(full_name.clone()).or_insert(span);
        full_name
    }

    pub fn add_symbol(
        &mut self,
        handler: &Handler,
        mod_prefix: &str,
        local_scope: Option<&str>,
        name: &Ident,
        span: Span,
    ) -> std::result::Result<String, ErrorEmitted> {
        let full_name = Self::make_full_symbol(mod_prefix, local_scope, name);
        self.symbols
            .get(&full_name)
            .map(|prev_span| {
                // Name clash.
                Err(handler.emit_err(Error::Parse {
                    error: ParseError::NameClash {
                        sym: name.name.clone(),
                        span: name.span.clone(),
                        prev_span: prev_span.clone(),
                    },
                }))
            })
            .unwrap_or_else(|| {
                // Not found in the symbol table.
                self.symbols.insert(full_name.clone(), span);
                Ok(full_name)
            })
    }

    pub fn check_for_clash(
        &self,
        handler: &Handler,
        other: &SymbolTable,
    ) -> std::result::Result<(), ErrorEmitted> {
        // Self has the original symbols, `other` has the new potentially clashing symbols.
        for (symbol, span) in &other.symbols {
            if let Some(prev_span) = self.symbols.get(symbol) {
                handler.emit_err(Error::Parse {
                    error: ParseError::NameClash {
                        sym: symbol.clone(),
                        span: span.clone(),
                        prev_span: prev_span.clone(),
                    },
                });
            }
        }

        if handler.has_errors() {
            Err(handler.cancel())
        } else {
            Ok(())
        }
    }

    fn make_full_symbol(mod_prefix: &str, local_scope: Option<&str>, name: &Ident) -> String {
        let local_scope_str = local_scope
            .map(|ls| ls.to_owned() + "::")
            .unwrap_or_default();
        mod_prefix.to_owned() + &local_scope_str + &name.name
    }
}
