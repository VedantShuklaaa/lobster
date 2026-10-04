pub type Price = u32;
pub type Qty = u32;
pub type OrderId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub maker: OrderId, // the resting order
    pub taker: OrderId, // the incoming order
    pub price: Price,   // always the maker's price
    pub qty: Qty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Add(Order),
    Cancel(OrderId),
    Modify { id: OrderId, new_qty: Qty },
}
