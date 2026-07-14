use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::common::error::{DbError, Result};
use crate::common::types::{
    CheckConstraint, CheckExpr, CheckOp, ColumnDef, ColumnDefault, ForeignKey,
    PrimaryKeyConstraint, Row, RowId, Schema, SortOrder, TableConstraintOrder, TrimSide, Value,
};
use crate::sql::parser::parse_check_constraint_expression;

use super::index_expr::evaluate_index_term;
use super::record::encode_record;
use super::varint::encode_varint;

const PAGE_SIZE: usize = 4096;
const SQLITE_VERSION_NUMBER: u32 = 3_046_000;

#[derive(Debug, Clone, Default)]
pub(crate) struct WritableDatabase {
    pub tables: BTreeMap<String, WritableTable>,
    pub indexes: BTreeMap<String, BTreeMap<String, crate::common::types::IndexMeta>>,
    pub extra_schema_objects: Vec<WritableSchemaObject>,
    pub sqlite_sequence: BTreeMap<String, u64>,
    pub sqlite_sequence_exists: bool,
    pub contains_without_rowid_tables: bool,
    pub user_version: u32,
    pub application_id: u32,
    pub schema_version: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct WritableSchemaObject {
    pub entry_type: String,
    pub name: String,
    pub table_name: String,
    pub root_page: u32,
    pub sql: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct WritableTable {
    pub schema: Schema,
    pub rows: Vec<(RowId, Row)>,
}

#[derive(Debug, Clone)]
struct TableLeafCellData {
    row_id: u64,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct IndexLeafCellData {
    key: Vec<Value>,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TableChildPage {
    page_no: u32,
    max_row_id: u64,
}

#[derive(Debug, Clone)]
struct IndexChildPage {
    page_no: u32,
    max_key: Vec<Value>,
}

pub(crate) fn write_database(path: &Path, database: &WritableDatabase) -> Result<()> {
    let sqlite_sequence_name = database
        .sqlite_sequence_exists
        .then_some("sqlite_sequence".to_string());
    let table_names = database
        .tables
        .iter()
        .filter(|(_, table)| !table.schema.is_view())
        .map(|(name, _)| name.clone())
        .chain(sqlite_sequence_name.clone())
        .collect::<Vec<_>>();
    let table_root_pages = table_names
        .iter()
        .enumerate()
        .map(|(index, table)| {
            let page_no = u32::try_from(index + 2)
                .map_err(|_| DbError::storage("sqlite page number overflow"))?;
            Ok((table.clone(), page_no))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let physical_indexes = collect_physical_indexes(database)?;
    let index_root_pages = physical_indexes
        .iter()
        .enumerate()
        .map(|(offset, (table, index, _meta))| {
            let page_no = u32::try_from(table_names.len() + offset + 2)
                .map_err(|_| DbError::storage("sqlite page number overflow"))?;
            Ok(((table.clone(), index.clone()), page_no))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    let physical_table_count = database
        .tables
        .values()
        .filter(|table| !table.schema.is_view())
        .count();
    let reserved_root_pages = physical_table_count
        .checked_add(usize::from(sqlite_sequence_name.is_some()))
        .ok_or_else(|| DbError::storage("sqlite reserved root page count overflow"))?
        .checked_add(index_root_pages.len())
        .ok_or_else(|| DbError::storage("sqlite reserved root page count overflow"))?;
    let mut next_page_no = u32::try_from(reserved_root_pages + 2)
        .map_err(|_| DbError::storage("sqlite page number overflow"))?;
    let mut pages = BTreeMap::new();

    for (table_name, table) in &database.tables {
        if table.schema.is_view() {
            continue;
        }
        let root_page = table_root_pages
            .get(table_name)
            .copied()
            .ok_or_else(|| DbError::storage(format!("missing root page for table {table_name}")))?;
        build_table_btree(table, root_page, &mut next_page_no, &mut pages)?;
    }
    if let Some(table_name) = sqlite_sequence_name.as_deref() {
        let root_page = table_root_pages
            .get(table_name)
            .copied()
            .ok_or_else(|| DbError::storage("missing root page for sqlite_sequence"))?;
        let table = build_sqlite_sequence_table(database)?;
        build_table_btree(&table, root_page, &mut next_page_no, &mut pages)?;
    }

    for (table_name, index_name, index) in &physical_indexes {
        let root_page = index_root_pages
            .get(&(table_name.clone(), index_name.clone()))
            .copied()
            .ok_or_else(|| {
                DbError::storage(format!(
                    "missing root page for index {index_name} on table {table_name}",
                ))
            })?;
        let table = database.tables.get(table_name).ok_or_else(|| {
            DbError::storage(format!("missing table {table_name} for index {index_name}"))
        })?;
        build_index_btree(table, index, root_page, &mut next_page_no, &mut pages)?;
    }

    insert_page(
        &mut pages,
        1,
        build_sqlite_schema_page(database, &table_root_pages, &index_root_pages)?,
    )?;

    let page_count = next_page_no
        .checked_sub(1)
        .ok_or_else(|| DbError::storage("sqlite page count underflow"))?;
    let change_counter = 1_u32;

    let page_count_usize =
        usize::try_from(page_count).map_err(|_| DbError::storage("sqlite page count overflow"))?;
    let mut bytes = Vec::with_capacity(page_count_usize * PAGE_SIZE);
    for page_no in 1..=page_count {
        let mut page = pages.remove(&page_no).ok_or_else(|| {
            DbError::storage(format!(
                "missing sqlite page {page_no} during file assembly"
            ))
        })?;
        if page_no == 1 {
            write_database_header(
                &mut page,
                page_count,
                change_counter,
                database.schema_version,
                database.user_version,
                database.application_id,
            );
        }
        bytes.extend_from_slice(&page);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn collect_physical_indexes(
    database: &WritableDatabase,
) -> Result<Vec<(String, String, crate::common::types::IndexMeta)>> {
    let mut physical_indexes = Vec::new();
    for (table_name, indexes) in &database.indexes {
        let table = database.tables.get(table_name).ok_or_else(|| {
            DbError::storage(format!("missing table {table_name} for indexed metadata"))
        })?;
        if table.schema.is_view() {
            continue;
        }
        for (index_name, index) in indexes {
            if is_synthesized_without_rowid_primary_key_index(&table.schema, index, table_name) {
                continue;
            }
            physical_indexes.push((table_name.clone(), index_name.clone(), index.clone()));
        }
    }
    Ok(physical_indexes)
}

fn is_synthesized_without_rowid_primary_key_index(
    schema: &Schema,
    index: &crate::common::types::IndexMeta,
    table_name: &str,
) -> bool {
    schema.without_rowid
        && index.name == format!("sqlite_autoindex_{table_name}_1")
        && schema
            .primary_key_constraint
            .as_ref()
            .is_some_and(|primary_key| primary_key.columns == index.columns)
}

fn insert_page(pages: &mut BTreeMap<u32, Vec<u8>>, page_no: u32, page: Vec<u8>) -> Result<()> {
    if pages.insert(page_no, page).is_some() {
        return Err(DbError::storage(format!(
            "sqlite page {page_no} was assigned more than once",
        )));
    }
    Ok(())
}

fn allocate_page(next_page_no: &mut u32) -> Result<u32> {
    let page_no = *next_page_no;
    *next_page_no = next_page_no
        .checked_add(1)
        .ok_or_else(|| DbError::storage("sqlite page number overflow"))?;
    Ok(page_no)
}

fn build_sqlite_schema_page(
    database: &WritableDatabase,
    table_root_pages: &BTreeMap<String, u32>,
    index_root_pages: &BTreeMap<(String, String), u32>,
) -> Result<Vec<u8>> {
    let mut cells = database
        .tables
        .iter()
        .enumerate()
        .map(|(index, (table_name, table))| {
            let row_id = u64::try_from(index + 1)
                .map_err(|_| DbError::storage("sqlite schema rowid overflow"))?;
            let root_page = if table.schema.is_view() {
                0
            } else {
                table_root_pages.get(table_name).copied().ok_or_else(|| {
                    DbError::storage(format!("missing root page for table {table_name}"))
                })?
            };
            let object_type = if table.schema.is_view() {
                "view"
            } else {
                "table"
            };
            let create_sql = table
                .schema
                .create_sql
                .clone()
                .unwrap_or_else(|| render_create_table(&table.schema));
            let payload = encode_record(&[
                Value::from(object_type),
                Value::Text(table_name.clone()),
                Value::Text(table_name.clone()),
                Value::Integer(i64::from(root_page)),
                Value::Text(create_sql),
            ])?;
            Ok((row_id, payload))
        })
        .collect::<Result<Vec<_>>>()?;

    let index_cells = collect_physical_indexes(database)?
        .into_iter()
        .enumerate()
        .map(|(offset, (table_name, index_name, index))| {
            let row_id = u64::try_from(database.tables.len() + offset + 1)
                .map_err(|_| DbError::storage("sqlite schema rowid overflow"))?;
            let root_page = index_root_pages
                .get(&(table_name.clone(), index_name.clone()))
                .copied()
                .ok_or_else(|| {
                    DbError::storage(format!(
                        "missing root page for index {index_name} on table {table_name}",
                    ))
                })?;
            let sql_value = if index_name.starts_with("sqlite_autoindex_") {
                Value::Null
            } else {
                Value::Text(render_create_index(&table_name, &index))
            };
            let payload = encode_record(&[
                Value::from("index"),
                Value::Text(index_name),
                Value::Text(table_name),
                Value::Integer(i64::from(root_page)),
                sql_value,
            ])?;
            Ok((row_id, payload))
        })
        .collect::<Result<Vec<_>>>()?;
    cells.extend(index_cells);

    for object in &database.extra_schema_objects {
        let row_id = u64::try_from(cells.len() + 1)
            .map_err(|_| DbError::storage("sqlite schema rowid overflow"))?;
        let payload = encode_record(&[
            Value::Text(object.entry_type.clone()),
            Value::Text(object.name.clone()),
            Value::Text(object.table_name.clone()),
            Value::Integer(i64::from(object.root_page)),
            object.sql.clone().map_or(Value::Null, Value::Text),
        ])?;
        cells.push((row_id, payload));
    }

    if database.sqlite_sequence_exists {
        let row_id = u64::try_from(cells.len() + 1)
            .map_err(|_| DbError::storage("sqlite schema rowid overflow"))?;
        let root_page = table_root_pages
            .get("sqlite_sequence")
            .copied()
            .ok_or_else(|| DbError::storage("missing root page for sqlite_sequence"))?;
        let create_sql = "CREATE TABLE sqlite_sequence(name,seq)".to_string();
        let payload = encode_record(&[
            Value::from("table"),
            Value::from("sqlite_sequence"),
            Value::from("sqlite_sequence"),
            Value::Integer(i64::from(root_page)),
            Value::Text(create_sql),
        ])?;
        cells.push((row_id, payload));
    }

    build_leaf_table_page(&cells, true)
}

fn build_sqlite_sequence_table(database: &WritableDatabase) -> Result<WritableTable> {
    let schema = Schema::new(
        "sqlite_sequence",
        vec![
            ColumnDef::new("name", crate::common::types::ColumnType::Text),
            ColumnDef::new("seq", crate::common::types::ColumnType::Integer),
        ],
    );
    let rows = database
        .sqlite_sequence
        .iter()
        .enumerate()
        .map(|(index, (name, seq))| {
            let row_id = u64::try_from(index + 1)
                .map_err(|_| DbError::storage("sqlite_sequence rowid overflow"))?;
            let seq = i64::try_from(*seq)
                .map_err(|_| DbError::storage("sqlite_sequence seq does not fit in i64"))?;
            Ok((
                RowId(row_id),
                vec![Value::Text(name.clone()), Value::Integer(seq)],
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(WritableTable { schema, rows })
}

fn build_table_btree(
    table: &WritableTable,
    root_page: u32,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<()> {
    if table.schema.without_rowid {
        return build_without_rowid_table_btree(table, root_page, next_page_no, pages);
    }

    let mut cells = table
        .rows
        .iter()
        .map(|(row_id, row)| {
            let payload = encode_table_row_payload(&table.schema, row)?;
            Ok(TableLeafCellData {
                row_id: row_id.0,
                payload,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    cells.sort_by_key(|cell| cell.row_id);

    let leaf_groups = paginate_leaf_cells(&cells, 8, leaf_table_cell_len)?;
    if leaf_groups.len() == 1 {
        let root = build_leaf_table_page_cells(&leaf_groups[0], false, next_page_no, pages)?;
        insert_page(pages, root_page, root)?;
        return Ok(());
    }

    let mut children = Vec::with_capacity(leaf_groups.len());
    for group in leaf_groups {
        let page_no = allocate_page(next_page_no)?;
        let max_row_id = group.last().map(|cell| cell.row_id).unwrap_or(0);
        let page = build_leaf_table_page_cells(&group, false, next_page_no, pages)?;
        insert_page(pages, page_no, page)?;
        children.push(TableChildPage {
            page_no,
            max_row_id,
        });
    }

    while children.len() > 1 {
        let groups = paginate_table_children(&children)?;
        let is_root_level = groups.len() == 1;
        let mut parent_pages = Vec::with_capacity(groups.len());
        for group in groups {
            let page_no = if is_root_level {
                root_page
            } else {
                allocate_page(next_page_no)?
            };
            let max_row_id = group
                .last()
                .map(|child| child.max_row_id)
                .ok_or_else(|| DbError::storage("sqlite interior table page has no children"))?;
            insert_page(pages, page_no, build_interior_table_page(&group)?)?;
            parent_pages.push(TableChildPage {
                page_no,
                max_row_id,
            });
        }
        children = parent_pages;
    }

    Ok(())
}

fn build_without_rowid_table_btree(
    table: &WritableTable,
    root_page: u32,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<()> {
    let primary_key = table
        .schema
        .primary_key_constraint
        .as_ref()
        .ok_or_else(|| {
            DbError::storage(format!(
                "WITHOUT ROWID table {} is missing PRIMARY KEY metadata",
                table.schema.name
            ))
        })?;

    let mut cells = table
        .rows
        .iter()
        .map(|(_row_id, row)| {
            let key = primary_key
                .columns
                .iter()
                .map(|column| {
                    let position = table.schema.column_index(column)?;
                    row.get(position).cloned().ok_or_else(|| {
                        DbError::storage(format!(
                            "row for table {} is missing column {column}",
                            table.schema.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let payload = encode_without_rowid_table_payload(&table.schema, row)?;
            Ok(IndexLeafCellData { key, payload })
        })
        .collect::<Result<Vec<_>>>()?;
    cells.sort_by(|left, right| left.key.cmp(&right.key));

    let leaf_groups = paginate_leaf_cells(&cells, 8, leaf_index_cell_len)?;
    if leaf_groups.len() == 1 {
        let root = build_leaf_index_page_cells(&leaf_groups[0], next_page_no, pages)?;
        insert_page(pages, root_page, root)?;
        return Ok(());
    }

    let mut children = Vec::with_capacity(leaf_groups.len());
    for group in leaf_groups {
        let page_no = allocate_page(next_page_no)?;
        let max_key = group
            .last()
            .map(|cell| cell.key.clone())
            .ok_or_else(|| DbError::storage("sqlite WITHOUT ROWID leaf page has no cells"))?;
        let page = build_leaf_index_page_cells(&group, next_page_no, pages)?;
        insert_page(pages, page_no, page)?;
        children.push(IndexChildPage { page_no, max_key });
    }

    while children.len() > 1 {
        let groups = paginate_index_children(&children)?;
        let is_root_level = groups.len() == 1;
        let mut parent_pages = Vec::with_capacity(groups.len());
        for group in groups {
            let page_no = if is_root_level {
                root_page
            } else {
                allocate_page(next_page_no)?
            };
            let max_key = group
                .last()
                .map(|child| child.max_key.clone())
                .ok_or_else(|| {
                    DbError::storage("sqlite WITHOUT ROWID interior page has no children")
                })?;
            let page = build_interior_index_page(&group, next_page_no, pages)?;
            insert_page(pages, page_no, page)?;
            parent_pages.push(IndexChildPage { page_no, max_key });
        }
        children = parent_pages;
    }

    Ok(())
}

fn build_index_btree(
    table: &WritableTable,
    index: &crate::common::types::IndexMeta,
    root_page: u32,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<()> {
    if table.schema.without_rowid {
        return build_without_rowid_index_btree(table, index, root_page, next_page_no, pages);
    }

    let mut cells = table
        .rows
        .iter()
        .filter_map(
            |(row_id, row)| match row_matches_partial_index(&table.schema, index, row) {
                Ok(true) => Some(Ok((row_id, row))),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            },
        )
        .map(|entry| {
            let (row_id, row) = entry?;
            let key = index
                .columns
                .iter()
                .map(|column| evaluate_index_term(&table.schema, row, column))
                .collect::<Result<Vec<_>>>()?;
            let mut values = key;
            values.push(Value::Integer(i64::try_from(row_id.0).map_err(|_| {
                DbError::storage("sqlite rowid does not fit in i64")
            })?));
            let payload = encode_record(&values)?;
            Ok(IndexLeafCellData {
                key: values,
                payload,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    cells.sort_by(|left, right| left.key.cmp(&right.key));

    let leaf_groups = paginate_leaf_cells(&cells, 8, leaf_index_cell_len)?;
    if leaf_groups.len() == 1 {
        let root = build_leaf_index_page_cells(&leaf_groups[0], next_page_no, pages)?;
        insert_page(pages, root_page, root)?;
        return Ok(());
    }

    let mut children = Vec::with_capacity(leaf_groups.len());
    for group in leaf_groups {
        let page_no = allocate_page(next_page_no)?;
        let max_key = group
            .last()
            .map(|cell| cell.key.clone())
            .ok_or_else(|| DbError::storage("sqlite index leaf page has no cells"))?;
        let page = build_leaf_index_page_cells(&group, next_page_no, pages)?;
        insert_page(pages, page_no, page)?;
        children.push(IndexChildPage { page_no, max_key });
    }

    while children.len() > 1 {
        let groups = paginate_index_children(&children)?;
        let is_root_level = groups.len() == 1;
        let mut parent_pages = Vec::with_capacity(groups.len());
        for group in groups {
            let page_no = if is_root_level {
                root_page
            } else {
                allocate_page(next_page_no)?
            };
            let max_key = group
                .last()
                .map(|child| child.max_key.clone())
                .ok_or_else(|| DbError::storage("sqlite interior index page has no children"))?;
            let page = build_interior_index_page(&group, next_page_no, pages)?;
            insert_page(pages, page_no, page)?;
            parent_pages.push(IndexChildPage { page_no, max_key });
        }
        children = parent_pages;
    }

    Ok(())
}

fn build_without_rowid_index_btree(
    table: &WritableTable,
    index: &crate::common::types::IndexMeta,
    root_page: u32,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<()> {
    let mut cells = table
        .rows
        .iter()
        .filter_map(
            |(_row_id, row)| match row_matches_partial_index(&table.schema, index, row) {
                Ok(true) => Some(Ok(row)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            },
        )
        .map(|entry| {
            let row = entry?;
            let values = encode_without_rowid_index_record_values(&table.schema, index, row)?;
            let payload = encode_record(&values)?;
            Ok(IndexLeafCellData {
                key: values,
                payload,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    cells.sort_by(|left, right| left.key.cmp(&right.key));

    let leaf_groups = paginate_leaf_cells(&cells, 8, leaf_index_cell_len)?;
    if leaf_groups.len() == 1 {
        let root = build_leaf_index_page_cells(&leaf_groups[0], next_page_no, pages)?;
        insert_page(pages, root_page, root)?;
        return Ok(());
    }

    let mut children = Vec::with_capacity(leaf_groups.len());
    for group in leaf_groups {
        let page_no = allocate_page(next_page_no)?;
        let max_key = group
            .last()
            .map(|cell| cell.key.clone())
            .ok_or_else(|| DbError::storage("sqlite WITHOUT ROWID index leaf page has no cells"))?;
        let page = build_leaf_index_page_cells(&group, next_page_no, pages)?;
        insert_page(pages, page_no, page)?;
        children.push(IndexChildPage { page_no, max_key });
    }

    while children.len() > 1 {
        let groups = paginate_index_children(&children)?;
        let is_root_level = groups.len() == 1;
        let mut parent_pages = Vec::with_capacity(groups.len());
        for group in groups {
            let page_no = if is_root_level {
                root_page
            } else {
                allocate_page(next_page_no)?
            };
            let max_key = group
                .last()
                .map(|child| child.max_key.clone())
                .ok_or_else(|| {
                    DbError::storage("sqlite WITHOUT ROWID index interior page has no children")
                })?;
            let page = build_interior_index_page(&group, next_page_no, pages)?;
            insert_page(pages, page_no, page)?;
            parent_pages.push(IndexChildPage { page_no, max_key });
        }
        children = parent_pages;
    }

    Ok(())
}

fn encode_table_row_payload(schema: &Schema, row: &Row) -> Result<Vec<u8>> {
    let mut values = row.clone();
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
        {
            values[index] = Value::Null;
        }
    }
    encode_record(&values)
}

fn encode_without_rowid_table_payload(schema: &Schema, row: &Row) -> Result<Vec<u8>> {
    let primary_key = schema.primary_key_constraint.as_ref().ok_or_else(|| {
        DbError::storage(format!(
            "WITHOUT ROWID table {} is missing PRIMARY KEY metadata",
            schema.name
        ))
    })?;

    let mut values = Vec::with_capacity(schema.columns.len());
    for column in &primary_key.columns {
        let position = schema.column_index(column)?;
        values.push(row.get(position).cloned().ok_or_else(|| {
            DbError::storage(format!(
                "row for table {} is missing column {column}",
                schema.name
            ))
        })?);
    }
    for column in &schema.columns {
        if primary_key.columns.iter().any(|pk| pk == &column.name) {
            continue;
        }
        let position = schema.column_index(&column.name)?;
        values.push(row.get(position).cloned().ok_or_else(|| {
            DbError::storage(format!(
                "row for table {} is missing column {}",
                schema.name, column.name
            ))
        })?);
    }
    encode_record(&values)
}

fn encode_without_rowid_index_record_values(
    schema: &Schema,
    index: &crate::common::types::IndexMeta,
    row: &Row,
) -> Result<Vec<Value>> {
    let primary_key = schema.primary_key_constraint.as_ref().ok_or_else(|| {
        DbError::storage(format!(
            "WITHOUT ROWID table {} is missing PRIMARY KEY metadata",
            schema.name
        ))
    })?;

    let mut values = Vec::new();
    for column in &index.columns {
        values.push(evaluate_index_term(schema, row, column)?);
    }
    for column in &primary_key.columns {
        if index.columns.iter().any(|indexed| indexed == column) {
            continue;
        }
        let position = schema.column_index(column)?;
        values.push(row.get(position).cloned().ok_or_else(|| {
            DbError::storage(format!(
                "row for table {} is missing column {column}",
                schema.name
            ))
        })?);
    }

    Ok(values)
}

fn row_matches_partial_index(
    schema: &Schema,
    index: &crate::common::types::IndexMeta,
    row: &Row,
) -> Result<bool> {
    let Some(predicate_sql) = index.predicate.as_deref() else {
        return Ok(true);
    };
    let predicate = parse_check_constraint_expression(predicate_sql)?;
    schema.validate_check_expr_metadata(&predicate)?;
    schema.matches_check_expr(&predicate, row)
}

fn paginate_leaf_cells<T: Clone, F>(
    cells: &[T],
    header_size: usize,
    mut cell_len: F,
) -> Result<Vec<Vec<T>>>
where
    F: FnMut(&T) -> Result<usize>,
{
    if cells.is_empty() {
        return Ok(vec![Vec::new()]);
    }

    let mut pages = Vec::new();
    let mut current = Vec::new();
    let mut current_size = header_size;

    for cell in cells {
        let required = 2_usize
            .checked_add(cell_len(cell)?)
            .ok_or_else(|| DbError::storage("sqlite leaf page size overflow"))?;
        if !current.is_empty() && current_size + required > PAGE_SIZE {
            pages.push(current);
            current = Vec::new();
            current_size = header_size;
        }
        if current_size + required > PAGE_SIZE {
            return Err(DbError::storage(
                "sqlite writer does not support a single btree cell larger than a page",
            ));
        }
        current.push(cell.clone());
        current_size += required;
    }

    pages.push(current);
    Ok(pages)
}

fn paginate_table_children(children: &[TableChildPage]) -> Result<Vec<Vec<TableChildPage>>> {
    paginate_interior_children(children, |child| {
        Ok(4 + encode_varint(child.max_row_id).len())
    })
}

fn paginate_index_children(children: &[IndexChildPage]) -> Result<Vec<Vec<IndexChildPage>>> {
    paginate_interior_children(children, interior_index_cell_len)
}

fn paginate_interior_children<T: Clone, F>(children: &[T], mut cell_len: F) -> Result<Vec<Vec<T>>>
where
    F: FnMut(&T) -> Result<usize>,
{
    if children.is_empty() {
        return Err(DbError::storage(
            "sqlite interior page must have at least one child",
        ));
    }
    if children.len() == 1 {
        return Ok(vec![children.to_vec()]);
    }

    let mut groups = Vec::new();
    let mut current = vec![children[0].clone()];
    let mut current_size = 12_usize;

    for child in &children[1..] {
        let previous_rightmost = current.last().ok_or_else(|| {
            DbError::storage("sqlite interior pagination lost its rightmost child")
        })?;
        let required = 2_usize
            .checked_add(cell_len(previous_rightmost)?)
            .ok_or_else(|| DbError::storage("sqlite interior page size overflow"))?;

        if current.len() >= 2 && current_size + required > PAGE_SIZE {
            groups.push(current);
            current = vec![child.clone()];
            current_size = 12;
            continue;
        }
        if current_size + required > PAGE_SIZE {
            return Err(DbError::storage(
                "sqlite writer does not support an interior btree cell larger than a page",
            ));
        }

        current.push(child.clone());
        current_size += required;
    }

    groups.push(current);

    if groups.len() > 1 && groups.last().map_or(false, |group| group.len() == 1) {
        let lone_group = groups
            .pop()
            .ok_or_else(|| DbError::storage("sqlite interior page rebalance underflow"))?;
        let lone_child = lone_group
            .into_iter()
            .next()
            .ok_or_else(|| DbError::storage("sqlite interior page rebalance lost its child"))?;
        let previous_group = groups
            .last_mut()
            .ok_or_else(|| DbError::storage("sqlite interior page rebalance lost its sibling"))?;
        if previous_group.len() < 3 {
            return Err(DbError::storage(
                "sqlite writer could not balance interior btree children across pages",
            ));
        }
        let moved_child = previous_group
            .pop()
            .ok_or_else(|| DbError::storage("sqlite interior page rebalance underflow"))?;
        let rebalanced_group = vec![moved_child, lone_child];
        let rebalanced_size = interior_page_size(&rebalanced_group, &mut cell_len)?;
        if rebalanced_size > PAGE_SIZE {
            return Err(DbError::storage(
                "sqlite writer could not fit rebalanced interior btree children on a page",
            ));
        }
        groups.push(rebalanced_group);
    }

    Ok(groups)
}

fn interior_page_size<T, F>(children: &[T], cell_len: &mut F) -> Result<usize>
where
    F: FnMut(&T) -> Result<usize>,
{
    if children.is_empty() {
        return Err(DbError::storage("sqlite interior page must have children"));
    }
    let mut size = 12_usize;
    for child in &children[..children.len() - 1] {
        size = size
            .checked_add(2)
            .and_then(|size| size.checked_add(cell_len(child).ok()?))
            .ok_or_else(|| DbError::storage("sqlite interior page size overflow"))?;
    }
    Ok(size)
}

fn build_leaf_table_page(cells: &[(u64, Vec<u8>)], first_page: bool) -> Result<Vec<u8>> {
    let mut page = vec![0_u8; PAGE_SIZE];
    let header_offset: usize = if first_page { 100 } else { 0 };
    let header_size: usize = 8;
    let pointer_array_start = header_offset + header_size;
    let pointer_array_bytes = cells
        .len()
        .checked_mul(2)
        .ok_or_else(|| DbError::storage("sqlite cell pointer array overflow"))?;

    let mut content_start = PAGE_SIZE;
    let mut pointers = Vec::with_capacity(cells.len());

    for (row_id, payload) in cells {
        let mut cell = encode_varint(
            u64::try_from(payload.len())
                .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
        );
        cell.extend_from_slice(&encode_varint(*row_id));
        cell.extend_from_slice(payload);

        let required_start = pointer_array_start
            .checked_add(pointer_array_bytes)
            .ok_or_else(|| DbError::storage("sqlite page pointer area overflow"))?;
        if content_start < required_start + cell.len() {
            return Err(DbError::storage(
                "sqlite writer does not support records that overflow a single leaf page",
            ));
        }

        content_start -= cell.len();
        page[content_start..content_start + cell.len()].copy_from_slice(&cell);
        pointers.push(
            u16::try_from(content_start)
                .map_err(|_| DbError::storage("sqlite cell offset does not fit in u16"))?,
        );
    }

    page[header_offset] = 0x0d;
    page[header_offset + 1..header_offset + 3].copy_from_slice(&0_u16.to_be_bytes());
    page[header_offset + 3..header_offset + 5].copy_from_slice(
        &u16::try_from(cells.len())
            .map_err(|_| DbError::storage("sqlite cell count does not fit in u16"))?
            .to_be_bytes(),
    );
    page[header_offset + 5..header_offset + 7].copy_from_slice(
        &u16::try_from(content_start)
            .map_err(|_| DbError::storage("sqlite content start does not fit in u16"))?
            .to_be_bytes(),
    );
    page[header_offset + 7] = 0;

    for (index, pointer) in pointers.iter().enumerate() {
        let offset = pointer_array_start + index * 2;
        page[offset..offset + 2].copy_from_slice(&pointer.to_be_bytes());
    }

    Ok(page)
}

fn build_leaf_table_page_cells(
    cells: &[TableLeafCellData],
    first_page: bool,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut page = vec![0_u8; PAGE_SIZE];
    let header_offset: usize = if first_page { 100 } else { 0 };
    let header_size: usize = 8;
    let pointer_array_start = header_offset + header_size;
    let pointer_array_bytes = cells
        .len()
        .checked_mul(2)
        .ok_or_else(|| DbError::storage("sqlite cell pointer array overflow"))?;

    let mut content_start = PAGE_SIZE;
    let mut pointers = Vec::with_capacity(cells.len());

    for cell in cells {
        let rendered = render_table_leaf_cell(cell, next_page_no, pages)?;
        let required_start = pointer_array_start
            .checked_add(pointer_array_bytes)
            .ok_or_else(|| DbError::storage("sqlite page pointer area overflow"))?;
        if content_start < required_start + rendered.len() {
            return Err(DbError::storage(
                "sqlite writer does not support records that overflow a single leaf page",
            ));
        }

        content_start -= rendered.len();
        page[content_start..content_start + rendered.len()].copy_from_slice(&rendered);
        pointers.push(
            u16::try_from(content_start)
                .map_err(|_| DbError::storage("sqlite cell offset does not fit in u16"))?,
        );
    }

    page[header_offset] = 0x0d;
    page[header_offset + 1..header_offset + 3].copy_from_slice(&0_u16.to_be_bytes());
    page[header_offset + 3..header_offset + 5].copy_from_slice(
        &u16::try_from(cells.len())
            .map_err(|_| DbError::storage("sqlite cell count does not fit in u16"))?
            .to_be_bytes(),
    );
    page[header_offset + 5..header_offset + 7].copy_from_slice(
        &u16::try_from(content_start)
            .map_err(|_| DbError::storage("sqlite content start does not fit in u16"))?
            .to_be_bytes(),
    );
    page[header_offset + 7] = 0;

    for (index, pointer) in pointers.iter().enumerate() {
        let offset = pointer_array_start + index * 2;
        page[offset..offset + 2].copy_from_slice(&pointer.to_be_bytes());
    }

    Ok(page)
}

fn build_leaf_index_page_cells(
    cells: &[IndexLeafCellData],
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut page = vec![0_u8; PAGE_SIZE];
    let header_offset: usize = 0;
    let header_size: usize = 8;
    let pointer_array_start = header_offset + header_size;
    let pointer_array_bytes = cells
        .len()
        .checked_mul(2)
        .ok_or_else(|| DbError::storage("sqlite cell pointer array overflow"))?;

    let mut content_start = PAGE_SIZE;
    let mut pointers = Vec::with_capacity(cells.len());

    for cell in cells {
        let rendered = render_index_leaf_cell(cell, next_page_no, pages)?;
        let required_start = pointer_array_start
            .checked_add(pointer_array_bytes)
            .ok_or_else(|| DbError::storage("sqlite page pointer area overflow"))?;
        if content_start < required_start + rendered.len() {
            return Err(DbError::storage(
                "sqlite writer does not support index records that overflow a single leaf page",
            ));
        }

        content_start -= rendered.len();
        page[content_start..content_start + rendered.len()].copy_from_slice(&rendered);
        pointers.push(
            u16::try_from(content_start)
                .map_err(|_| DbError::storage("sqlite cell offset does not fit in u16"))?,
        );
    }

    page[header_offset] = 0x0a;
    page[header_offset + 1..header_offset + 3].copy_from_slice(&0_u16.to_be_bytes());
    page[header_offset + 3..header_offset + 5].copy_from_slice(
        &u16::try_from(cells.len())
            .map_err(|_| DbError::storage("sqlite cell count does not fit in u16"))?
            .to_be_bytes(),
    );
    page[header_offset + 5..header_offset + 7].copy_from_slice(
        &u16::try_from(content_start)
            .map_err(|_| DbError::storage("sqlite content start does not fit in u16"))?
            .to_be_bytes(),
    );
    page[header_offset + 7] = 0;

    for (index, pointer) in pointers.iter().enumerate() {
        let offset = pointer_array_start + index * 2;
        page[offset..offset + 2].copy_from_slice(&pointer.to_be_bytes());
    }

    Ok(page)
}

fn leaf_table_cell_len(cell: &TableLeafCellData) -> Result<usize> {
    let local_payload = leaf_local_payload_len(cell.payload.len(), false)?;
    encode_varint(
        u64::try_from(cell.payload.len())
            .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
    )
    .len()
    .checked_add(encode_varint(cell.row_id).len())
    .and_then(|len| len.checked_add(local_payload))
    .and_then(|len| {
        if local_payload < cell.payload.len() {
            len.checked_add(4)
        } else {
            Some(len)
        }
    })
    .ok_or_else(|| DbError::storage("sqlite table leaf cell length overflow"))
}

fn leaf_index_cell_len(cell: &IndexLeafCellData) -> Result<usize> {
    let local_payload = index_local_payload_len(cell.payload.len())?;
    encode_varint(
        u64::try_from(cell.payload.len())
            .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
    )
    .len()
    .checked_add(local_payload)
    .and_then(|len| {
        if local_payload < cell.payload.len() {
            len.checked_add(4)
        } else {
            Some(len)
        }
    })
    .ok_or_else(|| DbError::storage("sqlite index leaf cell length overflow"))
}

fn render_table_leaf_cell(
    cell: &TableLeafCellData,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<Vec<u8>> {
    let local_payload = leaf_local_payload_len(cell.payload.len(), false)?;
    let mut rendered = encode_varint(
        u64::try_from(cell.payload.len())
            .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
    );
    rendered.extend_from_slice(&encode_varint(cell.row_id));
    rendered.extend_from_slice(&cell.payload[..local_payload]);
    if local_payload < cell.payload.len() {
        let first_overflow =
            write_overflow_chain(&cell.payload[local_payload..], next_page_no, pages)?;
        rendered.extend_from_slice(&first_overflow.to_be_bytes());
    }
    Ok(rendered)
}

fn render_index_leaf_cell(
    cell: &IndexLeafCellData,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<Vec<u8>> {
    let local_payload = index_local_payload_len(cell.payload.len())?;
    let mut rendered = encode_varint(
        u64::try_from(cell.payload.len())
            .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
    );
    rendered.extend_from_slice(&cell.payload[..local_payload]);
    if local_payload < cell.payload.len() {
        let first_overflow =
            write_overflow_chain(&cell.payload[local_payload..], next_page_no, pages)?;
        rendered.extend_from_slice(&first_overflow.to_be_bytes());
    }
    Ok(rendered)
}

fn interior_index_cell_len(child: &IndexChildPage) -> Result<usize> {
    let payload = encode_record(&child.max_key)?;
    let local_payload = index_local_payload_len(payload.len())?;
    encode_varint(
        u64::try_from(payload.len())
            .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
    )
    .len()
    .checked_add(4)
    .and_then(|len| len.checked_add(local_payload))
    .and_then(|len| {
        if local_payload < payload.len() {
            len.checked_add(4)
        } else {
            Some(len)
        }
    })
    .ok_or_else(|| DbError::storage("sqlite interior index cell length overflow"))
}

fn render_interior_index_cell(
    child: &IndexChildPage,
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<Vec<u8>> {
    let payload = encode_record(&child.max_key)?;
    let local_payload = index_local_payload_len(payload.len())?;

    let mut rendered = child.page_no.to_be_bytes().to_vec();
    rendered.extend_from_slice(&encode_varint(
        u64::try_from(payload.len())
            .map_err(|_| DbError::storage("sqlite payload length overflow"))?,
    ));
    rendered.extend_from_slice(&payload[..local_payload]);
    if local_payload < payload.len() {
        let first_overflow = write_overflow_chain(&payload[local_payload..], next_page_no, pages)?;
        rendered.extend_from_slice(&first_overflow.to_be_bytes());
    }
    Ok(rendered)
}

fn leaf_local_payload_len(payload_len: usize, is_index: bool) -> Result<usize> {
    let max_local = leaf_max_local(is_index)?;
    if payload_len <= max_local {
        return Ok(payload_len);
    }

    let min_local = leaf_min_local()?;
    let overflow_chunk = PAGE_SIZE
        .checked_sub(4)
        .ok_or_else(|| DbError::storage("sqlite overflow chunk size underflow"))?;
    let mut local = min_local + ((payload_len - min_local) % overflow_chunk);
    if local > max_local {
        local = min_local;
    }
    Ok(local)
}

fn index_local_payload_len(payload_len: usize) -> Result<usize> {
    leaf_local_payload_len(payload_len, true)
}

fn leaf_min_local() -> Result<usize> {
    PAGE_SIZE
        .checked_sub(12)
        .and_then(|value| value.checked_mul(32))
        .map(|value| value / 255)
        .and_then(|value| value.checked_sub(23))
        .ok_or_else(|| DbError::storage("sqlite leaf min-local computation overflow"))
}

fn leaf_max_local(is_index: bool) -> Result<usize> {
    if is_index {
        PAGE_SIZE
            .checked_sub(12)
            .and_then(|value| value.checked_mul(64))
            .map(|value| value / 255)
            .and_then(|value| value.checked_sub(23))
            .ok_or_else(|| DbError::storage("sqlite index max-local computation overflow"))
    } else {
        PAGE_SIZE
            .checked_sub(35)
            .ok_or_else(|| DbError::storage("sqlite table max-local computation overflow"))
    }
}

fn write_overflow_chain(
    bytes: &[u8],
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<u32> {
    if bytes.is_empty() {
        return Err(DbError::storage(
            "sqlite overflow writer requires at least one overflow payload byte",
        ));
    }

    let chunk_size = PAGE_SIZE
        .checked_sub(4)
        .ok_or_else(|| DbError::storage("sqlite overflow chunk size underflow"))?;
    let page_count = bytes
        .len()
        .checked_add(chunk_size - 1)
        .map(|total| total / chunk_size)
        .ok_or_else(|| DbError::storage("sqlite overflow page count overflow"))?;

    let mut page_nos = Vec::with_capacity(page_count);
    for _ in 0..page_count {
        page_nos.push(allocate_page(next_page_no)?);
    }

    for (index, page_no) in page_nos.iter().copied().enumerate() {
        let start = index
            .checked_mul(chunk_size)
            .ok_or_else(|| DbError::storage("sqlite overflow payload offset overflow"))?;
        let end = (start + chunk_size).min(bytes.len());
        let next = page_nos.get(index + 1).copied().unwrap_or(0);

        let mut page = vec![0_u8; PAGE_SIZE];
        page[..4].copy_from_slice(&next.to_be_bytes());
        page[4..4 + (end - start)].copy_from_slice(&bytes[start..end]);
        insert_page(pages, page_no, page)?;
    }

    Ok(page_nos[0])
}

fn build_interior_table_page(children: &[TableChildPage]) -> Result<Vec<u8>> {
    if children.len() < 2 {
        return Err(DbError::storage(
            "sqlite interior table page requires at least two children",
        ));
    }

    let mut page = vec![0_u8; PAGE_SIZE];
    let pointer_array_start = 12_usize;
    let cell_count = children.len() - 1;
    let pointer_array_bytes = cell_count
        .checked_mul(2)
        .ok_or_else(|| DbError::storage("sqlite cell pointer array overflow"))?;
    let required_start = pointer_array_start
        .checked_add(pointer_array_bytes)
        .ok_or_else(|| DbError::storage("sqlite page pointer area overflow"))?;

    let mut content_start = PAGE_SIZE;
    let mut pointers = Vec::with_capacity(cell_count);

    for child in &children[..cell_count] {
        let mut cell = child.page_no.to_be_bytes().to_vec();
        cell.extend_from_slice(&encode_varint(child.max_row_id));
        if content_start < required_start + cell.len() {
            return Err(DbError::storage(
                "sqlite writer does not support interior table pages that overflow a single page",
            ));
        }
        content_start -= cell.len();
        page[content_start..content_start + cell.len()].copy_from_slice(&cell);
        pointers.push(
            u16::try_from(content_start)
                .map_err(|_| DbError::storage("sqlite cell offset does not fit in u16"))?,
        );
    }

    page[0] = 0x05;
    page[1..3].copy_from_slice(&0_u16.to_be_bytes());
    page[3..5].copy_from_slice(
        &u16::try_from(cell_count)
            .map_err(|_| DbError::storage("sqlite cell count does not fit in u16"))?
            .to_be_bytes(),
    );
    page[5..7].copy_from_slice(
        &u16::try_from(content_start)
            .map_err(|_| DbError::storage("sqlite content start does not fit in u16"))?
            .to_be_bytes(),
    );
    page[7] = 0;
    page[8..12].copy_from_slice(&children[cell_count].page_no.to_be_bytes());

    for (index, pointer) in pointers.iter().enumerate() {
        let offset = pointer_array_start + index * 2;
        page[offset..offset + 2].copy_from_slice(&pointer.to_be_bytes());
    }

    Ok(page)
}

fn build_interior_index_page(
    children: &[IndexChildPage],
    next_page_no: &mut u32,
    pages: &mut BTreeMap<u32, Vec<u8>>,
) -> Result<Vec<u8>> {
    if children.len() < 2 {
        return Err(DbError::storage(
            "sqlite interior index page requires at least two children",
        ));
    }

    let mut page = vec![0_u8; PAGE_SIZE];
    let pointer_array_start = 12_usize;
    let cell_count = children.len() - 1;
    let pointer_array_bytes = cell_count
        .checked_mul(2)
        .ok_or_else(|| DbError::storage("sqlite cell pointer array overflow"))?;
    let required_start = pointer_array_start
        .checked_add(pointer_array_bytes)
        .ok_or_else(|| DbError::storage("sqlite page pointer area overflow"))?;

    let mut content_start = PAGE_SIZE;
    let mut pointers = Vec::with_capacity(cell_count);

    for child in &children[..cell_count] {
        let cell = render_interior_index_cell(child, next_page_no, pages)?;
        if content_start < required_start + cell.len() {
            return Err(DbError::storage(
                "sqlite writer does not support interior index pages that overflow a single page",
            ));
        }
        content_start -= cell.len();
        page[content_start..content_start + cell.len()].copy_from_slice(&cell);
        pointers.push(
            u16::try_from(content_start)
                .map_err(|_| DbError::storage("sqlite cell offset does not fit in u16"))?,
        );
    }

    page[0] = 0x02;
    page[1..3].copy_from_slice(&0_u16.to_be_bytes());
    page[3..5].copy_from_slice(
        &u16::try_from(cell_count)
            .map_err(|_| DbError::storage("sqlite cell count does not fit in u16"))?
            .to_be_bytes(),
    );
    page[5..7].copy_from_slice(
        &u16::try_from(content_start)
            .map_err(|_| DbError::storage("sqlite content start does not fit in u16"))?
            .to_be_bytes(),
    );
    page[7] = 0;
    page[8..12].copy_from_slice(&children[cell_count].page_no.to_be_bytes());

    for (index, pointer) in pointers.iter().enumerate() {
        let offset = pointer_array_start + index * 2;
        page[offset..offset + 2].copy_from_slice(&pointer.to_be_bytes());
    }

    Ok(page)
}

fn write_database_header(
    page: &mut [u8],
    page_count: u32,
    change_counter: u32,
    schema_version: u32,
    user_version: u32,
    application_id: u32,
) {
    page[..16].copy_from_slice(b"SQLite format 3\0");
    page[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_be_bytes());
    page[18] = 1;
    page[19] = 1;
    page[20] = 0;
    page[21] = 64;
    page[22] = 32;
    page[23] = 32;
    page[24..28].copy_from_slice(&change_counter.to_be_bytes());
    page[28..32].copy_from_slice(&page_count.to_be_bytes());
    page[32..36].copy_from_slice(&0_u32.to_be_bytes());
    page[36..40].copy_from_slice(&0_u32.to_be_bytes());
    page[40..44].copy_from_slice(&schema_version.to_be_bytes());
    page[44..48].copy_from_slice(&4_u32.to_be_bytes());
    page[48..52].copy_from_slice(&0_u32.to_be_bytes());
    page[52..56].copy_from_slice(&0_u32.to_be_bytes());
    page[56..60].copy_from_slice(&1_u32.to_be_bytes());
    page[60..64].copy_from_slice(&user_version.to_be_bytes());
    page[64..68].copy_from_slice(&0_u32.to_be_bytes());
    page[68..72].copy_from_slice(&application_id.to_be_bytes());
    page[72..92].fill(0);
    page[92..96].copy_from_slice(&change_counter.to_be_bytes());
    page[96..100].copy_from_slice(&SQLITE_VERSION_NUMBER.to_be_bytes());
}

fn render_create_table(schema: &Schema) -> String {
    let mut definitions = schema
        .columns
        .iter()
        .map(|column| render_column_def(schema, column))
        .collect::<Vec<_>>();
    definitions.extend(render_table_constraints(schema));
    let strict = if schema.strict { " STRICT" } else { "" };
    let without_rowid = if schema.without_rowid {
        " WITHOUT ROWID"
    } else {
        ""
    };
    format!(
        "CREATE TABLE {} ({}){}{}",
        schema.name,
        definitions.join(", "),
        strict,
        without_rowid
    )
}

fn render_table_constraints(schema: &Schema) -> Vec<String> {
    if schema.table_constraint_order.is_empty() {
        let mut definitions = schema
            .primary_key_constraint
            .iter()
            .map(render_primary_key_constraint)
            .collect::<Vec<_>>();
        definitions.extend(schema.checks.iter().map(render_check_constraint));
        definitions.extend(
            schema
                .unique_constraints
                .iter()
                .map(render_unique_constraint),
        );
        definitions.extend(schema.foreign_keys.iter().map(render_foreign_key));
        return definitions;
    }

    schema
        .table_constraint_order
        .iter()
        .filter_map(|entry| match entry {
            TableConstraintOrder::Check(index) => {
                schema.checks.get(*index).map(render_check_constraint)
            }
            TableConstraintOrder::ForeignKey(index) => {
                schema.foreign_keys.get(*index).map(render_foreign_key)
            }
            TableConstraintOrder::PrimaryKey => schema
                .primary_key_constraint
                .as_ref()
                .map(render_primary_key_constraint),
            TableConstraintOrder::Unique(index) => schema
                .unique_constraints
                .get(*index)
                .map(render_unique_constraint),
        })
        .collect()
}

fn render_create_index(table: &str, index: &crate::common::types::IndexMeta) -> String {
    let unique = if index.unique { " UNIQUE" } else { "" };
    let predicate = index
        .predicate
        .as_ref()
        .map(|predicate| format!(" WHERE {predicate}"))
        .unwrap_or_default();
    format!(
        "CREATE{} INDEX {} ON {} ({}){}",
        unique,
        index.name,
        table,
        index.rendered_columns().join(", "),
        predicate
    )
}

fn render_column_def(schema: &Schema, column: &ColumnDef) -> String {
    let mut rendered = match column.pragma_declared_type() {
        "" => column.name.clone(),
        declared_type => format!("{} {}", column.name, declared_type),
    };
    if let Some(collation) = &column.collation {
        rendered.push_str(" COLLATE ");
        rendered.push_str(collation);
    }
    let rendered_by_table_constraint = schema
        .primary_key_constraint
        .as_ref()
        .is_some_and(|constraint| constraint.columns.iter().any(|name| name == &column.name));
    if column.primary_key && !rendered_by_table_constraint {
        if let Some(constraint_name) = &column.primary_key_constraint_name {
            rendered.push_str(" CONSTRAINT ");
            rendered.push_str(constraint_name);
        }
        rendered.push_str(" PRIMARY KEY");
        if let Some(conflict_clause) = &column.primary_key_conflict_clause {
            rendered.push_str(" ON CONFLICT ");
            rendered.push_str(conflict_clause);
        }
        if let Some(sort_order) = column.primary_key_sort_order {
            match sort_order {
                SortOrder::Asc => rendered.push_str(" ASC"),
                SortOrder::Desc => rendered.push_str(" DESC"),
            }
        }
        if column.autoincrement {
            rendered.push_str(" AUTOINCREMENT");
        }
    }
    if column.unique {
        if let Some(constraint_name) = &column.unique_constraint_name {
            rendered.push_str(" CONSTRAINT ");
            rendered.push_str(constraint_name);
        }
        rendered.push_str(" UNIQUE");
        if let Some(conflict_clause) = &column.unique_conflict_clause {
            rendered.push_str(" ON CONFLICT ");
            rendered.push_str(conflict_clause);
        }
    }
    if !column.nullable && !column.primary_key {
        if let Some(constraint_name) = &column.not_null_constraint_name {
            rendered.push_str(" CONSTRAINT ");
            rendered.push_str(constraint_name);
        }
        rendered.push_str(" NOT NULL");
        if let Some(conflict_clause) = &column.not_null_conflict_clause {
            rendered.push_str(" ON CONFLICT ");
            rendered.push_str(conflict_clause);
        }
    }
    if let Some(default_value) = &column.default_value {
        rendered.push_str(" DEFAULT ");
        rendered.push_str(&render_column_default(default_value));
    }
    if let Some(expr) = &column.generated_expr {
        if column.generated_always_explicit {
            rendered.push_str(" GENERATED ALWAYS AS (");
        } else {
            rendered.push_str(" AS (");
        }
        rendered.push_str(expr);
        rendered.push(')');
        if column.generated_stored {
            rendered.push_str(" STORED");
        } else if column.generated_storage_explicit {
            rendered.push_str(" VIRTUAL");
        }
    }
    for check in &column.checks {
        rendered.push(' ');
        rendered.push_str(&render_check_constraint(check));
    }
    if let Some(foreign_key) = &column.foreign_key {
        rendered.push(' ');
        rendered.push_str(&render_inline_foreign_key(foreign_key));
    }
    rendered
}

fn render_foreign_key(foreign_key: &ForeignKey) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &foreign_key.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("FOREIGN KEY (");
    rendered.push_str(&foreign_key.rendered_child_columns());
    rendered.push_str(") REFERENCES ");
    rendered.push_str(&foreign_key.ref_table);
    if let Some(ref_columns) = foreign_key.rendered_referenced_columns() {
        rendered.push('(');
        rendered.push_str(&ref_columns);
        rendered.push(')');
    }
    append_foreign_key_clauses(&mut rendered, foreign_key);
    rendered
}

fn render_inline_foreign_key(foreign_key: &ForeignKey) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &foreign_key.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("REFERENCES ");
    rendered.push_str(&foreign_key.ref_table);
    if let Some(ref_columns) = foreign_key.rendered_referenced_columns() {
        rendered.push('(');
        rendered.push_str(&ref_columns);
        rendered.push(')');
    }
    append_foreign_key_clauses(&mut rendered, foreign_key);
    rendered
}

fn append_foreign_key_clauses(rendered: &mut String, foreign_key: &ForeignKey) {
    if let Some(match_clause) = &foreign_key.match_clause {
        rendered.push_str(" MATCH ");
        rendered.push_str(match_clause);
    }
    if let Some(on_delete) = &foreign_key.on_delete {
        rendered.push_str(" ON DELETE ");
        rendered.push_str(on_delete);
    }
    if let Some(on_update) = &foreign_key.on_update {
        rendered.push_str(" ON UPDATE ");
        rendered.push_str(on_update);
    }
    if let Some(deferrable) = foreign_key.deferrable {
        if deferrable {
            rendered.push_str(" DEFERRABLE");
        } else {
            rendered.push_str(" NOT DEFERRABLE");
        }
    }
    if let Some(initially_deferred) = foreign_key.initially_deferred {
        if initially_deferred {
            rendered.push_str(" INITIALLY DEFERRED");
        } else {
            rendered.push_str(" INITIALLY IMMEDIATE");
        }
    }
}

fn render_check_constraint(check: &CheckConstraint) -> String {
    if check.explicit_name {
        format!(
            "CONSTRAINT {} CHECK ({})",
            check.name,
            render_check_expr(&check.expr)
        )
    } else {
        format!("CHECK ({})", render_check_expr(&check.expr))
    }
}

fn render_primary_key_constraint(primary_key: &PrimaryKeyConstraint) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &primary_key.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("PRIMARY KEY(");
    rendered.push_str(&primary_key.rendered_columns().join(", "));
    rendered.push(')');
    if let Some(conflict_clause) = &primary_key.conflict_clause {
        rendered.push_str(" ON CONFLICT ");
        rendered.push_str(conflict_clause);
    }
    rendered
}

fn render_unique_constraint(unique: &crate::common::types::UniqueConstraint) -> String {
    let mut rendered = String::new();
    if let Some(constraint_name) = &unique.constraint_name {
        rendered.push_str("CONSTRAINT ");
        rendered.push_str(constraint_name);
        rendered.push(' ');
    }
    rendered.push_str("UNIQUE(");
    rendered.push_str(&unique.rendered_columns().join(", "));
    rendered.push(')');
    if let Some(conflict_clause) = &unique.conflict_clause {
        rendered.push_str(" ON CONFLICT ");
        rendered.push_str(conflict_clause);
    }
    rendered
}

fn render_check_expr(expr: &CheckExpr) -> String {
    match expr {
        CheckExpr::Compare { column, op, value } => {
            format!(
                "{} {} {}",
                column,
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::IsNull { column, negated } => {
            if *negated {
                format!("{column} IS NOT NULL")
            } else {
                format!("{column} IS NULL")
            }
        }
        CheckExpr::Glob {
            column,
            pattern,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!(
                "{column} {not}GLOB {}",
                render_literal(&Value::from(pattern.as_str()))
            )
        }
        CheckExpr::Regexp {
            column,
            pattern,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!(
                "{column} {not}REGEXP {}",
                render_literal(&Value::from(pattern.as_str()))
            )
        }
        CheckExpr::Like {
            column,
            pattern,
            escape,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let escape = escape
                .as_ref()
                .map(|escape| format!(" ESCAPE {}", render_literal(&Value::from(escape.as_str()))))
                .unwrap_or_default();
            format!(
                "{column} {not}LIKE {}{escape}",
                render_literal(&Value::from(pattern.as_str()))
            )
        }
        CheckExpr::InList {
            column,
            values,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            let values = values
                .iter()
                .map(render_literal)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{column} {not}IN ({values})")
        }
        CheckExpr::Between {
            column,
            low,
            high,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!(
                "{column} {not}BETWEEN {} AND {}",
                render_literal(low),
                render_literal(high)
            )
        }
        CheckExpr::IsBool {
            column,
            value,
            negated,
        } => {
            format!(
                "{column} IS {}{}",
                if *negated { "NOT " } else { "" },
                if *value { "TRUE" } else { "FALSE" }
            )
        }
        CheckExpr::Truthy { column } => column.clone(),
        CheckExpr::IsDistinct {
            column,
            value,
            negated,
        } => {
            let not = if *negated { "" } else { "NOT " };
            format!("{column} IS {not}DISTINCT FROM {}", render_literal(value))
        }
        CheckExpr::LengthCompare { column, op, value } => {
            format!(
                "length({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::OctetLengthCompare { column, op, value } => {
            format!(
                "octet_length({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::UnicodeCompare { column, op, value } => {
            format!(
                "unicode({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::UnicodeIsNull { column, negated } => {
            let not = if *negated { "NOT " } else { "" };
            format!("unicode({column}) IS {not}NULL")
        }
        CheckExpr::SignCompare { column, op, value } => {
            format!(
                "sign({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::HexCompare { column, op, value } => {
            format!(
                "hex({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::QuoteCompare { column, op, value } => {
            format!(
                "quote({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::NullIfIsNull {
            column,
            value,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!("nullif({column}, {}) IS {not}NULL", render_literal(value))
        }
        CheckExpr::ReplaceCompare {
            column,
            pattern,
            replacement,
            op,
            value,
        } => {
            format!(
                "replace({column}, {}, {}) {} {}",
                render_literal(&Value::from(pattern.as_str())),
                render_literal(&Value::from(replacement.as_str())),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::ReplaceColumnCompare {
            column,
            pattern,
            replacement,
            op,
        } => {
            format!(
                "replace({column}, {}, {}) {} {column}",
                render_literal(&Value::from(pattern.as_str())),
                render_literal(&Value::from(replacement.as_str())),
                render_check_op(*op)
            )
        }
        CheckExpr::RoundCompare {
            column,
            precision,
            op,
            value,
        } => {
            let args = precision
                .map(|precision| format!(", {precision}"))
                .unwrap_or_default();
            format!(
                "round({column}{args}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::RoundingCompare {
            column,
            func,
            op,
            value,
        } => {
            format!(
                "{}({column}) {} {}",
                func.sql_name(),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::CastCompare {
            column,
            target_type,
            op,
            value,
        } => {
            format!(
                "CAST({column} AS {}) {} {}",
                target_type.name(),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::MinMaxColumnCompare {
            column,
            limit,
            min,
            op,
        } => {
            let func = if *min { "min" } else { "max" };
            format!(
                "{func}({column}, {}) {} {column}",
                render_literal(limit),
                render_check_op(*op)
            )
        }
        CheckExpr::ConcatCompare {
            column,
            suffix,
            op,
            value,
        } => {
            let args = suffix
                .iter()
                .map(|value| format!(", {}", render_literal(value)))
                .collect::<String>();
            format!(
                "concat({column}{args}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::ConcatWsCompare {
            column,
            separator,
            suffix,
            op,
            value,
        } => {
            let separator = separator
                .as_ref()
                .map(|separator| render_literal(&Value::from(separator.as_str())))
                .unwrap_or_else(|| render_literal(&Value::Null));
            let args = suffix
                .iter()
                .map(|value| format!(", {}", render_literal(value)))
                .collect::<String>();
            format!(
                "concat_ws({separator}, {column}{args}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::JsonValidCompare {
            column,
            flags,
            compare,
        } => {
            let args = flags.map(|flags| format!(", {flags}")).unwrap_or_default();
            let expr = format!("json_valid({column}{args})");
            if let Some((op, value)) = compare {
                format!("{expr} {} {}", render_check_op(*op), render_literal(value))
            } else {
                expr
            }
        }
        CheckExpr::AbsCompare { column, op, value } => {
            format!(
                "abs({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::UnaryMathCompare {
            column,
            func,
            op,
            value,
        } => {
            format!(
                "{}({column}) {} {}",
                func.sql_name(),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::BinaryMathCompare {
            column,
            func,
            argument,
            column_is_second,
            op,
            value,
        } => {
            let rendered_argument = render_literal(argument);
            let args = if *column_is_second {
                format!("{rendered_argument}, {column}")
            } else {
                format!("{column}, {rendered_argument}")
            };
            format!(
                "{}({args}) {} {}",
                func.sql_name(),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::ArithmeticCompare {
            column,
            addend,
            op,
            value,
        } => {
            format!(
                "({column} + {}) {} {}",
                render_literal(addend),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::MultiplyCompare {
            column,
            factor,
            op,
            value,
        } => {
            format!(
                "({column} * {}) {} {}",
                render_literal(factor),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::DivideCompare {
            column,
            divisor,
            op,
            value,
        } => {
            format!(
                "({column} / {}) {} {}",
                render_literal(divisor),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::ModuloCompare {
            column,
            divisor,
            op,
            value,
            function_form,
        } => {
            let expr = if *function_form {
                format!("mod({column}, {})", render_literal(divisor))
            } else {
                format!("({column} % {})", render_literal(divisor))
            };
            format!("{expr} {} {}", render_check_op(*op), render_literal(value))
        }
        CheckExpr::TypeOfCompare { column, op, value } => {
            format!(
                "typeof({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::NoCaseCompare {
            column,
            collation,
            op,
            value,
        } => {
            format!(
                "{column} COLLATE {collation} {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::CaseFoldCompare {
            column,
            upper,
            op,
            value,
        } => {
            let func = if *upper { "upper" } else { "lower" };
            format!(
                "{func}({column}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::TrimCompare {
            column,
            side,
            characters,
            op,
            value,
        } => {
            let func = match side {
                TrimSide::Both => "trim",
                TrimSide::Start => "ltrim",
                TrimSide::End => "rtrim",
            };
            let args = characters
                .as_ref()
                .map(|characters| {
                    format!(", {}", render_literal(&Value::from(characters.as_str())))
                })
                .unwrap_or_default();
            format!(
                "{func}({column}{args}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::CoalesceCompare {
            column,
            fallbacks,
            op,
            value,
        } => {
            let args = fallbacks
                .iter()
                .map(|fallback| format!(", {}", render_literal(fallback)))
                .collect::<String>();
            format!(
                "coalesce({column}{args}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::InstrCompare {
            column,
            needle,
            op,
            value,
        } => {
            format!(
                "instr({column}, {}) {} {}",
                render_literal(needle),
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::SubstrCompare {
            column,
            start,
            length,
            op,
            value,
        } => {
            let length = length
                .map(|length| format!(", {length}"))
                .unwrap_or_default();
            format!(
                "substr({column}, {start}{length}) {} {}",
                render_check_op(*op),
                render_literal(value)
            )
        }
        CheckExpr::And(left, right) => {
            format!(
                "({}) AND ({})",
                render_check_expr(left),
                render_check_expr(right)
            )
        }
        CheckExpr::Or(left, right) => {
            format!(
                "({}) OR ({})",
                render_check_expr(left),
                render_check_expr(right)
            )
        }
        CheckExpr::Not(expr) => format!("NOT ({})", render_check_expr(expr)),
    }
}

fn render_check_op(op: CheckOp) -> &'static str {
    match op {
        CheckOp::Eq => "=",
        CheckOp::Ne => "!=",
        CheckOp::Gt => ">",
        CheckOp::Gte => ">=",
        CheckOp::Lt => "<",
        CheckOp::Lte => "<=",
    }
}

fn render_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(true) => "true".to_string(),
        Value::Boolean(false) => "false".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Real(value) => value.to_string(),
        Value::Blob(value) => format!(
            "X'{}'",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
    }
}

fn render_column_default(default_value: &ColumnDefault) -> String {
    match default_value {
        ColumnDefault::Literal(value) => render_literal(value),
        ColumnDefault::CurrentTimestamp => "CURRENT_TIMESTAMP".to_string(),
        ColumnDefault::CurrentDate => "CURRENT_DATE".to_string(),
        ColumnDefault::CurrentTime => "CURRENT_TIME".to_string(),
    }
}
