use crate::common::error::{DbError, Result};
use crate::common::types::{IndexMeta, Row, RowId, Schema, Value};

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
    use crate::common::types::{ColumnDef, ColumnType, IndexMeta, RowId, Schema, Value};

    use super::{
        decode_index_key, decode_row, decode_row_ids, decode_schema, encode_index_key, encode_row,
        encode_row_ids, encode_schema, project_index_key,
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
}
