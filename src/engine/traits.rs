//! Storage engine traits shared by future backends.

use std::path::PathBuf;

use crate::common::error::Result;
use crate::common::types::{ColumnDef, IndexMeta, Row, RowId, Schema, Value};
use crate::engine::txn::TransactionId;
use crate::sql::ast::CompareOp;
use crate::sql::ast::IsolationLevel;
use crate::sql::planner::PlanningContext;

pub trait CatalogStore {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()>;
    fn create_trigger(
        &self,
        _transaction_id: TransactionId,
        name: &str,
        _table: &str,
        _sql: &str,
    ) -> Result<()> {
        Err(crate::common::error::DbError::storage(format!(
            "triggers are not supported by this storage engine: {name}"
        )))
    }
    fn drop_schema(&self, transaction_id: TransactionId, name: &str) -> Result<()>;
    fn drop_trigger(&self, _transaction_id: TransactionId, name: &str) -> Result<()> {
        Err(crate::common::error::DbError::storage(format!(
            "unknown trigger: {name}"
        )))
    }
    fn replace_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()>;
    fn rename_schema(
        &self,
        transaction_id: TransactionId,
        old_name: &str,
        new_name: &str,
    ) -> Result<()>;
    fn add_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        column: ColumnDef,
    ) -> Result<()>;
    fn rename_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<()>;
    fn drop_column(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        old_name: &str,
    ) -> Result<()>;
    fn get_schema(&self, transaction_id: TransactionId, name: &str) -> Result<Option<Schema>>;
    fn list_schemas(&self, transaction_id: TransactionId) -> Result<Vec<Schema>>;
}

pub trait TableStore {
    fn insert_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row: Row,
    ) -> Result<RowId>;
    fn get_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<Option<Row>>;
    fn scan_rows(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<(RowId, Row)>>;
    fn delete_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
    ) -> Result<()>;
    fn update_row(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
        row: Row,
    ) -> Result<()>;
    fn update_row_with_columns(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        row_id: RowId,
        row: Row,
        _updated_columns: &[String],
    ) -> Result<()> {
        self.update_row(transaction_id, schema_name, row_id, row)
    }
}

pub trait IndexStore {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index: IndexMeta,
    ) -> Result<()>;
    fn drop_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<()>;
    fn get_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
    ) -> Result<Option<IndexMeta>>;
    fn list_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>>;
    fn list_all_indexes(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
    ) -> Result<Vec<IndexMeta>> {
        self.list_indexes(transaction_id, schema_name)
    }
    fn lookup_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key: &[Value],
    ) -> Result<Vec<RowId>>;
    fn scan_index_prefix(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
    ) -> Result<Vec<RowId>>;
    fn scan_index_range(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<Vec<RowId>>;
}

pub trait TransactionManager {
    fn begin(&self) -> Result<TransactionId>;
    fn begin_with_isolation(&self, _isolation_level: IsolationLevel) -> Result<TransactionId> {
        self.begin()
    }
    fn commit(&self, transaction_id: TransactionId) -> Result<()>;
    fn rollback(&self, transaction_id: TransactionId) -> Result<()>;
    fn savepoint(&self, _transaction_id: TransactionId, _name: &str) -> Result<()> {
        Err(crate::common::error::DbError::txn(
            "savepoints are not supported by this storage engine",
        ))
    }
    fn rollback_to_savepoint(&self, _transaction_id: TransactionId, _name: &str) -> Result<()> {
        Err(crate::common::error::DbError::txn(
            "savepoints are not supported by this storage engine",
        ))
    }
    fn release_savepoint(&self, _transaction_id: TransactionId, _name: &str) -> Result<()> {
        Err(crate::common::error::DbError::txn(
            "savepoints are not supported by this storage engine",
        ))
    }
}

pub trait StorageEngine: CatalogStore + TableStore + IndexStore + TransactionManager {}

impl<T> StorageEngine for T where T: CatalogStore + TableStore + IndexStore + TransactionManager {}

pub trait PlanningStorageEngine: StorageEngine {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext>;

    fn database_path(&self) -> Option<PathBuf> {
        None
    }

    fn journal_mode(&self) -> &'static str {
        "memory"
    }

    fn ignore_check_constraints(&self) -> bool {
        false
    }

    fn set_ignore_check_constraints(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }

    fn case_sensitive_like(&self) -> bool {
        false
    }

    fn set_case_sensitive_like(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }

    fn database_page_size(&self) -> u32 {
        4096
    }

    fn database_page_count(&self) -> Result<u32> {
        Ok(0)
    }

    fn database_freelist_count(&self) -> Result<u32> {
        Ok(0)
    }

    fn user_version(&self) -> Result<u32> {
        Ok(0)
    }

    fn set_user_version(&self, _version: u32) -> Result<()> {
        Ok(())
    }

    fn application_id(&self) -> Result<u32> {
        Ok(0)
    }

    fn set_application_id(&self, _application_id: u32) -> Result<()> {
        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        Ok(0)
    }

    fn set_schema_version(&self, _schema_version: u32) -> Result<()> {
        Ok(())
    }

    fn increment_schema_version(&self) -> Result<()> {
        let next = self.schema_version()?.wrapping_add(1);
        self.set_schema_version(next)
    }
}
