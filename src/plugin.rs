//! Zenoh plugin implementation for the redb backend.
//!
//! This module provides the integration between the redb storage backend and
//! Zenoh's plugin system, implementing the required traits for Volume and Storage.

use crate::backend::RedbBackend;
use crate::config::{HistoryMode, RedbBackendConfig, RedbStorageConfig};

use crate::storage::{RedbStorage, StoredValue, WriteOutcome};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{debug, info, warn};
use zenoh::{
    Result as ZResult,
    bytes::{Encoding, ZBytes},
    internal::{bail, zenoh_home, zerror},
    key_expr::OwnedKeyExpr,
    query::Parameters,
    time::Timestamp,
    try_init_log_from_env,
};
use zenoh_backend_traits::{
    Capability, History, Persistence, Storage, StorageInsertionResult, StoredData, Volume,
    config::{StorageConfig, VolumeConfig},
};
use zenoh_plugin_trait::{Plugin, plugin_long_version, plugin_version};
use zenoh_util::ffi::JsonValue;
use zenoh_util::time_range::{TimeExpr, TimeRange};

/// The environment variable used to configure the root directory for all redb storages.
pub const SCOPE_ENV_VAR: &str = "ZENOH_BACKEND_REDB_ROOT";

/// The default root directory (within zenoh's home directory) if ZENOH_BACKEND_REDB_ROOT is not specified.
pub const DEFAULT_ROOT_DIR: &str = "zenoh_backend_redb";

// Storage configuration properties
pub const PROP_STORAGE_DIR: &str = "dir";
pub const PROP_STORAGE_DB_FILE: &str = "db_file";
pub const PROP_STORAGE_CREATE_DB: &str = "create_db";
pub const PROP_STORAGE_READ_ONLY: &str = "read_only";
pub const PROP_STORAGE_CACHE_SIZE: &str = "cache_size";
pub const PROP_STORAGE_FSYNC: &str = "fsync";

// Volume configuration properties
pub const PROP_VOLUME_HISTORY: &str = "history";

// Special key for None (when the prefix being stripped exactly matches the key)
pub const NONE_KEY: &str = "@@none_key@@";

/// The selector parameter Zenoh reserves for a time range.
pub const PARAM_TIME: &str = "_time";

/// Pull a resolved [`TimeRange`] out of a selector's query parameters.
///
/// Returns `Ok(None)` when no `_time` was given, which is the common case and must
/// stay cheap. A malformed `_time` is an error rather than a silent full-history
/// reply: answering the wrong window is worse than refusing.
fn parse_time_range(parameters: &str) -> ZResult<Option<TimeRange<SystemTime>>> {
    if parameters.is_empty() {
        return Ok(None);
    }

    let Some(spec) = Parameters::from(parameters)
        .get(PARAM_TIME)
        .map(str::to_owned)
    else {
        return Ok(None);
    };

    let range: TimeRange<TimeExpr> = spec
        .parse()
        .map_err(|e| zerror!("Invalid `{}={}` in selector: {:?}", PARAM_TIME, spec, e))?;

    // Resolve `now(...)` against a single instant, so both bounds of one query
    // agree on when "now" was.
    Ok(Some(range.resolve_at(SystemTime::now())))
}

/// The redb backend plugin.
pub struct RedbBackendPlugin {}

#[cfg(feature = "dynamic_plugin")]
zenoh_plugin_trait::declare_plugin!(RedbBackendPlugin);

impl Plugin for RedbBackendPlugin {
    type StartArgs = VolumeConfig;
    type Instance = Box<dyn Volume>;

    const DEFAULT_NAME: &'static str = "redb_backend";
    const PLUGIN_VERSION: &'static str = plugin_version!();
    const PLUGIN_LONG_VERSION: &'static str = plugin_long_version!();

