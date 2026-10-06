use std::hint::black_box;
use std::time::Instant;

use engine::OrderBook;
use generator::{GenConfig, generate};

fn main() {
    let cmds = generate(&GenConfig::default());
    let mut out = Vec::with_capacity(64);

    // capacity: a number, or "auto" (= 2 * peak live orders), default 1<<14
    let arg = std::env::args().nth(1).unwrap_or_else(|| "16384".into());
    let cap: usize = if arg == "auto" {
        let mut probe = OrderBook::new();
        let mut peak = 0usize;
        for c in &cmds {
            out.clear();
            probe.apply_into(*c, &mut out);
            peak = peak.max(probe.live_orders());
        }
        println!("peak live orders: {peak}");
        2 * peak
    } else {
        arg.parse().expect("capacity must be a number or 'auto'")
    };
    println!("index capacity: {cap}");

    // warmup on a throwaway book
    let mut warm = OrderBook::with_capacity(cap);
    for c in cmds.iter().take(100_000) {
        out.clear();
        warm.apply_into(*c, &mut out);
        black_box(&out);
    }

    // cost of the timer itself, subtracted from results
    let n = 1_000_000u64;
    let t0 = Instant::now();
    for _ in 0..n {
        let t = Instant::now();
        black_box(t.elapsed());
    }
    let overhead = t0.elapsed().as_nanos() as u64 / n;

    // ONE timed pass, fresh book
    let mut book = OrderBook::with_capacity(cap);
    let mut lat: Vec<u64> = Vec::with_capacity(cmds.len());
    for c in &cmds {
        out.clear();
        let t = Instant::now();
        book.apply_into(*c, &mut out);
        black_box(&out);
        lat.push(t.elapsed().as_nanos() as u64);
    }

    lat.sort_unstable();
    let pct = |p: f64| lat[((lat.len() - 1) as f64 * p) as usize].saturating_sub(overhead);

    assert_eq!(lat.len(), cmds.len());
    println!("ops: {}  timer overhead: {overhead} ns", lat.len());
    println!("p50   {} ns", pct(0.50));
    println!("p90   {} ns", pct(0.90));
    println!("p99   {} ns", pct(0.99));
    println!("p99.9 {} ns", pct(0.999));
    println!("max   {} ns", lat[lat.len() - 1]);
}
