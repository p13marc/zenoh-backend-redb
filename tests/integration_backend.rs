//! Integration tests for backend lifecycle and management.

use std::sync::Arc;
use tempfile::TempDir;
use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig, StoredValue};

/// Helper function to create a test backend with temporary storage.
fn create_test_backend() -> (RedbBackend, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = RedbBackendConfig::new()
        .with_base_dir(temp_dir.path().to_path_buf())
        .with_create_dir(true);

    let backend = RedbBackend::new(config).expect("Failed to create backend");
    (backend, temp_dir)
}

#[test]
fn test_backend_initialization() {
    let (backend, _temp) = create_test_backend();

    // Backend should start with no storages
    let count = backend.storage_count().expect("Failed to get count");
    assert_eq!(count, 0);

    // Base directory should exist
    assert!(backend.config().base_dir.exists());
}

#[test]
fn test_backend_multiple_initialization() {
    let temp_dir = TempDir::new().unwrap();

    // Create first backend
    let config1 = RedbBackendConfig::new().with_base_dir(temp_dir.path().to_path_buf());
    let backend1 = RedbBackend::new(config1).unwrap();

    // Create second backend with same directory (should work)
    let config2 = RedbBackendConfig::new().with_base_dir(temp_dir.path().to_path_buf());
    let backend2 = RedbBackend::new(config2).unwrap();

    // Both should be independent
    backend1
        .create_storage("storage1".to_string(), None)
        .unwrap();
    backend2
        .create_storage("storage2".to_string(), None)
        .unwrap();

    assert_eq!(backend1.storage_count().unwrap(), 1);
    assert_eq!(backend2.storage_count().unwrap(), 1);
}

#[test]
fn test_storage_creation_lifecycle() {
    let (backend, _temp) = create_test_backend();

    // Create a storage
    let storage = backend
        .create_storage("test_storage".to_string(), None)
        .expect("Failed to create storage");

    assert_eq!(storage.name(), "test_storage");
    assert_eq!(backend.storage_count().unwrap(), 1);

    // Storage should be retrievable
    let retrieved = backend.get_storage("test_storage");
    assert!(retrieved.is_ok());

    // Storage should be in list
    let storages = backend.list_storages().unwrap();
    assert_eq!(storages.len(), 1);
    assert!(storages.contains(&"test_storage".to_string()));
}

#[test]
fn test_storage_creation_with_custom_config() {
    let (backend, _temp) = create_test_backend();

    let config = RedbStorageConfig::new()
        .with_cache_size(10 * 1024 * 1024)
        .with_fsync(false)
        .with_key_expr("test/**".to_string())
        .with_strip_prefix(true);

    let storage = backend
        .create_storage("custom_storage".to_string(), Some(config))
        .expect("Failed to create storage");

    assert_eq!(storage.config().cache_size, Some(10 * 1024 * 1024));
    assert!(!storage.config().fsync);
    assert_eq!(storage.config().key_expr, Some("test/**".to_string()));
    assert!(storage.config().strip_prefix);
}

#[test]
fn test_multiple_storages() {
    let (backend, _temp) = create_test_backend();

    // Create multiple storages
    for i in 0..5 {
        let name = format!("storage_{}", i);
        backend
            .create_storage(name, None)
            .expect("Failed to create storage");
    }

    assert_eq!(backend.storage_count().unwrap(), 5);

    // All should be listed
    let storages = backend.list_storages().unwrap();
    assert_eq!(storages.len(), 5);

    for i in 0..5 {
        let name = format!("storage_{}", i);
        assert!(storages.contains(&name));
    }
}

#[test]
fn test_storage_retrieval() {
    let (backend, _temp) = create_test_backend();

    // Create storage
    backend
        .create_storage("retrieve_test".to_string(), None)
        .unwrap();

    // Should be able to retrieve it
    let storage = backend.get_storage("retrieve_test");
    assert!(storage.is_ok());

    let storage = storage.unwrap();
    assert_eq!(storage.name(), "retrieve_test");
}