    fn start(name: &str, config: &Self::StartArgs) -> ZResult<Self::Instance> {
        try_init_log_from_env();
        info!("redb backend {}", Self::PLUGIN_LONG_VERSION);

        // History is a *volume*-level choice; see `HistoryMode` for why it cannot be
        // per-storage. One plugin serves both: declare a second volume that names
        // this backend and sets `history: "all"`.
        let rest: serde_json::Map<String, serde_json::Value> = (&config.rest).into();
        let history = match rest.get(PROP_VOLUME_HISTORY) {
            None => HistoryMode::Latest,
            Some(serde_json::Value::String(mode)) => HistoryMode::parse(mode).ok_or_else(|| {
                zerror!(
                    "Volume '{}': `{}` must be \"latest\" or \"all\", got \"{}\"",
                    name,
                    PROP_VOLUME_HISTORY,
                    mode
                )
            })?,
            Some(other) => {
                bail!(
                    "Volume '{}': `{}` must be a string, got {}",
                    name,
                    PROP_VOLUME_HISTORY,
                    other
                )
            }
        };
        info!("redb volume '{}' history mode: {}", name, history.as_str());

        // Determine root directory
        let root = if let Some(dir) = std::env::var_os(SCOPE_ENV_VAR) {
            PathBuf::from(dir)
        } else {
            let mut dir = PathBuf::from(zenoh_home());
            dir.push(DEFAULT_ROOT_DIR);
            dir
        };

        // Create backend configuration
        let backend_config = RedbBackendConfig::new()
            .with_base_dir(root.clone())
            .with_create_dir(true);

        // Create backend
        let backend = RedbBackend::new(backend_config)
            .map_err(|e| zerror!("Failed to create redb backend: {}", e))?;

        // Prepare admin status
        let mut properties = HashMap::new();
        properties.insert("root".to_string(), root.to_string_lossy().to_string());
        properties.insert("version".to_string(), Self::PLUGIN_VERSION.to_string());
        properties.insert("history".to_string(), history.as_str().to_string());

        let admin_status: serde_json::Value = properties
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();

        Ok(Box::new(RedbVolume {
            admin_status,
            backend: Arc::new(backend),
            history,
        }))
    }
}

/// Volume implementation for redb backend.
pub struct RedbVolume {
    admin_status: serde_json::Value,
    backend: Arc<RedbBackend>,
    /// The history mode every storage on this volume inherits.
    history: HistoryMode,
}

#[async_trait]
impl Volume for RedbVolume {
    fn get_admin_status(&self) -> JsonValue {
        (&self.admin_status).into()
    }

    /// Report what this volume can do.
    ///
    /// The storage manager makes two decisions from this that a storage cannot
    /// override, which is why the mode is configured per volume:
    ///
    /// * a storage declaring `replication` refuses to start unless this says
    ///   `History::Latest`;
    /// * in `Latest` mode the manager drops outdated samples before they reach the
    ///   backend, and in `All` mode it forwards every sample.
    fn get_capability(&self) -> Capability {
        Capability {
            persistence: Persistence::Durable,
            history: match self.history {
                HistoryMode::Latest => History::Latest,
                HistoryMode::All => History::All,
            },
        }
    }

