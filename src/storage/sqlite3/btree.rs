use crate::common::error::{DbError, Result};
use std::collections::BTreeSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};

use super::page::{BtreePageHeader, PageType};
use super::pager::Pager;
use super::schema::decode_table_record;
use super::varint::decode_varint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableLeafCell {
    pub row_id: RowId,
    pub row: Row,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub key: Vec<Value>,
    pub row_id: RowId,
}

pub fn scan_table_rows(
    pager: &Pager,
    root_page: u32,
    schema: &Schema,
) -> Result<Vec<(RowId, Row)>> {
    let mut rows = Vec::new();
    scan_table_page(pager, root_page, schema, &mut rows)?;
    Ok(rows)
}

pub fn get_table_row(
    pager: &Pager,
    root_page: u32,
    schema: &Schema,
    row_id: RowId,
) -> Result<Option<Row>> {
    get_table_row_from_page(pager, root_page, schema, row_id)
}

pub fn scan_index_entries(
    pager: &Pager,
    root_page: u32,
    index: &IndexMeta,
) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    scan_index_page(pager, root_page, index, &mut entries)?;
    Ok(entries)
}

pub fn lookup_index_entries(
    pager: &Pager,
    root_page: u32,
    index: &IndexMeta,
    key_prefix: &[Value],
) -> Result<Vec<RowId>> {
    let mut row_ids = BTreeSet::new();
    scan_matching_index_page(pager, root_page, index, key_prefix, &mut row_ids)?;
    Ok(row_ids.into_iter().collect())
}

fn scan_table_page(
    pager: &Pager,
    page_no: u32,
    schema: &Schema,
    rows: &mut Vec<(RowId, Row)>,
) -> Result<()> {
    let page = pager.read_page(page_no)?;
    let header = BtreePageHeader::decode(&page, page_no == 1)?;

    match header.page_type {
        PageType::LeafTable => {
            for cell in decode_leaf_table_cells(pager, &page, page_no == 1, schema)? {
                rows.push((cell.row_id, cell.row));
            }
            Ok(())
        }
        PageType::InteriorTable => {
            for child_page in decode_interior_table_children(&page, page_no == 1, &header)? {
                scan_table_page(pager, child_page, schema, rows)?;
            }
            Ok(())
        }
        PageType::LeafIndex if schema.without_rowid => {
            for row in decode_leaf_without_rowid_table_rows(pager, &page, page_no == 1, schema)? {
                let next = rows
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| DbError::storage("WITHOUT ROWID row counter overflow"))?;
                let row_id = RowId(
                    u64::try_from(next)
                        .map_err(|_| DbError::storage("WITHOUT ROWID row counter overflow"))?,
                );
                rows.push((row_id, row));
            }
            Ok(())
        }
        PageType::InteriorIndex if schema.without_rowid => {
            for child_page in decode_interior_index_child_pointers(&page, page_no == 1, &header)? {
                scan_table_page(pager, child_page, schema, rows)?;
            }
            Ok(())
        }
        other => Err(DbError::storage(format!(
            "sqlite table reader does not support page type {other:?}",
        ))),
    }
}

fn scan_index_page(
    pager: &Pager,
    page_no: u32,
    index: &IndexMeta,
    entries: &mut Vec<IndexEntry>,
) -> Result<()> {
    let page = pager.read_page(page_no)?;
    let header = BtreePageHeader::decode(&page, page_no == 1)?;

    match header.page_type {
        PageType::LeafIndex => {
            entries.extend(decode_leaf_index_cells(pager, &page, page_no == 1, index)?);
            Ok(())
        }
        PageType::InteriorIndex => {
            for child_page in
                decode_interior_index_children(pager, &page, page_no == 1, &header, index)?
            {
                scan_index_page(pager, child_page, index, entries)?;
            }
            Ok(())
        }
        other => Err(DbError::storage(format!(
            "sqlite index reader does not support page type {other:?}",
        ))),
    }
}

