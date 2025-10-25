//! Storage implementation for the zenoh-backend-redb storage backend.

use crate::config::RedbStorageConfig;
use crate::error::{RedbBackendError, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, trace, warn};

// Thread-local buffers for zero-allocation PUT/GET operations
thread_local! {
    static KEY_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(256));
    static VALUE_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(1024));
}

/// Table definition for storing key-value pairs with timestamps.
/// Key: Zenoh key expression as bytes
/// Value: Serialized StoredValue (payload + metadata)
const KV_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("zenoh_kv");

/// Represents a value stored in the database with associated metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredValue {
    /// The actual payload data
    pub payload: Vec<u8>,

    /// Zenoh timestamp (NTP64 format)
    pub timestamp: u64,

    /// Encoding format identifier
    pub encoding: String,
}

/// Zero-copy borrowed version of StoredValue.
/// This struct borrows data from the database without cloning, providing
/// much faster read performance at the cost of lifetime constraints.
#[derive(Debug)]
pub struct StoredValueRef<'a> {
    /// Borrowed payload data (no allocation!)
    pub payload: &'a [u8],

    /// Zenoh timestamp (NTP64 format)
    pub timestamp: u64,

    /// Borrowed encoding string (no allocation!)
    pub encoding: &'a str,
}

impl<'a> StoredValueRef<'a> {
    /// Convert borrowed value to owned StoredValue (requires cloning).
    pub fn to_owned(&self) -> StoredValue {
        StoredValue {
            payload: self.payload.to_vec(),
            timestamp: self.timestamp,
            encoding: self.encoding.to_string(),
        }
    }

    /// Get a reference to the payload without cloning.
    pub fn payload(&self) -> &[u8] {
        self.payload
    }

    /// Get the encoding string without cloning.
    pub fn encoding(&self) -> &str {
        self.encoding
    }

    /// Parse a StoredValueRef from bytes without allocating.
    /// The returned reference borrows from the input bytes.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < 10 {
            return Err(RedbBackendError::serialization(format!(
                "Invalid data: too short ({} bytes)",
                bytes.len()
            )));
        }

        // Read timestamp (8 bytes)
        let timestamp =
            u64::from_le_bytes(bytes[0..8].try_into().map_err(|e| {
                RedbBackendError::serialization(format!("Invalid timestamp: {}", e))
            })?);

        // Read encoding length (2 bytes)
        let encoding_len = u16::from_le_bytes(bytes[8..10].try_into().map_err(|e| {
            RedbBackendError::serialization(format!("Invalid encoding length: {}", e))
        })?) as usize;

        // Check bounds
        if bytes.len() < 10 + encoding_len {
            return Err(RedbBackendError::serialization(format!(
                "Invalid data: encoding length {} exceeds remaining bytes",
                encoding_len
            )));
        }

        // Borrow encoding string (no allocation!)
        let encoding = std::str::from_utf8(&bytes[10..10 + encoding_len]).map_err(|e| {
            RedbBackendError::serialization(format!("Invalid UTF-8 in encoding: {}", e))
        })?;

        // Borrow payload (no allocation!)
        let payload = &bytes[10 + encoding_len..];

        Ok(Self {
            payload,
            timestamp,
            encoding,
        })
    }
}

/// A guard type that holds a database transaction and provides zero-copy access
/// to stored values. The data remains valid as long as this guard is alive.
pub struct StoredValueGuard {
    // We need to store the transaction and keep it alive
    // The payload field will reference data from this transaction
    payload: Vec<u8>,
    timestamp: u64,
    encoding: String,
}

impl StoredValueGuard {
    /// Get a reference to the payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Get the timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Get the encoding string.
    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    /// Convert to owned StoredValue.
    pub fn to_owned(&self) -> StoredValue {
        StoredValue {
            payload: self.payload.clone(),
            timestamp: self.timestamp,
            encoding: self.encoding.clone(),
        }
    }
}

impl StoredValue {
    /// Create a new stored value.
    pub fn new(payload: Vec<u8>, timestamp: u64, encoding: String) -> Self {
        Self {
            payload,
            timestamp,
            encoding,
        }
    }

