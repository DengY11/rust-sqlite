use rustsql::common::types::Value;
use rustsql::storage::sqlite3::record::{decode_record, encode_record};

fn assert_storage_error_contains(bytes: &[u8], expected: &str) {
    let error = decode_record(bytes).unwrap_err();
    assert!(
        error.to_string().contains(expected),
        "expected error containing {expected:?}, got {error}"
    );
}

#[test]
fn sqlite_record_roundtrips_integer_text_null_and_boolean_values() {
    let values = vec![
        Value::Integer(7),
        Value::from("alice"),
        Value::Null,
        Value::Boolean(true),
    ];

    let encoded = encode_record(&values).unwrap();
    let decoded = decode_record(&encoded).unwrap();

    assert_eq!(decoded, values);
}

#[test]
fn sqlite_record_roundtrips_blob_values() {
    let values = vec![Value::Blob(vec![0x00, 0x01, 0xfe, 0xff]), Value::Integer(7)];

    let encoded = encode_record(&values).unwrap();
    let decoded = decode_record(&encoded).unwrap();

    assert_eq!(decoded, values);
}

#[test]
fn sqlite_record_roundtrips_real_values() {
    let values = vec![Value::Real(3.25), Value::Real(-0.5)];

    let encoded = encode_record(&values).unwrap();
    let decoded = decode_record(&encoded).unwrap();

    assert_eq!(decoded, values);
}

#[test]
fn sqlite_record_encodes_integers_with_minimal_serial_types() {
    let encoded = encode_record(&[
        Value::Integer(127),
        Value::Integer(256),
        Value::Integer(66_051),
        Value::Integer(16_909_060),
        Value::Integer(1_108_152_157_446),
    ])
    .unwrap();

    assert_eq!(
        encoded,
        vec![
            6, 1, 2, 3, 4, 5, 0x7f, 0x01, 0x00, 0x01, 0x02, 0x03, 0x01, 0x02, 0x03, 0x04, 0x01,
            0x02, 0x03, 0x04, 0x05, 0x06,
        ]
    );
}

#[test]
fn sqlite_record_decodes_compact_integer_serial_types() {
    let decoded = decode_record(&[
        6, 1, 2, 3, 4, 5, 0x7f, 0x01, 0x00, 0x01, 0x02, 0x03, 0x01, 0x02, 0x03, 0x04, 0x01, 0x02,
        0x03, 0x04, 0x05, 0x06,
    ])
    .unwrap();

    assert_eq!(
        decoded,
        vec![
            Value::Integer(127),
            Value::Integer(256),
            Value::Integer(66_051),
            Value::Integer(16_909_060),
            Value::Integer(1_108_152_157_446),
        ]
    );
}

#[test]
fn sqlite_record_rejects_trailing_payload_bytes() {
    let mut encoded = encode_record(&[Value::Integer(7)]).unwrap();
    encoded.push(0);

    assert_storage_error_contains(&encoded, "trailing bytes");
}

#[test]
fn sqlite_record_rejects_truncated_integer_payload() {
    let mut encoded = encode_record(&[Value::Integer(7)]).unwrap();
    encoded.pop();

    assert_storage_error_contains(&encoded, "invalid sqlite integer record length");
}

#[test]
fn sqlite_record_rejects_truncated_text_payload() {
    let mut encoded = encode_record(&[Value::from("alice")]).unwrap();
    encoded.pop();

    assert_storage_error_contains(&encoded, "invalid sqlite text record length");
}

#[test]
fn sqlite_record_rejects_reserved_serial_type() {
    assert_storage_error_contains(&[2, 10], "unsupported sqlite serial type 10");
}

#[test]
fn sqlite_record_rejects_invalid_utf8_text_payload() {
    assert_storage_error_contains(&[2, 15, 0xff], "invalid utf-8 in sqlite text record");
}

#[test]
fn sqlite_record_rejects_invalid_header_size() {
    assert_storage_error_contains(&[0], "invalid sqlite record header size");
}
