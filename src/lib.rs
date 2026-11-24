//! Zenoh storage backend using redb embedded database.
//!
//! This crate provides a storage backend for Zenoh that uses redb as the underlying
//! database engine. redb is a pure Rust, ACID-compliant embedded key-value store
//! with zero-copy reads and MVCC support.
//!
//! # Features
//!
//! - Pure Rust implementation with no C dependencies
//! - ACID compliance with MVCC (Multi-Version Concurrency Control)
//! - Zero-copy reads for excellent performance
//! - Zenoh's own key-expression matching for wildcard queries (`*`, `**`, `$*`)
//! - Optional full history per key, with `_time`-ranged reads and retention
//! - Configurable per-storage settings
//! - Read-only mode support
//!
//! # Example
//!
//! The plugin is the usual way in, but the engine can be used directly:
//!
//! ```rust,no_run
//! use zenoh::bytes::Encoding;
//! use zenoh::time::{NTP64, Timestamp, TimestampId};
//! use zenoh_backend_redb::{RedbStorage, RedbStorageConfig, StoredValue};
//!
//! let config = RedbStorageConfig::new().with_cache_size(32 * 1024 * 1024);
//! let storage = RedbStorage::open("./demo.redb", config, "demo").unwrap();
//!
//! let timestamp = Timestamp::new(NTP64(1), TimestampId::rand());
//! storage
//!     .put(
//!         "demo/key",
//!         StoredValue::new(b"hello".to_vec(), timestamp, Encoding::ZENOH_BYTES),
//!     )
//!     .unwrap();
//! assert!(storage.get("demo/key").unwrap().is_some());
//! ```

// Module declarations
pub mod config;
pub mod error;
pub mod storage;

#[cfg(feature = "plugin")]
pub mod plugin;

// Re-export main types for convenience
pub use config::{DecimationPolicy, HistoryMode, RedbStorageConfig, RetentionPolicy};
pub use error::{RedbBackendError, Result};
pub use storage::{
    RedbStorage, RetentionPass, StorageStats, StorageUsage, StoredValue, WriteOutcome,
};

#[cfg(feature = "plugin")]
pub use plugin::{DEFAULT_ROOT_DIR, RedbBackendPlugin, RedbVolume, SCOPE_ENV_VAR};

/// Version of the zenoh-backend-redb crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Name of the backend.
pub const BACKEND_NAME: &str = "redb";

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zenoh::bytes::Encoding;
    use zenoh::time::{NTP64, Timestamp, TimestampId};

    #[test]
    fn test_basic_workflow() {
        let temp_dir = TempDir::new().unwrap();
        let storage = RedbStorage::open(
            temp_dir.path().join("test.redb"),
            RedbStorageConfig::new(),
            "test",
        )
        .unwrap();

        // Store a value
        let timestamp = Timestamp::new(NTP64(12345), TimestampId::rand());
        let encoding = Encoding::ZENOH_BYTES;
        let value = StoredValue::new(b"hello world".to_vec(), timestamp, encoding);
        storage.put("test/key", value.clone()).unwrap();

        // Retrieve the value
        let retrieved = storage.get("test/key").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().payload, value.payload);

        // Delete the value
        storage.delete("test/key", timestamp).unwrap();

        // Verify it's gone
        let retrieved = storage.get("test/key").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_backend_name() {
        assert_eq!(BACKEND_NAME, "redb");
    }
}
