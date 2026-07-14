use std::collections::HashSet;

use crate::common::error::{DbError, Result};

const JSONB_NULL: u8 = 0;
const JSONB_TRUE: u8 = 1;
const JSONB_FALSE: u8 = 2;
const JSONB_INT: u8 = 3;
const JSONB_FLOAT: u8 = 5;
const JSONB_TEXT: u8 = 7;
const JSONB_TEXT_ESCAPED: u8 = 8;
const JSONB_TEXT_RAW: u8 = 10;
const JSONB_ARRAY: u8 = 11;
const JSONB_OBJECT: u8 = 12;

pub(crate) fn encode(value: &serde_json::Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_value(value, &mut out, "", &HashSet::new())?;
    Ok(out)
}

pub(crate) fn encode_with_raw_object_keys(
    value: &serde_json::Value,
    raw_object_keys: &HashSet<String>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_value(value, &mut out, "", raw_object_keys)?;
    Ok(out)
}

pub(crate) fn encode_object_entries_from_json_fragments(fields: &[String]) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    for field in fields {
        let (key, value) = parse_object_field_fragment(field)?;
        encode_string(&key, &mut payload)?;
        encode_value(&value, &mut payload, "", &HashSet::new())?;
    }
    let mut out = Vec::new();
    write_element(&mut out, JSONB_OBJECT, &payload)?;
    Ok(out)
}

pub(crate) fn object_key_path(parent: &str, key: &str) -> String {
    format!("{parent}K{}:{key}", key.len())
}

pub(crate) fn array_index_path(parent: &str, index: usize) -> String {
    format!("{parent}I{index};")
}

pub(crate) fn decode(bytes: &[u8]) -> Result<serde_json::Value> {
    let (value, consumed) = decode_at(bytes, 0)?;
    if consumed != bytes.len() {
        return Err(DbError::plan("malformed JSONB"));
    }
    Ok(value)
}

pub(crate) fn to_json_text(bytes: &[u8]) -> Result<String> {
    let (rendered, consumed) = render_at(bytes, 0)?;
    if consumed != bytes.len() {
        return Err(DbError::plan("malformed JSONB"));
    }
    Ok(rendered)
}

fn encode_value(
    value: &serde_json::Value,
    out: &mut Vec<u8>,
    path: &str,
    raw_object_keys: &HashSet<String>,
) -> Result<()> {
    match value {
        serde_json::Value::Null => write_element(out, JSONB_NULL, &[]),
        serde_json::Value::Bool(true) => write_element(out, JSONB_TRUE, &[]),
        serde_json::Value::Bool(false) => write_element(out, JSONB_FALSE, &[]),
        serde_json::Value::Number(value) if value.is_f64() => {
            write_element(out, JSONB_FLOAT, value.to_string().as_bytes())
        }
        serde_json::Value::Number(value) => {
            write_element(out, JSONB_INT, value.to_string().as_bytes())
        }
        serde_json::Value::String(value) => encode_string(value, out),
        serde_json::Value::Array(values) => {
            let mut payload = Vec::new();
            for (index, value) in values.iter().enumerate() {
                let item_path = array_index_path(path, index);
                encode_value(value, &mut payload, &item_path, raw_object_keys)?;
            }
            write_element(out, JSONB_ARRAY, &payload)
        }
        serde_json::Value::Object(object) => {
            let mut payload = Vec::new();
            for (key, value) in object {
                let key_path = object_key_path(path, key);
                if raw_object_keys.contains(&key_path) {
                    encode_raw_string(key, &mut payload)?;
                } else {
                    encode_string(key, &mut payload)?;
                }
                encode_value(value, &mut payload, &key_path, raw_object_keys)?;
            }
            write_element(out, JSONB_OBJECT, &payload)
        }
    }
}

fn encode_string(value: &str, out: &mut Vec<u8>) -> Result<()> {
    let quoted = serde_json::to_string(value)
        .map_err(|error| DbError::plan(format!("failed to render JSONB string: {error}")))?;
    let inner = quoted
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| DbError::plan("failed to render JSONB string"))?;
    if inner == value {
        write_element(out, JSONB_TEXT, value.as_bytes())
    } else {
        write_element(out, JSONB_TEXT_ESCAPED, inner.as_bytes())
    }
}

