use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::common::types::Schema;

use super::{read_json_if_exists, write_json_pretty};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogFile {
    pub schemas: BTreeMap<String, Schema>,
}

pub fn catalog_path(base: &Path) -> PathBuf {
    base.join("catalog.json")
}

pub fn load_catalog(base: &Path) -> Result<CatalogFile> {
    read_json_if_exists(&catalog_path(base)).map(|value| value.unwrap_or_default())
}

pub fn save_catalog(base: &Path, catalog: &CatalogFile) -> Result<()> {
    write_json_pretty(&catalog_path(base), catalog)
}
