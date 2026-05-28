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
