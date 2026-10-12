//! The engine thread: the only thread that touches the `OrderBook`.
//!
//! Sessions (one per WebSocket) send it commands through one channel; the simulator runs
//! on the same thread on a timer; every `snapshot_ms` it publishes the book to everyone.
//! The engine itself stays single-threaded and unaware of any of this.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use engine::{Command, Event, Order, OrderBook, OrderId, Price, Qty, Side};
use tokio::sync::{broadcast, mpsc as tmpsc};

use crate::config::Config;
use crate::protocol::{BookMsg, ServerMsg, Stats, TradeOut, WireSide, reject_reason};
use crate::sim::Sim;

/// Visitor order ids start here, far above anything the simulator uses, and below 2^53 so
/// they survive a round trip through a JavaScript number.
pub const VISITOR_ID_BASE: OrderId = 1 << 50;

const MAX_TRADES_PER_SNAPSHOT: usize = 200;
const MAX_LAT_SAMPLES: usize = 16_384;

pub enum Req {
    Limit {
        side: WireSide,
        price: Price,
        qty: Qty,
    },
    Market {
        side: WireSide,
        qty: Qty,
    },
    Cancel {
        id: OrderId,
    },
}

enum Msg {
    Register {
        sid: u32,
        tx: tmpsc::Sender<ServerMsg>,
    },
    Unregister {
        sid: u32,
    },
    Cmd {
        sid: u32,
        tag: u64,
        req: Req,
        enq: Instant,
    },
}

#[derive(Debug)]
pub struct Busy;

/// What sessions hold: a way to talk to the engine thread and to hear its snapshots.
#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Msg>,
    inflight: Arc<AtomicUsize>,
    max_inflight: usize,
    book_tx: broadcast::Sender<Arc<str>>,
    latest: Arc<RwLock<Arc<str>>>,
    next_sid: Arc<AtomicU32>,
}

impl EngineHandle {
    pub fn new_session(&self) -> u32 {
        self.next_sid.fetch_add(1, Ordering::Relaxed)
    }

    /// Replies for this session arrive on `tx`.
    pub fn register(&self, sid: u32, tx: tmpsc::Sender<ServerMsg>) {
        let _ = self.tx.send(Msg::Register { sid, tx });
    }

    /// Also cancels the session's resting orders.
    pub fn unregister(&self, sid: u32) {
        let _ = self.tx.send(Msg::Unregister { sid });
    }

