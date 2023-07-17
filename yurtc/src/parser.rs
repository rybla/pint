use crate::{
    ast,
    error::{print_on_failure, CompileError, ParseError},
    lexer::{self, Token, KEYWORDS},
};
use chumsky::{prelude::*, Stream};
use regex::Regex;
use std::{fs::read_to_string, path::Path};

#[cfg(test)]
mod tests;

type Ast = Vec<ast::Decl>;

pub(super) fn parse_path_to_ast(path: &Path, filename: &str) -> anyhow::Result<Ast> {
    parse_str_to_ast(&read_to_string(path)?, filename)
}

/// Parse `source` and returns an AST. Upon failure, print all compile errors and exit.
fn parse_str_to_ast(source: &str, filename: &str) -> anyhow::Result<Ast> {
    match parse_str_to_ast_inner(source) {
        Ok(ast) => Ok(ast),
        Err(errors) => {
            if !cfg!(test) {
                print_on_failure(filename, source, &errors);
            }
            yurtc_bail!(errors.len(), filename)
        }
    }
}

/// Parse `source` and returns an AST. Upon failure, return a vector of all compile errors
/// encountered.
fn parse_str_to_ast_inner(source: &str) -> Result<Ast, Vec<CompileError>> {
    let mut errors = vec![];

    // Lex the input into tokens and spans. Also collect any lex errors encountered.
    let (tokens, lex_errors) = lexer::lex(source);
    errors.extend(lex_errors);

    // Provide a token stream
    let eoi_span = source.len()..source.len();
    let token_stream = Stream::from_iter(eoi_span, tokens.into_iter());

    // Parse the token stream
    match yurt_program().parse(token_stream) {
        Ok(_) if !errors.is_empty() => Err(errors),
        Err(parsing_errors) => {
            let parsing_errors: Vec<_> = parsing_errors
                .iter()
                .map(|error| CompileError::Parse {
                    error: error.clone(),
                })
                .collect();

            errors.extend(parsing_errors);
            Err(errors)
        }
        Ok(ast) => Ok(ast),
    }
}

fn yurt_program<'sc>() -> impl Parser<Token<'sc>, Ast, Error = ParseError<'sc>> + Clone {
    choice((
        var_decl(expr()),
        let_decl(expr()),
        constraint_decl(expr()),
        solve_decl(),
        fn_decl(expr()),
    ))
    .repeated()
    .then_ignore(end())
}

fn var_decl<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Decl, Error = ParseError<'sc>> + Clone {
    let type_spec = just(Token::Colon).ignore_then(type_());
    let init = just(Token::Eq).ignore_then(expr);
    just(Token::Var)
        .ignore_then(ident())
        .then(type_spec.or_not())
        .then(init.or_not())
        .then_ignore(just(Token::Semi))
        .validate(|((name, ty), init), span, emit| {
            if ty.is_none() && init.is_none() {
                emit(ParseError::UntypedDecisionVar {
                    span,
                    name: name.clone(),
                })
            }
            ((name, ty), init)
        })
        .map(|((name, ty), init)| ast::Decl::Var(ast::VarStatement { name, ty, init }))
}

fn let_decl<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Decl, Error = ParseError<'sc>> + Clone {
    let type_spec = just(Token::Colon).ignore_then(type_());
    let init = just(Token::Eq).ignore_then(expr);
    just(Token::Let)
        .ignore_then(ident())
        .then(type_spec.or_not())
        .then(init)
        .then_ignore(just(Token::Semi))
        .map(|((name, ty), init)| ast::Decl::Let(ast::LetStatement { name, ty, init }))
}

fn constraint_decl<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Decl, Error = ParseError<'sc>> + Clone {
    just(Token::Constraint)
        .ignore_then(expr)
        .then_ignore(just(Token::Semi))
        .map(ast::Decl::Constraint)
}

fn solve_decl<'sc>() -> impl Parser<Token<'sc>, ast::Decl, Error = ParseError<'sc>> + Clone {
    let solve_satisfy = just(Token::Satisfy).to(ast::SolveFunc::Satisfy);
    let solve_minimize = just(Token::Minimize)
        .ignore_then(ident())
        .map(ast::SolveFunc::Minimize);
    let solve_maximize = just(Token::Maximize)
        .ignore_then(ident())
        .map(ast::SolveFunc::Maximize);

    just(Token::Solve)
        .ignore_then(choice((solve_satisfy, solve_minimize, solve_maximize)))
        .then_ignore(just(Token::Semi))
        .map(ast::Decl::Solve)
}

