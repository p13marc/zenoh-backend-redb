# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Zenoh storage backend using [redb](https://www.redb.org/) as the underlying database engine. It implements the `zenoh_backend_traits` interfaces to provide persistent storage for Zenoh's storage manager plugin. The backend is pure Rust with no C dependencies, ACID-compliant, and supports zero-copy reads.

**Current Zenoh version: 1.10.2** (pinned with `=1.10.2` in Cargo.toml)

## Build Commands

```bash
# Build everything (uses Rust 1.97 via rust-toolchain.toml)
cargo build --all-features

# Build release plugin (creates libzenoh_backend_redb.so)
cargo build --release --features plugin

# Install plugin to ~/.zenoh/lib/
just install-plugin
```

## Testing

```bash
# Run all tests (excludes zenohd integration tests)
just test
# Or directly:
cargo test --all-features -- --skip test_zenohd

# Run a specific test
just test-one TEST_NAME

# Run zenohd integration tests
# RECOMMENDED: Use Podman to ensure version compatibility
just docker-test-zenohd

# Local zenohd tests (requires zenohd 1.10.2 + storage_manager plugin installed)
just test-zenohd

# Run benchmarks
just bench
```

### Integration Test Requirements

The zenohd integration tests require:
1. **zenohd** installed and in PATH (version 1.10.2)
2. **libzenoh_plugin_storage_manager.so** in `~/.zenoh/lib/`
3. **libzenoh_backend_redb.so** in `~/.zenoh/lib/`

All three must be built with the **same Rust version** (1.97) and **same Zenoh version** (1.10.2).

Tests will skip gracefully with a message if plugins are missing or incompatible.

## Linting and Quality

```bash
# Format + clippy check
just check

# Format code
just fmt

# Run all quality checks (coverage, audit, license check, etc.)
just quality

# Pre-commit verification
just verify

# Security audit
just audit

# Check licenses
just deny
```

## Architecture

### Module Structure

| File | Purpose |
|------|---------|
| `lib.rs` | Crate entry point, re-exports public types |
| `backend.rs` | RedbBackend - manages multiple storage instances |
| `storage.rs` | RedbStorage - CRUD operations, wildcard matching |
| `config.rs` | RedbBackendConfig, RedbStorageConfig |
| `error.rs` | Error types (RedbBackendError, Result) |
| `plugin.rs` | Zenoh plugin integration (RedbBackendPlugin, RedbVolume) |

### Storage Design

RedbStorage uses a dual-table architecture (similar to RocksDB column families):
- **payloads table**: Raw payload bytes keyed by Zenoh key expression
- **data_info table**: Metadata (timestamp, encoding, deleted flag) for each key

In `history: "all"` volumes two more tables are written, keyed by a composite
`key || 0x00 || big-endian NTP64 || TimestampId`:

- **history_payloads / history_info**: every sample, not just the latest

The `0x00` separator is safe because a Zenoh key expression can never contain a NUL,
and it sorts below every byte a key can hold — so one key's samples are contiguous
and `a/b` sorts entirely before `a/b/c` instead of interleaving. Big-endian NTP64
makes redb's lexicographic order *be* chronological order, which is what turns a
time window into a single bounded range scan. `data_info` stays the latest-value
index in both modes, which keeps `get_all_entries` O(keys) and a no-`_time` GET a
point lookup.

Thread-local buffers (`KEY_BUFFER`, `VALUE_BUFFER`) are used for zero-allocation PUT/GET operations.

### History and retention

Two features that are easy to miss and hard to rediscover:

- **`history`** is a **volume**-level property (`"latest"` default, or `"all"`).
  It cannot be per-storage, because Zenoh asks the *volume* for its capability and
  makes two decisions from it a storage cannot override: a storage declaring
  `replication` refuses to start unless the volume reports `History::Latest`, and in
  `latest` mode the storage manager discards outdated samples before they reach the
  backend. Declare one volume per mode; the same plugin serves both.
- **`retention`** is per-storage and **mandatory for `all`-mode storages**, which
  refuse to start without it. Zenoh has no TTL — the manager's `garbage_collection`
  prunes its own in-memory metadata, never stored values — so bounding the disk is
  this backend's job.

Three rules the retention code in `storage.rs` exists to keep, each learned the hard
way:

- **Never decide what to delete from the file length.** redb reuses freed pages but
  does not return them to the filesystem, and every PUT is its own copy-on-write
  commit, so a healthy file is two to three times its live data. `max_bytes` is a
  limit on the file, but eviction is sized from the live data (`live_bytes`), and
  compaction — not deletion — is what shrinks the file. The log line reports sample
  counts for the same reason: "N -> N bytes" after a successful pass is normal.
- **Never hold the history in memory.** No `Vec` of every key, no sort. Reads go
  through `walk_history`, which reopens its read transaction every
  `SCAN_ROWS_PER_TXN` rows (a long-lived read transaction stops redb reusing pages,
  so a slow scan makes concurrent PUTs grow the file), and deletes go out in
  `DELETE_BATCH`-row commits. `tests/retention_memory.rs` measures this with a
  counting allocator; it only runs in a release build.
