use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::{Command, Event, Fill, Order, OrderId, Price, Qty, RejectReason, Side};

#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.write_u64(*b as u64);
        }
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

const NIL: u32 = u32::MAX;
const DEFAULT_LEVELS: usize = 1 << 16;

#[derive(Clone, Copy)]
struct Node {
    order: Order,
    prev: u32,
    next: u32, // also links the free list
}

#[derive(Clone, Copy)]
struct Level {
    head: u32,
    tail: u32,
}

const EMPTY: Level = Level {
    head: NIL,
    tail: NIL,
};

struct Slab {
    nodes: Vec<Node>,
    free: u32,
}

impl Slab {
    fn alloc(&mut self, order: Order) -> u32 {
        let node = Node {
            order,
            prev: NIL,
            next: NIL,
        };
        if self.free != NIL {
            let s = self.free;
            self.free = self.nodes[s as usize].next;
            self.nodes[s as usize] = node;
            s
        } else {
            self.nodes.push(node);
            (self.nodes.len() - 1) as u32
        }
    }

    fn release(&mut self, s: u32) {
        self.nodes[s as usize].next = self.free;
        self.free = s;
    }

    fn push_back(&mut self, level: &mut Level, s: u32) {
        let tail = level.tail;
        self.nodes[s as usize].prev = tail;
        self.nodes[s as usize].next = NIL;
        if tail == NIL {
            level.head = s;
        } else {
            self.nodes[tail as usize].next = s;
        }
        level.tail = s;
    }

    /// Detach from its level. Does not free the slot.
    fn unlink(&mut self, level: &mut Level, s: u32) {
        let Node { prev, next, .. } = self.nodes[s as usize];
        if prev == NIL {
            level.head = next;
        } else {
            self.nodes[prev as usize].next = next;
        }
        if next == NIL {
            level.tail = prev;
        } else {
            self.nodes[next as usize].prev = prev;
        }
    }
}

/// Two-level occupancy bitmap over price levels.
///
/// Bit `p` of `bits` is set iff level `p` is non-empty. Bit `w` of `summary` is set iff
/// `bits[w] != 0`. Finding the next occupied level is then at most three word scans
/// (bits, summary, bits) instead of walking up to 64k empty levels.
struct Occupancy {
    bits: Vec<u64>,
    summary: Vec<u64>,
}

impl Occupancy {
    fn new(levels: usize) -> Self {
        let words = levels.div_ceil(64);
        Self {
            bits: vec![0; words],
            summary: vec![0; words.div_ceil(64)],
        }
    }

    #[inline]
    fn set(&mut self, p: usize) {
        let w = p >> 6;
        self.bits[w] |= 1 << (p & 63);
        self.summary[w >> 6] |= 1 << (w & 63);
    }

    #[inline]
    fn clear(&mut self, p: usize) {
        let w = p >> 6;
        self.bits[w] &= !(1 << (p & 63));
        if self.bits[w] == 0 {
            self.summary[w >> 6] &= !(1 << (w & 63));
        }
    }

    #[inline]
    fn contains(&self, p: usize) -> bool {
        self.bits[p >> 6] >> (p & 63) & 1 == 1
    }

    /// Smallest occupied level strictly above `p`.
    #[inline]
    fn next_above(&self, p: usize) -> Option<usize> {
        let w = p >> 6;
        // two shifts: a single `<< (b + 1)` would overflow when b == 63
        let m = self.bits[w] & (!0u64 << (p & 63) << 1);
        if m != 0 {
            return Some((w << 6) | m.trailing_zeros() as usize);
        }
        let mut sw = w >> 6;
        let mut sm = self.summary[sw] & (!0u64 << (w & 63) << 1);
        loop {
            if sm != 0 {
                let w2 = (sw << 6) | sm.trailing_zeros() as usize;
                return Some((w2 << 6) | self.bits[w2].trailing_zeros() as usize);
            }
            sw += 1;
            if sw == self.summary.len() {
                return None;
            }
            sm = self.summary[sw];
        }
    }

