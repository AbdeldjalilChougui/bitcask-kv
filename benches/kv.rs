use bitcask_kv::{Bitcask, Options};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

fn bench(c: &mut Criterion) {
    let value = vec![b'x'; 100];
    let mut g = c.benchmark_group("bitcask");
    g.throughput(Throughput::Elements(1));

    g.bench_function("put 100B (no fsync)", |b| {
        let t = tempfile::tempdir().unwrap();
        let mut db = Bitcask::open(t.path()).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            db.put(&i.to_le_bytes(), &value).unwrap();
        });
    });

    g.bench_function("get 100B (10k keys)", |b| {
        let t = tempfile::tempdir().unwrap();
        let mut db = Bitcask::open(t.path()).unwrap();
        for i in 0..10_000u64 {
            db.put(&i.to_le_bytes(), &value).unwrap();
        }
        let mut i = 0u64;
        b.iter(|| {
            i = (i + 7919) % 10_000;
            db.get(&i.to_le_bytes()).unwrap().unwrap();
        });
    });

    g.bench_function("open: scan 10k records", |b| {
        b.iter_batched(
            || {
                let t = tempfile::tempdir().unwrap();
                let mut db = Bitcask::open_with(t.path(), Options { max_file_size: 1 << 20, ..Default::default() }).unwrap();
                for i in 0..10_000u64 {
                    db.put(&(i % 5000).to_le_bytes(), &value).unwrap();
                }
                t
            },
            |t| Bitcask::open(t.path()).unwrap().len(),
            BatchSize::PerIteration,
        );
    });

    g.bench_function("merge 10k writes / 1k keys", |b| {
        b.iter_batched(
            || {
                let t = tempfile::tempdir().unwrap();
                let mut db = Bitcask::open(t.path()).unwrap();
                for i in 0..10_000u64 {
                    db.put(&(i % 1000).to_le_bytes(), &value).unwrap();
                }
                (t, db)
            },
            |(_t, mut db)| db.merge().unwrap(),
            BatchSize::PerIteration,
        );
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
