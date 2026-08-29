//! Storage implementation for the zenoh-backend-redb storage backend.
//!
//! This implementation separates payload and metadata (data_info) into different tables,
//! similar to the RocksDB backend design using column families.

use crate::config::{HistoryMode, RedbStorageConfig, RetentionPolicy};
use crate::error::{RedbBackendError, Result};
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use std::cell::RefCell;
use std::path::Path;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};
use tracing::{debug, info, trace, warn};
use zenoh::bytes::{Encoding, ZBytes};
use zenoh::internal::buffers::ZSlice;
use zenoh::key_expr::keyexpr;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use zenoh_ext::{z_deserialize, z_serialize};
use zenoh_util::time_range::TimeRange;

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

/// What a write did, so the plugin layer can report the right
/// `StorageInsertionResult` without re-reading the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The key held nothing before.
    Inserted,
    /// An existing value was superseded.
    Replaced,
    /// The sample predates what is stored and was not applied. Only possible in
    /// [`HistoryMode::Latest`]; an `all`-mode storage keeps every sample.
    Outdated,
}

/// Table of historical payloads, keyed by `(key, timestamp)`.
/// Only written in [`HistoryMode::All`].
const HISTORY_PAYLOADS_TABLE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("history_payloads");

/// Table of historical metadata, keyed by `(key, timestamp)`.
/// Only written in [`HistoryMode::All`].
const HISTORY_INFO_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("history_info");

/// Separator between a key and its timestamp in the history tables.
///
/// A Zenoh key expression is UTF-8 and can never contain a NUL byte, and NUL sorts
/// below every byte a key *can* hold. Those two facts give the layout the
/// properties it needs: one key's samples are contiguous, and `a/b`'s samples all
/// sort before `a/b/c`'s instead of interleaving with them. Concatenating the
/// timestamp onto an unframed key would have neither.
const KEY_TIME_SEPARATOR: u8 = 0;

/// Bytes a timestamp occupies in a composite key: 8 for the NTP64, 16 for the id.
const TIMESTAMP_LEN: usize = 8 + 16;

/// `key || 0x00 || ntp64_be || timestamp_id`.
///
/// The NTP64 is big-endian so that redb's lexicographic ordering *is* chronological
/// ordering, which is what makes a time window one range scan. The id follows as a
/// tiebreaker so two sources writing at the same instant do not collide.
fn encode_history_key(key: &str, timestamp: &Timestamp) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 1 + TIMESTAMP_LEN);
    out.extend_from_slice(key.as_bytes());
    out.push(KEY_TIME_SEPARATOR);
    out.extend_from_slice(&timestamp.get_time().as_u64().to_be_bytes());
    out.extend_from_slice(&timestamp.get_id().to_le_bytes());
    out
}

/// The half-open byte range covering every sample of `key`.
///
/// The upper bound is `key || 0x01`: every composite key for `key` starts
/// `key || 0x00`, and nothing else in the table can fall between the two.
fn history_key_bounds(key: &str) -> (Vec<u8>, Vec<u8>) {
    let mut start = Vec::with_capacity(key.len() + 1);
    start.extend_from_slice(key.as_bytes());
    start.push(KEY_TIME_SEPARATOR);

    let mut end = Vec::with_capacity(key.len() + 1);
    end.extend_from_slice(key.as_bytes());
    end.push(KEY_TIME_SEPARATOR + 1);

    (start, end)
}

/// Inverse of [`encode_history_key`].
fn decode_history_key(bytes: &[u8]) -> Result<(String, Timestamp)> {
    if bytes.len() < TIMESTAMP_LEN + 1 {
        return Err(RedbBackendError::key_encoding(format!(
            "history key too short: {} bytes",
            bytes.len()
        )));
    }
    let split = bytes.len() - TIMESTAMP_LEN;
    if bytes[split - 1] != KEY_TIME_SEPARATOR {
        return Err(RedbBackendError::key_encoding(
            "history key is missing its separator".to_string(),
        ));
    }

    let key = String::from_utf8(bytes[..split - 1].to_vec())
        .map_err(|e| RedbBackendError::key_encoding(format!("Invalid UTF-8 in key: {e}")))?;

    let mut ntp = [0u8; 8];
    ntp.copy_from_slice(&bytes[split..split + 8]);
    let id = TimestampId::try_from(&bytes[split + 8..])
        .map_err(|e| RedbBackendError::key_encoding(format!("Invalid timestamp id: {e:?}")))?;

    Ok((key, Timestamp::new(NTP64(u64::from_be_bytes(ntp)), id)))
}

