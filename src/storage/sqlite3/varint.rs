use crate::common::error::{DbError, Result};

fn is_canonical_varint(value: u64, length: usize) -> bool {
    match length {
        1 => true,
        2..=8 => value >= (1_u64 << (7 * (length - 1))),
        9 => value > 0x00ff_ffff_ffff_ffff,
        _ => false,
    }
}

pub fn encode_varint(mut value: u64) -> Vec<u8> {
    if value <= 0x7f {
        return vec![value as u8];
    }

    if value > 0x00ff_ffff_ffff_ffff {
        let mut bytes = [0_u8; 9];
        bytes[8] = (value & 0xff) as u8;
        value >>= 8;

        for index in (0..8).rev() {
            bytes[index] = ((value & 0x7f) as u8) | 0x80;
            value >>= 7;
        }

        return bytes.to_vec();
    }

    let mut bytes = [0_u8; 9];
    let mut index = 8;

    while value > 0 {
        bytes[index] = (value & 0x7f) as u8;
        value >>= 7;

        if value == 0 {
            break;
        }

        index -= 1;
    }

    for byte in &mut bytes[index..8] {
        *byte |= 0x80;
    }

    bytes[index..=8].to_vec()
}

pub fn decode_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0_u64;

    for (index, byte) in bytes.iter().copied().enumerate().take(9) {
        if index == 8 {
            value = (value << 8) | u64::from(byte);
            if is_canonical_varint(value, 9) {
                return Ok((value, 9));
            }

            return Err(DbError::storage("non-canonical sqlite varint"));
        }

        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            let length = index + 1;
            if is_canonical_varint(value, length) {
                return Ok((value, length));
            }

            return Err(DbError::storage("non-canonical sqlite varint"));
        }
    }

    Err(DbError::storage("truncated sqlite varint"))
}
