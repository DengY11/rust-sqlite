use crate::common::error::{DbError, Result};
use crate::common::types::Value;

use super::varint::{decode_varint, encode_varint};

pub fn encode_record(values: &[Value]) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    let mut payload = Vec::new();

    for value in values {
        let serial_type = match value {
            Value::Null => 0,
            Value::Boolean(false) => 8,
            Value::Boolean(true) => 9,
            Value::Integer(number) => {
                let serial_type = integer_serial_type(*number);
                append_integer_payload(&mut payload, *number, serial_type);
                serial_type
            }
            Value::Real(number) => {
                payload.extend_from_slice(&number.to_be_bytes());
                7
            }
            Value::Blob(blob) => {
                let blob_len = u64::try_from(blob.len())
                    .map_err(|_| DbError::storage("sqlite blob value is too large"))?;
                payload.extend_from_slice(blob);
                blob_len
                    .checked_mul(2)
                    .and_then(|value| value.checked_add(12))
                    .ok_or_else(|| DbError::storage("sqlite blob serial type overflow"))?
            }
            Value::Text(text) => {
                let text_len = u64::try_from(text.len())
                    .map_err(|_| DbError::storage("sqlite text value is too large"))?;
                payload.extend_from_slice(text.as_bytes());
                text_len
                    .checked_mul(2)
                    .and_then(|value| value.checked_add(13))
                    .ok_or_else(|| DbError::storage("sqlite text serial type overflow"))?
            }
        };

        header.extend_from_slice(&encode_varint(serial_type));
    }

    let mut header_size = header.len() + 1;
    loop {
        let size_varint_len = encode_varint(
            u64::try_from(header_size)
                .map_err(|_| DbError::storage("sqlite record header is too large"))?,
        )
        .len();
        let total_size = header
            .len()
            .checked_add(size_varint_len)
            .ok_or_else(|| DbError::storage("sqlite record header is too large"))?;

        if total_size == header_size {
            break;
        }

        header_size = total_size;
    }

    let mut out = encode_varint(
        u64::try_from(header_size)
            .map_err(|_| DbError::storage("sqlite record header is too large"))?,
    );
    out.extend_from_slice(&header);
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_record(bytes: &[u8]) -> Result<Vec<Value>> {
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

    let mut payload_cursor = header_end;
    let mut values = Vec::with_capacity(serials.len());
    for serial in serials {
        match serial {
            0 => values.push(Value::Null),
            1..=6 => {
                let len = integer_len(serial);
                let end = payload_cursor
                    .checked_add(len)
                    .ok_or_else(|| DbError::storage("invalid sqlite integer record length"))?;
                let slice = bytes
                    .get(payload_cursor..end)
                    .ok_or_else(|| DbError::storage("invalid sqlite integer record length"))?;
                let number = decode_integer(slice);
                values.push(Value::Integer(number));
                payload_cursor = end;
            }
            7 => {
                let end = payload_cursor
                    .checked_add(8)
                    .ok_or_else(|| DbError::storage("invalid sqlite real record length"))?;
                let slice = bytes
                    .get(payload_cursor..end)
                    .ok_or_else(|| DbError::storage("invalid sqlite real record length"))?;
                let number = f64::from_be_bytes(
                    slice
                        .try_into()
                        .map_err(|_| DbError::storage("invalid sqlite real record length"))?,
                );
                values.push(Value::Real(number));
                payload_cursor = end;
            }
            8 => values.push(Value::Boolean(false)),
            9 => values.push(Value::Boolean(true)),
            serial if serial >= 12 && serial % 2 == 0 => {
                let len = usize::try_from((serial - 12) / 2)
                    .map_err(|_| DbError::storage("invalid sqlite blob record length"))?;
                let end = payload_cursor
                    .checked_add(len)
                    .ok_or_else(|| DbError::storage("invalid sqlite blob record length"))?;
                let slice = bytes
                    .get(payload_cursor..end)
                    .ok_or_else(|| DbError::storage("invalid sqlite blob record length"))?;
                values.push(Value::Blob(slice.to_vec()));
                payload_cursor = end;
            }
            serial if serial >= 13 && serial % 2 == 1 => {
                let len = usize::try_from((serial - 13) / 2)
                    .map_err(|_| DbError::storage("invalid sqlite text record length"))?;
                let end = payload_cursor
                    .checked_add(len)
                    .ok_or_else(|| DbError::storage("invalid sqlite text record length"))?;
                let slice = bytes
                    .get(payload_cursor..end)
                    .ok_or_else(|| DbError::storage("invalid sqlite text record length"))?;
                let text = std::str::from_utf8(slice)
                    .map_err(|_| DbError::storage("invalid utf-8 in sqlite text record"))?;
                values.push(Value::from(text));
                payload_cursor = end;
            }
            other => {
                return Err(DbError::storage(format!(
                    "unsupported sqlite serial type {other}"
                )));
            }
        }
    }

    if payload_cursor != bytes.len() {
        return Err(DbError::storage(
            "invalid sqlite record: trailing bytes after payload",
        ));
    }

    Ok(values)
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

fn integer_serial_type(value: i64) -> u64 {
    if value == 0 {
        return 8;
    }
    if value == 1 {
        return 9;
    }

    if (-128..=127).contains(&value) {
        1
    } else if (-32_768..=32_767).contains(&value) {
        2
    } else if (-8_388_608..=8_388_607).contains(&value) {
        3
    } else if (-2_147_483_648..=2_147_483_647).contains(&value) {
        4
    } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&value) {
        5
    } else {
        6
    }
}

fn append_integer_payload(payload: &mut Vec<u8>, value: i64, serial_type: u64) {
    match serial_type {
        8 | 9 => {}
        1..=6 => {
            let bytes = value.to_be_bytes();
            let len = integer_len(serial_type);
            payload.extend_from_slice(&bytes[8 - len..]);
        }
        _ => unreachable!("invalid sqlite integer serial type"),
    }
}
