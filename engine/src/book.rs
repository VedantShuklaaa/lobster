use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::{Command, Event, Fill, Order, OrderId, Price, Qty, RejectReason, Side};

#[derive(Default)]
pub struct OrderBook {
    /// Bids: best = highest price  -> bids.iter().next_back()
    bids: BTreeMap<Price, VecDeque<Order>>,

    /// Asks: best = lowest price   -> asks.iter().next()
    asks: BTreeMap<Price, VecDeque<Order>>,

    /// Where each resting order lives, for cancel/modify.
    index: HashMap<OrderId, (Side, Price)>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.last_key_value().map(|(price, _)| *price)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first_key_value().map(|(price, _)| *price)
    }

    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        match cmd {
            Command::Add(order) => self.add_limit(order),
            Command::Market { id, side, qty } => self.market(id, side, qty),
            Command::Cancel(id) => self.cancel(id),
            Command::Modify { id, new_qty } => self.modify(id, new_qty),
        }
    }

    fn add_limit(&mut self, mut order: Order) -> Vec<Event> {
        let mut events = Vec::new();

        // 1. validate
        if order.qty == 0 {
            events.push(Event::Rejected {
                id: order.id,
                reason: RejectReason::InvalidQty,
            });
            return events;
        }
        if self.index.contains_key(&order.id) {
            events.push(Event::Rejected {
                id: order.id,
                reason: RejectReason::DuplicateId,
            });
            return events;
        }

        // 2. match against the opposite side while it crosses
        while order.qty > 0 {
            // a) best price on the opposite side
            let best = match order.side {
                Side::Bid => self.asks.first_key_value().map(|(p, _)| *p),
                Side::Ask => self.bids.last_key_value().map(|(p, _)| *p),
            };
            let Some(best_price) = best else { break };

            // b) does it cross?
            let crosses = match order.side {
                Side::Bid => order.price >= best_price,
                Side::Ask => order.price <= best_price,
            };
            if !crosses {
                break;
            }

            // c) get that level from the opposite side's map
            let opp = match order.side {
                Side::Bid => &mut self.asks,
                Side::Ask => &mut self.bids,
            };
            let level = opp.get_mut(&best_price).unwrap();

            // d) fill FIFO within the level
            while order.qty > 0 {
                let Some(maker) = level.front_mut() else {
                    break;
                };
                let traded = order.qty.min(maker.qty);

                events.push(Event::Fill(Fill {
                    maker: maker.id,
                    taker: order.id,
                    price: best_price, // maker's price
                    qty: traded,
                }));

                order.qty -= traded;
                maker.qty -= traded;

                if maker.qty == 0 {
                    let id = maker.id;
                    level.pop_front();
                    self.index.remove(&id);
                }
            }

            // e) drop the level if it's empty
            if level.is_empty() {
                opp.remove(&best_price);
            }
        }

        // 3. rest the leftover
        if order.qty > 0 {
            let side_book = match order.side {
                Side::Bid => &mut self.bids,
                Side::Ask => &mut self.asks,
            };
            side_book.entry(order.price).or_default().push_back(order);
            self.index.insert(order.id, (order.side, order.price));
        }

        events
    }

    fn market(&mut self, id: OrderId, side: Side, qty: u32) -> Vec<Event> {
        // match like add_limit but with no price limit, never rest.
        // zero fills -> Rejected { NoLiquidity }; remainder is dropped.
        todo!()
    }

    fn cancel(&mut self, id: OrderId) -> Vec<Event> {
        let Some((side, price)) = self.index.remove(&id) else {
            return vec![Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            }];
        };

        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        if let Some(level) = book.get_mut(&price) {
            if let Some(pos) = level.iter().position(|o| o.id == id) {
                level.remove(pos);
            }

            if level.is_empty() {
                book.remove(&price);
            }
        }
        Vec::new()
    }

    fn modify(&mut self, id: OrderId, new_qty: u32) -> Vec<Event> {
        // v1 rule: decreasing qty keeps queue position,
        // increasing qty loses priority (cancel + re-add at the back).
        // Unknown id -> Rejected { UnknownOrder }, new_qty == 0 -> InvalidQty.
        todo!()
    }

    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        let book = match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        };
        book.get(&price)
            .map_or(0, |level| level.iter().map(|o| o.qty).sum())
    }
}
