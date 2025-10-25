//! Plugin integration example for zenoh-backend-redb.
//!
//! This example demonstrates how to use the redb backend as a Zenoh storage plugin
//! in a programmatic way (without using a configuration file).
//!
//! Note: This example shows the plugin structure but requires a full Zenoh runtime
//! to work properly. For standalone usage, see basic_usage.rs instead.

use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("=== Zenoh Backend redb - Plugin Integration Example ===\n");

    // This example demonstrates the plugin architecture and concepts.
    // In a real deployment, the plugin would be loaded by zenohd from a shared library.

    println!("1. Backend Architecture Overview:");
    println!("   - Plugin Layer: Implements Zenoh Volume and Storage traits");
    println!("   - Backend Layer: Manages multiple storage instances");
    println!("   - Storage Layer: Handles actual data operations with redb");
    println!();

    // Create backend (this would normally be done by the plugin loader)
    println!("2. Creating backend...");
    let config = RedbBackendConfig::new()
        .with_base_dir("./plugin_demo_storage".into())
        .with_create_dir(true);

    let backend = RedbBackend::new(config)?;
    println!("   ✓ Backend created\n");

    // Create multiple storage instances
    println!("3. Creating storage instances...");

    // Storage 1: Sensor data
    let sensor_config = RedbStorageConfig::new()
        .with_key_expr("sensor/**".to_string())
        .with_strip_prefix(true)
        .with_cache_size(50 * 1024 * 1024); // 50MB

    let sensor_storage = backend.create_storage("sensor_data".to_string(), Some(sensor_config))?;
    println!("   ✓ Created 'sensor_data' storage");

    // Storage 2: Configuration data (read-only)
    let config_config = RedbStorageConfig::new()
        .with_key_expr("config/**".to_string())
        .with_read_only(false) // Set to true for actual read-only
        .with_fsync(true);

    let config_storage = backend.create_storage("config_data".to_string(), Some(config_config))?;
    println!("   ✓ Created 'config_data' storage");

    // Storage 3: Time-series metrics
    let metrics_config = RedbStorageConfig::new()
        .with_key_expr("metrics/**".to_string())
        .with_fsync(false) // Faster writes, less durable
        .with_cache_size(100 * 1024 * 1024); // 100MB

    let metrics_storage =
        backend.create_storage("metrics_data".to_string(), Some(metrics_config))?;
    println!("   ✓ Created 'metrics_data' storage\n");

    // Demonstrate storage operations
    println!("4. Demonstrating storage operations...\n");

    // Sensor data operations
    println!("   Sensor Storage:");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    use zenoh_backend_redb::StoredValue;

    sensor_storage.put(
        "sensor/temperature/room1",
        StoredValue::new(b"23.5".to_vec(), timestamp, "application/json".to_string()),
    )?;
    sensor_storage.put(
        "sensor/humidity/room1",
        StoredValue::new(b"65".to_vec(), timestamp, "application/json".to_string()),
    )?;
    println!("     ✓ Stored sensor readings");

    // Config data operations
    println!("   Config Storage:");
    config_storage.put(
        "config/app/timeout",
        StoredValue::new(b"30".to_vec(), timestamp, "text/plain".to_string()),
    )?;
    println!("     ✓ Stored configuration\n");

    // Metrics operations
    println!("   Metrics Storage:");
    for i in 0..5 {
        let metric_value = format!("{}", 100 + i);
        metrics_storage.put(
            &format!("metrics/cpu/usage/{}", i),
            StoredValue::new(
                metric_value.as_bytes().to_vec(),
                timestamp + i,
                "application/json".to_string(),
            ),
        )?;
    }
    println!("     ✓ Stored metrics time-series\n");

    // Query operations
    println!("5. Querying data...\n");

    // Query sensor data by prefix
    let sensor_data = sensor_storage.get_by_prefix("sensor/")?;
    println!("   Sensor data (prefix query):");
    for (key, value) in &sensor_data {
        println!(
            "     - {}: {}",
            key,
            String::from_utf8_lossy(&value.payload)
        );
    }
    println!();

    // Query metrics with wildcard
    let metrics_data = metrics_storage.get_by_wildcard("metrics/cpu/**")?;
    println!("   Metrics data (wildcard query):");
    for (key, value) in &metrics_data {
        println!(
            "     - {}: {}",
            key,
            String::from_utf8_lossy(&value.payload)
        );
    }
    println!();

    // Storage statistics
    println!("6. Storage Statistics:\n");
    let storages = backend.list_storages()?;
    for storage_name in &storages {
        let storage = backend.get_storage(storage_name)?;
        let count = storage.count()?;
        println!("   {} - {} entries", storage_name, count);
    }
    println!();

    // Plugin integration points
    println!("7. Plugin Integration Points:\n");
    println!("   When deployed as a Zenoh plugin:");
    println!("   - Volume trait: Manages backend lifecycle");
    println!("   - Storage trait: Handles Zenoh get/put/delete operations");
    println!("   - Async operations: All storage ops are async-compatible");
    println!("   - Timestamp conversion: Zenoh timestamps ↔ Unix timestamps");
    println!("   - Key expression: Supports Zenoh wildcard patterns");
    println!();

    // Configuration example
    println!("8. Zenoh Router Configuration:\n");
    println!("   In zenoh.json5:");
    println!(
        r#"   {{
     plugins: {{
       storage_manager: {{
         volumes: {{
           redb: {{}}
         }},
         storages: {{
           sensor_data: {{
             key_expr: "sensor/**",
             volume: {{
               id: "redb",
               dir: "sensor_data",
               create_db: true,
               cache_size: 52428800
             }}
           }}
         }}
       }}
     }}
   }}"#
    );
    println!();

    // Cleanup demonstration
    println!("9. Cleanup and resource management...");
    println!("   Each storage implements Drop for automatic cleanup");
    println!("   Backend manages storage lifecycle");
    println!("   redb handles database file closing automatically");
    println!();

    // Optional: Clear all data
    println!("10. Clearing test data (optional)...");
    for storage_name in &storages {
        let storage = backend.get_storage(storage_name)?;
        storage.clear()?;
        println!("    ✓ Cleared {}", storage_name);
    }
    println!();

    println!("=== Plugin Integration Example Complete! ===\n");
    println!("Next Steps:");
    println!("  1. Build the plugin: cargo build --release");
    println!("  2. Install plugin: cp target/release/libzenoh_backend_redb.so ~/.zenoh/lib/");
    println!("  3. Configure: Create zenoh.json5 (see config/zenoh-redb-example.json5)");
    println!("  4. Run router: zenohd -c zenoh.json5");
    println!("  5. Test with: z_put/z_get or REST API");

    Ok(())
}
