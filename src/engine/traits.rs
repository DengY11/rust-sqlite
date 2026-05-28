//! Storage engine traits shared by future backends.

use crate::common::error::Result;
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::txn::TransactionId;
use crate::sql::planner::PlanningContext;

pub trait CatalogStore {
    fn create_schema(&self, transaction_id: TransactionId, schema: Schema) -> Result<()>;
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
}

pub trait IndexStore {
    fn create_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index: IndexMeta,
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
    fn lookup_index(
        &self,
        transaction_id: TransactionId,
        schema_name: &str,
        index_name: &str,
        key: &[Value],
    ) -> Result<Vec<RowId>>;
}

pub trait TransactionManager {
    fn begin(&self) -> Result<TransactionId>;
    fn commit(&self, transaction_id: TransactionId) -> Result<()>;
    fn rollback(&self, transaction_id: TransactionId) -> Result<()>;
}

pub trait StorageEngine: CatalogStore + TableStore + IndexStore + TransactionManager {}

impl<T> StorageEngine for T where T: CatalogStore + TableStore + IndexStore + TransactionManager {}

pub trait PlanningStorageEngine: StorageEngine {
    fn planning_context_snapshot(
        &self,
        transaction_id: Option<TransactionId>,
    ) -> Result<PlanningContext>;
}
