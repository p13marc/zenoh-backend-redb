//! Integration tests that spawn zenohd with the redb backend plugin.
//!
//! These tests verify that the plugin loads correctly and that data persists
//! across zenohd restarts.
//!
//! **Requirements:**
//! - Plugin must be built: `cargo build --release --features plugin`
//! - zenohd with storage_manager plugin must be installed
//!
//! **IMPORTANT:** These tests require zenohd AND the storage_manager plugin to be installed.
//! The easiest way to run them is using Docker where everything is built together:
//!
//! ```bash
//! just docker-test-zenohd
//! ```
//!
//! For local testing, you need to build zenohd with storage_manager from source:
//! ```bash
//! # Clone zenoh and build with plugins
//! git clone https://github.com/eclipse-zenoh/zenoh.git
//! cd zenoh
//! cargo build --release -p zenohd -p zenoh-plugin-storage-manager
//! # Copy plugins to a known location
//! cp target/release/libzenoh_plugin_storage_manager.so ~/.zenoh/lib/
//! ```

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// Get the path to the built plugin library
fn get_plugin_path() -> PathBuf {
    // Check local build first
    let local_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("target/release/libzenoh_backend_redb.so");

    if local_path.exists() {
        return local_path;
    }

    // Check system path (Docker environment)
    let system_path = PathBuf::from("/usr/local/lib/libzenoh_backend_redb.so");
    if system_path.exists() {
        return system_path;
    }

    local_path
}

/// Get the directory containing plugins (storage_manager and our redb backend)
fn get_plugin_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // Local release build directory
    if let Ok(cwd) = std::env::current_dir() {
        let local_release = cwd.join("target/release");
        if local_release.exists() {
            dirs.push(local_release);
        }
    }

    // System lib directory (Docker)
    let system_lib = PathBuf::from("/usr/local/lib");
    if system_lib.exists() {
        dirs.push(system_lib);
    }

    // User's zenoh lib directory
    if let Some(home) = std::env::var_os("HOME") {
        let zenoh_lib = PathBuf::from(home).join(".zenoh/lib");
        if zenoh_lib.exists() {
            dirs.push(zenoh_lib);
        }
    }

    dirs
}

/// Helper struct to manage zenohd process lifecycle
struct ZenohdInstance {
    child: Child,
    _temp_dir: TempDir,
    port: u16,
}

impl ZenohdInstance {
    /// Spawn zenohd with a configuration that loads our redb backend
    fn spawn(port: u16, storage_dir: &std::path::Path) -> Result<Self, String> {
        let temp_dir = TempDir::new().map_err(|e| format!("Failed to create temp dir: {}", e))?;
        let config_path = temp_dir.path().join("zenoh-config.json5");
        let plugin_path = get_plugin_path();

        // Check if plugin exists
        if !plugin_path.exists() {
            return Err(format!(
                "Plugin not found at: {:?}\nRun: cargo build --release --features plugin",
                plugin_path
            ));
        }

        // Get all plugin search directories
        let search_dirs = get_plugin_search_dirs();
        let search_dirs_json: Vec<String> = search_dirs
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect();

        // Create zenohd configuration
        let config = format!(
            r#"{{
    mode: "router",
    listen: {{
        endpoints: ["tcp/127.0.0.1:{}"]
    }},
    scouting: {{
        multicast: {{
            enabled: false
        }}
    }},
    plugins_loading: {{
        enabled: true,
        search_dirs: [{}]
    }},
    plugins: {{
        storage_manager: {{
            volumes: {{
                redb: {{}}
            }},
            storages: {{
                test_storage: {{
                    key_expr: "test/**",
                    volume: {{
                        id: "redb",
                        dir: "test_db",
                        create_db: true,
                        fsync: true
                    }}
                }}
            }}
        }}
    }}
}}"#,
            port,
            search_dirs_json.join(", "),
        );

        std::fs::write(&config_path, &config)
            .map_err(|e| format!("Failed to write config: {}", e))?;

        // Spawn zenohd with output visible for debugging
        let child = Command::new("zenohd")
            .arg("-c")
            .arg(&config_path)
            .env("ZENOH_BACKEND_REDB_ROOT", storage_dir)
            .env(
                "RUST_LOG",
                "warn,zenoh_backend_redb=info,zenoh_plugin_storage_manager=info",
            )
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!(
                    "Failed to spawn zenohd. Is it installed and in PATH? Error: {}",
                    e
                )
            })?;

        // Give zenohd time to start and load plugins
        std::thread::sleep(Duration::from_secs(3));

        Ok(ZenohdInstance {
            child,
            _temp_dir: temp_dir,
            port,
        })
    }

    /// Check if zenohd is still running
    fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
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

    /// Get the endpoint to connect to this instance
    fn endpoint(&self) -> String {
        format!("tcp/127.0.0.1:{}", self.port)
    }
}