/// What one retention pass did.
///
/// Reported on the admin space so that a policy is verifiable from outside the
/// process — "the storage says it is bounded" is not the same as "the storage is
/// bounded".
#[derive(Debug, Clone, Default)]
pub struct RetentionPass {
    /// When the pass ran. `None` if no pass has run yet.
    pub ran_at: Option<SystemTime>,
    /// How long it took.
    pub duration: Option<Duration>,
    /// Samples removed.
    pub samples_dropped: u64,
    /// File size before the pass.
    pub bytes_before: Option<u64>,
    /// File size after the pass, compaction included.
    pub bytes_after: Option<u64>,
}

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
    /// Rows in the metadata table, tombstones included. One per key.
    pub key_count: u64,
    /// Individual samples retained. Zero in [`HistoryMode::Latest`], where a key
    /// *is* its only sample; in [`HistoryMode::All`] this is the number that
    /// actually grows, and the one retention bounds.
    pub sample_count: u64,
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
    /// The redb database instance.
    ///
    /// Behind an `RwLock` only so that retention can compact: `Database::compact`
    /// needs `&mut`, while every other operation needs `&`. Read guards are held
    /// just long enough to begin a transaction — redb 4's transactions are owned,
    /// not borrowed from the database — so concurrent readers do not contend.
    db: RwLock<Database>,

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
            // "No such file or directory" is technically true and completely
            // unhelpful: the file is missing *because we were told not to create
            // it*. Say which setting caused that, since the two that can are far
            // apart in a config file.
            if !path.exists() {
                return RedbBackendError::other(format!(
                    "{path:?} does not exist, and this storage is configured not to \
                     create it ({}). Either create the database first, or set \
                     `create_db: true` and `read_only: false`. ({e})",
                    if config.read_only {
                        "`read_only: true` implies the database must already exist"
                    } else {
                        "`create_db: false`"
                    }
                ));
            }
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

    /// Borrow the database for a single operation.
    ///
    /// A poisoned lock is recovered rather than propagated: the guard is only ever
    /// held to start a transaction, so a panic elsewhere cannot have left redb's own
    /// state torn — redb's transactions are atomic independently of this lock.
    fn db(&self) -> std::sync::RwLockReadGuard<'_, Database> {
        self.db.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Begin a read transaction.
    fn begin_read(&self) -> Result<redb::ReadTransaction> {
        Ok(self.db().begin_read()?)
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
        let mut txn = self.db().begin_write()?;
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
            // Created unconditionally, in both modes. Opening a table is cheap, and
            // creating them up front means switching a volume to `history: "all"`
            // does not need a migration step on a database that already exists.
            write_txn.open_table(HISTORY_PAYLOADS_TABLE)?;
            write_txn.open_table(HISTORY_INFO_TABLE)?;
        }
        write_txn.commit()?;

        info!("Redb storage created successfully");

        Ok(Self {
            db: RwLock::new(db),
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
    /// Store a key-value pair with metadata.
    ///
    /// Last-writer-wins is decided here, inside the write transaction, so that a
    /// concurrent writer cannot read the same "existing" timestamp and have both
    /// conclude they are the newer one.
    ///
    /// In [`HistoryMode::All`] every sample is appended regardless of order — that
    /// is the point of the mode — and only the latest-value index is guarded by the
    /// timestamp comparison.
    pub fn put(&self, key: &str, value: StoredValue) -> Result<WriteOutcome> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        trace!("Putting key: {}", key);

        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let data_info_bytes = encode_data_info(
                value.encoding.clone(),
                &value.timestamp,
                false, // not deleted
            )?;

            let write_txn = self.begin_write()?;
            let outcome;
            {
                let mut payloads_table = write_txn.open_table(PAYLOADS_TABLE)?;
                let mut data_info_table = write_txn.open_table(DATA_INFO_TABLE)?;

                let existing = match data_info_table.get(key_buf.as_slice())? {
                    Some(guard) => Some(decode_data_info(guard.value())?.1),
                    None => None,
                };
                let is_newer = existing.is_none_or(|stored| value.timestamp > stored);

                if self.config.history == HistoryMode::All {
                    // Append the sample under its own (key, timestamp). Ordering
                    // does not gate this: a late-arriving sample is still a fact
                    // about the instant it carries.
                    let history_key = encode_history_key(key, &value.timestamp);
                    write_txn
                        .open_table(HISTORY_PAYLOADS_TABLE)?
                        .insert(history_key.as_slice(), value.payload.as_slice())?;
                    write_txn
                        .open_table(HISTORY_INFO_TABLE)?
                        .insert(history_key.as_slice(), data_info_bytes.as_slice())?;
                } else if !is_newer {
                    // Latest-only: an older sample has nowhere to go.
                    drop(payloads_table);
                    drop(data_info_table);
                    write_txn.abort()?;
                    debug!("Ignoring outdated put for key: {}", key);
                    return Ok(WriteOutcome::Outdated);
                }

                if is_newer {
                    payloads_table.insert(key_buf.as_slice(), value.payload.as_slice())?;
                    data_info_table.insert(key_buf.as_slice(), data_info_bytes.as_slice())?;
                }

                outcome = if is_newer && existing.is_some() {
                    WriteOutcome::Replaced
                } else {
                    // Either the key was new, or this is an `all`-mode append that
                    // did not disturb the latest value. Both added a sample.
                    WriteOutcome::Inserted
                };
            }
            write_txn.commit()?;

            debug!("Stored key: {}", key);
            Ok(outcome)
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

            let read_txn = self.begin_read()?;
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
    pub fn delete(&self, key: &str, timestamp: Timestamp) -> Result<WriteOutcome> {
        if self.config.read_only {
            return Err(RedbBackendError::other("Storage is read-only"));
        }

        trace!("Deleting key: {}", key);

        KEY_BUFFER.with(|key_buf| {
            let mut key_buf = key_buf.borrow_mut();
            key_buf.clear();
            self.encode_key_into(key, &mut key_buf)?;

            let tombstone = encode_data_info(Encoding::ZENOH_BYTES, &timestamp, true)?;

            let write_txn = self.begin_write()?;
            let outcome;
            {
                let mut payloads_table = write_txn.open_table(PAYLOADS_TABLE)?;
                let mut data_info_table = write_txn.open_table(DATA_INFO_TABLE)?;

                let existing = match data_info_table.get(key_buf.as_slice())? {
                    Some(guard) => Some(decode_data_info(guard.value())?.1),
                    None => None,
                };
                // Same last-writer-wins rule as `put`, decided in the same
                // transaction: a DELETE that predates the stored value must not
                // remove it.
                //
                // `>=`, not `>`: a deletion carrying the same timestamp as the value
                // it retires is the ordinary case when a producer PUTs and DELETEs
                // within one timestamp tick, and it must succeed. Only a strictly
                // older deletion is rejected. (`put` uses the opposite tie-break —
                // an equal-timestamp PUT is a duplicate, not an update.)
                let is_newer = existing.is_none_or(|stored| timestamp >= stored);

                if self.config.history == HistoryMode::All {
                    // A deletion is a fact about an instant, so it is appended
                    // regardless of order — an out-of-order DELETE still bounds the
                    // validity of whatever preceded it.
                    let history_key = encode_history_key(key, &timestamp);
                    write_txn
                        .open_table(HISTORY_PAYLOADS_TABLE)?
                        .insert(history_key.as_slice(), [].as_slice())?;
                    write_txn
                        .open_table(HISTORY_INFO_TABLE)?
                        .insert(history_key.as_slice(), tombstone.as_slice())?;
                } else if !is_newer {
                    drop(payloads_table);
                    drop(data_info_table);
                    write_txn.abort()?;
                    debug!("Ignoring outdated delete for key: {}", key);
                    return Ok(WriteOutcome::Outdated);
                }

                if is_newer {
                    payloads_table.remove(key_buf.as_slice())?;

                    if self.config.history == HistoryMode::All {
                        // Leave a tombstone in the latest index rather than removing
                        // the row. The storage manager resolves every wildcard query
                        // through `get_all_entries`, which is built from this table:
                        // dropping the row would make the key's retained history
                        // unreachable by any wildcard `_time` selector, even though
                        // the samples are still on disk.
                        data_info_table.insert(key_buf.as_slice(), tombstone.as_slice())?;
                    } else {
                        data_info_table.remove(key_buf.as_slice())?;
                    }
                }

                outcome = if is_newer {
                    WriteOutcome::Replaced
                } else {
                    // All-mode, out of order: the tombstone was recorded but the
                    // latest value stands.
                    WriteOutcome::Inserted
                };
            }
            write_txn.commit()?;

            debug!("Deleted key: {}", key);
            Ok(outcome)
        })
    }

    /// Every sample of `key` whose timestamp falls inside `range`, oldest first.
    ///
    /// This is the read that `History::All` exists for. Because the composite key
    /// is `key || 0x00 || big-endian NTP64`, redb's own ordering is chronological
    /// ordering, so a time window is one bounded range scan — no secondary index,
    /// and no rows belonging to other keys are touched.
    ///
    /// Tombstones inside the window are **skipped**, not returned: a deletion has
    /// no value to reply with, and `StoredData` has nowhere to say "this one is a
    /// deletion".
    pub fn get_range(&self, key: &str, range: &TimeRange<SystemTime>) -> Result<Vec<StoredValue>> {
        let read_txn = self.begin_read()?;
        let payloads_table = read_txn.open_table(HISTORY_PAYLOADS_TABLE)?;
        let info_table = read_txn.open_table(HISTORY_INFO_TABLE)?;

        let (start, end) = history_key_bounds(key);
        let mut results = Vec::new();

        for item in info_table.range(start.as_slice()..end.as_slice())? {
            let (key_bytes, info_bytes) = item?;
            let (_, timestamp) = decode_history_key(key_bytes.value())?;

            // The scan is already bounded to this key; `contains` applies the
            // bound's inclusivity, which the byte range cannot express.
            if !range.contains(timestamp.get_time().to_system_time()) {
                continue;
            }

            let (encoding, timestamp, deleted) = decode_data_info(info_bytes.value())?;
            if deleted {
                continue;
            }

            if let Some(payload) = payloads_table.get(key_bytes.value())? {
                results.push(StoredValue::new(
                    payload.value().to_vec(),
                    timestamp,
                    encoding,
                ));
            }
        }

        Ok(results)
    }

    /// Retrieve all key-value pairs from the storage.
    pub fn get_all(&self) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting all entries");

        let read_txn = self.begin_read()?;
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
        let read_txn = self.begin_read()?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        match data_info_table.get(key.as_bytes())? {
            Some(info_guard) => {
                let (_, timestamp, _) = decode_data_info(info_guard.value())?;
                Ok(Some(timestamp))
            }
            None => Ok(None),
        }
    }

    /// Apply the configured retention policy once.
    ///
    /// Enforcement is periodic rather than per-write: doing it on every PUT would
    /// put a scan on the hot path. Deletions happen in bounded, committed batches
    /// so a pass never holds a write transaction open across the whole database.
    ///
    /// Returns what the pass did, which is reported on the admin space so retention
    /// is verifiable from outside the process.
    pub fn enforce_retention(&self) -> Result<RetentionPass> {
        let Some(policy) = self.config.retention.clone() else {
            return Ok(RetentionPass::default());
        };
        if self.config.read_only {
            return Ok(RetentionPass::default());
        }

        let started = SystemTime::now();
        let bytes_before = self.on_disk_bytes();
        let mut pass = RetentionPass {
            ran_at: Some(started),
            ..RetentionPass::default()
        };

        let now = NTP64::from(
            started
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| RedbBackendError::other(format!("system clock before epoch: {e}")))?,
        );

        // Which rows must go. Decided in a read transaction over the ordered table,
        // where every sample of a key is already contiguous and in time order, so
        // age, per-key count and decimation are all answerable in one pass.
        let doomed = self.select_expired(&policy, now)?;
        pass.samples_dropped = self.drop_history_rows(&doomed)?;

        // Size is enforced last, because the rules above may already have brought
        // the file under the limit.
        if let Some(max_bytes) = policy.max_bytes {
            pass.samples_dropped += self.enforce_max_bytes(max_bytes)?;
        }

        pass.bytes_before = bytes_before;
        pass.bytes_after = self.on_disk_bytes();
        pass.duration = started.elapsed().ok();

        if pass.samples_dropped > 0 {
            info!(
                "Retention pass on '{}': dropped {} samples, {:?} -> {:?} bytes",
                self.name, pass.samples_dropped, pass.bytes_before, pass.bytes_after
            );
        }
        Ok(pass)
    }

    /// Size of the database file, from the filesystem.
    fn on_disk_bytes(&self) -> Option<u64> {
        self.config
            .db_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
    }

    /// Composite keys that the age, per-key-count and decimation rules condemn.
    fn select_expired(&self, policy: &RetentionPolicy, now: NTP64) -> Result<Vec<Vec<u8>>> {
        let read_txn = self.begin_read()?;
        let info_table = read_txn.open_table(HISTORY_INFO_TABLE)?;

        let min_age_keep = policy
            .max_age_secs
            .map(|secs| now - NTP64::from(Duration::from_secs(secs)));
        let decimate_before = policy
            .decimate
            .as_ref()
            .map(|d| now - NTP64::from(Duration::from_secs(d.recent_secs)));

        let mut doomed = Vec::new();

        // Per-key state. The table is ordered by key then time, so a key's samples
        // arrive together and oldest-first; that is what lets one linear pass answer
        // all three rules without buffering a whole key's history.
        let mut current_key: Option<String> = None;
        let mut key_rows: Vec<(Vec<u8>, NTP64)> = Vec::new();

        let flush = |key_rows: &mut Vec<(Vec<u8>, NTP64)>, doomed: &mut Vec<Vec<u8>>| {
            if let Some(max) = policy.max_samples_per_key {
                let excess = (key_rows.len() as u64).saturating_sub(max) as usize;
                // Oldest first, so the excess is at the front.
                for (raw, _) in key_rows.iter().take(excess) {
                    doomed.push(raw.clone());
                }
            }
            key_rows.clear();
        };

        for item in info_table.iter()? {
            let (key_bytes, _) = item?;
            let raw = key_bytes.value().to_vec();
            let (key, timestamp) = decode_history_key(&raw)?;
            let time = *timestamp.get_time();

            if current_key.as_deref() != Some(key.as_str()) {
                flush(&mut key_rows, &mut doomed);
                current_key = Some(key);
            }

            // Rule 1: too old outright.
            if min_age_keep.is_some_and(|cutoff| time < cutoff) {
                doomed.push(raw);
                continue;
            }

            // Rule 2: beyond the full-resolution window, keep one per bucket.
            if let (Some(cutoff), Some(d)) = (decimate_before, policy.decimate.as_ref())
                && time < cutoff
            {
                let bucket = time.as_secs() as u64 / d.bucket_secs.max(1);
                let already_kept_this_bucket = key_rows.last().is_some_and(|(_, kept)| {
                    kept.as_secs() as u64 / d.bucket_secs.max(1) == bucket
                });
                if already_kept_this_bucket {
                    doomed.push(raw);
                    continue;
                }
            }

            key_rows.push((raw, time));
        }
        flush(&mut key_rows, &mut doomed);

        Ok(doomed)
    }

    /// Remove history rows in committed batches.
    ///
    /// Bounded batches rather than one transaction: a single write txn spanning
    /// millions of rows would hold the write lock for the whole pass, and retention
    /// must not block writes for minutes at a time.
    fn drop_history_rows(&self, keys: &[Vec<u8>]) -> Result<u64> {
        const BATCH: usize = 4096;
        let mut dropped = 0u64;

        for chunk in keys.chunks(BATCH) {
            let write_txn = self.begin_write()?;
            {
                let mut payloads = write_txn.open_table(HISTORY_PAYLOADS_TABLE)?;
                let mut info = write_txn.open_table(HISTORY_INFO_TABLE)?;
                for raw in chunk {
                    payloads.remove(raw.as_slice())?;
                    if info.remove(raw.as_slice())?.is_some() {
                        dropped += 1;
                    }
                }
            }
            write_txn.commit()?;
        }

        Ok(dropped)
    }

    /// Drop the oldest samples until the file is under `max_bytes`.
    ///
    /// This has to compact. redb does not return space to the filesystem when rows
    /// are removed, so without compaction the file size never falls, the policy
    /// never converges, and every pass would delete more data while reporting no
    /// improvement — silent, unbounded data loss dressed up as retention.
    fn enforce_max_bytes(&self, max_bytes: u64) -> Result<u64> {
        const MAX_ROUNDS: usize = 8;
        let mut dropped = 0u64;

        for _ in 0..MAX_ROUNDS {
            let Some(size) = self.on_disk_bytes() else {
                // Without a path we cannot measure, and guessing would be worse
                // than doing nothing.
                warn!(
                    "Storage '{}': max_bytes is configured but the database path is \
                     unknown, so size cannot be enforced",
                    self.name
                );
                return Ok(dropped);
            };
            if size <= max_bytes {
                break;
            }

            // Oldest samples across all keys. Collected by timestamp rather than by
            // table order, because table order is by key first.
            let mut all: Vec<(NTP64, Vec<u8>)> = {
                let read_txn = self.begin_read()?;
                let info = read_txn.open_table(HISTORY_INFO_TABLE)?;
                let mut v = Vec::new();
                for item in info.iter()? {
                    let (key_bytes, _) = item?;
                    let raw = key_bytes.value().to_vec();
                    let (_, ts) = decode_history_key(&raw)?;
                    v.push((*ts.get_time(), raw));
                }
                v
            };
            if all.is_empty() {
                // Nothing left to evict, yet the file is still over the limit. The
                // remainder is latest-value data and redb's own overhead, neither of
                // which retention may touch — deleting current values to satisfy a
                // size budget would be data loss, not retention. Say so loudly
                // instead of looping: this is a misconfiguration (max_bytes below
                // the storage's irreducible size), and silence would look like the
                // policy working.
                warn!(
                    "Storage '{}' is {} bytes, over its max_bytes of {}, but its \
                     history is already empty. The remainder is current values and \
                     redb overhead, which retention will not delete. Raise \
                     max_bytes or reduce what this storage holds.",
                    self.name, size, max_bytes
                );
                break;
            }
            all.sort_by_key(|(ts, _)| *ts);

            // Drop a proportional slice, so an over-limit file converges in a few
            // rounds instead of one row at a time.
            let over = size.saturating_sub(max_bytes) as f64 / size.max(1) as f64;
            let take = ((all.len() as f64 * over).ceil() as usize).clamp(1, all.len());
            let batch: Vec<Vec<u8>> = all.into_iter().take(take).map(|(_, raw)| raw).collect();

            dropped += self.drop_history_rows(&batch)?;

            // Reclaim the space to the filesystem. A compaction can legitimately
            // refuse (redb declines while transactions are outstanding), and that
            // must not fail the pass: the deletions above are already committed and
            // correct. The next pass will try again.
            let mut db = self.db.write().unwrap_or_else(|e| e.into_inner());
            match db.compact() {
                Ok(true) => trace!("Compacted '{}' after retention", self.name),
                Ok(false) => trace!("Nothing left to compact on '{}'", self.name),
                Err(e) => {
                    warn!(
                        "Compaction after retention on '{}' failed: {}",
                        self.name, e
                    );
                    drop(db);
                    break;
                }
            }
        }

        Ok(dropped)
    }

    /// What this storage costs, for the admin space.
    ///
    /// Sizes come from redb and the filesystem, never from adding up key and value
    /// lengths: the gap between "bytes I stored" and "bytes on disk" is exactly the
    /// fragmentation and metadata overhead an operator needs to see, and an
    /// estimate would hide it. `on_disk_bytes` is the real file.
    pub fn stats(&self) -> Result<StorageStats> {
        let read_txn = self.begin_read()?;
        let payloads_table = read_txn.open_table(PAYLOADS_TABLE)?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let history_payloads = read_txn.open_table(HISTORY_PAYLOADS_TABLE)?;
        let history_info = read_txn.open_table(HISTORY_INFO_TABLE)?;

        // All four tables, not just the latest-value pair. An `all`-mode storage
        // keeps almost everything in the history tables, so counting only the
        // latest ones would report a few megabytes against a multi-gigabyte file —
        // and since the docs tell an operator that the gap to `on_disk_bytes` is
        // what a compaction could reclaim, it would claim nearly the whole file is
        // reclaimable when almost none of it is.
        let table_stats = [
            payloads_table.stats()?,
            data_info_table.stats()?,
            history_payloads.stats()?,
            history_info.stats()?,
        ];
        let stored_bytes: u64 = table_stats.iter().map(|s| s.stored_bytes()).sum();
        let metadata_bytes: u64 = table_stats.iter().map(|s| s.metadata_bytes()).sum();
        let fragmented_bytes: u64 = table_stats.iter().map(|s| s.fragmented_bytes()).sum();

        let cache = self.db().cache_stats();

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
            stored_bytes,
            metadata_bytes,
            fragmented_bytes,
            key_count: data_info_table.len()?,
            sample_count: history_info.len()?,
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
        let read_txn = self.begin_read()?;
        let data_info_table = read_txn.open_table(DATA_INFO_TABLE)?;

        let mut results = Vec::new();
        for item in data_info_table.iter()? {
            let (key_bytes, info_bytes) = item?;
            let (_, timestamp, deleted) = decode_data_info(info_bytes.value())?;

            // In `all` mode a tombstoned key is still enumerable. The storage
            // manager resolves every wildcard query through this list and then GETs
            // each key, so omitting a deleted key would hide its whole retained
            // history from `_time` selectors while a direct GET on the exact key
            // still returned it.
            if !deleted || self.config.history == HistoryMode::All {
                results.push((self.decode_key(key_bytes.value())?, timestamp));
            }
        }

        Ok(results)
    }

    /// Retrieve all key-value pairs matching a given prefix.
    pub fn get_by_prefix(&self, prefix: &str) -> Result<Vec<(String, StoredValue)>> {
        trace!("Getting entries by prefix: {}", prefix);

        let read_txn = self.begin_read()?;
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

        let read_txn = self.begin_read()?;
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
        let read_txn = self.begin_read()?;
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
            // Delete and recreate the tables - much more efficient than removing
            // keys one by one. The history tables go too: leaving them behind would
            // make a "cleared" all-mode storage still answer every `_time` query
            // with the full history it was just told to forget.
            write_txn.delete_table(PAYLOADS_TABLE)?;
            write_txn.delete_table(DATA_INFO_TABLE)?;
            write_txn.delete_table(HISTORY_PAYLOADS_TABLE)?;
            write_txn.delete_table(HISTORY_INFO_TABLE)?;

            // Recreate them
            write_txn.open_table(PAYLOADS_TABLE)?;
            write_txn.open_table(DATA_INFO_TABLE)?;
            write_txn.open_table(HISTORY_PAYLOADS_TABLE)?;
            write_txn.open_table(HISTORY_INFO_TABLE)?;
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
    use crate::config::DecimationPolicy;
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

        storage
            .delete(
                "test/key",
                Timestamp::new(NTP64(999999999), TimestampId::rand()),
            )
            .unwrap();
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

    fn history_storage() -> (RedbStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("history.redb");
        let config = RedbStorageConfig::default().with_history(HistoryMode::All);
        let storage = RedbStorage::new(db_path, config, "history".to_string()).unwrap();
        (storage, temp_dir)
    }

    fn at(secs: u64, id: TimestampId) -> Timestamp {
        Timestamp::new(NTP64::from(std::time::Duration::from_secs(secs)), id)
    }

    fn full_range() -> TimeRange<SystemTime> {
        TimeRange {
            start: zenoh_util::time_range::TimeBound::Unbounded,
            end: zenoh_util::time_range::TimeBound::Unbounded,
        }
    }

    /// Clearing an `all`-mode storage must forget the history too, not just the
    /// latest values — otherwise a cleared storage still answers every `_time`
    /// query with everything it was told to forget.
    #[test]
    fn clear_forgets_the_history_as_well() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();

        for secs in [100, 200, 300] {
            storage
                .put(
                    "k",
                    StoredValue::new(b"x".to_vec(), at(secs, id), Encoding::ZENOH_BYTES),
                )
                .unwrap();
        }
        assert_eq!(storage.get_range("k", &full_range()).unwrap().len(), 3);

        storage.clear().unwrap();

        assert!(storage.get("k").unwrap().is_none());
        assert!(
            storage.get_range("k", &full_range()).unwrap().is_empty(),
            "a cleared storage must not still serve its history"
        );
    }

    /// An out-of-order DELETE must not erase a newer value, in either mode.
    ///
    /// In `all` mode the storage manager does *not* pre-filter outdated samples —
    /// it only does that for latest-value volumes — so a replayed or aligned
    /// deletion reaches the backend as the normal case, not the exception.
    #[test]
    fn an_out_of_order_delete_does_not_erase_a_newer_value() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();

        storage
            .put(
                "k",
                StoredValue::new(b"newer".to_vec(), at(200, id), Encoding::ZENOH_BYTES),
            )
            .unwrap();

        let outcome = storage.delete("k", at(100, id)).unwrap();

        assert_eq!(
            storage.get("k").unwrap().map(|v| v.payload),
            Some(b"newer".to_vec()),
            "a DELETE older than the stored value must not remove it"
        );
        assert_eq!(
            outcome,
            WriteOutcome::Inserted,
            "the tombstone was recorded"
        );

        // ...but it *is* recorded in the history, because it is a fact about t=100.
        // The value written at t=200 is still the only thing with a payload.
        let samples = storage.get_range("k", &full_range()).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].payload, b"newer");

        // A newer DELETE does remove it.
        assert_eq!(
            storage.delete("k", at(300, id)).unwrap(),
            WriteOutcome::Replaced
        );
        assert!(storage.get("k").unwrap().is_none());
    }

    /// A deleted key must stay enumerable in `all` mode, or its retained history
    /// becomes unreachable through every wildcard selector.
    ///
    /// The storage manager resolves wildcard queries by listing entries and then
    /// GETting each key. Dropping a tombstoned key from that list would hide its
    /// whole history from `_time` selectors while a direct GET on the exact key
    /// still answered — the same query returning different data depending on how
    /// it was spelled.
    #[test]
    fn a_deleted_key_stays_enumerable_in_all_mode() {
        let (history, _t1) = history_storage();
        let (latest, _t2) = create_test_storage();
        let id = TimestampId::rand();

        for storage in [&history, &latest] {
            storage
                .put(
                    "k",
                    StoredValue::new(b"v".to_vec(), at(100, id), Encoding::ZENOH_BYTES),
                )
                .unwrap();
            storage.delete("k", at(200, id)).unwrap();
        }

        assert_eq!(
            history.get_all_timestamps().unwrap().len(),
            1,
            "an all-mode storage must still list a tombstoned key so its history is reachable"
        );
        assert_eq!(
            latest.get_all_timestamps().unwrap().len(),
            0,
            "a latest-mode storage has nothing left to reach, so it must not list it"
        );

        // Either way, the key itself reads as absent.
        assert!(history.get("k").unwrap().is_none());
        assert!(latest.get("k").unwrap().is_none());
    }

    /// Statistics must account for the history tables, which are where an
    /// `all`-mode storage keeps essentially everything.
    #[test]
    fn stats_count_history_not_just_the_latest_values() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();

        for secs in 0..20u64 {
            storage
                .put(
                    "k",
                    StoredValue::new(vec![0u8; 1024], at(100 + secs, id), Encoding::ZENOH_BYTES),
                )
                .unwrap();
        }

        let stats = storage.stats().unwrap();

        assert_eq!(stats.key_count, 1, "one key");
        assert_eq!(stats.sample_count, 20, "twenty samples under it");
        assert!(
            stats.stored_bytes >= 20 * 1024,
            "stored_bytes must cover the history, not just the latest value: {}",
            stats.stored_bytes
        );
    }

    fn retained_storage(policy: RetentionPolicy, dir: &TempDir) -> RedbStorage {
        let db_path = dir.path().join("retained.redb");
        let config = RedbStorageConfig::default()
            .with_db_path(db_path.clone())
            .with_history(HistoryMode::All)
            .with_retention(policy);
        RedbStorage::new(db_path, config, "retained".to_string()).unwrap()
    }

    /// Fill `key` with one sample per `step` seconds, ending `now`, and return the
    /// payloads written, in order.
    ///
    /// The return value is the point: the wall clock is read *once*, here. A test
    /// that re-derived the expected payloads from a second `SystemTime::now()`
    /// disagreed with this one whenever a second ticked in between — a flake that
    /// looked exactly like a retention bug.
    fn fill(
        storage: &RedbStorage,
        key: &str,
        count: u64,
        step: u64,
        id: TimestampId,
    ) -> Vec<String> {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut written = Vec::new();
        for i in 0..count {
            let secs = now - (count - i) * step;
            let payload = format!("{secs}");
            storage
                .put(
                    key,
                    StoredValue::new(
                        payload.clone().into_bytes(),
                        at(secs, id),
                        Encoding::ZENOH_BYTES,
                    ),
                )
                .unwrap();
            written.push(payload);
        }
        written
    }

    /// `max_age_secs` must drop older samples and keep newer ones — and the effect
    /// must survive a reopen, since retention that only exists in memory is not
    /// retention.
    #[test]
    fn max_age_drops_old_samples_and_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        let id = TimestampId::rand();

        let policy = RetentionPolicy {
            max_age_secs: Some(500),
            ..Default::default()
        };

        {
            let storage = retained_storage(policy.clone(), &dir);
            // 20 samples, 100s apart: the oldest ~2000s back, the newest ~100s back.
            fill(&storage, "k", 20, 100, id);
            assert_eq!(storage.get_range("k", &full_range()).unwrap().len(), 20);

            let pass = storage.enforce_retention().unwrap();
            assert!(pass.samples_dropped > 0, "old samples must be dropped");

            let kept = storage.get_range("k", &full_range()).unwrap();
            assert!(!kept.is_empty(), "recent samples must be kept");
            assert!(kept.len() < 20);

            // Everything kept is inside the window.
            let cutoff = SystemTime::now() - Duration::from_secs(500);
            for sample in &kept {
                assert!(
                    sample.timestamp.get_time().to_system_time() >= cutoff,
                    "a sample older than max_age_secs survived the pass"
                );
            }
        }

        // Reopen: the drop was committed to disk, not just to a cache.
        let storage = retained_storage(policy, &dir);
        let kept = storage.get_range("k", &full_range()).unwrap();
        assert!(!kept.is_empty());
        assert!(kept.len() < 20, "dropped samples must not come back");
    }

    /// `max_samples_per_key` must bound one key without touching another's history.
    #[test]
    fn max_samples_per_key_bounds_each_key_independently() {
        let dir = TempDir::new().unwrap();
        let id = TimestampId::rand();

        let storage = retained_storage(
            RetentionPolicy {
                max_samples_per_key: Some(5),
                ..Default::default()
            },
            &dir,
        );

        let busy = fill(&storage, "busy", 20, 1, id);
        fill(&storage, "quiet", 3, 1, id);

        storage.enforce_retention().unwrap();

        assert_eq!(
            storage.get_range("busy", &full_range()).unwrap().len(),
            5,
            "the busy key must be trimmed to the limit"
        );
        assert_eq!(
            storage.get_range("quiet", &full_range()).unwrap().len(),
            3,
            "a key under the limit must be left alone — one pathological key must \
             not evict another key's history"
        );

        // What survived is the *newest* five, not an arbitrary five. Compared
        // against what `fill` actually wrote rather than against a freshly sampled
        // clock, which would disagree whenever a second ticked mid-test.
        let expected: Vec<String> = busy[busy.len() - 5..].to_vec();
        let kept: Vec<String> = storage
            .get_range("busy", &full_range())
            .unwrap()
            .iter()
            .map(|s| String::from_utf8(s.payload.clone()).unwrap())
            .collect();
        assert_eq!(kept, expected, "the newest five must be the ones kept");
    }

    /// Decimation keeps full resolution recently and one sample per bucket beyond.
    #[test]
    fn decimation_thins_only_beyond_the_recent_window() {
        let dir = TempDir::new().unwrap();
        let id = TimestampId::rand();

        let storage = retained_storage(
            RetentionPolicy {
                decimate: Some(DecimationPolicy {
                    recent_secs: 100,
                    bucket_secs: 100,
                }),
                ..Default::default()
            },
            &dir,
        );

        // 60 samples 10s apart: the last ~10 fall inside the 100s recent window.
        fill(&storage, "k", 60, 10, id);
        storage.enforce_retention().unwrap();

        let kept = storage.get_range("k", &full_range()).unwrap();
        assert!(kept.len() < 60, "older samples must have been thinned");

        let recent_cutoff = SystemTime::now() - Duration::from_secs(100);
        let recent = kept
            .iter()
            .filter(|s| s.timestamp.get_time().to_system_time() >= recent_cutoff)
            .count();
        assert!(
            recent >= 9,
            "samples inside the recent window must keep full resolution, kept {recent}"
        );

        // Beyond the window, at most one per 100s bucket.
        let mut buckets = std::collections::HashMap::new();
        for sample in &kept {
            let t = sample.timestamp.get_time();
            if t.to_system_time() < recent_cutoff {
                *buckets.entry(t.as_secs() as u64 / 100).or_insert(0u32) += 1;
            }
        }
        for (bucket, count) in &buckets {
            assert_eq!(
                *count, 1,
                "bucket {bucket} kept {count} samples, expected 1"
            );
        }
    }

    /// `max_bytes` must actually bring the file down, which means it must compact:
    /// redb does not return space to the filesystem when rows are removed, so
    /// without compaction the size never falls, the policy never converges, and
    /// every pass would delete more data while reporting no improvement.
    #[test]
    fn max_bytes_shrinks_the_file_on_disk() {
        let dir = TempDir::new().unwrap();
        let id = TimestampId::rand();

        let db_path = dir.path().join("sized.redb");
        let config = RedbStorageConfig::default()
            .with_db_path(db_path.clone())
            .with_history(HistoryMode::All)
            .with_retention(RetentionPolicy {
                max_bytes: Some(256 * 1024),
                ..Default::default()
            });
        let storage = RedbStorage::new(&db_path, config, "sized".to_string()).unwrap();

        // Write comfortably past the limit: 400 samples of 4 KiB.
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for i in 0..400u64 {
            storage
                .put(
                    "k",
                    StoredValue::new(
                        vec![0u8; 4096],
                        at(now - 400 + i, id),
                        Encoding::ZENOH_BYTES,
                    ),
                )
                .unwrap();
        }

        let before = std::fs::metadata(&db_path).unwrap().len();
        assert!(
            before > 256 * 1024,
            "test needs to start over the limit, was {before}"
        );

        let pass = storage.enforce_retention().unwrap();
        let after = std::fs::metadata(&db_path).unwrap().len();

        assert!(pass.samples_dropped > 0, "samples must have been evicted");
        assert!(
            after < before,
            "the file must actually shrink ({before} -> {after}); if it does not, \
             compaction is not happening and max_bytes can never converge"
        );

        // The newest samples are the ones that survive.
        let kept = storage.get_range("k", &full_range()).unwrap();
        assert!(!kept.is_empty(), "eviction must not empty the storage");
    }

    /// `max_bytes` set below what the storage can possibly shrink to must not turn
    /// into an eviction loop that deletes everything and still reports failure.
    #[test]
    fn max_bytes_below_the_irreducible_size_stops_rather_than_deleting_everything() {
        let dir = TempDir::new().unwrap();
        let id = TimestampId::rand();

        let db_path = dir.path().join("tiny.redb");
        let config = RedbStorageConfig::default()
            .with_db_path(db_path.clone())
            .with_history(HistoryMode::All)
            // Far below any real redb file, which always carries page overhead.
            .with_retention(RetentionPolicy {
                max_bytes: Some(1),
                ..Default::default()
            });
        let storage = RedbStorage::new(&db_path, config, "tiny".to_string()).unwrap();

        for i in 0..10u64 {
            storage
                .put(
                    "k",
                    StoredValue::new(vec![0u8; 512], at(100 + i, id), Encoding::ZENOH_BYTES),
                )
                .unwrap();
        }

        // Must terminate, not spin, and must leave the current value intact even
        // though the size target is unreachable.
        let pass = storage.enforce_retention().unwrap();
        assert!(pass.ran_at.is_some());
        assert!(
            storage.get("k").unwrap().is_some(),
            "retention must never delete the current value to chase a size budget"
        );
    }

    /// A policy that bounds nothing is not a policy.
    #[test]
    fn an_empty_retention_policy_is_not_bounded() {
        assert!(!RetentionPolicy::default().is_bounded());
        assert!(
            RetentionPolicy {
                max_age_secs: Some(1),
                ..Default::default()
            }
            .is_bounded()
        );
    }

    /// A storage with no policy configured must not have its data quietly removed.
    #[test]
    fn no_policy_means_no_deletions() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();
        fill(&storage, "k", 10, 1000, id);

        let pass = storage.enforce_retention().unwrap();
        assert_eq!(pass.samples_dropped, 0);
        assert!(pass.ran_at.is_none());
        assert_eq!(storage.get_range("k", &full_range()).unwrap().len(), 10);
    }

    /// `read_only` / `create_db: false` on a database that does not exist must fail
    /// with an error that names the setting responsible.
    ///
    /// The bare redb error is "No such file or directory", which is true and
    /// useless: the file is missing precisely because the config said not to create
    /// it. This shipped as a broken storage in the example config until the flags
    /// were actually wired, so the message needs to point at the cause.
    #[test]
    fn opening_a_missing_database_read_only_explains_why() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("never-created.redb");

        for (config, expected) in [
            (
                RedbStorageConfig::default()
                    .with_db_path(path.clone())
                    .with_read_only(true),
                "read_only",
            ),
            (
                RedbStorageConfig::default()
                    .with_db_path(path.clone())
                    .with_create_db(false),
                "create_db",
            ),
        ] {
            let Err(err) = RedbStorage::new(&path, config, "probe".to_string()) else {
                panic!("a missing database must not be created here");
            };
            let msg = err.to_string();
            assert!(
                msg.contains(expected),
                "the error must name the setting responsible; got: {msg}"
            );
            assert!(!path.exists(), "nothing may have been created");
        }
    }

    /// A composite key must survive a round trip exactly — the timestamp is the
    /// addressing, so a lossy encode silently reorders history.
    #[test]
    fn history_key_round_trips() {
        let id = TimestampId::rand();
        let ts = Timestamp::new(NTP64(0x1234_5678_9abc_def0), id);

        let encoded = encode_history_key("v1/h-aaaa/telemetry/cpu", &ts);
        let (key, decoded) = decode_history_key(&encoded).unwrap();

        assert_eq!(key, "v1/h-aaaa/telemetry/cpu");
        assert_eq!(decoded, ts);
    }

    /// Byte order must make redb's ordering chronological, and the NUL separator
    /// must stop one key's rows from interleaving with a longer key's.
    ///
    /// `a/b` and `a/b/c` are the case that breaks a naive unframed encoding: `/`
    /// (0x2f) sorts above NUL, so without the separator `a/b`'s timestamp bytes
    /// could be read as part of `a/b/c`.
    #[test]
    fn history_keys_sort_chronologically_and_group_by_key() {
        let id = TimestampId::rand();

        let earlier = encode_history_key("a/b", &at(100, id));
        let later = encode_history_key("a/b", &at(200, id));
        assert!(
            earlier < later,
            "later timestamps must sort after earlier ones"
        );

        let nested = encode_history_key("a/b/c", &at(1, id));
        assert!(
            later < nested,
            "every row of `a/b` must sort before any row of `a/b/c`"
        );

        // And the bounds cover exactly one key's rows.
        let (start, end) = history_key_bounds("a/b");
        assert!(start.as_slice() <= earlier.as_slice() && earlier.as_slice() < end.as_slice());
        assert!(start.as_slice() <= later.as_slice() && later.as_slice() < end.as_slice());
        assert!(
            nested.as_slice() >= end.as_slice(),
            "`a/b/c` must fall outside `a/b`'s bounds"
        );
    }

    /// `all` mode keeps every sample; `latest` mode keeps one.
    #[test]
    fn all_mode_keeps_every_sample_latest_mode_keeps_one() {
        let id = TimestampId::rand();

        let (history, _t1) = history_storage();
        let (latest, _t2) = create_test_storage();

        for (i, storage) in [&history, &latest].into_iter().enumerate() {
            for secs in [100, 200, 300] {
                let value = StoredValue::new(
                    format!("sample-{secs}").into_bytes(),
                    at(secs, id),
                    Encoding::ZENOH_BYTES,
                );
                storage.put("k", value).unwrap();
            }
            let _ = i;
        }

        let samples = history.get_range("k", &full_range()).unwrap();
        assert_eq!(samples.len(), 3, "all mode must keep every sample");
        // Oldest first, which is what a range query is expected to return.
        assert_eq!(samples[0].payload, b"sample-100");
        assert_eq!(samples[2].payload, b"sample-300");

        // Both modes agree on the latest value.
        assert_eq!(history.get("k").unwrap().unwrap().payload, b"sample-300");
        assert_eq!(latest.get("k").unwrap().unwrap().payload, b"sample-300");
        assert_eq!(latest.get_range("k", &full_range()).unwrap().len(), 0);
    }

    /// A late-arriving sample is still a fact about the instant it carries: `all`
    /// mode must store it without letting it disturb the latest value.
    #[test]
    fn an_out_of_order_sample_lands_in_history_but_not_in_latest() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();

        storage
            .put(
                "k",
                StoredValue::new(b"newer".to_vec(), at(200, id), Encoding::ZENOH_BYTES),
            )
            .unwrap();
        let outcome = storage
            .put(
                "k",
                StoredValue::new(b"older".to_vec(), at(100, id), Encoding::ZENOH_BYTES),
            )
            .unwrap();

        assert_eq!(outcome, WriteOutcome::Inserted, "the sample was stored");
        assert_eq!(
            storage.get("k").unwrap().unwrap().payload,
            b"newer",
            "an older sample must not become the latest value"
        );
        assert_eq!(storage.get_range("k", &full_range()).unwrap().len(), 2);
    }

    /// A time window must return only what falls inside it, and must not reach
    /// into a neighbouring key's samples.
    #[test]
    fn a_time_window_returns_only_that_window() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();

        for secs in [100, 200, 300, 400] {
            storage
                .put(
                    "k",
                    StoredValue::new(
                        format!("{secs}").into_bytes(),
                        at(secs, id),
                        Encoding::ZENOH_BYTES,
                    ),
                )
                .unwrap();
            // A second key whose samples must never appear in `k`'s answers.
            storage
                .put(
                    "k2",
                    StoredValue::new(b"other".to_vec(), at(secs, id), Encoding::ZENOH_BYTES),
                )
                .unwrap();
        }

        use zenoh_util::time_range::TimeBound;
        let window = TimeRange {
            start: TimeBound::Inclusive(at(200, id).get_time().to_system_time()),
            end: TimeBound::Inclusive(at(300, id).get_time().to_system_time()),
        };

        let samples = storage.get_range("k", &window).unwrap();
        let payloads: Vec<_> = samples
            .iter()
            .map(|s| String::from_utf8(s.payload.clone()).unwrap())
            .collect();
        assert_eq!(payloads, vec!["200", "300"]);

        // Exclusive bounds must actually exclude.
        let exclusive = TimeRange {
            start: TimeBound::Exclusive(at(200, id).get_time().to_system_time()),
            end: TimeBound::Exclusive(at(400, id).get_time().to_system_time()),
        };
        let payloads: Vec<_> = storage
            .get_range("k", &exclusive)
            .unwrap()
            .iter()
            .map(|s| String::from_utf8(s.payload.clone()).unwrap())
            .collect();
        assert_eq!(payloads, vec!["300"]);
    }

    /// A deletion is a fact about a point in time. It must be recorded in the
    /// history, and it must not be replied to as if it were a value.
    #[test]
    fn a_deletion_is_recorded_but_never_replied() {
        let (storage, _temp) = history_storage();
        let id = TimestampId::rand();

        storage
            .put(
                "k",
                StoredValue::new(b"live".to_vec(), at(100, id), Encoding::ZENOH_BYTES),
            )
            .unwrap();
        storage.delete("k", at(200, id)).unwrap();
        storage
            .put(
                "k",
                StoredValue::new(b"again".to_vec(), at(300, id), Encoding::ZENOH_BYTES),
            )
            .unwrap();

        let payloads: Vec<_> = storage
            .get_range("k", &full_range())
            .unwrap()
            .iter()
            .map(|s| String::from_utf8(s.payload.clone()).unwrap())
            .collect();

        // The tombstone at t=200 is stored (it bounds the "live" value's validity)
        // but has no value to reply with, so it is skipped.
        assert_eq!(payloads, vec!["live", "again"]);
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

        storage
            .delete(
                "key2",
                Timestamp::new(NTP64(999999999), TimestampId::rand()),
            )
            .unwrap();
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