    async fn create_storage(&self, config: StorageConfig) -> ZResult<Box<dyn Storage>> {
        debug!("Creating redb storage with config: {:?}", config);

        let cfg = config.volume_cfg.into_serde_value();
        let volume_cfg = match cfg.as_object() {
            Some(v) => v,
            None => bail!("redb backed storages need volume-specific configurations"),
        };

        // Parse read_only property
        let read_only = match volume_cfg.get(PROP_STORAGE_READ_ONLY) {
            None | Some(serde_json::Value::Bool(false)) => false,
            Some(serde_json::Value::Bool(true)) => true,
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a boolean",
                    PROP_STORAGE_READ_ONLY
                )
            }
        };

        // Parse create_db property
        let create_db = match volume_cfg.get(PROP_STORAGE_CREATE_DB) {
            None | Some(serde_json::Value::Bool(true)) => true,
            Some(serde_json::Value::Bool(false)) => false,
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a boolean",
                    PROP_STORAGE_CREATE_DB
                )
            }
        };

        // Parse fsync property
        let fsync = match volume_cfg.get(PROP_STORAGE_FSYNC) {
            None | Some(serde_json::Value::Bool(true)) => true,
            Some(serde_json::Value::Bool(false)) => false,
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a boolean",
                    PROP_STORAGE_FSYNC
                )
            }
        };

        // Parse cache_size property
        let cache_size = match volume_cfg.get(PROP_STORAGE_CACHE_SIZE) {
            None => None,
            Some(serde_json::Value::Number(n)) => {
                if let Some(size) = n.as_u64() {
                    Some(size as usize)
                } else {
                    bail!(
                        "Optional property `{}` of redb storage configurations must be a positive number",
                        PROP_STORAGE_CACHE_SIZE
                    )
                }
            }
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a number",
                    PROP_STORAGE_CACHE_SIZE
                )
            }
        };

        // Determine database path
        let db_path = if let Some(serde_json::Value::String(dir)) = volume_cfg.get(PROP_STORAGE_DIR)
        {
            let mut path = self.backend.config().base_dir.clone();
            path.push(dir);
            path.set_extension("redb");
            path
        } else if let Some(serde_json::Value::String(filename)) =
            volume_cfg.get(PROP_STORAGE_DB_FILE)
        {
            let mut path = self.backend.config().base_dir.clone();
            path.push(filename);
            if path.extension().is_none() {
                path.set_extension("redb");
            }
            path
        } else {
            bail!(
                "Required property `{}` or `{}` for redb Storage must be a string",
                PROP_STORAGE_DIR,
                PROP_STORAGE_DB_FILE
            )
        };

        // Create storage configuration
        let mut storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(create_db)
            .with_read_only(read_only)
            .with_fsync(fsync);

        if let Some(size) = cache_size {
            storage_config = storage_config.with_cache_size(size);
        }
        storage_config = storage_config.with_history(self.history);

        // Get storage name from config
        let storage_name = config.name.clone();

        // Create the storage directly (not using backend.create_storage to avoid double management)
        let redb_storage = RedbStorage::new(&db_path, storage_config.clone(), storage_name.clone())
            .map_err(|e| zerror!("Failed to create redb storage: {}", e))?;

        info!("Created redb storage '{}' at {:?}", storage_name, db_path);

        Ok(Box::new(RedbStoragePlugin {
            config,
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        }))
    }
}

/// Storage implementation for redb backend.
struct RedbStoragePlugin {
    config: StorageConfig,
    /// The storage itself. Every `RedbStorage` method takes `&self` and redb does
    /// its own concurrency control, so reads need no lock — which is what lets the
    /// *synchronous* `get_admin_status` report live statistics rather than having
    /// to give up under contention.
    storage: Arc<RedbStorage>,
    /// Serialises the read-then-write in `put` and `delete`. Those compare the
    /// incoming timestamp against the stored one and then act on the result, which
    /// has to be atomic or two concurrent writers can each decide they are newer.
    write_lock: Arc<tokio::sync::Mutex<()>>,
    storage_config: RedbStorageConfig,
}

#[async_trait]
impl Storage for RedbStoragePlugin {
    /// Report the storage's configuration *and* what it currently costs.
    ///
    /// This is the hook Zenoh gives a backend to describe itself on the admin
    /// space, and it is already read by `zenctl storage list` and the GUI's storage
    /// panel. Reporting only the config let an operator see that a storage existed
    /// but not what it was consuming — and "something grew and nobody was watching
    /// the number" is the shape of most storage incidents.
    fn get_admin_status(&self) -> JsonValue {
        let mut status = self.config.to_json_value();

        let capability = json!({
            "persistence": "durable",
            "history": self.storage_config.history.as_str(),
        });

        let stats = match self.storage.stats() {
            Ok(stats) => {
                let mut cache = json!({
                    "size_bytes": stats.cache_size_bytes,
                    "used_bytes": stats.cache_used_bytes,
                    "read_hits": stats.cache_read_hits,
                    "read_misses": stats.cache_read_misses,
                    "evictions": stats.cache_evictions,
                });
                // Absent rather than 0.0 before any read: a ratio nobody has
                // measured yet must not look like a cache that is missing every
                // time.
                if let Some(ratio) = stats.cache_hit_ratio() {
                    cache["hit_ratio"] = json!(ratio);
                }

                json!({
                    "on_disk_bytes": stats.on_disk_bytes,
                    "stored_bytes": stats.stored_bytes,
                    "metadata_bytes": stats.metadata_bytes,
                    "fragmented_bytes": stats.fragmented_bytes,
                    "key_count": stats.key_count,
                    "sample_count": stats.sample_count,
                    "live_keys": stats.live_keys,
                    "tombstones": stats.tombstones,
                    "oldest_timestamp": stats.oldest_timestamp.map(|t| t.to_string()),
                    "newest_timestamp": stats.newest_timestamp.map(|t| t.to_string()),
                    "cache": cache,
                })
            }
            Err(e) => {
                // Never fail the admin space over statistics: a storage that is
                // serving correctly must still report itself.
                warn!("Failed to collect storage statistics: {}", e);
                json!({ "error": e.to_string() })
            }
        };

        if let Some(obj) = status.as_object_mut() {
            obj.insert("capability".to_string(), capability);
            obj.insert("stats".to_string(), stats);
            if let Some(path) = &self.storage_config.db_path {
                obj.insert(
                    "db_path".to_string(),
                    json!(path.to_string_lossy().to_string()),
                );
            }
        }

        status.into()
    }

