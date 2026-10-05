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
