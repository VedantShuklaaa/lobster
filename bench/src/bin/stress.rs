use bench::{Profile, profile, replay_once, worst_case_scan};
use generator::{GenConfig, generate};

fn main() {
    let label = std::env::args().nth(1).unwrap_or_else(|| "engine".into());
    let runs: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let workloads = [
        ("narrow", GenConfig::narrow()),
        ("wide", GenConfig::wide()),
        ("sparse", GenConfig::sparse()),
    ];

    let mut results: Vec<(&str, Profile, Vec<f64>)> = Vec::new();
    for (name, cfg) in &workloads {
        let cmds = generate(cfg);
        let prof = profile(&cmds);
        if prof.out_of_range > 0 {
            eprintln!(
                "warning: {name}: {} adds were rejected as PriceOutOfRange, so the engine isn't doing the work you think",
                prof.out_of_range
            );
        }

        let cap = prof.capacity();
        let mut out = Vec::with_capacity(64);
        replay_once(&cmds, cap, &mut out); // warmup
        let mut mops: Vec<f64> = (0..runs)
            .map(|_| cmds.len() as f64 / replay_once(&cmds, cap, &mut out).as_secs_f64() / 1e6)
            .collect();
        mops.sort_by(|a, b| a.partial_cmp(b).unwrap());
        results.push((name, prof, mops));
    }

    println!("## Workload shape ({label})\n");
    println!(
        "| Workload | Price range | Peak live | Mean live | Fills | Ghost cancels | Scans | Steps/cmd | Scan p99 | Scan max |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (name, p, _) in &results {
        println!(
            "| {name} | {}..{} | {} | {:.0} | {} | {} | {} | {:.3} | {} | {} |",
            p.min_price,
            p.max_price,
            p.peak_live,
            p.mean_live,
            p.fills,
            p.unknown_order,
            p.scans,
            p.steps_per_command(),
            p.scan_p99,
            p.scan_max,
        );
    }

    println!("\n## Replay throughput ({label}, {runs} runs, median / min..max)\n");
    println!("| Workload | M ops/s | min..max | vs narrow |");
    println!("|---|---|---|---|");
    let narrow_median = median(&results[0].2);
    for (name, _, mops) in &results {
        let m = median(mops);
        println!(
            "| {name} | {m:.1} | {:.1}..{:.1} | {:.2}x |",
            mops[0],
            mops[mops.len() - 1],
            m / narrow_median
        );
    }

    // Worst case for the best-price scan: the whole gap is empty on every cancel.
    let (near, far, cycles) = (1_000u32, 65_000u32, 20_000usize);
    let cmds = worst_case_scan(near, far, cycles);
    let mut out = Vec::with_capacity(64);
    replay_once(&cmds, 8, &mut out); // warmup
    let mut secs: Vec<f64> = (0..5)
        .map(|_| replay_once(&cmds, 8, &mut out).as_secs_f64())
        .collect();
    secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let per_cycle_us = median(&secs) / cycles as f64 * 1e6;
    let per_level_ns = per_cycle_us * 1e3 / (far - near) as f64;
    println!("\n## Worst-case scan ({label})\n");
    println!(
        "Next ask {} ticks away; add + cancel of the best ask, {cycles} cycles.\n",
        far - near
    );
    println!("| Gap | Per add+cancel | Per level of gap |");
    println!("|---|---|---|");
    println!(
        "| {} levels | {per_cycle_us:.2} us | {per_level_ns:.3} ns |",
        far - near
    );
}

fn median(sorted: &[f64]) -> f64 {
    sorted[sorted.len() / 2]
}
