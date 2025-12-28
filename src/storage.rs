//! Storage implementation for the zenoh-backend-redb storage backend.
//!
//! This implementation separates payload and metadata (data_info) into different tables,
//! similar to the RocksDB backend design using column families.

use crate::config::RedbStorageConfig;
use crate::error::{RedbBackendError, Result};
use redb::{Database, ReadableTable, TableDefinition};
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, trace, warn};
use zenoh::bytes::{Encoding, ZBytes};
use zenoh::internal::buffers::ZSlice;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use zenoh_ext::{z_deserialize, z_serialize};

// Thread-local buffers for zero-allocation PUT/GET operations
thread_local! {
    static KEY_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(256));
    static VALUE_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(1024));
}

/// Table definition for storing payloads.
/// Key: Zenoh key expression as bytes
/// Value: Raw payload bytes
const PAYLOADS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("payloads");

/// Table definition for storing data info (metadata).
/// Key: Zenoh key expression as bytes
/// Value: Serialized DataInfo (timestamp, encoding, deleted flag)
const DATA_INFO_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("data_info");

/// Metadata associated with a stored value.
/// This matches the RocksDB backend's DataInfo structure.
#[derive(Debug, Clone)]
struct DataInfo {
    /// Zenoh timestamp with both time and ID components
    pub timestamp: Timestamp,
    /// Whether this entry represents a deletion (tombstone)
    pub deleted: bool,
    /// Encoding format of the payload
    pub encoding: Encoding,
}

/// Tuple representation for serialization of DataInfo.
/// Format: (timestamp_time, timestamp_id, deleted, encoding_id, encoding_schema)
type DataInfoTuple = (u64, [u8; 16], bool, u16, Vec<u8>);

impl DataInfo {
    /// Convert DataInfo to tuple format for serialization.
    pub fn as_tuple(&self) -> DataInfoTuple {
        let timestamp_time = self.timestamp.get_time().as_u64();
        let timestamp_id = self.timestamp.get_id().to_le_bytes();
        let encoding_id = self.encoding.id();
        let encoding_schema = self
            .encoding
            .schema()
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let deleted = self.deleted;
        (
            timestamp_time,
            timestamp_id,
            deleted,
            encoding_id,
            encoding_schema,
        )
    }

    /// Create DataInfo from tuple format during deserialization.
    pub fn from_tuple(
        (timestamp_time, timestamp_id, deleted, encoding_id, encoding_schema): DataInfoTuple,
    ) -> Result<Self> {
        let timestamp_id = TimestampId::try_from(timestamp_id)
            .map_err(|e| RedbBackendError::serialization(format!("Invalid timestamp ID: {}", e)))?;
        let timestamp = Timestamp::new(NTP64(timestamp_time), timestamp_id);
        let encoding_schema = if encoding_schema.is_empty() {
            None
        } else {
            Some(ZSlice::from(encoding_schema))
        };
        let encoding = Encoding::new(encoding_id, encoding_schema);
        Ok(DataInfo {
            timestamp,
            deleted,
            encoding,
        })
    }
}

/// Encode DataInfo into bytes using Zenoh's serialization.
fn encode_data_info(encoding: Encoding, timestamp: &Timestamp, deleted: bool) -> Result<Vec<u8>> {
    let data_info = DataInfo {
        timestamp: *timestamp,
        deleted,
        encoding,
    };
    let bytes = z_serialize(&data_info.as_tuple());
    Ok(bytes.to_bytes().into_owned())
}

/// Decode DataInfo from bytes.
fn decode_data_info(buf: &[u8]) -> Result<(Encoding, Timestamp, bool)> {
    let bytes = ZBytes::from(buf);
    let tuple: DataInfoTuple = z_deserialize(&bytes).map_err(|_| {
        RedbBackendError::serialization(
            "Failed to decode data-info (encoding, deleted, timestamp)".to_string(),
        )
    })?;
    let data_info = DataInfo::from_tuple(tuple)?;
    Ok((data_info.encoding, data_info.timestamp, data_info.deleted))
}

/// Represents a value stored in the database with associated metadata.
#[derive(Debug, Clone)]
pub struct StoredValue {
    /// The actual payload data
    pub payload: Vec<u8>,
    /// Zenoh timestamp with both time and ID
    pub timestamp: Timestamp,
    /// Encoding format identifier
    pub encoding: Encoding,
}

impl StoredValue {
    /// Create a new stored value.
    pub fn new(payload: Vec<u8>, timestamp: Timestamp, encoding: Encoding) -> Self {
        Self {
            payload,
            timestamp,
            encoding,
        }
    }

    /// Get the timestamp.
    pub fn timestamp(&self) -> &Timestamp {
        &self.timestamp
    }

    /// Get the encoding.
    pub fn encoding(&self) -> &Encoding {
        &self.encoding
    }

