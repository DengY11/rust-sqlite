pub mod error;
pub mod types;

#[cfg(test)]
mod tests {
    use crate::common::error::DbError;
    use crate::common::types::Value;

    #[test]
    fn common_module_reexports_submodules() {
        let error = DbError::sql("x");
        let value = Value::from("hello");
        assert_eq!(error.to_string(), "sql error: x");
        assert_eq!(value.to_string(), "hello");
    }
}
