pub mod common;
pub mod db;
pub mod engine;
pub mod repl;
pub mod sql;
pub mod storage;

#[cfg(test)]
mod tests {
    use crate::common::types::Value;
    use crate::sql::ast::Statement;

    #[test]
    fn library_root_exposes_primary_modules() {
        let value = Value::Integer(42);
        let statement = Statement::Begin;
        assert_eq!(value.to_string(), "42");
        assert_eq!(format!("{statement:?}"), "Begin");
    }
}