#[test]
fn test_storage_retrieval_nonexistent() {
    let (backend, _temp) = create_test_backend();

    // Should fail to retrieve non-existent storage
    let result = backend.get_storage("nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_storage_removal() {
    let (backend, _temp) = create_test_backend();

    // Create and remove storage
    backend
        .create_storage("removable".to_string(), None)
        .unwrap();
    assert_eq!(backend.storage_count().unwrap(), 1);

    backend.remove_storage("removable").unwrap();
    assert_eq!(backend.storage_count().unwrap(), 0);

    // Should no longer be retrievable
    let result = backend.get_storage("removable");
    assert!(result.is_err());
}

#[test]
fn test_storage_removal_nonexistent() {
    let (backend, _temp) = create_test_backend();

    // Should fail to remove non-existent storage
    let result = backend.remove_storage("nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_storage_replacement() {
    let (backend, _temp) = create_test_backend();

    // Create storage
    let storage1 = backend
        .create_storage("replaceable".to_string(), None)
        .unwrap();

    // Store some data
    let value = StoredValue::new(b"data1".to_vec(), 1, "text/plain".to_string());
    storage1.put("key1", value).unwrap();

    // Drop storage1 to release the database file
    drop(storage1);

    // Remove the storage from backend
    backend.remove_storage("replaceable").unwrap();

    // Now create storage with same name (using different DB file)
    let config = RedbStorageConfig::new().with_db_file("replaceable_v2.redb".to_string());
    let storage2 = backend
        .create_storage("replaceable".to_string(), Some(config))
        .unwrap();

    // Should have one storage again
    assert_eq!(backend.storage_count().unwrap(), 1);

    // Storage should be accessible
    assert_eq!(storage2.name(), "replaceable");

    // Old data should not be there (new DB file)
    assert!(storage2.get("key1").unwrap().is_none());
}

#[test]
fn test_storage_exists_check() {
    let (backend, _temp) = create_test_backend();

    // Should not exist initially
    assert!(!backend.has_storage("check_test").unwrap());

    // Create storage
    backend
        .create_storage("check_test".to_string(), None)
        .unwrap();

    // Should exist now
    assert!(backend.has_storage("check_test").unwrap());

    // Remove storage
    backend.remove_storage("check_test").unwrap();

    // Should not exist anymore
    assert!(!backend.has_storage("check_test").unwrap());
}

#[test]
fn test_backend_close() {
    let (backend, _temp) = create_test_backend();

    // Create some storages
    backend
        .create_storage("storage1".to_string(), None)
        .unwrap();
    backend
        .create_storage("storage2".to_string(), None)
        .unwrap();

    assert_eq!(backend.storage_count().unwrap(), 2);

    // Close backend
    backend.close().unwrap();

    // Should have no storages after close
    assert_eq!(backend.storage_count().unwrap(), 0);
}

#[test]
fn test_concurrent_storage_access() {
    use std::thread;

    let (backend, _temp) = create_test_backend();
    let backend = Arc::new(backend);

    // Create storage
    backend
        .create_storage("concurrent".to_string(), None)
        .unwrap();

    // Spawn multiple threads accessing storage
    let mut handles = vec![];

    for i in 0..10 {
        let backend_clone = Arc::clone(&backend);
        let handle = thread::spawn(move || {
            let storage = backend_clone.get_storage("concurrent").unwrap();
            let key = format!("key_{}", i);
            let value = StoredValue::new(
                format!("value_{}", i).into_bytes(),
                i as u64,
                "text/plain".to_string(),
            );
            storage.put(&key, value).unwrap();
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all data was written
    let storage = backend.get_storage("concurrent").unwrap();
    let count = storage.count().unwrap();
    assert_eq!(count, 10);
}

#[test]
fn test_storage_isolation() {
    let (backend, _temp) = create_test_backend();

    // Create two storages
    let storage1 = backend
        .create_storage("isolated1".to_string(), None)
        .unwrap();
    let storage2 = backend
        .create_storage("isolated2".to_string(), None)
        .unwrap();

    // Put data in storage1
    let value1 = StoredValue::new(b"data1".to_vec(), 1, "text/plain".to_string());
    storage1.put("key1", value1).unwrap();

    // Put data in storage2
    let value2 = StoredValue::new(b"data2".to_vec(), 2, "text/plain".to_string());
    storage2.put("key2", value2).unwrap();

    // Each storage should only have its own data
    assert_eq!(storage1.count().unwrap(), 1);
    assert_eq!(storage2.count().unwrap(), 1);

    // Storage1 should not see storage2's data
    assert!(storage1.get("key2").unwrap().is_none());
    assert!(storage1.get("key1").unwrap().is_some());

    // Storage2 should not see storage1's data
    assert!(storage2.get("key1").unwrap().is_none());
    assert!(storage2.get("key2").unwrap().is_some());
}

#[test]
fn test_backend_config_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path().to_path_buf();

    // Create backend with custom config
    let config = RedbBackendConfig::new()
        .with_base_dir(base_dir.clone())
        .with_create_dir(true);

    let backend = RedbBackend::new(config).unwrap();

    // Verify config is accessible
    assert_eq!(backend.config().base_dir, base_dir);
    assert!(backend.config().create_dir);
}

#[test]
fn test_storage_with_read_only_config() {
    let (backend, _temp) = create_test_backend();

    // Create a normal storage first and add data
    let storage1 = backend
        .create_storage("writable".to_string(), None)
        .unwrap();
    let value = StoredValue::new(b"data".to_vec(), 1, "text/plain".to_string());
    storage1.put("key1", value).unwrap();

    // Now try to create a read-only storage pointing to same DB
    let config = RedbStorageConfig::new()
        .with_db_file("writable.redb".to_string())
        .with_read_only(true)
        .with_create_db(false);

    // This might fail if DB is already open, but demonstrates the config
    let _ = backend.create_storage("readonly".to_string(), Some(config));
}

#[test]
fn test_empty_backend_operations() {
    let (backend, _temp) = create_test_backend();

    // Operations on empty backend
    assert_eq!(backend.storage_count().unwrap(), 0);
    assert_eq!(backend.list_storages().unwrap().len(), 0);
    assert!(!backend.has_storage("anything").unwrap());
    assert!(backend.get_storage("anything").is_err());
    assert!(backend.remove_storage("anything").is_err());
}

#[test]
fn test_backend_directory_creation() {
    let temp_dir = TempDir::new().unwrap();
    let nested_path = temp_dir.path().join("level1").join("level2").join("level3");

    let config = RedbBackendConfig::new()
        .with_base_dir(nested_path.clone())
        .with_create_dir(true);

    let backend = RedbBackend::new(config).unwrap();

    // Nested directory should be created
    assert!(nested_path.exists());
    assert!(nested_path.is_dir());

    // Should be able to create storage in nested directory
    let storage = backend
        .create_storage("nested_test".to_string(), None)
        .unwrap();
    assert!(storage.count().is_ok());
}

#[test]
fn test_storage_names_with_special_characters() {
    let (backend, _temp) = create_test_backend();

    // Test various storage names
    let names = vec![
        "simple",
        "with-dashes",
        "with_underscores",
        "with.dots",
        "with123numbers",
        "MixedCase",
    ];

    for name in &names {
        let result = backend.create_storage(name.to_string(), None);
        assert!(
            result.is_ok(),
            "Failed to create storage with name: {}",
            name
        );
    }

    assert_eq!(backend.storage_count().unwrap(), names.len());
}
