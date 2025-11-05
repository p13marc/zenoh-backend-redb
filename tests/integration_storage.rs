//! Integration tests for storage operations and data persistence.

use std::collections::HashSet;
use tempfile::TempDir;
use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig, StoredValue};
use zenoh::bytes::Encoding;
use zenoh::time::{NTP64, Timestamp, TimestampId};


/// Helper function to create a test backend and storage.
fn create_test_storage() -> (RedbBackend, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = RedbBackendConfig::new()
        .with_base_dir(temp_dir.path().to_path_buf())
        .with_create_dir(true);

    let backend = RedbBackend::new(config).expect("Failed to create backend");
    let _storage = backend
        .create_storage("test_storage".to_string(), None)
        .expect("Failed to create storage");

    (backend, temp_dir)
}
/// Helper to create a test value with proper types
fn test_value(payload: Vec<u8>, time: u64) -> StoredValue {
    let timestamp = Timestamp::new(NTP64(time), TimestampId::rand());
    let encoding = Encoding::TEXT_PLAIN;
    StoredValue::new(payload, timestamp, encoding)
}


#[test]
fn test_basic_put_get_delete() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put a value
    let value = test_value(b"test_value".to_vec(), 12345);
    storage.put("test_key", value.clone()).unwrap();

    // Get the value
    let retrieved = storage.get("test_key").unwrap();
    assert!(retrieved.is_some());
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.payload, value.payload);
    assert_eq!(retrieved.timestamp, value.timestamp);
    assert_eq!(retrieved.encoding.id(), value.encoding.id());

    // Delete the value
    storage.delete("test_key").unwrap();
    // deleted

    // Verify it's gone
    let retrieved = storage.get("test_key").unwrap();
    assert!(retrieved.is_none());
}

#[test]
fn test_put_overwrite() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put initial value
    let value1 = test_value(b"value1".to_vec(), 100);
    storage.put("key", value1).unwrap();

    // Overwrite with new value
    let value2 = test_value(b"value2".to_vec(), 200);
    storage.put("key", value2.clone()).unwrap();

    // Should get the new value
    let retrieved = storage.get("key").unwrap().unwrap();
    assert_eq!(retrieved.payload, value2.payload);
    assert_eq!(retrieved.timestamp.get_time().as_u64(), 200);
}

#[test]
fn test_delete_nonexistent() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Delete non-existent key should return false
    storage.delete("nonexistent").unwrap();  // returns ()
    // Delete was called (no error)
}

#[test]
fn test_get_nonexistent() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Get non-existent key should return None
    let result = storage.get("nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_multiple_keys() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put multiple keys
    for i in 0..100 {
        let key = format!("key_{}", i);
        let value = test_value(format!("value_{}", i).into_bytes(), i as u64);
        storage.put(&key, value).unwrap();
    }

    // Verify count
    assert_eq!(storage.count().unwrap(), 100);

    // Verify all keys exist
    for i in 0..100 {
        let key = format!("key_{}", i);
        let retrieved = storage.get(&key).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(
            retrieved.unwrap().payload,
            format!("value_{}", i).into_bytes()
        );
    }
}

#[test]
fn test_get_all() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put some data
    for i in 0..10 {
        let key = format!("key_{}", i);
        let value = test_value(format!("value_{}", i).into_bytes(), i as u64);
        storage.put(&key, value).unwrap();
    }

    // Get all entries
    let all = storage.get_all().unwrap();
    assert_eq!(all.len(), 10);

    // Verify all keys are present
    let keys: HashSet<String> = all.iter().map(|(k, _)| k.clone()).collect();
    for i in 0..10 {
        let key = format!("key_{}", i);
        assert!(keys.contains(&key));
    }
}

