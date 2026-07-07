use std::collections::BTreeMap;

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Schema, Value};
use crate::sql::ast::{Statement, TableConstraint};
use crate::sql::parse_sql;

use super::page::{BtreePageHeader, PageType};
use super::pager::Pager;
use super::varint::decode_varint;

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    schemas: BTreeMap<String, Schema>,
    table_root_pages: BTreeMap<String, u32>,
    indexes: BTreeMap<String, BTreeMap<String, IndexMeta>>,
    index_root_pages: BTreeMap<String, BTreeMap<String, u32>>,
    sqlite_sequence_root_page: Option<u32>,
}

impl Catalog {
    #[must_use]
    pub fn schemas(&self) -> &BTreeMap<String, Schema> {
        &self.schemas
    }

    #[must_use]
    pub fn indexes(&self) -> &BTreeMap<String, BTreeMap<String, IndexMeta>> {
        &self.indexes
    }

    #[must_use]
    pub fn table_root_page(&self, table: &str) -> Option<u32> {
        self.table_root_pages.get(table).copied()
    }

    #[must_use]
    pub fn index_root_page(&self, table: &str, index: &str) -> Option<u32> {
        self.index_root_pages
            .get(table)
            .and_then(|pages| pages.get(index))
            .copied()
    }

    #[must_use]
    pub fn sqlite_sequence_root_page(&self) -> Option<u32> {
        self.sqlite_sequence_root_page
    }
}

#[derive(Debug)]
struct SchemaRow {
    entry_type: String,
    name: String,
    table_name: String,
    root_page: u32,
    sql: Option<String>,
}

#[derive(Debug)]
enum RecordValue {
    Null,
    Integer(i64),
    Text(String),
}

pub fn load_catalog(pager: &Pager) -> Result<Catalog> {
    let rows = read_schema_rows(pager)?;
    let mut catalog = Catalog::default();
    let autoindex_max_ordinals = max_autoindex_ordinals(&rows);

    for row in rows {
        if row.name == "sqlite_sequence" && row.entry_type == "table" {
            catalog.sqlite_sequence_root_page = Some(row.root_page);
            continue;
        }

        if row.name.starts_with("sqlite_")
            && !(row.entry_type == "index" && row.name.starts_with("sqlite_autoindex_"))
        {
            continue;
        }

        match row.entry_type.as_str() {
            "table" => {
                let Some(sql) = row.sql.as_deref() else {
                    continue;
                };
                let schema = parse_schema(sql, &row.name)?;
                catalog
                    .table_root_pages
                    .insert(schema.name.clone(), row.root_page);
                catalog.schemas.insert(schema.name.clone(), schema);
            }
            "index" => {
                let index = match row.sql.as_deref() {
                    Some(sql) => parse_index(sql, &row.name, &row.table_name)?,
                    None if row.name.starts_with("sqlite_autoindex_") => parse_autoindex(
                        &catalog.schemas,
                        &row.name,
                        &row.table_name,
                        autoindex_max_ordinals.get(&row.table_name).copied(),
                    )?,
                    None => continue,
                };
                catalog
                    .index_root_pages
                    .entry(row.table_name.clone())
                    .or_default()
                    .insert(index.name.clone(), row.root_page);
                catalog
                    .indexes
                    .entry(row.table_name)
                    .or_default()
                    .insert(index.name.clone(), index);
            }
            _ => {}
        }
    }

    synthesize_without_rowid_primary_key_indexes(&mut catalog);

    Ok(catalog)
}

fn synthesize_without_rowid_primary_key_indexes(catalog: &mut Catalog) {
    let tables = catalog
        .schemas
        .iter()
        .filter_map(|(table_name, schema)| {
            if !schema.without_rowid {
                return None;
            }
            let primary_key = schema.primary_key_constraint.as_ref()?;
            Some((table_name.clone(), primary_key.columns.clone()))
        })
        .collect::<Vec<_>>();

    for (table_name, columns) in tables {
        let Some(root_page) = catalog.table_root_pages.get(&table_name).copied() else {
            continue;
        };
        let index_name = format!("sqlite_autoindex_{table_name}_1");
        catalog
            .indexes
            .entry(table_name.clone())
            .or_default()
            .entry(index_name.clone())
            .or_insert_with(|| IndexMeta {
                name: index_name.clone(),
                columns: columns.clone(),
                decorated_columns: Some(columns.clone()),
                unique: true,
                predicate: None,
            });
        catalog
            .index_root_pages
            .entry(table_name)
            .or_default()
            .entry(index_name)
            .or_insert(root_page);
    }
}

