use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zenoh::bytes::Encoding;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use tempfile::TempDir;
use zenoh_backend_redb::{RedbBackend, RedbBackendConfig, RedbStorageConfig, StoredValue};

/// Helper to create a temporary backend for benchmarks
fn create_test_backend() -> (RedbBackend, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let config = RedbBackendConfig::new()
        .with_base_dir(temp_dir.path().to_path_buf())
        .with_create_dir(true);
    let backend = RedbBackend::new(config).unwrap();
    (backend, temp_dir)
}

/// Helper to create a stored value with a given size
fn create_value(size: usize, timestamp: u64) -> StoredValue {
    let payload = vec![0u8; size];
    StoredValue::new(payload, Timestamp::new(NTP64(timestamp), TimestampId::rand()), Encoding::ZENOH_BYTES)
}

/// Benchmark single PUT operations with varying payload sizes
fn bench_put_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("put_operations");

    for size in [100, 1_000, 10_000, 100_000, 1_000_000].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let (backend, _temp_dir) = create_test_backend();
            let storage = backend
                .create_storage("bench_storage".to_string(), None)
                .unwrap();
            let mut counter = 0u64;

            b.iter(|| {
                let key = format!("test/key/{}", counter);
                let value = create_value(size, counter);
                counter += 1;
                storage.put(&key, black_box(value)).unwrap();
            });
        });
    }
    group.finish();
}

/// Benchmark single GET operations after population
fn bench_get_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_operations");

    for size in [100, 1_000, 10_000, 100_000, 1_000_000].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let (backend, _temp_dir) = create_test_backend();
            let storage = backend
                .create_storage("bench_storage".to_string(), None)
                .unwrap();

            // Pre-populate with data
            for i in 0..100 {
                let key = format!("test/key/{}", i);
                let value = create_value(size, i);
                storage.put(&key, value).unwrap();
            }

            let mut counter = 0;
            b.iter(|| {
                let key = format!("test/key/{}", counter % 100);
                counter += 1;
                black_box(storage.get(&key).unwrap());
            });
        });
    }
    group.finish();
}

