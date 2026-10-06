use std::hint::black_box;

use engine::OrderBook;
use generator::{GenConfig, generate};

fn main() {
    let cmds = generate(&GenConfig::default());
    // repeat so the profile has enough samples
    for _ in 0..20 {
        let mut book = OrderBook::with_capacity(1 << 14);
        for c in &cmds {
            black_box(book.apply(*c));
        }
    }
}