fn fn_decl<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Decl, Error = ParseError<'sc>> + Clone {
    let type_spec = just(Token::Colon).ignore_then(type_());

    let params = ident()
        .then(type_spec)
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let return_type = just(Token::Arrow).ignore_then(type_());

    just(Token::Fn)
        .ignore_then(ident())
        .then(params)
        .then(return_type)
        .then(code_block_expr(expr))
        .map(|(((name, params), return_type), body)| ast::Decl::Fn {
            name,
            params,
            return_type,
            body,
        })
}

fn code_block_expr<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Block, Error = ParseError<'sc>> + Clone {
    let code_block_body = choice((
        var_decl(expr.clone()),
        let_decl(expr.clone()),
        constraint_decl(expr.clone()),
    ))
    .repeated()
    .then(expr);

    code_block_body
        .delimited_by(just(Token::BraceOpen), just(Token::BraceClose))
        .map(|(statements, expr)| ast::Block {
            statements,
            final_expr: Box::new(expr),
        })
}

fn unary_op<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone {
    choice((
        just(Token::Plus).to(ast::UnaryOp::Pos),
        just(Token::Minus).to(ast::UnaryOp::Neg),
        just(Token::Bang).to(ast::UnaryOp::Not),
    ))
    .then(expr)
    .map(|(op, expr)| ast::Expr::UnaryOp {
        op,
        expr: Box::new(expr),
    })
}

fn if_expr<'sc>(
    expr: impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
) -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone {
    let then_block = code_block_expr(expr.clone());
    let else_block = just(Token::Else).ignore_then(code_block_expr(expr.clone()));

    just(Token::If)
        .ignore_then(expr)
        .then(then_block)
        .then(else_block)
        .map(|((condition, then_block), else_block)| {
            ast::Expr::If(ast::IfExpr {
                condition: Box::new(condition),
                then_block,
                else_block,
            })
        })
}

fn expr<'sc>() -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone {
    recursive(|expr| {
        let args = expr
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

        let call = ident()
            .then(args.clone())
            .map(|(name, args)| ast::Expr::Call { name, args });

        let tuple = args
            .validate(|args, span, emit| {
                if args.is_empty() {
                    emit(ParseError::EmptyTupleExpr { span })
                }
                args
            })
            .map(ast::Expr::Tuple);

        let atom = choice((
            immediate().map(ast::Expr::Immediate),
            unary_op(expr.clone()),
            code_block_expr(expr.clone()).map(ast::Expr::Block),
            if_expr(expr.clone()),
            call,
            tuple,
            ident().map(ast::Expr::Ident),
        ));

        comparison_op(additive_op(multiplicative_op(tuple_index(atom))))
    })
}

fn tuple_index<'sc, P>(
    parser: P,
) -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone
where
    P: Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
{
    let indices =
        filter_map(|span, token| match &token {
            Token::IntLiteral(num_str) => num_str
                .parse::<usize>()
                .map(|index| vec![index])
                .map_err(|_| ParseError::InvalidIntegerTupleIndex {
                    span,
                    index: num_str,
                }),

            // If the next token is of the form `<int>.<int>` which, to the lexer, looks like a real,
            // break it apart manually.
            Token::RealLiteral(num_str) => {
                match Regex::new(r"[0-9]+\.[0-9]+")
                    .expect("valid regex")
                    .captures(num_str)
                {
                    Some(_) => {
                        // Collect the spans for the two integers
                        let dot_index = num_str
                            .chars()
                            .position(|c| c == '.')
                            .expect("guaranteed by regex");
                        let spans = [
                            span.start..span.start + dot_index,
                            span.start + dot_index + 1..span.end,
                        ];

                        // Split at `.` then collect the two indices as `usize`. Report errors as
                        // needed
                        num_str
                            .split('.')
                            .zip(spans.iter())
                            .map(|(index, span)| {
                                index.parse::<usize>().map_err(|_| {
                                    ParseError::InvalidIntegerTupleIndex {
                                        span: span.clone(),
                                        index,
                                    }
                                })
                            })
                            .collect::<Result<Vec<usize>, _>>()
                    }
                    None => Err(ParseError::InvalidTupleIndex { span, index: token }),
                }
            }
            _ => Err(ParseError::InvalidTupleIndex { span, index: token }),
        });

    parser
        .then(just(Token::Dot).ignore_then(indices).repeated().flatten())
        .foldl(|expr, index| ast::Expr::TupleIndex {
            tuple: Box::new(expr),
            index,
        })
}