- **Cutoffs saturate** (`ntp_before`). `NTP64 - NTP64` is a bare `u64` subtraction:
  a window longer than the clock wraps in a release build and condemns every sample.

Retention runs on a plain OS thread, **not** a tokio task: `Volume::create_storage`
is not called from inside a tokio runtime in zenohd, and spawning one there panics
and takes down every `all`-mode storage at router startup.

### Wildcard Matching

`matches_wildcard()` in storage.rs defers to Zenoh's own algebra
(`keyexpr::intersects`) rather than splitting on `/`. Two rules a hand-rolled
matcher does not have, and both matter:

- `*` and `**` never match a chunk beginning with `@`. That is the entire basis of
  the verbatim planes (`@rpc`, `@blob`, `@catalog`): `v1/*/state/**` cannot reach
  `v1/@catalog/state/**`, which is why a catalog needs a storage of its own.
- `$*` is a sub-chunk wildcard; treating it as a literal makes a selector silently
  match nothing.

Wildcard and prefix reads are **bounded range scans** over the ordered table, using
the selector's longest wildcard-free prefix (`literal_prefix`). Note the subtlety:
`a/**` also matches `a` itself, which sorts *before* the prefix `a/`, so that key is
probed separately — a scan that just starts at the prefix loses it.

### Plugin System

The crate builds as both `rlib` (library) and `cdylib` (dynamic plugin). The `plugin` feature enables `zenoh_plugin_trait::declare_plugin!` for dynamic loading by zenohd.

**Critical**: The plugin must be compiled with the exact same Rust version and Zenoh
dependency version as zenohd, and both plugins must be built in **one** cargo
workspace so their compiled feature strings unify.

The failure is not a crash — it is worse. zenohd starts, logs a single ERROR line,
and then serves no storage at all. Three distinct causes produce that identical
symptom: separate workspaces ("Incompatible Zenoh feature sets"), a crate shipping
its own `rust-toolchain.toml` ("Incompatible rustc versions"), and a version-skewed
zenohd. See the Version compatibility section of README.md.

### Plugin Hierarchy

1. **RedbBackendPlugin** -> implements `Plugin`, creates RedbVolume
2. **RedbVolume** -> implements `Volume`, creates RedbStoragePlugin instances
3. **RedbStoragePlugin** -> implements `Storage`, holds `Arc<RedbStorage>`

One background OS thread per storage (`spawn_background` in plugin.rs) does everything
that must not happen on the request path: retention passes, flushing an `fsync: false`
storage, and computing `StorageUsage`. **`get_admin_status` must stay O(1)**: it is
synchronous, it runs for every admin-space query, and redb's `Table::stats()` walks
every page of the table. `RedbStorage::stats()` is the free half; `usage()` reads the
whole database and is only called inline for files up to 64 MiB.

Nothing in the plugin layer takes a lock: every `RedbStorage` method takes `&self` and
redb does its own concurrency control. `put`/`delete` do their last-writer-wins
comparison and the write inside one redb write transaction, and redb admits one writer
at a time, which is what makes that comparison atomic.

## Configuration

Backend is configured through Zenoh's storage_manager plugin:

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

### Storage Volume Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `dir` | string | required | Database name; the file is `<dir>.redb`, with `.redb` *appended* (a dot in `dir` is kept) |
| `db_file` | string | - | Alternative to dir, explicit filename |
| `create_db` | bool | true | Create database if missing |
| `read_only` | bool | false | Read-only mode |
| `cache_size` | number | `67108864` (64 MiB) | redb page-cache budget in bytes (never redb’s own 1 GiB default) |
| `fsync` | bool | true | `Durability::Immediate` vs `Durability::None`. redb 4 has no level between, and a non-durable commit is not persisted *at all* until a durable one follows, so with `false` the backend promotes a commit to durable every `flush_interval_secs` |
| `flush_interval_secs` | number | `5` | With `fsync: false`, the longest a write stays unpersisted (and the longest redb accumulates per-commit memory). Ignored with `fsync: true` |
| `retention` | object | - | Required for `all`-mode storages. `max_age_secs`, `max_bytes`, `max_samples_per_key`, `decimate`, `interval_secs` |

Volume-level:

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `history` | string | `"latest"` | `"latest"` or `"all"` — see History and retention above |

## Environment Variables

- `ZENOH_BACKEND_REDB_ROOT`: Override default storage directory (default: `~/.zenoh/zenoh_backend_redb`)

## Version Compatibility

When updating Zenoh versions:
1. Update all zenoh dependencies in `Cargo.toml` (use `=X.Y.Z` for exact version)
2. Update `rust-toolchain.toml` to match zenohd's Rust version
3. Rebuild storage_manager plugin from same Zenoh version
4. Rebuild and reinstall redb plugin
5. Run `just test-zenohd` to verify compatibility