    /// Refuses (`Busy`) when too many commands are already waiting, so one slow moment
    /// cannot build an unbounded queue.
    pub fn submit(&self, sid: u32, tag: u64, req: Req) -> Result<(), Busy> {
        if self.inflight.fetch_add(1, Ordering::AcqRel) >= self.max_inflight {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return Err(Busy);
        }
        let msg = Msg::Cmd {
            sid,
            tag,
            req,
            enq: Instant::now(),
        };
        if self.tx.send(msg).is_err() {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return Err(Busy);
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.book_tx.subscribe()
    }

    /// Most recent snapshot, so a new visitor sees a book immediately.
    pub fn latest(&self) -> Arc<str> {
        self.latest.read().unwrap().clone()
    }
}

pub fn spawn(cfg: &Config) -> EngineHandle {
    let (tx, rx) = mpsc::channel();
    let (book_tx, _) = broadcast::channel(64);
    let inflight = Arc::new(AtomicUsize::new(0));
    let latest: Arc<RwLock<Arc<str>>> = Arc::new(RwLock::new(Arc::from("{}")));

    let handle = EngineHandle {
        tx,
        inflight: inflight.clone(),
        max_inflight: cfg.max_inflight,
        book_tx: book_tx.clone(),
        latest: latest.clone(),
        next_sid: Arc::new(AtomicU32::new(1)),
    };

    let state = Engine::new(cfg.clone(), inflight, book_tx, latest);
    thread::Builder::new()
        .name("engine".into())
        .spawn(move || state.run(rx))
        .expect("spawn engine thread");
    handle
}

struct SessionEntry {
    tx: tmpsc::Sender<ServerMsg>,
    open: u32,
}

struct VisitorOrder {
    sid: u32,
    remaining: Qty,
}

struct Engine {
    cfg: Config,
    book: OrderBook,
    sim: Sim,
    events: Vec<Event>,
    sessions: HashMap<u32, SessionEntry>,
    orders: HashMap<OrderId, VisitorOrder>,
    next_vid: OrderId,
    trades: Vec<TradeOut>,
    lat: Vec<u32>,
    cmds_total: u64,
    rate_count: u64,
    rate_since: Instant,
    cmds_per_sec: u32,
    p50: u32,
    p99: u32,
    seq: u64,
    bids: Vec<(Price, u64)>,
    asks: Vec<(Price, u64)>,
    start: Instant,
    inflight: Arc<AtomicUsize>,
    book_tx: broadcast::Sender<Arc<str>>,
    latest: Arc<RwLock<Arc<str>>>,
}

impl Engine {
    fn new(
        cfg: Config,
        inflight: Arc<AtomicUsize>,
        book_tx: broadcast::Sender<Arc<str>>,
        latest: Arc<RwLock<Arc<str>>>,
    ) -> Self {
        let now = Instant::now();
        Self {
            book: OrderBook::with_capacity(4_096),
            sim: Sim::new(cfg.sim_seed, cfg.center_price, 150, 600),
            events: Vec::with_capacity(256),
            sessions: HashMap::new(),
            orders: HashMap::new(),
            next_vid: VISITOR_ID_BASE,
            trades: Vec::new(),
            lat: Vec::with_capacity(4_096),
            cmds_total: 0,
            rate_count: 0,
            rate_since: now,
            cmds_per_sec: 0,
            p50: 0,
            p99: 0,
            seq: 0,
            bids: Vec::new(),
            asks: Vec::new(),
            start: now,
            inflight,
            book_tx,
            latest,
            cfg,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        let sim_period = (self.cfg.sim_rate > 0)
            .then(|| Duration::from_secs_f64(1.0 / self.cfg.sim_rate as f64));
        let snap_period = Duration::from_millis(self.cfg.snapshot_ms.max(5));
        let start = Instant::now();
        let mut next_sim = start;
        let mut next_snap = start;

        loop {
            let deadline = match sim_period {
                Some(_) => next_sim.min(next_snap),
                None => next_snap,
            };
            let wait = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(m) => {
                    self.handle(m);
                    for _ in 0..63 {
                        match rx.try_recv() {
                            Ok(m) => self.handle(m),
                            Err(_) => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();
            if let Some(period) = sim_period {
                let mut n = 0;
                while now >= next_sim && n < 200 {
                    self.sim_step();
                    next_sim += period;
                    n += 1;
                }
                if now >= next_sim {
                    next_sim = now + period; // fell behind: drop the backlog, do not spiral
                }
            }
            if now >= next_snap {
                self.publish(now);
                next_snap += snap_period;
                if now >= next_snap {
                    next_snap = now + snap_period;
                }
            }
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Register { sid, tx } => {
                self.sessions.insert(sid, SessionEntry { tx, open: 0 });
            }
            Msg::Unregister { sid } => self.unregister(sid),
            Msg::Cmd { sid, tag, req, enq } => {
                self.visitor(sid, tag, req, enq);
                self.inflight.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn send(&self, sid: u32, msg: ServerMsg) {
        if let Some(s) = self.sessions.get(&sid) {
            // a session that cannot keep up loses messages; it is closed by its own write timeout
            let _ = s.tx.try_send(msg);
        }
    }

    /// Apply one command and time it. Events are left in `self.events`.
    fn apply(&mut self, cmd: Command) -> u64 {
        self.events.clear();
        let t0 = Instant::now();
        self.book.apply_into(cmd, &mut self.events);
        let ns = t0.elapsed().as_nanos() as u64;
        self.cmds_total += 1;
        self.rate_count += 1;
        if self.lat.len() < MAX_LAT_SAMPLES {
            self.lat.push(ns.min(u32::MAX as u64) as u32);
        }
        ns
    }

    /// Trades, simulator bookkeeping, and updates to visitors whose resting orders were hit.
    fn process_fills(&mut self, events: &[Event], taker_side: Side) {
        for ev in events {
            let Event::Fill(f) = ev else { continue };
            if self.trades.len() < MAX_TRADES_PER_SNAPSHOT {
                self.trades.push(TradeOut {
                    price: f.price,
                    qty: f.qty,
                    side: taker_side.into(),
                });
            }
            self.sim.on_fill(f.maker, f.taker, f.qty);
            if let Some(vo) = self.orders.get_mut(&f.maker) {
                vo.remaining = vo.remaining.saturating_sub(f.qty);
                let (sid, left, id) = (vo.sid, vo.remaining, f.maker);
                if left == 0 {
                    self.orders.remove(&id);
                    if let Some(s) = self.sessions.get_mut(&sid) {
                        s.open = s.open.saturating_sub(1);
                    }
                    self.send(sid, ServerMsg::OrderDone { id });
                } else {
                    self.send(
                        sid,
                        ServerMsg::OrderUpdate {
                            id,
                            remaining: left,
                        },
                    );
                }
            }
        }
    }

    fn sim_step(&mut self) {
        let cmd = self.sim.next_command();
        let side = match cmd {
            Command::Add(o) => o.side,
            Command::Market { side, .. } => side,
            _ => Side::Bid,
        };
        self.apply(cmd);
        let events = std::mem::take(&mut self.events);
        self.process_fills(&events, side);
        self.events = events;
    }

    fn reject(&self, sid: u32, tag: u64, reason: &'static str) {
        self.send(sid, ServerMsg::Reject { tag, reason });
    }

    fn visitor(&mut self, sid: u32, tag: u64, req: Req, enq: Instant) {
        let Some(open) = self.sessions.get(&sid).map(|s| s.open) else {
            return; // session already gone
        };
        match req {
            Req::Limit { side, price, qty } => {
                if qty == 0 {
                    return self.reject(sid, tag, "invalid_qty");
                }
                if qty > self.cfg.max_qty {
                    return self.reject(sid, tag, "qty_too_large");
                }
                let mid = self.sim.mid() as i64;
                if (price as i64 - mid).unsigned_abs() > self.cfg.price_band as u64 {
                    return self.reject(sid, tag, "price_out_of_band");
                }
                if open >= self.cfg.max_open_orders {
                    return self.reject(sid, tag, "too_many_orders");
                }
                let id = self.take_vid();
                let started = Instant::now();
                let engine_ns = self.apply(Command::Add(Order {
                    id,
                    side: side.into(),
                    price,
                    qty,
                }));
                let queue_ns = started.saturating_duration_since(enq).as_nanos() as u64;
                let events = std::mem::take(&mut self.events);
                let (fills, filled) = own_fills(&events, id);
                let rejected = own_reject(&events, id);
                self.process_fills(&events, side.into());
                self.events = events;
                if let Some(reason) = rejected {
                    return self.reject(sid, tag, reason);
                }
                let remaining = qty - filled as Qty;
                if remaining > 0 {
                    self.orders.insert(id, VisitorOrder { sid, remaining });
                    if let Some(s) = self.sessions.get_mut(&sid) {
                        s.open += 1;
                    }
                }
                self.send(
                    sid,
                    ServerMsg::Ack {
                        tag,
                        id,
                        filled,
                        remaining,
                        fills,
                        engine_ns,
                        queue_ns,
                    },
                );
            }
            Req::Market { side, qty } => {
                if qty == 0 {
                    return self.reject(sid, tag, "invalid_qty");
                }
                if qty > self.cfg.max_qty {
                    return self.reject(sid, tag, "qty_too_large");
                }
                let id = self.take_vid();
                let started = Instant::now();
                let engine_ns = self.apply(Command::Market {
                    id,
                    side: side.into(),
                    qty,
                });
                let queue_ns = started.saturating_duration_since(enq).as_nanos() as u64;
                let events = std::mem::take(&mut self.events);
                let (fills, filled) = own_fills(&events, id);
                let rejected = own_reject(&events, id);
                self.process_fills(&events, side.into());
                self.events = events;
                if let Some(reason) = rejected {
                    return self.reject(sid, tag, reason);
                }
                self.send(
                    sid,
                    ServerMsg::Ack {
                        tag,
                        id,
                        filled,
                        remaining: 0,
                        fills,
                        engine_ns,
                        queue_ns,
                    },
                );
            }
            Req::Cancel { id } => {
                match self.orders.get(&id) {
                    None => return self.reject(sid, tag, "unknown_order"),
                    Some(vo) if vo.sid != sid => return self.reject(sid, tag, "not_your_order"),
                    Some(_) => {}
                }
                let started = Instant::now();
                let engine_ns = self.apply(Command::Cancel(id));
                let queue_ns = started.saturating_duration_since(enq).as_nanos() as u64;
                self.orders.remove(&id);
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.open = s.open.saturating_sub(1);
                }
                self.send(
                    sid,
                    ServerMsg::Cancelled {
                        tag,
                        id,
                        engine_ns,
                        queue_ns,
                    },
                );
            }
        }
    }

    fn take_vid(&mut self) -> OrderId {
        let id = self.next_vid;
        self.next_vid += 1;
        id
    }

    fn unregister(&mut self, sid: u32) {
        self.sessions.remove(&sid);
        let mine: Vec<OrderId> = self
            .orders
            .iter()
            .filter(|(_, o)| o.sid == sid)
            .map(|(&id, _)| id)
            .collect();
        for id in mine {
            self.apply(Command::Cancel(id));
            self.orders.remove(&id);
        }
    }

    fn publish(&mut self, now: Instant) {
        let window = now.duration_since(self.rate_since);
        if window >= Duration::from_secs(1) {
            self.cmds_per_sec = (self.rate_count as f64 / window.as_secs_f64()).round() as u32;
            self.rate_count = 0;
            self.rate_since = now;
            if !self.lat.is_empty() {
                self.lat.sort_unstable();
                let n = self.lat.len();
                self.p50 = self.lat[n / 2];
                self.p99 = self.lat[(n * 99 / 100).min(n - 1)];
                self.lat.clear();
            }
        }

        let levels = self.cfg.depth_levels;
        self.book.depth_into(Side::Bid, levels, &mut self.bids);
        self.book.depth_into(Side::Ask, levels, &mut self.asks);
        self.seq += 1;
        let msg = BookMsg {
            kind: "book",
            seq: self.seq,
            bids: &self.bids,
            asks: &self.asks,
            trades: &self.trades,
            stats: Stats {
                uptime_s: now.duration_since(self.start).as_secs(),
                cmds_total: self.cmds_total,
                cmds_per_sec: self.cmds_per_sec,
                live_orders: self.book.live_orders(),
                visitors: self.sessions.len(),
                apply_p50_ns: self.p50,
                apply_p99_ns: self.p99,
            },
        };
        let json: Arc<str> = Arc::from(serde_json::to_string(&msg).expect("serialize book"));
        self.trades.clear();
        *self.latest.write().unwrap() = json.clone();
        let _ = self.book_tx.send(json); // no receivers is fine
    }
}

/// This command's own fills as (price, qty), and their total.
fn own_fills(events: &[Event], id: OrderId) -> (Vec<(Price, Qty)>, u64) {
    let mut fills = Vec::new();
    let mut total = 0u64;
    for ev in events {
        if let Event::Fill(f) = ev
            && f.taker == id
        {
            fills.push((f.price, f.qty));
            total += f.qty as u64;
        }
    }
    (fills, total)
}

fn own_reject(events: &[Event], id: OrderId) -> Option<&'static str> {
    events.iter().find_map(|e| match e {
        Event::Rejected { id: rid, reason } if *rid == id => Some(reject_reason(*reason)),
        _ => None,
    })
}
