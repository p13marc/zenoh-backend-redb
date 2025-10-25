use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
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

/// Helper to create a stored value
fn create_value(size: usize, timestamp: u64) -> StoredValue {
    let payload = vec![0u8; size];
    StoredValue::new(payload, timestamp, "application/octet-stream".to_string())
}

/// Benchmark backend creation
fn bench_backend_creation(c: &mut Criterion) {
    c.bench_function("backend_creation", |b| {
        b.iter_batched(
            || {
                let temp_dir = TempDir::new().unwrap();
                let config = RedbBackendConfig::new()
                    .with_base_dir(temp_dir.path().to_path_buf())
                    .with_create_dir(true);
                (temp_dir, config)
            },
            |(temp_dir, config)| {
                let backend = RedbBackend::new(black_box(config)).unwrap();
                drop(backend);
                drop(temp_dir);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark storage creation
fn bench_storage_creation(c: &mut Criterion) {
    c.bench_function("storage_creation", |b| {
        b.iter_batched(
            || {
                let (backend, temp_dir) = create_test_backend();
                let storage_name = format!("storage_{}", rand::random::<u64>());
                (backend, temp_dir, storage_name)
            },
            |(backend, _temp_dir, storage_name)| {
                backend
                    .create_storage(black_box(storage_name), None)
                    .unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark storage creation with custom config
fn bench_storage_creation_with_config(c: &mut Criterion) {
    c.bench_function("storage_creation_with_config", |b| {
        b.iter_batched(
            || {
                let (backend, temp_dir) = create_test_backend();
                let storage_name = format!("storage_{}", rand::random::<u64>());
                let config = RedbStorageConfig::new()
                    .with_fsync(false)
                    .with_cache_size(10 * 1024 * 1024);
                (backend, temp_dir, storage_name, config)
            },
            |(backend, _temp_dir, storage_name, config)| {
                backend
                    .create_storage(black_box(storage_name), Some(config))
                    .unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark getting existing storage
fn bench_get_storage(c: &mut Criterion) {
    let (backend, _temp_dir) = create_test_backend();

    // Pre-create storages
    for i in 0..10 {
        backend
            .create_storage(format!("storage_{}", i), None)
            .unwrap();
    }

    c.bench_function("get_storage", |b| {
        let mut counter = 0;
        b.iter(|| {
            let storage_name = format!("storage_{}", counter % 10);
            counter += 1;
            black_box(backend.get_storage(&storage_name).unwrap());
        });
    });
}

/// Benchmark checking if storage exists
fn bench_has_storage(c: &mut Criterion) {
    let (backend, _temp_dir) = create_test_backend();

    // Pre-create storages
    for i in 0..10 {
        backend
            .create_storage(format!("storage_{}", i), None)
            .unwrap();
    }

    c.bench_function("has_storage", |b| {
        let mut counter = 0;
        b.iter(|| {
            let storage_name = format!("storage_{}", counter % 20); // Half exist, half don't
            counter += 1;
            let _ = black_box(backend.has_storage(&storage_name));
        });
    });
}

/// Benchmark listing all storages
fn bench_list_storages(c: &mut Criterion) {
    let mut group = c.benchmark_group("list_storages");

    for num_storages in [1, 10, 50, 100].iter() {
        group.throughput(Throughput::Elements(*num_storages as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_storages),
            num_storages,
            |b, &num_storages| {
                let (backend, _temp_dir) = create_test_backend();

                // Pre-create storages
                for i in 0..num_storages {
                    backend
                        .create_storage(format!("storage_{}", i), None)
                        .unwrap();
                }

                b.iter(|| {
                    let _ = black_box(backend.list_storages());
                });
            },
        );
    }
    group.finish();
}

/// Benchmark removing storage
fn bench_remove_storage(c: &mut Criterion) {
    c.bench_function("remove_storage", |b| {
        b.iter_batched(
            || {
                let (backend, temp_dir) = create_test_backend();
                let storage_name = format!("storage_{}", rand::random::<u64>());
                backend.create_storage(storage_name.clone(), None).unwrap();
                (backend, temp_dir, storage_name)
            },
            |(backend, _temp_dir, storage_name)| {
                backend.remove_storage(black_box(&storage_name)).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark getting storage count
fn bench_storage_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_count");

    for num_storages in [1, 10, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(num_storages),
            num_storages,
            |b, &num_storages| {
                let (backend, _temp_dir) = create_test_backend();

                // Pre-create storages
                for i in 0..num_storages {
                    backend
                        .create_storage(format!("storage_{}", i), None)
                        .unwrap();
                }

                b.iter(|| {
                    let _ = black_box(backend.storage_count());
                });
            },
        );
    }
    group.finish();
}

/// Benchmark multiple storage operations
fn bench_multi_storage_operations(c: &mut Criterion) {
    c.bench_function("multi_storage_put", |b| {
        let (backend, _temp_dir) = create_test_backend();

        // Create multiple storages
        for i in 0..10 {
            backend
                .create_storage(format!("storage_{}", i), None)
                .unwrap();
        }

        let mut counter = 0u64;
        b.iter(|| {
            // Distribute writes across storages
            for i in 0..10 {
                let storage = backend.get_storage(&format!("storage_{}", i)).unwrap();
                let key = format!("test/key/{}", counter);
                let value = create_value(100, counter);
                storage.put(&key, black_box(value)).unwrap();
                counter += 1;
            }
        });
    });
}

/// Benchmark storage isolation (operations don't interfere)
fn bench_storage_isolation(c: &mut Criterion) {
    c.bench_function("storage_isolation_reads", |b| {
        let (backend, _temp_dir) = create_test_backend();

        // Create two storages
        let storage1 = backend
            .create_storage("storage_1".to_string(), None)
            .unwrap();
        let storage2 = backend
            .create_storage("storage_2".to_string(), None)
            .unwrap();

        // Pre-populate both
        for i in 0..100 {
            let key = format!("test/key/{}", i);
            let value = create_value(100, i);
            storage1.put(&key, value.clone()).unwrap();
            storage2.put(&key, value).unwrap();
        }

        let mut counter = 0;
        b.iter(|| {
            let key = format!("test/key/{}", counter % 100);
            counter += 1;
            // Read from both storages
            black_box(storage1.get(&key).unwrap());
            black_box(storage2.get(&key).unwrap());
        });
    });
}

/// Benchmark backend with many storages and operations
fn bench_high_storage_count_operations(c: &mut Criterion) {
    c.bench_function("operations_with_50_storages", |b| {
        let (backend, _temp_dir) = create_test_backend();

        // Create many storages
        for i in 0..50 {
            backend
                .create_storage(format!("storage_{}", i), None)
                .unwrap();
        }

        let mut counter = 0u64;
        b.iter(|| {
            // Randomly access different storages
            let storage_idx = counter % 50;
            let storage = backend
                .get_storage(&format!("storage_{}", storage_idx))
                .unwrap();
            let key = format!("test/key/{}", counter);
            let value = create_value(100, counter);
            storage.put(&key, black_box(value)).unwrap();
            counter += 1;
        });
    });
}

/// Benchmark backend close operation
fn bench_backend_close(c: &mut Criterion) {
    c.bench_function("backend_close", |b| {
        b.iter_batched(
            || {
                let (backend, temp_dir) = create_test_backend();
                // Create some storages
                for i in 0..5 {
                    backend
                        .create_storage(format!("storage_{}", i), None)
                        .unwrap();
                }
                (backend, temp_dir)
            },
            |(backend, _temp_dir)| {
                backend.close().unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Benchmark full lifecycle: create, use, close
fn bench_full_lifecycle(c: &mut Criterion) {
    c.bench_function("full_backend_lifecycle", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |temp_dir| {
                let config = RedbBackendConfig::new()
                    .with_base_dir(temp_dir.path().to_path_buf())
                    .with_create_dir(true);
                let backend = RedbBackend::new(config).unwrap();

                // Create storage
                let storage = backend
                    .create_storage("test_storage".to_string(), None)
                    .unwrap();

                // Do some operations
                for i in 0..10 {
                    let key = format!("test/key/{}", i);
                    let value = create_value(100, i);
                    storage.put(&key, value).unwrap();
                }

                // Close
                backend.close().unwrap();
                drop(temp_dir);
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    backend_benches,
    bench_backend_creation,
    bench_storage_creation,
    bench_storage_creation_with_config,
    bench_get_storage,
    bench_has_storage,
    bench_list_storages,
    bench_remove_storage,
    bench_storage_count,
    bench_multi_storage_operations,
    bench_storage_isolation,
    bench_high_storage_count_operations,
    bench_backend_close,
    bench_full_lifecycle,
);

criterion_main!(backend_benches);