fn max_autoindex_ordinals(rows: &[SchemaRow]) -> BTreeMap<String, usize> {
    let mut max_ordinals = BTreeMap::new();
    for row in rows {
        if row.entry_type != "index" || !row.name.starts_with("sqlite_autoindex_") {
            continue;
        }
        let Some((table_name, ordinal)) = parse_autoindex_name(&row.name, &row.table_name) else {
            continue;
        };
        let entry = max_ordinals.entry(table_name).or_insert(0);
        *entry = (*entry).max(ordinal);
    }
    max_ordinals
}

fn read_schema_rows(pager: &Pager) -> Result<Vec<SchemaRow>> {
    let mut rows = Vec::new();
    scan_schema_page(pager, 1, true, &mut rows)?;
    Ok(rows)
}

fn scan_schema_page(
    pager: &Pager,
    page_no: u32,
    first_page: bool,
    rows: &mut Vec<SchemaRow>,
) -> Result<()> {
    let page = pager.read_page(page_no)?;
    let header = BtreePageHeader::decode(&page, first_page)?;

    match header.page_type {
        PageType::LeafTable => {
            for cell_offset in schema_cell_offsets(&page, first_page, &header)? {
                rows.push(decode_schema_cell_with_pager(
                    Some(pager),
                    &page,
                    cell_offset,
                )?);
            }
            Ok(())
        }
        PageType::InteriorTable => {
            for child_page in decode_interior_table_children(&page, first_page, &header)? {
                scan_schema_page(pager, child_page, false, rows)?;
            }
            Ok(())
        }
        other => Err(DbError::storage(format!(
            "sqlite schema loader does not support page type {other:?}",
        ))),
    }
}

fn schema_cell_offsets(
    page: &[u8],
    first_page: bool,
    header: &BtreePageHeader,
) -> Result<Vec<usize>> {
    let local_header_len = match header.page_type {
        PageType::InteriorIndex | PageType::InteriorTable => 12,
        PageType::LeafIndex | PageType::LeafTable => 8,
    };
    let pointer_array_start: usize = if first_page {
        100 + local_header_len
    } else {
        local_header_len
    };
    let mut offsets = Vec::with_capacity(usize::from(header.cell_count));

    for cell_index in 0..usize::from(header.cell_count) {
        let pointer_offset = pointer_array_start
            .checked_add(cell_index.saturating_mul(2))
            .ok_or_else(|| DbError::storage("sqlite schema cell pointer offset overflow"))?;
        let pointer_end = pointer_offset
            .checked_add(2)
            .ok_or_else(|| DbError::storage("sqlite schema cell pointer range overflow"))?;
        let cell_ptr_bytes = page.get(pointer_offset..pointer_end).ok_or_else(|| {
            DbError::storage("sqlite schema page has truncated cell pointer array")
        })?;
        let cell_offset =
            usize::from(u16::from_be_bytes(cell_ptr_bytes.try_into().map_err(
                |_| DbError::storage("sqlite schema page cell pointer is invalid"),
            )?));
        offsets.push(cell_offset);
    }

    Ok(offsets)
}

fn decode_interior_table_children(
    page: &[u8],
    first_page: bool,
    header: &BtreePageHeader,
) -> Result<Vec<u32>> {
    if header.page_type != PageType::InteriorTable {
        return Err(DbError::storage(
            "sqlite schema interior child reader requires an interior table page",
        ));
    }

    let mut children = Vec::with_capacity(usize::from(header.cell_count) + 1);
    for cell_offset in schema_cell_offsets(page, first_page, header)? {
        children.push(decode_interior_child_pointer(page, cell_offset)?);
    }
    children.push(header.rightmost_pointer.ok_or_else(|| {
        DbError::storage("sqlite schema interior table page is missing rightmost pointer")
    })?);

    Ok(children)
}

fn decode_interior_child_pointer(page: &[u8], cell_offset: usize) -> Result<u32> {
    let child_bytes = page
        .get(cell_offset..cell_offset + 4)
        .ok_or_else(|| DbError::storage("sqlite schema interior table cell is truncated"))?;
    Ok(u32::from_be_bytes(child_bytes.try_into().map_err(
        |_| DbError::storage("sqlite schema interior child pointer is invalid"),
    )?))
}

