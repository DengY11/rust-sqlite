# SQLite 3.46 Compatibility Phase 1 Design

## Goal

Add a new storage engine that can read and write real SQLite 3 database files with a narrow, testable compatibility surface. Phase 1 targets SQLite 3.46.x style on-disk compatibility for common rowid tables and basic indexes, while reusing the existing SQL parser, planner, and executor where possible.

## Non-Goals

- Full SQLite feature parity in one step
- WAL-mode write compatibility in phase 1
- `WITHOUT ROWID` tables
- Virtual tables, triggers, FTS, RTree, JSON extensions, or loadable extensions
- Full pragma compatibility
- Replacing the current planner/executor in this phase

## Why A New Engine

The current `storage::v2` engine is page-based, but it is not SQLite-compatible:

- page magic is custom (`RSV2`)
- catalog metadata is custom rather than `sqlite_schema`
- page payload encoding is custom rather than SQLite btree/cell/record encoding
- WAL framing is custom rather than SQLite rollback journal or SQLite WAL format

Continuing to evolve `storage::v2` toward SQLite compatibility would create a long-lived translation layer and repeated semantic mismatches. A new `storage::sqlite3` engine gives a clean compatibility boundary and leaves `v2` available as an experimental engine.

## User-Visible Outcome

After phase 1:

- `rustsql` can open a real SQLite `.db` file created by the system `sqlite3` CLI.
- `rustsql` can read table definitions and ordinary index definitions from `sqlite_schema`.
- `rustsql` can scan and query data stored in ordinary rowid tables.
- `rustsql` can create a new SQLite-format database file and insert basic rows that the system `sqlite3` CLI can later read.
- The existing SQL layer continues to drive reads and simple writes through a new SQLite-compatible storage backend.

## Scope

### In Scope

- SQLite database header parsing and validation
- Page-size-aware file IO
- SQLite varint codec
- SQLite record header and serial type codec
- SQLite btree page parsing for table and index leaf/interior pages
- Overflow page traversal for large payloads
- `sqlite_schema` loading into an internal planning/catalog representation
- Table full scan
- Row lookup by rowid
- Basic secondary index lookup and ordered index scan
- Creating a new SQLite-format database with `sqlite_schema`
- Basic writes for `CREATE TABLE`, `CREATE INDEX`, and `INSERT`
- Cross-validation against the system `sqlite3` CLI

### Deferred To Later Phases

- rollback journal format compatibility beyond the minimum safe write path
- SQLite WAL format compatibility
- autovacuum and freelist trunk/leaf recycling completeness
- `WITHOUT ROWID`
- advanced ALTER TABLE behavior
- exact query planner parity
- all SQLite built-in functions and coercion edge cases

## Architecture

### New Module Layout

Add a new engine family under `src/storage/sqlite3/`.

Planned modules:

- `mod.rs`
  - public engine entrypoint implementing the existing storage traits
- `file_header.rs`
  - database header parsing, validation, and encoding
- `page.rs`
  - page kinds, shared offsets, cell pointer accessors, freeblock helpers
- `varint.rs`
  - SQLite varint encode/decode helpers
- `record.rs`
  - record header parsing, serial type handling, row/value conversion
- `btree.rs`
  - table/index page decoding, cursor traversal, cell access
- `overflow.rs`
  - overflow chain read/write support
- `schema.rs`
  - `sqlite_schema` loading and translation into internal schema/index metadata
- `pager.rs`
  - page reads/writes against SQLite file layout
- `engine.rs`
  - trait adapter that exposes scans/lookups/inserts to the existing executor

If the implementation grows, `cursor.rs` can be split out from `btree.rs`.

### Integration Strategy

The existing parser, planner, optimizer, and executor remain in place. Phase 1 changes storage integration only:

- `Database::open` and REPL engine selection gain a SQLite-compatible engine option.
- The new engine implements the same trait surface expected by the SQL layer.
- Planning metadata comes from `sqlite_schema`, translated into the current `Schema` and `IndexMeta` types.

This preserves momentum and avoids rewriting SQL behavior while the file-format work is still stabilizing.

## Data Model Mapping

### Catalog Source

SQLite stores schema objects in `sqlite_schema`. Phase 1 will:

- read `type`, `name`, `tbl_name`, `rootpage`, and `sql`
- parse supported `CREATE TABLE` and `CREATE INDEX` statements using the existing parser when possible
- build internal table and index metadata from parsed SQL plus root page ids

