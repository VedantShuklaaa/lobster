//! Single-threaded engine vs engine on its own thread behind SPSC rings.
//!
//! usage: pipeline [preset] [runs]      preset: narrow|bursty|wide|sparse
//!
//! The rings publish their counters every `batch` items. Batch 1 is lowest
//! latency; larger batches hand over fewer cache lines per item. The sweep
//! below shows the throughput side of that trade.
//!
//! Every configuration is first checked to produce the same events in the same
//! order as the single-threaded run, then timed. Runs are interleaved so slow
//! drift on the machine hits every row alike. Compare rows only with each
//! other and only within one machine.

use std::time::Instant;

use generator::GenConfig;
use pipeline::{Config, Summary, run_direct, run_pipeline};

const BATCHES: [usize; 5] = [1, 8, 32, 128, 512];

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let preset = std::env::args().nth(1).unwrap_or_else(|| "narrow".into());
    let runs: usize = std::env::args()
        .nth(2)
        .map(|s| s.parse().expect("runs must be a number"))
        .unwrap_or(15);

    let cfg = match preset.as_str() {
        "narrow" => GenConfig::narrow(),
        "bursty" => GenConfig::bursty(),
        "wide" => GenConfig::wide(),
        "sparse" => GenConfig::sparse(),
        other => panic!("unknown preset '{other}' (narrow|bursty|wide|sparse)"),
    };
    let cmds = generator::generate(&cfg);
    let base = Config::default();
    println!(
        "preset: {preset}  commands: {}  cmd ring: {}  evt ring: {}  runs: {runs}",
        cmds.len(),
        base.cmd_ring,
        base.evt_ring
    );

    // correctness first: same events, same order, for every batch size
    let want: Summary = run_direct(&cmds, base.index_capacity);
    println!("direct   events {}  hash {:#018x}", want.events, want.hash);
    for batch in BATCHES {
        let got = run_pipeline(&cmds, Config { batch, ..base });
        if got != want {
            eprintln!(
                "MISMATCH at batch {batch}: events {} hash {:#018x}",
                got.events, got.hash
            );
            std::process::exit(1);
        }
    }
    println!("pipeline output identical at every batch size\n");

    let n = cmds.len() as f64;
    let mut direct = Vec::with_capacity(runs);
    let mut piped: Vec<Vec<f64>> = BATCHES.iter().map(|_| Vec::with_capacity(runs)).collect();
    for _ in 0..runs {
        let t = Instant::now();
        std::hint::black_box(run_direct(&cmds, base.index_capacity));
        direct.push(n / t.elapsed().as_secs_f64() / 1e6);

        for (i, &batch) in BATCHES.iter().enumerate() {
            let t = Instant::now();
            std::hint::black_box(run_pipeline(&cmds, Config { batch, ..base }));
            piped[i].push(n / t.elapsed().as_secs_f64() / 1e6);
        }
    }

    println!("| path                   | M ops/s  | min..max       | vs single |");
    println!("|------------------------|----------|----------------|-----------|");
    let single = median(&mut direct);
    println!(
        "| single thread          | {single:>8.1} | {:>6.1}..{:<6.1} | {:>8.2}x |",
        direct[0],
        direct[direct.len() - 1],
        1.0
    );
    for (i, &batch) in BATCHES.iter().enumerate() {
        let v = &mut piped[i];
        let med = median(v);
        println!(
            "| threaded, batch {batch:<6} | {med:>8.1} | {:>6.1}..{:<6.1} | {:>8.2}x |",
            v[0],
            v[v.len() - 1],
            med / single
        );
    }
}