fn scan_matching_index_page(
    pager: &Pager,
    page_no: u32,
    index: &IndexMeta,
    key_prefix: &[Value],
    row_ids: &mut BTreeSet<RowId>,
) -> Result<()> {
    let page = pager.read_page(page_no)?;
    let header = BtreePageHeader::decode(&page, page_no == 1)?;

    match header.page_type {
        PageType::LeafIndex => {
            for entry in decode_leaf_index_cells(pager, &page, page_no == 1, index)? {
                if entry.key.starts_with(key_prefix) {
                    row_ids.insert(entry.row_id);
                }
            }
            Ok(())
        }
        PageType::InteriorIndex => {
            for child_page in
                decode_interior_index_children(pager, &page, page_no == 1, &header, index)?
            {
                scan_matching_index_page(pager, child_page, index, key_prefix, row_ids)?;
            }
            Ok(())
        }
        other => Err(DbError::storage(format!(
            "sqlite index reader does not support page type {other:?}",
        ))),
    }
}

fn get_table_row_from_page(
    pager: &Pager,
    page_no: u32,
    schema: &Schema,
    row_id: RowId,
) -> Result<Option<Row>> {
    let page = pager.read_page(page_no)?;
    let header = BtreePageHeader::decode(&page, page_no == 1)?;

    match header.page_type {
        PageType::LeafTable => {
            for cell in decode_leaf_table_cells(pager, &page, page_no == 1, schema)? {
                if cell.row_id == row_id {
                    return Ok(Some(cell.row));
                }
            }
            Ok(None)
        }
        PageType::InteriorTable => {
            let cell_offsets = cell_offsets(&page, page_no == 1, &header)?;
            for cell_offset in cell_offsets {
                let left_child = decode_interior_child_pointer(&page, cell_offset)?;
                let key_offset = cell_offset
                    .checked_add(4)
                    .ok_or_else(|| DbError::storage("sqlite interior cell offset overflow"))?;
                let (pivot_rowid, _) = decode_varint(slice_from(&page, key_offset)?)?;
                if row_id.0 <= pivot_rowid {
                    return get_table_row_from_page(pager, left_child, schema, row_id);
                }
            }

            let rightmost = header.rightmost_pointer.ok_or_else(|| {
                DbError::storage("sqlite interior table page is missing rightmost pointer")
            })?;
            get_table_row_from_page(pager, rightmost, schema, row_id)
        }
        other => Err(DbError::storage(format!(
            "sqlite table reader does not support page type {other:?}",
        ))),
    }
}

fn decode_leaf_table_cells(
    pager: &Pager,
    page: &[u8],
    first_page: bool,
    schema: &Schema,
) -> Result<Vec<TableLeafCell>> {
    let header = BtreePageHeader::decode(page, first_page)?;
    if header.page_type != PageType::LeafTable {
        return Err(DbError::storage(
            "sqlite leaf-table cell reader requires a leaf table page",
        ));
    }

    let mut cells = Vec::with_capacity(usize::from(header.cell_count));
    for cell_offset in cell_offsets(page, first_page, &header)? {
        cells.push(decode_leaf_table_cell(pager, page, cell_offset, schema)?);
    }
    Ok(cells)
}

fn decode_leaf_table_cell(
    pager: &Pager,
    page: &[u8],
    cell_offset: usize,
    schema: &Schema,
) -> Result<TableLeafCell> {
    let cell = slice_from(page, cell_offset)?;
    let (payload_size, payload_len) = decode_varint(cell)?;
    let (row_id, row_id_len) = decode_varint(&cell[payload_len..])?;

    let cell_header_len = payload_len
        .checked_add(row_id_len)
        .ok_or_else(|| DbError::storage("sqlite table cell header length overflow"))?;
    let payload_size = usize::try_from(payload_size)
        .map_err(|_| DbError::storage("sqlite table payload is too large"))?;
    let payload = read_btree_payload(
        pager,
        page,
        cell_offset,
        cell_header_len,
        payload_size,
        false,
        "table",
    )?;

    let row_id = RowId(row_id);
    let mut row = decode_table_record(&payload)?;
    patch_integer_primary_key_alias(schema, row_id, &mut row)?;

    Ok(TableLeafCell { row_id, row })
}

