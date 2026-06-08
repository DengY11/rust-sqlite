use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::common::types::{IndexMeta, RowId, Schema};

use super::page::{PageId, PageKind, decode_payload, encode_payload_page};
use super::pager::Pager;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogState {
    pub schemas: BTreeMap<String, Schema>,
    pub table_roots: BTreeMap<String, PageId>,
    pub next_row_ids: BTreeMap<String, u64>,
    pub indexes: BTreeMap<String, BTreeMap<String, IndexMeta>>,
    pub index_roots: BTreeMap<String, BTreeMap<String, PageId>>,
}

impl CatalogState {
    pub fn allocate_row_id(&mut self, table: &str) -> RowId {
        let next = self.next_row_ids.entry(table.to_string()).or_insert(1);
        let row_id = RowId(*next);
        *next += 1;
        row_id
    }
}

pub fn load_catalog(pager: &Pager) -> Result<CatalogState> {
    let page = pager.read_page(PageId(1))?;
    let payload = decode_payload(&page, PageKind::Catalog)?;
    if payload.is_empty() {
        Ok(CatalogState::default())
    } else {
        Ok(serde_json::from_slice(&payload)?)
    }
}

pub fn store_catalog(pager: &mut Pager, txn_id: u64, catalog: &CatalogState) -> Result<()> {
    let payload = serde_json::to_vec(catalog)?;
    let page = encode_payload_page(PageKind::Catalog, &payload)?;
    pager.write_page(txn_id, PageId(1), page)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::common::types::{ColumnDef, ColumnType, IndexMeta, Schema};

    use super::{CatalogState, load_catalog, store_catalog};
    use crate::storage::v2::page::PageId;
    use crate::storage::v2::pager::Pager;

    #[test]
    fn catalog_state_allocates_row_ids_monotonically() {
        let mut catalog = CatalogState::default();
        assert_eq!(catalog.allocate_row_id("users").0, 1);
        assert_eq!(catalog.allocate_row_id("users").0, 2);
        assert_eq!(catalog.allocate_row_id("orders").0, 1);
    }

    #[test]
    fn catalog_helpers_roundtrip_catalog_page_through_pager() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();

        let mut catalog = CatalogState::default();
        catalog.schemas.insert(
            "users".to_string(),
            Schema::new(
                "users",
                vec![ColumnDef::primary_key("id", ColumnType::Integer)],
            ),
        );
        catalog.table_roots.insert("users".to_string(), PageId(2));
        store_catalog(&mut pager, txn, &catalog).unwrap();
        pager.commit(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        let loaded = load_catalog(&reopened).unwrap();
        assert_eq!(loaded.schemas.len(), 1);
        assert_eq!(loaded.table_roots.get("users"), Some(&PageId(2)));
    }

    #[test]
    fn catalog_helpers_roundtrip_index_metadata_through_pager() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();

        let mut catalog = CatalogState::default();
        catalog
            .indexes
            .entry("users".to_string())
            .or_default()
            .insert(
                "idx_users_name_email".to_string(),
                IndexMeta {
                    name: "idx_users_name_email".to_string(),
                    columns: vec!["name".to_string(), "email".to_string()],
                    unique: false,
                },
            );
        catalog
            .index_roots
            .entry("users".to_string())
            .or_default()
            .insert("idx_users_name_email".to_string(), PageId(9));

        store_catalog(&mut pager, txn, &catalog).unwrap();
        pager.commit(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        let loaded = load_catalog(&reopened).unwrap();
        assert_eq!(
            loaded.indexes["users"]["idx_users_name_email"].columns,
            vec!["name", "email"]
        );
        assert_eq!(
            loaded.index_roots["users"]["idx_users_name_email"],
            PageId(9)
        );
    }
}
