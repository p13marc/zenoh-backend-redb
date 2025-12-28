# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Zenoh storage backend using [redb](https://www.redb.org/) as the underlying database engine. It implements the `zenoh_backend_traits` interfaces to provide persistent storage for Zenoh's storage manager plugin. The backend is pure Rust with no C dependencies, ACID-compliant, and supports zero-copy reads.

## Build Commands

```bash
# Build everything (uses Rust 1.85.0 via rust-toolchain.toml)
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
cargo test --all-features -- --skip test_zenohd --skip test_plugin_library_exists

# Run a specific test
just test-one TEST_NAME
# Or:
cargo test --all-features TEST_NAME

# Run zenohd integration tests (requires matching zenohd version)
# RECOMMENDED: Use Podman to ensure version compatibility
just docker-test-zenohd

# Local zenohd tests (requires zenohd 1.7.1 installed)
just test-zenohd

# Run benchmarks
just bench
```

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

# Security audit (ignores known upstream Zenoh vulnerabilities)
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

**Critical**: The plugin must be compiled with the exact same Rust version and Zenoh dependency version as zenohd. ABI incompatibility causes SIGSEGV crashes.

### Plugin Hierarchy

1. **RedbBackendPlugin** → implements `Plugin`, creates RedbVolume
2. **RedbVolume** → implements `Volume`, creates RedbStoragePlugin instances
3. **RedbStoragePlugin** → implements `Storage`, wraps RedbStorage with async mutex

## Configuration

Backend is configured through Zenoh's storage_manager plugin:

```json5
{
  plugins: {
    storage_manager: {
      volumes: {
        redb: {
          base_dir: "./zenoh_redb_storage"
        }
      },
      storages: {
        demo: {
          key_expr: "demo/example/**",
          strip_prefix: "demo/example",
          volume: {
            id: "redb",
            db_file: "demo_storage",
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
| `db_file` | string | required | Database filename |
| `dir` | string | - | Alternative to db_file, directory name |
| `create_db` | bool | true | Create database if missing |
| `read_only` | bool | false | Read-only mode |
| `cache_size` | number | redb default | Cache size in bytes |
| `fsync` | bool | true | Enable fsync for durability |

## Environment Variables

- `ZENOH_BACKEND_REDB_ROOT`: Override default storage directory (default: `~/.zenoh/zenoh_backend_redb`)