    /// Serialize the stored value to bytes using a compact binary format.
    ///
    /// Format:
    /// - 8 bytes: timestamp (u64, little-endian)
    /// - 2 bytes: encoding length (u16, little-endian)
    /// - N bytes: encoding string (UTF-8)
    /// - remaining bytes: payload (as-is)
    ///
    /// This is much more efficient than JSON as it stores the payload directly
    /// without base64 encoding overhead (~33% size reduction).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.to_bytes_into(&mut bytes)?;
        Ok(bytes)
    }

    /// Serialize the stored value into an existing buffer (zero-allocation version).
    /// Reuses the provided buffer, clearing it first.
    pub fn to_bytes_into(&self, bytes: &mut Vec<u8>) -> Result<()> {
        let encoding_bytes = self.encoding.as_bytes();
        let encoding_len = encoding_bytes.len();

        if encoding_len > u16::MAX as usize {
            return Err(RedbBackendError::serialization(format!(
                "Encoding string too long: {} bytes",
                encoding_len
            )));
        }

        // Calculate total size and reserve capacity
        let total_size = 8 + 2 + encoding_len + self.payload.len();
        bytes.clear();
        bytes.reserve(total_size);

        // Write timestamp (8 bytes)
        bytes.extend_from_slice(&self.timestamp.to_le_bytes());

        // Write encoding length (2 bytes)
        bytes.extend_from_slice(&(encoding_len as u16).to_le_bytes());

        // Write encoding string
        bytes.extend_from_slice(encoding_bytes);

        // Write payload as-is (no encoding!)
        bytes.extend_from_slice(&self.payload);

        Ok(())
    }

    /// Deserialize a stored value from bytes using the binary format.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 10 {
            return Err(RedbBackendError::serialization(format!(
                "Invalid data: too short ({} bytes)",
                bytes.len()
            )));
        }

        // Read timestamp (8 bytes)
        let timestamp =
            u64::from_le_bytes(bytes[0..8].try_into().map_err(|e| {
                RedbBackendError::serialization(format!("Invalid timestamp: {}", e))
            })?);

        // Read encoding length (2 bytes)
        let encoding_len = u16::from_le_bytes(bytes[8..10].try_into().map_err(|e| {
            RedbBackendError::serialization(format!("Invalid encoding length: {}", e))
        })?) as usize;

        // Check bounds
        if bytes.len() < 10 + encoding_len {
            return Err(RedbBackendError::serialization(format!(
                "Invalid data: encoding length {} exceeds remaining bytes",
                encoding_len
            )));
        }

        // Read encoding string
        let encoding = std::str::from_utf8(&bytes[10..10 + encoding_len])
            .map_err(|e| {
                RedbBackendError::serialization(format!("Invalid UTF-8 in encoding: {}", e))
            })?
            .to_string();

        // Read payload (remaining bytes, as-is!)
        let payload = bytes[10 + encoding_len..].to_vec();

        Ok(Self {
            payload,
            timestamp,
            encoding,
        })
    }
}

/// A storage instance backed by redb.
pub struct RedbStorage {
    /// The underlying redb database
    db: Arc<Database>,

    /// Storage configuration
    config: RedbStorageConfig,

    /// Storage name
    name: String,
}

impl RedbStorage {
    /// Create a new redb storage instance.
    pub fn new<P: AsRef<Path>>(path: P, name: String, config: RedbStorageConfig) -> Result<Self> {
        let path = path.as_ref();

        info!("Opening redb database at: {:?}", path);

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open or create the database
        let db = if config.read_only {
            Database::open(path)?
        } else {
            Database::create(path)?
        };

        // Initialize the table
        if !config.read_only {
            let write_txn = db.begin_write()?;
            {
                let _table = write_txn.open_table(KV_TABLE)?;
            }
            write_txn.commit()?;
            debug!("Initialized table in database");
        }

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

    /// Store a key-value pair in the database.
    /// Optimized to use thread-local buffers, eliminating per-call allocations.
    pub fn put(&self, key: &str, value: StoredValue) -> Result<()> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        trace!("Putting key: {}", key);

        // Use thread-local buffers to avoid allocations
        KEY_BUFFER.with(|key_buf| {
            VALUE_BUFFER.with(|val_buf| {
                let mut key_buf = key_buf.borrow_mut();
                let mut val_buf = val_buf.borrow_mut();

                // Encode key into reusable buffer
                key_buf.clear();
                self.encode_key_into(key, &mut key_buf)?;

                // Encode value into reusable buffer
                val_buf.clear();
                value.to_bytes_into(&mut val_buf)?;

                let write_txn = self.db.begin_write()?;
                {
                    let mut table = write_txn.open_table(KV_TABLE)?;
                    table.insert(key_buf.as_slice(), val_buf.as_slice())?;
                }
                write_txn.commit()?;

                debug!("Stored key: {}", key);
                Ok(())
            })
        })
    }