fn decode_schema_cell_with_pager(
    pager: Option<&Pager>,
    page: &[u8],
    cell_offset: usize,
) -> Result<SchemaRow> {
    let cell = page
        .get(cell_offset..)
        .ok_or_else(|| DbError::storage("sqlite schema cell offset is out of bounds"))?;
    let (payload_size, payload_size_len) = decode_varint(cell)?;
    let (_, rowid_len) = decode_varint(&cell[payload_size_len..])?;

    let payload = match pager {
        Some(pager) => read_leaf_table_payload(
            pager,
            page,
            cell_offset,
            payload_size_len
                .checked_add(rowid_len)
                .ok_or_else(|| DbError::storage("sqlite schema cell header length overflow"))?,
            usize::try_from(payload_size)
                .map_err(|_| DbError::storage("sqlite schema payload is too large"))?,
        )?,
        None => {
            let payload_offset = cell_offset
                .checked_add(payload_size_len)
                .and_then(|offset| offset.checked_add(rowid_len))
                .ok_or_else(|| DbError::storage("sqlite schema payload offset overflow"))?;
            let payload_end = payload_offset
                .checked_add(
                    usize::try_from(payload_size)
                        .map_err(|_| DbError::storage("sqlite schema payload is too large"))?,
                )
                .ok_or_else(|| DbError::storage("sqlite schema payload range overflow"))?;
            page.get(payload_offset..payload_end)
                .ok_or_else(|| {
                    DbError::storage(
                        "sqlite3 schema loader does not support overflowed schema records",
                    )
                })?
                .to_vec()
        }
    };

    let values = decode_record_values(&payload)?;
    let [entry_type, name, table_name, root_page, sql] = &values[..] else {
        return Err(DbError::storage(
            "sqlite schema row does not contain the expected five columns",
        ));
    };

    Ok(SchemaRow {
        entry_type: require_text(entry_type, "type")?,
        name: require_text(name, "name")?,
        table_name: require_text(table_name, "tbl_name")?,
        root_page: require_u32(root_page, "rootpage")?,
        sql: optional_text(sql, "sql")?,
    })
}

fn decode_record_values(bytes: &[u8]) -> Result<Vec<RecordValue>> {
    let (serials, mut payload_cursor) = decode_record_header(bytes)?;
    let mut values = Vec::with_capacity(serials.len());
    for serial in serials {
        values.push(decode_serial_value(bytes, &mut payload_cursor, serial)?);
    }

    Ok(values)
}

fn read_leaf_table_payload(
    pager: &Pager,
    page: &[u8],
    cell_offset: usize,
    cell_header_len: usize,
    payload_size: usize,
) -> Result<Vec<u8>> {
    let usable_size = pager.usable_size()?;
    let max_local = usable_size
        .checked_sub(35)
        .ok_or_else(|| DbError::storage("sqlite schema max-local computation overflow"))?;
    let min_local = usable_size
        .checked_sub(12)
        .and_then(|value| value.checked_mul(32))
        .map(|value| value / 255)
        .and_then(|value| value.checked_sub(23))
        .ok_or_else(|| DbError::storage("sqlite schema min-local computation overflow"))?;

    let local_payload = if payload_size <= max_local {
        payload_size
    } else {
        let overflow_chunk = usable_size
            .checked_sub(4)
            .ok_or_else(|| DbError::storage("sqlite schema overflow chunk underflow"))?;
        let mut local = min_local + ((payload_size - min_local) % overflow_chunk);
        if local > max_local {
            local = min_local;
        }
        local
    };

    let payload_offset = cell_offset
        .checked_add(cell_header_len)
        .ok_or_else(|| DbError::storage("sqlite schema payload offset overflow"))?;
    let local_end = payload_offset
        .checked_add(local_payload)
        .ok_or_else(|| DbError::storage("sqlite schema payload range overflow"))?;
    let local_bytes = page.get(payload_offset..local_end).ok_or_else(|| {
        DbError::storage("sqlite schema payload bytes are truncated before overflow handling")
    })?;

    if local_payload >= payload_size {
        return Ok(local_bytes.to_vec());
    }

    let overflow_ptr_end = local_end
        .checked_add(4)
        .ok_or_else(|| DbError::storage("sqlite schema overflow pointer range overflow"))?;
    let overflow_ptr = page
        .get(local_end..overflow_ptr_end)
        .ok_or_else(|| DbError::storage("sqlite schema overflow pointer is truncated"))?;
    let first_overflow_page = u32::from_be_bytes(
        overflow_ptr
            .try_into()
            .map_err(|_| DbError::storage("sqlite schema overflow pointer is invalid"))?,
    );
    if first_overflow_page == 0 {
        return Err(DbError::storage(
            "sqlite schema overflow payload is missing its first overflow page pointer",
        ));
    }

    let overflow_bytes =
        pager.read_overflow_chain(first_overflow_page, payload_size - local_payload)?;
    let mut payload = Vec::with_capacity(payload_size);
    payload.extend_from_slice(local_bytes);
    payload.extend_from_slice(&overflow_bytes);
    Ok(payload)
}

