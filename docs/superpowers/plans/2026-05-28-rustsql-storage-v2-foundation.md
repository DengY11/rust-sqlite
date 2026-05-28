# RustSQL storage_v2 Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first real `storage_v2` backend with fixed-size pages, a minimal WAL, a RowId-keyed B+Tree table store, and transaction-safe persistence behind the existing `StorageEngine` traits.

**Architecture:** Keep the SQL, planner, executor, and engine trait surfaces stable. Implement a narrow but real vertical slice under `src/storage/v2/`: fixed 4 KiB pages, a meta page, append-only page allocation, WAL recovery, and one B+Tree per table keyed by `RowId`, while intentionally deferring secondary indexes so planner behavior stays predictable.

**Tech Stack:** Rust, std fs/io, serde/serde_json for catalog+row payload encoding, existing `StorageEngine` / `PlanningStorageEngine` traits, `cargo test` integration tests.

---

## File Structure

### Existing files to modify

- Modify: `/Users/bytedance/code/rustsql/src/storage/mod.rs`
  - Export the new `v2` backend alongside `memory` and `v1`.

### New files to create

- Create: `/Users/bytedance/code/rustsql/src/storage/v2/mod.rs`
  - Public backend entrypoint implementing `CatalogStore`, `TableStore`, `IndexStore`, `TransactionManager`, and `PlanningStorageEngine`.
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/page.rs`
  - Fixed-size page format, page ids, page kinds, meta page encoding/decoding.
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/wal.rs`
  - WAL header, frame format, commit marker, recovery scanner.
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/pager.rs`
  - Database file open/init, page read/write, dirty page tracking, commit/rollback plumbing.
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/btree.rs`
  - Minimal B+Tree supporting `get`, `insert`, `scan_all`, and simple leaf delete.
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/catalog.rs`
  - Persisted catalog model: schemas, table root pages, next row ids.
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/codec.rs`
  - Row and schema byte encoding helpers using `serde_json`.
- Create: `/Users/bytedance/code/rustsql/tests/storage_v2_tests.rs`
  - Backend-level and database-level persistence/recovery tests for `storage_v2`.

### Existing behavior to preserve

- Preserve executor/storage boundary in `/Users/bytedance/code/rustsql/src/sql/executor.rs:24`.
- Preserve planner index selection semantics in `/Users/bytedance/code/rustsql/src/sql/planner.rs:104` by returning no index metadata from `storage_v2` in this slice.
- Preserve trait signatures in `/Users/bytedance/code/rustsql/src/engine/traits.rs:8`.

---

### Task 1: Define page format and WAL record layout

**Files:**
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/page.rs`
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/wal.rs`
- Test: inline unit tests in both files

- [ ] **Step 1: Write the failing page-format tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_page_roundtrips_through_bytes() {
        let meta = MetaPage {
            page_size: PAGE_SIZE as u32,
            page_count: 3,
            catalog_page_id: PageId(1),
            next_txn_id: 9,
        };

        let encoded = meta.encode();
        let decoded = MetaPage::decode(&encoded).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn rejects_page_buffer_with_wrong_magic() {
        let mut bytes = [0_u8; PAGE_SIZE];
        bytes[..4].copy_from_slice(b"nope");
        let error = MetaPage::decode(&bytes).unwrap_err();
        assert!(error.to_string().contains("invalid storage_v2 page magic"));
    }
}
```

- [ ] **Step 2: Run the new page-format tests and confirm they fail**

Run: `cargo test storage_v2::page --lib -- --nocapture`

Expected: FAIL with unresolved module/type errors for `MetaPage`, `PageId`, or `PAGE_SIZE`.

- [ ] **Step 3: Add fixed-size page primitives and meta-page encoding**

```rust
pub const PAGE_SIZE: usize = 4096;
pub const STORAGE_MAGIC: &[u8; 4] = b"RSV2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Meta = 1,
    Leaf = 2,
    Internal = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaPage {
    pub page_size: u32,
    pub page_count: u32,
    pub catalog_page_id: PageId,
    pub next_txn_id: u64,
}
```

- [ ] **Step 4: Write the failing WAL tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::v2::page::PageId;

    #[test]
    fn wal_recovery_replays_only_committed_transactions() {
        let mut wal = Vec::new();
        write_frame(&mut wal, 7, PageId(2), &[1_u8; 4096]).unwrap();
        write_commit(&mut wal, 7, 1).unwrap();
        write_frame(&mut wal, 8, PageId(3), &[2_u8; 4096]).unwrap();

        let recovered = recover_frames(&wal).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].txn_id, 7);
        assert_eq!(recovered[0].page_id, PageId(2));
    }
}
```

