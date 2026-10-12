//! One WebSocket session: checks the visitor in, forwards their commands to the engine thread,
//! and writes back the public book feed plus their private replies.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::App;
use crate::config::Config;
use crate::engine_thread::Req;
use crate::limits::{ConnGuard, TokenBucket};
use crate::protocol::{ClientMsg, Limits, ServerMsg};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BAD_MESSAGES: u32 = 5;

fn origin_ok(cfg: &Config, headers: &HeaderMap) -> bool {
    cfg.allowed_origins.is_empty()
        || headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| cfg.allowed_origins.iter().any(|a| a == o))
}

/// Behind Caddy the peer is always localhost; Caddy appends the real client to
/// X-Forwarded-For, so the last entry is the one we can trust.
fn client_ip(cfg: &Config, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if cfg.trust_proxy
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|s| s.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(app): State<Arc<App>>,
) -> Response {
    if !origin_ok(&app.cfg, &headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let ip = client_ip(&app.cfg, &headers, peer);
    let Some(guard) = app.tracker.try_acquire(ip) else {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    };
    ws.max_message_size(app.cfg.max_msg_bytes)
        .max_frame_size(app.cfg.max_msg_bytes)
        .on_upgrade(move |socket| session(socket, app, guard))
}

async fn send_text(sink: &mut SplitSink<WebSocket, Message>, text: Utf8Bytes) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::Text(text))).await,
        Ok(Ok(()))
    )
}

async fn send_msg(sink: &mut SplitSink<WebSocket, Message>, msg: &ServerMsg) -> bool {
    let json = serde_json::to_string(msg).expect("serialize reply");
    send_text(sink, json.into()).await
}

async fn session(socket: WebSocket, app: Arc<App>, _guard: ConnGuard) {
    let cfg = &app.cfg;
    let engine = &app.engine;
    let sid = engine.new_session();
    let (reply_tx, mut reply_rx) = mpsc::channel::<ServerMsg>(256);
    engine.register(sid, reply_tx);
    let mut book_rx = engine.subscribe();
    let (mut sink, mut stream) = socket.split();

    let hello = ServerMsg::Hello {
        session: sid,
        levels: cfg.depth_levels,
        snapshot_ms: cfg.snapshot_ms,
        limits: Limits {
            max_qty: cfg.max_qty,
            price_band: cfg.price_band,
            max_open_orders: cfg.max_open_orders,
            msg_rate: cfg.msg_rate,
        },
    };
    let mut alive = send_msg(&mut sink, &hello).await;
    let first = engine.latest();
    if alive && first.starts_with("{\"type\"") {
        alive = send_text(&mut sink, Utf8Bytes::from(&*first)).await;
    }

    let now = std::time::Instant::now();
    let mut orders_bucket = TokenBucket::new(cfg.msg_rate, cfg.msg_burst, now);
    let mut ping_bucket = TokenBucket::new(10.0, 20.0, now);
    let mut bad = 0u32;
    let idle_for = Duration::from_secs(cfg.idle_secs);
    let idle = tokio::time::sleep(idle_for);
    tokio::pin!(idle);

    while alive {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(msg)) = incoming else { break };
                idle.as_mut().reset(Instant::now() + idle_for);
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Binary(_) => break,
                };
                let now = std::time::Instant::now();
                match serde_json::from_str::<ClientMsg>(&text) {
                    Err(_) => {
                        bad += 1;
                        alive = send_msg(&mut sink, &ServerMsg::Error { message: "bad_message" }).await
                            && bad < MAX_BAD_MESSAGES;
                    }
                    Ok(ClientMsg::Ping { t }) => {
                        if ping_bucket.allow(now) {
                            alive = send_msg(&mut sink, &ServerMsg::Pong { t }).await;
                        }
                    }
                    Ok(cmd) => {
                        let (tag, req) = match cmd {
                            ClientMsg::Order { tag, side, price, qty } => (tag, Req::Limit { side, price, qty }),
                            ClientMsg::Market { tag, side, qty } => (tag, Req::Market { side, qty }),
                            ClientMsg::Cancel { tag, id } => (tag, Req::Cancel { id }),
                            ClientMsg::Ping { .. } => unreachable!(),
                        };
                        let refusal = if !orders_bucket.allow(now) {
                            Some("rate_limited")
                        } else if engine.submit(sid, tag, req).is_err() {
                            Some("busy")
                        } else {
                            None
                        };
                        if let Some(reason) = refusal {
                            alive = send_msg(&mut sink, &ServerMsg::Reject { tag, reason }).await;
                        }
                    }
                }
            }
            reply = reply_rx.recv() => {
                let Some(reply) = reply else { break };
                alive = send_msg(&mut sink, &reply).await;
            }
            book = book_rx.recv() => match book {
                Ok(json) => alive = send_text(&mut sink, Utf8Bytes::from(&*json)).await,
                Err(broadcast::error::RecvError::Lagged(_)) => {} // skip ahead, the next snapshot is complete
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = &mut idle => break,
        }
    }

    engine.unregister(sid);
}
