use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::common::types::{RowId, Schema};

use super::page::{PageId, PageKind, decode_payload, encode_payload_page};
use super::pager::Pager;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogState {
    pub schemas: BTreeMap<String, Schema>,
    pub table_roots: BTreeMap<String, PageId>,
    pub next_row_ids: BTreeMap<String, u64>,
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
