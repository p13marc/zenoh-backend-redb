//! Integration test that spawns zenohd with the redb backend plugin.
//!
//! This test verifies that the plugin works correctly when loaded by zenohd,
//! testing the full end-to-end flow of Zenoh with the redb storage backend.
//!
//! **Note:** These tests spawn zenohd instances on different ports but should be run
//! serially to avoid potential port conflicts or resource contention:
//! ```bash
//! cargo test --test integration_zenohd -- --test-threads=1
//! ```

use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// Helper struct to manage zenohd process lifecycle
struct ZenohdProcess {
    child: Child,
    #[allow(dead_code)]
    temp_dir: TempDir,
    #[allow(dead_code)]
    config_path: std::path::PathBuf,
}

impl ZenohdProcess {
    /// Spawn zenohd with a configuration that loads our redb backend
    fn spawn(port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("zenoh-config.json5");
        let db_path = temp_dir.path().join("test_storage");
        let plugin_path = std::env::current_dir()?.join("target/release/libzenoh_backend_redb.so");

        // Check if plugin exists
        if !plugin_path.exists() {
            eprintln!("Plugin not found at: {:?}", plugin_path);
            eprintln!("Run: cargo build --release --features plugin");
            return Err("Plugin library not found".into());
        }

        // Create zenohd configuration with our backend
        let config = format!(
            r#"{{
    listen: {{
        endpoints: ["tcp/127.0.0.1:{}"]
    }},
    plugins: {{
        storage_manager: {{
            volumes: {{
                redb: {{
                    __path__: ["{}"],
                    private: {{
                        base_dir: "{}",
                        create_dir: true
                    }}
                }}
            }},
            storages: {{
                test_storage: {{
                    key_expr: "test/**",
                    volume: {{
                        id: "redb",
                        db_file: "test_db.redb",
                        cache_size: 10485760,
                        fsync: true,
                        strip_prefix: false
                    }}
                }}
            }}
        }}
    }}
}}"#,
            port,
            plugin_path.display(),
            db_path.display()
        );

        std::fs::write(&config_path, config)?;

        // Spawn zenohd process with inherited output so we can see what's happening
        let child = Command::new("zenohd")
            .arg("-c")
            .arg(&config_path)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!(
                    "Failed to spawn zenohd. Make sure zenohd is installed and in PATH. Error: {}",
                    e
                )
            })?;

        // Give zenohd time to start and load plugins
        std::thread::sleep(Duration::from_secs(3));

        Ok(ZenohdProcess {
            child,
            temp_dir,
            config_path,
        })
    }

    /// Check if zenohd is still running
    fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true, // Still running
            Ok(Some(status)) => {
                eprintln!("zenohd exited with status: {}", status);
                false
            }
            Err(e) => {
                eprintln!("Error checking zenohd status: {}", e);
                false
            }
        }
    }
}

