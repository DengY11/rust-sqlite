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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use crate::common::types::{ColumnDef, ColumnType, Schema};

    use super::{CatalogFile, catalog_path, load_catalog, save_catalog};

    #[test]
    fn catalog_helpers_roundtrip_and_use_expected_path() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        assert_eq!(catalog_path(base), base.join("catalog.json"));

        let mut schemas = BTreeMap::new();
        schemas.insert(
            "users".to_string(),
            Schema::new(
                "users",
                vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            ),
        );
        let catalog = CatalogFile { schemas };
        save_catalog(base, &catalog).unwrap();
        assert_eq!(load_catalog(base).unwrap().schemas.len(), 1);
    }
}