Unsupported schema objects in phase 1:

- triggers
- views that require semantic expansion
- virtual table modules

These objects must be either ignored safely when irrelevant to execution or rejected with explicit unsupported errors.

### Row Storage

For ordinary rowid tables:

- btree key maps to SQLite rowid
- btree payload maps to SQLite record bytes
- row decoding converts SQLite serial types into existing `Value` variants

For ordinary secondary indexes:

- index key bytes encode indexed column values using SQLite index-record ordering
- payload carries the rowid

## Compatibility Boundaries

### Guaranteed In Phase 1

- real SQLite 3 header compatibility
- readable and writable rowid-table files
- readable and writable ordinary secondary indexes
- cross-tool interoperability with `sqlite3` for the covered features

### Not Guaranteed In Phase 1

- byte-for-byte file identity with SQLite output
- planner choice parity
- exact lock-state behavior under concurrent multi-process writers
- all corner-case affinity and collation behavior

The target is interoperability and correct observable behavior for the covered subset, not implementation identity.

## Write Path Strategy

Phase 1 should use the simplest durable write path that does not block later evolution:

- begin with direct database-file page writes guarded by explicit flush ordering in tests
- structure pager code so rollback-journal support can be inserted without changing btree/record code
- do not implement SQLite WAL-mode writes in phase 1

This keeps the initial write path understandable. The pager must be designed with explicit hooks for:

- pre-write page capture
- atomic commit boundary
- future rollback journal generation

## Error Handling

Errors should be explicit and classified as:

- corrupted SQLite file
- unsupported SQLite feature in phase 1
- unsupported schema object
- type/record decode failure
- planner/executor request not yet supported by the new engine

Corruption and unsupported-feature paths must never silently degrade into wrong results.

## Testing Strategy

Phase 1 is test-driven. Every new primitive and engine step gets red-green coverage.

### Unit Tests

- varint roundtrip and boundary values
- record serial type encode/decode
- file header parsing/encoding
- page header and cell-pointer parsing
- overflow-chain reconstruction
- table/index btree page decoding from synthetic pages

### Integration Tests

- create a database with system `sqlite3`, then open it from `rustsql`
- create a database with `rustsql`, then query it from system `sqlite3`
- schema roundtrip through `sqlite_schema`
- rowid scans, rowid lookup, and index lookup on real SQLite files

### Behavior Tests

- `CREATE TABLE`, `CREATE INDEX`, `INSERT`, `SELECT` on the new engine
- mixed text/integer/null records
- larger records that force overflow pages

### Golden/Fixture Tests

Keep a small set of generated fixture databases under test temp directories rather than hand-maintained binary fixtures where possible. Generate them via `sqlite3` inside tests or setup helpers so the source of truth remains obvious.

## Delivery Plan

Phase 1 implementation is split into four sub-projects:

1. SQLite file primitives
2. SQLite btree and schema reader
3. SQLite-compatible storage engine
4. Behavior alignment for the covered subset

Only sub-projects 1 through 3 are required to claim phase 1 storage compatibility complete.

## Acceptance Criteria

Phase 1 is complete when all of the following are true:

- `rustsql` can open a database created by system `sqlite3` and correctly list/read covered tables and indexes.
- `rustsql` can create a new SQLite-format database file that system `sqlite3` opens without repair or warnings.
- `CREATE TABLE`, `CREATE INDEX`, `INSERT`, and `SELECT` succeed on the new engine for covered rowid-table cases.
- Tests demonstrate successful cross-read/write interoperability in both directions.
- Unsupported features fail explicitly rather than producing partial or silent behavior.

## Risks

### Affinity And Comparison Drift

The current executor/value model may not exactly match SQLite affinity, comparison, and collation rules. Phase 1 should document mismatches and keep tests focused on the supported subset.

### Executor Assumptions

The executor currently assumes internal catalog and row representations that were designed for the existing engines. The adapter layer in `storage::sqlite3` must absorb these differences cleanly rather than leaking SQLite layout details upward.

### Write Safety

Phase 1 write support must stay conservative. If durable multi-step commit safety becomes unclear, reduce the initial write scope rather than shipping a corrupting path.

## Open Decisions Resolved

- Compatibility baseline: SQLite 3.46.x semantics and on-disk format target
- Storage approach: new `storage::sqlite3` engine, not a retrofit of `storage::v2`
- Phase 1 table model: rowid tables only
- WAL-mode writes: deferred
- Existing SQL stack: retained for phase 1
