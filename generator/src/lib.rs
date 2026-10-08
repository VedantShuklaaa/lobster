use engine::{Command, Order, OrderId, Side};
use rand::{Rng, SeedableRng, rngs::StdRng};

pub struct GenConfig {
    pub seed: u64,
    pub n: usize,
    pub mid: u32,
    pub p_cancel: f64,
    pub p_market: f64,
    pub band: i64,
    pub mid_step: i64,
    pub max_offset: i64,
    pub min_live: usize,
}

impl Default for GenConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            n: 1_000_000,
            mid: 10_000,
            p_cancel: 0.45,
            p_market: 0.05,
            band: 1_000,
            mid_step: 1,
            max_offset: 20,
            min_live: 0,
        }
    }
}

impl GenConfig {
    pub fn narrow() -> Self {
        Self::default()
    }

    pub fn wide() -> Self {
        Self {
            mid: 32_768,
            max_offset: 30_000,
            p_cancel: 0.50,
            min_live: 20_000,
            ..Self::default()
        }
    }

    pub fn sparse() -> Self {
        Self {
            mid: 32_768,
            max_offset: 30_000,
            p_cancel: 0.50,
            min_live: 64,
            ..Self::default()
        }
    }
}

pub fn generate(cfg: &GenConfig) -> Vec<Command> {
    let mut rng = StdRng::seed_from_u64(cfg.seed);
    let mut cmds = Vec::with_capacity(cfg.n);
    let mut live: Vec<OrderId> = Vec::new();
    let mut next_id: OrderId = 1;
    let mut mid = cfg.mid as i64;
    let lo = cfg.mid as i64 - cfg.band;
    let hi = cfg.mid as i64 + cfg.band;

    for _ in 0..cfg.n {
        mid = (mid + rng.gen_range(-cfg.mid_step..=cfg.mid_step)).clamp(lo, hi);
        let r: f64 = rng.gen_range(0.0..1.0);
        let side = if rng.gen_bool(0.5) {
            Side::Bid
        } else {
            Side::Ask
        };

        if r < cfg.p_cancel && live.len() > cfg.min_live {
            // may target an already-filled order: realistic cancel race
            let i = rng.gen_range(0..live.len());
            cmds.push(Command::Cancel(live.swap_remove(i)));
        } else if r >= cfg.p_cancel && r < cfg.p_cancel + cfg.p_market {
            cmds.push(Command::Market {
                id: next_id,
                side,
                qty: rng.gen_range(1..=50),
            });
            next_id += 1;
        } else {
            // offset -2 => occasionally crosses the spread
            let offset: i64 = rng.gen_range(-2..=cfg.max_offset);
            let price = match side {
                Side::Bid => mid - offset,
                Side::Ask => mid + offset,
            }
            .max(1) as u32;
            let id = next_id;
            next_id += 1;
            live.push(id);
            cmds.push(Command::Add(Order {
                id,
                side,
                price,
                qty: rng.gen_range(1..=100),
            }));
        }
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let cfg = GenConfig {
            n: 10_000,
            ..Default::default()
        };
        assert_eq!(generate(&cfg), generate(&cfg));
    }

    /// FNV-1a over the Debug form of every command.
    fn fingerprint(cmds: &[Command]) -> u64 {
        use std::fmt::Write;
        struct Fnv(u64);
        impl Write for Fnv {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                for b in s.bytes() {
                    self.0 = (self.0 ^ b as u64).wrapping_mul(0x100000001b3);
                }
                Ok(())
            }
        }
        let mut h = Fnv(0xcbf29ce484222325);
        for c in cmds {
            write!(h, "{c:?};").unwrap();
        }
        h.0
    }

    #[test]
    fn narrow_stream_is_frozen() {
        let cfg = GenConfig {
            n: 200_000,
            ..GenConfig::narrow()
        };
        assert_eq!(fingerprint(&generate(&cfg)), 0x655a0a5bcc4b74c5);
    }

    #[test]
    fn presets_stay_inside_the_ladder() {
        // engine rejects prices >= 65_536; price 1 means the `.max(1)` clamp kicked in
        for cfg in [GenConfig::wide(), GenConfig::sparse()] {
            let cfg = GenConfig { n: 200_000, ..cfg };
            for c in generate(&cfg) {
                if let Command::Add(o) = c {
                    assert!(
                        o.price > 1 && o.price < 65_536,
                        "price {} out of bounds",
                        o.price
                    );
                }
            }
        }
    }
}
