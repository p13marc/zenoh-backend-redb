//! Configuration structures for the zenoh-backend-redb storage backend.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the redb backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedbBackendConfig {
    /// Base directory for storing databases.
    /// If not specified, defaults to "./zenoh_redb_backend"
    #[serde(default = "default_base_dir")]
    pub base_dir: PathBuf,

    /// Whether to create the directory if it doesn't exist.
    #[serde(default = "default_true")]
    pub create_dir: bool,

    /// Default configuration for storages (can be overridden per storage).
    #[serde(default)]
    pub default_storage_config: RedbStorageConfig,
}

/// Configuration for a single redb storage instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedbStorageConfig {
    /// Database file name. If not specified, uses the storage name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_file: Option<String>,

    /// Full path to the database file. Overrides base_dir and db_file if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_path: Option<PathBuf>,

    /// Page-cache budget in bytes for redb.
    ///
    /// Defaults to [`DEFAULT_CACHE_SIZE`] (64 MiB) and is **always** set explicitly —
    /// this backend never inherits redb's own default, which is 1 GiB. On a small
    /// guest an unbounded cache reads as a slow multi-day RSS climb ending at the OOM
    /// killer, with nothing in the logs to connect it to a storage setting.
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,

    /// Whether to enable fsync for durability.
    /// Default is true for data safety.
    #[serde(default = "default_true")]
    pub fsync: bool,

    /// Key expression prefix for this storage.
    /// Only keys matching this prefix will be stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_expr: Option<String>,

    /// Whether to strip the key_expr prefix from stored keys.
    /// Default is false (store full key).
    #[serde(default)]
    pub strip_prefix: bool,

    /// Table name within the database.
    /// Default is "zenoh_kv"
    #[serde(default = "default_table_name")]
    pub table_name: String,

    /// Whether to create the database if it doesn't exist.
    #[serde(default = "default_true")]
    pub create_db: bool,

    /// Read-only mode. If true, the storage will not accept writes.
    #[serde(default)]
    pub read_only: bool,
}

impl Default for RedbBackendConfig {
    fn default() -> Self {
        Self {
            base_dir: default_base_dir(),
            create_dir: true,
            default_storage_config: RedbStorageConfig::default(),
        }
    }
}

impl RedbBackendConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the base directory for databases.
    pub fn with_base_dir(mut self, base_dir: PathBuf) -> Self {
        self.base_dir = base_dir;
        self
    }

    /// Set whether to create the directory if it doesn't exist.
    pub fn with_create_dir(mut self, create_dir: bool) -> Self {
        self.create_dir = create_dir;
        self
    }

    /// Set the default storage configuration.
    pub fn with_default_storage_config(mut self, config: RedbStorageConfig) -> Self {
        self.default_storage_config = config;
        self
    }
}

impl RedbStorageConfig {
    /// Create a new storage configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the database file name.
    pub fn with_db_file(mut self, db_file: String) -> Self {
        self.db_file = Some(db_file);
        self
    }

    /// Set the full database path.
    pub fn with_db_path(mut self, db_path: PathBuf) -> Self {
        self.db_path = Some(db_path);
        self
    }

    /// Set the cache size in bytes.
    pub fn with_cache_size(mut self, cache_size: usize) -> Self {
        self.cache_size = cache_size;
        self
    }

    /// Set whether to enable fsync.
    pub fn with_fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }

    /// Set the key expression prefix.
    pub fn with_key_expr(mut self, key_expr: String) -> Self {
        self.key_expr = Some(key_expr);
        self
    }

    /// Set whether to strip the prefix from stored keys.
    pub fn with_strip_prefix(mut self, strip_prefix: bool) -> Self {
        self.strip_prefix = strip_prefix;
        self
    }

    /// Set the table name.
    pub fn with_table_name(mut self, table_name: String) -> Self {
        self.table_name = table_name;
        self
    }

    /// Set whether to create the database if it doesn't exist.
    pub fn with_create_db(mut self, create_db: bool) -> Self {
        self.create_db = create_db;
        self
    }

    /// Set read-only mode.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Get the effective database path for a given storage name and backend config.
    pub fn effective_db_path(
        &self,
        storage_name: &str,
        backend_config: &RedbBackendConfig,
    ) -> PathBuf {
        if let Some(ref path) = self.db_path {
            // Explicit path takes precedence
            path.clone()
        } else {
            // Construct path from base_dir and db_file (or storage name)
            let filename = self.db_file.as_deref().unwrap_or(storage_name).to_string() + ".redb";
            backend_config.base_dir.join(filename)
        }
    }
}