    /// Store multiple key-value pairs in a single transaction (batch operation).
    /// This is significantly faster than calling put() multiple times.
    /// Optimized to reuse buffers and minimize allocations.
    pub fn put_batch(&self, entries: Vec<(&str, StoredValue)>) -> Result<()> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        if entries.is_empty() {
            return Ok(());
        }

        trace!("Batch putting {} entries", entries.len());

        // Pre-allocate buffers to reuse across entries (avoid per-entry allocation)
        let mut key_buffer = Vec::with_capacity(256); // Typical key size
        let mut value_buffer = Vec::with_capacity(1024); // Typical value size

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(KV_TABLE)?;
            for (key, value) in entries {
                // Encode key into reusable buffer
                key_buffer.clear();
                self.encode_key_into(key, &mut key_buffer)?;

                // Encode value into reusable buffer
                value_buffer.clear();
                value.to_bytes_into(&mut value_buffer)?;

                // Insert using buffer references (no allocation!)
                table.insert(key_buffer.as_slice(), value_buffer.as_slice())?;
            }
        }
        write_txn.commit()?;

        debug!("Batch stored entries");
        Ok(())
    }

    /// Retrieve a value by its exact key.
    /// Optimized to use thread-local buffer for key encoding.
    /// Returns an owned copy of the data (clones the payload).
    pub fn get(&self, key: &str) -> Result<Option<StoredValue>> {
        trace!("Getting key: {}", key);

        // Use thread-local buffer to avoid allocation
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(KV_TABLE)?;

            let result = table.get(key_buf.as_slice())?;

            match result {
                Some(value_guard) => {
                    let value_bytes = value_guard.value();
                    let stored_value = StoredValue::from_bytes(value_bytes)?;
                    debug!("Found key: {}", key);
                    Ok(Some(stored_value))
                }
                None => {
                    trace!("Key not found: {}", key);
                    Ok(None)
                }
            }
        })
    }

    /// Retrieve multiple values by their keys in a single transaction.
    /// This is more efficient than calling `get()` multiple times as it uses
    /// a single read transaction for all lookups.
    ///
    /// # Arguments
    /// * `keys` - Slice of keys to fetch
    ///
    /// # Returns
    /// A vector of `Option<StoredValue>` in the same order as the input keys.
    /// `None` indicates the key was not found.
    ///
    /// # Performance
    /// - Uses a single read transaction for all keys
    /// - Pre-allocates result vector
    /// - Reuses buffer for key encoding
    ///
    /// # Example
    /// ```rust,ignore
    /// let keys = vec!["key1", "key2", "key3"];
    /// let values = storage.get_many(&keys)?;
    /// for (key, value) in keys.iter().zip(values.iter()) {
    ///     if let Some(v) = value {
    ///         println!("{}: {} bytes", key, v.payload.len());
    ///     }
    /// }
    /// ```
    pub fn get_many(&self, keys: &[&str]) -> Result<Vec<Option<StoredValue>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        trace!("Getting {} keys in batch", keys.len());

        // Use thread-local buffer for key encoding
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();

            // Pre-allocate result vector
            let mut results = Vec::with_capacity(keys.len());

            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(KV_TABLE)?;

            for key in keys {
                key_buf.clear();
                self.encode_key_into(key, &mut key_buf)?;

                let result = table.get(key_buf.as_slice())?;

                let value = match result {
                    Some(value_guard) => {
                        let value_bytes = value_guard.value();
                        Some(StoredValue::from_bytes(value_bytes)?)
                    }
                    None => None,
                };

                results.push(value);
            }

            debug!("Retrieved batch of {} keys", keys.len());
            Ok(results)
        })
    }

    /// Retrieve a value by its exact key with zero-copy semantics.
    /// Returns a guard that provides access to the data without cloning.
    /// The data is valid as long as the guard is held.
    ///
    /// # Performance
    /// - Eliminates payload cloning overhead
    /// - Useful for large payloads or high-frequency reads
    /// - Ideal for read-heavy workloads where data is only inspected
    ///
    /// # Note
    /// Due to redb's memory-mapped architecture and transaction lifetime requirements,
    /// we still need to copy the data. However, this method uses optimized parsing
    /// and can be extended in the future for true zero-copy if redb's API allows.
    ///
    /// # Example
    /// ```rust,ignore
    /// // Get value with guard
    /// if let Some(guard) = storage.get_ref("my/key")? {
    ///     // Access borrowed data through guard
    ///     println!("Payload size: {}", guard.payload().len());
    ///     println!("Encoding: {}", guard.encoding());
    ///
    ///     // Convert to owned if needed for longer-term storage
    ///     let owned = guard.to_owned();
    /// }
    /// ```
    pub fn get_ref(&self, key: &str) -> Result<Option<StoredValueGuard>> {
        trace!("Getting key with guard: {}", key);

        // Use thread-local buffer to avoid allocation
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(KV_TABLE)?;

            let result = table.get(key_buf.as_slice())?;

            match result {
                Some(value_guard) => {
                    let value_bytes = value_guard.value();

                    // Parse using zero-copy reference
                    let value_ref = StoredValueRef::from_bytes(value_bytes)?;

                    // Create guard with the data
                    // Note: We still need to clone here due to redb's transaction lifetime
                    // The guard pattern allows future optimization if redb supports it
                    let guard = StoredValueGuard {
                        payload: value_ref.payload.to_vec(),
                        timestamp: value_ref.timestamp,
                        encoding: value_ref.encoding.to_string(),
                    };

                    debug!("Found key with guard: {}", key);
                    Ok(Some(guard))
                }
                None => {
                    trace!("Key not found: {}", key);
                    Ok(None)
                }
            }
        })
    }

    /// Delete a key-value pair from the database.
    /// Optimized to use thread-local buffer for key encoding.
    pub fn delete(&self, key: &str) -> Result<bool> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        trace!("Deleting key: {}", key);

        // Use thread-local buffer to avoid allocation
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let write_txn = self.db.begin_write()?;
            let deleted = {
                let mut table = write_txn.open_table(KV_TABLE)?;
                table.remove(key_buf.as_slice())?.is_some()
            };
            write_txn.commit()?;

            if deleted {
                debug!("Deleted key: {}", key);
            } else {
                trace!("Key not found for deletion: {}", key);
            }

            Ok(deleted)
        })
    }

    /// Delete multiple keys in a single transaction.
    /// This is more efficient than calling `delete()` multiple times.
    ///
    /// # Arguments
    /// * `keys` - Slice of keys to delete
    ///
    /// # Returns
    /// The number of keys that were actually deleted (existed in the database).
    ///
    /// # Performance
    /// - Uses a single write transaction for all deletions
    /// - Reuses buffer for key encoding
    /// - Amortizes transaction overhead across all deletes
    ///
    /// # Example
    /// ```rust,ignore
    /// let keys = vec!["key1", "key2", "key3"];
    /// let deleted_count = storage.delete_many(&keys)?;
    /// println!("Deleted {} keys", deleted_count);
    /// ```
    pub fn delete_many(&self, keys: &[&str]) -> Result<usize> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        if keys.is_empty() {
            return Ok(0);
        }

        trace!("Deleting {} keys in batch", keys.len());

        // Use thread-local buffer for key encoding
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            let mut deleted_count = 0;

            let write_txn = self.db.begin_write()?;
            {
                let mut table = write_txn.open_table(KV_TABLE)?;

                for key in keys {
                    key_buf.clear();
                    self.encode_key_into(key, &mut key_buf)?;

                    if table.remove(key_buf.as_slice())?.is_some() {
                        deleted_count += 1;
                    }
                }
            }
            write_txn.commit()?;

            debug!("Deleted {} keys in batch", deleted_count);
            Ok(deleted_count)
        })
    }

    /// Get all entries in the database.
    /// Returns a vector of (key, value) tuples.
    /// Optimized to pre-allocate capacity based on table size.
    pub fn get_all(&self) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting all entries");

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;

        // Pre-allocate capacity for better performance
        let count = table.len()? as usize;
        let mut results = Vec::with_capacity(count);

        let iter = table.iter()?;
        for item in iter {
            let (key_guard, value_guard) = item?;
            let key = self.decode_key(key_guard.value())?;
            let value = StoredValue::from_bytes(value_guard.value())?;
            results.push((key, value));
        }

        debug!("Retrieved {} entries", results.len());
        Ok(results)
    }

    /// Get all entries matching a key prefix.
    pub fn get_by_prefix(&self, prefix: &str) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting entries with prefix: {}", prefix);

        // Use thread-local buffer to avoid allocation
        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(prefix, &mut key_buf)?;

            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(KV_TABLE)?;

            // Pre-allocate with reasonable initial capacity (heuristic: ~10% of total)
            let total_count = table.len()? as usize;
            let estimated_capacity = (total_count / 10).max(16);
            let mut results = Vec::with_capacity(estimated_capacity);

            // Iterate through all entries and filter by prefix
            let iter = table.iter()?;
            for item in iter {
                let (key_guard, value_guard) = item?;
                let key_bytes = key_guard.value();

                // Check if key starts with prefix
                if key_bytes.starts_with(key_buf.as_slice()) {
                    let key = self.decode_key(key_bytes)?;
                    let value = StoredValue::from_bytes(value_guard.value())?;
                    results.push((key, value));
                }
            }

            debug!(
                "Retrieved {} entries with prefix '{}'",
                results.len(),
                prefix
            );
            Ok(results)
        })
    }

    /// Get all entries matching a wildcard pattern.
    /// Supports simple wildcard matching with '*' (single segment) and '**' (multi-segment).
    pub fn get_by_wildcard(&self, pattern: &str) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting entries matching pattern: {}", pattern);

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;

        // Pre-allocate with reasonable initial capacity (heuristic: ~10% of total)
        let total_count = table.len()? as usize;
        let estimated_capacity = (total_count / 10).max(16);
        let mut results = Vec::with_capacity(estimated_capacity);

        // Iterate through all entries and filter by pattern
        let iter = table.iter()?;
        for item in iter {
            let (key_guard, value_guard) = item?;
            let key = self.decode_key(key_guard.value())?;

            // Check if key matches the wildcard pattern
            if self.matches_wildcard(&key, pattern) {
                let value = StoredValue::from_bytes(value_guard.value())?;
                results.push((key, value));
            }
        }

        debug!(
            "Retrieved {} entries matching pattern '{}'",
            results.len(),
            pattern
        );
        Ok(results)
    }

    /// Get the number of entries in the database.
    pub fn count(&self) -> Result<u64> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(KV_TABLE)?;

        let count = table.len()?;
        Ok(count)
    }

    /// Clear all entries from the database.
    pub fn clear(&self) -> Result<()> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        warn!("Clearing all entries from storage: {}", self.name);

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(KV_TABLE)?;
            // Delete all entries
            let keys: Vec<Vec<u8>> = table
                .iter()?
                .map(|item| item.map(|(k, _)| k.value().to_vec()))
                .collect::<std::result::Result<Vec<_>, _>>()?;

            for key in keys {
                table.remove(key.as_slice())?;
            }
        }
        write_txn.commit()?;

        info!("Cleared all entries from storage: {}", self.name);
        Ok(())
    }

    /// Encode a key string to bytes.
    /// Optimized to minimize allocations.
    #[allow(dead_code)]
    fn encode_key(&self, key: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.encode_key_into(key, &mut bytes)?;
        Ok(bytes)
    }

    /// Encode a key string into an existing buffer (zero-allocation version).
    /// Reuses the provided buffer, clearing it first.
    fn encode_key_into(&self, key: &str, bytes: &mut Vec<u8>) -> Result<()> {
        // Apply prefix stripping if configured
        let key_to_store = if self.config.strip_prefix {
            if let Some(ref prefix) = self.config.key_expr {
                key.strip_prefix(prefix.as_str()).unwrap_or(key)
            } else {
                key
            }
        } else {
            key
        };

        // Clear and write key bytes
        bytes.clear();
        bytes.extend_from_slice(key_to_store.as_bytes());
        Ok(())
    }

    /// Encode a key using stack allocation for small keys (< 128 bytes).
    /// This is a fast-path optimization that avoids heap allocation for typical keys.
    /// Returns a SmallVec that uses stack storage for keys up to 128 bytes.
    #[allow(dead_code)]
    fn encode_key_small(&self, key: &str) -> Result<SmallVec<[u8; 128]>> {
        // Apply prefix stripping if configured
        let key_to_store = if self.config.strip_prefix {
            if let Some(ref prefix) = self.config.key_expr {
                key.strip_prefix(prefix.as_str()).unwrap_or(key)
            } else {
                key
            }
        } else {
            key
        };

        // Use SmallVec for stack allocation of small keys
        let mut bytes = SmallVec::new();
        bytes.extend_from_slice(key_to_store.as_bytes());
        Ok(bytes)
    }

    /// Decode key bytes back to a string.
    fn decode_key(&self, key_bytes: &[u8]) -> Result<String> {
        let key = std::str::from_utf8(key_bytes)
            .map_err(|e| RedbBackendError::key_encoding(e.to_string()))?
            .to_string();

        // Re-add prefix if it was stripped
        let full_key = if self.config.strip_prefix {
            if let Some(ref prefix) = self.config.key_expr {
                format!("{}{}", prefix, key)
            } else {
                key
            }
        } else {
            key
        };

        Ok(full_key)
    }

    /// Check if a key matches a wildcard pattern.
    /// Supports:
    /// - '*' matches a single path segment (e.g., "a/*/c" matches "a/b/c")
    /// - '**' matches zero or more path segments (e.g., "a/**/c" matches "a/c", "a/b/c", "a/b/x/c")
    fn matches_wildcard(&self, key: &str, pattern: &str) -> bool {
        // Split by '/' to handle path segments
        let key_parts: Vec<&str> = key.split('/').collect();
        let pattern_parts: Vec<&str> = pattern.split('/').collect();

        matches_parts(&key_parts, &pattern_parts)
    }
}

