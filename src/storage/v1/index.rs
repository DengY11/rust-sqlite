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
