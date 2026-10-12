use std::net::SocketAddr;
use std::str::FromStr;

use engine::{Price, Qty};

/// Everything tunable about the server. Defaults are safe for a public demo;
/// `from_env` overrides them from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub allowed_origins: Vec<String>,
    pub trust_proxy: bool,

    pub sim_rate: u32,
    pub sim_seed: u64,
    pub center_price: Price,
    pub depth_levels: usize,
    pub snapshot_ms: u64,

    pub max_conns: usize,
    pub max_conns_per_ip: usize,
    pub max_open_orders: u32,
    pub max_qty: Qty,
    pub price_band: u32,
    pub msg_rate: f64,
    pub msg_burst: f64,
    pub idle_secs: u64,
    pub max_msg_bytes: usize,
    pub max_inflight: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".parse().unwrap(),
            allowed_origins: Vec::new(),
            trust_proxy: false,
            sim_rate: 2_000,
            sim_seed: 7,
            center_price: 10_000,
            depth_levels: 12,
            snapshot_ms: 50,
            max_conns: 200,
            max_conns_per_ip: 5,
            max_open_orders: 20,
            max_qty: 500,
            price_band: 200,
            msg_rate: 5.0,
            msg_burst: 10.0,
            idle_secs: 300,
            max_msg_bytes: 512,
            max_inflight: 4_096,
        }
    }
}

fn env<T: FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            bind: env("BIND", d.bind),
            allowed_origins: std::env::var("ALLOWED_ORIGINS")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            trust_proxy: env("TRUST_PROXY", d.trust_proxy),
            sim_rate: env("SIM_RATE", d.sim_rate),
            sim_seed: env("SIM_SEED", d.sim_seed),
            max_conns: env("MAX_CONNS", d.max_conns),
            max_conns_per_ip: env("MAX_CONNS_PER_IP", d.max_conns_per_ip),
            max_open_orders: env("MAX_OPEN_ORDERS", d.max_open_orders),
            ..d
        }
    }
}