    /// Largest occupied level strictly below `p`.
    #[inline]
    fn prev_below(&self, p: usize) -> Option<usize> {
        let w = p >> 6;
        let m = self.bits[w] & ((1u64 << (p & 63)) - 1);
        if m != 0 {
            return Some((w << 6) | (63 - m.leading_zeros() as usize));
        }
        let mut sw = w >> 6;
        let mut sm = self.summary[sw] & ((1u64 << (w & 63)) - 1);
        loop {
            if sm != 0 {
                let w2 = (sw << 6) | (63 - sm.leading_zeros() as usize);
                return Some((w2 << 6) | (63 - self.bits[w2].leading_zeros() as usize));
            }
            if sw == 0 {
                return None;
            }
            sw -= 1;
            sm = self.summary[sw];
        }
    }
}

/// One side of the book: a direct-indexed array of price levels.
struct Ladder {
    levels: Vec<Level>,
    /// Which levels are non-empty, for O(1)-ish best-price advance.
    occ: Occupancy,
    best: Option<Price>,
    /// Number of non-empty levels.
    active: u32,
    is_bid: bool,
}

impl Ladder {
    fn new(n: usize, is_bid: bool) -> Self {
        Self {
            levels: vec![EMPTY; n],
            occ: Occupancy::new(n),
            best: None,
            active: 0,
            is_bid,
        }
    }

    fn push_back(&mut self, slab: &mut Slab, price: Price, slot: u32) {
        let level = &mut self.levels[price as usize];
        let was_empty = level.head == NIL;
        slab.push_back(level, slot);
        if was_empty {
            self.occ.set(price as usize);
            self.active += 1;
            self.best = Some(match self.best {
                None => price,
                Some(b) if self.is_bid => b.max(price),
                Some(b) => b.min(price),
            });
        }
    }

    fn unlink(&mut self, slab: &mut Slab, price: Price, slot: u32) {
        let level = &mut self.levels[price as usize];
        slab.unlink(level, slot);
        if level.head == NIL {
            self.level_emptied(price);
        }
    }

    fn level_emptied(&mut self, price: Price) {
        self.occ.clear(price as usize);
        self.active -= 1;
        if self.active == 0 {
            self.best = None;
        } else if self.best == Some(price) {
            // every other non-empty level is at a worse price, so one exists
            let next = if self.is_bid {
                self.occ.prev_below(price as usize)
            } else {
                self.occ.next_above(price as usize)
            };
            self.best = Some(next.expect("active > 0 but no occupied level") as Price);
        }
    }
}

pub struct OrderBook {
    bids: Ladder,
    asks: Ladder,
    slab: Slab,
    index: FastMap<OrderId, u32>,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(orders: usize) -> Self {
        Self::with_config(orders, DEFAULT_LEVELS)
    }

    /// `price_levels`: prices must be < this. `orders`: pre-sizes the slab and index.
    pub fn with_config(orders: usize, price_levels: usize) -> Self {
        let mut index = FastMap::default();
        index.reserve(orders);
        Self {
            bids: Ladder::new(price_levels, true),
            asks: Ladder::new(price_levels, false),
            slab: Slab {
                nodes: Vec::with_capacity(orders),
                free: NIL,
            },
            index,
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.best
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.best
    }

    pub fn live_orders(&self) -> usize {
        self.index.len()
    }

    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let mut out = Vec::new();
        self.apply_into(cmd, &mut out);
        out
    }

    /// Hot-path entry point. Appends events to `out`; the caller clears and reuses the buffer.
    pub fn apply_into(&mut self, cmd: Command, out: &mut Vec<Event>) {
        match cmd {
            Command::Add(order) => self.add_limit(order, out),
            Command::Market { id, side, qty } => self.market(id, side, qty, out),
            Command::Cancel(id) => self.cancel(id, out),
            Command::Modify { id, new_qty } => self.modify(id, new_qty, out),
        }
    }

    fn match_order(
        &mut self,
        id: OrderId,
        side: Side,
        limit: Option<Price>,
        mut qty: Qty,
        events: &mut Vec<Event>,
    ) -> Qty {
        let opp = match side {
            Side::Bid => &mut self.asks,
            Side::Ask => &mut self.bids,
        };

        while qty > 0 {
            let Some(best_price) = opp.best else { break };

            if let Some(limit) = limit {
                let crosses = match side {
                    Side::Bid => limit >= best_price,
                    Side::Ask => limit <= best_price,
                };
                if !crosses {
                    break;
                }
            }

            while qty > 0 {
                let s = opp.levels[best_price as usize].head;
                if s == NIL {
                    break; // level emptied, opp.best already advanced
                }
                let node = &mut self.slab.nodes[s as usize];
                let traded = qty.min(node.order.qty);

                events.push(Event::Fill(Fill {
                    maker: node.order.id,
                    taker: id,
                    price: best_price,
                    qty: traded,
                }));

                qty -= traded;
                node.order.qty -= traded;

                if node.order.qty == 0 {
                    let maker_id = node.order.id;
                    opp.unlink(&mut self.slab, best_price, s);
                    self.slab.release(s);
                    self.index.remove(&maker_id);
                }
            }
        }
        qty
    }

