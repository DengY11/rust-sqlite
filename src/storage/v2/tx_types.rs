use std::collections::{BTreeSet, HashSet, VecDeque};

use crate::common::types::{RowId, Value};
use crate::engine::txn::TransactionId;
use crate::sql::ast::IsolationLevel;
use crate::storage::v2::page::PageId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    Active,
    Committed,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnSnapshot {
    pub visible_up_to: u64,
    pub active_txns: BTreeSet<TransactionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageWriteSetEntry {
    pub page_id: PageId,
    pub before_image: Option<Vec<u8>>,
    pub after_image: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoRecord {
    PageWrite {
        page_id: PageId,
    },
    InsertRow {
        table: String,
        row_id: RowId,
    },
    DeleteRow {
        table: String,
        row_id: RowId,
        previous_bytes: Vec<u8>,
    },
    IndexInsert {
        table: String,
        index: String,
        row_id: RowId,
        key: Vec<Value>,
    },
    IndexDelete {
        table: String,
        index: String,
        row_id: RowId,
        key: Vec<Value>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxnWriteSet {
    pub touched_pages: HashSet<PageId>,
    pub page_writes: Vec<PageWriteSetEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnState {
    pub id: TransactionId,
    pub isolation_level: IsolationLevel,
    pub status: TxnStatus,
    pub terminal_error: Option<String>,
    pub start_ts: u64,
    pub commit_ts: Option<u64>,
    pub snapshot: TxnSnapshot,
    pub write_set: TxnWriteSet,
    pub undo_records: Vec<UndoRecord>,
    pub purge_records: VecDeque<UndoRecord>,
}
