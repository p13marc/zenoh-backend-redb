//! Backend implementation for the zenoh-backend-redb storage backend.

use crate::config::{RedbBackendConfig, RedbStorageConfig};
use crate::error::{RedbBackendError, Result};
use crate::storage::RedbStorage;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

/// The redb backend manages multiple storage instances.
pub struct RedbBackend {
    /// Backend configuration
    config: RedbBackendConfig,

    /// Map of storage name to storage instance
    storages: Arc<RwLock<HashMap<String, Arc<RedbStorage>>>>,
}

impl RedbBackend {
    /// Create a new redb backend with the given configuration.
    pub fn new(config: RedbBackendConfig) -> Result<Self> {
        info!("Creating redb backend with base dir: {:?}", config.base_dir);

        // Create base directory if needed
        if config.create_dir {
            std::fs::create_dir_all(&config.base_dir)?;
            debug!("Created base directory: {:?}", config.base_dir);
        }

        Ok(Self {
            config,
            storages: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Create a new storage instance.
    pub fn create_storage(
        &self,
        name: String,
        config: Option<RedbStorageConfig>,
    ) -> Result<Arc<RedbStorage>> {
        info!("Creating storage: {}", name);

        // Use provided config or default from backend config
        let storage_config = config.unwrap_or_else(|| self.config.default_storage_config.clone());

        // Determine the database path
        let db_path = storage_config.effective_db_path(&name, &self.config);

        debug!("Storage '{}' will use database at: {:?}", name, db_path);

        // Create the storage
        let storage = RedbStorage::new(db_path, name.clone(), storage_config)?;
        let storage_arc = Arc::new(storage);

        // Register the storage
        {
            let mut storages = self.storages.write().map_err(|e| {
                RedbBackendError::other(format!("Failed to acquire write lock: {}", e))
            })?;

            if storages.contains_key(&name) {
                warn!("Storage '{}' already exists, replacing it", name);
            }

            storages.insert(name.clone(), storage_arc.clone());
        }

        info!("Storage '{}' created successfully", name);
        Ok(storage_arc)
    }

    /// Get an existing storage instance by name.
    pub fn get_storage(&self, name: &str) -> Result<Arc<RedbStorage>> {
        let storages = self
            .storages
            .read()
            .map_err(|e| RedbBackendError::other(format!("Failed to acquire read lock: {}", e)))?;

        storages
            .get(name)
            .cloned()
            .ok_or_else(|| RedbBackendError::storage_not_found(name))
    }

    /// Remove a storage instance.
    pub fn remove_storage(&self, name: &str) -> Result<()> {
        info!("Removing storage: {}", name);

        let mut storages = self
            .storages
            .write()
            .map_err(|e| RedbBackendError::other(format!("Failed to acquire write lock: {}", e)))?;

        if storages.remove(name).is_some() {
            info!("Storage '{}' removed successfully", name);
            Ok(())
        } else {
            warn!("Storage '{}' not found", name);
            Err(RedbBackendError::storage_not_found(name))
        }
    }

    /// List all storage names.
    pub fn list_storages(&self) -> Result<Vec<String>> {
        let storages = self
            .storages
            .read()
            .map_err(|e| RedbBackendError::other(format!("Failed to acquire read lock: {}", e)))?;

        Ok(storages.keys().cloned().collect())
    }

    /// Get the backend configuration.
    pub fn config(&self) -> &RedbBackendConfig {
        &self.config
    }

    /// Get the number of storages managed by this backend.
    pub fn storage_count(&self) -> Result<usize> {
        let storages = self
            .storages
            .read()
            .map_err(|e| RedbBackendError::other(format!("Failed to acquire read lock: {}", e)))?;

        Ok(storages.len())
    }

    /// Check if a storage exists.
    pub fn has_storage(&self, name: &str) -> Result<bool> {
        let storages = self
            .storages
            .read()
            .map_err(|e| RedbBackendError::other(format!("Failed to acquire read lock: {}", e)))?;

        Ok(storages.contains_key(name))
    }

    /// Close all storages and clean up resources.
    pub fn close(&self) -> Result<()> {
        info!("Closing redb backend");

        let mut storages = self
            .storages
            .write()
            .map_err(|e| RedbBackendError::other(format!("Failed to acquire write lock: {}", e)))?;

        let count = storages.len();
        storages.clear();

        info!("Closed {} storage(s)", count);
        Ok(())
    }
}

impl Drop for RedbBackend {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            warn!("Error closing backend during drop: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_backend() -> (RedbBackend, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config = RedbBackendConfig::new().with_base_dir(temp_dir.path().to_path_buf());
        let backend = RedbBackend::new(config).unwrap();
        (backend, temp_dir)
    }

    #[test]
    fn test_backend_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config = RedbBackendConfig::new().with_base_dir(temp_dir.path().to_path_buf());
        let backend = RedbBackend::new(config);
        assert!(backend.is_ok());
    }

    #[test]
    fn test_create_storage() {
        let (backend, _temp) = create_test_backend();

        let storage = backend.create_storage("test_storage".to_string(), None);
        assert!(storage.is_ok());

        let storage = storage.unwrap();
        assert_eq!(storage.name(), "test_storage");
    }

    #[test]
    fn test_get_storage() {
        let (backend, _temp) = create_test_backend();

        backend
            .create_storage("test_storage".to_string(), None)
            .unwrap();

        let storage = backend.get_storage("test_storage");
        assert!(storage.is_ok());
    }

    #[test]
    fn test_get_nonexistent_storage() {
        let (backend, _temp) = create_test_backend();

        let storage = backend.get_storage("nonexistent");
        assert!(storage.is_err());
    }

    #[test]
    fn test_remove_storage() {
        let (backend, _temp) = create_test_backend();

        backend
            .create_storage("test_storage".to_string(), None)
            .unwrap();

        let result = backend.remove_storage("test_storage");
        assert!(result.is_ok());

        let storage = backend.get_storage("test_storage");
        assert!(storage.is_err());
    }

    #[test]
    fn test_list_storages() {
        let (backend, _temp) = create_test_backend();

        backend
            .create_storage("storage1".to_string(), None)
            .unwrap();
        backend
            .create_storage("storage2".to_string(), None)
            .unwrap();
        backend
            .create_storage("storage3".to_string(), None)
            .unwrap();

        let storages = backend.list_storages().unwrap();
        assert_eq!(storages.len(), 3);
        assert!(storages.contains(&"storage1".to_string()));
        assert!(storages.contains(&"storage2".to_string()));
        assert!(storages.contains(&"storage3".to_string()));
    }

    #[test]
    fn test_storage_count() {
        let (backend, _temp) = create_test_backend();

        assert_eq!(backend.storage_count().unwrap(), 0);

        backend
            .create_storage("storage1".to_string(), None)
            .unwrap();
        assert_eq!(backend.storage_count().unwrap(), 1);

        backend
            .create_storage("storage2".to_string(), None)
            .unwrap();
        assert_eq!(backend.storage_count().unwrap(), 2);

        backend.remove_storage("storage1").unwrap();
        assert_eq!(backend.storage_count().unwrap(), 1);
    }

    #[test]
    fn test_has_storage() {
        let (backend, _temp) = create_test_backend();

        assert!(!backend.has_storage("test_storage").unwrap());

        backend
            .create_storage("test_storage".to_string(), None)
            .unwrap();

        assert!(backend.has_storage("test_storage").unwrap());
    }

    #[test]
    fn test_custom_storage_config() {
        let (backend, _temp) = create_test_backend();

        let custom_config = RedbStorageConfig::new()
            .with_key_expr("demo/**".to_string())
            .with_strip_prefix(true);

        let storage = backend
            .create_storage("custom_storage".to_string(), Some(custom_config))
            .unwrap();

        assert_eq!(storage.config().key_expr, Some("demo/**".to_string()));
        assert!(storage.config().strip_prefix);
    }

    #[test]
    fn test_close() {
        let (backend, _temp) = create_test_backend();

        backend
            .create_storage("storage1".to_string(), None)
            .unwrap();
        backend
            .create_storage("storage2".to_string(), None)
            .unwrap();

        assert_eq!(backend.storage_count().unwrap(), 2);

        backend.close().unwrap();
        assert_eq!(backend.storage_count().unwrap(), 0);
    }
}
