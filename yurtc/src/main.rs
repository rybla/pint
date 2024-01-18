use std::path::Path;
use yurtc::{asm_gen::intent_to_asm, error, parser};

fn main() -> anyhow::Result<()> {
    let (filepath, compile_flag, asm_flag, solve_flag) = parse_cli();
    let filepath = Path::new(&filepath);

    // Lex + Parse
    let intermediate_intent = match parser::parse_project(filepath) {
        Ok(ii) => ii,
        Err(errors) => {
            if !cfg!(test) {
                error::print_errors(&errors);
            }
            yurtc::yurtc_bail!(errors.len(), filepath)
        }
    };

    if !compile_flag && !solve_flag {
        eprintln!("{intermediate_intent}");
        return Ok(());
    }

    // Flatten the intermediate intent
    let mut flattened = match intermediate_intent.flatten() {
        Ok(flattened) => flattened,
        Err(error) => {
            if !cfg!(test) {
                error::print_errors(&vec![error::Error::Compile { error }]);
            }
            yurtc::yurtc_bail!(1, filepath)
        }
    };

    // Compile the flattened intent down to a final intent
    let intent = match flattened.compile() {
        Ok(intent) => intent,
        Err(error) => {
            eprintln!("{flattened}");
            if !cfg!(test) {
                error::print_errors(&vec![error::Error::Compile { error }]);
            }
            yurtc::yurtc_bail!(1, filepath)
        }
    };

    // This is WIP. So far, simply print the serialized JSON to `stdout`. That'll likely change in
    // the future when we decide on a serialized scheme.
    if asm_flag {
        match intent_to_asm(&intent) {
            Ok(intent) => {
                serde_json::to_writer(std::io::stdout(), &intent)?;
            }
            Err(error) => {
                if !cfg!(test) {
                    error::print_errors(&vec![error::Error::Compile { error }]);
                }
                yurtc::yurtc_bail!(1, filepath)
            }
        };
    }

    if !solve_flag {
        if !asm_flag {
            eprintln!("{intent}");
        }
        return Ok(());
    }

    if solve_flag && !cfg!(feature = "solver-scip") && !cfg!(feature = "solver-pcp") {
        eprintln!("Solving is disabled in this build.");
    }

    #[cfg(feature = "solver-scip")]
    {
        use russcip::ProblemCreated;
        use yurtc::solvers::scip::*;

        // Solve the final intent. This assumes, for now, that the final intent has no state variables
        let solver = Solver::<ProblemCreated>::new(&intent);
        let solver = match solver.solve() {
            Ok(solver) => solver,
            Err(error) => {
                if !cfg!(test) {
                    error::print_errors(&vec![error::Error::Solve { error }]);
                }
                yurtc::yurtc_bail!(1, filepath)
            }
        };
        solver.print_solution();
    }

    Ok(())
}

fn parse_cli() -> (String, bool, bool, bool) {
    // This is very basic for now.  It only take a single source file and a single optional flag.
    // It'll also just exit if `-h` or `-V` are passed, or if there's an error.
    let cli = clap::command!()
        .arg(
            clap::Arg::new("compile")
                .short('c')
                .long("compile")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            clap::Arg::new("solve")
                .short('s')
                .long("solve")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            clap::Arg::new("asm")
                .short('a')
                .long("asm")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            clap::Arg::new("filepath")
                .required(true)
                .action(clap::ArgAction::Set),
        )
        .get_matches();

    let filepath = cli.get_one::<String>("filepath").unwrap();

    let compile_flag = cli.get_flag("compile");

    let asm_flag = cli.get_flag("asm");

    let solve_flag = cli.get_flag("solve");

    (filepath.clone(), compile_flag, asm_flag, solve_flag)
}
