//! Basic usage example for zenoh-backend-redb.
//!
//! This example demonstrates how to create a backend, create a storage,
//! and perform basic CRUD operations.

use zenoh::bytes::Encoding;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig, StoredValue};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for logging
    tracing_subscriber::fmt::init();

    println!("=== Zenoh Backend redb - Basic Usage Example ===\n");

    // 1. Create a backend with custom configuration
    println!("1. Creating backend...");
    let config = RedbBackendConfig::new()
        .with_base_dir("./example_databases".into())
        .with_create_dir(true);

    let backend = RedbBackend::new(config)?;
    println!("   ✓ Backend created\n");

    // 2. Create a storage instance
    println!("2. Creating storage...");
    let storage_config = RedbStorageConfig::new()
        .with_key_expr("demo/**".to_string())
        .with_strip_prefix(false)
        .with_fsync(true);

    let storage = backend.create_storage("demo_storage".to_string(), Some(storage_config))?;
    println!("   ✓ Storage 'demo_storage' created\n");

    // 3. Store some key-value pairs
    println!("3. Storing data...");
    let entries = vec![
        ("demo/sensor/temperature", "23.5", "application/json"),
        ("demo/sensor/humidity", "65", "application/json"),
        ("demo/sensor/pressure", "1013", "application/json"),
        ("demo/device/status", "online", "text/plain"),
        ("demo/device/name", "sensor-001", "text/plain"),
    ];

    for (key, value, encoding) in &entries {
        let time_u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let timestamp = Timestamp::new(NTP64(time_u64), TimestampId::rand());
        let enc = match *encoding {
            "application/json" => Encoding::APPLICATION_JSON,
            "text/plain" => Encoding::TEXT_PLAIN,
            _ => Encoding::ZENOH_BYTES,
        };
        let stored_value = StoredValue::new(value.as_bytes().to_vec(), timestamp, enc);
        storage.put(key, stored_value)?;
    }
    println!();

    // 4. Retrieve a specific value
    println!("4. Retrieving specific value...");
    let key = "demo/sensor/temperature";
    if let Some(value) = storage.get(key)? {
        let payload_str = String::from_utf8_lossy(&value.payload);
        println!("   ✓ Retrieved '{}': {}", key, payload_str);
        println!("     Timestamp: {}", value.timestamp);
        println!("     Encoding: {}\n", value.encoding);
    }

    // 5. Retrieve all values
    println!("5. Retrieving all values...");
    let all_entries = storage.get_all()?;
    println!("   ✓ Total entries: {}", all_entries.len());
    for (key, value) in &all_entries {
        let payload_str = String::from_utf8_lossy(&value.payload);
        println!("     - {}: {}", key, payload_str);
    }
    println!();

    // 6. Query by prefix
    println!("6. Querying by prefix 'demo/sensor/'...");
    let sensor_entries = storage.get_by_prefix("demo/sensor/")?;
    println!("   ✓ Found {} sensor entries:", sensor_entries.len());
    for (key, value) in &sensor_entries {
        let payload_str = String::from_utf8_lossy(&value.payload);
        println!("     - {}: {}", key, payload_str);
    }
    println!();

    // 7. Query with wildcards
    println!("7. Querying with wildcard 'demo/*/temperature'...");
    let wildcard_entries = storage.get_by_wildcard("demo/*/temperature")?;
    println!("   ✓ Found {} matching entries:", wildcard_entries.len());
    for (key, value) in &wildcard_entries {
        let payload_str = String::from_utf8_lossy(&value.payload);
        println!("     - {}: {}", key, payload_str);
    }
    println!();

    // 8. Update a value
    // 8. Update an existing value
    println!("8. Updating value...");
    let time_u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let timestamp = Timestamp::new(NTP64(time_u64), TimestampId::rand());
    let updated_value = StoredValue::new(
        "24.8".as_bytes().to_vec(),
        timestamp,
        Encoding::APPLICATION_JSON,
    );
    storage.put("demo/sensor/temperature", updated_value)?;
    if let Some(value) = storage.get("demo/sensor/temperature")? {
        let payload_str = String::from_utf8_lossy(&value.payload);
        println!("   ✓ Updated temperature: {}\n", payload_str);
    }

    // 9. Delete a value
    println!("9. Deleting value...");
    storage.delete("demo/device/status")?;
    println!("   ✓ Deleted 'demo/device/status'\n");

    // 10. Count remaining entries
    println!("10. Counting entries...");
    let count = storage.count()?;
    println!("    ✓ Total entries: {}\n", count);

    // 11. List all storages in backend
    println!("11. Listing storages in backend...");
    let storages = backend.list_storages()?;
    println!("    ✓ Storages: {:?}\n", storages);

    // 12. Cleanup (optional - clear all data)
    println!("12. Cleaning up...");
    storage.clear()?;
    println!("    ✓ Cleared all entries from storage");
    println!("    Final count: {}\n", storage.count()?);

    println!("=== Example completed successfully! ===");

    Ok(())
}
