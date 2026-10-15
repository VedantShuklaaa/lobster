use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use server::config::Config;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn test_cfg() -> Config {
    Config {
        sim_rate: 0, // empty book, so every test controls the whole market
        snapshot_ms: 20,
        msg_rate: 1_000.0,
        msg_burst: 1_000.0,
        max_conns_per_ip: 50,
        conn_burst: 1_000.0,
        ..Config::default()
    }
}

async fn start(cfg: Config) -> SocketAddr {
    let app = server::start(cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            server::router(app).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

struct Client {
    ws: Ws,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Client {
        let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        Client { ws }
    }

    async fn send(&mut self, v: Value) {
        self.ws.send(Message::text(v.to_string())).await.unwrap();
    }

    /// Next message of the given type, skipping others. Panics after 3 s.
    async fn recv(&mut self, ty: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let m = self
                    .ws
                    .next()
                    .await
                    .expect("socket closed")
                    .expect("read error");
                if let Message::Text(t) = m {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == ty {
                        return v;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {ty}"))
    }

    /// Wait for a book snapshot satisfying `pred`.
    async fn book_where(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let v = self.recv("book").await;
                if pred(&v) {
                    return v;
                }
            }
        })
        .await
        .expect("book never matched")
    }
}

#[tokio::test]
async fn hello_then_a_book_feed() {
    let addr = start(test_cfg()).await;
    let mut c = Client::connect(addr).await;
    let hello = c.recv("hello").await;
    assert_eq!(hello["levels"], 12);
    assert_eq!(hello["limits"]["max_qty"], 500);
    let b1 = c.recv("book").await;
    let b2 = c.recv("book").await;
    assert!(b2["seq"].as_u64() > b1["seq"].as_u64());
    assert!(b1["bids"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn two_visitors_trade_and_everyone_sees_it() {
    let addr = start(test_cfg()).await;
    let mut a = Client::connect(addr).await;
    let mut b = Client::connect(addr).await;
    a.recv("hello").await;
    b.recv("hello").await;

    a.send(json!({"type":"order","tag":1,"side":"ask","price":10000,"qty":10}))
        .await;
    let ack = a.recv("ack").await;
    assert_eq!(ack["tag"], 1);
    assert_eq!(ack["remaining"], 10);
    assert_eq!(ack["filled"], 0);
    let a_id = ack["id"].as_u64().unwrap();
    assert!(a_id >= 1 << 50, "visitor ids sit above the simulator's");
    assert!(ack["engine_ns"].as_u64().unwrap() < 1_000_000_000);
    assert!(ack["queue_ns"].as_u64().is_some());

    // B cannot cancel A's order
    b.send(json!({"type":"cancel","tag":9,"id":a_id})).await;
    let rej = b.recv("reject").await;
    assert_eq!(rej["tag"], 9);
    assert_eq!(rej["reason"], "not_your_order");

    // B buys 4
    b.send(json!({"type":"order","tag":2,"side":"bid","price":10000,"qty":4}))
        .await;
    let ack = b.recv("ack").await;
    assert_eq!(ack["filled"], 4);
    assert_eq!(ack["remaining"], 0);
    assert_eq!(ack["fills"], json!([[10000, 4]]));

    // A is told its order was partly filled
    let upd = a.recv("order_update").await;
    assert_eq!(upd["id"], a_id);
    assert_eq!(upd["remaining"], 6);

    // everyone's book shows 6 left, and the trade went by
    let book = b.book_where(|v| v["asks"] == json!([[10000, 6]])).await;
    assert!(book["stats"]["live_orders"].as_u64().unwrap() >= 1);

    // A cancels the rest
    a.send(json!({"type":"cancel","tag":3,"id":a_id})).await;
    let c = a.recv("cancelled").await;
    assert_eq!(c["id"], a_id);
    b.book_where(|v| v["asks"].as_array().unwrap().is_empty())
        .await;
}

#[tokio::test]
async fn you_cannot_cancel_someone_elses_order() {
    let addr = start(test_cfg()).await;
    let mut a = Client::connect(addr).await;
    let mut b = Client::connect(addr).await;
    a.send(json!({"type":"order","tag":1,"side":"bid","price":9990,"qty":5}))
        .await;
    let id = a.recv("ack").await["id"].as_u64().unwrap();
    b.send(json!({"type":"cancel","tag":2,"id":id})).await;
    assert_eq!(b.recv("reject").await["reason"], "not_your_order");
}

#[tokio::test]
async fn market_order_and_no_liquidity() {
    let addr = start(test_cfg()).await;
    let mut a = Client::connect(addr).await;
    a.send(json!({"type":"market","tag":1,"side":"bid","qty":5}))
        .await;
    assert_eq!(a.recv("reject").await["reason"], "no_liquidity");

    a.send(json!({"type":"order","tag":2,"side":"ask","price":10001,"qty":3}))
        .await;
    a.recv("ack").await;
    a.send(json!({"type":"market","tag":3,"side":"bid","qty":5}))
        .await;
    let ack = a.recv("ack").await;
    assert_eq!(ack["filled"], 3);
    assert_eq!(ack["remaining"], 0, "a market order never rests");
}

#[tokio::test]
async fn resting_orders_are_cancelled_when_the_visitor_leaves() {
    let addr = start(test_cfg()).await;
    let mut a = Client::connect(addr).await;
    let mut watcher = Client::connect(addr).await;
    a.send(json!({"type":"order","tag":1,"side":"bid","price":9990,"qty":5}))
        .await;
    a.recv("ack").await;
    watcher
        .book_where(|v| v["bids"] == json!([[9990, 5]]))
        .await;
    drop(a);
    watcher
        .book_where(|v| v["bids"].as_array().unwrap().is_empty())
        .await;
}

#[tokio::test]
async fn limits_are_enforced() {
    let mut cfg = test_cfg();
    cfg.max_open_orders = 3;
    let addr = start(cfg).await;
    let mut c = Client::connect(addr).await;

    c.send(json!({"type":"order","tag":1,"side":"bid","price":20000,"qty":1}))
        .await;
    assert_eq!(c.recv("reject").await["reason"], "price_out_of_band");
    c.send(json!({"type":"order","tag":2,"side":"bid","price":9990,"qty":501}))
        .await;
    assert_eq!(c.recv("reject").await["reason"], "qty_too_large");
    c.send(json!({"type":"order","tag":3,"side":"bid","price":9990,"qty":0}))
        .await;
    assert_eq!(c.recv("reject").await["reason"], "invalid_qty");

    for tag in 10..13 {
        c.send(json!({"type":"order","tag":tag,"side":"bid","price":9990,"qty":1}))
            .await;
        c.recv("ack").await;
    }
    c.send(json!({"type":"order","tag":13,"side":"bid","price":9990,"qty":1}))
        .await;
    assert_eq!(c.recv("reject").await["reason"], "too_many_orders");
}

#[tokio::test]
async fn commands_are_rate_limited() {
    let mut cfg = test_cfg();
    cfg.msg_rate = 1.0;
    cfg.msg_burst = 3.0;
    cfg.max_open_orders = 100;
    let addr = start(cfg).await;
    let mut c = Client::connect(addr).await;
    for tag in 0..10 {
        c.send(json!({"type":"order","tag":tag,"side":"bid","price":9990,"qty":1}))
            .await;
    }
    let (mut acks, mut limited) = (0, 0);
    while acks + limited < 10 {
        let m = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Message::Text(t) = c.ws.next().await.unwrap().unwrap() {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == "ack" || v["type"] == "reject" {
                        return v;
                    }
                }
            }
        })
        .await
        .unwrap();
        if m["type"] == "ack" {
            acks += 1;
        } else {
            assert_eq!(m["reason"], "rate_limited");
            limited += 1;
        }
    }
    assert!((3..=5).contains(&acks), "acks {acks}");
    assert!(limited >= 5, "limited {limited}");
}

#[tokio::test]
async fn garbage_gets_an_error_and_then_a_disconnect() {
    let addr = start(test_cfg()).await;
    let mut c = Client::connect(addr).await;
    c.send(json!({"type":"launch_missiles"})).await;
    assert_eq!(c.recv("error").await["message"], "bad_message");
    for _ in 0..6 {
        let _ = c.ws.send(Message::text("not json")).await;
    }
    // the server hangs up after repeated garbage
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match c.ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "server never closed the connection");
}