    /// Get the payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// The main storage implementation using redb.
pub struct RedbStorage {
    /// The redb database instance
    db: Arc<Database>,

    /// Storage configuration
    config: RedbStorageConfig,

    /// Storage name for logging
    name: String,
}

impl RedbStorage {
    /// Create a new RedbStorage instance.
    pub fn new<P: AsRef<Path>>(path: P, config: RedbStorageConfig, name: String) -> Result<Self> {
        info!("Creating redb storage at: {:?}", path.as_ref());

        let db = Database::create(path.as_ref())?;

        // Initialize both tables
        let write_txn = db.begin_write()?;
        {
            // Create both tables if they don't exist
            write_txn.open_table(PAYLOADS_TABLE)?;
            write_txn.open_table(DATA_INFO_TABLE)?;
        }
        write_txn.commit()?;

        info!("Redb storage created successfully");

        Ok(Self {
            db: Arc::new(db),
            config,
            name,
        })
    }

    /// Get the storage name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the storage configuration.
    pub fn config(&self) -> &RedbStorageConfig {
        &self.config
    }

    /// Store a key-value pair with metadata.
    pub fn put(&self, key: &str, value: StoredValue) -> Result<()> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        trace!("Putting key: {}", key);

        // Use thread-local buffers to avoid allocations
        KEY_BUFFER.with(|key_buf| {
            VALUE_BUFFER.with(|_val_buf| {
                let mut key_buf = key_buf.borrow_mut();

                // Encode key into reusable buffer
                key_buf.clear();
                self.encode_key_into(key, &mut key_buf)?;

                // Encode data_info
                let data_info_bytes = encode_data_info(
                    value.encoding.clone(),
                    &value.timestamp,
                    false, // not deleted
                )?;

                let write_txn = self.db.begin_write()?;
                {
                    // Store payload
                    let mut payloads_table = write_txn.open_table(PAYLOADS_TABLE)?;
                    payloads_table.insert(key_buf.as_slice(), value.payload.as_slice())?;

                    // Store data_info
                    let mut data_info_table = write_txn.open_table(DATA_INFO_TABLE)?;
                    data_info_table.insert(key_buf.as_slice(), data_info_bytes.as_slice())?;
                }
                write_txn.commit()?;

                debug!("Stored key: {}", key);
                Ok(())
            })
        })
    }

