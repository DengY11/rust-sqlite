pub mod ast;
pub mod executor;
pub(crate) mod jsonb;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod plan;
pub mod planner;

pub use parser::parse_sql;

#[cfg(test)]
mod tests {
    use super::ast::Statement;
    use super::parse_sql;

    #[test]
    fn sql_module_reexports_parse_sql() {
        let statements = parse_sql("BEGIN;").unwrap();
        assert_eq!(
            statements,
            vec![Statement::Begin {
                isolation_level: None,
            }]
        );
    }
}