pub(crate) fn decode_table_record(bytes: &[u8]) -> Result<Vec<Value>> {
    let (serials, mut payload_cursor) = decode_record_header(bytes)?;
    let mut values = Vec::with_capacity(serials.len());
    for serial in serials {
        values.push(decode_table_serial_value(
            bytes,
            &mut payload_cursor,
            serial,
        )?);
    }

    if payload_cursor != bytes.len() {
        return Err(DbError::storage(
            "invalid sqlite record: trailing bytes after payload",
        ));
    }

    Ok(values)
}

fn decode_record_header(bytes: &[u8]) -> Result<(Vec<u64>, usize)> {
    let (header_size, first_len) = decode_varint(bytes)?;
    let header_end = usize::try_from(header_size)
        .map_err(|_| DbError::storage("invalid sqlite record header size"))?;
    if header_end > bytes.len() || header_end < first_len {
        return Err(DbError::storage("invalid sqlite record header size"));
    }

    let mut serials = Vec::new();
    let mut cursor = first_len;
    while cursor < header_end {
        let (serial, consumed) = decode_varint(&bytes[cursor..header_end])?;
        serials.push(serial);
        cursor += consumed;
    }

    Ok((serials, header_end))
}

fn decode_serial_value(
    bytes: &[u8],
    payload_cursor: &mut usize,
    serial: u64,
) -> Result<RecordValue> {
    match serial {
        0 => Ok(RecordValue::Null),
        1..=6 => {
            let len = integer_len(serial);
            let end = payload_cursor
                .checked_add(len)
                .ok_or_else(|| DbError::storage("invalid sqlite integer record length"))?;
            let slice = bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite integer record length"))?;
            *payload_cursor = end;
            Ok(RecordValue::Integer(decode_integer(slice)))
        }
        8 => Ok(RecordValue::Integer(0)),
        9 => Ok(RecordValue::Integer(1)),
        serial if serial >= 12 && serial % 2 == 0 => {
            let len = usize::try_from((serial - 12) / 2)
                .map_err(|_| DbError::storage("invalid sqlite blob record length"))?;
            let end = payload_cursor
                .checked_add(len)
                .ok_or_else(|| DbError::storage("invalid sqlite blob record length"))?;
            bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite blob record length"))?;
            *payload_cursor = end;
            Err(DbError::storage(format!(
                "unsupported sqlite schema serial type {serial}",
            )))
        }
        serial if serial >= 13 && serial % 2 == 1 => {
            let len = usize::try_from((serial - 13) / 2)
                .map_err(|_| DbError::storage("invalid sqlite text record length"))?;
            let end = payload_cursor
                .checked_add(len)
                .ok_or_else(|| DbError::storage("invalid sqlite text record length"))?;
            let slice = bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite text record length"))?;
            let text = std::str::from_utf8(slice)
                .map_err(|_| DbError::storage("invalid utf-8 in sqlite text record"))?;
            *payload_cursor = end;
            Ok(RecordValue::Text(text.to_string()))
        }
        other => Err(DbError::storage(format!(
            "unsupported sqlite schema serial type {other}",
        ))),
    }
}