fn decode_leaf_index_cells(
    pager: &Pager,
    page: &[u8],
    first_page: bool,
    index: &IndexMeta,
) -> Result<Vec<IndexEntry>> {
    let header = BtreePageHeader::decode(page, first_page)?;
    if header.page_type != PageType::LeafIndex {
        return Err(DbError::storage(
            "sqlite leaf-index cell reader requires a leaf index page",
        ));
    }

    let mut cells = Vec::with_capacity(usize::from(header.cell_count));
    for cell_offset in cell_offsets(page, first_page, &header)? {
        cells.push(decode_leaf_index_cell(pager, page, cell_offset, index)?);
    }
    Ok(cells)
}

fn decode_leaf_without_rowid_table_rows(
    pager: &Pager,
    page: &[u8],
    first_page: bool,
    schema: &Schema,
) -> Result<Vec<Row>> {
    let header = BtreePageHeader::decode(page, first_page)?;
    if header.page_type != PageType::LeafIndex {
        return Err(DbError::storage(
            "sqlite WITHOUT ROWID table reader requires a leaf index page",
        ));
    }

    let mut rows = Vec::with_capacity(usize::from(header.cell_count));
    for cell_offset in cell_offsets(page, first_page, &header)? {
        rows.push(decode_leaf_without_rowid_table_cell(
            pager,
            page,
            cell_offset,
            schema,
        )?);
    }
    Ok(rows)
}

fn decode_leaf_without_rowid_table_cell(
    pager: &Pager,
    page: &[u8],
    cell_offset: usize,
    schema: &Schema,
) -> Result<Row> {
    let cell = slice_from(page, cell_offset)?;
    let (payload_size, payload_len) = decode_varint(cell)?;
    let payload_size = usize::try_from(payload_size)
        .map_err(|_| DbError::storage("sqlite WITHOUT ROWID payload is too large"))?;
    let payload = read_btree_payload(
        pager,
        page,
        cell_offset,
        payload_len,
        payload_size,
        true,
        "WITHOUT ROWID table",
    )?;
    let values = decode_index_record(&payload)?;
    reorder_without_rowid_record(schema, values)
}

fn decode_leaf_index_cell(
    pager: &Pager,
    page: &[u8],
    cell_offset: usize,
    index: &IndexMeta,
) -> Result<IndexEntry> {
    let cell = slice_from(page, cell_offset)?;
    let (payload_size, payload_len) = decode_varint(cell)?;

    let payload_size = usize::try_from(payload_size)
        .map_err(|_| DbError::storage("sqlite index payload is too large"))?;
    let payload = read_btree_payload(
        pager,
        page,
        cell_offset,
        payload_len,
        payload_size,
        true,
        "index",
    )?;

    let values = decode_index_record(&payload)?;
    decode_index_entry_values(index, values)
}

fn patch_integer_primary_key_alias(
    schema: &Schema,
    row_id: RowId,
    row: &mut [Value],
) -> Result<()> {
    if row.len() != schema.columns.len() {
        return Err(DbError::storage(format!(
            "sqlite table row for {} expected {} columns but decoded {}",
            schema.name,
            schema.columns.len(),
            row.len()
        )));
    }

    for (index, column) in schema.columns.iter().enumerate() {
        if column.primary_key
            && matches!(
                column.column_type,
                crate::common::types::ColumnType::Integer
            )
            && !matches!(
                column.primary_key_sort_order,
                Some(crate::common::types::SortOrder::Desc)
            )
            && matches!(row[index], Value::Null)
        {
            let rowid = i64::try_from(row_id.0)
                .map_err(|_| DbError::storage("sqlite rowid does not fit in i64"))?;
            row[index] = Value::Integer(rowid);
        }
    }

    Ok(())
}

