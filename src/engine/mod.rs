//! Shared engine-facing traits and identifiers.

pub mod traits;
pub mod txn;

pub use traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, StorageEngine, TableStore, TransactionManager,
};
pub use txn::TransactionId;

#[cfg(test)]
mod tests {
    use crate::engine::TransactionId;

    #[test]
    fn engine_module_reexports_transaction_id_and_traits_surface() {
        let transaction_id = TransactionId(7);
        let _catalog_store_name = std::any::type_name::<dyn crate::engine::CatalogStore>();
        let _storage_engine_name = std::any::type_name::<dyn crate::engine::StorageEngine>();
        assert_eq!(transaction_id.0, 7);
    }
}