- [ ] **Step 5: Run the WAL tests and confirm they fail**

Run: `cargo test storage_v2::wal --lib -- --nocapture`

Expected: FAIL with unresolved function/type errors for `write_frame`, `write_commit`, or `recover_frames`.

- [ ] **Step 6: Implement WAL frame and commit-marker encoding**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    pub txn_id: u64,
    pub page_id: PageId,
    pub page_bytes: Vec<u8>,
}

pub fn write_frame(out: &mut Vec<u8>, txn_id: u64, page_id: PageId, page: &[u8]) -> Result<()> {
    // frame type + txn id + page id + page bytes
    Ok(())
}

pub fn write_commit(out: &mut Vec<u8>, txn_id: u64, frame_count: u32) -> Result<()> {
    // commit marker with txn id + frame count
    Ok(())
}

pub fn recover_frames(bytes: &[u8]) -> Result<Vec<WalFrame>> {
    // only return frames belonging to transactions with a complete commit marker
    Ok(Vec::new())
}
```

- [ ] **Step 7: Re-run both low-level test groups**

Run: `cargo test storage_v2::page --lib -- --nocapture && cargo test storage_v2::wal --lib -- --nocapture`

Expected: PASS for page and WAL unit tests.

---

### Task 2: Build pager open/init/recovery and transactional dirty-page handling

**Files:**
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/pager.rs`
- Modify: `/Users/bytedance/code/rustsql/src/storage/v2/page.rs`
- Modify: `/Users/bytedance/code/rustsql/src/storage/v2/wal.rs`
- Test: inline unit tests in `/Users/bytedance/code/rustsql/src/storage/v2/pager.rs`

- [ ] **Step 1: Write the failing pager tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pager_initializes_new_database_and_reopens_meta_page() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");

        {
            let pager = Pager::open(&path).unwrap();
            let meta = pager.meta().unwrap();
            assert_eq!(meta.page_count, 2);
            assert_eq!(meta.catalog_page_id, PageId(1));
        }

        let reopened = Pager::open(&path).unwrap();
        let meta = reopened.meta().unwrap();
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.catalog_page_id, PageId(1));
    }

    #[test]
    fn rollback_discards_dirty_pages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let page_id = pager.allocate_page(txn).unwrap();
        pager.write_page(txn, page_id, vec![9_u8; PAGE_SIZE]).unwrap();
        pager.rollback(txn).unwrap();

        let reopened = Pager::open(&path).unwrap();
        assert!(reopened.read_page(page_id).is_err());
    }
}
```

- [ ] **Step 2: Run the pager tests and confirm they fail**

Run: `cargo test storage_v2::pager --lib -- --nocapture`

Expected: FAIL with unresolved `Pager` API.

- [ ] **Step 3: Implement `Pager::open()` with bootstrapped meta/catalog pages**

```rust
pub struct Pager {
    db_path: PathBuf,
    wal_path: PathBuf,
    meta: MetaPage,
    active_txn: Option<u64>,
    dirty_pages: BTreeMap<PageId, Vec<u8>>,
}

impl Pager {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        let wal_path = db_path.with_extension("wal");
        let meta = if db_path.exists() {
            Self::load_meta_and_recover(&db_path, &wal_path)?
        } else {
            Self::bootstrap_new_file(&db_path, &wal_path)?
        };

        Ok(Self {
            db_path,
            wal_path,
            meta,
            active_txn: None,
            dirty_pages: BTreeMap::new(),
        })
    }
}
```

- [ ] **Step 4: Implement begin/commit/rollback around dirty pages and WAL**

```rust
pub fn begin(&mut self) -> Result<u64> {
    // reject nested writers, reserve txn id from meta.next_txn_id
}

pub fn commit(&mut self, txn_id: u64) -> Result<()> {
    // append dirty pages to WAL, fsync WAL, write pages into DB, fsync DB, truncate WAL
}

