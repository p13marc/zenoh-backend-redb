# Zenoh Backend redb

[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

A [Zenoh](https://zenoh.io) storage backend using [redb](https://www.redb.org/) as the underlying database engine.

## Overview

This backend provides persistent storage for Zenoh using redb, a pure Rust embedded key-value database with ACID compliance and zero-copy reads. It's particularly well-suited for edge computing, IoT devices, and applications requiring a lightweight, dependency-free storage solution.

**Compatible with Zenoh 1.10.0**

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
| `cache_size` | Number | `67108864` (64 MiB) | redb page-cache budget in bytes. See [Why `cache_size` has a default](#why-cache_size-has-a-default). |
| `fsync` | Boolean | `true` | Enable fsync for durability |
| `create_db` | Boolean | `true` | Create database if it doesn't exist |
| `read_only` | Boolean | `false` | Read-only mode |

### Environment Variables

- `ZENOH_BACKEND_REDB_ROOT`: Override default storage directory (default: `~/.zenoh/zenoh_backend_redb`)

## Version compatibility

This plugin is a `cdylib` loaded into `zenohd`, and Zenoh checks compatibility by
comparing a `Compatibility` struct built from the **exact rustc version**, the
**compiled feature set** and the **struct versions**. A mismatch is not a link error and
not a crash you can read:

> **zenohd starts, logs a single ERROR line, and then serves no storage at all.**

Every GET returns nothing and every PUT is dropped, with a healthy-looking router. So
the version requirements below are not advisory.

| | Must match zenohd |
|---|---|
| Zenoh | `1.10.0` (pinned `=1.10.0` in `Cargo.toml`, all five zenoh crates) |
| rustc | `1.97` (`rust-toolchain.toml`) |

### The three traps

All three produce the identical symptom above.

1. **Plugins built in separate workspaces.** `zenoh-plugin-storage-manager` takes
   `zenoh_backend_traits` with `default-features = false`; other backends take it with
   defaults. Built separately, their compiled feature strings differ and the
   compatibility check rejects the pair — *"Incompatible Zenoh feature sets"*. Build the
   storage manager and this backend in **one** cargo workspace so features unify. The
   `Dockerfile` here does that; `just docker-test-zenohd` is the supported path.
2. **A crate shipping its own `rust-toolchain.toml`.** It will pin a different rustc
   than the one `zenohd` was built with — *"Incompatible rustc versions"*. Remove it
   before building, or build everything with the host toolchain.
3. **A version-skewed `zenohd`.** `cargo install zenohd --version 1.10.0 --locked`, and
   confirm with `zenohd --version` before blaming the backend.

### Why `cache_size` has a default

`cache_size` defaults to **64 MiB** and is always passed to redb explicitly. This
backend never inherits redb's own default, which is **1 GiB** in redb 4.

That is a deliberate refusal, not a tuning preference. On a small guest an unbounded
page cache presents as a slow multi-day RSS climb that ends at the OOM killer, with
nothing in any log connecting it to a storage setting. Raise it on a host with memory to
spare (read-heavy: 200 MiB+); lower it on an edge node (10–50 MiB).

### Durability

`fsync: true` (the default) maps to redb's `Durability::Immediate`: when a commit
returns, the data is on disk. `fsync: false` maps to `Durability::None` — redb 4 removed
the intermediate `Eventual` level, so the trade is sharper than the name suggests:
commits are **not persisted at all** until some later durable commit lands. Reasonable
for a cache or a replayable stream; wrong for a system of record.

## What a storage reports about itself

Each storage publishes its configuration *and* its cost on Zenoh's admin space,
under `@/<zid>/router/status/plugins/storage_manager/**`. `zenctl storage list` and
the GUI's storage panel read it; so can any `GET`.

```json5
{
  capability: { persistence: "durable", history: "latest" },
  db_path: "/var/lib/zenoh/redb/telemetry.redb",
  stats: {
    on_disk_bytes:    41947136,   // the real file, from the filesystem
    stored_bytes:     33554432,   // keys + values actually inserted
    metadata_bytes:    1048576,   // btree branch keys and redb metadata
    fragmented_bytes:  7344128,   // what a compaction could reclaim
    key_count:           10240,
    live_keys:           10100,
    tombstones:            140,
    oldest_timestamp: "...",
    newest_timestamp: "...",
    cache: {
      size_bytes:     67108864,
      used_bytes:     41943040,
      read_hits:        982341,
      read_misses:        1204,
      hit_ratio:      0.998777,   // absent until something has been read
      evictions:             0,
    },
  },
}
```

Sizes come from redb and the filesystem, never from adding up key and value
lengths. The gap between `stored_bytes` and `on_disk_bytes` *is* the overhead an
operator needs to see, and an estimate would hide exactly that.

The number to watch is `evictions` climbing while `hit_ratio` sits flat: that is
`cache_size` being smaller than the working set. `fragmented_bytes` growing without
bound is the case for a compaction.

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

# Local testing (requires matching zenohd 1.10.0 + plugins in ~/.zenoh/lib/)
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