    async fn put(
        &mut self,
        key: Option<OwnedKeyExpr>,
        payload: ZBytes,
        encoding: Encoding,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let _write = self.write_lock.lock().await;
        let storage = &self.storage;

        if self.storage_config.read_only {
            warn!("Received PUT for read-only DB on {:?} - ignored", key);
            return Err("Received update for read-only DB".into());
        }

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Storing key: {} with timestamp: {}", key_str, timestamp);

        // Convert ZBytes to Vec<u8>
        let payload_bytes = payload.to_bytes().to_vec();

        // Create stored value with native Zenoh timestamp (preserves both time and ID)
        let value = StoredValue::new(payload_bytes, timestamp, encoding);

        // Last-writer-wins is decided inside the write transaction, not trusted from
        // upstream. The storage manager does filter outdated samples before calling
        // us, but against an in-memory cache seeded from `get_all_entries` at startup
        // (`storages_mgt/service.rs`, `guard_cache_if_latest`) — per-process and
        // per-storage, so replay, alignment and a restarted manager all bypass it.
        let outcome = storage
            .put(&key_str, value)
            .map_err(|e| zerror!("Failed to put key '{}': {}", key_str, e))?;

        Ok(match outcome {
            WriteOutcome::Inserted => StorageInsertionResult::Inserted,
            WriteOutcome::Replaced => StorageInsertionResult::Replaced,
            WriteOutcome::Outdated => {
                debug!("Ignored outdated PUT for {} at {}", key_str, timestamp);
                StorageInsertionResult::Outdated
            }
        })
    }

    async fn delete(
        &mut self,
        key: Option<OwnedKeyExpr>,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let _write = self.write_lock.lock().await;
        let storage = &self.storage;

        if self.storage_config.read_only {
            warn!("Received DELETE for read-only DB on {:?} - ignored", key);
            return Err("Received update for read-only DB".into());
        }

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Deleting key: {} with timestamp: {}", key_str, timestamp);

        // Last-writer-wins is decided inside the write transaction, the same way
        // `put` decides it — a DELETE that predates the value we hold must not
        // remove it. In `all` mode the tombstone is still appended to the history
        // either way, because "this key was deleted at t" is a fact about t
        // regardless of what arrived afterwards.
        //
        // Deleting an absent key is not an error: the storage manager replays
        // deletions during alignment and expects them to be idempotent.
        let outcome = storage
            .delete(&key_str, timestamp)
            .map_err(|e| zerror!("Failed to delete key '{}': {}", key_str, e))?;

        Ok(match outcome {
            WriteOutcome::Outdated => {
                debug!("Ignored outdated DELETE for {} at {}", key_str, timestamp);
                StorageInsertionResult::Outdated
            }
            _ => StorageInsertionResult::Deleted,
        })
    }

