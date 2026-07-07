use rustsql::storage::sqlite3::varint::{decode_varint, encode_varint};

#[test]
fn sqlite_varint_roundtrips_boundary_values() {
    for value in [0_u64, 1, 127, 128, 16383, 16384, 1 << 20, (1 << 32) - 1] {
        let encoded = encode_varint(value);
        let (decoded, consumed) = decode_varint(&encoded).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(consumed, encoded.len());
    }
}

#[test]
fn sqlite_varint_uses_dedicated_nine_byte_path() {
    let encoded = encode_varint(u64::MAX);
    assert_eq!(encoded, vec![0xff; 9]);

    let (decoded, consumed) = decode_varint(&encoded).unwrap();
    assert_eq!(decoded, u64::MAX);
    assert_eq!(consumed, 9);
}

#[test]
fn sqlite_varint_rejects_truncated_input() {
    assert!(decode_varint(&[]).is_err());
    assert!(decode_varint(&[0x81]).is_err());

    let nine_byte = encode_varint(u64::MAX);
    for truncated_len in 1..nine_byte.len() {
        assert!(decode_varint(&nine_byte[..truncated_len]).is_err());
    }
}

#[test]
fn sqlite_varint_rejects_non_canonical_encodings() {
    for bytes in [
        vec![0x80, 0x00],
        vec![0x80, 0x81, 0x00],
        vec![0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
    ] {
        assert!(
            decode_varint(&bytes).is_err(),
            "accepted non-canonical bytes: {bytes:02x?}"
        );
    }
}
