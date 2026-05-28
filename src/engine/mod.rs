//! Shared engine-facing traits and identifiers.

pub mod traits;
pub mod txn;

pub use traits::{
    CatalogStore, IndexStore, PlanningStorageEngine, StorageEngine, TableStore, TransactionManager,
};
pub use txn::TransactionId;
