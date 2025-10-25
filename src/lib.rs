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
//! - Support for Zenoh wildcard queries (`*` and `**`)
//! - Configurable per-storage settings
//! - Read-only mode support
//! - Prefix stripping for efficient key storage
//!
//! # Example
//!
//! ```rust,no_run
//! use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig};
//!
//! // Create a backend with custom configuration
//! let config = RedbBackendConfig::new()
//!     .with_base_dir("./my_databases".into())
//!     .with_create_dir(true);
//!
//! let backend = RedbBackend::new(config).unwrap();
//!
//! // Create a storage instance
//! let storage_config = RedbStorageConfig::new()
//!     .with_key_expr("demo/**".to_string())
//!     .with_strip_prefix(false);
//!
//! let storage = backend.create_storage(
//!     "my_storage".to_string(),
//!     Some(storage_config)
//! ).unwrap();
//! ```

// Module declarations
pub mod backend;
pub mod config;
pub mod error;
pub mod storage;

#[cfg(feature = "plugin")]
pub mod plugin;

// Re-export main types for convenience
pub use backend::RedbBackend;
pub use config::{RedbBackendConfig, RedbStorageConfig};
pub use error::{RedbBackendError, Result};
pub use storage::{RedbStorage, StoredValue};

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

    #[test]
    fn test_basic_workflow() {
        // Create a temporary directory for the test
        let temp_dir = TempDir::new().unwrap();

        // Create backend
        let config = RedbBackendConfig::new().with_base_dir(temp_dir.path().to_path_buf());
        let backend = RedbBackend::new(config).unwrap();

        // Create storage
        let storage = backend
            .create_storage("test_storage".to_string(), None)
            .unwrap();

        // Store a value
        let value = StoredValue::new(b"hello world".to_vec(), 12345, "text/plain".to_string());
        storage.put("test/key", value.clone()).unwrap();

        // Retrieve the value
        let retrieved = storage.get("test/key").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().payload, value.payload);

        // Delete the value
        let deleted = storage.delete("test/key").unwrap();
        assert!(deleted);

        // Verify it's gone
        let retrieved = storage.get("test/key").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_backend_name() {
        assert_eq!(BACKEND_NAME, "redb");
    }
}
