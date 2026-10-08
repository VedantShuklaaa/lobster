use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::{Command, Event, OrderBook, RejectReason};

#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub commands: usize,
    pub adds: usize,
    pub cancels: usize,
    pub markets: usize,
    pub modifies: usize,
    pub min_price: u32,
    pub max_price: u32,
    pub peak_live: usize,
    pub mean_live: f64,
    pub fills: u64,
    pub unknown_order: u64,
    pub no_liquidity: u64,
    pub out_of_range: u64,
    pub steady_commands: usize,
    pub scans: u64,
    pub scan_steps: u64,
    pub scan_p99: u32,
    pub scan_max: u32,
}

pub const WARMUP_PERCENT: usize = 10;

impl Profile {
    pub fn capacity(&self) -> usize {
        self.peak_live * 2
    }

    pub fn steps_per_command(&self) -> f64 {
        self.scan_steps as f64 / self.steady_commands as f64
    }
}

/// Replay `cmds` once through a large book and collect workload statistics.
///
/// The scan length is read from outside the engine: a side's best price only moves
/// toward better prices when an order rests (one jump, no scan) and toward worse
/// prices when its best level empties (the scan walks one level at a time), so the
/// distance a best price moved the wrong way equals the number of levels scanned.
pub fn profile(cmds: &[Command]) -> Profile {
    let mut book = OrderBook::with_capacity(1 << 16);
    let mut out = Vec::with_capacity(64);
    let steady_from = cmds.len() * WARMUP_PERCENT / 100;
    let mut p = Profile {
        commands: cmds.len(),
        steady_commands: cmds.len() - steady_from,
        min_price: u32::MAX,
        ..Default::default()
    };
    let mut live_sum = 0u64;
    let mut scan_lens: Vec<u32> = Vec::new();

    for (i, &c) in cmds.iter().enumerate() {
        match c {
            Command::Add(o) => {
                p.adds += 1;
                p.min_price = p.min_price.min(o.price);
                p.max_price = p.max_price.max(o.price);
            }
            Command::Market { .. } => p.markets += 1,
            Command::Cancel(_) => p.cancels += 1,
            Command::Modify { .. } => p.modifies += 1,
        }

        let (bid0, ask0) = (book.best_bid(), book.best_ask());
        out.clear();
        book.apply_into(c, &mut out);

        for e in &out {
            match e {
                Event::Fill(_) => p.fills += 1,
                Event::Rejected { reason, .. } => match reason {
                    RejectReason::UnknownOrder => p.unknown_order += 1,
                    RejectReason::NoLiquidity => p.no_liquidity += 1,
                    RejectReason::PriceOutOfRange => p.out_of_range += 1,
                    _ => {}
                },
            }
        }

        let mut steps = 0u32;
        if let (Some(a), Some(b)) = (bid0, book.best_bid()) {
            steps += a.saturating_sub(b); // bids get worse by going down
        }
        if let (Some(a), Some(b)) = (ask0, book.best_ask()) {
            steps += b.saturating_sub(a); // asks get worse by going up
        }
        if steps > 0 && i >= steady_from {
            p.scans += 1;
            p.scan_steps += steps as u64;
            scan_lens.push(steps);
        }

        let live = book.live_orders();
        p.peak_live = p.peak_live.max(live);
        live_sum += live as u64;
    }

    p.mean_live = live_sum as f64 / cmds.len() as f64;
    scan_lens.sort_unstable();
    if let Some(&max) = scan_lens.last() {
        p.scan_max = max;
        p.scan_p99 = scan_lens[(scan_lens.len() - 1) * 99 / 100];
    }
    p
}

/// One timed replay on a fresh, pre-sized book. Same method as the criterion bench:
/// the book is built inside the timed section, dropped outside it, and one event
/// buffer is reused.
pub fn replay_once(cmds: &[Command], capacity: usize, out: &mut Vec<Event>) -> Duration {
    let start = Instant::now();
    let mut book = OrderBook::with_capacity(capacity);
    for cmd in cmds {
        out.clear();
        book.apply_into(*cmd, out);
        black_box(&*out);
    }
    let elapsed = start.elapsed();
    drop(black_box(book));
    elapsed
}

/// Worst case for the best-price scan. One ask rests at `far`; then, `cycles` times,
/// an ask is added at `near` and cancelled. Every cancel empties the best level, so
/// the scan walks the whole `far - near` gap of empty levels.
pub fn worst_case_scan(near: u32, far: u32, cycles: usize) -> Vec<Command> {
    use engine::{Order, Side};
    let ask = |id, price| Order {
        id,
        side: Side::Ask,
        price,
        qty: 1,
    };
    let mut cmds = Vec::with_capacity(1 + 2 * cycles);
    cmds.push(Command::Add(ask(1, far)));
    for i in 0..cycles as u64 {
        cmds.push(Command::Add(ask(2 + i, near)));
        cmds.push(Command::Cancel(2 + i));
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_case_scans_the_whole_gap_every_cycle() {
        let cycles = 100;
        let cmds = worst_case_scan(1_000, 65_000, cycles);
        let p = profile(&cmds);
        // cancel k sits at index 2k; only those past the warmup window are counted
        let steady_from = cmds.len() * WARMUP_PERCENT / 100;
        let expected = (1..=cycles).filter(|k| 2 * k >= steady_from).count() as u64;
        assert_eq!(p.scans, expected);
        assert_eq!(p.scan_max, 64_000);
        assert_eq!(p.scan_steps, expected * 64_000);
        assert_eq!(p.peak_live, 2);
    }
}
