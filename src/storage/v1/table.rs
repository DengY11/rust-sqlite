use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::common::types::{Row, RowId};

use super::{read_json_if_exists, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRowRecord {
    pub row_id: RowId,
    pub row: Row,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableFile {
    pub next_row_id: u64,
    pub rows: Vec<TableRowRecord>,
}

impl Default for TableFile {
    fn default() -> Self {
        Self {
            next_row_id: 1,
            rows: Vec::new(),
        }
    }
}

pub fn tables_dir(base: &Path) -> PathBuf {
    base.join("tables")
}

pub fn table_path(base: &Path, table: &str) -> PathBuf {
    tables_dir(base).join(format!("{table}.json"))
}

pub fn load_table(base: &Path, table: &str) -> Result<TableFile> {
    read_json_if_exists(&table_path(base, table)).map(|value| value.unwrap_or_default())
}

pub fn save_table(base: &Path, table: &str, data: &TableFile) -> Result<()> {
    write_json_pretty(&table_path(base, table), data)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::common::types::{RowId, Value};

    use super::{TableFile, TableRowRecord, load_table, save_table, table_path, tables_dir};

    #[test]
    fn table_helpers_roundtrip_rows_and_paths() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        assert_eq!(tables_dir(base), base.join("tables"));
        assert_eq!(
            table_path(base, "users"),
            base.join("tables").join("users.json")
        );

        let table = TableFile {
            next_row_id: 2,
            rows: vec![TableRowRecord {
                row_id: RowId(1),
                row: vec![Value::Integer(1), Value::from("alice")],
            }],
        };
        save_table(base, "users", &table).unwrap();
        let loaded = load_table(base, "users").unwrap();
        assert_eq!(loaded.next_row_id, 2);
        assert_eq!(loaded.rows.len(), 1);
    }
}