fn decode_interior_table_children(
    page: &[u8],
    first_page: bool,
    header: &BtreePageHeader,
) -> Result<Vec<u32>> {
    if header.page_type != PageType::InteriorTable {
        return Err(DbError::storage(
            "sqlite interior-table child reader requires an interior table page",
        ));
    }

    let mut children = Vec::with_capacity(usize::from(header.cell_count) + 1);
    for cell_offset in cell_offsets(page, first_page, header)? {
        children.push(decode_interior_child_pointer(page, cell_offset)?);
    }
    children.push(header.rightmost_pointer.ok_or_else(|| {
        DbError::storage("sqlite interior table page is missing rightmost pointer")
    })?);

    Ok(children)
}

fn decode_interior_index_child_pointers(
    page: &[u8],
    first_page: bool,
    header: &BtreePageHeader,
) -> Result<Vec<u32>> {
    if header.page_type != PageType::InteriorIndex {
        return Err(DbError::storage(
            "sqlite interior-index child reader requires an interior index page",
        ));
    }

    let mut children = Vec::with_capacity(usize::from(header.cell_count) + 1);
    for cell_offset in cell_offsets(page, first_page, header)? {
        children.push(decode_interior_child_pointer(page, cell_offset)?);
    }
    children.push(header.rightmost_pointer.ok_or_else(|| {
        DbError::storage("sqlite interior index page is missing rightmost pointer")
    })?);

    Ok(children)
}

fn decode_interior_index_children(
    pager: &Pager,
    page: &[u8],
    first_page: bool,
    header: &BtreePageHeader,
    index: &IndexMeta,
) -> Result<Vec<u32>> {
    if header.page_type != PageType::InteriorIndex {
        return Err(DbError::storage(
            "sqlite interior-index child reader requires an interior index page",
        ));
    }

    let mut children = Vec::with_capacity(usize::from(header.cell_count) + 1);
    for cell_offset in cell_offsets(page, first_page, header)? {
        validate_interior_index_cell(pager, page, cell_offset, index)?;
        children.push(decode_interior_child_pointer(page, cell_offset)?);
    }
    children.push(header.rightmost_pointer.ok_or_else(|| {
        DbError::storage("sqlite interior index page is missing rightmost pointer")
    })?);

    Ok(children)
}

fn decode_interior_child_pointer(page: &[u8], cell_offset: usize) -> Result<u32> {
    let child_bytes = page
        .get(cell_offset..cell_offset + 4)
        .ok_or_else(|| DbError::storage("sqlite interior table cell is truncated"))?;
    Ok(u32::from_be_bytes(child_bytes.try_into().map_err(
        |_| DbError::storage("sqlite interior table child pointer is invalid"),
    )?))
}

fn validate_interior_index_cell(
    pager: &Pager,
    page: &[u8],
    cell_offset: usize,
    index: &IndexMeta,
) -> Result<()> {
    let cell = slice_from(page, cell_offset)?;
    let payload = cell
        .get(4..)
        .ok_or_else(|| DbError::storage("sqlite interior index cell is truncated"))?;
    let (payload_size, payload_len) = decode_varint(payload)?;
    let payload_size = usize::try_from(payload_size)
        .map_err(|_| DbError::storage("sqlite interior index payload is too large"))?;
    let payload = read_btree_payload(
        pager,
        page,
        cell_offset,
        4 + payload_len,
        payload_size,
        true,
        "interior index",
    )?;

    let values = decode_index_record(&payload)?;
    let minimum_value_count = minimum_index_record_value_count(index)?;
    if values.len() < minimum_value_count {
        return Err(DbError::storage(format!(
            "sqlite interior index {} expected at least {} record values but decoded {}",
            index.name,
            minimum_value_count,
            values.len()
        )));
    }

    Ok(())
}