fn encode_raw_string(value: &str, out: &mut Vec<u8>) -> Result<()> {
    write_element(out, JSONB_TEXT_RAW, value.as_bytes())
}

fn parse_object_field_fragment(field: &str) -> Result<(String, serde_json::Value)> {
    let wrapped = format!("{{{field}}}");
    let parsed = serde_json::from_str::<serde_json::Value>(&wrapped)
        .map_err(|error| DbError::plan(format!("malformed JSON object field: {error}")))?;
    let serde_json::Value::Object(object) = parsed else {
        return Err(DbError::plan("malformed JSON object field"));
    };
    if object.len() != 1 {
        return Err(DbError::plan("malformed JSON object field"));
    }
    object
        .into_iter()
        .next()
        .ok_or_else(|| DbError::plan("malformed JSON object field"))
}

fn write_element(out: &mut Vec<u8>, element_type: u8, payload: &[u8]) -> Result<()> {
    if element_type > 0x0f {
        return Err(DbError::plan("invalid JSONB element type"));
    }
    let length = payload.len();
    if length <= 11 {
        out.push(((length as u8) << 4) | element_type);
    } else if let Ok(length) = u8::try_from(length) {
        out.push(0xc0 | element_type);
        out.push(length);
    } else if let Ok(length) = u16::try_from(length) {
        out.push(0xd0 | element_type);
        out.extend_from_slice(&length.to_be_bytes());
    } else if let Ok(length) = u32::try_from(length) {
        out.push(0xe0 | element_type);
        out.extend_from_slice(&length.to_be_bytes());
    } else {
        let length = u64::try_from(length)
            .map_err(|_| DbError::plan("JSONB payload length is too large"))?;
        out.push(0xf0 | element_type);
        out.extend_from_slice(&length.to_be_bytes());
    }
    out.extend_from_slice(payload);
    Ok(())
}

fn decode_at(bytes: &[u8], offset: usize) -> Result<(serde_json::Value, usize)> {
    let (element_type, payload_start, payload_end) = read_header(bytes, offset)?;
    let payload = &bytes[payload_start..payload_end];
    let value = match element_type {
        JSONB_NULL => {
            require_empty_payload(payload)?;
            serde_json::Value::Null
        }
        JSONB_TRUE => {
            require_empty_payload(payload)?;
            serde_json::Value::Bool(true)
        }
        JSONB_FALSE => {
            require_empty_payload(payload)?;
            serde_json::Value::Bool(false)
        }
        JSONB_INT | JSONB_FLOAT => {
            let text = std::str::from_utf8(payload)
                .map_err(|_| DbError::plan("malformed JSONB number"))?;
            let value = serde_json::from_str::<serde_json::Value>(text)
                .map_err(|_| DbError::plan("malformed JSONB number"))?;
            if !value.is_number() {
                return Err(DbError::plan("malformed JSONB number"));
            }
            value
        }
        JSONB_TEXT | JSONB_TEXT_RAW => {
            let text =
                std::str::from_utf8(payload).map_err(|_| DbError::plan("malformed JSONB text"))?;
            serde_json::Value::String(text.to_string())
        }
        JSONB_TEXT_ESCAPED => {
            let text =
                std::str::from_utf8(payload).map_err(|_| DbError::plan("malformed JSONB text"))?;
            let quoted = format!("\"{text}\"");
            serde_json::from_str::<serde_json::Value>(&quoted)
                .map_err(|_| DbError::plan("malformed JSONB text"))?
        }
        JSONB_ARRAY => decode_array(payload)?,
        JSONB_OBJECT => decode_object(payload)?,
        _ => return Err(DbError::plan("unsupported JSONB element type")),
    };
    Ok((value, payload_end))
}

fn read_header(bytes: &[u8], offset: usize) -> Result<(u8, usize, usize)> {
    let Some(header) = bytes.get(offset).copied() else {
        return Err(DbError::plan("malformed JSONB"));
    };
    let element_type = header & 0x0f;
    let length_marker = header >> 4;
    let (payload_start, payload_len) = match length_marker {
        0..=11 => (offset + 1, usize::from(length_marker)),
        12 => {
            let length = read_be_length(bytes, offset + 1, 1)?;
            (offset + 2, length)
        }
        13 => {
            let length = read_be_length(bytes, offset + 1, 2)?;
            (offset + 3, length)
        }
        14 => {
            let length = read_be_length(bytes, offset + 1, 4)?;
            (offset + 5, length)
        }
        15 => {
            let length = read_be_length(bytes, offset + 1, 8)?;
            (offset + 9, length)
        }
        _ => unreachable!("4-bit marker is always in range"),
    };
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or_else(|| DbError::plan("malformed JSONB"))?;
    if payload_end > bytes.len() {
        return Err(DbError::plan("malformed JSONB"));
    }
    Ok((element_type, payload_start, payload_end))
}