pub fn rollback(&mut self, txn_id: u64) -> Result<()> {
    // discard dirty_pages and clear active_txn
}
```

- [ ] **Step 5: Add an explicit recovery test for committed WAL frames**

```rust
#[test]
fn open_replays_committed_wal_frames() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db.rsql");
    let mut pager = Pager::open(&path).unwrap();
    let txn = pager.begin().unwrap();
    let page_id = pager.allocate_page(txn).unwrap();
    pager.write_page(txn, page_id, vec![5_u8; PAGE_SIZE]).unwrap();
    pager.commit(txn).unwrap();

    let reopened = Pager::open(&path).unwrap();
    assert_eq!(reopened.read_page(page_id).unwrap()[0], 5);
}
```

- [ ] **Step 6: Re-run pager tests**

Run: `cargo test storage_v2::pager --lib -- --nocapture`

Expected: PASS for init, rollback, and recovery tests.

---

### Task 3: Implement the RowId-keyed B+Tree for table storage

**Files:**
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/btree.rs`
- Modify: `/Users/bytedance/code/rustsql/src/storage/v2/page.rs`
- Modify: `/Users/bytedance/code/rustsql/src/storage/v2/pager.rs`
- Test: inline unit tests in `/Users/bytedance/code/rustsql/src/storage/v2/btree.rs`

- [ ] **Step 1: Write the failing B+Tree tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn inserts_and_reads_values_from_a_single_leaf() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let mut tree = BTree::create(&mut pager, txn).unwrap();
        tree.insert(&mut pager, txn, 1, b"alice").unwrap();
        assert_eq!(tree.get(&pager, 1).unwrap(), Some(b"alice".to_vec()));
    }

    #[test]
    fn splits_leaf_pages_and_preserves_sorted_scan_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let mut tree = BTree::create(&mut pager, txn).unwrap();

        for key in 1..=64 {
            tree.insert(&mut pager, txn, key, format!("row-{key}").as_bytes()).unwrap();
        }

        let keys: Vec<u64> = tree.scan_all(&pager).unwrap().into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, (1..=64).collect::<Vec<_>>());
    }
}
```

- [ ] **Step 2: Run the B+Tree tests and confirm they fail**

Run: `cargo test storage_v2::btree --lib -- --nocapture`

Expected: FAIL with unresolved `BTree` API.

- [ ] **Step 3: Implement minimal B+Tree nodes and insert/get/scan operations**

```rust
pub struct BTree {
    root_page_id: PageId,
}

impl BTree {
    pub fn create(pager: &mut Pager, txn_id: u64) -> Result<Self> {
        let root_page_id = pager.allocate_leaf_page(txn_id)?;
        pager.write_leaf_node(txn_id, root_page_id, LeafNode::empty())?;
        Ok(Self { root_page_id })
    }

    pub fn get(&self, pager: &Pager, key: u64) -> Result<Option<Vec<u8>>> {
        let leaf = self.find_leaf(pager, self.root_page_id, key)?;
        Ok(leaf.lookup(key).cloned())
    }

    pub fn insert(&mut self, pager: &mut Pager, txn_id: u64, key: u64, value: &[u8]) -> Result<()> {
        if let Some(split) = self.insert_into_page(pager, txn_id, self.root_page_id, key, value)? {
            let new_root = pager.allocate_internal_page(txn_id)?;
            let root = InternalNode::from_split(self.root_page_id, split.separator_key, split.right_page_id);
            pager.write_internal_node(txn_id, new_root, root)?;
            self.root_page_id = new_root;
        }
        Ok(())
    }

    pub fn scan_all(&self, pager: &Pager) -> Result<Vec<(u64, Vec<u8>)>> {
        let first_leaf = self.leftmost_leaf(pager, self.root_page_id)?;
        self.scan_from_leaf_chain(pager, first_leaf)
    }
}
```

- [ ] **Step 4: Add a simple delete test and implementation without rebalance**

```rust
#[test]
fn delete_removes_visible_key_without_rebalancing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db.rsql");
    let mut pager = Pager::open(&path).unwrap();
    let txn = pager.begin().unwrap();
    let mut tree = BTree::create(&mut pager, txn).unwrap();
    tree.insert(&mut pager, txn, 7, b"carol").unwrap();
    tree.delete(&mut pager, txn, 7).unwrap();
    assert_eq!(tree.get(&pager, 7).unwrap(), None);
}
```

- [ ] **Step 5: Re-run the B+Tree tests**

Run: `cargo test storage_v2::btree --lib -- --nocapture`

Expected: PASS for single-leaf, split, scan, and delete tests.

---

### Task 4: Add catalog/codec layers and bridge `storage_v2` to existing traits

**Files:**
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/catalog.rs`
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/codec.rs`
- Create: `/Users/bytedance/code/rustsql/src/storage/v2/mod.rs`
- Modify: `/Users/bytedance/code/rustsql/src/storage/mod.rs`
- Test: `/Users/bytedance/code/rustsql/tests/storage_v2_tests.rs`

- [ ] **Step 1: Write the failing storage_v2 integration tests**

```rust
use rustsql::common::types::{RowId, Schema};
use rustsql::common::types::{ColumnDef, ColumnType, Value};
use rustsql::engine::{CatalogStore, PlanningStorageEngine, TableStore, TransactionManager};
use rustsql::storage::v2::FileStorage;
use tempfile::tempdir;