    fn add_limit(&mut self, mut order: Order, out: &mut Vec<Event>) {
        if order.qty == 0 {
            out.push(Event::Rejected {
                id: order.id,
                reason: RejectReason::InvalidQty,
            });
            return;
        }
        if order.price as usize >= self.bids.levels.len() {
            out.push(Event::Rejected {
                id: order.id,
                reason: RejectReason::PriceOutOfRange,
            });
            return;
        }
        if self.index.contains_key(&order.id) {
            out.push(Event::Rejected {
                id: order.id,
                reason: RejectReason::DuplicateId,
            });
            return;
        }

        order.qty = self.match_order(order.id, order.side, Some(order.price), order.qty, out);

        if order.qty > 0 {
            let s = self.slab.alloc(order);
            let ladder = match order.side {
                Side::Bid => &mut self.bids,
                Side::Ask => &mut self.asks,
            };
            ladder.push_back(&mut self.slab, order.price, s);
            self.index.insert(order.id, s);
        }
    }

    fn market(&mut self, id: OrderId, side: Side, qty: Qty, out: &mut Vec<Event>) {
        if qty == 0 {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::InvalidQty,
            });
            return;
        }
        let start = out.len();
        self.match_order(id, side, None, qty, out);
        if out.len() == start {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::NoLiquidity,
            });
        }
    }

    fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        let Some(s) = self.index.remove(&id) else {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            });
            return;
        };

        let Order { side, price, .. } = self.slab.nodes[s as usize].order;
        let ladder = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        ladder.unlink(&mut self.slab, price, s);
        self.slab.release(s);
    }

    fn modify(&mut self, id: OrderId, new_qty: Qty, out: &mut Vec<Event>) {
        if new_qty == 0 {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::InvalidQty,
            });
            return;
        }
        let Some(&s) = self.index.get(&id) else {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            });
            return;
        };

        let node = &mut self.slab.nodes[s as usize];
        if new_qty <= node.order.qty {
            node.order.qty = new_qty; // decrease keeps queue position
            return;
        }

        // increase: lose priority, move to the back of the level
        node.order.qty = new_qty;
        let Order { side, price, .. } = node.order;
        let ladder = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = &mut ladder.levels[price as usize];
        if level.tail == s {
            return; // already at the back
        }
        // not the tail, so other orders remain: the level never empties here
        self.slab.unlink(level, s);
        self.slab.push_back(level, s);
    }

    // ---- test/debug helpers ----

    fn walk(&self, level: Level, mut f: impl FnMut(&Order)) {
        let mut s = level.head;
        while s != NIL {
            let n = &self.slab.nodes[s as usize];
            f(&n.order);
            s = n.next;
        }
    }

    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        let ladder = match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        };
        let Some(&level) = ladder.levels.get(price as usize) else {
            return 0;
        };
        let mut sum = 0;
        self.walk(level, |o| sum += o.qty);
        sum
    }

    pub fn total_qty(&self) -> u64 {
        let mut sum = 0u64;
        for ladder in [&self.bids, &self.asks] {
            for level in &ladder.levels {
                self.walk(*level, |o| sum += o.qty as u64);
            }
        }
        sum
    }

    #[doc(hidden)]
    pub fn check_invariants(&self) {
        if let (Some(b), Some(a)) = (self.best_bid(), self.best_ask()) {
            assert!(b < a, "crossed book: bid {b} >= ask {a}");
        }
        let mut count = 0;
        for ladder in [&self.bids, &self.asks] {
            let side = if ladder.is_bid { Side::Bid } else { Side::Ask };
            let mut active = 0u32;
            let mut best: Option<Price> = None;
            for (p, level) in ladder.levels.iter().enumerate() {
                let price = p as Price;

                assert_eq!(
                    ladder.occ.contains(p),
                    level.head != NIL,
                    "occupancy bit mismatch at {price}"
                );

                if level.head == NIL {
                    assert_eq!(level.tail, NIL, "half-empty level at {price}");
                    continue;
                }
                active += 1;
                best = Some(match best {
                    None => price,
                    Some(b) if ladder.is_bid => b.max(price),
                    Some(b) => b.min(price),
                });
                let (mut s, mut prev) = (level.head, NIL);
                while s != NIL {
                    let n = &self.slab.nodes[s as usize];
                    assert_eq!(n.prev, prev, "broken prev link");
                    assert_eq!(n.order.side, side);
                    assert_eq!(n.order.price, price);
                    assert!(n.order.qty > 0, "zero-qty resting order {}", n.order.id);
                    assert_eq!(self.index.get(&n.order.id), Some(&s), "index mismatch");
                    count += 1;
                    prev = s;
                    s = n.next;
                }
                assert_eq!(prev, level.tail, "tail mismatch");
            }

            for (w, &word) in ladder.occ.bits.iter().enumerate() {
                let summary_bit = ladder.occ.summary[w >> 6] >> (w & 63) & 1 == 1;
                assert_eq!(summary_bit, word != 0, "summary bit mismatch at word {w}");
            }

            assert_eq!(active, ladder.active, "active level count mismatch");
            assert_eq!(best, ladder.best, "cached best price mismatch");
        }
        assert_eq!(count, self.index.len(), "index has orphan entries");
    }
}