    async fn get(
        &mut self,
        key: Option<OwnedKeyExpr>,
        parameters: &str,
    ) -> ZResult<Vec<StoredData>> {
        let storage = &self.storage;

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Getting key: {}", key_str);

        // A `_time`-ranged GET is the whole point of `history: "all"`. The selector's
        // query parameters reach a backend verbatim, and Zenoh ships the parser, so
        // both documented syntaxes work: `[start..end]` and `[start;duration]`, with
        // `now(-1h)`-style relative expressions.
        if let Some(range) = parse_time_range(parameters)? {
            if self.storage_config.history != HistoryMode::All {
                // Answering a time range from a latest-only storage would silently
                // return one sample and look like "there was no other data".
                warn!(
                    "Ignoring `_time` on storage '{}': its volume is history: \"latest\", \
                     which keeps one value per key. Configure a volume with \
                     history: \"all\" to serve time ranges.",
                    self.config.name
                );
            } else {
                let samples = storage
                    .get_range(&key_str, &range)
                    .map_err(|e| zerror!("Failed to range-get key '{}': {}", key_str, e))?;

                return Ok(samples
                    .into_iter()
                    .map(|v| StoredData {
                        payload: ZBytes::from(v.payload),
                        encoding: v.encoding,
                        timestamp: v.timestamp,
                    })
                    .collect());
            }
        }

        match storage
            .get(&key_str)
            .map_err(|e| zerror!("Failed to get key '{}': {}", key_str, e))?
        {
            Some(stored_value) => {
                // Convert back to Zenoh types
                let payload = ZBytes::from(stored_value.payload);
                let encoding = stored_value.encoding.clone();
                let timestamp = stored_value.timestamp;

                Ok(vec![StoredData {
                    payload,
                    encoding,
                    timestamp,
                }])
            }
            None => Ok(vec![]),
        }
    }

    async fn get_all_entries(&self) -> ZResult<Vec<(Option<OwnedKeyExpr>, Timestamp)>> {
        let storage = &self.storage;

        debug!("Getting all entries");

        // Metadata only. The storage manager calls this to resolve *every* wildcard
        // query (it intersects the selector itself, then issues per-key GETs), so
        // reading payloads here would mean loading the whole database into memory
        // and throwing it away on each one.
        let entries = storage
            .get_all_timestamps()
            .map_err(|e| zerror!("Failed to get all entries: {}", e))?;

        let mut result = Vec::new();
        for (key_str, timestamp) in entries {
            // Convert key string back to OwnedKeyExpr
            let key_expr = if key_str == NONE_KEY {
                None
            } else {
                match OwnedKeyExpr::new(key_str.as_str()) {
                    Ok(ke) => Some(ke),
                    Err(e) => {
                        warn!("Invalid key in database: '{}' - {}", key_str, e);
                        continue;
                    }
                }
            };

            result.push((key_expr, timestamp));
        }

        debug!("Retrieved {} entries", result.len());
        Ok(result)
    }
}

impl Drop for RedbStoragePlugin {
    fn drop(&mut self) {
        debug!("Dropping redb storage plugin");
        // Storage cleanup is handled automatically by RedbStorage's Drop implementation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zenoh::time::NTP64;

    #[test]
    fn test_plugin_constants() {
        assert_eq!(RedbBackendPlugin::DEFAULT_NAME, "redb_backend");
        assert!(!RedbBackendPlugin::PLUGIN_VERSION.is_empty());
        assert!(!RedbBackendPlugin::PLUGIN_LONG_VERSION.is_empty());
    }

    #[test]
    fn test_default_root_dir() {
        assert_eq!(DEFAULT_ROOT_DIR, "zenoh_backend_redb");
    }

    #[test]
    fn test_special_none_key() {
        assert_eq!(NONE_KEY, "@@none_key@@");
    }

    #[test]
    fn test_environment_variable_constant() {
        assert_eq!(SCOPE_ENV_VAR, "ZENOH_BACKEND_REDB_ROOT");
    }

    #[test]
    fn test_property_constants() {
        assert_eq!(PROP_STORAGE_DIR, "dir");
        assert_eq!(PROP_STORAGE_DB_FILE, "db_file");
        assert_eq!(PROP_STORAGE_CREATE_DB, "create_db");
        assert_eq!(PROP_STORAGE_READ_ONLY, "read_only");
        assert_eq!(PROP_STORAGE_CACHE_SIZE, "cache_size");
        assert_eq!(PROP_STORAGE_FSYNC, "fsync");
    }

    #[test]
    fn test_redb_volume_structure() {
        // Test that RedbVolume can be constructed
        let backend_config = RedbBackendConfig::new();
        let backend = RedbBackend::new(backend_config).unwrap();

        let properties: HashMap<String, String> = HashMap::new();
        let admin_status: serde_json::Value = properties
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();

        let volume = RedbVolume {
            history: HistoryMode::Latest,
            admin_status,
            backend: Arc::new(backend),
        };

        // Verify capability
        let cap = volume.get_capability();
        assert_eq!(cap.persistence, Persistence::Durable);
        assert_eq!(cap.history, History::Latest);
    }

    #[tokio::test]
    async fn test_storage_plugin_drop() {
        // Test that RedbStoragePlugin drop doesn't panic
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        // Drop should work without panic
        drop(storage_plugin);
    }

    #[tokio::test]
    async fn test_storage_plugin_admin_status() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test_storage".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        let admin_status = storage_plugin.get_admin_status();

        // Just verify we can get admin status without panicking
        // The actual structure is implementation detail
        let _ = admin_status;
    }

