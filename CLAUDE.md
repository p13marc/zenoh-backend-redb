# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Zenoh storage backend using [redb](https://www.redb.org/) as the underlying database engine. It implements the `zenoh_backend_traits` interfaces to provide persistent storage for Zenoh's storage manager plugin. The backend is pure Rust with no C dependencies, ACID-compliant, and supports zero-copy reads.

**Current Zenoh version: 1.7.0** (pinned with `=1.7.0` in Cargo.toml)

## Build Commands

```bash
# Build everything (uses Rust 1.91.1 via rust-toolchain.toml)
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

# Local zenohd tests (requires zenohd 1.7.0 + storage_manager plugin installed)
just test-zenohd

# Run benchmarks
just bench
```

### Integration Test Requirements

The zenohd integration tests require:
1. **zenohd** installed and in PATH (version 1.7.0)
2. **libzenoh_plugin_storage_manager.so** in `~/.zenoh/lib/`
3. **libzenoh_backend_redb.so** in `~/.zenoh/lib/`

All three must be built with the **same Rust version** (1.91.1) and **same Zenoh version** (1.7.0).

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

Thread-local buffers (`KEY_BUFFER`, `VALUE_BUFFER`) are used for zero-allocation PUT/GET operations.

### Wildcard Matching

The `matches_wildcard()` function in storage.rs supports Zenoh key expression wildcards:
- `*` matches a single path segment
- `**` matches zero or more path segments

### Plugin System

The crate builds as both `rlib` (library) and `cdylib` (dynamic plugin). The `plugin` feature enables `zenoh_plugin_trait::declare_plugin!` for dynamic loading by zenohd.

**Critical**: The plugin must be compiled with the exact same Rust version and Zenoh dependency version as zenohd. ABI incompatibility causes SIGSEGV crashes. Feature sets must also match - this is why `zenoh_backend_traits` uses `default-features = false`.

### Plugin Hierarchy

1. **RedbBackendPlugin** -> implements `Plugin`, creates RedbVolume
2. **RedbVolume** -> implements `Volume`, creates RedbStoragePlugin instances
3. **RedbStoragePlugin** -> implements `Storage`, wraps RedbStorage with async mutex

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
| `dir` | string | required | Database directory name (creates `<name>.redb`) |
| `db_file` | string | - | Alternative to dir, explicit filename |
| `create_db` | bool | true | Create database if missing |
| `read_only` | bool | false | Read-only mode |
| `cache_size` | number | redb default | Cache size in bytes |
| `fsync` | bool | true | Enable fsync for durability |

## Environment Variables

- `ZENOH_BACKEND_REDB_ROOT`: Override default storage directory (default: `~/.zenoh/zenoh_backend_redb`)

## Version Compatibility

When updating Zenoh versions:
1. Update all zenoh dependencies in `Cargo.toml` (use `=X.Y.Z` for exact version)
2. Update `rust-toolchain.toml` to match zenohd's Rust version
3. Rebuild storage_manager plugin from same Zenoh version
4. Rebuild and reinstall redb plugin
5. Run `just test-zenohd` to verify compatibility
