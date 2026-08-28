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

And one **volume**-level property:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `history` | String | `"latest"` | `"latest"` or `"all"`. See [History](#history-latest-and-all). |

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

## History: `latest` and `all`

By default a storage keeps **one value per key** — the last writer wins. That is
right for state documents, catalogs, and event logs whose records each own a unique
key. It is wrong for anything whose value is the *sequence*: a metric sampled every
five seconds has no "latest" worth keeping alone.

Set `history: "all"` on a volume and every sample is kept, addressed by
`(key, timestamp)`, with a time-ranged GET returning the window.

```json5
volumes: {
  redb: {},                                             // durable · latest
  "redb-history": { backend: "redb", history: "all" },  // durable · all
},
storages: {
  "fleet-latest": {
    key_expr: "v1/*/state/**",
    volume: { id: "redb", dir: "latest" },
  },
  "fleet-timeseries": {
    key_expr: "v1/*/telemetry/**",
    volume: { id: "redb-history", dir: "timeseries" },
  },
}
```

Query a window with Zenoh's `_time` selector parameter — both documented syntaxes
work, including relative expressions:

```
v1/h-3fa9c2d41b7e/telemetry/cpu?_time=[now(-1h)..now()]
v1/h-3fa9c2d41b7e/telemetry/cpu?_time=[2026-08-28T06:00:00Z;1h]
```

Without `_time` the same key returns only its latest sample.

### Why `history` is a volume property and not a storage one

Zenoh asks the **volume** for its capability, and the storage manager makes two
decisions from the answer that an individual storage cannot override:

- A storage declaring `replication` **fails to start** unless its volume reports
  `History::Latest`. Replication works only on latest-value backends.
- In `latest` mode the manager discards outdated samples before they reach the
  backend. In `all` mode it forwards every sample, which is what makes the append
  stream complete.

So a volume reporting `all` cannot host *any* replicated storage. Declaring one
volume per mode — as above — keeps that choice explicit instead of silently
stripping replication from every storage that shares the volume. Both volumes are
served by the same plugin.

### Two things to know before deploying `all`

- **Replication is unavailable** on an `all` volume. Do not configure both.
- **Retention is mandatory.** An `all`-mode storage with no `retention` policy
  **refuses to start**. See below.

Deletions are recorded in the history as tombstones — a deletion is a fact about a
point in time, and dropping it would make the history claim the previous value was
live right up to the next sample. Tombstones are never *replied* to: there is no
value to return.

## Retention

**Zenoh storages have no TTL.** The storage manager's `garbage_collection` prunes
*metadata* — its own in-memory wildcard-update and latest-value caches — and never
touches stored values. So retention is this backend's job, and an `all`-mode storage
without a policy **refuses to start**: a loud config error is recoverable in
seconds, a full disk is not.

```json5
storages: {
  "fleet-timeseries": {
    key_expr: "v1/*/telemetry/**",
    volume: {
      id: "redb-history",
      dir: "timeseries",
      retention: {
        max_age_secs: 2592000,        // 30 d — drop samples older than this
        max_bytes: 10737418240,       // 10 GiB — evict oldest until under
        max_samples_per_key: 100000,  // bound per-key growth independently
        decimate: {                   // optional: thin out older data
          recent_secs: 86400,         // full resolution for a day
          bucket_secs: 300,           // then one sample per 5 min
        },
        interval_secs: 3600,          // how often a pass runs
      },
    },
  },
}
```

| Limit | Effect |
|---|---|
| `max_age_secs` | Drop samples older than this |
| `max_bytes` | Evict oldest samples until the **file** is under this size |
| `max_samples_per_key` | Cap one key's history so a pathological key cannot evict everyone else's |
| `decimate` | Full resolution for `recent_secs`, then one sample per `bucket_secs` |
| `interval_secs` | Seconds between passes (default 3600) |

Every limit is optional and all of them apply — a sample goes if *any* rule says so.
Two shapes are **rejected** rather than accepted quietly, because both read as
protection that is not there: a `retention` block that sets no limit at all, and a
`retention` block on a `latest`-mode volume, which has no history to prune and would
report passes on the admin space while reclaiming nothing.

`max_bytes` bounds the file, but retention only ever deletes *history*. If the
current values and redb's own overhead already exceed the budget, a pass logs that
plainly and stops — it will not delete live data to hit a number.

Decimation is what makes a year of 5-second telemetry affordable, and it is a policy
only the backend can apply: the router's downsampling interceptor shapes *traffic*,
not storage, so it cannot thin data that is already on disk.

### How it runs

A background thread on `interval_secs`, not a check on every write — enforcement on
the PUT path would make every write pay for a scan. (A thread rather than an async
task: `zenohd` does not call a volume's `create_storage` from inside a tokio
runtime, and the work — scanning tables and compacting the file — is blocking
anyway.) Deletions go out in bounded,
committed batches, so a pass never holds the write lock across the whole database,
and reads are never blocked.

`max_bytes` is measured against the **real file** and enforcing it **compacts**.
This matters: redb does not return space to the filesystem when rows are removed, so
without compaction the file size would never fall, the policy would never converge,
and every pass would delete more data while reporting no improvement — silent data
loss dressed up as retention.

Each pass is reported on the admin space (`retention.last_pass`: samples dropped,
bytes before and after, duration), so a policy is verifiable from outside the
process. A storage claiming to be bounded is not the same as a storage that is.

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