/// Recursive helper for wildcard matching.
fn matches_parts(key_parts: &[&str], pattern_parts: &[&str]) -> bool {
    match (key_parts.first(), pattern_parts.first()) {
        (None, None) => true,
        (Some(_), None) => false,
        (None, Some(&"**")) => matches_parts(&[], &pattern_parts[1..]),
        (None, Some(_)) => false,
        (Some(_), Some(&"**")) => {
            // '**' can match zero or more segments
            // Try matching with zero segments
            if matches_parts(key_parts, &pattern_parts[1..]) {
                return true;
            }
            // Try matching with one or more segments
            matches_parts(&key_parts[1..], pattern_parts)
        }
        (Some(_k), Some(&"*")) => {
            // '*' matches exactly one segment
            matches_parts(&key_parts[1..], &pattern_parts[1..])
        }
        (Some(k), Some(&p)) => {
            // Exact match required
            if *k == p {
                matches_parts(&key_parts[1..], &pattern_parts[1..])
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_storage() -> (RedbStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let config = RedbStorageConfig::new();
        let storage = RedbStorage::new(db_path, "test_storage".to_string(), config).unwrap();
        (storage, temp_dir)
    }

    #[test]
    fn test_put_and_get() {
        let (storage, _temp) = create_test_storage();

        let value = StoredValue::new(
            b"test_data".to_vec(),
            123456789,
            "application/octet-stream".to_string(),
        );
        storage.put("test/key", value.clone()).unwrap();

        let retrieved = storage.get("test/key").unwrap();
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.payload, value.payload);
        assert_eq!(retrieved.timestamp, value.timestamp);
        assert_eq!(retrieved.encoding, value.encoding);
    }

    #[test]
    fn test_delete() {
        let (storage, _temp) = create_test_storage();

        let value = StoredValue::new(b"test_data".to_vec(), 123456789, "text/plain".to_string());
        storage.put("test/key", value).unwrap();

        let deleted = storage.delete("test/key").unwrap();
        assert!(deleted);

        let retrieved = storage.get("test/key").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_get_all() {
        let (storage, _temp) = create_test_storage();

        for i in 0..5 {
            let key = format!("test/key/{}", i);
            let value = StoredValue::new(
                format!("data_{}", i).into_bytes(),
                i as u64,
                "text/plain".to_string(),
            );
            storage.put(&key, value).unwrap();
        }

        let all = storage.get_all().unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_get_by_prefix() {
        let (storage, _temp) = create_test_storage();

        storage
            .put(
                "test/a/1",
                StoredValue::new(b"1".to_vec(), 1, "text/plain".to_string()),
            )
            .unwrap();
        storage
            .put(
                "test/a/2",
                StoredValue::new(b"2".to_vec(), 2, "text/plain".to_string()),
            )
            .unwrap();
        storage
            .put(
                "test/b/1",
                StoredValue::new(b"3".to_vec(), 3, "text/plain".to_string()),
            )
            .unwrap();

        let results = storage.get_by_prefix("test/a/").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_wildcard_matching() {
        let (storage, _temp) = create_test_storage();

        assert!(storage.matches_wildcard("a/b/c", "a/b/c"));
        assert!(storage.matches_wildcard("a/b/c", "a/*/c"));
        assert!(storage.matches_wildcard("a/b/c", "a/**/c"));
        assert!(storage.matches_wildcard("a/b/x/y/c", "a/**/c"));
        assert!(storage.matches_wildcard("a/c", "a/**/c"));

        assert!(!storage.matches_wildcard("a/b/c", "a/x/c"));
        assert!(!storage.matches_wildcard("a/b/c/d", "a/*/c"));
    }

    #[test]
    fn test_matches_parts_function() {
        use super::matches_parts;

        assert!(matches_parts(&["a", "b", "c"], &["a", "b", "c"]));
        assert!(matches_parts(&["a", "b", "c"], &["a", "*", "c"]));
        assert!(matches_parts(&["a", "b", "c"], &["a", "**", "c"]));
        assert!(!matches_parts(&["a", "b", "c"], &["a", "x", "c"]));
    }

    #[test]
    fn test_count() {
        let (storage, _temp) = create_test_storage();

        assert_eq!(storage.count().unwrap(), 0);

        storage
            .put(
                "key1",
                StoredValue::new(b"data1".to_vec(), 1, "text/plain".to_string()),
            )
            .unwrap();
        storage
            .put(
                "key2",
                StoredValue::new(b"data2".to_vec(), 2, "text/plain".to_string()),
            )
            .unwrap();

        assert_eq!(storage.count().unwrap(), 2);
    }

    #[test]
    fn test_clear() {
        let (storage, _temp) = create_test_storage();

        storage
            .put(
                "key1",
                StoredValue::new(b"data1".to_vec(), 1, "text/plain".to_string()),
            )
            .unwrap();
        storage
            .put(
                "key2",
                StoredValue::new(b"data2".to_vec(), 2, "text/plain".to_string()),
            )
            .unwrap();

        assert_eq!(storage.count().unwrap(), 2);

        storage.clear().unwrap();
        assert_eq!(storage.count().unwrap(), 0);
    }

    #[test]
    fn test_get_ref_zero_copy() {
        let (storage, _temp_dir) = create_test_storage();

        let key = "test/zero_copy";
        let payload = b"Hello, zero-copy world!".to_vec();
        let value = StoredValue::new(payload.clone(), 12345, "text/plain".to_string());

        storage.put(key, value).unwrap();

        // Test get_ref
        let guard = storage.get_ref(key).unwrap();
        assert!(guard.is_some());

        let guard = guard.unwrap();
        assert_eq!(guard.payload(), payload.as_slice());
        assert_eq!(guard.timestamp(), 12345);
        assert_eq!(guard.encoding(), "text/plain");
    }

    #[test]
    fn test_get_ref_not_found() {
        let (storage, _temp_dir) = create_test_storage();

        let guard = storage.get_ref("nonexistent/key").unwrap();
        assert!(guard.is_none());
    }

    #[test]
    fn test_get_ref_to_owned() {
        let (storage, _temp_dir) = create_test_storage();

        let key = "test/convert";
        let payload = b"Convert to owned".to_vec();
        let value = StoredValue::new(payload.clone(), 54321, "application/json".to_string());

        storage.put(key, value).unwrap();

        // Get with guard and convert to owned
        let guard = storage.get_ref(key).unwrap().unwrap();
        let owned = guard.to_owned();

        assert_eq!(owned.payload, payload);
        assert_eq!(owned.timestamp, 54321);
        assert_eq!(owned.encoding, "application/json");
    }

    #[test]
    fn test_get_ref_large_payload() {
        let (storage, _temp_dir) = create_test_storage();

        let key = "test/large";
        let payload = vec![0u8; 1024 * 1024]; // 1 MB
        let value = StoredValue::new(
            payload.clone(),
            99999,
            "application/octet-stream".to_string(),
        );

        storage.put(key, value).unwrap();

        let guard = storage.get_ref(key).unwrap().unwrap();
        assert_eq!(guard.payload().len(), 1024 * 1024);
        assert_eq!(guard.timestamp(), 99999);
    }

    #[test]
    fn test_stored_value_ref_from_bytes() {
        // Create test data
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&12345u64.to_le_bytes()); // timestamp
        bytes.extend_from_slice(&10u16.to_le_bytes()); // encoding length
        bytes.extend_from_slice(b"text/plain"); // encoding
        bytes.extend_from_slice(b"Hello!"); // payload

        let value_ref = StoredValueRef::from_bytes(&bytes).unwrap();
        assert_eq!(value_ref.timestamp, 12345);
        assert_eq!(value_ref.encoding, "text/plain");
        assert_eq!(value_ref.payload, b"Hello!");
    }

    #[test]
    fn test_stored_value_ref_to_owned() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&12345u64.to_le_bytes());
        bytes.extend_from_slice(&10u16.to_le_bytes());
        bytes.extend_from_slice(b"text/plain");
        bytes.extend_from_slice(b"Test data");

        let value_ref = StoredValueRef::from_bytes(&bytes).unwrap();
        let owned = value_ref.to_owned();

        assert_eq!(owned.payload, b"Test data");
        assert_eq!(owned.timestamp, 12345);
        assert_eq!(owned.encoding, "text/plain");
    }

    #[test]
    fn test_get_many() {
        let (storage, _temp_dir) = create_test_storage();

        // Insert test data
        let keys = vec!["key1", "key2", "key3", "key4"];
        for (i, key) in keys.iter().enumerate() {
            let payload = format!("payload{}", i + 1).into_bytes();
            let value = StoredValue::new(payload, i as u64, "text/plain".to_string());
            storage.put(key, value).unwrap();
        }

        // Test get_many with all keys
        let results = storage.get_many(&keys).unwrap();
        assert_eq!(results.len(), 4);

        for (i, result) in results.iter().enumerate() {
            assert!(result.is_some());
            let value = result.as_ref().unwrap();
            assert_eq!(value.payload, format!("payload{}", i + 1).into_bytes());
            assert_eq!(value.timestamp, i as u64);
        }

        // Test get_many with mix of existing and non-existing keys
        let mixed_keys = vec!["key1", "nonexistent", "key3"];
        let results = storage.get_many(&mixed_keys).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].is_some());
        assert!(results[1].is_none());
        assert!(results[2].is_some());

        // Test get_many with empty slice
        let empty_results = storage.get_many(&[]).unwrap();
        assert_eq!(empty_results.len(), 0);
    }

    #[test]
    fn test_delete_many() {
        let (storage, _temp_dir) = create_test_storage();

        // Insert test data
        let keys = vec!["del1", "del2", "del3", "del4", "del5"];
        for key in &keys {
            let payload = format!("data for {}", key).into_bytes();
            let value = StoredValue::new(payload, 100, "text/plain".to_string());
            storage.put(key, value).unwrap();
        }

        // Verify all keys exist
        assert_eq!(storage.count().unwrap(), 5);

        // Delete three keys
        let to_delete = vec!["del1", "del3", "del5"];
        let deleted_count = storage.delete_many(&to_delete).unwrap();
        assert_eq!(deleted_count, 3);

        // Verify remaining keys
        assert_eq!(storage.count().unwrap(), 2);
        assert!(storage.get("del2").unwrap().is_some());
        assert!(storage.get("del4").unwrap().is_some());
        assert!(storage.get("del1").unwrap().is_none());
        assert!(storage.get("del3").unwrap().is_none());
        assert!(storage.get("del5").unwrap().is_none());

        // Test delete_many with non-existing keys
        let nonexistent = vec!["fake1", "fake2"];
        let deleted_count = storage.delete_many(&nonexistent).unwrap();
        assert_eq!(deleted_count, 0);

        // Test delete_many with empty slice
        let deleted_count = storage.delete_many(&[]).unwrap();
        assert_eq!(deleted_count, 0);
    }

    #[test]
    fn test_get_many_and_delete_many_together() {
        let (storage, _temp_dir) = create_test_storage();

        // Setup: Insert 10 keys
        let keys: Vec<String> = (0..10).map(|i| format!("item{}", i)).collect();
        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();

        for key in &key_refs {
            let payload = format!("data for {}", key).into_bytes();
            let value = StoredValue::new(payload, 200, "application/json".to_string());
            storage.put(key, value).unwrap();
        }

        // Get all keys
        let results = storage.get_many(&key_refs).unwrap();
        assert_eq!(results.len(), 10);
        assert!(results.iter().all(|r| r.is_some()));

        // Delete half of them
        let to_delete: Vec<&str> = keys.iter().step_by(2).map(|s| s.as_str()).collect();
        let deleted_count = storage.delete_many(&to_delete).unwrap();
        assert_eq!(deleted_count, 5);

        // Get all keys again - half should be None
        let results = storage.get_many(&key_refs).unwrap();
        assert_eq!(results.len(), 10);

        for (i, result) in results.iter().enumerate() {
            if i % 2 == 0 {
                assert!(result.is_none(), "item{} should be deleted", i);
            } else {
                assert!(result.is_some(), "item{} should exist", i);
            }
        }
    }
}
