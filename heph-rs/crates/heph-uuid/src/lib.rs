//! UUID generation for Heph

use uuid::Uuid;

/// Generate a new random UUID v4
pub fn new() -> String {
    Uuid::new_v4().to_string()
}

/// Generate a new UUID v4 as bytes
pub fn new_bytes() -> [u8; 16] {
    *Uuid::new_v4().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_generates_valid_uuid() {
        let uuid = new();
        assert_eq!(uuid.len(), 36); // UUID string length
        assert!(uuid.contains('-'));
    }

    #[test]
    fn test_new_generates_unique_uuids() {
        let uuid1 = new();
        let uuid2 = new();
        assert_ne!(uuid1, uuid2);
    }

    #[test]
    fn test_new_bytes_correct_length() {
        let bytes = new_bytes();
        assert_eq!(bytes.len(), 16);
    }
}