#[test]
fn storage_v2_persists_schema_and_rows_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage.create_schema(
            txn,
            rustsql::common::types::Schema::new(
                "users",
                vec![
                    ColumnDef::primary_key("id", ColumnType::Integer),
                    ColumnDef::new("name", ColumnType::Text).nullable(false),
                ],
            ),
        ).unwrap();
        storage.insert_row(txn, "users", vec![Value::Integer(1), Value::from("alice")]).unwrap();
        storage.commit(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert!(reopened.get_schema(txn, "users").unwrap().is_some());
    assert_eq!(reopened.scan_rows(txn, "users").unwrap().len(), 1);
    reopened.rollback(txn).unwrap();
}

#[test]
fn storage_v2_rollback_discards_uncommitted_rows_after_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let storage = FileStorage::open(&path).unwrap();
        let txn = storage.begin().unwrap();
        storage.create_schema(txn, Schema::new(
            "users",
            vec![ColumnDef::primary_key("id", ColumnType::Integer)],
        )).unwrap();
        storage.insert_row(txn, "users", vec![Value::Integer(1)]).unwrap();
        storage.rollback(txn).unwrap();
    }

    let reopened = FileStorage::open(&path).unwrap();
    let txn = reopened.begin().unwrap();
    assert!(reopened.get_schema(txn, "users").unwrap().is_none());
    assert!(reopened.scan_rows(txn, "users").unwrap().is_empty());
    reopened.rollback(txn).unwrap();
}

#[test]
fn storage_v2_planning_context_exposes_schema_without_indexes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    let storage = FileStorage::open(&path).unwrap();
    let txn = storage.begin().unwrap();
    storage.create_schema(txn, Schema::new(
        "users",
        vec![ColumnDef::primary_key("id", ColumnType::Integer)],
    )).unwrap();

    let context = storage.planning_context_snapshot(Some(txn)).unwrap();
    assert!(context.schema("users").is_some());
    assert!(context.indexes_for("users").is_empty());

    storage.rollback(txn).unwrap();
}
```

- [ ] **Step 2: Run the storage_v2 test target and confirm it fails**

Run: `cargo test --test storage_v2_tests -- --nocapture`

Expected: FAIL with unresolved module/type errors for `rustsql::storage::v2`.

- [ ] **Step 3: Implement row/schema codec helpers using stable JSON payloads**

```rust
pub fn encode_row(row: &Row) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(row)?)
}

pub fn decode_row(bytes: &[u8]) -> Result<Row> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_schema(schema: &Schema) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(schema)?)
}

pub fn decode_schema(bytes: &[u8]) -> Result<Schema> {
    Ok(serde_json::from_slice(bytes)?)
}
```

- [ ] **Step 4: Implement persisted catalog state and table root lookup**

```rust
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
```

- [ ] **Step 5: Implement `storage_v2::FileStorage` behind the current traits**

```rust
#[derive(Debug)]
pub struct FileStorage {
    pager: RefCell<Pager>,
    catalog: RefCell<CatalogState>,
    active_txn: Cell<Option<TransactionId>>,
}

impl PlanningStorageEngine for FileStorage {
    fn planning_context_snapshot(&self, transaction_id: Option<TransactionId>) -> Result<PlanningContext> {
        self.validate_snapshot_transaction(transaction_id)?;
        let catalog = self.catalog.borrow();
        let schemas = catalog.schemas.clone().into_iter().collect::<HashMap<_, _>>();
        let indexes = catalog
            .schemas
            .keys()
            .map(|table| (table.clone(), Vec::new()))
            .collect::<HashMap<_, _>>();
        Ok(PlanningContext::new(schemas, indexes))
    }
}

