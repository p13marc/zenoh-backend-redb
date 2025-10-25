# Integration Tests

This directory contains integration tests for the zenoh-backend-redb storage backend.

## Test Files

### `integration_backend.rs`
Tests the backend layer functionality, including:
- Backend creation and configuration
- Storage instance management
- Multiple storage handling
- Backend lifecycle

### `integration_storage.rs`
Tests the storage layer functionality, including:
- Basic PUT/GET/DELETE operations
- Batch operations
- Wildcard and prefix queries
- Key encoding and decoding
- Storage configuration options

### `integration_plugin.rs`
Tests the plugin layer functionality, including:
- Plugin metadata and constants
- Plugin exports and accessibility
- Feature flag handling

### `integration_zenohd.rs` 🚀 **End-to-End Test**
Full integration test that spawns a real `zenohd` daemon with the redb backend plugin loaded.

**Tests:**
- Complete Zenoh session lifecycle with redb storage
- PUT/GET/DELETE operations through Zenoh protocol
- Prefix queries with wildcard patterns
- Storage persistence across zenohd restarts
- Plugin loading and initialization

**Requirements:**
- `zenohd` must be installed and available in PATH
- Plugin must be built: `cargo build --release --features plugin`
- Tests are marked with `#[ignore]` by default

## Running Tests

### Unit and Integration Tests (without zenohd)
```bash
# Run all tests except ignored ones
cargo test

# Run specific test file
cargo test --test integration_storage

# Run with output
cargo test -- --nocapture
```

### End-to-End Integration Tests (with zenohd)
```bash
# Build the plugin first
cargo build --release --features plugin

# Check plugin exists
cargo test --test integration_zenohd test_plugin_library_exists -- --ignored

# Run the full integration test
cargo test --test integration_zenohd test_zenohd_with_redb_plugin -- --ignored --nocapture

# Run persistence test
cargo test --test integration_zenohd test_zenohd_storage_persistence -- --ignored --nocapture

# Run all zenohd tests
cargo test --test integration_zenohd -- --ignored --nocapture
```

## Installing zenohd

If you don't have `zenohd` installed:

### From Zenoh releases
```bash
# Download from https://github.com/eclipse-zenoh/zenoh/releases
# Or use cargo:
cargo install zenohd --git https://github.com/eclipse-zenoh/zenoh --branch main
```

### From source
```bash
git clone https://github.com/eclipse-zenoh/zenoh.git
cd zenoh
cargo build --release -p zenohd
sudo cp target/release/zenohd /usr/local/bin/
```

### Verify installation
```bash
zenohd --version
```

## Test Architecture

### integration_zenohd.rs - How it works

1. **Plugin Building**: Builds `libzenoh_backend_redb.so` in release mode
2. **Configuration**: Generates a `zenoh-config.json5` with storage_manager plugin config
3. **Daemon Spawn**: Launches `zenohd` as a subprocess with the configuration
4. **Session Connection**: Creates a Zenoh session that connects to the local zenohd
5. **Operations**: Performs PUT/GET/DELETE through Zenoh protocol
6. **Verification**: Validates data is stored and retrieved correctly
7. **Cleanup**: Gracefully terminates zenohd and cleans up temporary files

### Configuration Example

The test generates a configuration similar to:

```json5
{
    plugins: {
        storage_manager: {
            volumes: {
                redb: {
                    __path__: ["/path/to/libzenoh_backend_redb.so"],
                    private: {
                        base_dir: "/tmp/test_storage",
                        create_dir: true
                    }
                }
            },
            storages: {
                test_storage: {
                    key_expr: "test/**",
                    volume: {
                        id: "redb",
                        db_file: "test_db.redb",
                        cache_size: 10485760,
                        fsync: true,
                        strip_prefix: false
                    }
                }
            }
        }
    }
}
```

## Troubleshooting

### "zenohd not found"
- Install zenohd using the instructions above
- Ensure it's in your PATH: `which zenohd`

### "Plugin library not found"
- Build the plugin: `cargo build --release --features plugin`
- Check it exists: `ls -lh target/release/libzenoh_backend_redb.so`

### "Test timed out"
- zenohd might take longer to start on slow systems
- Increase sleep durations in the test if needed
- Check zenohd logs for startup errors

### "Connection refused"
- zenohd might not be listening on the expected port
- Check if another zenohd instance is running: `ps aux | grep zenohd`
- Kill existing instances: `pkill zenohd`

### Tests hang indefinitely
- zenohd might have crashed during startup
- Check for conflicting Zenoh instances
- Run with `--nocapture` to see detailed output

## CI/CD Considerations

For CI pipelines:

```yaml
# Example GitHub Actions snippet
- name: Install zenohd
  run: cargo install zenohd --git https://github.com/eclipse-zenoh/zenoh --branch main

- name: Build plugin
  run: cargo build --release --features plugin

- name: Run integration tests
  run: cargo test --test integration_zenohd -- --ignored --nocapture
  timeout-minutes: 5
```

## Performance Testing

The integration tests can also serve as basic performance benchmarks:

```bash
# Run with timing
cargo test --test integration_zenohd -- --ignored --nocapture --test-threads=1 | grep "time:"
```

For comprehensive performance testing, use the benchmark suite:
```bash
cargo bench --bench storage_benchmarks
```

## Contributing

When adding new integration tests:

1. Keep tests independent and isolated
2. Use temporary directories for storage
3. Clean up resources in Drop implementations
4. Mark tests requiring external dependencies with `#[ignore]`
5. Add documentation explaining test purpose and requirements
6. Handle error cases gracefully with meaningful messages

## Related Documentation

- [PERFORMANCE_ANALYSIS.md](../PERFORMANCE_ANALYSIS.md) - Detailed performance analysis
- [OPTIMIZATIONS.md](../OPTIMIZATIONS.md) - Implemented optimizations
- [README.md](../README.md) - Main project documentation
- [Zenoh Documentation](https://zenoh.io/docs/) - Zenoh protocol and API