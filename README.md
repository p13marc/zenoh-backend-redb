# Zenoh Backend redb

[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

A [Zenoh](https://zenoh.io) storage backend using [redb](https://www.redb.org/) as the underlying database engine.

## Overview

This backend provides persistent storage for Zenoh using redb, a pure Rust embedded key-value database with ACID compliance and zero-copy reads. It's particularly well-suited for edge computing, IoT devices, and applications requiring a lightweight, dependency-free storage solution.

**Compatible with Zenoh 1.7.0**

### Features

- **Pure Rust** - No C dependencies, fully memory-safe
- **High Performance** - Zero-copy reads with MVCC support, thread-local buffers
- **ACID Compliant** - Reliable data storage with transaction support
- **Wildcard Queries** - Supports Zenoh wildcard patterns (`*` and `**`)
- **Flexible Configuration** - Per-storage configuration options
- **Read-Only Mode** - Optional read-only storage instances
- **Prefix Stripping** - Efficient key storage with optional prefix removal
- **Embedded** - No separate database server required
- **Persistent** - Data survives zenohd restarts

## Installation

1. **Build the plugin library**:

```bash
cargo build --release --features plugin
```

2. **Install the plugin** in your Zenoh plugin directory:

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
        redb: {}
      },
      storages: {
        demo: {
          key_expr: "demo/example/**",
          strip_prefix: "demo/example",
          volume: {
            id: "redb",
            dir: "demo_storage",
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

### Storage Configuration

Each storage instance can be individually configured:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `dir` | String | required | Database directory name (creates `<name>.redb`) |
| `db_file` | String | - | Alternative to `dir`, explicit database filename |
| `cache_size` | Number | redb default | Cache size in bytes |
| `fsync` | Boolean | `true` | Enable fsync for durability |
| `create_db` | Boolean | `true` | Create database if it doesn't exist |
| `read_only` | Boolean | `false` | Read-only mode |

### Environment Variables

- `ZENOH_BACKEND_REDB_ROOT`: Override default storage directory (default: `~/.zenoh/zenoh_backend_redb`)

## Usage Examples

### Basic Storage Configuration

```json5
{
  plugins: {
    storage_manager: {
      volumes: {
        redb: {}
      },
      storages: {
        sensor_data: {
          key_expr: "sensor/**",
          volume: {
            id: "redb",
            dir: "sensors",
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
        redb: {}
      },
      storages: {
        sensors: {
          key_expr: "sensor/**",
          volume: {
            id: "redb",
            dir: "sensor_db"
          }
        },
        config: {
          key_expr: "config/**",
          volume: {
            id: "redb",
            dir: "config_db"
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
      volumes: {
        redb: {}
      },
      storages: {
        demo: {
          key_expr: "demo/example/**",
          strip_prefix: "demo/example",
          volume: {
            id: "redb",
            dir: "demo_storage"
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
      volumes: {
        redb: {}
      },
      storages: {
        archive: {
          key_expr: "archive/**",
          volume: {
            id: "redb",
            dir: "archive_db",
            read_only: true
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
      volumes: {
        redb: {}
      },
      storages: {
        large_data: {
          key_expr: "large/**",
          volume: {
            id: "redb",
            dir: "large_db",
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
│  - Wildcard matching (* and **)     │
│  - Dual-table design                │
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
| Pure Rust | Yes | No (C++) | No (C) |
| ACID | Yes | Yes | Yes |
| Zero-copy reads | Yes | No | Yes |
| Concurrent writes | MVCC | Yes | Limited |
| Memory-mapped | Yes | No | Yes |
| Setup complexity | Simple | Moderate | Moderate |
| Best for | Edge/Embedded | High-throughput | Read-heavy |

## Testing

Run unit and integration tests (excludes zenohd tests):

```bash
just test
```

### zenohd Integration Tests

The zenohd integration tests verify the full plugin lifecycle:
- Plugin loading in zenohd
- PUT/GET/DELETE operations
- Wildcard queries (`*` and `**`)
- Data persistence across zenohd restarts

**Requirements:** The tests require zenohd and the storage_manager plugin to be built with the **exact same Zenoh version and Rust compiler** as the redb plugin.

```bash
# Recommended: Use Podman for version-matched testing
just docker-test-zenohd

# Local testing (requires matching zenohd 1.7.0 + plugins in ~/.zenoh/lib/)
just test-zenohd
```

The Docker method builds zenohd and all plugins from source with matching versions, ensuring compatibility.

## Development

```bash
# Install development tools
just install-tools

# Format and lint
just check

# Run all quality checks
just quality

# Pre-commit verification
just verify
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

## Status

This project is currently in **alpha** stage. The API may change as we gather feedback and improve the implementation.
