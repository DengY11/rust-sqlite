//! Transaction identifiers shared across engine components.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TransactionId(pub u64);

#[cfg(test)]
mod tests {
    use super::TransactionId;

    #[test]
    fn transaction_id_supports_ordering_and_serde_roundtrip() {
        assert!(TransactionId(1) < TransactionId(2));
        let json = serde_json::to_string(&TransactionId(9)).unwrap();
        let decoded: TransactionId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, TransactionId(9));
    }
}
