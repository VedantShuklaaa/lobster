use std::net::SocketAddr;

use server::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env();
    if cfg.allowed_origins.is_empty() {
        tracing::warn!("ALLOWED_ORIGINS is empty: any website can open a WebSocket to this server");
    }
    tracing::info!(
        bind = %cfg.bind,
        sim_rate = cfg.sim_rate,
        trust_proxy = cfg.trust_proxy,
        origins = ?cfg.allowed_origins,
        "starting"
    );

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let app = server::start(cfg);
    axum::serve(
        listener,
        server::router(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await?;
    Ok(())
}
