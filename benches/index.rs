//! Cold-start and warm-tick timings. Run with `cargo bench --bench index`; the
//! cases report timings only and CI does not gate on them.
#![expect(
    clippy::unwrap_used,
    reason = "a bench harness may abort on setup failure"
)]
use std::path::Path;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use meta_call_lsp::index::{Persistence, Reindexer, rebuild_from_inputs};

/// Generated tree size: enough to exercise discovery, extraction and cross-file
const FILES: usize = 400;

fn source(index: usize) -> String {
    let (import, delegation) = if index == 0 {
        (String::new(), "value".to_string())
    } else {
        let previous = index - 1;
        (
            format!("from mod_{previous} import helper_{previous}\n\n\n"),
            format!("helper_{previous}(value)"),
        )
    };
    format!(
        "{import}class Unit_{index}:\n    def __init__(self, value):\n        self.value = value\n\n    def scaled(self, factor):\n        return self.value * factor\n\n\ndef helper_{index}(value):\n    return {delegation}\n\n\ndef wrapper_{index}(value):\n    unit = Unit_{index}(value)\n    return unit.scaled(2)\n"
    )
}

fn write_tree(root: &Path) {
    for index in 0..FILES {
        std::fs::write(root.join(format!("mod_{index}.py")), source(index)).unwrap();
    }
}

fn tree_with_index() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path());
    let mut writer = Reindexer::with_persistence(Persistence::Enabled);
    writer.rebuild(dir.path(), &[]).unwrap();
    dir
}

fn cold_start(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_start");
    group.sample_size(10);

    group.bench_function("full_extract", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                write_tree(dir.path());
                dir
            },
            |dir| rebuild_from_inputs(dir.path(), &[]).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("index_load", |b| {
        b.iter_batched(
            tree_with_index,
            |dir| {
                let mut reindexer = Reindexer::new();
                reindexer.seed_from_shards(dir.path());
                reindexer.rebuild(dir.path(), &[]).unwrap()
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn warm_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("warm_tick");
    group.sample_size(10);

    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path());
    let mut reindexer = Reindexer::new();
    reindexer.rebuild(dir.path(), &[]).unwrap();

    let mut ticks = 0usize;
    group.bench_function("one_file_changed", |b| {
        b.iter(|| {
            ticks += 1;
            let index = ticks % FILES;
            let path = dir.path().join(format!("mod_{index}.py"));
            std::fs::write(&path, format!("# tick {ticks}\n{}", source(index))).unwrap();
            reindexer.rebuild(dir.path(), &[]).unwrap()
        });
    });

    group.finish();
}

criterion_group!(benches, cold_start, warm_tick);
criterion_main!(benches);
