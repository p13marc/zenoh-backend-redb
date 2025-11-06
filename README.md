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

1. **Build the plugin library**:

```bash
cargo build --release --features plugin
```

2. **Install the plugin** in your Zenoh plugin directory (typically `~/.zenoh/lib/`):

```bash
cp target/release/libzenoh_backend_redb.so ~/.zenoh/lib/
```

## Quick Start

1. **Configure Zenoh** to use the backend in your `zenoh.json5` config file:

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

2. **Start Zenoh** with the configuration:

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

## Usage Examples

### Basic Storage Configuration

```json5
{
  plugins: {
    storage_manager: {
      volumes: {
        redb: {
          base_dir: "./my_data"
        }
      },
      storages: {
        sensor_data: {
          key_expr: "sensor/**",
          volume: {
            id: "redb",
            db_file: "sensors",
            fsync: true
          }
        }
      }
    }
  }
}
```

### Multiple Storages

```json5
{
  plugins: {
    storage_manager: {
      volumes: {
        redb: {
          base_dir: "./zenoh_data"
        }
      },
      storages: {
        sensors: {
          key_expr: "sensor/**",
          volume: {
            id: "redb",
            db_file: "sensor_db"
          }
        },
        config: {
          key_expr: "config/**",
          volume: {
            id: "redb",
            db_file: "config_db",
            read_only: false
          }
        }
      }
    }
  }
}
```

### Using Strip Prefix

Strip prefix saves storage space by removing the common prefix from stored keys:

```json5
{
  plugins: {
    storage_manager: {
      storages: {
        demo: {
          key_expr: "demo/example/**",
          strip_prefix: "demo/example",  // Keys stored without this prefix
          volume: {
            id: "redb",
            db_file: "demo_storage"
          }
        }
      }
    }
  }
}
```

### Read-Only Storage

```json5
{
  plugins: {
    storage_manager: {
      storages: {
        archive: {
          key_expr: "archive/**",
          volume: {
            id: "redb",
            db_file: "archive_db",
            read_only: true  // Prevents modifications
          }
        }
      }
    }
  }
}
```

### Custom Cache Size

```json5
{
  plugins: {
    storage_manager: {
      storages: {
        large_data: {
          key_expr: "large/**",
          volume: {
            id: "redb",
            db_file: "large_db",
            cache_size: 104857600  // 100 MB cache
          }
        }
      }
    }
  }
}
```

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