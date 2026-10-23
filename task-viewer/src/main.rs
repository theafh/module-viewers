#![cfg_attr(test, allow(clippy::unwrap_used, clippy::float_cmp))]

mod dates;
mod db;
mod harvest;
mod series;
#[cfg(test)]
mod test_support;
mod web;

use std::net::SocketAddr;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "task_viewer=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let pool = db::init().await?;
    let state = web::AppState::new(pool, web::init_templates()?);
    let app = web::router(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], web::PORT));
    tracing::info!("task-viewer listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
