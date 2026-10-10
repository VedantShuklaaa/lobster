use engine::{Command, Order, OrderId, Side};
use rand::{Rng, SeedableRng, rngs::StdRng};

pub struct GenConfig {
    pub seed: u64,
    pub n: usize,
    /// Starting mid price (and the centre of the mid's allowed band).
    pub mid: u32,
    pub p_cancel: f64,
    pub p_market: f64,
    /// Mid price may drift at most this far from `mid` (clamp on the random walk).
    pub band: i64,
    /// Mid price moves by a uniform step in `-mid_step..=mid_step` per command.
    pub mid_step: i64,
    /// Passive orders rest at an offset in `-2..=max_offset` ticks from the mid
    /// (negative offsets cross the spread). This is what sets how wide the book is.
    pub max_offset: i64,
    /// Cancels only fire while the generator tracks more than this many live
    /// orders, so this steers how thin the book stays. 0 = no floor.
    pub min_live: usize,
    /// `Some` switches to the bursty generator (runs of same-type, same-side commands and
    /// prices clustered near the touch). `None` keeps the original i.i.d. stream, byte for
    /// byte: the published numbers are replays of it.
    pub bursty: Option<Burst>,
}

/// Knobs for the bursty stream.
#[derive(Clone, Copy, Debug)]
pub struct Burst {
    /// Probability that a command repeats the previous command's type, and (separately)
    /// its side. The type mix stays at `p_cancel` / `p_market` / rest, because a
    /// non-repeat is a fresh draw from that mix; only the clustering changes.
    pub p_repeat: f64,
    /// Passive orders rest at `-2 + Exp(offset_scale)` ticks from the mid (capped at
    /// `max_offset`): dense at the touch with a thin tail, instead of uniform.
    pub offset_scale: f64,
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
            bursty: None,
        }
    }
}

impl GenConfig {
    /// The original workload: orders packed within ~20 ticks of the mid.
    /// Best case for an array-indexed ladder.
    pub fn narrow() -> Self {
        Self::default()
    }

    /// Dense but wide: orders spread uniformly over ~30k ticks each side of the mid,
    /// ~20k resting orders, so the whole 65,536-tick ladder is touched (cache
    /// footprint stress). Same shape as `sparse` except for `min_live`.
    pub fn wide() -> Self {
        Self {
            mid: 32_768,
            max_offset: 30_000,
            p_cancel: 0.50,
            min_live: 20_000,
            ..Self::default()
        }
    }

    /// Same type mix and price band as `narrow`, but bursty: runs of same-type, same-side
    /// commands (~8% of neighbours differ in type, against ~55% in `narrow`) and prices
    /// clustered at the touch. Meant to show how much of the narrow numbers is the i.i.d.
    /// shape of the stream.
    pub fn bursty() -> Self {
        Self {
            max_offset: 60,
            bursty: Some(Burst {
                p_repeat: 0.85,
                offset_scale: 8.0,
            }),
            ..Self::default()
        }
    }

    /// Wide and thin: same price range as `wide`, but only a handful of live orders,
    /// so neighbouring populated levels are far apart and the best-price scan
    /// has to walk long empty gaps.
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
    if let Some(burst) = cfg.bursty {
        return generate_bursty(cfg, burst);
    }
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

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Cancel,
    Market,
}

fn generate_bursty(cfg: &GenConfig, burst: Burst) -> Vec<Command> {
    let mut rng = StdRng::seed_from_u64(cfg.seed);
    let mut cmds = Vec::with_capacity(cfg.n);
    let mut live: Vec<OrderId> = Vec::new();
    let mut next_id: OrderId = 1;
    let mut mid = cfg.mid as i64;
    let lo = cfg.mid as i64 - cfg.band;
    let hi = cfg.mid as i64 + cfg.band;
    let (mut kind, mut side) = (Kind::Add, Side::Bid);

    for i in 0..cfg.n {
        mid = (mid + rng.gen_range(-cfg.mid_step..=cfg.mid_step)).clamp(lo, hi);

        if i == 0 || !rng.gen_bool(burst.p_repeat) {
            let r: f64 = rng.gen_range(0.0..1.0);
            kind = if r < cfg.p_cancel {
                Kind::Cancel
            } else if r < cfg.p_cancel + cfg.p_market {
                Kind::Market
            } else {
                Kind::Add
            };
        }
        if i == 0 || !rng.gen_bool(burst.p_repeat) {
            side = if rng.gen_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
        }
        // nothing to cancel yet (or below the floor): the slot becomes an add
        let kind_now = if kind == Kind::Cancel && live.len() <= cfg.min_live {
            Kind::Add
        } else {
            kind
        };

        match kind_now {
            Kind::Cancel => {
                let j = rng.gen_range(0..live.len());
                cmds.push(Command::Cancel(live.swap_remove(j)));
            }
            Kind::Market => {
                cmds.push(Command::Market {
                    id: next_id,
                    side,
                    qty: rng.gen_range(1..=50),
                });
                next_id += 1;
            }
            Kind::Add => {
                let u: f64 = rng.gen_range(0.0..1.0);
                let tail = -(1.0 - u).ln() * burst.offset_scale;
                let offset = ((tail as i64) - 2).min(cfg.max_offset);
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

    /// The default (narrow) stream must never change: every published benchmark
    /// number is a replay of it. This hash was taken from the generator as it was
    /// before the band / mid_step / max_offset / min_live knobs existed.
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
        for cfg in [GenConfig::wide(), GenConfig::sparse(), GenConfig::bursty()] {
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

    #[test]
    fn bursty_is_deterministic_and_clustered() {
        let cfg = GenConfig {
            n: 200_000,
            ..GenConfig::bursty()
        };
        let a = generate(&cfg);
        assert_eq!(a, generate(&cfg));

        let tag = |c: &Command| match c {
            Command::Add(_) => 0,
            Command::Cancel(_) => 1,
            Command::Market { .. } => 2,
            Command::Modify { .. } => 3,
        };
        let changes = a.windows(2).filter(|w| tag(&w[0]) != tag(&w[1])).count();
        let share = |t| a.iter().filter(|c| tag(c) == t).count() as f64 / a.len() as f64;
        // far fewer type changes than the i.i.d. stream (~55%) ...
        assert!(
            changes * 100 / a.len() < 15,
            "changes {}%",
            changes * 100 / a.len()
        );
        // ... with the same overall mix (~50 / 45 / 5)
        assert!((share(0) - 0.50).abs() < 0.06, "add {}", share(0));
        assert!((share(1) - 0.45).abs() < 0.06, "cancel {}", share(1));
        assert!((share(2) - 0.05).abs() < 0.03, "market {}", share(2));
    }
}