fn multiplicative_op<'sc, P>(
    parser: P,
) -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone
where
    P: Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
{
    parser
        .clone()
        .then(
            just(Token::Star)
                .to(ast::BinaryOp::Mul)
                .or(just(Token::Div).to(ast::BinaryOp::Div))
                .or(just(Token::Mod).to(ast::BinaryOp::Mod))
                .then(parser)
                .repeated(),
        )
        .foldl(|lhs, (op, rhs)| ast::Expr::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
}

fn additive_op<'sc, P>(
    parser: P,
) -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone
where
    P: Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
{
    parser
        .clone()
        .then(
            just(Token::Plus)
                .to(ast::BinaryOp::Add)
                .or(just(Token::Minus).to(ast::BinaryOp::Sub))
                .then(parser)
                .repeated(),
        )
        .foldl(|lhs, (op, rhs)| ast::Expr::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
}

fn comparison_op<'sc, P>(
    parser: P,
) -> impl Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone
where
    P: Parser<Token<'sc>, ast::Expr, Error = ParseError<'sc>> + Clone,
{
    parser
        .clone()
        .then(
            just(Token::Lt)
                .to(ast::BinaryOp::LessThan)
                .or(just(Token::Gt).to(ast::BinaryOp::GreaterThan))
                .or(just(Token::LtEq).to(ast::BinaryOp::LessThanOrEqual))
                .or(just(Token::GtEq).to(ast::BinaryOp::GreaterThanOrEqual))
                .or(just(Token::EqEq).to(ast::BinaryOp::Equal))
                .or(just(Token::NotEq).to(ast::BinaryOp::NotEqual))
                .then(parser)
                .repeated(),
        )
        .foldl(|lhs, (op, rhs)| ast::Expr::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
}

fn ident<'sc>() -> impl Parser<Token<'sc>, ast::Ident, Error = ParseError<'sc>> + Clone {
    filter_map(|span, token| match token {
        // Accept detected identifier. The lexer makes sure that these are not keywords.
        Token::Ident(id) => Ok(ast::Ident(id.to_owned())),

        // Tokens that represent keywords are not allowed
        _ if KEYWORDS.contains(&token) => Err(ParseError::KeywordAsIdent {
            span,
            keyword: token,
        }),

        // Other tokens are not allowed either
        _ => Err(ParseError::ExpectedFound {
            span,
            expected: vec![],
            found: Some(token),
        }),
    })
}

fn type_<'sc>() -> impl Parser<Token<'sc>, ast::Type, Error = ParseError<'sc>> + Clone {
    recursive(|type_| {
        let tuple = type_
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .delimited_by(just(Token::ParenOpen), just(Token::ParenClose))
            .validate(|args, span, emit| {
                if args.is_empty() {
                    emit(ParseError::EmptyTupleType { span })
                }
                args
            });

        choice((
            just(Token::Real).to(ast::Type::Real),
            just(Token::Int).to(ast::Type::Int),
            just(Token::Bool).to(ast::Type::Bool),
            just(Token::String).to(ast::Type::String),
            tuple.map(ast::Type::Tuple),
        ))
    })
}

fn immediate<'sc>() -> impl Parser<Token<'sc>, ast::Immediate, Error = ParseError<'sc>> + Clone {
    let integer_parser = |num_str: &str| {
        // Try an i64 first, if it'll fit, and return ast::Immediate::Int.
        num_str
            .parse::<i64>()
            .map(ast::Immediate::Int)
            .or_else(|_| {
                // Try a big-int if that fails and return an ast::Immedate::BigInt.  The BigInt
                // FromStr::from_str() isn't smart about radices though.
                use num_traits::Num;
                let (radix, offs) = match num_str.chars().nth(1) {
                    Some('b') => (2, 2),
                    Some('x') => (16, 2),
                    _ => (10, 0),
                };
                num_bigint::BigInt::from_str_radix(&num_str[offs..], radix)
                    .map(ast::Immediate::BigInt)
            })
            .unwrap()
    };

    select! {
        Token::RealLiteral(num_str) => ast::Immediate::Real(num_str.parse().unwrap()),
        Token::IntLiteral(num_str) => integer_parser(num_str),
        Token::True => ast::Immediate::Bool(true),
        Token::False => ast::Immediate::Bool(false),
        Token::StringLiteral(str_val) => ast::Immediate::String(str_val),
    }
}
