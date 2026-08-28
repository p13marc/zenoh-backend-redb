//! Storage implementation for the zenoh-backend-redb storage backend.
//!
//! This implementation separates payload and metadata (data_info) into different tables,
//! similar to the RocksDB backend design using column families.

use crate::config::RedbStorageConfig;
use crate::error::{RedbBackendError, Result};
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, trace, warn};
use zenoh::bytes::{Encoding, ZBytes};
use zenoh::internal::buffers::ZSlice;
use zenoh::key_expr::keyexpr;
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

/// A point-in-time report of what a storage costs.
///
/// Returned by [`RedbStorage::stats`] and rendered onto the Zenoh admin space, where
/// `zenctl storage list` and the GUI's storage panel can read it. An operator could
/// previously see that a storage *existed* but not what it was consuming — and three
/// of the four things that went wrong on the target fleet this year were "something
/// grew and nobody was watching the number".
#[derive(Debug, Clone)]
pub struct StorageStats {
    /// Size of the database file on disk, from the filesystem. `None` if the path is
    /// not known or cannot be stat'ed.
    pub on_disk_bytes: Option<u64>,
    /// Bytes of keys and values actually inserted, excluding indexing overhead.
    pub stored_bytes: u64,
    /// Bytes of btree branch keys and other redb metadata.
    pub metadata_bytes: u64,
    /// Bytes lost to fragmentation. The gap between this plus the two above and
    /// `on_disk_bytes` is what a compaction could reclaim.
    pub fragmented_bytes: u64,
    /// Rows in the metadata table, tombstones included.
    pub key_count: u64,
    /// Keys with a live value.
    pub live_keys: u64,
    /// Keys holding a deletion tombstone.
    pub tombstones: u64,
    /// Oldest timestamp held, tombstones included.
    pub oldest_timestamp: Option<Timestamp>,
    /// Newest timestamp held, tombstones included.
    pub newest_timestamp: Option<Timestamp>,
    /// Configured page-cache budget.
    pub cache_size_bytes: usize,
    /// Bytes currently held in the page cache.
    pub cache_used_bytes: usize,
    /// Cache reads served from memory.
    pub cache_read_hits: u64,
    /// Cache reads that had to go to storage.
    pub cache_read_misses: u64,
    /// Evictions caused by the cache being full. A climbing count against a flat
    /// hit ratio is the signal that `cache_size` is too small for the working set.
    pub cache_evictions: u64,
}

