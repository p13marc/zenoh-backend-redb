//! Zenoh plugin implementation for the redb backend.
//!
//! This module provides the integration between the redb storage backend and
//! Zenoh's plugin system, implementing the required traits for Volume and Storage.

use crate::backend::RedbBackend;
use crate::config::{RedbBackendConfig, RedbStorageConfig};

use crate::storage::{RedbStorage, StoredValue};
use async_trait::async_trait;
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
        let redb_storage = RedbStorage::new(&db_path, storage_name.clone(), storage_config.clone())
            .map_err(|e| zerror!("Failed to create redb storage: {}", e))?;

        info!("Created redb storage '{}' at {:?}", storage_name, db_path);

        Ok(Box::new(RedbStoragePlugin {
            config,
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
            storage_config,
        }))
    }
}

/// Storage implementation for redb backend.
struct RedbStoragePlugin {
    config: StorageConfig,
    storage: Arc<tokio::sync::Mutex<RedbStorage>>,
    storage_config: RedbStorageConfig,
}

#[async_trait]
impl Storage for RedbStoragePlugin {
    fn get_admin_status(&self) -> JsonValue {
        self.config.to_json_value().into()
    }

    async fn put(
        &mut self,
        key: Option<OwnedKeyExpr>,
        payload: ZBytes,
        encoding: Encoding,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let storage = self.storage.lock().await;

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

        // Convert Zenoh timestamp to Unix timestamp (seconds)
        let unix_timestamp = timestamp.get_time().as_u64();

        // Convert encoding to string
        let encoding_str = encoding.to_string();

        // Create stored value
        let value = StoredValue::new(payload_bytes, unix_timestamp, encoding_str);

        // Store in database
        storage
            .put(&key_str, value)
            .map_err(|e| zerror!("Failed to put key '{}': {}", key_str, e))?;

        Ok(StorageInsertionResult::Inserted)
    }

    async fn delete(
        &mut self,
        key: Option<OwnedKeyExpr>,
        _timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let storage = self.storage.lock().await;

        if self.storage_config.read_only {
            warn!("Received DELETE for read-only DB on {:?} - ignored", key);
            return Err("Received update for read-only DB".into());
        }

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Deleting key: {}", key_str);

        storage
            .delete(&key_str)
            .map_err(|e| zerror!("Failed to delete key '{}': {}", key_str, e))?;

        // Always return Deleted, even if key wasn't found
        Ok(StorageInsertionResult::Deleted)
    }

    async fn get(
        &mut self,
        key: Option<OwnedKeyExpr>,
        _parameters: &str,
    ) -> ZResult<Vec<StoredData>> {
        let storage = self.storage.lock().await;

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
                let encoding = Encoding::from(stored_value.encoding);

                // Convert Unix timestamp back to Zenoh Timestamp
                let timestamp = Timestamp::new(
                    zenoh::time::NTP64(stored_value.timestamp),
                    zenoh::time::TimestampId::rand(),
                );

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
        let storage = self.storage.lock().await;

        debug!("Getting all entries");

        let entries = storage
            .get_all()
            .map_err(|e| zerror!("Failed to get all entries: {}", e))?;

        let mut result = Vec::new();
        for (key_str, stored_value) in entries {
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

            // Convert Unix timestamp to Zenoh Timestamp
            let timestamp = Timestamp::new(
                zenoh::time::NTP64(stored_value.timestamp),
                zenoh::time::TimestampId::rand(),
            );

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
            RedbStorage::new(&db_path, "test".to_string(), storage_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
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
            RedbStorage::new(&db_path, "test".to_string(), storage_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
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
            RedbStorage::new(&db_path, "test".to_string(), storage_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
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

    #[tokio::test]
    async fn test_storage_put_none_key() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(true);

        let redb_storage =
            RedbStorage::new(&db_path, "test".to_string(), storage_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
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
            RedbStorage::new(&db_path, "test".to_string(), storage_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
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

        let redb_storage = RedbStorage::new(&db_path, "test".to_string(), storage_config).unwrap();

        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        // Put some data first
        redb_storage
            .put(
                &key.to_string(),
                StoredValue::new(
                    payload.to_bytes().to_vec(),
                    timestamp.get_time().as_u64(),
                    encoding.to_string(),
                ),
            )
            .unwrap();

        drop(redb_storage);

        // Now open as read-only
        let ro_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(false)
            .with_read_only(true);

        let ro_storage = RedbStorage::new(&db_path, "test".to_string(), ro_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(ro_storage)),
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

        let redb_storage = RedbStorage::new(&db_path, "test".to_string(), storage_config).unwrap();

        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        // Put some data first
        redb_storage
            .put(
                &key.to_string(),
                StoredValue::new(
                    payload.to_bytes().to_vec(),
                    timestamp.get_time().as_u64(),
                    encoding.to_string(),
                ),
            )
            .unwrap();

        drop(redb_storage);

        // Now open as read-only
        let ro_config = RedbStorageConfig::new()
            .with_db_path(db_path.clone())
            .with_create_db(false)
            .with_read_only(true);

        let ro_storage = RedbStorage::new(&db_path, "test".to_string(), ro_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(ro_storage)),
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
            RedbStorage::new(&db_path, "test".to_string(), storage_config.clone()).unwrap();

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
            storage: Arc::new(tokio::sync::Mutex::new(redb_storage)),
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
