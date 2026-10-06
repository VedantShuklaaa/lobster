use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use engine::OrderBook;
use generator::{GenConfig, generate};

fn replay(c: &mut Criterion) {
    let cmds = generate(&GenConfig::default());
    let mut out = Vec::with_capacity(64);

    let mut g = c.benchmark_group("replay");
    g.throughput(Throughput::Elements(cmds.len() as u64));
    g.sample_size(10);
    g.bench_function("v4_array_levels", |b| {
        b.iter(|| {
            // ~2x peak live orders (8.6k) for this workload
            let mut book = OrderBook::with_capacity(1 << 14);
            for cmd in &cmds {
                out.clear();
                book.apply_into(*cmd, &mut out);
                black_box(&out);
            }
            book // returned so the drop happens outside the timed section
        })
    });
    g.finish();
}

criterion_group!(benches, replay);
criterion_main!(benches);