fn decode_index_entry_values(index: &IndexMeta, values: Vec<Value>) -> Result<IndexEntry> {
    let minimum_value_count = minimum_index_record_value_count(index)?;
    if values.len() < minimum_value_count {
        return Err(DbError::storage(format!(
            "sqlite index {} expected at least {} record values but decoded {}",
            index.name,
            minimum_value_count,
            values.len()
        )));
    }

    if values.len() == index.columns.len() + 1 {
        let row_id_value = values
            .last()
            .ok_or_else(|| DbError::storage("sqlite index record is missing trailing rowid"))?;
        if let Value::Integer(row_id) = row_id_value {
            let row_id = u64::try_from(*row_id).map_err(|_| {
                DbError::storage("sqlite index rowid must be a non-negative INTEGER")
            })?;
            return Ok(IndexEntry {
                key: values[..values.len() - 1].to_vec(),
                row_id: RowId(row_id),
            });
        }
    }

    let key = values[..index.columns.len()].to_vec();
    let trailing_primary_key = values[index.columns.len()..].to_vec();
    let row_id = synthetic_row_id_from_key(&trailing_primary_key)?;
    Ok(IndexEntry { key, row_id })
}

fn minimum_index_record_value_count(index: &IndexMeta) -> Result<usize> {
    index
        .columns
        .len()
        .checked_add(1)
        .ok_or_else(|| DbError::storage("sqlite index column count overflow"))
}

fn synthetic_row_id_from_key(key: &[Value]) -> Result<RowId> {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let value = hasher.finish();
    if value == 0 {
        return Ok(RowId(1));
    }
    Ok(RowId(value))
}

fn cell_offsets(page: &[u8], first_page: bool, header: &BtreePageHeader) -> Result<Vec<usize>> {
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
            .ok_or_else(|| DbError::storage("sqlite cell pointer offset overflow"))?;
        let pointer_end = pointer_offset
            .checked_add(2)
            .ok_or_else(|| DbError::storage("sqlite cell pointer range overflow"))?;
        let cell_ptr_bytes = page.get(pointer_offset..pointer_end).ok_or_else(|| {
            DbError::storage("sqlite btree page has truncated cell pointer array")
        })?;
        let cell_offset =
            usize::from(u16::from_be_bytes(cell_ptr_bytes.try_into().map_err(
                |_| DbError::storage("sqlite btree cell pointer is invalid"),
            )?));
        offsets.push(cell_offset);
    }

    Ok(offsets)
}

fn slice_from(page: &[u8], offset: usize) -> Result<&[u8]> {
    page.get(offset..)
        .ok_or_else(|| DbError::storage("sqlite cell offset is out of bounds"))
}

fn decode_index_record(bytes: &[u8]) -> Result<Vec<Value>> {
    let (serials, mut payload_cursor) = decode_record_header(bytes)?;
    let mut values = Vec::with_capacity(serials.len());
    for serial in serials {
        values.push(decode_index_serial_value(
            bytes,
            &mut payload_cursor,
            serial,
        )?);
    }

    if payload_cursor != bytes.len() {
        return Err(DbError::storage(
            "invalid sqlite index record: trailing bytes after payload",
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

fn decode_index_serial_value(
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
            "unsupported sqlite index serial type {other}",
        ))),
    }
}