impl StorageStats {
    /// Read-cache hit ratio in `[0, 1]`, or `None` before any read has happened.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let total = self.cache_read_hits + self.cache_read_misses;
        (total > 0).then(|| self.cache_read_hits as f64 / total as f64)
    }
}

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
    /// Open (or create) the redb database with the cache budget and create/open
    /// semantics the storage config asks for.
    ///
    /// The cache size is always set explicitly. redb's own default is 1 GiB, which
    /// on a small guest reads as a slow multi-day RSS climb ending at the OOM killer
    /// rather than as a configuration mistake — so this backend never inherits it.
    ///
    /// `read_only` is still enforced in this crate rather than by redb:
    /// `Builder::open_read_only` returns a distinct `ReadOnlyDatabase` type, which
    /// would split every transaction call site below for no behavioural gain.
    fn open_database(path: &Path, config: &RedbStorageConfig) -> Result<Database> {
        let mut builder = Database::builder();
        builder.set_cache_size(config.cache_size);

        // `read_only` and `create_db: false` both mean "this file must already exist".
        let result = if config.read_only || !config.create_db {
            builder.open(path)
        } else {
            builder.create(path)
        };

        result.map_err(|e| {
            // redb 3 dropped support for the v2 file format this crate wrote before
            // 0.4. The error you get is about a bad magic number, which reads as
            // corruption rather than as a version skew, so say what it really is.
            if matches!(e, redb::DatabaseError::UpgradeRequired(_)) {
                RedbBackendError::other(format!(
                    "{path:?} was written by an older redb file format that redb 4 \
                     cannot open. Either delete it, or open it once with redb 2.6 and \
                     call `Database::upgrade()` before using this version. ({e})"
                ))
            } else {
                RedbBackendError::from(e)
            }
        })
    }

    /// Apply the configured durability to a write transaction.
    ///
    /// `fsync: true` (the default) is `Durability::Immediate`: a commit that returns
    /// has reached the disk. `fsync: false` is `Durability::None` — redb 4 removed
    /// the intermediate `Eventual` level, so the trade is sharper than the config
    /// name suggests: commits are not persisted at all until some later durable
    /// commit lands. Fine for a cache or a replayable stream, wrong for a system of
    /// record.
    fn apply_durability(
        txn: &mut redb::WriteTransaction,
        config: &RedbStorageConfig,
    ) -> Result<()> {
        txn.set_durability(if config.fsync {
            Durability::Immediate
        } else {
            Durability::None
        })?;
        Ok(())
    }

    /// Begin a write transaction with the configured durability applied.
    fn begin_write(&self) -> Result<redb::WriteTransaction> {
        let mut txn = self.db.begin_write()?;
        Self::apply_durability(&mut txn, &self.config)?;
        Ok(txn)
    }

    /// Create a new RedbStorage instance.
    pub fn new<P: AsRef<Path>>(path: P, config: RedbStorageConfig, name: String) -> Result<Self> {
        info!("Creating redb storage at: {:?}", path.as_ref());

        let db = Self::open_database(path.as_ref(), &config)?;

        // Initialize both tables
        let mut write_txn = db.begin_write()?;
        Self::apply_durability(&mut write_txn, &config)?;
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

                let write_txn = self.begin_write()?;
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

            let write_txn = self.begin_write()?;
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

    /// Read only the stored timestamp for a key, without loading its payload.
    ///
    /// Unlike [`RedbStorage::get`] this reports the timestamp of a tombstone too: a
    /// DELETE at t=5 must still win against a PUT at t=3 that arrives afterwards, so
    /// the caller comparing timestamps needs to see it.
    pub fn timestamp_of(&self, key: &str) -> Result<Option<Timestamp>> {
        let read_txn = self.db.begin_read()?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        match data_info_table.get(key.as_bytes())? {
            Some(info_guard) => {
                let (_, timestamp, _) = decode_data_info(info_guard.value())?;
                Ok(Some(timestamp))
            }
            None => Ok(None),
        }
    }

    /// What this storage costs, for the admin space.
    ///
    /// Sizes come from redb and the filesystem, never from adding up key and value
    /// lengths: the gap between "bytes I stored" and "bytes on disk" is exactly the
    /// fragmentation and metadata overhead an operator needs to see, and an
    /// estimate would hide it. `on_disk_bytes` is the real file.
    pub fn stats(&self) -> Result<StorageStats> {
        let read_txn = self.db.begin_read()?;
        let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let payload_stats = payloads_table.stats()?;
        let info_stats = data_info_table.stats()?;
        let cache = self.db.cache_stats();

        // Scan the metadata table for the timestamp span and the live/tombstone
        // split. This is the one number that cannot come from redb.
        let mut live_keys = 0u64;
        let mut tombstones = 0u64;
        let mut oldest: Option<Timestamp> = None;
        let mut newest: Option<Timestamp> = None;
        for item in data_info_table.iter()? {
            let (_, info_bytes) = item?;
            let (_, timestamp, deleted) = decode_data_info(info_bytes.value())?;
            if deleted {
                tombstones += 1;
            } else {
                live_keys += 1;
            }
            if oldest.is_none_or(|o| timestamp < o) {
                oldest = Some(timestamp);
            }
            if newest.is_none_or(|n| timestamp > n) {
                newest = Some(timestamp);
            }
        }

        let on_disk_bytes = self
            .config
            .db_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());

        Ok(StorageStats {
            on_disk_bytes,
            stored_bytes: payload_stats.stored_bytes() + info_stats.stored_bytes(),
            metadata_bytes: payload_stats.metadata_bytes() + info_stats.metadata_bytes(),
            fragmented_bytes: payload_stats.fragmented_bytes() + info_stats.fragmented_bytes(),
            key_count: data_info_table.len()?,
            live_keys,
            tombstones,
            oldest_timestamp: oldest,
            newest_timestamp: newest,
            cache_size_bytes: self.config.cache_size,
            cache_used_bytes: cache.used_bytes(),
            cache_read_hits: cache.read_hits(),
            cache_read_misses: cache.read_misses(),
            cache_evictions: cache.evictions(),
        })
    }

    /// Every live key and its timestamp, without reading a single payload.
    ///
    /// This is what the storage manager calls to resolve a wildcard query: it asks
    /// for every entry, intersects the selector itself, and only then issues
    /// per-key GETs. Answering that from [`RedbStorage::get_all`] meant loading the
    /// entire database into memory and discarding all of it, on every wildcard
    /// query. Here only the metadata table is touched.
    pub fn get_all_timestamps(&self) -> Result<Vec<(String, Timestamp)>> {
        let read_txn = self.db.begin_read()?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut results = Vec::new();
        for item in data_info_table.iter()? {
            let (key_bytes, info_bytes) = item?;
            let (_, timestamp, deleted) = decode_data_info(info_bytes.value())?;
            if !deleted {
                results.push((self.decode_key(key_bytes.value())?, timestamp));
            }
        }

        Ok(results)
    }

    /// Retrieve all key-value pairs matching a given prefix.
    pub fn get_by_prefix(&self, prefix: &str) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting entries by prefix: {}", prefix);

        let read_txn = self.db.begin_read()?;
        let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut results = Vec::new();

        // redb tables are ordered, so start at the prefix and stop as soon as we
        // leave it rather than reading every row in the table.
        for item in data_info_table.range(prefix.as_bytes()..)? {
            let (key_bytes, info_bytes) = item?;
            if !key_bytes.value().starts_with(prefix.as_bytes()) {
                break;
            }
            let key = self.decode_key(key_bytes.value())?;

            let (encoding, timestamp, deleted) = decode_data_info(info_bytes.value())?;

            if !deleted && let Some(payload_guard) = payloads_table.get(key_bytes.value())? {
                let payload_bytes = payload_guard.value();
                let stored_value = StoredValue::new(payload_bytes.to_vec(), timestamp, encoding);
                results.push((key, stored_value));
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

        // Only the rows under the selector's literal prefix can possibly match, and
        // redb keeps the table ordered, so the scan starts there and stops on the
        // first key that leaves it. `keyexpr::intersects` then runs over candidates
        // rather than over the whole table.
        let prefix = literal_prefix(pattern);

        // One key can match without living under the prefix: `a/**` intersects `a`
        // itself, but its literal prefix is `a/` and `a` sorts *before* `a/`, so the
        // range below starts past it. Probe it directly rather than widening the
        // scan, which would drag in every unrelated key sharing those bytes.
        if let Some(exact) = prefix.strip_suffix('/')
            && !exact.is_empty()
            && Self::matches_wildcard(exact, pattern)
            && let Some(info_guard) = data_info_table.get(exact.as_bytes())?
        {
            let (encoding, timestamp, deleted) = decode_data_info(info_guard.value())?;
            if !deleted && let Some(payload_guard) = payloads_table.get(exact.as_bytes())? {
                results.push((
                    exact.to_string(),
                    StoredValue::new(payload_guard.value().to_vec(), timestamp, encoding),
                ));
            }
        }

        for item in data_info_table.range(prefix.as_bytes()..)? {
            let (key_bytes, info_bytes) = item?;
            if !key_bytes.value().starts_with(prefix.as_bytes()) {
                break;
            }
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

        let write_txn = self.begin_write()?;
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

    /// Does `key` match the key expression `pattern`?
    ///
    /// This defers to Zenoh's own key-expression algebra rather than splitting on
    /// `/` ourselves. A storage that answers with different semantics than the
    /// router that routed the query to it is a divergence that surfaces later as
    /// "why did this GET answer differently through the storage".
    ///
    /// Two rules a hand-rolled `*`/`**` matcher does not have, and both matter:
    ///
    /// 1. **Verbatim chunks.** `*` and `**` never match a chunk beginning with `@`.
    ///    That is the entire basis of the verbatim planes (`@rpc`, `@media`,
    ///    `@blob`, `@catalog`): `v1/*/state/**` cannot reach `v1/@catalog/state/**`,
    ///    which is why a catalog needs a storage of its own.
    /// 2. **`$*`**, the sub-chunk wildcard. A selector using it previously matched
    ///    nothing at all, silently.
    ///
    /// A key or pattern that is not a valid key expression matches nothing.
    fn matches_wildcard(key: &str, pattern: &str) -> bool {
        match (keyexpr::new(key), keyexpr::new(pattern)) {
            (Ok(key), Ok(pattern)) => pattern.intersects(key),
            _ => false,
        }
    }
}

/// The longest wildcard-free prefix of a key expression: everything up to the
/// first chunk containing `*` or `$`.
///
/// This is what bounds a wildcard scan. `v1/h-3fa9c2d41b7e/telemetry/**` yields
/// `v1/h-3fa9c2d41b7e/telemetry/` — a per-host drill-in reads a slice of the table
/// instead of all of it. `v1/*/state/**` yields `v1/`, which buys nothing, and that is
/// fine: the point is that the common shapes are bounded, not that every shape is.
///
/// The prefix always ends at a chunk boundary, so it can never match a partial
/// chunk: for `v1/h-3fa*/x` the prefix is `v1/`, not `v1/h-3fa`.
fn literal_prefix(pattern: &str) -> &str {
    let mut end = 0;
    for chunk in pattern.split('/') {
        if chunk.contains('*') || chunk.contains('$') {
            break;
        }
        // +1 for the '/' that follows this chunk.
        end += chunk.len() + 1;
    }
    // A pattern with no wildcard at all is entirely literal; `end` then overshoots
    // by the trailing separator that is not there.
    &pattern[..end.min(pattern.len())]
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

    /// `*` and `**` must not reach a chunk beginning with `@`.
    ///
    /// This is the rule the hand-rolled matcher did not have, and it is not a
    /// detail: the verbatim planes exist because of it. A fleet-wide state
    /// selector must not be able to see the catalog, which is precisely why the
    /// catalog is configured as a storage of its own.
    #[test]
    fn star_does_not_match_a_verbatim_chunk() {
        assert!(!RedbStorage::matches_wildcard(
            "v1/@catalog/state/entity/h-aaaabbbbcccc",
            "v1/*/state/**"
        ));
        assert!(!RedbStorage::matches_wildcard(
            "v1/@catalog/state/entity/h-aaaabbbbcccc",
            "v1/**"
        ));

        // ...but a selector that names the verbatim chunk reaches it.
        assert!(RedbStorage::matches_wildcard(
            "v1/@catalog/state/entity/h-aaaabbbbcccc",
            "v1/@catalog/state/**"
        ));

        // The same rule for the other planes.
        assert!(!RedbStorage::matches_wildcard(
            "v1/h-aaaabbbbcccc/@rpc/sysinfo/processes",
            "v1/*/*/sysinfo/**"
        ));
    }

    #[test]
    fn literal_prefix_stops_at_the_first_wildcard_chunk() {
        // The shapes that matter: a per-host drill-in and a catalog read are
        // bounded; a fleet-wide selector is not, and that is expected.
        assert_eq!(
            literal_prefix("v1/h-3fa9c2d41b7e/telemetry/**"),
            "v1/h-3fa9c2d41b7e/telemetry/"
        );
        assert_eq!(
            literal_prefix("v1/@catalog/state/entity/*"),
            "v1/@catalog/state/entity/"
        );
        assert_eq!(literal_prefix("v1/*/state/**"), "v1/");
        assert_eq!(literal_prefix("**"), "");

        // A fully literal pattern is its own prefix, with no trailing separator
        // invented for it.
        assert_eq!(literal_prefix("a/b/c"), "a/b/c");

        // The prefix must never cut a chunk in half: `h-3fa` is not a key boundary,
        // so a scan starting there could skip rows that do match.
        assert_eq!(literal_prefix("v1/h-3fa*/x"), "v1/");
        assert_eq!(literal_prefix("v1/sensor$*/x"), "v1/");
    }

    /// The bounded scan must return exactly what the full scan returned.
    #[test]
    fn range_scan_finds_the_same_keys_a_full_scan_would() {
        let (storage, _temp) = create_test_storage();
        let ts = Timestamp::new(NTP64(1), TimestampId::rand());

        for key in [
            "v1/h-aaaa/telemetry/cpu",
            "v1/h-aaaa/telemetry/mem",
            "v1/h-aaaa/state/health",
            "v1/h-bbbb/telemetry/cpu",
            "v1/@catalog/state/entity/h-aaaa",
        ] {
            let value = StoredValue::new(b"x".to_vec(), ts, Encoding::ZENOH_BYTES);
            storage.put(key, value).unwrap();
        }

        // Bounded by a literal prefix.
        let mut hit: Vec<String> = storage
            .get_by_wildcard("v1/h-aaaa/telemetry/**")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        hit.sort();
        assert_eq!(
            hit,
            vec!["v1/h-aaaa/telemetry/cpu", "v1/h-aaaa/telemetry/mem"]
        );

        // Unbounded prefix (`v1/`) still has to give the right answer — and still
        // must not reach the verbatim @catalog chunk.
        let mut hit: Vec<String> = storage
            .get_by_wildcard("v1/*/state/**")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        hit.sort();
        assert_eq!(hit, vec!["v1/h-aaaa/state/health"]);
    }

    /// `a/**` intersects `a` itself, and the bounded scan must not lose it.
    ///
    /// This is the case a range scan gets wrong for free: the literal prefix of
    /// `a/**` is `a/`, and `a` sorts *before* `a/`, so a scan that simply starts at
    /// the prefix skips a key that genuinely matches. Zenoh's own algebra says it
    /// matches (`ab/**` intersects `ab`), and the full scan this replaced returned
    /// it.
    #[test]
    fn a_wildcard_still_finds_the_key_equal_to_its_literal_prefix() {
        let (storage, _temp) = create_test_storage();
        let ts = Timestamp::new(NTP64(1), TimestampId::rand());

        for key in ["sensors", "sensors/room1", "sensorsX", "other"] {
            let value = StoredValue::new(b"x".to_vec(), ts, Encoding::ZENOH_BYTES);
            storage.put(key, value).unwrap();
        }

        assert!(
            RedbStorage::matches_wildcard("sensors", "sensors/**"),
            "precondition: zenoh's algebra says `sensors/**` matches `sensors`"
        );

        let mut hit: Vec<String> = storage
            .get_by_wildcard("sensors/**")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        hit.sort();
        assert_eq!(hit, vec!["sensors", "sensors/room1"]);

        // And the probe must not invent a match: `sensorsX` shares the bytes but is
        // a different chunk.
        assert!(!hit.contains(&"sensorsX".to_string()));
    }

    /// A range scan must stop at the prefix, not run past it into the next key.
    #[test]
    fn prefix_scan_does_not_bleed_into_neighbouring_keys() {
        let (storage, _temp) = create_test_storage();
        let ts = Timestamp::new(NTP64(1), TimestampId::rand());

        // `a/bc` sorts immediately after `a/b/...` and must not be picked up by a
        // scan for `a/b/`.
        for key in ["a/b/one", "a/b/two", "a/bc/three", "a/c/four"] {
            let value = StoredValue::new(b"x".to_vec(), ts, Encoding::ZENOH_BYTES);
            storage.put(key, value).unwrap();
        }

        let mut hit: Vec<String> = storage
            .get_by_prefix("a/b/")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        hit.sort();
        assert_eq!(hit, vec!["a/b/one", "a/b/two"]);
    }

    /// `$*` is a sub-chunk wildcard. The old matcher treated it as a literal, so
    /// a selector using it returned nothing at all and said nothing about it.
    #[test]
    fn sub_chunk_wildcard_is_supported() {
        assert!(RedbStorage::matches_wildcard("a/sensor1/c", "a/sensor$*/c"));
        assert!(RedbStorage::matches_wildcard("a/sensor/c", "a/sensor$*/c"));
        assert!(!RedbStorage::matches_wildcard("a/other/c", "a/sensor$*/c"));
    }

    /// The placeholder used when `strip_prefix` consumes a key entirely is a
    /// verbatim chunk, so `**` cannot reach it. That is the correct outcome —
    /// it is an internal sentinel, not part of anyone's keyspace — but it is
    /// worth pinning so the behaviour is not rediscovered as a bug.
    #[test]
    fn the_none_key_sentinel_is_verbatim() {
        assert!(!RedbStorage::matches_wildcard(
            crate::plugin::NONE_KEY,
            "**"
        ));
        assert!(RedbStorage::matches_wildcard(
            crate::plugin::NONE_KEY,
            crate::plugin::NONE_KEY
        ));
    }

    /// Anything that is not a valid key expression matches nothing, rather than
    /// panicking or matching by accident.
    #[test]
    fn invalid_key_expressions_match_nothing() {
        assert!(!RedbStorage::matches_wildcard("a//b", "**"));
        assert!(!RedbStorage::matches_wildcard("a/b", "a/["));
        assert!(!RedbStorage::matches_wildcard("", "**"));
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
