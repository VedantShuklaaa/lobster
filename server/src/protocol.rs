//! The JSON messages on the wire. One WebSocket per visitor.
//!
//! Client to server: `order`, `market`, `cancel`, `ping`.
//! Server to client: `hello`, `book` (public, ~20 per second), and private replies
//! (`ack`, `cancelled`, `reject`, `order_update`, `order_done`, `pong`, `error`).

use engine::{OrderId, Price, Qty, Side};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WireSide {
    Bid,
    Ask,
}

impl From<WireSide> for Side {
    fn from(s: WireSide) -> Side {
        match s {
            WireSide::Bid => Side::Bid,
            WireSide::Ask => Side::Ask,
        }
    }
}

impl From<Side> for WireSide {
    fn from(s: Side) -> WireSide {
        match s {
            Side::Bid => WireSide::Bid,
            Side::Ask => WireSide::Ask,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// `tag` is chosen by the client and echoed in the reply, to match replies to requests.
    Order {
        tag: u64,
        side: WireSide,
        price: Price,
        qty: Qty,
    },
    Market {
        tag: u64,
        side: WireSide,
        qty: Qty,
    },
    Cancel {
        tag: u64,
        id: OrderId,
    },
    /// `t` is the client's own clock reading, echoed back so it can measure the round trip.
    Ping {
        t: f64,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct Limits {
    pub max_qty: Qty,
    pub price_band: u32,
    pub max_open_orders: u32,
    pub msg_rate: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        session: u32,
        levels: usize,
        snapshot_ms: u64,
        limits: Limits,
    },
    /// Reply to `order` or `market`. `remaining` is the quantity now resting (0 for market).
    /// `engine_ns` is the time `apply_into` took; `queue_ns` is how long the command waited
    /// for the engine thread. Neither includes the network.
    Ack {
        tag: u64,
        id: OrderId,
        filled: u64,
        remaining: Qty,
        fills: Vec<(Price, Qty)>,
        engine_ns: u64,
        queue_ns: u64,
    },
    Cancelled {
        tag: u64,
        id: OrderId,
        engine_ns: u64,
        queue_ns: u64,
    },
    Reject {
        tag: u64,
        reason: &'static str,
    },
    /// One of your resting orders was partly filled.
    OrderUpdate {
        id: OrderId,
        remaining: Qty,
    },
    /// One of your resting orders was fully filled.
    OrderDone {
        id: OrderId,
    },
    Pong {
        t: f64,
    },
    Error {
        message: &'static str,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct TradeOut {
    pub price: Price,
    pub qty: Qty,
    /// Side of the aggressing order.
    pub side: WireSide,
}

#[derive(Clone, Debug, Serialize)]
pub struct Stats {
    pub uptime_s: u64,
    pub cmds_total: u64,
    pub cmds_per_sec: u32,
    pub live_orders: usize,
    pub visitors: usize,
    /// Time inside `apply_into` over the last second, all commands.
    pub apply_p50_ns: u32,
    pub apply_p99_ns: u32,
}

/// Public snapshot, same for everyone.
#[derive(Serialize)]
pub struct BookMsg<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub seq: u64,
    pub bids: &'a [(Price, u64)],
    pub asks: &'a [(Price, u64)],
    pub trades: &'a [TradeOut],
    pub stats: Stats,
}

pub fn reject_reason(r: engine::RejectReason) -> &'static str {
    use engine::RejectReason::*;
    match r {
        NoLiquidity => "no_liquidity",
        UnknownOrder => "unknown_order",
        DuplicateId => "duplicate_id",
        InvalidQty => "invalid_qty",
        PriceOutOfRange => "price_out_of_range",
    }
}