#[tokio::test]
async fn ping_is_echoed() {
    let addr = start(test_cfg()).await;
    let mut c = Client::connect(addr).await;
    c.send(json!({"type":"ping","t":123.5})).await;
    assert_eq!(c.recv("pong").await["t"], 123.5);
}

#[tokio::test]
async fn origin_allowlist() {
    let mut cfg = test_cfg();
    cfg.allowed_origins = vec!["https://good.example".into()];
    let addr = start(cfg).await;

    let with_origin = |o: Option<&str>| {
        let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
        if let Some(o) = o {
            req.headers_mut().insert("Origin", o.parse().unwrap());
        }
        req
    };
    assert!(connect_async(with_origin(None)).await.is_err(), "no origin");
    assert!(
        connect_async(with_origin(Some("https://evil.example")))
            .await
            .is_err()
    );
    assert!(
        connect_async(with_origin(Some("https://good.example")))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn per_ip_connection_cap() {
    let mut cfg = test_cfg();
    cfg.max_conns_per_ip = 2;
    let addr = start(cfg).await;
    let _a = Client::connect(addr).await;
    let _b = Client::connect(addr).await;
    assert!(
        connect_async(format!("ws://{addr}/ws")).await.is_err(),
        "third connection"
    );
}

#[tokio::test]
async fn live_simulator_keeps_the_book_moving() {
    let mut cfg = test_cfg();
    cfg.sim_rate = 2_000;
    let addr = start(cfg).await;
    let mut c = Client::connect(addr).await;
    // wait until the book has filled up and some trades have printed
    let book = c
        .book_where(|v| {
            v["bids"].as_array().unwrap().len() >= 6 && v["asks"].as_array().unwrap().len() >= 6
        })
        .await;
    let (best_bid, best_ask) = (
        book["bids"][0][0].as_u64().unwrap(),
        book["asks"][0][0].as_u64().unwrap(),
    );
    assert!(best_bid < best_ask, "crossed book {best_bid} {best_ask}");
    let later = c
        .book_where(|v| v["stats"]["cmds_per_sec"].as_u64().unwrap() > 1_000)
        .await;
    assert!(later["stats"]["cmds_total"].as_u64().unwrap() > 100);
}

#[tokio::test]
async fn connection_attempts_are_rate_limited() {
    let mut cfg = test_cfg();
    cfg.conn_burst = 3.0;
    cfg.conn_rate = 0.001;
    let addr = start(cfg).await;
    for _ in 0..3 {
        drop(Client::connect(addr).await);
    }
    assert!(
        connect_async(format!("ws://{addr}/ws")).await.is_err(),
        "fourth attempt inside the burst window"
    );
}

#[tokio::test]
async fn trusted_proxy_without_forwarded_for_is_refused() {
    let mut cfg = test_cfg();
    cfg.trust_proxy = true;
    let addr = start(cfg).await;
    assert!(connect_async(format!("ws://{addr}/ws")).await.is_err());
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("X-Forwarded-For", "203.0.113.7".parse().unwrap());
    assert!(connect_async(req).await.is_ok());
}
