use engine::{Command, Order, OrderId, Side};
use rand::{rngs::StdRng, Rng, SeedableRng};

pub struct GenConfig {
    pub seed: u64,
    pub n: usize,
    pub mid: u32,
    pub p_cancel: f64,
    pub p_market: f64,
}

impl Default for GenConfig {
    fn default() -> Self {
        Self { seed: 42, n: 1_000_000, mid: 10_000, p_cancel: 0.45, p_market: 0.05 }
    }
}

pub fn generate(cfg: &GenConfig) -> Vec<Command> {
    let mut rng = StdRng::seed_from_u64(cfg.seed);
    let mut cmds = Vec::with_capacity(cfg.n);
    let mut live: Vec<OrderId> = Vec::new();
    let mut next_id: OrderId = 1;
    let mut mid = cfg.mid as i64;
    let lo = cfg.mid as i64 - 1_000;
    let hi = cfg.mid as i64 + 1_000;

    for _ in 0..cfg.n {
        mid = (mid + rng.gen_range(-1..=1)).clamp(lo, hi);
        let r: f64 = rng.gen_range(0.0..1.0);
        let side = if rng.gen_bool(0.5) { Side::Bid } else { Side::Ask };

        if r < cfg.p_cancel && !live.is_empty() {
            // may target an already-filled order: realistic cancel race
            let i = rng.gen_range(0..live.len());
            cmds.push(Command::Cancel(live.swap_remove(i)));
        } else if r >= cfg.p_cancel && r < cfg.p_cancel + cfg.p_market {
            cmds.push(Command::Market { id: next_id, side, qty: rng.gen_range(1..=50) });
            next_id += 1;
        } else {
            // offset -2 => occasionally crosses the spread
            let offset: i64 = rng.gen_range(-2..=20);
            let price = match side {
                Side::Bid => mid - offset,
                Side::Ask => mid + offset,
            }
            .max(1) as u32;
            let id = next_id;
            next_id += 1;
            live.push(id);
            cmds.push(Command::Add(Order { id, side, price, qty: rng.gen_range(1..=100) }));
        }
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let cfg = GenConfig { n: 10_000, ..Default::default() };
        assert_eq!(generate(&cfg), generate(&cfg));
    }
}