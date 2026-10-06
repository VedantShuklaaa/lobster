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

        order.qty = self.match_order(
            order.id,
            order.side,
            Some(order.price),
            order.qty,
            &mut events,
        );

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
        let mut events = Vec::new();

        if qty == 0 {
            events.push(Event::Rejected {
                id,
                reason: RejectReason::InvalidQty,
            });
            return events;
        }

        self.match_order(id, side, None, qty, &mut events);

        // remainder is dropped; nothing filled at all -> reject
        if events.is_empty() {
            events.push(Event::Rejected {
                id,
                reason: RejectReason::NoLiquidity,
            });
        }
        events
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

    fn modify(&mut self, id: OrderId, new_qty: Qty) -> Vec<Event> {
        if new_qty == 0 {
            return vec![Event::Rejected {
                id,
                reason: RejectReason::InvalidQty,
            }];
        }
        let Some(&(side, price)) = self.index.get(&id) else {
            return vec![Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            }];
        };

        let book = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = book.get_mut(&price).unwrap();
        let pos = level.iter().position(|o| o.id == id).unwrap();

        if new_qty <= level[pos].qty {
            level[pos].qty = new_qty;
        } else {
            let mut order = level.remove(pos).unwrap();
            order.qty = new_qty;
            level.push_back(order);
        }

        Vec::new()
    }

    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        let book = match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        };
        book.get(&price)
            .map_or(0, |level| level.iter().map(|o| o.qty).sum())
    }

    /// Matches `qty` against the opposite side. `limit = None` means no price limit (market order).
    /// Returns the unfilled remainder.
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

            while qty > 0 {
                let Some(maker) = level.front_mut() else {
                    break;
                };
                let traded = qty.min(maker.qty);

                events.push(Event::Fill(Fill {
                    maker: maker.id,
                    taker: id,
                    price: best_price,
                    qty: traded,
                }));

                qty -= traded;
                maker.qty -= traded;

                if maker.qty == 0 {
                    let maker_id = maker.id;
                    level.pop_front();
                    self.index.remove(&maker_id);
                }
            }

            if level.is_empty() {
                opp.remove(&best_price);
            }
        }
        qty
    }
}