/// Default redb page-cache budget: 64 MiB.
///
/// Deliberately not redb's default (1 GiB). See [`RedbStorageConfig::cache_size`].
pub const DEFAULT_CACHE_SIZE: usize = 64 * 1024 * 1024;

/// `Default` is written out rather than derived so that it cannot drift from the
/// serde defaults above. A derived `Default` gave `fsync: false`, `create_db: false`
/// and an empty `table_name`, none of which matches what parsing the same config
/// from JSON produces.
impl Default for RedbStorageConfig {
    fn default() -> Self {
        Self {
            db_file: None,
            db_path: None,
            cache_size: default_cache_size(),
            fsync: default_true(),
            key_expr: None,
            strip_prefix: false,
            table_name: default_table_name(),
            create_db: true,
            read_only: false,
        }
    }
}

// Default value functions for serde
fn default_cache_size() -> usize {
    DEFAULT_CACHE_SIZE
}

fn default_base_dir() -> PathBuf {
    PathBuf::from("./zenoh_redb_backend")
}

fn default_true() -> bool {
    true
}

fn default_table_name() -> String {
    "zenoh_kv".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache budget must never fall back to redb's own default (1 GiB in redb 4).
    /// An unbounded page cache on a small guest is a slow RSS climb that ends at the
    /// OOM killer, and nothing in the logs points back at a storage setting.
    #[test]
    fn cache_size_defaults_to_64_mib_not_redbs_default() {
        assert_eq!(DEFAULT_CACHE_SIZE, 64 * 1024 * 1024);
        assert_eq!(RedbStorageConfig::default().cache_size, DEFAULT_CACHE_SIZE);

        // ...and a config parsed from JSON that omits the field agrees.
        let parsed: RedbStorageConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.cache_size, DEFAULT_CACHE_SIZE);
    }

    /// `Default` is hand-written precisely so it cannot drift from the serde
    /// defaults; a derived one gave `fsync: false` and `create_db: false`.
    #[test]
    fn derived_and_parsed_defaults_agree() {
        let parsed: RedbStorageConfig = serde_json::from_str("{}").unwrap();
        let default = RedbStorageConfig::default();

        assert_eq!(parsed.fsync, default.fsync);
        assert_eq!(parsed.create_db, default.create_db);
        assert_eq!(parsed.table_name, default.table_name);
        assert_eq!(parsed.read_only, default.read_only);
        assert!(default.fsync, "fsync must default to on");
        assert!(default.create_db, "create_db must default to on");
    }

    #[test]
    fn test_default_config() {
        let config = RedbBackendConfig::default();
        assert_eq!(config.base_dir, PathBuf::from("./zenoh_redb_backend"));
        assert!(config.create_dir);
    }

    #[test]
    fn test_storage_config_builder() {
        let config = RedbStorageConfig::new()
            .with_db_file("test.redb".to_string())
            .with_cache_size(1024 * 1024)
            .with_fsync(false);

        assert_eq!(config.db_file, Some("test.redb".to_string()));
        assert_eq!(config.cache_size, 1024 * 1024);
        assert!(!config.fsync);
    }

    #[test]
    fn test_effective_db_path() {
        let backend_config = RedbBackendConfig::default();

        // Test with explicit path
        let storage_config = RedbStorageConfig::new().with_db_path(PathBuf::from("/tmp/test.redb"));
        assert_eq!(
            storage_config.effective_db_path("storage1", &backend_config),
            PathBuf::from("/tmp/test.redb")
        );

        // Test with db_file
        let storage_config = RedbStorageConfig::new().with_db_file("custom.redb".to_string());
        assert_eq!(
            storage_config.effective_db_path("storage1", &backend_config),
            PathBuf::from("./zenoh_redb_backend/custom.redb.redb")
        );

        // Test with storage name as default
        let storage_config = RedbStorageConfig::new();
        assert_eq!(
            storage_config.effective_db_path("mystorage", &backend_config),
            PathBuf::from("./zenoh_redb_backend/mystorage.redb")
        );
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = RedbStorageConfig::new()
            .with_cache_size(1024)
            .with_key_expr("demo/**".to_string());

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: RedbStorageConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.cache_size, deserialized.cache_size);
        assert_eq!(config.key_expr, deserialized.key_expr);
    }
}
