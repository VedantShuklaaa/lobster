use bench::profile;
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use engine::OrderBook;
use generator::{GenConfig, generate};

fn replay(c: &mut Criterion) {
    let mut g = c.benchmark_group("workloads");
    g.sample_size(10);

    let workloads = [
        ("narrow", GenConfig::narrow()),
        ("wide", GenConfig::wide()),
        ("sparse", GenConfig::sparse()),
    ];

    for (id, cfg) in workloads {
        let cmds = generate(&cfg);
        // ~2x this workload's peak live orders (8.6k for narrow)
        let capacity = profile(&cmds).capacity();
        let mut out = Vec::with_capacity(64);

        g.throughput(Throughput::Elements(cmds.len() as u64));
        g.bench_function(id, |b| {
            b.iter(|| {
                let mut book = OrderBook::with_capacity(capacity);
                for cmd in &cmds {
                    out.clear();
                    book.apply_into(*cmd, &mut out);
                    black_box(&out);
                }
                book // returned so the drop happens outside the timed section
            })
        });
    }
    g.finish();
}

criterion_group!(benches, replay);
criterion_main!(benches);