#[test]
fn test_get_by_prefix() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put data with different prefixes
    storage
        .put(
            "sensors/temp/1",
            test_value(b"20".to_vec(), 1),
        )
        .unwrap();
    storage
        .put(
            "sensors/temp/2",
            test_value(b"21".to_vec(), 2),
        )
        .unwrap();
    storage
        .put(
            "sensors/humidity/1",
            test_value(b"50".to_vec(), 3),
        )
        .unwrap();
    storage
        .put(
            "config/timeout",
            test_value(b"30".to_vec(), 4),
        )
        .unwrap();

    // Query by prefix
    let temp_results = storage.get_by_prefix("sensors/temp/").unwrap();
    assert_eq!(temp_results.len(), 2);

    let sensor_results = storage.get_by_prefix("sensors/").unwrap();
    assert_eq!(sensor_results.len(), 3);

    let config_results = storage.get_by_prefix("config/").unwrap();
    assert_eq!(config_results.len(), 1);
}

#[test]
fn test_get_by_prefix_no_matches() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    storage
        .put(
            "key1",
            test_value(b"value1".to_vec(), 1),
        )
        .unwrap();

    let results = storage.get_by_prefix("nonexistent/").unwrap();
    assert_eq!(results.len(), 0);
}

#[test]
fn test_wildcard_single_segment() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put data
    storage
        .put(
            "a/b/c",
            test_value(b"1".to_vec(), 1),
        )
        .unwrap();
    storage
        .put(
            "a/x/c",
            test_value(b"2".to_vec(), 2),
        )
        .unwrap();
    storage
        .put(
            "a/y/c",
            test_value(b"3".to_vec(), 3),
        )
        .unwrap();
    storage
        .put(
            "a/b/d",
            test_value(b"4".to_vec(), 4),
        )
        .unwrap();

    // Query with single wildcard
    let results = storage.get_by_wildcard("a/*/c").unwrap();
    assert_eq!(results.len(), 3);

    let keys: HashSet<String> = results.iter().map(|(k, _)| k.clone()).collect();
    assert!(keys.contains("a/b/c"));
    assert!(keys.contains("a/x/c"));
    assert!(keys.contains("a/y/c"));
    assert!(!keys.contains("a/b/d"));
}

#[test]
fn test_wildcard_multi_segment() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put data
    storage
        .put(
            "a/c",
            test_value(b"1".to_vec(), 1),
        )
        .unwrap();
    storage
        .put(
            "a/b/c",
            test_value(b"2".to_vec(), 2),
        )
        .unwrap();
    storage
        .put(
            "a/b/x/c",
            test_value(b"3".to_vec(), 3),
        )
        .unwrap();
    storage
        .put(
            "a/b/x/y/c",
            test_value(b"4".to_vec(), 4),
        )
        .unwrap();
    storage
        .put(
            "x/y/z",
            test_value(b"5".to_vec(), 5),
        )
        .unwrap();

    // Query with multi-segment wildcard
    let results = storage.get_by_wildcard("a/**/c").unwrap();
    assert_eq!(results.len(), 4);

    let keys: HashSet<String> = results.iter().map(|(k, _)| k.clone()).collect();
    assert!(keys.contains("a/c"));
    assert!(keys.contains("a/b/c"));
    assert!(keys.contains("a/b/x/c"));
    assert!(keys.contains("a/b/x/y/c"));
    assert!(!keys.contains("x/y/z"));
}

#[test]
fn test_wildcard_complex_patterns() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put sensor data
    let sensors = [
        "sensors/room1/temperature",
        "sensors/room1/humidity",
        "sensors/room2/temperature",
        "sensors/room2/humidity",
        "sensors/outdoor/temperature",
    ];

    for (i, key) in sensors.iter().enumerate() {
        storage
            .put(
                key,
                test_value(format!("value_{}", i).into_bytes(), i as u64)
            )
            .unwrap();
    }

    // Query all temperatures
    let temps = storage.get_by_wildcard("sensors/*/temperature").unwrap();
    assert_eq!(temps.len(), 3);

    // Query all room1 sensors (room* not supported, use exact match or **)
    let room1 = storage.get_by_wildcard("sensors/room1/**").unwrap();
    assert_eq!(room1.len(), 2);

    // Query all room2 sensors
    let room2 = storage.get_by_wildcard("sensors/room2/**").unwrap();
    assert_eq!(room2.len(), 2);

    // Query everything under sensors
    let all = storage.get_by_wildcard("sensors/**").unwrap();
    assert_eq!(all.len(), 5);
}

