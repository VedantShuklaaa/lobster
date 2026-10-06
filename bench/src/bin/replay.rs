use std::hint::black_box;

use engine::OrderBook;
use generator::{GenConfig, generate};

fn main() {
    let cmds = generate(&GenConfig::default());
    let mut out = Vec::with_capacity(64);

    // repeat so the profile has enough samples
    for _ in 0..20 {
        let mut book = OrderBook::with_capacity(1 << 14);
        for c in &cmds {
            out.clear();
            book.apply_into(*c, &mut out);
            black_box(&out);
        }
    }
}
