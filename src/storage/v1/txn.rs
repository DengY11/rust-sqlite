use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::engine::txn::TransactionId;

use super::{remove_file_if_exists, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTransactionFile {
    pub transaction_id: TransactionId,
}

pub fn txn_dir(base: &Path) -> PathBuf {
    base.join("txn")
}

pub fn active_txn_path(base: &Path) -> PathBuf {
    txn_dir(base).join("active.json")
}

pub fn write_active_txn(base: &Path, transaction_id: TransactionId) -> Result<()> {
    write_json_pretty(
        &active_txn_path(base),
        &ActiveTransactionFile { transaction_id },
    )
}

pub fn clear_active_txn(base: &Path) -> Result<()> {
    remove_file_if_exists(&active_txn_path(base))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::engine::txn::TransactionId;

    use super::{active_txn_path, clear_active_txn, txn_dir, write_active_txn};

    #[test]
    fn active_transaction_helpers_write_and_clear_metadata_file() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        assert_eq!(txn_dir(base), base.join("txn"));
        assert_eq!(active_txn_path(base), base.join("txn").join("active.json"));

        write_active_txn(base, TransactionId(11)).unwrap();
        assert!(active_txn_path(base).exists());
        clear_active_txn(base).unwrap();
        assert!(!active_txn_path(base).exists());
    }
}
