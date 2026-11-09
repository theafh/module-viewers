#![cfg_attr(test, allow(clippy::unwrap_used, clippy::float_cmp))]

mod dates;
mod db;
mod harvest;
#[cfg(test)]
mod launcher_tests;
mod series;
#[cfg(test)]
mod test_support;
mod web;

use std::net::SocketAddr;
use std::time::Instant;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "task_viewer=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Keep a panic trail in the process log (nohup captures stderr).
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "task-viewer panicked");
        previous(info);
    }));

    // The runtime is explicit so its teardown is bounded: returning from an
    // async `main` drops the runtime, and that drop waits for every blocking
    // task without a bound, which a running harvest could stretch past the
    // launcher's stop window.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let deadline = runtime.block_on(serve_until_stopped())?;
    runtime.shutdown_timeout(web::remaining(deadline));
    Ok(())
}

/// Open the database, serve the dashboard, and return the deadline the drain
/// finished against once a stop signal arrives.
async fn serve_until_stopped() -> anyhow::Result<Instant> {
    let pool = db::init().await?;
    let state = web::AppState::new(pool, web::init_templates()?)?;
    let app = web::router(state.clone());
    // Test-only override: an isolated test runs the built binary on a free
    // loopback port beside a running dashboard. The host allowlist, the
    // same-origin `Origin` check, and every documented `7880` stay derived
    // from `web::PORT`, so what a browser reaches stays unchanged.
    let port = match std::env::var("TASK_VIEWER_PORT") {
        Ok(raw) => raw.trim().parse().unwrap_or(web::PORT),
        Err(_) => web::PORT,
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("task-viewer listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let deadline = web::serve(listener, app, state, web::SHUTDOWN_GRACE, shutdown_signal()).await;
    tracing::info!("task-viewer listener closed");
    Ok(deadline)
}

/// Resolve on SIGTERM or Ctrl-C, logging one line when the signal arrives.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(terminate) => terminate,
            Err(error) => {
                tracing::error!(%error, "the SIGTERM handler could not be installed");
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("stop signal received; shutting down");
                return;
            }
        };
        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received; shutting down"),
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received; shutting down"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("stop signal received; shutting down");
    }
}
