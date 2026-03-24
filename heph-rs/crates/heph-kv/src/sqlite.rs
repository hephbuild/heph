//! SQLite-based KV store

use crate::{BatchOp, KvBatch, KvError, KvStore, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Arc;

pub struct SqliteKvStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteKvStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| KvError::Database(e.to_string()))?;

        // Create table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS kv (
                key BLOB PRIMARY KEY,
                value BLOB NOT NULL
            )",
            [],
        )
        .map_err(|e| KvError::Database(e.to_string()))?;

        // Create index
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_key ON kv(key)",
            [],
        )
        .map_err(|e| KvError::Database(e.to_string()))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self> {
        Self::open(":memory:")
    }
}

impl KvStore for SqliteKvStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT value FROM kv WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| KvError::Database(e.to_string()))
    }

    fn set(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .map_err(|e| KvError::Database(e.to_string()))?;
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM kv WHERE key = ?1", params![key])
            .map_err(|e| KvError::Database(e.to_string()))?;
        Ok(())
    }
}

impl KvBatch for SqliteKvStore {
    fn write_batch(&self, ops: Vec<BatchOp>) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .map_err(|e| KvError::Database(e.to_string()))?;

        for op in ops {
            match op {
                BatchOp::Set { key, value } => {
                    tx.execute(
                        "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
                        params![&key, &value],
                    )
                    .map_err(|e| KvError::Database(e.to_string()))?;
                }
                BatchOp::Delete { key } => {
                    tx.execute("DELETE FROM kv WHERE key = ?1", params![&key])
                        .map_err(|e| KvError::Database(e.to_string()))?;
                }
            }
        }

        tx.commit()
            .map_err(|e| KvError::Database(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_get() {
        let store = SqliteKvStore::in_memory().unwrap();
        store.set(b"key1", b"value1").unwrap();

        let val = store.get(b"key1").unwrap();
        assert_eq!(val, Some(b"value1".to_vec()));
    }

    #[test]
    fn test_get_missing() {
        let store = SqliteKvStore::in_memory().unwrap();
        let val = store.get(b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_delete() {
        let store = SqliteKvStore::in_memory().unwrap();
        store.set(b"key1", b"value1").unwrap();
        store.delete(b"key1").unwrap();

        let val = store.get(b"key1").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_exists() {
        let store = SqliteKvStore::in_memory().unwrap();
        store.set(b"key1", b"value1").unwrap();

        assert!(store.exists(b"key1").unwrap());
        assert!(!store.exists(b"missing").unwrap());
    }

    #[test]
    fn test_batch() {
        let store = SqliteKvStore::in_memory().unwrap();
        let ops = vec![
            BatchOp::Set {
                key: b"key1".to_vec(),
                value: b"value1".to_vec(),
            },
            BatchOp::Set {
                key: b"key2".to_vec(),
                value: b"value2".to_vec(),
            },
        ];

        store.write_batch(ops).unwrap();

        assert_eq!(store.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(store.get(b"key2").unwrap(), Some(b"value2".to_vec()));
    }

    #[test]
    fn test_update() {
        let store = SqliteKvStore::in_memory().unwrap();
        store.set(b"key1", b"value1").unwrap();
        store.set(b"key1", b"value2").unwrap();

        let val = store.get(b"key1").unwrap();
        assert_eq!(val, Some(b"value2".to_vec()));
    }

    #[test]
    fn test_batch_delete() {
        let store = SqliteKvStore::in_memory().unwrap();
        store.set(b"key1", b"value1").unwrap();
        store.set(b"key2", b"value2").unwrap();

        let ops = vec![BatchOp::Delete {
            key: b"key1".to_vec(),
        }];

        store.write_batch(ops).unwrap();

        assert_eq!(store.get(b"key1").unwrap(), None);
        assert_eq!(store.get(b"key2").unwrap(), Some(b"value2".to_vec()));
    }
}
