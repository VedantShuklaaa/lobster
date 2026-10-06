use engine::*;

fn add(id: u64, side: Side, price: u32, qty: u32) -> Command {
    Command::Add(Order {
        id,
        side,
        price,
        qty,
    })
}

fn fill(maker: u64, taker: u64, price: u32, qty: u32) -> Event {
    Event::Fill(Fill {
        maker,
        taker,
        price,
        qty,
    })
}

#[test]
fn non_crossing_orders_rest() {
    let mut book = OrderBook::new();
    assert!(book.apply(add(1, Side::Bid, 100, 10)).is_empty());
    assert!(book.apply(add(2, Side::Ask, 105, 10)).is_empty());
    assert_eq!(book.best_bid(), Some(100));
    assert_eq!(book.best_ask(), Some(105));
}

#[test]
fn bid_below_best_ask_does_not_fill() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 10));
    let ev = book.apply(add(2, Side::Bid, 99, 10));
    assert!(ev.is_empty());
    assert_eq!(book.best_bid(), Some(99));
    assert_eq!(book.best_ask(), Some(100));
}

#[test]
fn partial_fill_taker_smaller() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 10));
    let ev = book.apply(add(2, Side::Bid, 100, 4));
    assert_eq!(ev, vec![fill(1, 2, 100, 4)]);
    assert_eq!(book.qty_at(Side::Ask, 100), 6);
    assert_eq!(book.best_bid(), None); // taker fully filled, nothing rests
}

#[test]
fn fills_at_maker_price_and_remainder_rests() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 10));
    let ev = book.apply(add(2, Side::Bid, 101, 15));
    assert_eq!(ev, vec![fill(1, 2, 100, 10)]); // maker's price, not 101
    assert_eq!(book.best_ask(), None);
    assert_eq!(book.best_bid(), Some(101));
    assert_eq!(book.qty_at(Side::Bid, 101), 5);
}

#[test]
fn time_priority_within_level() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Ask, 100, 5));
    let ev = book.apply(add(3, Side::Bid, 100, 7));
    assert_eq!(ev, vec![fill(1, 3, 100, 5), fill(2, 3, 100, 2)]);
    assert_eq!(book.qty_at(Side::Ask, 100), 3);
}

#[test]
fn sweeps_multiple_levels_best_price_first() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Ask, 101, 5));
    let ev = book.apply(add(3, Side::Bid, 101, 8));
    assert_eq!(ev, vec![fill(1, 3, 100, 5), fill(2, 3, 101, 3)]);
    assert_eq!(book.best_ask(), Some(101));
    assert_eq!(book.qty_at(Side::Ask, 101), 2);
}

#[test]
fn ask_taker_matches_bids_best_price_first() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Bid, 99, 5));
    book.apply(add(2, Side::Bid, 100, 5));
    let ev = book.apply(add(3, Side::Ask, 99, 7));
    assert_eq!(ev, vec![fill(2, 3, 100, 5), fill(1, 3, 99, 2)]);
}

#[test]
fn rejects_zero_qty_and_duplicate_id() {
    let mut book = OrderBook::new();
    assert_eq!(
        book.apply(add(1, Side::Bid, 100, 0)),
        vec![Event::Rejected {
            id: 1,
            reason: RejectReason::InvalidQty
        }]
    );
    book.apply(add(2, Side::Bid, 100, 5));
    assert_eq!(
        book.apply(add(2, Side::Bid, 101, 5)),
        vec![Event::Rejected {
            id: 2,
            reason: RejectReason::DuplicateId
        }]
    );
}

#[test]
fn cancel_removes_resting_order() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Bid, 100, 10));
    assert!(book.apply(Command::Cancel(1)).is_empty());
    assert_eq!(book.best_bid(), None);
}

#[test]
fn cancel_keeps_other_orders_at_same_level() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Ask, 100, 7));
    book.apply(Command::Cancel(1));
    assert_eq!(book.qty_at(Side::Ask, 100), 7);
    // order 2 is now first in the queue
    let ev = book.apply(add(3, Side::Bid, 100, 7));
    assert_eq!(ev, vec![fill(2, 3, 100, 7)]);
}

#[test]
fn cancel_unknown_id_rejects() {
    let mut book = OrderBook::new();
    assert_eq!(
        book.apply(Command::Cancel(99)),
        vec![Event::Rejected {
            id: 99,
            reason: RejectReason::UnknownOrder
        }]
    );
}

