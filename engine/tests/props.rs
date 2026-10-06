use engine::*;
use proptest::prelude::*;

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

// Small id/price ranges on purpose: forces duplicates, unknown ids, crossing.
fn cmd() -> impl Strategy<Value = Command> {
    prop_oneof![
        4 => (1u64..40, side(), 95u32..=105, 0u32..=10)
            .prop_map(|(id, side, price, qty)| Command::Add(Order { id, side, price, qty })),
        1 => (1u64..40, side(), 0u32..=15)
            .prop_map(|(id, side, qty)| Command::Market { id, side, qty }),
        3 => (1u64..40).prop_map(Command::Cancel),
        1 => (1u64..40, 0u32..=15)
            .prop_map(|(id, new_qty)| Command::Modify { id, new_qty }),
    ]
}

proptest! {
    #[test]
    fn invariants_hold(cmds in prop::collection::vec(cmd(), 1..300)) {
        let mut book = OrderBook::new();
        for c in cmds {
            let before = book.total_qty() as i64;
            let events = book.apply(c);
            let after = book.total_qty() as i64;

            let filled: i64 = events.iter().filter_map(|e| match e {
                Event::Fill(f) => Some(f.qty as i64),
                _ => None,
            }).sum();
            let rejected = events.iter().any(|e| matches!(e, Event::Rejected { .. }));

            for e in &events {
                if let Event::Fill(f) = e { prop_assert!(f.qty > 0); }
            }

            // qty conservation: a fill removes `filled` from makers, and the
            // taker's non-filled remainder (qty - filled) rests.
            match c {
                Command::Add(o) => {
                    if rejected { prop_assert_eq!(after, before); }
                    else { prop_assert_eq!(after - before, o.qty as i64 - 2 * filled); }
                }
                Command::Market { .. } => prop_assert_eq!(after - before, -filled),
                Command::Cancel(_) => {
                    prop_assert!(after <= before);
                    if rejected { prop_assert_eq!(after, before); }
                }
                Command::Modify { .. } => {
                    if rejected { prop_assert_eq!(after, before); }
                }
            }

            book.check_invariants();
        }
    }
}
