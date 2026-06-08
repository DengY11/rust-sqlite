use serde::{Deserialize, Serialize};

use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};
use crate::engine::txn::TransactionId;

use super::tx_types::TxnSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowVersion {
    pub row: Row,
    pub created_by_txn: u64,
    pub created_commit_ts: Option<u64>,
    pub deleted_by_txn: Option<u64>,
    pub deleted_commit_ts: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedRow {
    pub versions: Vec<RowVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredRow {
    Legacy(Row),
    Versioned(VersionedRow),
}

pub fn encode_row(row: &Row) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&StoredRow::Legacy(row.clone()))?)
}

pub fn decode_row(bytes: &[u8]) -> Result<Row> {
    Ok(match decode_versioned_row(bytes)? {
        VersionedRow { versions } => versions
            .into_iter()
            .next()
            .map(|version| version.row)
            .unwrap_or_default(),
    })
}

pub fn encode_uncommitted_row_version(row: &Row, txn_id: u64) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&StoredRow::Versioned(VersionedRow {
        versions: vec![RowVersion {
            row: row.clone(),
            created_by_txn: txn_id,
            created_commit_ts: None,
            deleted_by_txn: None,
            deleted_commit_ts: None,
        }],
    }))?)
}

pub fn decode_versioned_row(bytes: &[u8]) -> Result<VersionedRow> {
    Ok(match serde_json::from_slice(bytes)? {
        StoredRow::Legacy(row) => VersionedRow {
            versions: vec![RowVersion {
                row,
                created_by_txn: 0,
                created_commit_ts: Some(0),
                deleted_by_txn: None,
                deleted_commit_ts: None,
            }],
        },
        StoredRow::Versioned(versioned) => versioned,
    })
}

pub fn visible_row(bytes: &[u8], txn_id: u64, snapshot: &TxnSnapshot) -> Result<Option<Row>> {
    let versioned = decode_versioned_row(bytes)?;
    for version in versioned.versions {
        let creator_visible = version_visible_to_snapshot(
            version.created_by_txn,
            version.created_commit_ts,
            txn_id,
            snapshot,
        );
        if !creator_visible {
            continue;
        }

        let deleted_for_self = version.deleted_by_txn == Some(txn_id);
        if deleted_for_self {
            continue;
        }

        let deleted_in_snapshot = version
            .deleted_by_txn
            .zip(version.deleted_commit_ts)
            .is_some_and(|(deleted_by_txn, commit_ts)| {
                version_visible_to_snapshot(deleted_by_txn, Some(commit_ts), txn_id, snapshot)
            });
        if deleted_in_snapshot {
            continue;
        }

        return Ok(Some(version.row));
    }

    Ok(None)
}

pub fn mark_row_deleted(bytes: &[u8], txn_id: u64, snapshot: &TxnSnapshot) -> Result<Option<Vec<u8>>> {
    let mut versioned = decode_versioned_row(bytes)?;
    for version in &mut versioned.versions {
        let creator_visible = version_visible_to_snapshot(
            version.created_by_txn,
            version.created_commit_ts,
            txn_id,
            snapshot,
        );
        if !creator_visible {
            continue;
        }

        let deleted_in_snapshot = version.deleted_by_txn == Some(txn_id)
            || version
                .deleted_by_txn
                .zip(version.deleted_commit_ts)
                .is_some_and(|(deleted_by_txn, commit_ts)| {
                    version_visible_to_snapshot(deleted_by_txn, Some(commit_ts), txn_id, snapshot)
                });
        if deleted_in_snapshot {
            continue;
        }

        version.deleted_by_txn = Some(txn_id);
        version.deleted_commit_ts = None;
        return Ok(Some(serde_json::to_vec(&StoredRow::Versioned(versioned))?));
    }

    Ok(None)
}

pub fn append_row_version(
    bytes: &[u8],
    row: &Row,
    txn_id: u64,
    snapshot: &TxnSnapshot,
) -> Result<Option<Vec<u8>>> {
    let mut versioned = decode_versioned_row(bytes)?;
    let Some(visible_index) = versioned.versions.iter().position(|version| {
        let creator_visible = version_visible_to_snapshot(
            version.created_by_txn,
            version.created_commit_ts,
            txn_id,
            snapshot,
        );
        if !creator_visible {
            return false;
        }

        if version.deleted_by_txn == Some(txn_id) {
            return false;
        }

        !version
            .deleted_by_txn
            .zip(version.deleted_commit_ts)
            .is_some_and(|(deleted_by_txn, commit_ts)| {
                version_visible_to_snapshot(deleted_by_txn, Some(commit_ts), txn_id, snapshot)
            })
    }) else {
        return Ok(None);
    };

    if versioned.versions[visible_index].created_by_txn == txn_id
        && versioned.versions[visible_index].created_commit_ts.is_none()
    {
        versioned.versions[visible_index].row = row.clone();
        return Ok(Some(serde_json::to_vec(&StoredRow::Versioned(versioned))?));
    }

    versioned.versions[visible_index].deleted_by_txn = Some(txn_id);
    versioned.versions[visible_index].deleted_commit_ts = None;
    versioned.versions.insert(
        0,
        RowVersion {
            row: row.clone(),
            created_by_txn: txn_id,
            created_commit_ts: None,
            deleted_by_txn: None,
            deleted_commit_ts: None,
        },
    );
    Ok(Some(serde_json::to_vec(&StoredRow::Versioned(versioned))?))
}

pub fn finalize_row_versions(bytes: &[u8], txn_id: u64, commit_ts: u64) -> Result<Vec<u8>> {
    let mut versioned = decode_versioned_row(bytes)?;
    for version in &mut versioned.versions {
        if version.created_by_txn == txn_id && version.created_commit_ts.is_none() {
            version.created_commit_ts = Some(commit_ts);
        }
        if version.deleted_by_txn == Some(txn_id) && version.deleted_commit_ts.is_none() {
            version.deleted_commit_ts = Some(commit_ts);
        }
    }
    Ok(serde_json::to_vec(&StoredRow::Versioned(versioned))?)
}