#[test]
fn cancel_after_full_fill_rejects() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Bid, 100, 5)); // order 1 fully filled
    assert_eq!(
        book.apply(Command::Cancel(1)),
        vec![Event::Rejected {
            id: 1,
            reason: RejectReason::UnknownOrder
        }]
    );
}

fn market(id: u64, side: Side, qty: u32) -> Command {
    Command::Market { id, side, qty }
}

#[test]
fn market_sweeps_levels() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Ask, 101, 5));
    let ev = book.apply(market(3, Side::Bid, 8));
    assert_eq!(ev, vec![fill(1, 3, 100, 5), fill(2, 3, 101, 3)]);
    assert_eq!(book.qty_at(Side::Ask, 101), 2);
}

#[test]
fn market_remainder_is_dropped_not_rested() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    let ev = book.apply(market(2, Side::Bid, 20));
    assert_eq!(ev, vec![fill(1, 2, 100, 5)]);
    assert_eq!(book.best_bid(), None); // nothing rested
    assert_eq!(book.best_ask(), None);
}

#[test]
fn market_into_empty_book_rejects() {
    let mut book = OrderBook::new();
    assert_eq!(
        book.apply(market(1, Side::Bid, 10)),
        vec![Event::Rejected {
            id: 1,
            reason: RejectReason::NoLiquidity
        }]
    );
}

#[test]
fn market_sell_hits_best_bid_first() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Bid, 99, 5));
    book.apply(add(2, Side::Bid, 100, 5));
    let ev = book.apply(market(3, Side::Ask, 7));
    assert_eq!(ev, vec![fill(2, 3, 100, 5), fill(1, 3, 99, 2)]);
}

#[test]
fn modify_decrease_keeps_priority() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 10));
    book.apply(add(2, Side::Ask, 100, 10));
    book.apply(Command::Modify { id: 1, new_qty: 4 });
    let ev = book.apply(add(3, Side::Bid, 100, 6));
    // order 1 still first: 4 from it, then 2 from order 2
    assert_eq!(ev, vec![fill(1, 3, 100, 4), fill(2, 3, 100, 2)]);
}

#[test]
fn modify_increase_loses_priority() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Ask, 100, 5));
    book.apply(Command::Modify { id: 1, new_qty: 8 });
    let ev = book.apply(add(3, Side::Bid, 100, 6));
    // order 2 now first
    assert_eq!(ev, vec![fill(2, 3, 100, 5), fill(1, 3, 100, 1)]);
}

#[test]
fn modify_rejects_unknown_and_zero() {
    let mut book = OrderBook::new();
    assert_eq!(
        book.apply(Command::Modify { id: 9, new_qty: 5 }),
        vec![Event::Rejected {
            id: 9,
            reason: RejectReason::UnknownOrder
        }]
    );
    book.apply(add(1, Side::Bid, 100, 5));
    assert_eq!(
        book.apply(Command::Modify { id: 1, new_qty: 0 }),
        vec![Event::Rejected {
            id: 1,
            reason: RejectReason::InvalidQty
        }]
    );
}

#[test]
fn cancel_middle_of_level_keeps_links_intact() {
    let mut book = OrderBook::new();
    book.apply(add(1, Side::Ask, 100, 5));
    book.apply(add(2, Side::Ask, 100, 5));
    book.apply(add(3, Side::Ask, 100, 5));
    book.apply(Command::Cancel(2));
    let ev = book.apply(add(4, Side::Bid, 100, 10));
    assert_eq!(ev, vec![fill(1, 4, 100, 5), fill(3, 4, 100, 5)]);
}

#[test]
fn slots_are_reused_after_cancel_and_fill() {
    let mut book = OrderBook::new();
    for round in 0..100u64 {
        let b = round * 10;
        book.apply(add(b + 1, Side::Ask, 100, 5));
        book.apply(add(b + 2, Side::Ask, 100, 5));
        book.apply(Command::Cancel(b + 1));
        let ev = book.apply(add(b + 3, Side::Bid, 100, 5));
        assert_eq!(ev, vec![fill(b + 2, b + 3, 100, 5)]);
        assert_eq!(book.best_ask(), None);
    }
}

#[test]
fn apply_into_appends_and_does_not_clear() {
    let mut book = OrderBook::new();
    let mut out = Vec::new();
    book.apply_into(add(1, Side::Ask, 100, 5), &mut out);
    book.apply_into(add(2, Side::Bid, 100, 5), &mut out);
    book.apply_into(Command::Cancel(99), &mut out);
    assert_eq!(
        out,
        vec![
            fill(1, 2, 100, 5),
            Event::Rejected {
                id: 99,
                reason: RejectReason::UnknownOrder
            }
        ]
    );
}
