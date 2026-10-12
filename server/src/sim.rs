//! A small streaming market simulator, so the book is alive when nobody is trading.
//!
//! Same flavour as the benchmark generator's `bursty` preset (runs of same-type, same-side
//! commands, prices dense at the touch), but it produces one command at a time, never
//! runs out, and keeps the book bounded: it tracks which of its own orders are still
//! resting (fills reduce them) and cancels more when there are too many.

use std::collections::HashMap;

use engine::{Command, Order, OrderId, Price, Qty, Side};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const P_CANCEL: f64 = 0.45;
const P_MARKET: f64 = 0.05;
const P_REPEAT: f64 = 0.6;
const P_MOVE: f64 = 0.01;
const OFFSET_SCALE: f64 = 4.0;
const MAX_OFFSET: i64 = 25;
const MID_BAND: i64 = 150;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Cancel,
    Market,
}

pub struct Sim {
    rng: StdRng,
    center: i64,
    mid: i64,
    next_id: OrderId,
    /// Resting simulator orders: `ids` for random picks, `info` maps id to (position in `ids`, qty left).
    ids: Vec<OrderId>,
    info: HashMap<OrderId, (usize, Qty)>,
    kind: Kind,
    side: Side,
    min_live: usize,
    max_live: usize,
}

impl Sim {
    pub fn new(seed: u64, center: Price, min_live: usize, max_live: usize) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            center: center as i64,
            mid: center as i64,
            next_id: 1,
            ids: Vec::new(),
            info: HashMap::new(),
            kind: Kind::Add,
            side: Side::Bid,
            min_live,
            max_live,
        }
    }

    pub fn mid(&self) -> Price {
        self.mid as Price
    }

    pub fn resting(&self) -> usize {
        self.ids.len()
    }

    pub fn next_command(&mut self) -> Command {
        if self.rng.gen_bool(P_MOVE) {
            let step = if self.rng.gen_bool(0.5) { 1 } else { -1 };
            self.mid = (self.mid + step).clamp(self.center - MID_BAND, self.center + MID_BAND);
        }
        if !self.rng.gen_bool(P_REPEAT) {
            let r: f64 = self.rng.gen_range(0.0..1.0);
            self.kind = if r < P_CANCEL {
                Kind::Cancel
            } else if r < P_CANCEL + P_MARKET {
                Kind::Market
            } else {
                Kind::Add
            };
        }
        if !self.rng.gen_bool(P_REPEAT) {
            self.side = if self.rng.gen_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
        }
        let kind = if self.ids.len() > self.max_live {
            Kind::Cancel
        } else if self.kind == Kind::Cancel && self.ids.len() <= self.min_live {
            Kind::Add
        } else {
            self.kind
        };

        match kind {
            Kind::Cancel => {
                let id = self.ids[self.rng.gen_range(0..self.ids.len())];
                self.forget(id);
                Command::Cancel(id)
            }
            Kind::Market => {
                let id = self.take_id();
                Command::Market {
                    id,
                    side: self.side,
                    qty: self.rng.gen_range(1..=50),
                }
            }
            Kind::Add => {
                let u: f64 = self.rng.gen_range(0.0..1.0);
                let offset = (((-(1.0 - u).ln() * OFFSET_SCALE) as i64) - 2).min(MAX_OFFSET);
                let price = match self.side {
                    Side::Bid => self.mid - offset,
                    Side::Ask => self.mid + offset,
                }
                .max(1) as Price;
                let qty = self.rng.gen_range(1..=100);
                let id = self.take_id();
                self.info.insert(id, (self.ids.len(), qty));
                self.ids.push(id);
                Command::Add(Order {
                    id,
                    side: self.side,
                    price,
                    qty,
                })
            }
        }
    }

    /// Call for every fill the engine reports, so resting quantities stay accurate.
    pub fn on_fill(&mut self, maker: OrderId, taker: OrderId, qty: Qty) {
        self.reduce(maker, qty);
        self.reduce(taker, qty);
    }

    fn take_id(&mut self) -> OrderId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn reduce(&mut self, id: OrderId, qty: Qty) {
        let Some((_, left)) = self.info.get_mut(&id) else {
            return;
        };
        *left = left.saturating_sub(qty);
        if *left == 0 {
            self.forget(id);
        }
    }

    fn forget(&mut self, id: OrderId) {
        if let Some((pos, _)) = self.info.remove(&id) {
            self.ids.swap_remove(pos);
            if let Some(&moved) = self.ids.get(pos)
                && let Some(e) = self.info.get_mut(&moved)
            {
                e.0 = pos;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::{Event, OrderBook};

    /// Drive the real engine with the sim for a while: the book must stay bounded, the sim's
    /// view of its resting orders must match the engine's, and the mid must stay in its band.
    #[test]
    fn sim_stays_bounded_and_consistent_with_the_engine() {
        let mut sim = Sim::new(1, 10_000, 80, 600);
        let mut book = OrderBook::with_capacity(2_048);
        let mut ev = Vec::new();
        let mut max_live = 0;
        for _ in 0..300_000 {
            let cmd = sim.next_command();
            ev.clear();
            book.apply_into(cmd, &mut ev);
            for e in &ev {
                if let Event::Fill(f) = e {
                    sim.on_fill(f.maker, f.taker, f.qty);
                }
                if let Event::Rejected { reason, .. } = e {
                    // a market order into an empty side is the only legitimate reject
                    assert_eq!(*reason, engine::RejectReason::NoLiquidity);
                }
            }
            max_live = max_live.max(book.live_orders());
            assert_eq!(sim.resting(), book.live_orders(), "sim and engine disagree");
            let mid = sim.mid() as i64;
            assert!((mid - 10_000).abs() <= MID_BAND);
        }
        assert!(max_live <= 650, "book grew to {max_live}");
        assert!(max_live >= 80, "book never filled up: {max_live}");
    }

    #[test]
    fn sim_is_deterministic() {
        let run = || {
            let mut s = Sim::new(9, 10_000, 80, 600);
            (0..5_000)
                .map(|_| format!("{:?}", s.next_command()))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }
}
