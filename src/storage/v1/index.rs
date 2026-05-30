use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::common::types::{IndexMeta, RowId, Value};

use super::{read_json_if_exists, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntryFile {
    pub key: Vec<Value>,
    pub row_ids: Vec<RowId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexFile {
    pub meta: IndexMeta,
    pub entries: Vec<IndexEntryFile>,
}

pub fn indexes_dir(base: &Path) -> PathBuf {
    base.join("indexes")
}

pub fn table_indexes_dir(base: &Path, table: &str) -> PathBuf {
    indexes_dir(base).join(table)
}

pub fn index_path(base: &Path, table: &str, index: &str) -> PathBuf {
    table_indexes_dir(base, table).join(format!("{index}.json"))
}

pub fn load_index(base: &Path, table: &str, index: &str) -> Result<Option<IndexFile>> {
    read_json_if_exists(&index_path(base, table, index))
}

pub fn save_index(base: &Path, table: &str, index: &str, data: &IndexFile) -> Result<()> {
    write_json_pretty(&index_path(base, table, index), data)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::common::types::{IndexMeta, RowId, Value};

    use super::{
        IndexEntryFile, IndexFile, index_path, indexes_dir, load_index, save_index,
        table_indexes_dir,
    };

    #[test]
    fn index_helpers_build_paths_and_roundtrip_index_files() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        assert_eq!(indexes_dir(base), base.join("indexes"));
        assert_eq!(
            table_indexes_dir(base, "users"),
            base.join("indexes").join("users")
        );
        assert_eq!(
            index_path(base, "users", "idx_users_name"),
            base.join("indexes")
                .join("users")
                .join("idx_users_name.json")
        );

        let data = IndexFile {
            meta: IndexMeta {
                name: "idx_users_name".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
            },
            entries: vec![IndexEntryFile {
                key: vec![Value::from("alice")],
                row_ids: vec![RowId(1)],
            }],
        };
        save_index(base, "users", "idx_users_name", &data).unwrap();
        assert_eq!(
            load_index(base, "users", "idx_users_name")
                .unwrap()
                .unwrap()
                .entries
                .len(),
            1
        );
    }
}
