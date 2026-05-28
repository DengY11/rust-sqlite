use crate::common::error::Result;
use crate::common::types::{Row, Schema};

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
