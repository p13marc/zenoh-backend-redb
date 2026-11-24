# Zenoh Backend redb

[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

A [Zenoh](https://zenoh.io) storage backend using [redb](https://www.redb.org/) as the underlying database engine.

## Overview

This backend provides persistent storage for Zenoh using redb, a pure Rust embedded key-value database with ACID compliance and zero-copy reads. It's particularly well-suited for edge computing, IoT devices, and applications requiring a lightweight, dependency-free storage solution.

**Compatible with Zenoh 1.10.2**

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
| `dir` | String | required | Database name: the file is `<dir>.redb` under the backend root. A dot in the name is kept (`telemetry.v1` → `telemetry.v1.redb`). Must stay inside the root: relative, no `..`. |
| `db_file` | String | - | Alternative to `dir`: the file name as given (`.redb` is added only if it has no extension). Same rule: inside the root. |
| `cache_size` | Number | `67108864` (64 MiB) | redb page-cache budget in bytes. See [Why `cache_size` has a default](#why-cache_size-has-a-default). |
| `fsync` | Boolean | `true` | Every commit reaches the disk before it returns. See [Durability](#durability). |
| `flush_interval_secs` | Number | `5` | With `fsync: false`, the longest a write stays unpersisted. Ignored with `fsync: true`. |
| `create_db` | Boolean | `true` | Create database if it doesn't exist |
| `read_only` | Boolean | `false` | Open the database without write access. See [Read-Only Storage](#read-only-storage). |

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
| Zenoh | `1.10.2` (pinned `=1.10.2` in `Cargo.toml`, all five zenoh crates) |
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
3. **A version-skewed `zenohd`.** `cargo install zenohd --version 1.10.2 --locked`, and
   confirm with `zenohd --version` before blaming the backend.

### Why `cache_size` has a default

`cache_size` defaults to **64 MiB** and is always passed to redb explicitly. This
backend never inherits redb's own default, which is **1 GiB**.

That is a deliberate refusal, not a tuning preference. On a small guest an unbounded
page cache presents as a slow multi-day RSS climb that ends at the OOM killer, with
nothing in any log connecting it to a storage setting. Raise it on a host with memory to
spare (read-heavy: 200 MiB+); lower it on an edge node (10–50 MiB).

### Durability

`fsync: true` (the default) maps to redb's `Durability::Immediate`: when a commit
returns, the data is on disk. Every sample is one commit, so every sample is one
`fsync`.

`fsync: false` trades that for throughput, and the trade has a bound:
**a crash loses at most `flush_interval_secs` (default 5) of writes.**

The bound is this backend's doing, not redb's. `fsync: false` maps to
`Durability::None`, and redb has no level in between: a non-durable commit is
**not persisted at all** until a durable one follows, and redb holds memory for
each one until then. Left alone, a storage that never issued a durable commit would
lose *everything written since it started* to a kill, and grow without limit while
it ran. So with `fsync: false` the backend makes a commit durable whenever
`flush_interval_secs` has passed since the last durable one, and a background thread
flushes the tail when writes stop.

A clean shutdown always persists everything, in either mode.

### On-disk format and upgrades

A database carries a format version (a `meta` table), and this build writes
**format 2**.

| Format | Written by | Opened by this version |
|---|---|---|
| 1 | 0.4, 0.5 | Yes: **migrated in place**, automatically, the first time it is opened for writing |
| 2 | 0.6 | Yes |
| newer | a later version | No: refused, with an error naming both formats |

The migration is one transaction: after a crash the file is either still format 1 or
fully format 2. It only moves the sample stored under a storage's bare `strip_prefix`
(see below), so it is quick. **It is one-way, and 0.5 will not tell you so**: 0.5
knows nothing of the format stamp, so it opens a migrated file without complaint and
then looks for that sample under the old name. It does not find it, and logs the
empty key as invalid whenever it lists the storage. Everything else reads normally,
which makes the loss easy to miss. Take a copy of the database before upgrading if
you may need to go back. A `read_only` storage cannot migrate; it asks for the
database to be opened writable once.

What changed in format 2: a sample whose key is exactly the storage's `strip_prefix`
has no key left after stripping. It used to be stored under `@@none_key@@`, which is
itself a valid key, so `<prefix>/@@none_key@@` landed on the same row. It is now
stored under the empty string, which no key expression can be.

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
| `max_bytes` | Keep the **file** under this size: compact it, and evict the oldest samples if the data itself is too big |
| `max_samples_per_key` | Cap one key's history so a pathological key cannot evict everyone else's |
| `decimate` | Full resolution for `recent_secs`, then one sample per `bucket_secs` |
| `interval_secs` | Seconds between passes (default 3600) |

Every limit is optional and all of them apply — a sample goes if *any* rule says so.
Some shapes are **rejected at startup** rather than accepted quietly, because each
reads as protection that is not there:

- a `retention` block that sets no limit at all;
- a `retention` block on a `latest`-mode volume, which has no history to prune and
  would report passes on the admin space while reclaiming nothing;
- an unknown field. `max_byte: 10737418240` is a typo, not "no size limit";
- `interval_secs: 0` or `decimate.bucket_secs: 0`.

A window longer than the clock (`max_age_secs: 3153600000`, "100 years") is fine and
means what it says: nothing is that old, so nothing is dropped.

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
anyway.)

A pass costs what it removes, not what the storage holds:

- It reads the history in short read transactions and deletes in committed batches
  of 4096 rows, so it never holds more than one batch of keys in memory and never
  holds the write lock across the whole database.
- A key's samples are stored oldest-first, so with `max_age_secs` a pass reads the
  rows it deletes plus one row per key, then skips to the next key.

### What the file size does, and does not, tell you

**redb does not return space to the filesystem when rows are removed.** It reuses the
freed pages for later writes. So after a pass that dropped half a million samples the
file is *exactly* as long as before, and the log line says so:

```
Retention pass on 'timeseries': dropped 453480 samples (1903211 -> 1449731),
file 7877742592 -> 7877742592 bytes (not compacted: freed pages are reused, the
file only shrinks on compaction)
```

The sample counts are the evidence that the pass worked. A storage at a steady
ingest rate drops about the same number every pass — that is the rate at which
samples age out, not the same samples being dropped again.

For the same reason a healthy file is routinely two to three times its live data:
every PUT is its own copy-on-write commit.

### `max_bytes`

`max_bytes` is a limit on the **real file**. A file is bigger than its data in two
layers — pages nothing uses any more, and free space inside the pages that are used
(a page that has just split is half empty) — so a pass that finds the file over the
limit pulls its levers in order of what they cost, and deletes last:

1. **compacts**, which returns the unused pages;
2. if what is stored fits the limit, **compacts thoroughly**, which packs the
   half-empty pages (redb's `CompactionMode::Maximum`);
3. only if the file is still over, **evicts the oldest samples**, across all keys, as
   many as the excess calls for (to about 90 % of the limit, so the next few minutes
   of writes do not put it straight back over), and packs again.

A file that is over the limit merely because it is loosely packed loses no samples.
If compaction cannot run, nothing is deleted on the strength of the file length.

Compaction needs the database to itself: reads and writes on that storage wait while
it runs. A thorough one also leaves pages full, so writes are a little slower until
they split again. Leave headroom in `max_bytes` (twice the data you expect to hold is
a reasonable start) so that a pass rarely has to do either.

Without `max_bytes` the file is never compacted. It stops growing once freed pages
cover new writes, at its high-water mark.

Each pass is reported on the admin space, so a policy is verifiable from outside the
process — a storage claiming to be bounded is not the same as a storage that is:

```json5
retention: {
  policy: { max_age_secs: 2592000, max_bytes: 10737418240, interval_secs: 3600 },
  last_pass: {
    samples_dropped:          453480,
    samples_evicted_for_size:      0,   // non-zero: max_bytes, not age, is the bound
    samples_before:          1903211,
    samples_after:           1449731,
    bytes_before:         7877742592,   // the file; only moves when `compacted`
    bytes_after:          7877742592,
    compacted:                 false,
    duration_ms:                 812,
  },
}
```

## What a storage reports about itself

Each storage publishes its configuration *and* its cost on Zenoh's admin space,
under `@/<zid>/router/status/plugins/storage_manager/**`. `zenctl storage list` and
the GUI's storage panel read it; so can any `GET`.

```json5
{
  capability: { persistence: "durable", history: "latest" },
  db_path: "/var/lib/zenoh/redb/telemetry.redb",
  health: {
    state: "ok",                  // or "recovering": see "When the disk fills up"
    last_error: null,             // what took it out of service, kept after recovery
    failed_at: null,              // seconds since the epoch
    reopens: 0,                   // times the database was reopened after a failure
  },
  stats: {
    // Free to read, computed for every query:
    on_disk_bytes:    41947136,   // the real file, from the filesystem
    key_count:           10240,
    sample_count:            0,   // history rows; 0 on a `latest` volume
    // Need the tables read; see below:
    stored_bytes:     33554432,   // keys + values actually inserted
    metadata_bytes:    1048576,   // btree branch keys and redb metadata
    fragmented_bytes:  7344128,   // unused space inside pages
    live_keys:           10100,
    tombstones:            140,
    oldest_timestamp: "...",
    newest_timestamp: "...",
    usage_age_secs:          0,   // how old those seven are
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
`cache_size` being smaller than the working set.

`stored_bytes + metadata_bytes + fragmented_bytes` is the live data: what the file
would be after a compaction. The gap from there to `on_disk_bytes` is freed pages
waiting to be reused.

**An admin query never reads the database.** The seven "usage" fields can only be
computed by walking every page of every table, which on a multi-gigabyte storage is a
multi-gigabyte read. So:

- up to 64 MiB on disk, they are computed for the query and `usage_age_secs` is `0`;
- above that, they come from a snapshot. A query that finds it missing or more than
  60 s old asks a background thread for a new one and returns what it has, so the
  first query after startup shows them as `null` and the next shows them filled in.

## When the disk fills up

Retention exists so that this backend does not fill a disk. Something else on the
same filesystem still can, and then a write to the database fails.

redb is strict about that: after one failed write it refuses every further operation
on that database until it has been closed and reopened. So the storage does exactly
that, by itself:

1. the failed write is reported, and the storage logs **one** ERROR line saying it is
   out of service and why;
2. while it is, every PUT, DELETE and GET fails immediately with an error that says
   the storage is recovering (the storage manager logs those per sample; this backend
   does not add to them);
3. its background thread tries to reopen the database after 1 s, then 2 s, 4 s … up to
   every 30 s;
4. the first attempt that succeeds puts it back in service, with one INFO line.

No router restart, and nothing to do but free the space. What was committed before
the failure is there afterwards. With `fsync: false`, writes that had not been flushed
yet are lost, as after a crash.

`health` on the admin space (above) shows where a storage is in this, and `reopens`
that it happened at all.

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

A read-only storage opens its database without write access, through redb's
read-only handle. That is a property of how the file is opened, not a flag checked
before each PUT:

- **the file is not modified**, not even at startup — it can be on read-only media,
  or owned by someone else;
- **any number of read-only storages can share one file**, in one router or several;
  a writable storage and a read-only one cannot;
- the database **must already exist**, be this backend's, and have been closed
  cleanly: a read-only storage cannot create tables or repair a file;
- PUT and DELETE are refused, and retention, flushing and compaction do not run.

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
zenohd
  └─ storage_manager plugin      subscribes, strips prefixes, resolves wildcards
       └─ RedbVolume             one per `volumes` entry; fixes the history mode
            └─ RedbStoragePlugin   one per storage; implements zenoh's `Storage`
                 ├─ RedbStorage      one redb file: latest values, history, retention
                 └─ background thread  retention, flushing, usage snapshots
```

`RedbStorage` is the engine and can be used without `zenohd`
(`cargo run --example basic_usage`). Everything else is the glue that makes it a
zenoh storage backend.

## Comparison with Other Backends

| Feature | redb | RocksDB | LMDB |
|---------|------|---------|------|
| Pure Rust | Yes | No (C++) | No (C) |
| ACID | Yes | Yes | Yes |
| Zero-copy reads | Yes | No | Yes |
| Concurrent writes | MVCC | Yes | Limited |
| Setup complexity | Simple | Moderate | Moderate |
| Best for | Edge/Embedded | High-throughput | Read-heavy |

## Testing

Run unit and integration tests (excludes zenohd tests):

```bash
just test
```

### Conformance

`just conformance` runs the router-storage conformance suite against a real
`zenohd` with a redb volume under it. It asserts the contract an operator actually
depends on — a state doc outliving its publisher, a DELETE retiring it, `*` not
reaching a `@catalog` chunk, blob chunks surviving their sensor, a fleet `@rpc` GET
still fanning in, every event record surviving — plus the two cases a latest-value
backend cannot pass at all: a `_time`-ranged GET and a retention pass.

The first seven say nothing specific to redb; a correct backend of any kind passes
them. That is the point: they are the cheapest proof that swapping the backend
changes nothing an operator relies on. The suite runs in CI and a failure blocks
merge.

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

# Local testing (requires matching zenohd 1.10.2 + plugins in ~/.zenoh/lib/)
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

**Beta.** The storage contract is verified end-to-end against a real `zenohd` on
every change: `tests/conformance_router_storage.rs` spawns a router and asserts that
a state doc outlives its publisher, a DELETE retires it, `*` cannot reach a
`@catalog` chunk, blob chunks survive their sensor, a fleet `@rpc` GET still fans
in, every event record survives, plus a `_time`-ranged GET and a retention pass. A
failure blocks merge.

The Rust API is still moving — 0.4 changed `RedbStorage::put`/`delete` signatures —
and `history: "all"` has not yet run in a production deployment. Treat the plugin
interface and on-disk format as stable, and the library API as not.