impl Drop for ZenohdInstance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Create a Zenoh session configuration that connects to a specific endpoint
fn client_config(endpoint: &str) -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", r#""client""#)
        .expect("Failed to set mode");
    config
        .insert_json5("connect/endpoints", &format!(r#"["{}"]"#, endpoint))
        .expect("Failed to set connect endpoints");
    // Disable scouting to avoid interference
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("Failed to disable multicast");
    config
}

/// Check if the storage_manager plugin is available and compatible.
/// This does a quick test by starting zenohd and checking if it loads the plugin.
fn check_storage_manager_plugin() -> bool {
    let search_dirs = get_plugin_search_dirs();

    // First check if the file exists
    let mut plugin_found = false;
    for dir in &search_dirs {
        let plugin_path = dir.join("libzenoh_plugin_storage_manager.so");
        if plugin_path.exists() {
            println!("Found storage_manager plugin at: {:?}", plugin_path);
            plugin_found = true;
            break;
        }
    }

    if !plugin_found {
        println!("storage_manager plugin not found in search directories");
        return false;
    }

    // Try to load the plugin with zenohd to check compatibility
    let temp_dir = match TempDir::new() {
        Ok(d) => d,
        Err(_) => return false,
    };

    let config_path = temp_dir.path().join("test-config.json5");
    let search_dirs_json: Vec<String> = search_dirs
        .iter()
        .map(|p| format!("\"{}\"", p.display()))
        .collect();

    let config = format!(
        r#"{{
    mode: "router",
    scouting: {{ multicast: {{ enabled: false }} }},
    plugins_loading: {{
        enabled: true,
        search_dirs: [{}]
    }},
    plugins: {{
        storage_manager: {{
            volumes: {{}},
            storages: {{}}
        }}
    }}
}}"#,
        search_dirs_json.join(", ")
    );

    if std::fs::write(&config_path, &config).is_err() {
        return false;
    }

    // Run zenohd briefly and capture output to check for plugin load errors
    let output = Command::new("zenohd")
        .arg("-c")
        .arg(&config_path)
        .env("RUST_LOG", "error")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    match output {
        Ok(mut child) => {
            // Give it a moment to try loading plugins
            std::thread::sleep(Duration::from_secs(2));
            let _ = child.kill();

            // Check stderr for compatibility errors
            if let Some(stderr) = child.stderr.take() {
                use std::io::{BufRead, BufReader};
                let reader = BufReader::new(stderr);
                for line in reader.lines().take(20).flatten() {
                    if line.contains("Plugin compatibility mismatch")
                        || line.contains("Incompatible rustc versions")
                    {
                        println!("⚠ storage_manager plugin has version mismatch with zenohd");
                        println!("  The plugin was built with a different Rust version.");
                        println!(
                            "  Use Docker to ensure matching versions: just docker-test-zenohd"
                        );
                        return false;
                    }
                }
            }

            let _ = child.wait();
            true
        }
        Err(_) => false,
    }
}

