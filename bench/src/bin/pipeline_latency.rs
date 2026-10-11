//! End-to-end latency through the engine thread, at a controlled load.
//!
//! usage: pipeline_latency [preset] [--pin CALLER_CPU,ENGINE_CPU] [--repeat N]
//!
//! The caller sends commands at a fixed rate; latency is measured from each
//! command's due time to the moment the engine thread has applied it and
//! published its events (see `pipeline::paced`). One row per offered rate.
//! Each cell is the median over the repeats; "achieved" is the completed rate,
//! and a row marked `*` saturated (could not keep up with the offered rate).
//!
//! Pin the two threads to two CPUs on DIFFERENT physical cores. On Linux the
//! binary warns if the two CPUs are hyperthread siblings. Without --pin the OS
//! places the threads, and macOS cannot pin at all.

use std::hint::black_box;
use std::time::Instant;

use generator::GenConfig;
use pipeline::paced::run_paced;
use pipeline::pin::{pin_current_thread, thread_siblings};
use pipeline::{Config, run_direct};

/// Offered load, commands per second.
const RATES: [f64; 7] = [0.5e6, 1e6, 2e6, 5e6, 10e6, 15e6, 20e6];

struct Cell {
    achieved: f64,
    p50: u64,
    p90: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

fn median<T: Copy + PartialOrd>(v: &mut [T]) -> T {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn timer_report() {
    // smallest visible step between two reads, and the cost of one read
    let t0 = Instant::now();
    let mut min_step = u128::MAX;
    for _ in 0..200_000 {
        let a = Instant::now();
        let mut b = Instant::now();
        while b == a {
            b = Instant::now();
        }
        min_step = min_step.min((b - a).as_nanos());
    }
    let n = 1_000_000u32;
    let t1 = Instant::now();
    for _ in 0..n {
        black_box(Instant::now());
    }
    let cost = t1.elapsed().as_nanos() as f64 / n as f64;
    let _ = t0;
    println!("timer: smallest step {min_step} ns, one read costs about {cost:.0} ns");
}

fn main() {
    let mut preset = "narrow".to_string();
    let mut pin: Option<(usize, usize)> = None;
    let mut repeat = 3usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pin" => {
                let v = args.next().expect("--pin needs CALLER,ENGINE");
                let (a, b) = v.split_once(',').expect("--pin needs CALLER,ENGINE");
                pin = Some((
                    a.parse().expect("cpu number"),
                    b.parse().expect("cpu number"),
                ));
            }
            "--repeat" => {
                repeat = args
                    .next()
                    .expect("--repeat needs N")
                    .parse()
                    .expect("number")
            }
            other => preset = other.to_string(),
        }
    }

    let gen_cfg = match preset.as_str() {
        "narrow" => GenConfig::narrow(),
        "bursty" => GenConfig::bursty(),
        "wide" => GenConfig::wide(),
        "sparse" => GenConfig::sparse(),
        other => panic!("unknown preset '{other}' (narrow|bursty|wide|sparse)"),
    };
    let cmds = generator::generate(&gen_cfg);
    let mut cfg = Config::default();

    println!(
        "preset: {preset}  commands: {}  repeats: {repeat}",
        cmds.len()
    );
    match pin {
        Some((c, e)) => {
            cfg.caller_cpu = Some(c);
            cfg.engine_cpu = Some(e);
            // pinning is requested inside the run; check here that it works
            match pin_current_thread(c) {
                Ok(()) => println!("pinned: caller cpu {c}, engine cpu {e}"),
                Err(err) => println!("WARNING: cannot pin ({err}); results are unpinned"),
            }
            if let (Some(a), Some(b)) = (thread_siblings(c), thread_siblings(e)) {
                println!("cpu {c} shares a core with {a:?}, cpu {e} with {b:?}");
                if a.contains(&e) {
                    println!(
                        "WARNING: both CPUs are the same physical core (hyperthread siblings)"
                    );
                }
            }
        }
        None => println!("pinned: no (OS places the threads)"),
    }
    timer_report();

    // correctness: a paced run must produce exactly the single-threaded events
    let want = run_direct(&cmds, cfg.index_capacity);
    let got = run_paced(&cmds, cfg, 2e6).summary;
    if got != want {
        eprintln!("MISMATCH: the paced pipeline changed the output");
        std::process::exit(1);
    }
    println!("output identical to the single-threaded run\n");

    println!("latency in ns, due time -> engine finished and published (median of {repeat} runs)");
    println!("| offered M/s | achieved M/s |    p50 |    p90 |    p99 |  p99.9 |      max |");
    println!("|-------------|--------------|--------|--------|--------|--------|----------|");
    for &rate in &RATES {
        let mut cells = Vec::with_capacity(repeat);
        for _ in 0..repeat {
            let p = run_paced(&cmds, cfg, rate);
            let lat = p.latencies(0.1);
            let at = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize];
            cells.push(Cell {
                achieved: p.achieved_rate(),
                p50: at(0.50),
                p90: at(0.90),
                p99: at(0.99),
                p999: at(0.999),
                max: lat[lat.len() - 1],
            });
        }
        let col = |f: fn(&Cell) -> u64| median(&mut cells.iter().map(f).collect::<Vec<_>>());
        let achieved = median(&mut cells.iter().map(|c| c.achieved).collect::<Vec<_>>());
        let saturated = if achieved < rate * 0.97 { '*' } else { ' ' };
        println!(
            "| {:>10.1}{saturated} | {:>12.2} | {:>6} | {:>6} | {:>6} | {:>6} | {:>8} |",
            rate / 1e6,
            achieved / 1e6,
            col(|c| c.p50),
            col(|c| c.p90),
            col(|c| c.p99),
            col(|c| c.p999),
            col(|c| c.max),
        );
    }
}
