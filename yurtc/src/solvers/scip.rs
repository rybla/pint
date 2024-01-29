mod constraint;
mod invert;
mod print;
mod variable;

use crate::{
    error::SolveError,
    intent::{Intent, SolveDirective},
    span::empty_span,
};
use russcip::{prelude::*, ProblemCreated, Solved};

pub struct Solver<'a, State> {
    model: Model<State>,
    intent: &'a Intent,
    unique_var_suffix: usize, // unique suffix for variables introduced by the solver
    unique_cons_suffix: usize, // unique suffix for names of cosntraints
}

impl<'a, State> Solver<'a, State> {
    /// Creates a new instance of `Solver` given an `Intent`.
    pub fn new(intent: &'a Intent) -> Solver<ProblemCreated> {
        Solver {
            model: Model::new()
                .hide_output()
                .include_default_plugins()
                .create_prob("solver")
                .set_obj_sense(match intent.directive {
                    // For constraint satisfaction problems, `ObjSense` does not matter. The
                    // objective function is going to be set to 0 anyways.
                    SolveDirective::Minimize(_) | SolveDirective::Satisfy => ObjSense::Minimize,
                    SolveDirective::Maximize(_) => ObjSense::Maximize,
                }),
            intent,
            unique_var_suffix: 0,
            unique_cons_suffix: 0,
        }
    }

    fn new_var_name(&mut self) -> String {
        let new_name = format!("INTRODUCED{}", self.unique_var_suffix);
        self.unique_var_suffix += 1;
        new_name
    }

    fn new_cons_name(&mut self) -> String {
        let new_name = format!("CONS{}", self.unique_cons_suffix);
        self.unique_cons_suffix += 1;
        new_name
    }
}

impl<'a> Solver<'a, ProblemCreated> {
    pub fn solve(mut self) -> Result<Solver<'a, Solved>, SolveError> {
        // No state variables are allowed
        if !self.intent.states.is_empty() {
            return Err(SolveError::Internal {
                msg: "(scip) no state variables are allowed at this stage",
                span: empty_span(),
            });
        }

        // Convert all variables
        for variable in &self.intent.vars {
            self.convert_variable(variable)?;
        }

        // Convert all constraints
        for constraint in &self.intent.constraints {
            self.convert_constraint(constraint)?;
        }

        Ok(Solver {
            model: self.model.solve(),
            intent: self.intent,
            unique_var_suffix: self.unique_var_suffix,
            unique_cons_suffix: self.unique_cons_suffix,
        })
    }
}