    /// Retrieve a value by its exact key.
    pub fn get(&self, key: &str) -> Result<Option<StoredValue>> {
        trace!("Getting key: {}", key);

        // Use thread-local buffer to avoid allocation
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let read_txn = self.db.begin_read()?;
            let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
            let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

            // Try to get both payload and data_info
            let payload_result = payloads_table.get(key_buf.as_slice())?;
            let data_info_result = data_info_table.get(key_buf.as_slice())?;

            match (payload_result, data_info_result) {
                (Some(payload_guard), Some(info_guard)) => {
                    let payload_bytes = payload_guard.value();
                    let info_bytes = info_guard.value();

                    let (encoding, timestamp, deleted) = decode_data_info(info_bytes)?;

                    if deleted {
                        // This is a tombstone, treat as not found
                        trace!("Key found but marked as deleted: {}", key);
                        Ok(None)
                    } else {
                        let stored_value = StoredValue::new(
                            payload_bytes.to_vec(),
                            timestamp,
                            encoding,
                        );
                        debug!("Found key: {}", key);
                        Ok(Some(stored_value))
                    }
                }
                (Some(_), None) => {
                    warn!("Payload exists but data_info missing for key: {} - possible database corruption", key);
                    Ok(None)
                }
                (None, Some(_)) => {
                    // Data info exists but no payload - treat as deleted or corrupted
                    trace!("Data info exists but no payload for key: {}", key);
                    Ok(None)
                }
                (None, None) => {
                    trace!("Key not found: {}", key);
                    Ok(None)
                }
            }
        })
    }

    /// Delete a key-value pair.
    pub fn delete(&self, key: &str) -> Result<()> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        trace!("Deleting key: {}", key);

        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let write_txn = self.db.begin_write()?;
            {
                // Delete from both tables
                let mut payloads_table = write_txn.open_table(PAYLOADS_TABLE)?;
                let mut data_info_table = write_txn.open_table(DATA_INFO_TABLE)?;

                payloads_table.remove(key_buf.as_slice())?;
                data_info_table.remove(key_buf.as_slice())?;
            }
            write_txn.commit()?;

            debug!("Deleted key: {}", key);
            Ok(())
        })
    }

    /// Retrieve all key-value pairs from the storage.
    pub fn get_all(&self) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting all entries");

        let read_txn = self.db.begin_read()?;
        let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut results = Vec::new();

        // Iterate over data_info table (it's the authoritative source for what exists)
        for item in data_info_table.iter()? {
            let (key_bytes, info_bytes) = item?;
            let key = self.decode_key(key_bytes.value())?;

            let (encoding, timestamp, deleted) = decode_data_info(info_bytes.value())?;

            if !deleted {
                // Get the payload
                if let Some(payload_guard) = payloads_table.get(key_bytes.value())? {
                    let payload_bytes = payload_guard.value();
                    let stored_value =
                        StoredValue::new(payload_bytes.to_vec(), timestamp, encoding);
                    results.push((key, stored_value));
                } else {
                    warn!(
                        "Data info exists but no payload for key: {} - skipping",
                        key
                    );
                }
            }
        }

        debug!("Retrieved {} entries", results.len());
        Ok(results)
    }

    /// Retrieve all key-value pairs matching a given prefix.
    pub fn get_by_prefix(&self, prefix: &str) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting entries by prefix: {}", prefix);

        let read_txn = self.db.begin_read()?;
        let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut results = Vec::new();

        for item in data_info_table.iter()? {
            let (key_bytes, info_bytes) = item?;
            let key = self.decode_key(key_bytes.value())?;

            if key.starts_with(prefix) {
                let (encoding, timestamp, deleted) = decode_data_info(info_bytes.value())?;

                if !deleted && let Some(payload_guard) = payloads_table.get(key_bytes.value())? {
                    let payload_bytes = payload_guard.value();
                    let stored_value =
                        StoredValue::new(payload_bytes.to_vec(), timestamp, encoding);
                    results.push((key, stored_value));
                }
            }
        }

        debug!(
            "Retrieved {} entries with prefix '{}'",
            results.len(),
            prefix
        );
        Ok(results)
    }

    /// Retrieve all key-value pairs matching a wildcard pattern.
    pub fn get_by_wildcard(&self, pattern: &str) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting entries by wildcard: {}", pattern);

        let read_txn = self.db.begin_read()?;
        let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut results = Vec::new();

        for item in data_info_table.iter()? {
            let (key_bytes, info_bytes) = item?;
            let key = self.decode_key(key_bytes.value())?;

            if Self::matches_wildcard(&key, pattern) {
                let (encoding, timestamp, deleted) = decode_data_info(info_bytes.value())?;

                if !deleted && let Some(payload_guard) = payloads_table.get(key_bytes.value())? {
                    let payload_bytes = payload_guard.value();
                    let stored_value =
                        StoredValue::new(payload_bytes.to_vec(), timestamp, encoding);
                    results.push((key, stored_value));
                }
            }
        }

        debug!(
            "Retrieved {} entries matching wildcard '{}'",
            results.len(),
            pattern
        );
        Ok(results)
    }

    /// Count the total number of key-value pairs in storage.
    pub fn count(&self) -> Result<usize> {
        let read_txn = self.db.begin_read()?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut count = 0;
        for item in data_info_table.iter()? {
            let (_, info_bytes) = item?;
            let (_, _, deleted) = decode_data_info(info_bytes.value())?;
            if !deleted {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Clear all entries from the storage.
    pub fn clear(&self) -> Result<()> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        info!("Clearing all entries from storage");

        let write_txn = self.db.begin_write()?;
        {
            // Delete and recreate both tables - much more efficient than removing keys one by one
            write_txn.delete_table(PAYLOADS_TABLE)?;
            write_txn.delete_table(DATA_INFO_TABLE)?;

            // Recreate the tables
            write_txn.open_table(PAYLOADS_TABLE)?;
            write_txn.open_table(DATA_INFO_TABLE)?;
        }
        write_txn.commit()?;

        info!("Storage cleared");
        Ok(())
    }

    /// Encode a key string into an existing buffer (zero-allocation).
    fn encode_key_into(&self, key: &str, buffer: &mut Vec<u8>) -> Result<()> {
        buffer.extend_from_slice(key.as_bytes());
        Ok(())
    }

    /// Decode key bytes back to a string.
    fn decode_key(&self, bytes: &[u8]) -> Result<String> {
        String::from_utf8(bytes.to_vec())
            .map_err(|e| RedbBackendError::serialization(format!("Invalid UTF-8 in key: {}", e)))
    }

    /// Check if a key matches a wildcard pattern.
    /// Supports '*' and '**' wildcards.
    fn matches_wildcard(key: &str, pattern: &str) -> bool {
        let key_parts: Vec<&str> = key.split('/').collect();
        let pattern_parts: Vec<&str> = pattern.split('/').collect();
        matches_parts(&key_parts, &pattern_parts)
    }
}

/// Recursive helper function for wildcard matching.
fn matches_parts(key_parts: &[&str], pattern_parts: &[&str]) -> bool {
    match (key_parts.first(), pattern_parts.first()) {
        (None, None) => true,
        (Some(_), None) => false,
        (None, Some(&"**")) => matches_parts(key_parts, &pattern_parts[1..]),
        (None, Some(_)) => false,
        (Some(_), Some(&"**")) => {
            matches_parts(key_parts, &pattern_parts[1..])
                || matches_parts(&key_parts[1..], pattern_parts)
        }
        (Some(_), Some(&"*")) => matches_parts(&key_parts[1..], &pattern_parts[1..]),
        (Some(&key_part), Some(&pattern_part)) => {
            key_part == pattern_part && matches_parts(&key_parts[1..], &pattern_parts[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zenoh::time::TimestampId;

    fn create_test_storage() -> (RedbStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let config = RedbStorageConfig::default();
        let storage = RedbStorage::new(db_path, config, "test".to_string()).unwrap();
        (storage, temp_dir)
    }

    #[test]
    fn test_put_and_get() {
        let (storage, _temp) = create_test_storage();

        let timestamp = Timestamp::new(NTP64(123456789), TimestampId::rand());
        let encoding = Encoding::ZENOH_BYTES;
        let payload = b"test data".to_vec();

        let value = StoredValue::new(payload.clone(), timestamp, encoding.clone());
        storage.put("test/key", value).unwrap();

        let retrieved = storage.get("test/key").unwrap().unwrap();
        assert_eq!(retrieved.payload, payload);
        assert_eq!(retrieved.timestamp, timestamp);
        assert_eq!(retrieved.encoding.id(), encoding.id());
    }

    #[test]
    fn test_delete() {
        let (storage, _temp) = create_test_storage();

        let timestamp = Timestamp::new(NTP64(123456789), TimestampId::rand());
        let value = StoredValue::new(b"data".to_vec(), timestamp, Encoding::ZENOH_BYTES);
        storage.put("test/key", value).unwrap();

        storage.delete("test/key").unwrap();
        assert!(storage.get("test/key").unwrap().is_none());
    }

    #[test]
    fn test_get_all() {
        let (storage, _temp) = create_test_storage();

        let timestamp = Timestamp::new(NTP64(123456789), TimestampId::rand());
        let value1 = StoredValue::new(b"data1".to_vec(), timestamp, Encoding::ZENOH_BYTES);
        let value2 = StoredValue::new(b"data2".to_vec(), timestamp, Encoding::ZENOH_BYTES);

        storage.put("key1", value1).unwrap();
        storage.put("key2", value2).unwrap();

        let all = storage.get_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_get_by_prefix() {
        let (storage, _temp) = create_test_storage();

        let timestamp = Timestamp::new(NTP64(123456789), TimestampId::rand());
        let value = StoredValue::new(b"data".to_vec(), timestamp, Encoding::ZENOH_BYTES);

        storage.put("test/foo", value.clone()).unwrap();
        storage.put("test/bar", value.clone()).unwrap();
        storage.put("other/baz", value).unwrap();

        let results = storage.get_by_prefix("test/").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_wildcard_matching() {
        assert!(RedbStorage::matches_wildcard("a/b/c", "a/b/c"));
        assert!(RedbStorage::matches_wildcard("a/b/c", "a/*/c"));
        assert!(RedbStorage::matches_wildcard("a/b/c", "a/**/c"));
        assert!(RedbStorage::matches_wildcard("a/b/c/d", "a/**/d"));
        assert!(!RedbStorage::matches_wildcard("a/b/c", "a/b/d"));
    }

    #[test]
    fn test_matches_parts_function() {
        let key_parts = vec!["a", "b", "c"];
        let pattern_parts = vec!["a", "*", "c"];
        assert!(matches_parts(&key_parts, &pattern_parts));
    }

    #[test]
    fn test_count() {
        let (storage, _temp) = create_test_storage();

        let timestamp = Timestamp::new(NTP64(123456789), TimestampId::rand());
        let value = StoredValue::new(b"data".to_vec(), timestamp, Encoding::ZENOH_BYTES);

        storage.put("key1", value.clone()).unwrap();
        storage.put("key2", value.clone()).unwrap();
        storage.put("key3", value).unwrap();

        assert_eq!(storage.count().unwrap(), 3);

        storage.delete("key2").unwrap();
        assert_eq!(storage.count().unwrap(), 2);
    }

    #[test]
    fn test_clear() {
        let (storage, _temp) = create_test_storage();

        let timestamp = Timestamp::new(NTP64(123456789), TimestampId::rand());
        let value = StoredValue::new(b"data".to_vec(), timestamp, Encoding::ZENOH_BYTES);

        storage.put("key1", value.clone()).unwrap();
        storage.put("key2", value).unwrap();

        storage.clear().unwrap();
        assert_eq!(storage.count().unwrap(), 0);
    }
}