fn version_visible_to_snapshot(
    version_txn_id: u64,
    commit_ts: Option<u64>,
    current_txn_id: u64,
    snapshot: &TxnSnapshot,
) -> bool {
    match commit_ts {
        Some(commit_ts) => {
            commit_ts <= snapshot.visible_up_to
                && !snapshot
                    .active_txns
                    .contains(&TransactionId(version_txn_id))
        }
        None => version_txn_id == current_txn_id,
    }
}

pub fn encode_schema(schema: &Schema) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(schema)?)
}

pub fn decode_schema(bytes: &[u8]) -> Result<Schema> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_index_key(values: &[Value]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(values)?)
}

pub fn decode_index_key(bytes: &[u8]) -> Result<Vec<Value>> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_row_ids(row_ids: &[RowId]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(row_ids)?)
}

pub fn decode_row_ids(bytes: &[u8]) -> Result<Vec<RowId>> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn project_index_key(schema: &Schema, index: &IndexMeta, row: &Row) -> Result<Vec<Value>> {
    index
        .columns
        .iter()
        .map(|column| {
            let position = schema
                .columns
                .iter()
                .position(|entry| entry.name == *column)
                .ok_or_else(|| {
                    DbError::storage(format!("unknown column {column} on table {}", schema.name))
                })?;
            row.get(position).cloned().ok_or_else(|| {
                DbError::storage(format!(
                    "row for table {} is missing column {column}",
                    schema.name
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::engine::txn::TransactionId;
    use crate::common::types::{ColumnDef, ColumnType, IndexMeta, RowId, Schema, Value};
    use crate::storage::v2::tx_types::TxnSnapshot;

    use super::{
        decode_index_key, decode_row, decode_row_ids, decode_schema, encode_index_key, encode_row,
        encode_row_ids, encode_schema, project_index_key, visible_row, mark_row_deleted,
        RowVersion, StoredRow, VersionedRow,
    };

    #[test]
    fn row_and_schema_codecs_roundtrip_values() {
        let row = vec![
            Value::Integer(1),
            Value::from("alice"),
            Value::Boolean(true),
        ];
        let schema = Schema::new(
            "users",
            vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("name", ColumnType::Text),
                ColumnDef::new("active", ColumnType::Boolean),
            ],
        );

        assert_eq!(decode_row(&encode_row(&row).unwrap()).unwrap(), row);
        assert_eq!(
            decode_schema(&encode_schema(&schema).unwrap()).unwrap(),
            schema
        );
    }

    #[test]
    fn codecs_reject_invalid_json_payloads() {
        assert!(decode_row(b"not-json").is_err());
        assert!(decode_schema(b"not-json").is_err());
    }

    #[test]
    fn index_key_and_rowid_list_codecs_roundtrip_values() {
        let key = vec![
            Value::from("alice"),
            Value::from("a@example.com"),
            Value::Boolean(true),
        ];
        let row_ids = vec![RowId(1), RowId(7), RowId(11)];

        assert_eq!(
            decode_index_key(&encode_index_key(&key).unwrap()).unwrap(),
            key
        );
        assert_eq!(
            decode_row_ids(&encode_row_ids(&row_ids).unwrap()).unwrap(),
            row_ids
        );
    }

    #[test]
    fn project_index_key_extracts_values_in_index_column_order() {
        let schema = Schema::new(
            "users",
            vec![
                ColumnDef::primary_key("id", ColumnType::Integer),
                ColumnDef::new("email", ColumnType::Text),
                ColumnDef::new("name", ColumnType::Text),
            ],
        );
        let row = vec![
            Value::Integer(1),
            Value::from("a@example.com"),
            Value::from("alice"),
        ];
        let index = IndexMeta {
            name: "idx_users_name_email".to_string(),
            columns: vec!["name".to_string(), "email".to_string()],
            unique: false,
        };

        assert_eq!(
            project_index_key(&schema, &index, &row).unwrap(),
            vec![Value::from("alice"), Value::from("a@example.com")]
        );
    }

    #[test]
    fn visible_row_hides_versions_from_transactions_active_in_snapshot() {
        let row = vec![Value::Integer(1), Value::from("alice")];
        let bytes = serde_json::to_vec(&StoredRow::Versioned(VersionedRow {
            versions: vec![RowVersion {
                row: row.clone(),
                created_by_txn: 7,
                created_commit_ts: Some(7),
                deleted_by_txn: None,
                deleted_commit_ts: None,
            }],
        }))
        .unwrap();

        let snapshot = TxnSnapshot {
            visible_up_to: 7,
            active_txns: BTreeSet::from([TransactionId(7)]),
        };

        assert!(visible_row(&bytes, 1, &snapshot).unwrap().is_none());
    }

    #[test]
    fn mark_row_deleted_ignores_delete_from_transactions_active_in_snapshot() {
        let row = vec![Value::Integer(1), Value::from("alice")];
        let bytes = serde_json::to_vec(&StoredRow::Versioned(VersionedRow {
            versions: vec![RowVersion {
                row,
                created_by_txn: 3,
                created_commit_ts: Some(3),
                deleted_by_txn: Some(9),
                deleted_commit_ts: Some(9),
            }],
        }))
        .unwrap();

        let snapshot = TxnSnapshot {
            visible_up_to: 9,
            active_txns: BTreeSet::from([TransactionId(9)]),
        };

        assert!(mark_row_deleted(&bytes, 1, &snapshot).unwrap().is_some());
    }
}