#[cfg(test)]
mod occupancy_tests {
    use super::*;
    use std::collections::BTreeSet;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Random set/clear against a BTreeSet; every query must agree. Sizes include
    /// non-multiples of 64 and ones that straddle summary-word boundaries.
    #[test]
    fn bitmap_matches_btreeset() {
        for &n in &[1usize, 63, 64, 65, 1000, 4096, 4097, 65_536] {
            let mut occ = Occupancy::new(n);
            let mut reference = BTreeSet::new();
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ n as u64);
            for step in 0..20_000 {
                let p = rng.below(n);
                // alternate dense and sparse phases
                let sparse = (step / 2_000) % 2 == 1;
                if reference.contains(&p) || (sparse && rng.below(4) != 0) {
                    occ.clear(p);
                    reference.remove(&p);
                } else {
                    occ.set(p);
                    reference.insert(p);
                }
                let q = rng.below(n);
                assert_eq!(occ.contains(q), reference.contains(&q));
                assert_eq!(
                    occ.next_above(q),
                    reference.range(q + 1..).next().copied(),
                    "n={n} q={q}"
                );
                assert_eq!(
                    occ.prev_below(q),
                    reference.range(..q).next_back().copied(),
                    "n={n} q={q}"
                );
            }
        }
    }

    /// Random commands on a tiny ladder (lots of level churn and boundary prices),
    /// full invariant check after every command.
    #[test]
    fn random_commands_keep_invariants() {
        let levels = 300;
        let mut book = OrderBook::with_config(64, levels);
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        let mut out = Vec::new();
        let mut next_id = 1u64;
        for _ in 0..30_000 {
            let r = rng.below(100);
            let cmd = if r < 45 {
                // ids may already be gone: exercises UnknownOrder
                Command::Cancel(1 + rng.below(next_id as usize + 1) as u64)
            } else if r < 50 {
                Command::Market {
                    id: {
                        next_id += 1;
                        next_id
                    },
                    side: if rng.below(2) == 0 {
                        Side::Bid
                    } else {
                        Side::Ask
                    },
                    qty: 1 + rng.below(50) as u32,
                }
            } else if r < 55 {
                Command::Modify {
                    id: 1 + rng.below(next_id as usize + 1) as u64,
                    new_qty: 1 + rng.below(100) as u32,
                }
            } else {
                next_id += 1;
                Command::Add(Order {
                    id: next_id,
                    side: if rng.below(2) == 0 {
                        Side::Bid
                    } else {
                        Side::Ask
                    },
                    // includes 0, 63/64 boundaries and the top levels
                    price: rng.below(levels + 5) as Price,
                    qty: 1 + rng.below(100) as u32,
                })
            };
            out.clear();
            book.apply_into(cmd, &mut out);
            book.check_invariants();
        }
    }
}