impl Drop for ZenohdProcess {
    fn drop(&mut self) {
        // Try to gracefully terminate zenohd
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_zenohd_with_redb_plugin() {
    // Spawn zenohd with our plugin on port 7447
    let mut zenohd = ZenohdProcess::spawn(7447).expect("Failed to spawn zenohd");

    // Verify zenohd is running
    assert!(zenohd.is_running(), "zenohd should be running");

    // Give zenohd more time to fully initialize storage
    println!("Waiting for zenohd to fully initialize...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Create a Zenoh session to connect to zenohd
    let session = zenoh::open(zenoh::config::Config::default()).await.unwrap();

    println!("✓ Connected to zenohd");

    // Give storage time to initialize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Test 1: PUT operation
    println!("\n=== Test 1: PUT operation ===");
    let key = "test/sensor/temperature";
    let value = "23.5";

    session.put(key, value).await.unwrap();

    println!("✓ PUT successful: {} = {}", key, value);

    // Give storage time to persist
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Test 2: GET operation
    println!("\n=== Test 2: GET operation ===");

    // Give storage more time to be ready
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("Querying key: {}", key);
    let replies = session.get(key).await.unwrap();

    let mut found = false;
    let mut reply_count = 0;
    while let Ok(reply) = replies.recv_async().await {
        reply_count += 1;
        println!("Received reply #{}", reply_count);
        match reply.into_result() {
            Ok(sample) => {
                let received_value = sample.payload().try_to_string().unwrap();
                println!("✓ GET successful: {} = {}", key, received_value);
                assert_eq!(received_value.as_ref(), value);
                found = true;
                break;
            }
            Err(e) => {
                println!("Reply error: {:?}", e);
            }
        }
    }

    if !found {
        println!("Total replies received: {}", reply_count);
    }
    assert!(found, "Should have received the stored value");

    // Test 3: Multiple PUT operations
    println!("\n=== Test 3: Multiple PUT operations ===");
    let test_data = vec![
        ("test/sensor/humidity", "65"),
        ("test/sensor/pressure", "1013"),
        ("test/config/timeout", "30"),
        ("test/metrics/cpu", "75"),
    ];

    for (k, v) in &test_data {
        session.put(*k, *v).await.unwrap();
        println!("✓ PUT: {} = {}", k, v);
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Test 4: Prefix query
    println!("\n=== Test 4: Prefix query (test/sensor/*) ===");
    let replies = session.get("test/sensor/*").await.unwrap();

    let mut sensor_count = 0;
    let mut total_replies = 0;
    while let Ok(reply) = replies.recv_async().await {
        total_replies += 1;
        match reply.into_result() {
            Ok(sample) => {
                let key_str = sample.key_expr().as_str();
                let value_str = sample.payload().try_to_string().unwrap();
                println!("  - {} = {}", key_str, value_str);
                assert!(key_str.starts_with("test/sensor/"));
                sensor_count += 1;
            }
            Err(e) => {
                println!("Prefix query reply error: {:?}", e);
            }
        }
    }
    println!(
        "Total prefix replies: {}, sensor count: {}",
        total_replies, sensor_count
    );

    // We stored temperature, humidity, and pressure under test/sensor/
    assert!(
        sensor_count >= 3,
        "Should have at least 3 sensor readings, got {}",
        sensor_count
    );

    println!("✓ Found {} sensor readings", sensor_count);

    // Test 5: DELETE operation
    println!("\n=== Test 5: DELETE operation ===");
    let delete_key = "test/config/timeout";

    session.delete(delete_key).await.unwrap();
    println!("✓ DELETE successful: {}", delete_key);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify deletion
    let replies = session.get(delete_key).await.unwrap();

    let mut found_after_delete = false;
    while let Ok(reply) = replies.recv_async().await {
        if reply.into_result().is_ok() {
            found_after_delete = true;
            break;
        }
    }

    assert!(!found_after_delete, "Key should have been deleted");
    println!("✓ Verified deletion");

    // Test 6: Verify zenohd is still running
    println!("\n=== Test 6: Verify zenohd health ===");
    assert!(zenohd.is_running(), "zenohd should still be running");
    println!("✓ zenohd is healthy");

    // Cleanup
    println!("\n=== Cleanup ===");
    drop(session);
    println!("✓ Session closed");

    println!("\n=== All tests passed! ===");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_zenohd_storage_persistence() {
    println!("=== Testing storage persistence across zenohd restarts ===");

    // Phase 1: Start zenohd and store data
    println!("\n--- Phase 1: Store data ---");
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("zenoh-config.json5");
    let db_path = temp_dir.path().join("persistent_storage");
    let plugin_path = std::env::current_dir()
        .unwrap()
        .join("target/release/libzenoh_backend_redb.so");

    if !plugin_path.exists() {
        eprintln!("Plugin not found. Run: cargo build --release --features plugin");
        panic!("Plugin library not found");
    }

    let config_content = format!(
        r#"{{
    listen: {{
        endpoints: ["tcp/127.0.0.1:7448"]
    }},
    plugins: {{
        storage_manager: {{
            volumes: {{
                redb: {{
                    __path__: ["{}"],
                    private: {{
                        base_dir: "{}",
                        create_dir: true
                    }}
                }}
            }},
            storages: {{
                persistent_storage: {{
                    key_expr: "persistent/**",
                    volume: {{
                        id: "redb",
                        db_file: "persist.redb",
                        cache_size: 10485760,
                        fsync: true
                    }}
                }}
            }}
        }}
    }}
}}"#,
        plugin_path.display(),
        db_path.display()
    );

    std::fs::write(&config_path, &config_content).unwrap();

    // Start zenohd
    let mut zenohd_1 = Command::new("zenohd")
        .arg("-c")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn zenohd");

    std::thread::sleep(Duration::from_secs(3));

    // Connect and store data
    let session = zenoh::open(zenoh::config::Config::default()).await.unwrap();

    let test_key = "persistent/test/data";
    let test_value = "persistent_value_123";

    session.put(test_key, test_value).await.unwrap();
    println!("✓ Stored: {} = {}", test_key, test_value);

    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(session);

    // Phase 2: Stop zenohd
    println!("\n--- Phase 2: Stopping zenohd ---");
    let _ = zenohd_1.kill();
    let _ = zenohd_1.wait();
    println!("✓ zenohd stopped");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Phase 3: Restart zenohd and verify data persisted
    println!("\n--- Phase 3: Restart and verify persistence ---");
    let mut zenohd_2 = Command::new("zenohd")
        .arg("-c")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn zenohd");

    std::thread::sleep(Duration::from_secs(3));

    // Connect again
    let session2 = zenoh::open(zenoh::config::Config::default()).await.unwrap();

    // Query the data
    let replies = session2.get(test_key).await.unwrap();

    let mut found = false;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.into_result() {
            let received_value = sample.payload().try_to_string().unwrap();
            println!(
                "✓ Retrieved after restart: {} = {}",
                test_key, received_value
            );
            assert_eq!(received_value.as_ref(), test_value);
            found = true;
            break;
        }
    }

    assert!(found, "Data should have persisted across restart");
    println!("✓ Persistence verified!");

    // Cleanup
    drop(session2);
    let _ = zenohd_2.kill();
    let _ = zenohd_2.wait();

    println!("\n=== Persistence test passed! ===");
}

#[test]
fn test_plugin_library_exists() {
    let plugin_path = std::env::current_dir()
        .unwrap()
        .join("target/release/libzenoh_backend_redb.so");

    if !plugin_path.exists() {
        panic!(
            "Plugin library not found at: {:?}\nRun: cargo build --release --features plugin",
            plugin_path
        );
    }

    println!("✓ Plugin library found at: {:?}", plugin_path);
}