impl IndexStore for FileStorage {
    fn create_index(&self, _tx: TransactionId, _table: &str, _index: IndexMeta) -> Result<()> {
        Err(DbError::storage("storage_v2 does not implement secondary indexes yet"))
    }

    fn get_index(&self, _tx: TransactionId, _table: &str, _name: &str) -> Result<Option<IndexMeta>> {
        Ok(None)
    }

    fn list_indexes(&self, _tx: TransactionId, _table: &str) -> Result<Vec<IndexMeta>> {
        Ok(Vec::new())
    }

    fn lookup_index(&self, _tx: TransactionId, _table: &str, _name: &str, _key: &[Value]) -> Result<Vec<RowId>> {
        Err(DbError::storage("storage_v2 does not implement secondary indexes yet"))
    }
}
```

- [ ] **Step 6: Export the module**

Update `/Users/bytedance/code/rustsql/src/storage/mod.rs` to:

```rust
pub mod memory;
pub mod v1;
pub mod v2;
```

- [ ] **Step 7: Re-run the storage_v2 test target**

Run: `cargo test --test storage_v2_tests -- --nocapture`

Expected: PASS for persistence, rollback, `get_row`, and planning-context coverage.

---

### Task 5: Verify database-level integration without changing SQL/planner/executor

**Files:**
- Modify: `/Users/bytedance/code/rustsql/tests/storage_v2_tests.rs`
- Test: `/Users/bytedance/code/rustsql/tests/storage_v2_tests.rs`

- [ ] **Step 1: Add database-level tests using `Database::with_storage(...)`**

```rust
use rustsql::db::Database;
use rustsql::storage::v2;

#[test]
fn database_with_storage_v2_runs_create_insert_select_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");

    {
        let db = Database::with_storage(v2::FileStorage::open(&path).unwrap());
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'alice');").unwrap();
        assert_eq!(db.query("SELECT * FROM users WHERE id = 1;").unwrap().len(), 1);
    }

    let reopened = Database::with_storage(v2::FileStorage::open(&path).unwrap());
    assert_eq!(reopened.query("SELECT name FROM users WHERE id = 1;").unwrap().len(), 1);
}

#[test]
fn database_with_storage_v2_rejects_create_index_for_now() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::with_storage(v2::FileStorage::open(&path).unwrap());

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
    let error = db.execute("CREATE INDEX idx_users_name ON users (name);").unwrap_err();
    assert!(error.to_string().contains("secondary indexes"));
}
```

- [ ] **Step 2: Run the focused database-level tests and confirm behavior**

Run: `cargo test --test storage_v2_tests database_with_storage_v2 -- --nocapture`

Expected: PASS with no changes required in `/Users/bytedance/code/rustsql/src/sql/executor.rs:24` or `/Users/bytedance/code/rustsql/src/sql/planner.rs:38`.

- [ ] **Step 3: Run the full test suite as the final verification gate**

Run: `cargo test -- --nocapture`

Expected: PASS for all existing parser/planner/executor/storage tests plus the new `storage_v2` coverage.

---

## Notes and guardrails for implementation

- Do **not** change `StorageEngine` trait signatures in `/Users/bytedance/code/rustsql/src/engine/traits.rs:8` during this slice.
- Do **not** change planner index selection logic in `/Users/bytedance/code/rustsql/src/sql/planner.rs:104`; instead, keep `storage_v2` index metadata empty so planner stays on `SeqScan`.
- Do **not** switch `/Users/bytedance/code/rustsql/src/db.rs:130` from `v1` to `v2` yet. Phase 2 foundation should be introduced behind `Database::with_storage(...)` first.
- Reuse `Schema::validate_row_values()` and `Schema::validate_primary_key_uniqueness()` from `/Users/bytedance/code/rustsql/src/common/types.rs:155` before mutating tree state.
- Keep WAL intentionally simple: single-process, single active write transaction, append frames, recover only committed transactions, truncate after checkpoint.
- Keep B+Tree deletion intentionally shallow: remove visible leaf entry; do not implement merge/rebalance in this slice.

## Minimal acceptance checklist

- `storage_v2` persists schemas and rows across reopen.
- Rollback discards uncommitted page and row changes.
- WAL recovery replays committed frames and ignores torn/uncommitted tail data.
- `scan_rows()` returns rows in `RowId` order.
- `planning_context_snapshot()` exposes schemas and no indexes.
- `CREATE INDEX` through `storage_v2` fails clearly instead of pretending support.
- Existing non-v2 tests still pass unchanged.