fn decode_table_serial_value(
    bytes: &[u8],
    payload_cursor: &mut usize,
    serial: u64,
) -> Result<Value> {
    match serial {
        0 => Ok(Value::Null),
        1..=6 => {
            let len = integer_len(serial);
            let end = payload_cursor
                .checked_add(len)
                .ok_or_else(|| DbError::storage("invalid sqlite integer record length"))?;
            let slice = bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite integer record length"))?;
            *payload_cursor = end;
            Ok(Value::Integer(decode_integer(slice)))
        }
        7 => {
            let end = payload_cursor
                .checked_add(8)
                .ok_or_else(|| DbError::storage("invalid sqlite real record length"))?;
            let slice = bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite real record length"))?;
            let number = f64::from_be_bytes(
                slice
                    .try_into()
                    .map_err(|_| DbError::storage("invalid sqlite real record length"))?,
            );
            *payload_cursor = end;
            Ok(Value::Real(number))
        }
        8 => Ok(Value::Integer(0)),
        9 => Ok(Value::Integer(1)),
        serial if serial >= 12 && serial % 2 == 0 => {
            let len = usize::try_from((serial - 12) / 2)
                .map_err(|_| DbError::storage("invalid sqlite blob record length"))?;
            let end = payload_cursor
                .checked_add(len)
                .ok_or_else(|| DbError::storage("invalid sqlite blob record length"))?;
            let slice = bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite blob record length"))?;
            *payload_cursor = end;
            Ok(Value::Blob(slice.to_vec()))
        }
        serial if serial >= 13 && serial % 2 == 1 => {
            let len = usize::try_from((serial - 13) / 2)
                .map_err(|_| DbError::storage("invalid sqlite text record length"))?;
            let end = payload_cursor
                .checked_add(len)
                .ok_or_else(|| DbError::storage("invalid sqlite text record length"))?;
            let slice = bytes
                .get(*payload_cursor..end)
                .ok_or_else(|| DbError::storage("invalid sqlite text record length"))?;
            let text = std::str::from_utf8(slice)
                .map_err(|_| DbError::storage("invalid utf-8 in sqlite text record"))?;
            *payload_cursor = end;
            Ok(Value::Text(text.to_string()))
        }
        other => Err(DbError::storage(format!(
            "unsupported sqlite table serial type {other}",
        ))),
    }
}

fn integer_len(serial: u64) -> usize {
    match serial {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 => 8,
        _ => 0,
    }
}

fn decode_integer(slice: &[u8]) -> i64 {
    let sign_byte = if slice.first().is_some_and(|byte| byte & 0x80 != 0) {
        0xff
    } else {
        0x00
    };
    let mut bytes = [sign_byte; 8];
    bytes[8 - slice.len()..].copy_from_slice(slice);
    i64::from_be_bytes(bytes)
}

fn require_text(value: &RecordValue, column: &str) -> Result<String> {
    match value {
        RecordValue::Text(text) => Ok(text.clone()),
        _ => Err(DbError::storage(format!(
            "sqlite schema column {column} is not TEXT",
        ))),
    }
}

fn optional_text(value: &RecordValue, column: &str) -> Result<Option<String>> {
    match value {
        RecordValue::Null => Ok(None),
        RecordValue::Text(text) => Ok(Some(text.clone())),
        _ => Err(DbError::storage(format!(
            "sqlite schema column {column} is not TEXT or NULL",
        ))),
    }
}

fn require_u32(value: &RecordValue, column: &str) -> Result<u32> {
    match value {
        RecordValue::Integer(value) => u32::try_from(*value).map_err(|_| {
            DbError::storage(format!(
                "sqlite schema column {column} is not a supported positive INTEGER",
            ))
        }),
        _ => Err(DbError::storage(format!(
            "sqlite schema column {column} is not INTEGER",
        ))),
    }
}

fn parse_schema(sql: &str, expected_name: &str) -> Result<Schema> {
    let statements = parse_sql(sql)?;
    let [statement] = statements.as_slice() else {
        return Err(DbError::storage(
            "sqlite schema CREATE TABLE SQL must contain exactly one statement",
        ));
    };

    match statement {
        Statement::CreateTable {
            name,
            columns,
            constraints,
            strict,
            without_rowid,
            ..
        } => {
            if name != expected_name {
                return Err(DbError::storage(format!(
                    "sqlite schema table name mismatch: expected {expected_name}, parsed {name}",
                )));
            }

            let mut schema = Schema::new(name.clone(), columns.clone());
            schema.strict = *strict;
            schema.without_rowid = *without_rowid;
            for constraint in constraints {
                match constraint {
                    TableConstraint::Check(check) => schema = schema.with_check(check.clone()),
                    TableConstraint::ForeignKey(foreign_key) => {
                        schema = schema.with_foreign_key(foreign_key.clone());
                    }
                    TableConstraint::PrimaryKey(primary_key_constraint) => {
                        schema.mark_primary_key_columns(primary_key_constraint)?;
                    }
                    TableConstraint::Unique(unique_constraint) => {
                        schema = schema.with_unique_constraint(unique_constraint.clone());
                    }
                }
            }
            Ok(schema)
        }
        _ => Err(DbError::storage(
            "sqlite schema row did not parse as CREATE TABLE",
        )),
    }
}

