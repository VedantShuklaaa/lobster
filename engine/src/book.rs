use std::collections::{BTreeMap, HashMap};

use crate::{Command, Event, Fill, Order, OrderId, Price, Qty, RejectReason, Side};
use std::hash::{BuildHasherDefault, Hasher};

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

pub struct OrderBook {
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
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
        let mut index = FastMap::default();
        index.reserve(orders);
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            slab: Slab {
                nodes: Vec::with_capacity(orders),
                free: NIL,
            },
            index,
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.last_key_value().map(|(p, _)| *p)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first_key_value().map(|(p, _)| *p)
    }

    pub fn live_orders(&self) -> usize {
        self.index.len()
    }

    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let mut out = Vec::new();
        self.apply_into(cmd, &mut out);
        out
    }

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
        while qty > 0 {
            let best = match side {
                Side::Bid => self.asks.first_key_value().map(|(p, _)| *p),
                Side::Ask => self.bids.last_key_value().map(|(p, _)| *p),
            };
            let Some(best_price) = best else { break };

            if let Some(limit) = limit {
                let crosses = match side {
                    Side::Bid => limit >= best_price,
                    Side::Ask => limit <= best_price,
                };
                if !crosses {
                    break;
                }
            }

            let opp = match side {
                Side::Bid => &mut self.asks,
                Side::Ask => &mut self.bids,
            };
            let level = opp.get_mut(&best_price).unwrap();

            while qty > 0 && level.head != NIL {
                let s = level.head;
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
                    self.slab.unlink(level, s);
                    self.slab.release(s);
                    self.index.remove(&maker_id);
                }
            }

            if level.head == NIL {
                opp.remove(&best_price);
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
            let book = match order.side {
                Side::Bid => &mut self.bids,
                Side::Ask => &mut self.asks,
            };
            let level = book.entry(order.price).or_insert(Level {
                head: NIL,
                tail: NIL,
            });
            self.slab.push_back(level, s);
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
        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = book.get_mut(&price).unwrap();
        self.slab.unlink(level, s);
        self.slab.release(s);
        if level.head == NIL {
            book.remove(&price);
        }
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
        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = book.get_mut(&price).unwrap();
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
        let book = match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        };
        let Some(&level) = book.get(&price) else {
            return 0;
        };
        let mut sum = 0;
        self.walk(level, |o| sum += o.qty);
        sum
    }

    pub fn total_qty(&self) -> u64 {
        let mut sum = 0u64;
        for level in self.bids.values().chain(self.asks.values()) {
            self.walk(*level, |o| sum += o.qty as u64);
        }
        sum
    }

    #[doc(hidden)]
    pub fn check_invariants(&self) {
        if let (Some(b), Some(a)) = (self.best_bid(), self.best_ask()) {
            assert!(b < a, "crossed book: bid {b} >= ask {a}");
        }
        let mut count = 0;
        for (side, book) in [(Side::Bid, &self.bids), (Side::Ask, &self.asks)] {
            for (price, level) in book {
                assert!(
                    level.head != NIL && level.tail != NIL,
                    "empty level at {price}"
                );
                let (mut s, mut prev) = (level.head, NIL);
                while s != NIL {
                    let n = &self.slab.nodes[s as usize];
                    assert_eq!(n.prev, prev, "broken prev link");
                    assert_eq!(n.order.side, side);
                    assert_eq!(n.order.price, *price);
                    assert!(n.order.qty > 0, "zero-qty resting order {}", n.order.id);
                    assert_eq!(self.index.get(&n.order.id), Some(&s), "index mismatch");
                    count += 1;
                    prev = s;
                    s = n.next;
                }
                assert_eq!(prev, level.tail, "tail mismatch");
            }
        }
        assert_eq!(count, self.index.len(), "index has orphan entries");
    }
}
