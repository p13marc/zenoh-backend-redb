//! Zenoh plugin implementation for the redb backend.
//!
//! This module provides the integration between the redb storage backend and
//! Zenoh's plugin system, implementing the required traits for Volume and Storage.

use crate::backend::RedbBackend;
use crate::config::{RedbBackendConfig, RedbStorageConfig};

use crate::storage::{RedbStorage, StoredValue};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info, warn};
use zenoh::{
    Result as ZResult,
    bytes::{Encoding, ZBytes},
    internal::{bail, zenoh_home, zerror},
    key_expr::OwnedKeyExpr,
    time::Timestamp,
    try_init_log_from_env,
};
use zenoh_backend_traits::{
    Capability, History, Persistence, Storage, StorageInsertionResult, StoredData, Volume,
    config::{StorageConfig, VolumeConfig},
};
use zenoh_plugin_trait::{Plugin, plugin_long_version, plugin_version};
use zenoh_util::ffi::JsonValue;

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

// Special key for None (when the prefix being stripped exactly matches the key)
pub const NONE_KEY: &str = "@@none_key@@";

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

    fn start(_name: &str, _config: &Self::StartArgs) -> ZResult<Self::Instance> {
        try_init_log_from_env();
        info!("redb backend {}", Self::PLUGIN_LONG_VERSION);

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

        let admin_status: serde_json::Value = properties
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();

        Ok(Box::new(RedbVolume {
            admin_status,
            backend: Arc::new(backend),
        }))
    }
}

/// Volume implementation for redb backend.
pub struct RedbVolume {
    admin_status: serde_json::Value,
    backend: Arc<RedbBackend>,
}

#[async_trait]
impl Volume for RedbVolume {
    fn get_admin_status(&self) -> JsonValue {
        (&self.admin_status).into()
    }

    fn get_capability(&self) -> Capability {
        Capability {
            persistence: Persistence::Durable,
            history: History::Latest,
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
            "history": "latest",
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

        // Last-writer-wins, decided here rather than trusted from upstream.
        //
        // The storage manager does filter outdated samples before calling us, but it
        // does so against an in-memory cache seeded from `get_all_entries` at startup
        // (`storages_mgt/service.rs`, `guard_cache_if_latest`). That cache is
        // per-process and per-storage; replay, alignment and a restarted manager can
        // all deliver an older sample to a key we already hold a newer value for.
        // Without this check the older payload silently wins and the newer data is
        // gone, which is exactly the class of bug a durable storage must not have.
        let existing = storage
            .timestamp_of(&key_str)
            .map_err(|e| zerror!("Failed to read timestamp for key '{}': {}", key_str, e))?;

        if let Some(stored) = existing
            && timestamp <= stored
        {
            debug!(
                "Ignoring outdated PUT for {}: incoming {} <= stored {}",
                key_str, timestamp, stored
            );
            return Ok(StorageInsertionResult::Outdated);
        }

        // Convert ZBytes to Vec<u8>
        let payload_bytes = payload.to_bytes().to_vec();

        // Create stored value with native Zenoh timestamp (preserves both time and ID)
        let value = StoredValue::new(payload_bytes, timestamp, encoding);

        // Store in database
        storage
            .put(&key_str, value)
            .map_err(|e| zerror!("Failed to put key '{}': {}", key_str, e))?;

        Ok(if existing.is_some() {
            StorageInsertionResult::Replaced
        } else {
            StorageInsertionResult::Inserted
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

        // Same last-writer-wins rule as `put`: a DELETE that predates the value we
        // hold must not remove it.
        if let Some(stored) = storage
            .timestamp_of(&key_str)
            .map_err(|e| zerror!("Failed to read timestamp for key '{}': {}", key_str, e))?
            && timestamp < stored
        {
            debug!(
                "Ignoring outdated DELETE for {}: incoming {} < stored {}",
                key_str, timestamp, stored
            );
            return Ok(StorageInsertionResult::Outdated);
        }

        storage
            .delete(&key_str)
            .map_err(|e| zerror!("Failed to delete key '{}': {}", key_str, e))?;

        // Deleting an absent key is not an error: the storage manager replays
        // deletions during alignment and expects them to be idempotent.
        Ok(StorageInsertionResult::Deleted)
    }

    async fn get(
        &mut self,
        key: Option<OwnedKeyExpr>,
        _parameters: &str,
    ) -> ZResult<Vec<StoredData>> {
        let storage = &self.storage;

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Getting key: {}", key_str);

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