fn parse_index(sql: &str, expected_name: &str, expected_table: &str) -> Result<IndexMeta> {
    let statements = parse_sql(sql)?;
    let [statement] = statements.as_slice() else {
        return Err(DbError::storage(
            "sqlite schema CREATE INDEX SQL must contain exactly one statement",
        ));
    };

    match statement {
        Statement::CreateIndex {
            name,
            table,
            columns,
            decorated_columns,
            unique,
            predicate,
            ..
        } => {
            if name != expected_name {
                return Err(DbError::storage(format!(
                    "sqlite schema index name mismatch: expected {expected_name}, parsed {name}",
                )));
            }
            if table != expected_table {
                return Err(DbError::storage(format!(
                    "sqlite schema index table mismatch: expected {expected_table}, parsed {table}",
                )));
            }

            Ok(IndexMeta {
                name: name.clone(),
                columns: columns.clone(),
                decorated_columns: decorated_columns.clone(),
                unique: *unique,
                predicate: predicate.clone(),
            })
        }
        _ => Err(DbError::storage(
            "sqlite schema row did not parse as CREATE INDEX",
        )),
    }
}

fn parse_autoindex(
    schemas: &BTreeMap<String, Schema>,
    index_name: &str,
    table_name: &str,
    max_ordinal: Option<usize>,
) -> Result<IndexMeta> {
    let schema = schemas.get(table_name).ok_or_else(|| {
        DbError::storage(format!(
            "sqlite autoindex {index_name} referenced unknown table {table_name}",
        ))
    })?;

    let inline_unique_count = schema.columns.iter().filter(|column| column.unique).count();
    let table_unique_count = schema.unique_constraints.len();
    let total_unique_count = inline_unique_count
        .checked_add(table_unique_count)
        .ok_or_else(|| DbError::storage("sqlite autoindex unique constraint count overflow"))?;
    let (parsed_table_name, ordinal) =
        parse_autoindex_name(index_name, table_name).ok_or_else(|| {
            DbError::storage(format!("unsupported sqlite autoindex name {index_name}"))
        })?;
    if parsed_table_name != table_name {
        return Err(DbError::storage(format!(
            "sqlite autoindex {index_name} table name mismatch: expected {table_name}, parsed {parsed_table_name}",
        )));
    }

    let unique_start_ordinal = match max_ordinal {
        Some(max_ordinal) if total_unique_count > 0 && max_ordinal >= total_unique_count => {
            max_ordinal - total_unique_count + 1
        }
        _ => usize::MAX,
    };

    let columns = if ordinal >= unique_start_ordinal && total_unique_count > 0 {
        let unique_zero_based = ordinal - unique_start_ordinal;
        if unique_zero_based < inline_unique_count {
            let column = schema
                .columns
                .iter()
                .filter(|column| column.unique)
                .nth(unique_zero_based)
                .ok_or_else(|| {
                    DbError::storage(format!(
                        "sqlite autoindex {index_name} could not be matched to inline UNIQUE column",
                    ))
                })?;
            vec![column.name.clone()]
        } else {
            schema
                .unique_constraints
                .get(unique_zero_based - inline_unique_count)
                .map(|unique| unique.columns.clone())
                .ok_or_else(|| {
                    DbError::storage(format!(
                        "sqlite autoindex {index_name} could not be matched to table UNIQUE constraint",
                    ))
                })?
        }
    } else {
        let primary_key_columns = schema
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        if primary_key_columns.is_empty() {
            return Err(DbError::storage(format!(
                "sqlite autoindex {index_name} could not be matched to UNIQUE or PRIMARY KEY columns",
            )));
        }
        primary_key_columns
    };

    Ok(IndexMeta {
        name: index_name.to_string(),
        columns,
        decorated_columns: None,
        unique: true,
        predicate: None,
    })
}

fn parse_autoindex_name(index_name: &str, table_name: &str) -> Option<(String, usize)> {
    let suffix = index_name.strip_prefix("sqlite_autoindex_")?;
    let (parsed_table_name, ordinal) = suffix.rsplit_once('_')?;
    if parsed_table_name != table_name {
        return None;
    }
    let ordinal = ordinal.parse::<usize>().ok()?;
    Some((parsed_table_name.to_string(), ordinal))
}