fn reorder_without_rowid_record(schema: &Schema, values: Vec<Value>) -> Result<Row> {
    if values.len() != schema.columns.len() {
        return Err(DbError::storage(format!(
            "sqlite WITHOUT ROWID row for {} expected {} columns but decoded {}",
            schema.name,
            schema.columns.len(),
            values.len()
        )));
    }

    let primary_key = schema.primary_key_constraint.as_ref().ok_or_else(|| {
        DbError::storage(format!(
            "WITHOUT ROWID table {} is missing PRIMARY KEY metadata",
            schema.name
        ))
    })?;

    let mut record_order = Vec::with_capacity(schema.columns.len());
    for column in &primary_key.columns {
        record_order.push(column.clone());
    }
    for column in &schema.columns {
        if !primary_key.columns.iter().any(|pk| pk == &column.name) {
            record_order.push(column.name.clone());
        }
    }

    if record_order.len() != schema.columns.len() {
        return Err(DbError::storage(format!(
            "WITHOUT ROWID table {} column layout could not be reconstructed",
            schema.name
        )));
    }

    let mut row = vec![Value::Null; schema.columns.len()];
    for (value, column_name) in values.into_iter().zip(record_order.into_iter()) {
        let position = schema.column_index(&column_name)?;
        row[position] = value;
    }

    Ok(row)
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

fn read_btree_payload(
    pager: &Pager,
    page: &[u8],
    cell_offset: usize,
    cell_header_len: usize,
    payload_size: usize,
    is_index: bool,
    kind: &str,
) -> Result<Vec<u8>> {
    let usable_size = pager.usable_size()?;
    let max_local = btree_max_local(usable_size, is_index)?;
    let min_local = btree_min_local(usable_size, is_index)?;
    let local_payload = if payload_size <= max_local {
        payload_size
    } else {
        let overflow_usable = usable_size
            .checked_sub(4)
            .ok_or_else(|| DbError::storage("sqlite usable page size is too small for overflow"))?;
        let mut local = min_local + ((payload_size - min_local) % overflow_usable);
        if local > max_local {
            local = min_local;
        }
        local
    };

    let payload_offset = cell_offset
        .checked_add(cell_header_len)
        .ok_or_else(|| DbError::storage(format!("sqlite {kind} payload offset overflow")))?;
    let local_payload_end = payload_offset
        .checked_add(local_payload)
        .ok_or_else(|| DbError::storage(format!("sqlite {kind} payload range overflow")))?;
    let local_bytes = page.get(payload_offset..local_payload_end).ok_or_else(|| {
        DbError::storage(format!(
            "sqlite {kind} payload bytes are truncated before overflow handling",
        ))
    })?;

    if payload_size <= local_payload {
        return Ok(local_bytes.to_vec());
    }

    let overflow_ptr_end = local_payload_end.checked_add(4).ok_or_else(|| {
        DbError::storage(format!("sqlite {kind} overflow pointer range overflow"))
    })?;
    let overflow_ptr = page
        .get(local_payload_end..overflow_ptr_end)
        .ok_or_else(|| DbError::storage(format!("sqlite {kind} overflow pointer is truncated",)))?;
    let first_overflow_page = u32::from_be_bytes(
        overflow_ptr
            .try_into()
            .map_err(|_| DbError::storage(format!("sqlite {kind} overflow pointer is invalid")))?,
    );
    if first_overflow_page == 0 {
        return Err(DbError::storage(format!(
            "sqlite {kind} overflow payload is missing its first overflow page pointer",
        )));
    }

    let overflow_bytes =
        pager.read_overflow_chain(first_overflow_page, payload_size - local_payload)?;
    let mut payload = Vec::with_capacity(payload_size);
    payload.extend_from_slice(local_bytes);
    payload.extend_from_slice(&overflow_bytes);
    Ok(payload)
}

fn btree_max_local(usable_size: usize, is_index: bool) -> Result<usize> {
    if is_index {
        usable_size
            .checked_sub(12)
            .and_then(|value| value.checked_mul(64))
            .map(|value| value / 255)
            .and_then(|value| value.checked_sub(23))
            .ok_or_else(|| DbError::storage("sqlite index max-local computation overflow"))
    } else {
        usable_size
            .checked_sub(35)
            .ok_or_else(|| DbError::storage("sqlite table max-local computation overflow"))
    }
}

fn btree_min_local(usable_size: usize, is_index: bool) -> Result<usize> {
    if is_index {
        usable_size
            .checked_sub(12)
            .and_then(|value| value.checked_mul(32))
            .map(|value| value / 255)
            .and_then(|value| value.checked_sub(23))
            .ok_or_else(|| DbError::storage("sqlite index min-local computation overflow"))
    } else {
        usable_size
            .checked_sub(12)
            .and_then(|value| value.checked_mul(32))
            .map(|value| value / 255)
            .and_then(|value| value.checked_sub(23))
            .ok_or_else(|| DbError::storage("sqlite table min-local computation overflow"))
    }
}
