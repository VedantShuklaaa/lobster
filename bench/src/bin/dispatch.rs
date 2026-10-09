//! Where do the ~4.5 ns/command of `apply_into` self time go?
//!
//! Replays the same command stream through a dispatch with trivial handlers (no book,
//! no memory structure), changing one thing at a time:
//!
//!   cmd/random   24-byte `Command` stream, generated order   <- what the engine sees
//!   cmd/sorted   same commands, stable-sorted by type        <- branches become predictable
//!   tag/random   1-byte tag + 8-byte payload, generated order <- 9 bytes/command instead of 24
//!   tag/sorted   same, sorted
//!   floor        XOR of the payloads, no dispatch            <- loop and memory-read floor
//!
//! Reading the result (all in ns/command):
//!   mispredict cost   = cmd/random - cmd/sorted   (cross-check: tag/random - tag/sorted)
//!   stream-read cost  = cmd/random - tag/random   (24 B vs 9 B per command; compiled
//!                                                  differently, so treat as approximate)
//!   call + dispatch   = cmd/sorted - floor        (cost when everything is predictable)
//!
//!   cargo run --release -p bench --bin dispatch -- [passes]    (default 30)

use std::hint::black_box;
use std::time::Instant;

use engine::Command;
use generator::{GenConfig, generate};

#[inline(never)]
fn on_add(p: u64) -> u64 {
    p.rotate_left(7) ^ 0x9E37_79B9
}
#[inline(never)]
fn on_cancel(p: u64) -> u64 {
    p.rotate_left(13) ^ 0x85EB_CA6B
}
#[inline(never)]
fn on_market(p: u64) -> u64 {
    p.rotate_left(29) ^ 0xC2B2_AE35
}
#[inline(never)]
fn on_modify(p: u64) -> u64 {
    p.rotate_left(3) ^ 0x27D4_EB2F
}

/// Same shape as `OrderBook::apply_into`: a non-inlined function that takes the command
/// by value and matches on it.
#[inline(never)]
fn apply(cmd: Command) -> u64 {
    match cmd {
        Command::Add(o) => on_add(o.id),
        Command::Market { id, .. } => on_market(id),
        Command::Cancel(id) => on_cancel(id),
        Command::Modify { id, .. } => on_modify(id),
    }
}

fn run_cmds(cmds: &[Command]) -> u64 {
    let mut acc = 0u64;
    for &c in cmds {
        acc ^= apply(c);
    }
    acc
}

#[inline(never)]
fn apply_tag(tag: u8, p: u64) -> u64 {
    match tag {
        0 => on_add(p),
        1 => on_cancel(p),
        2 => on_market(p),
        _ => on_modify(p),
    }
}

fn run_tags(tags: &[u8], payload: &[u64]) -> u64 {
    let mut acc = 0u64;
    for (&t, &p) in tags.iter().zip(payload) {
        acc ^= apply_tag(t, p);
    }
    acc
}

fn run_floor(payload: &[u64]) -> u64 {
    let mut acc = 0u64;
    for &p in payload {
        acc ^= p;
    }
    acc
}

fn tag_of(c: &Command) -> u8 {
    match c {
        Command::Add(_) => 0,
        Command::Cancel(_) => 1,
        Command::Market { .. } => 2,
        Command::Modify { .. } => 3,
    }
}

fn payload_of(c: &Command) -> u64 {
    match c {
        Command::Add(o) => o.id,
        Command::Market { id, .. } | Command::Modify { id, .. } => *id,
        Command::Cancel(id) => *id,
    }
}

/// Median ns per command over `passes` timed passes (after 3 warmup passes).
fn time_ns_per_cmd(n: usize, passes: usize, mut f: impl FnMut() -> u64) -> (f64, u64) {
    let mut result = 0;
    for _ in 0..3 {
        result = black_box(f());
    }
    let mut ns: Vec<f64> = (0..passes)
        .map(|_| {
            let t = Instant::now();
            result = black_box(f());
            t.elapsed().as_nanos() as f64 / n as f64
        })
        .collect();
    ns.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (ns[ns.len() / 2], result)
}

fn main() {
    let passes: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let random = generate(&GenConfig::narrow());
    let n = random.len();
    let mut sorted = random.clone();
    sorted.sort_by_key(tag_of); // stable

    let tags_r: Vec<u8> = random.iter().map(tag_of).collect();
    let pay_r: Vec<u64> = random.iter().map(payload_of).collect();
    let tags_s: Vec<u8> = sorted.iter().map(tag_of).collect();
    let pay_s: Vec<u64> = sorted.iter().map(payload_of).collect();

    // how unpredictable is the type sequence?
    let mut counts = [0usize; 4];
    for &t in &tags_r {
        counts[t as usize] += 1;
    }
    let changes = tags_r.windows(2).filter(|w| w[0] != w[1]).count();
    println!(
        "stream: {n} commands, add {:.1}%  cancel {:.1}%  market {:.1}%  modify {:.1}%; type changes between neighbours: {:.1}% (sorted: {:.2}%)\n",
        100.0 * counts[0] as f64 / n as f64,
        100.0 * counts[1] as f64 / n as f64,
        100.0 * counts[2] as f64 / n as f64,
        100.0 * counts[3] as f64 / n as f64,
        100.0 * changes as f64 / n as f64,
        100.0 * tags_s.windows(2).filter(|w| w[0] != w[1]).count() as f64 / n as f64,
    );

    let (cmd_r, r1) = time_ns_per_cmd(n, passes, || run_cmds(black_box(&random)));
    let (cmd_s, r2) = time_ns_per_cmd(n, passes, || run_cmds(black_box(&sorted)));
    let (tag_r, r3) = time_ns_per_cmd(n, passes, || {
        run_tags(black_box(&tags_r), black_box(&pay_r))
    });
    let (tag_s, r4) = time_ns_per_cmd(n, passes, || {
        run_tags(black_box(&tags_s), black_box(&pay_s))
    });
    let (floor, _) = time_ns_per_cmd(n, passes, || run_floor(black_box(&pay_r)));

    // XOR is order-independent, so every dispatch variant must agree; a mismatch means
    // a variant is not doing the same work (or the sort lost commands)
    assert!(
        r1 == r2 && r2 == r3 && r3 == r4,
        "variants disagree: {r1:x} {r2:x} {r3:x} {r4:x}"
    );

    println!("| Variant | bytes/cmd | ns/cmd |");
    println!("|---|---|---|");
    println!(
        "| cmd/random | {} | {cmd_r:.2} |",
        std::mem::size_of::<Command>()
    );
    println!(
        "| cmd/sorted | {} | {cmd_s:.2} |",
        std::mem::size_of::<Command>()
    );
    println!("| tag/random | 9 | {tag_r:.2} |");
    println!("| tag/sorted | 9 | {tag_s:.2} |");
    println!("| floor | 8 | {floor:.2} |");
    println!();
    println!(
        "mispredict cost   cmd: {:.2} ns   tag: {:.2} ns",
        cmd_r - cmd_s,
        tag_r - tag_s
    );
    println!(
        "stream-read cost  {:.2} ns  (cmd/random - tag/random, approximate)",
        cmd_r - tag_r
    );
    println!(
        "call + dispatch   {:.2} ns  (cmd/sorted - floor, everything predictable)",
        cmd_s - floor
    );
    println!("\nfor scale: apply_into self time in the v5 profiles was ~4.1-4.6 ns/command");
}