#[test]
fn test_clear_storage() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Put some data
    for i in 0..50 {
        let key = format!("key_{}", i);
        let value = test_value(format!("value_{}", i).into_bytes(), i as u64);
        storage.put(&key, value).unwrap();
    }

    assert_eq!(storage.count().unwrap(), 50);

    // Clear storage
    storage.clear().unwrap();

    // Verify it's empty
    assert_eq!(storage.count().unwrap(), 0);
    let all = storage.get_all().unwrap();
    assert_eq!(all.len(), 0);
}

#[test]
fn test_large_payloads() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Create a large payload (1 MB)
    let large_data = vec![0u8; 1024 * 1024];
    let value = test_value(large_data.clone(), 1 as u64);

    // Store it
    storage.put("large_key", value).unwrap();

    // Retrieve it
    let retrieved = storage.get("large_key").unwrap().unwrap();
    assert_eq!(retrieved.payload.len(), 1024 * 1024);
    assert_eq!(retrieved.payload, large_data);
}

#[test]
fn test_special_characters_in_keys() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    let special_keys = vec![
        "key with spaces",
        "key/with/slashes",
        "key-with-dashes",
        "key_with_underscores",
        "key.with.dots",
        "key:with:colons",
        "key@with@at",
        "key#with#hash",
        "key$with$dollar",
        "key%with%percent",
    ];

    for key in &special_keys {
        let value = test_value(format!("value for {}", key).into_bytes(), 1 as u64);
        storage.put(key, value).unwrap();
    }

    // Verify all can be retrieved
    for key in &special_keys {
        let retrieved = storage.get(key).unwrap();
        assert!(retrieved.is_some(), "Failed to retrieve key: {}", key);
    }

    assert_eq!(storage.count().unwrap(), special_keys.len());
}

#[test]
fn test_empty_payload() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Store empty payload
    let value = test_value(vec![], 1);
    storage.put("empty_key", value).unwrap();

    // Retrieve it
    let retrieved = storage.get("empty_key").unwrap().unwrap();
    assert_eq!(retrieved.payload.len(), 0);
}

#[test]
fn test_different_encodings() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    let encodings = [
        "text/plain",
        "application/json",
        "application/xml",
        "application/octet-stream",
        "application/cbor",
        "text/html",
        "image/jpeg",
    ];

    for (i, encoding) in encodings.iter().enumerate() {
        let key = format!("key_{}", i);
        let timestamp = Timestamp::new(NTP64(i as u64), TimestampId::rand());
        let enc = Encoding::from(*encoding);
        let value = StoredValue::new(b"data".to_vec(), timestamp, enc);
        storage.put(&key, value).unwrap();
    }

    // Verify encodings are preserved
    for (i, encoding) in encodings.iter().enumerate() {
        let key = format!("key_{}", i);
        let retrieved = storage.get(&key).unwrap().unwrap();
        assert_eq!(retrieved.encoding.to_string().as_str(), *encoding);
    }
}

#[test]
fn test_timestamp_preservation() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    let timestamps = [0, 1, 1000, 1234567890, u64::MAX - 1, u64::MAX];

    for (i, &ts) in timestamps.iter().enumerate() {
        let key = format!("key_{}", i);
        let value = test_value(b"data".to_vec(), ts);
        storage.put(&key, value).unwrap();
    }

    // Verify timestamps are preserved
    for (i, &ts) in timestamps.iter().enumerate() {
        let key = format!("key_{}", i);
        let retrieved = storage.get(&key).unwrap().unwrap();
        assert_eq!(retrieved.timestamp.get_time().as_u64(), ts);
    }
}