#[test]
fn test_plugin_library_exists() {
    let plugin_path = get_plugin_path();
    assert!(
        plugin_path.exists(),
        "Plugin library not found at: {:?}\nRun: cargo build --release --features plugin",
        plugin_path
    );
    println!("✓ Plugin library found at: {:?}", plugin_path);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_zenohd_plugin_loads_correctly() {
    println!("\n=== Test: Plugin loads correctly ===\n");

    if !check_storage_manager_plugin() {
        println!("⚠ SKIPPED: storage_manager plugin not found.");
        println!("  Run these tests with Docker: just docker-test-zenohd");
        return;
    }

    let storage_dir = TempDir::new().expect("Failed to create storage dir");

    // Spawn zenohd with our plugin
    let mut zenohd =
        ZenohdInstance::spawn(7450, storage_dir.path()).expect("Failed to spawn zenohd");

    // Give it time to fully initialize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify zenohd is still running (plugin didn't crash it)
    assert!(
        zenohd.is_running(),
        "zenohd should be running after plugin load"
    );
    println!("✓ zenohd is running with plugin loaded");

    // Connect as a client
    let config = client_config(&zenohd.endpoint());
    let session = zenoh::open(config)
        .await
        .expect("Failed to connect to zenohd");
    println!("✓ Client connected to zenohd");

    // Verify zenohd is still healthy after client connection
    assert!(zenohd.is_running(), "zenohd should still be running");
    println!("✓ zenohd healthy after client connection");

    drop(session);
    println!("\n=== Plugin load test passed! ===\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_zenohd_put_get_operations() {
    println!("\n=== Test: PUT and GET operations ===\n");

    if !check_storage_manager_plugin() {
        println!("⚠ SKIPPED: storage_manager plugin not found.");
        println!("  Run these tests with Docker: just docker-test-zenohd");
        return;
    }

    let storage_dir = TempDir::new().expect("Failed to create storage dir");
    let mut zenohd =
        ZenohdInstance::spawn(7451, storage_dir.path()).expect("Failed to spawn zenohd");

    // Wait for storage to be fully initialized
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(zenohd.is_running(), "zenohd should be running");

    let config = client_config(&zenohd.endpoint());
    let session = zenoh::open(config)
        .await
        .expect("Failed to connect to zenohd");

    // Wait for session to be fully established
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Test PUT
    let key = "test/sensor/temperature";
    let value = "23.5";
    session.put(key, value).await.expect("PUT operation failed");
    println!("✓ PUT: {} = {}", key, value);

    // Give storage time to persist
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Test GET with timeout
    println!("Querying key: {}", key);
    let replies = session
        .get(key)
        .timeout(Duration::from_secs(5))
        .await
        .expect("GET operation failed");

    let mut found = false;
    let mut reply_count = 0;
    while let Ok(reply) = replies.recv_async().await {
        reply_count += 1;
        match reply.into_result() {
            Ok(sample) => {
                let received = sample.payload().try_to_string().unwrap();
                println!("✓ GET: {} = {}", sample.key_expr(), received);
                assert_eq!(received.as_ref(), value, "Retrieved value should match");
                found = true;
                break;
            }
            Err(e) => {
                println!("  Reply {}: error {:?}", reply_count, e);
            }
        }
    }
    println!("Total replies received: {}", reply_count);
    assert!(found, "Should have received the stored value");

    drop(session);
    println!("\n=== PUT/GET test passed! ===\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_zenohd_delete_operation() {
    println!("\n=== Test: DELETE operation ===\n");

    if !check_storage_manager_plugin() {
        println!("⚠ SKIPPED: storage_manager plugin not found.");
        println!("  Run these tests with Docker: just docker-test-zenohd");
        return;
    }

    let storage_dir = TempDir::new().expect("Failed to create storage dir");
    let mut zenohd =
        ZenohdInstance::spawn(7452, storage_dir.path()).expect("Failed to spawn zenohd");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(zenohd.is_running(), "zenohd should be running");

    let config = client_config(&zenohd.endpoint());
    let session = zenoh::open(config)
        .await
        .expect("Failed to connect to zenohd");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Store a value
    let key = "test/to_delete";
    let value = "temporary_value";
    session.put(key, value).await.expect("PUT failed");
    println!("✓ PUT: {} = {}", key, value);

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify it exists
    let replies = session
        .get(key)
        .timeout(Duration::from_secs(5))
        .await
        .expect("GET failed");
    let mut found = false;
    while let Ok(reply) = replies.recv_async().await {
        if reply.into_result().is_ok() {
            found = true;
            break;
        }
    }
    assert!(found, "Value should exist before delete");
    println!("✓ Verified value exists");

    // Delete it
    session.delete(key).await.expect("DELETE failed");
    println!("✓ DELETE: {}", key);

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify it's gone
    let replies = session
        .get(key)
        .timeout(Duration::from_secs(5))
        .await
        .expect("GET after delete failed");
    let mut still_exists = false;
    while let Ok(reply) = replies.recv_async().await {
        if reply.into_result().is_ok() {
            still_exists = true;
            break;
        }
    }
    assert!(!still_exists, "Value should not exist after delete");
    println!("✓ Verified value deleted");

    drop(session);
    println!("\n=== DELETE test passed! ===\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_zenohd_wildcard_queries() {
    println!("\n=== Test: Wildcard queries ===\n");

    if !check_storage_manager_plugin() {
        println!("⚠ SKIPPED: storage_manager plugin not found.");
        println!("  Run these tests with Docker: just docker-test-zenohd");
        return;
    }

    let storage_dir = TempDir::new().expect("Failed to create storage dir");
    let mut zenohd =
        ZenohdInstance::spawn(7453, storage_dir.path()).expect("Failed to spawn zenohd");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(zenohd.is_running(), "zenohd should be running");

    let config = client_config(&zenohd.endpoint());
    let session = zenoh::open(config)
        .await
        .expect("Failed to connect to zenohd");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Store multiple values
    let test_data = [
        ("test/sensors/temp", "22.5"),
        ("test/sensors/humidity", "65"),
        ("test/sensors/pressure", "1013"),
        ("test/config/timeout", "30"),
    ];

    for (k, v) in &test_data {
        session.put(*k, *v).await.expect("PUT failed");
        println!("  PUT: {} = {}", k, v);
    }
    println!("✓ Stored {} values", test_data.len());

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Query with single-level wildcard
    println!("\nQuerying test/sensors/*");
    let replies = session
        .get("test/sensors/*")
        .timeout(Duration::from_secs(5))
        .await
        .expect("Wildcard GET failed");

    let mut sensor_count = 0;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.into_result() {
            let key = sample.key_expr().as_str();
            let val = sample.payload().try_to_string().unwrap();
            println!("  Found: {} = {}", key, val);
            assert!(key.starts_with("test/sensors/"), "Key should match pattern");
            sensor_count += 1;
        }
    }
    assert_eq!(sensor_count, 3, "Should find 3 sensor values");
    println!("✓ Wildcard query returned {} results", sensor_count);

    // Query with multi-level wildcard
    println!("\nQuerying test/**");
    let replies = session
        .get("test/**")
        .timeout(Duration::from_secs(5))
        .await
        .expect("Multi-wildcard failed");

    let mut total_count = 0;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.into_result() {
            let key = sample.key_expr().as_str();
            println!("  Found: {}", key);
            total_count += 1;
        }
    }
    assert_eq!(total_count, 4, "Should find all 4 values");
    println!("✓ Multi-level wildcard returned {} results", total_count);

    drop(session);
    println!("\n=== Wildcard test passed! ===\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_zenohd_storage_persistence() {
    println!("\n=== Test: Storage persistence across restarts ===\n");

    if !check_storage_manager_plugin() {
        println!("⚠ SKIPPED: storage_manager plugin not found.");
        println!("  Run these tests with Docker: just docker-test-zenohd");
        return;
    }

    // Use a persistent storage directory (not cleaned up between restarts)
    let storage_dir = TempDir::new().expect("Failed to create storage dir");
    let storage_path = storage_dir.path().to_path_buf();

    let test_key = "test/persistent/data";
    let test_value = "persistent_value_12345";

    // Phase 1: Start zenohd and store data
    println!("--- Phase 1: Store data ---");
    {
        let mut zenohd =
            ZenohdInstance::spawn(7454, &storage_path).expect("Failed to spawn zenohd");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(zenohd.is_running(), "zenohd should be running");

        let config = client_config(&zenohd.endpoint());
        let session = zenoh::open(config)
            .await
            .expect("Failed to connect to zenohd");

        tokio::time::sleep(Duration::from_secs(1)).await;

        session.put(test_key, test_value).await.expect("PUT failed");
        println!("✓ Stored: {} = {}", test_key, test_value);

        // Ensure data is flushed to disk
        tokio::time::sleep(Duration::from_secs(2)).await;

        drop(session);
        // zenohd is dropped here, stopping the process
    }
    println!("✓ zenohd stopped");

    // Brief pause between restarts
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Phase 2: Restart zenohd and verify data persisted
    println!("\n--- Phase 2: Verify persistence ---");
    {
        let mut zenohd =
            ZenohdInstance::spawn(7454, &storage_path).expect("Failed to spawn zenohd");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            zenohd.is_running(),
            "zenohd should be running after restart"
        );
        println!("✓ zenohd restarted");

        let config = client_config(&zenohd.endpoint());
        let session = zenoh::open(config)
            .await
            .expect("Failed to connect to zenohd");

        tokio::time::sleep(Duration::from_secs(1)).await;

        // Query the persisted data
        let replies = session
            .get(test_key)
            .timeout(Duration::from_secs(5))
            .await
            .expect("GET failed");

        let mut found = false;
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.into_result() {
                let received = sample.payload().try_to_string().unwrap();
                println!("✓ Retrieved after restart: {} = {}", test_key, received);
                assert_eq!(
                    received.as_ref(),
                    test_value,
                    "Persisted value should match"
                );
                found = true;
                break;
            }
        }
        assert!(found, "Data should have persisted across restart");

        drop(session);
    }

    println!("\n=== Persistence test passed! ===\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_zenohd_multiple_keys_persistence() {
    println!("\n=== Test: Multiple keys persistence ===\n");

    if !check_storage_manager_plugin() {
        println!("⚠ SKIPPED: storage_manager plugin not found.");
        println!("  Run these tests with Docker: just docker-test-zenohd");
        return;
    }

    let storage_dir = TempDir::new().expect("Failed to create storage dir");
    let storage_path = storage_dir.path().to_path_buf();

    let test_data = [
        ("test/persist/key1", "value1"),
        ("test/persist/key2", "value2"),
        ("test/persist/nested/key3", "value3"),
    ];

    // Phase 1: Store multiple values
    println!("--- Phase 1: Store multiple values ---");
    {
        let mut zenohd =
            ZenohdInstance::spawn(7455, &storage_path).expect("Failed to spawn zenohd");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(zenohd.is_running());

        let config = client_config(&zenohd.endpoint());
        let session = zenoh::open(config).await.expect("Failed to connect");

        tokio::time::sleep(Duration::from_secs(1)).await;

        for (k, v) in &test_data {
            session.put(*k, *v).await.expect("PUT failed");
            println!("  Stored: {} = {}", k, v);
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(session);
    }
    println!("✓ zenohd stopped");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Phase 2: Verify all values persisted
    println!("\n--- Phase 2: Verify all values ---");
    {
        let mut zenohd =
            ZenohdInstance::spawn(7455, &storage_path).expect("Failed to spawn zenohd");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(zenohd.is_running());

        let config = client_config(&zenohd.endpoint());
        let session = zenoh::open(config).await.expect("Failed to connect");

        tokio::time::sleep(Duration::from_secs(1)).await;

        for (k, expected_v) in &test_data {
            let replies = session
                .get(*k)
                .timeout(Duration::from_secs(5))
                .await
                .expect("GET failed");

            let mut found = false;
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.into_result() {
                    let received = sample.payload().try_to_string().unwrap();
                    assert_eq!(received.as_ref(), *expected_v);
                    println!("  ✓ Retrieved: {} = {}", k, received);
                    found = true;
                    break;
                }
            }
            assert!(found, "Key {} should have persisted", k);
        }

        drop(session);
    }

    println!("\n=== Multiple keys persistence test passed! ===\n");
}