fn read_be_length(bytes: &[u8], start: usize, width: usize) -> Result<usize> {
    let Some(raw) = bytes.get(start..start + width) else {
        return Err(DbError::plan("malformed JSONB"));
    };
    let mut value = 0_u64;
    for byte in raw {
        value = (value << 8) | u64::from(*byte);
    }
    usize::try_from(value).map_err(|_| DbError::plan("JSONB payload length is too large"))
}

fn require_empty_payload(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(DbError::plan("malformed JSONB"))
    }
}

fn decode_array(payload: &[u8]) -> Result<serde_json::Value> {
    let mut values = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let (value, consumed) = decode_at(payload, offset)?;
        values.push(value);
        offset = consumed;
    }
    Ok(serde_json::Value::Array(values))
}

fn decode_object(payload: &[u8]) -> Result<serde_json::Value> {
    let mut object = serde_json::Map::new();
    let mut offset = 0;
    while offset < payload.len() {
        let (key, consumed_key) = decode_at(payload, offset)?;
        let serde_json::Value::String(key) = key else {
            return Err(DbError::plan("malformed JSONB object key"));
        };
        offset = consumed_key;
        if offset >= payload.len() {
            return Err(DbError::plan("malformed JSONB object"));
        }
        let (value, consumed_value) = decode_at(payload, offset)?;
        object.insert(key, value);
        offset = consumed_value;
    }
    Ok(serde_json::Value::Object(object))
}

fn render_at(bytes: &[u8], offset: usize) -> Result<(String, usize)> {
    let (element_type, payload_start, payload_end) = read_header(bytes, offset)?;
    let payload = &bytes[payload_start..payload_end];
    let rendered = match element_type {
        JSONB_NULL => {
            require_empty_payload(payload)?;
            "null".to_string()
        }
        JSONB_TRUE => {
            require_empty_payload(payload)?;
            "true".to_string()
        }
        JSONB_FALSE => {
            require_empty_payload(payload)?;
            "false".to_string()
        }
        JSONB_INT | JSONB_FLOAT => std::str::from_utf8(payload)
            .map_err(|_| DbError::plan("malformed JSONB number"))?
            .to_string(),
        JSONB_TEXT | JSONB_TEXT_RAW => {
            let text =
                std::str::from_utf8(payload).map_err(|_| DbError::plan("malformed JSONB text"))?;
            serde_json::to_string(text)
                .map_err(|error| DbError::plan(format!("failed to render JSON text: {error}")))?
        }
        JSONB_TEXT_ESCAPED => {
            let text =
                std::str::from_utf8(payload).map_err(|_| DbError::plan("malformed JSONB text"))?;
            let quoted = format!("\"{text}\"");
            let decoded = serde_json::from_str::<String>(&quoted)
                .map_err(|_| DbError::plan("malformed JSONB text"))?;
            serde_json::to_string(&decoded)
                .map_err(|error| DbError::plan(format!("failed to render JSON text: {error}")))?
        }
        JSONB_ARRAY => render_array(payload)?,
        JSONB_OBJECT => render_object(payload)?,
        _ => return Err(DbError::plan("unsupported JSONB element type")),
    };
    Ok((rendered, payload_end))
}

fn render_array(payload: &[u8]) -> Result<String> {
    let mut values = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let (value, consumed) = render_at(payload, offset)?;
        values.push(value);
        offset = consumed;
    }
    Ok(format!("[{}]", values.join(",")))
}

fn render_object(payload: &[u8]) -> Result<String> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let (key, consumed_key) = render_at(payload, offset)?;
        offset = consumed_key;
        if offset >= payload.len() {
            return Err(DbError::plan("malformed JSONB object"));
        }
        let (value, consumed_value) = render_at(payload, offset)?;
        fields.push(format!("{key}:{value}"));
        offset = consumed_value;
    }
    Ok(format!("{{{}}}", fields.join(",")))
}
