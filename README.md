# Zenoh Backend redb

[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

A [Zenoh](https://zenoh.io) storage backend using [redb](https://www.redb.org/) as the underlying database engine.

## Overview

This backend provides persistent storage for Zenoh using redb, a pure Rust embedded key-value database with ACID compliance and zero-copy reads. It's particularly well-suited for edge computing, IoT devices, and applications requiring a lightweight, dependency-free storage solution.

### Features

- 🦀 **Pure Rust** - No C dependencies, fully memory-safe
- ⚡ **High Performance** - Zero-copy reads with MVCC support, thread-local buffers, bulk operations
- 🔒 **ACID Compliant** - Reliable data storage with transaction support
- 🌐 **Wildcard Queries** - Supports Zenoh wildcard patterns (`*` and `**`)
- 🎛️ **Flexible Configuration** - Per-storage configuration options
- 📖 **Read-Only Mode** - Optional read-only storage instances
- 🔑 **Prefix Stripping** - Efficient key storage with optional prefix removal
- 💾 **Embedded** - No separate database server required
- 📊 **Pre-allocated Buffers** - Thread-local and pre-sized allocations minimize overhead

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
zenoh-backend-redb = "0.1.0"
```

Or use it as a Zenoh plugin (see [Usage as a Plugin](#usage-as-a-plugin) below).

## Quick Start

### As a Library

```rust
use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig, StoredValue};

// Create a backend
let config = RedbBackendConfig::new()
    .with_base_dir("./my_databases".into())
    .with_create_dir(true);

let backend = RedbBackend::new(config)?;

// Create a storage
let storage = backend.create_storage(
    "my_storage".to_string(),
    None  // Use default config
)?;

// Store a value
let value = StoredValue::new(
    b"Hello, Zenoh!".to_vec(),
    timestamp,
    "text/plain".to_string(),
);
storage.put("demo/greeting", value)?;

// Retrieve the value
if let Some(value) = storage.get("demo/greeting")? {
    println!("Retrieved: {}", String::from_utf8_lossy(&value.payload));
}

// Query with wildcards
let results = storage.get_by_wildcard("demo/**")?;
for (key, value) in results {
    println!("{}: {}", key, String::from_utf8_lossy(&value.payload));
}
```

### As a Zenoh Plugin

1. **Install the plugin library** in your Zenoh plugin directory (typically `~/.zenoh/lib/`):

```bash
cargo build --release
cp target/release/libzenoh_backend_redb.so ~/.zenoh/lib/
```

2. **Configure Zenoh** to use the backend in your `zenoh.json5` config file:

```json5
{
  plugins: {
    storage_manager: {
      volumes: {
        redb: {
          // Backend-level configuration
          base_dir: "./zenoh_redb_storage",
          create_dir: true
        }
      },
      storages: {
        demo: {
          key_expr: "demo/example/**",
          strip_prefix: "demo/example",
          volume: {
            id: "redb",
            db_file: "demo_storage",
            create_db: true,
            fsync: true
          }
        }
      }
    }
  }
}
```

3. **Start Zenoh** with the configuration:

```bash
zenohd -c zenoh.json5
```

## Configuration

### Backend Configuration

The backend manages the overall storage infrastructure:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `base_dir` | String | `"./zenoh_redb_backend"` | Base directory for database files |
| `create_dir` | Boolean | `true` | Create directory if it doesn't exist |
| `default_storage_config` | Object | `{}` | Default configuration for storages |

### Storage Configuration

Each storage instance can be individually configured:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `db_file` | String | Storage name | Database filename (without path) |
| `db_path` | String | - | Full path to database (overrides `base_dir` and `db_file`) |
| `cache_size` | Number | redb default | Cache size in bytes |
| `fsync` | Boolean | `true` | Enable fsync for durability |
| `key_expr` | String | - | Key expression prefix filter |
| `strip_prefix` | Boolean | `false` | Strip key_expr prefix from stored keys |
| `table_name` | String | `"zenoh_kv"` | Table name within the database |
| `create_db` | Boolean | `true` | Create database if it doesn't exist |
| `read_only` | Boolean | `false` | Read-only mode |

## Examples

### Basic CRUD Operations

```rust
// Create and store
let value = StoredValue::new(
    b"sensor data".to_vec(),
    12345678,
    "application/octet-stream".to_string(),
);
storage.put("sensor/temperature", value)?;

// Read
let value = storage.get("sensor/temperature")?;

// Delete
storage.delete("sensor/temperature")?;
```

### Wildcard Queries

```rust
// Single-segment wildcard
let results = storage.get_by_wildcard("sensor/*/temperature")?;

// Multi-segment wildcard
let results = storage.get_by_wildcard("sensor/**")?;
```

### Prefix Queries

```rust
// Get all keys with a specific prefix
let results = storage.get_by_prefix("sensor/")?;
```

### Using Strip Prefix

```rust
let config = RedbStorageConfig::new()
    .with_key_expr("demo/example/".to_string())
    .with_strip_prefix(true);

// When storing "demo/example/sensor/temp"
// It will be stored as just "sensor/temp"
// Saving storage space
```

## Running Examples

Run the basic usage example:

```bash
cargo run --example basic_usage
```

This will demonstrate:
- Creating a backend and storage
- Storing and retrieving values
- Prefix and wildcard queries
- Updating and deleting values
- Listing and managing storages

## Testing

### Unit and Integration Tests

Run the standard test suite:

```bash
# Run all tests (excluding ignored tests)
cargo test

# Run with logging
RUST_LOG=debug cargo test

# Run specific test file
cargo test --test integration_storage
```

### End-to-End Integration Tests with zenohd ✅

We provide comprehensive integration tests that spawn a real `zenohd` daemon with the redb backend plugin loaded. These tests verify the complete Zenoh workflow including PUT/GET/DELETE operations and storage persistence.

**✅ Status:** Tests are working and passing! All 3 integration tests validate the complete plugin workflow.

**Prerequisites:**
- zenohd built from Zenoh source (matching rustc 1.85.0)
- storage_manager plugin installed in `~/.zenoh/lib/`
- Our plugin built with `default-features = false` for zenoh_backend_traits

**Quick Setup:**
```bash
# 1. Clone and build Zenoh from source
git clone https://github.com/eclipse-zenoh/zenoh.git
cd zenoh
cargo build --release -p zenohd -p zenoh-plugin-storage-manager

# 2. Install binaries
cp target/release/zenohd ~/.cargo/bin/
mkdir -p ~/.zenoh/lib
cp target/release/libzenoh_plugin_storage_manager.so ~/.zenoh/lib/

# 3. Build our plugin (already configured correctly)
cd ../zenoh-backend-redb
cargo build --release --features plugin

# 4. Run tests (use --test-threads=1 to avoid port conflicts)
cargo test --test integration_zenohd -- --test-threads=1 --nocapture
```

**Test Coverage:**
- ✅ `test_zenohd_with_redb_plugin` - Full PUT/GET/DELETE workflow with prefix queries
- ✅ `test_zenohd_storage_persistence` - Data persistence across daemon restarts
- ✅ `test_plugin_library_exists` - Quick validation check

**📖 Documentation:** See [ZENOHD_INTEGRATION_TEST_SETUP.md](ZENOHD_INTEGRATION_TEST_SETUP.md) for detailed instructions and [INTEGRATION_TEST_SUCCESS.md](INTEGRATION_TEST_SUCCESS.md) for complete test results.

## Benchmarking

Run performance benchmarks:

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark suite
cargo bench --bench storage_benchmarks
cargo bench --bench backend_benchmarks

# Run quick benchmarks (faster, less accurate)
cargo bench -- --quick

# Save baseline for future comparison
cargo bench -- --save-baseline main

# Compare against baseline
cargo bench -- --baseline main
```

View HTML reports:

```bash
open target/criterion/report/index.html
```

Using task runners:

```bash
# With just
just bench              # Run all benchmarks
just bench-storage      # Storage benchmarks only
just bench-report       # Open HTML report

# With cargo-make
cargo make bench
cargo make bench-storage
cargo make bench-report
```

For detailed benchmarking documentation, see [PHASE6_BENCHMARKS.md](PHASE6_BENCHMARKS.md).

## Performance Considerations

### redb Performance Characteristics

- **Read Performance**: Excellent due to zero-copy reads and memory-mapping
- **Write Performance**: Good with MVCC allowing concurrent readers
- **Memory Usage**: Efficient with configurable cache size
- **Durability**: Configurable fsync for balancing performance vs. safety

### Optimization Tips

1. **Adjust cache size** for your workload:
   ```rust
   config.with_cache_size(100 * 1024 * 1024) // 100 MB
   ```

2. **Disable fsync** for non-critical data:
   ```rust
   config.with_fsync(false)
   ```

4. **Use prefix stripping** for long key expressions:
   ```rust
   config.with_strip_prefix(true)
   ```

5. **Use prefix queries** instead of wildcards when possible:
   ```rust
   storage.get_by_prefix("sensor/")  // Faster
   // vs
   storage.get_by_wildcard("sensor/**")  // Slower
   ```

For detailed performance analysis and benchmarking results, see [OPTIMIZATION_RESULTS.md](OPTIMIZATION_RESULTS.md).

## Architecture

```
┌─────────────────────────────────────┐
│       Zenoh Application             │
└────────────┬────────────────────────┘
             │
             ↓
┌─────────────────────────────────────┐
│      RedbBackend                    │
│  - Manages multiple storages        │
│  - Configuration management         │
└────────────┬────────────────────────┘
             │
             ↓
┌─────────────────────────────────────┐
│      RedbStorage                    │
│  - CRUD operations                  │
│  - Wildcard matching                │
│  - Key encoding/decoding            │
└────────────┬────────────────────────┘
             │
             ↓
┌─────────────────────────────────────┐
│         redb Database               │
│  - ACID transactions                │
│  - MVCC                             │
│  - Zero-copy reads                  │
└─────────────────────────────────────┘
```

## Comparison with Other Backends

| Feature | redb | RocksDB | LMDB |
|---------|------|---------|------|
| Pure Rust | ✅ Yes | ❌ No (C++) | ❌ No (C) |
| ACID | ✅ Yes | ✅ Yes | ✅ Yes |
| Zero-copy reads | ✅ Yes | ❌ No | ✅ Yes |
| Concurrent writes | ⚠️ MVCC | ✅ Yes | ❌ Limited |
| Memory-mapped | ✅ Yes | ❌ No | ✅ Yes |
| Setup complexity | ✅ Simple | ⚠️ Moderate | ⚠️ Moderate |
| Best for | Edge/Embedded | High-throughput | Read-heavy |

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.

### Development Setup

```bash
# Clone the repository
git clone https://github.com/yourusername/zenoh-backend-redb.git
cd zenoh-backend-redb

# Run tests
cargo test

# Check formatting
cargo fmt --check

# Run clippy
cargo clippy -- -D warnings

# Build documentation
cargo doc --open
```

## License

This project is licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- Eclipse Public License 2.0 ([LICENSE-EPL](LICENSE-EPL) or https://www.eclipse.org/legal/epl-2.0/)

at your option.

## Resources

- [Zenoh Website](https://zenoh.io)
- [Zenoh Documentation](https://zenoh.io/docs/)
- [redb Documentation](https://docs.rs/redb/)
- [Zenoh GitHub](https://github.com/eclipse-zenoh/zenoh)
- [redb GitHub](https://github.com/cberner/redb)

## Acknowledgments

- Built on top of [Zenoh](https://zenoh.io) by Eclipse Foundation
- Uses [redb](https://www.redb.org/) by Christopher Berner
- Inspired by other Zenoh backends (RocksDB, Filesystem)

## Status

This project is currently in **alpha** stage. The API may change as we gather feedback and improve the implementation.

## Roadmap

- [ ] Complete Zenoh plugin trait implementation
- [ ] Add compression support
- [ ] Implement backup/restore utilities
- [ ] Add Prometheus metrics
- [ ] Performance benchmarks vs other backends
- [ ] Production deployment guide
- [ ] Advanced query optimizations