/// Benchmark DELETE operations
fn bench_delete_operations(c: &mut Criterion) {
    let (backend, _temp_dir) = create_test_backend();
    let storage = backend
        .create_storage("bench_storage".to_string(), None)
        .unwrap();

    c.bench_function("delete_single", |b| {
        b.iter_batched(
            || {
                // Setup: insert a key
                let key = format!("test/key/{}", rand::random::<u64>());
                let value = create_value(1000, 0);
                storage.put(&key, value).unwrap();
                key
            },
            |key| {
                // Benchmark: delete the key
                storage.delete(black_box(&key)).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark batch PUT operations
fn bench_batch_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_put");

    for batch_size in [10, 100, 1_000, 10_000].iter() {
        group.throughput(Throughput::Elements(*batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            batch_size,
            |b, &batch_size| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                b.iter(|| {
                    for i in 0..batch_size {
                        let key = format!("test/batch/{}", i);
                        let value = create_value(1000, i as u64);
                        storage.put(&key, black_box(value)).unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark batch PUT operations vs individual PUTs
fn bench_batch_put_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_put_comparison");

    for batch_size in [10, 100, 1_000].iter() {
        group.throughput(Throughput::Elements(*batch_size as u64));

        // Benchmark individual puts
        group.bench_with_input(
            BenchmarkId::new("individual", batch_size),
            batch_size,
            |b, &batch_size| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                b.iter(|| {
                    for i in 0..batch_size {
                        let key = format!("test/batch/{}", i);
                        let value = create_value(1000, i as u64);
                        storage.put(&key, black_box(value)).unwrap();
                    }
                });
            },
        );

        // Benchmark batch put
        group.bench_with_input(
            BenchmarkId::new("batch", batch_size),
            batch_size,
            |b, &batch_size| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                b.iter(|| {
                    let entries: Vec<_> = (0..batch_size)
                        .map(|i| {
                            let key = format!("test/batch/{}", i);
                            let value = create_value(1000, i as u64);
                            (key, value)
                        })
                        .collect();

                    let entries_ref: Vec<_> = entries
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.clone()))
                        .collect();

                    storage.put_batch(black_box(entries_ref)).unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Benchmark prefix queries with varying result set sizes
fn bench_prefix_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("prefix_queries");

    for num_results in [10, 100, 1_000, 10_000].iter() {
        group.throughput(Throughput::Elements(*num_results as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_results),
            num_results,
            |b, &num_results| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                // Pre-populate with data
                for i in 0..num_results {
                    let key = format!("test/prefix/sensor/{}", i);
                    let value = create_value(100, i as u64);
                    storage.put(&key, value).unwrap();
                }

                // Add some noise data with different prefix
                for i in 0..1000 {
                    let key = format!("test/other/{}", i);
                    let value = create_value(100, i as u64);
                    storage.put(&key, value).unwrap();
                }

                b.iter(|| {
                    black_box(storage.get_by_prefix("test/prefix/").unwrap());
                });
            },
        );
    }
    group.finish();
}

/// Benchmark wildcard queries with single-segment wildcard (*)
fn bench_wildcard_single_segment(c: &mut Criterion) {
    let mut group = c.benchmark_group("wildcard_single_segment");

    for num_results in [10, 100, 1_000].iter() {
        group.throughput(Throughput::Elements(*num_results as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_results),
            num_results,
            |b, &num_results| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                // Pre-populate with matching data
                for i in 0..num_results {
                    let key = format!("test/sensor{}/temperature", i);
                    let value = create_value(100, i as u64);
                    storage.put(&key, value).unwrap();
                }

                b.iter(|| {
                    black_box(storage.get_by_wildcard("test/*/temperature").unwrap());
                });
            },
        );
    }
    group.finish();
}

/// Benchmark wildcard queries with multi-segment wildcard (**)
fn bench_wildcard_multi_segment(c: &mut Criterion) {
    let mut group = c.benchmark_group("wildcard_multi_segment");

    for num_results in [10, 100, 1_000].iter() {
        group.throughput(Throughput::Elements(*num_results as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_results),
            num_results,
            |b, &num_results| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                // Pre-populate with matching data at various depths
                for i in 0..num_results {
                    let depth = i % 3;
                    let key = match depth {
                        0 => format!("test/sensor{}", i),
                        1 => format!("test/room/sensor{}", i),
                        _ => format!("test/building/floor/sensor{}", i),
                    };
                    let value = create_value(100, i as u64);
                    storage.put(&key, value).unwrap();
                }

                b.iter(|| {
                    black_box(storage.get_by_wildcard("test/**").unwrap());
                });
            },
        );
    }
    group.finish();
}

/// Benchmark get_all operations
fn bench_get_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_all");

    for total_entries in [100, 1_000, 10_000].iter() {
        group.throughput(Throughput::Elements(*total_entries as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(total_entries),
            total_entries,
            |b, &total_entries| {
                let (backend, _temp_dir) = create_test_backend();
                let storage = backend
                    .create_storage("bench_storage".to_string(), None)
                    .unwrap();

                // Pre-populate with data
                for i in 0..total_entries {
                    let key = format!("test/entry/{}", i);
                    let value = create_value(100, i as u64);
                    storage.put(&key, value).unwrap();
                }

                b.iter(|| {
                    black_box(storage.get_all().unwrap());
                });
            },
        );
    }
    group.finish();
}

/// Benchmark concurrent read operations
fn bench_concurrent_reads(c: &mut Criterion) {
    use std::sync::Arc;
    use std::thread;

    c.bench_function("concurrent_reads_4_threads", |b| {
        b.iter_batched(
            || {
                // Setup: create storage and populate
                let (backend, temp_dir) = create_test_backend();
                let storage = Arc::new(
                    backend
                        .create_storage("bench_storage".to_string(), None)
                        .unwrap(),
                );

                // Pre-populate with data
                for i in 0..1000 {
                    let key = format!("test/key/{}", i);
                    let value = create_value(1000, i);
                    storage.put(&key, value).unwrap();
                }

                (storage, temp_dir)
            },
            |(storage, _temp_dir)| {
                // Benchmark: concurrent reads
                let mut handles = vec![];
                for thread_id in 0..4 {
                    let storage_clone = Arc::clone(&storage);
                    let handle = thread::spawn(move || {
                        for i in 0..25 {
                            let key = format!("test/key/{}", (thread_id * 25 + i) % 1000);
                            black_box(storage_clone.get(&key).unwrap());
                        }
                    });
                    handles.push(handle);
                }
                for handle in handles {
                    handle.join().unwrap();
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark storage with fsync enabled vs disabled
fn bench_fsync_impact(c: &mut Criterion) {
    let mut group = c.benchmark_group("fsync_impact");

    for fsync_enabled in [false, true].iter() {
        let label = if *fsync_enabled {
            "with_fsync"
        } else {
            "without_fsync"
        };
        group.bench_function(label, |b| {
            let (backend, _temp_dir) = create_test_backend();
            let config = RedbStorageConfig::new().with_fsync(*fsync_enabled);
            let storage = backend
                .create_storage("bench_storage".to_string(), Some(config))
                .unwrap();

            let mut counter = 0u64;
            b.iter(|| {
                let key = format!("test/key/{}", counter);
                let value = create_value(1000, counter);
                counter += 1;
                storage.put(&key, black_box(value)).unwrap();
            });
        });
    }
    group.finish();
}

/// Benchmark storage with prefix stripping
fn bench_prefix_stripping(c: &mut Criterion) {
    let mut group = c.benchmark_group("prefix_stripping");

    for strip_prefix in [false, true].iter() {
        let label = if *strip_prefix {
            "with_strip"
        } else {
            "without_strip"
        };
        group.bench_function(label, |b| {
            let (backend, _temp_dir) = create_test_backend();
            let mut config =
                RedbStorageConfig::new().with_key_expr("test/long/prefix/path/".to_string());
            if *strip_prefix {
                config = config.with_strip_prefix(true);
            }
            let storage = backend
                .create_storage("bench_storage".to_string(), Some(config))
                .unwrap();

            let mut counter = 0u64;
            b.iter(|| {
                let key = format!("test/long/prefix/path/sensor/{}", counter);
                let value = create_value(100, counter);
                counter += 1;
                storage.put(&key, black_box(value)).unwrap();
            });
        });
    }
    group.finish();
}

/// Benchmark key encoding/decoding overhead
fn bench_key_operations(c: &mut Criterion) {
    c.bench_function("key_encoding", |b| {
        let mut counter = 0;
        b.iter(|| {
            let key = format!("test/sensor/{}/temperature/reading", counter);
            counter += 1;
            black_box(key.as_bytes());
        });
    });
}

/// Benchmark bulk get_many operations vs individual gets
fn bench_get_many_vs_individual(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_many_comparison");

    for count in [10, 50, 100, 500].iter() {
        // Setup: Create storage with keys
        let (backend, _temp_dir) = create_test_backend();
        let storage = backend
            .create_storage("bench_storage".to_string(), None)
            .unwrap();

        let keys: Vec<String> = (0..*count).map(|i| format!("test/key/{}", i)).collect();
        for key in &keys {
            let value = create_value(1000, 100);
            storage.put(key, value).unwrap();
        }

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();

        // Benchmark get_many
        group.bench_with_input(BenchmarkId::new("get_many", count), count, |b, _| {
            b.iter(|| {
                black_box(storage.get_many(key_refs.clone()).unwrap());
            });
        });

        // Benchmark individual gets
        group.bench_with_input(BenchmarkId::new("individual_gets", count), count, |b, _| {
            b.iter(|| {
                for key in &key_refs {
                    black_box(storage.get(key).unwrap());
                }
            });
        });
    }
    group.finish();
}

/// Benchmark bulk delete_many operations vs individual deletes
fn bench_delete_many_vs_individual(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete_many_comparison");

    for count in [10, 50, 100, 500].iter() {
        // Benchmark delete_many
        group.bench_with_input(
            BenchmarkId::new("delete_many", count),
            count,
            |b, &count| {
                b.iter_batched(
                    || {
                        // Setup: Create fresh storage with keys
                        let (backend, temp_dir) = create_test_backend();
                        let storage = backend
                            .create_storage("bench_storage".to_string(), None)
                            .unwrap();

                        let keys: Vec<String> =
                            (0..count).map(|i| format!("test/key/{}", i)).collect();
                        for key in &keys {
                            let value = create_value(1000, 100);
                            storage.put(key, value).unwrap();
                        }

                        (storage, keys, temp_dir)
                    },
                    |(storage, keys, _temp_dir)| {
                        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
                        black_box(storage.delete_many(key_refs.clone()).unwrap());
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );

        // Benchmark individual deletes
        group.bench_with_input(
            BenchmarkId::new("individual_deletes", count),
            count,
            |b, &count| {
                b.iter_batched(
                    || {
                        // Setup: Create fresh storage with keys
                        let (backend, temp_dir) = create_test_backend();
                        let storage = backend
                            .create_storage("bench_storage".to_string(), None)
                            .unwrap();

                        let keys: Vec<String> =
                            (0..count).map(|i| format!("test/key/{}", i)).collect();
                        for key in &keys {
                            let value = create_value(1000, 100);
                            storage.put(key, value).unwrap();
                        }

                        (storage, keys, temp_dir)
                    },
                    |(storage, keys, _temp_dir)| {
                        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
                        for key in &key_refs {
                            black_box(storage.delete(key).unwrap());
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    storage_benches,
    bench_put_operations,
    bench_get_operations,
    bench_delete_operations,
    bench_batch_put,
    bench_batch_put_comparison,
    bench_prefix_queries,
    bench_wildcard_single_segment,
    bench_wildcard_multi_segment,
    bench_get_all,
    bench_concurrent_reads,
    bench_fsync_impact,
    bench_prefix_stripping,
    bench_key_operations,
    bench_get_many_vs_individual,
    bench_delete_many_vs_individual,
);

criterion_main!(storage_benches);