#[test]
fn test_data_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("persistent.redb");

    // Create storage, add data, and close
    {
        let config = RedbBackendConfig::new()
            .with_base_dir(temp_dir.path().to_path_buf())
            .with_create_dir(true);

        let backend = RedbBackend::new(config).unwrap();

        let storage_config = RedbStorageConfig::new().with_db_path(db_path.clone());

        let storage = backend
            .create_storage("persistent".to_string(), Some(storage_config))
            .unwrap();

        // Add some data
        for i in 0..10 {
            let key = format!("key_{}", i);
            let value = test_value(format!("value_{}", i).into_bytes(), i as u64);
            storage.put(&key, value).unwrap();
        }

        assert_eq!(storage.count().unwrap(), 10);
    }
    // Backend and storage dropped here

    // Reopen the database
    {
        let config = RedbBackendConfig::new()
            .with_base_dir(temp_dir.path().to_path_buf())
            .with_create_dir(true);

        let backend = RedbBackend::new(config).unwrap();

        let storage_config = RedbStorageConfig::new().with_db_path(db_path);

        let storage = backend
            .create_storage("persistent".to_string(), Some(storage_config))
            .unwrap();

        // Data should still be there
        assert_eq!(storage.count().unwrap(), 10);

        // Verify all data
        for i in 0..10 {
            let key = format!("key_{}", i);
            let retrieved = storage.get(&key).unwrap();
            assert!(retrieved.is_some());
            assert_eq!(
                retrieved.unwrap().payload,
                format!("value_{}", i).into_bytes()
            );
        }
    }
}

#[test]
fn test_concurrent_reads() {
    use std::sync::Arc;
    use std::thread;

    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();
    let storage = Arc::new(storage);

    // Populate with data
    for i in 0..100 {
        let key = format!("key_{}", i);
        let value = test_value(format!("value_{}", i).into_bytes(), i as u64);
        storage.put(&key, value).unwrap();
    }

    // Spawn multiple readers
    let mut handles = vec![];
    for thread_id in 0..10 {
        let storage_clone = Arc::clone(&storage);
        let handle = thread::spawn(move || {
            for i in 0..100 {
                let key = format!("key_{}", i);
                let retrieved = storage_clone.get(&key).unwrap();
                assert!(
                    retrieved.is_some(),
                    "Thread {} failed to read key {}",
                    thread_id,
                    key
                );
            }
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn test_concurrent_writes() {
    use std::sync::Arc;
    use std::thread;

    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();
    let storage = Arc::new(storage);

    // Spawn multiple writers
    let mut handles = vec![];
    for thread_id in 0..10 {
        let storage_clone = Arc::clone(&storage);
        let handle = thread::spawn(move || {
            for i in 0..10 {
                let key = format!("thread_{}_key_{}", thread_id, i);
                let value = test_value(format!("thread_{}_value_{}", thread_id, i).into_bytes(), (thread_id * 10 + i) as u64);
                storage_clone.put(&key, value).unwrap();
            }
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all data was written
    assert_eq!(storage.count().unwrap(), 100);
}

#[test]
fn test_storage_with_prefix_stripping() {
    let temp_dir = TempDir::new().unwrap();
    let config = RedbBackendConfig::new()
        .with_base_dir(temp_dir.path().to_path_buf())
        .with_create_dir(true);

    let backend = RedbBackend::new(config).unwrap();

    let storage_config = RedbStorageConfig::new()
        .with_key_expr("demo/app/".to_string())
        .with_strip_prefix(true);

    let storage = backend
        .create_storage("prefix_test".to_string(), Some(storage_config))
        .unwrap();

    // Store with full key
    let value = test_value(b"test".to_vec(), 1);
    storage.put("demo/app/sensor/temp", value).unwrap();

    // Should be able to retrieve with full key
    let retrieved = storage.get("demo/app/sensor/temp").unwrap();
    assert!(retrieved.is_some());
}

#[test]
fn test_batch_operations() {
    let (backend, _temp) = create_test_storage();
    let storage = backend.get_storage("test_storage").unwrap();

    // Batch insert
    let batch_size = 1000;
    for i in 0..batch_size {
        let key = format!("batch_key_{}", i);
        let value = test_value(format!("batch_value_{}", i).into_bytes(), i as u64);
        storage.put(&key, value).unwrap();
    }

    // Verify count
    assert_eq!(storage.count().unwrap(), batch_size);

    // Batch retrieve via get_all
    let all = storage.get_all().unwrap();
    assert_eq!(all.len(), batch_size as usize);

    // Batch delete (via clear)
    storage.clear().unwrap();
    assert_eq!(storage.count().unwrap(), 0);
}

