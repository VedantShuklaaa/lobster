//! lobster live server: the engine behind a WebSocket, plus a market simulator.

pub mod config;
pub mod engine_thread;
pub mod limits;
pub mod protocol;
pub mod session;
pub mod sim;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use config::Config;
use engine_thread::EngineHandle;
use limits::ConnTracker;

pub struct App {
    pub cfg: Config,
    pub engine: EngineHandle,
    pub tracker: Arc<ConnTracker>,
}

/// Starts the engine thread and returns the shared state.
pub fn start(cfg: Config) -> Arc<App> {
    let engine = engine_thread::spawn(&cfg);
    let tracker = ConnTracker::new(cfg.max_conns, cfg.max_conns_per_ip);
    Arc::new(App {
        cfg,
        engine,
        tracker,
    })
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/ws", get(session::ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(app)
}