    #[tokio::test]
    async fn test_storage_put_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        // Put data
        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        let result = storage_plugin
            .put(Some(key.clone()), payload.clone(), encoding, timestamp)
            .await;
        assert!(result.is_ok());

        // Get data
        let result = storage_plugin.get(Some(key), "").await;
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].payload.to_bytes(), payload.to_bytes());
    }

    /// A `_time`-ranged GET against an `all`-mode storage returns the window; the
    /// same GET without `_time` returns only the latest. That pair is the contract
    /// `History::All` exists to provide.
    #[tokio::test]
    async fn test_time_ranged_get_returns_the_window() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("history.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true)
            .with_history(HistoryMode::All);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "history".to_string()).unwrap();

        let mut plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "history".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "redb-history".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        let key = OwnedKeyExpr::new("test/cpu").unwrap();
        let id = zenoh::time::TimestampId::rand();

        // Five samples, one per second, ending "now" so that a relative `_time`
        // expression has something to find.
        let now = SystemTime::now();
        for i in 0..5u64 {
            let at = now - std::time::Duration::from_secs(10 - i);
            let ntp = NTP64::from(at.duration_since(std::time::UNIX_EPOCH).unwrap());
            plugin
                .put(
                    Some(key.clone()),
                    ZBytes::from(format!("sample{i}")),
                    Encoding::ZENOH_STRING,
                    Timestamp::new(ntp, id),
                )
                .await
                .unwrap();
        }

        // No `_time`: the latest value only.
        let latest = plugin.get(Some(key.clone()), "").await.unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].payload.to_bytes().as_ref(), b"sample4");

        // With `_time`: the whole window, oldest first.
        let windowed = plugin
            .get(Some(key.clone()), "_time=[now(-3600s)..now()]")
            .await
            .unwrap();
        assert_eq!(windowed.len(), 5, "every sample in the window");
        assert_eq!(windowed[0].payload.to_bytes().as_ref(), b"sample0");
        assert_eq!(windowed[4].payload.to_bytes().as_ref(), b"sample4");

        // A window that predates every sample returns nothing, rather than
        // falling back to the latest value.
        let empty = plugin
            .get(Some(key.clone()), "_time=[now(-7200s)..now(-3600s)]")
            .await
            .unwrap();
        assert!(empty.is_empty());

        // Other selector parameters must not be mistaken for a time range.
        let unrelated = plugin.get(Some(key), "foo=bar").await.unwrap();
        assert_eq!(unrelated.len(), 1);
    }

    /// A malformed `_time` must be an error, not a silent full-history reply:
    /// answering the wrong window is worse than refusing.
    #[test]
    fn test_malformed_time_range_is_rejected() {
        assert!(parse_time_range("_time=nonsense").is_err());
        assert!(parse_time_range("").unwrap().is_none());
        assert!(parse_time_range("foo=bar").unwrap().is_none());
        assert!(
            parse_time_range("_time=[now(-1h)..now()]")
                .unwrap()
                .is_some()
        );
    }

    /// The admin space must report what the storage costs, not just its config.
    #[tokio::test]
    async fn test_admin_status_reports_size_and_cache() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true)
            .with_cache_size(8 * 1024 * 1024);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        let id = zenoh::time::TimestampId::rand();
        for i in 0..5 {
            storage_plugin
                .put(
                    Some(OwnedKeyExpr::new(format!("test/k{i}")).unwrap()),
                    ZBytes::from(vec![0u8; 1024]),
                    Encoding::ZENOH_BYTES,
                    Timestamp::new(NTP64(100 + i), id),
                )
                .await
                .unwrap();
        }

        let status: serde_json::Value =
            serde_json::to_value(storage_plugin.get_admin_status()).unwrap();
        let stats = &status["stats"];

        assert_eq!(stats["key_count"], 5);
        assert_eq!(stats["live_keys"], 5);
        assert_eq!(stats["tombstones"], 0);

        // The size must be the real file, not an estimate summed from lengths.
        let on_disk = stats["on_disk_bytes"].as_u64().expect("on_disk_bytes");
        let actual = std::fs::metadata(&db_path).unwrap().len();
        assert_eq!(on_disk, actual);
        assert!(on_disk > 0);

        // Stored bytes must at least account for the payloads we wrote.
        assert!(
            stats["stored_bytes"].as_u64().unwrap() >= 5 * 1024,
            "stored_bytes {} should cover 5 KiB of payloads",
            stats["stored_bytes"]
        );

        assert_eq!(stats["cache"]["size_bytes"], 8 * 1024 * 1024);
        assert!(status["capability"]["persistence"] == "durable");

        // The timestamp span of what is held.
        assert!(stats["oldest_timestamp"].is_string());
        assert!(stats["newest_timestamp"].is_string());
    }

    /// A tombstone must be visible in the counts, distinctly from a live key.
    #[tokio::test]
    async fn test_admin_status_counts_are_not_estimates() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        let id = zenoh::time::TimestampId::rand();
        let key = OwnedKeyExpr::new("test/gone").unwrap();
        storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("x"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), id),
            )
            .await
            .unwrap();
        storage_plugin
            .delete(Some(key), Timestamp::new(NTP64(200), id))
            .await
            .unwrap();

        let status: serde_json::Value =
            serde_json::to_value(storage_plugin.get_admin_status()).unwrap();
        assert_eq!(status["stats"]["live_keys"], 0);
    }

    /// An out-of-order PUT must not clobber a newer stored value.
    ///
    /// The storage manager filters most of these upstream, but only against an
    /// in-memory cache seeded at startup — replay, alignment and a restarted manager
    /// all reach the backend directly.
    #[tokio::test]
    async fn test_storage_put_outdated_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        let key = OwnedKeyExpr::new("test/ordered").unwrap();
        let id = zenoh::time::TimestampId::rand();

        // First write of this key: Inserted.
        let newer = storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("newer"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(200), id),
            )
            .await
            .unwrap();
        assert!(matches!(newer, StorageInsertionResult::Inserted));

        // An older sample arrives late: rejected, and the payload is untouched.
        let older = storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("older"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), id),
            )
            .await
            .unwrap();
        assert!(matches!(older, StorageInsertionResult::Outdated));

        let data = storage_plugin.get(Some(key.clone()), "").await.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(
            data[0].payload.to_bytes().as_ref(),
            b"newer",
            "the older PUT must not have overwritten the newer value"
        );

        // A genuinely newer sample replaces it.
        let newest = storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("newest"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(300), id),
            )
            .await
            .unwrap();
        assert!(matches!(newest, StorageInsertionResult::Replaced));

        let data = storage_plugin.get(Some(key), "").await.unwrap();
        assert_eq!(data[0].payload.to_bytes().as_ref(), b"newest");
    }

    /// A DELETE that predates the stored value must not remove it.
    #[tokio::test]
    async fn test_storage_delete_outdated_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        let key = OwnedKeyExpr::new("test/tombstone").unwrap();
        let id = zenoh::time::TimestampId::rand();

        storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("live"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(200), id),
            )
            .await
            .unwrap();

        let stale = storage_plugin
            .delete(Some(key.clone()), Timestamp::new(NTP64(100), id))
            .await
            .unwrap();
        assert!(matches!(stale, StorageInsertionResult::Outdated));
        assert_eq!(
            storage_plugin
                .get(Some(key.clone()), "")
                .await
                .unwrap()
                .len(),
            1,
            "a stale DELETE must not remove a newer value"
        );

        let fresh = storage_plugin
            .delete(Some(key.clone()), Timestamp::new(NTP64(300), id))
            .await
            .unwrap();
        assert!(matches!(fresh, StorageInsertionResult::Deleted));
        assert!(storage_plugin.get(Some(key), "").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_storage_put_none_key() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        // Put with None key
        let payload = ZBytes::from("none_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        let result = storage_plugin
            .put(None, payload.clone(), encoding, timestamp)
            .await;
        assert!(result.is_ok());

        // Get with None key
        let result = storage_plugin.get(None, "").await;
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].payload.to_bytes(), payload.to_bytes());
    }

    #[tokio::test]
    async fn test_storage_delete() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        // Put data
        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        storage_plugin
            .put(Some(key.clone()), payload, encoding, timestamp)
            .await
            .unwrap();

        // Delete data
        let result = storage_plugin.delete(Some(key.clone()), timestamp).await;
        assert!(result.is_ok());

        // Verify it's gone
        let result = storage_plugin.get(Some(key), "").await;
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.len(), 0);
    }

    #[tokio::test]
    async fn test_storage_read_only_rejects_put() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // First create with write access
        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage = RedbStorage::new(&db_path, storage_config, "test".to_string()).unwrap();

        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        // Put some data first
        redb_storage
            .put(
                key.as_ref(),
                StoredValue::new(payload.to_bytes().to_vec(), timestamp, encoding.clone()),
            )
            .unwrap();

        drop(redb_storage);

        // Now open as read-only
        let ro_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(false)
            .with_read_only(true);

        let ro_storage = RedbStorage::new(&db_path, ro_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(ro_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config: ro_config,
        };

        // Try to put - should fail
        let new_payload = ZBytes::from("new_value");
        let result = storage_plugin
            .put(Some(key), new_payload, encoding, timestamp)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_storage_read_only_rejects_delete() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // First create with write access
        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage = RedbStorage::new(&db_path, storage_config, "test".to_string()).unwrap();

        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        // Put some data first
        redb_storage
            .put(
                key.as_ref(),
                StoredValue::new(payload.to_bytes().to_vec(), timestamp, encoding),
            )
            .unwrap();

        drop(redb_storage);

        // Now open as read-only
        let ro_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(false)
            .with_read_only(true);

        let ro_storage = RedbStorage::new(&db_path, ro_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(ro_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config: ro_config,
        };

        // Try to delete - should fail
        let result = storage_plugin.delete(Some(key), timestamp).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_storage_get_all_entries() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin {
            config: StorageConfig {
                name: "test".to_string(),
                key_expr: "test/**".parse().unwrap(),
                strip_prefix: None,
                volume_cfg: serde_json::Value::Object(Default::default()).into(),
                volume_id: "test_volume".to_string(),
                complete: false,
                garbage_collection_config: Default::default(),
                replication: None,
            },
            storage: Arc::new(redb_storage),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            storage_config,
        };

        // Put multiple entries
        for i in 1..=3 {
            let key = OwnedKeyExpr::new(format!("test/key{}", i)).unwrap();
            let payload = ZBytes::from(format!("value{}", i));
            let encoding = Encoding::ZENOH_STRING;
            let timestamp = Timestamp::new(NTP64(100 + i), zenoh::time::TimestampId::rand());

            storage_plugin
                .put(Some(key), payload, encoding, timestamp)
                .await
                .unwrap();
        }

        // Get all entries
        let result = storage_plugin.get_all_entries().await;
        assert!(result.is_ok());

        let entries = result.unwrap();
        assert_eq!(entries.len(), 3);
    }
}
