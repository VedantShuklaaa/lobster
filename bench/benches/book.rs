use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use engine::OrderBook;
use generator::{generate, GenConfig};

fn replay(c: &mut Criterion) {
    let cmds = generate(&GenConfig::default());
    let mut g = c.benchmark_group("replay");
    g.throughput(Throughput::Elements(cmds.len() as u64));
    g.sample_size(10);
    g.bench_function("v1_btreemap", |b| {
        b.iter(|| {
            let mut book = OrderBook::with_capacity(1 << 20);
            for cmd in &cmds {
                black_box(book.apply(*cmd));
            }
            book // dropped outside the timed section
        })
    });
    g.finish();
}

criterion_group!(benches, replay);
criterion_main!(benches);