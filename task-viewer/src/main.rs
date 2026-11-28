#![cfg_attr(test, allow(clippy::unwrap_used, clippy::float_cmp))]

mod dates;
mod db;
mod harvest;
#[cfg(all(test, unix))]
mod launcher_tests;
mod series;
#[cfg(test)]
mod test_support;
mod warn_once;
mod web;

use anyhow::Context as _;
use std::ffi::OsStr;
use std::future::Future;
use std::net::SocketAddr;
use std::time::Instant;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> anyhow::Result<()> {
    let (filter, warning) = log_filter(std::env::var_os("RUST_LOG").as_deref());
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
    if let Some(warning) = warning {
        warn_log_filter_fallback(&warning);
    }

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

/// The documented log filter, used when `RUST_LOG` is unset, blank, or cannot
/// be used as written.
const DEFAULT_LOG_FILTER: &str = "task_viewer=info";

/// The log filter named by the configured `RUST_LOG` value, together with at
/// most one warning for the caller to log once the subscriber is installed.
///
/// An unset or whitespace-only value takes [`DEFAULT_LOG_FILTER`] silently. A
/// value that is not valid UTF-8, or one the filter parser rejects, also takes
/// the default and returns a warning naming `RUST_LOG`, so the fallback is
/// never silent. Every other value is kept as written, `off` and a bare word
/// the parser reads as a target directive included.
fn log_filter(raw: Option<&OsStr>) -> (tracing_subscriber::EnvFilter, Option<String>) {
    let default = || tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER);
    let Some(raw) = raw else {
        return (default(), None);
    };
    let Some(raw) = raw.to_str() else {
        return (
            default(),
            Some(format!(
                "RUST_LOG is not valid UTF-8; using the default filter {DEFAULT_LOG_FILTER}"
            )),
        );
    };
    if raw.trim().is_empty() {
        return (default(), None);
    }
    match tracing_subscriber::EnvFilter::try_new(raw) {
        Ok(filter) => (filter, None),
        Err(_) => (
            default(),
            Some(format!(
                "RUST_LOG is {raw:?}, which the log filter parser rejects; using the default filter {DEFAULT_LOG_FILTER}"
            )),
        ),
    }
}

/// Log one `RUST_LOG` fallback warning.
///
/// Kept as a named function so a test can run the same emission `main` performs
/// after the subscriber is installed: a warning logged before `.init()` reaches
/// no subscriber.
fn warn_log_filter_fallback(warning: &str) {
    tracing::warn!("{}", warning);
}

/// Open the database, serve the dashboard, and return the deadline the drain
/// finished against once a stop signal arrives.
async fn serve_until_stopped() -> anyhow::Result<Instant> {
    let pool = db::init().await?;
    let state = web::AppState::new(pool, web::init_templates()?)?;
    let app = web::router(state.clone());
    // The variable is read on every build, release included, and the launcher
    // drops an inherited value and exports its own port. A non-default port is
    // for an isolated run beside a running dashboard: the host allowlist, the
    // same-origin `Origin` check, and every documented `7880` stay derived from
    // `web::PORT`, so a server on another port rejects a `Host` naming it.
    let port = configured_port(std::env::var_os("TASK_VIEWER_PORT").as_deref())?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = bind_and_log(addr).await?;
    let deadline = web::serve(
        listener,
        app,
        state,
        web::SHUTDOWN_GRACE,
        shutdown_signal(tokio::signal::ctrl_c()),
    )
    .await;
    tracing::info!("task-viewer listener closed");
    Ok(deadline)
}

/// The listen port named by the configured `TASK_VIEWER_PORT` value, or
/// `web::PORT` when no value is set. A value that is set but not valid UTF-8 is
/// a start-up failure naming the variable, the same way `database_path` refuses
/// a non-Unicode `TASK_VIEWER_DB`, so `None` here means only that the variable
/// is unset. A malformed value is likewise a start-up failure rather than a
/// silent fall back to the documented port, because a test that named a port
/// must never reach the dashboard's own.
fn configured_port(raw: Option<&OsStr>) -> anyhow::Result<u16> {
    let Some(raw) = raw else {
        return Ok(web::PORT);
    };
    let raw = raw.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "TASK_VIEWER_PORT is not valid UTF-8; set it to a port between 1 and 65535 or unset it"
        )
    })?;
    parse_port(raw)
}

/// Parse the configured `TASK_VIEWER_PORT` value into a listen port. Zero is
/// rejected along with a value that is not a whole number: binding port zero
/// would hand the server an ephemeral port that no launcher, test, or browser
/// can find, so it fails start-up naming the offending value instead.
fn parse_port(raw: &str) -> anyhow::Result<u16> {
    let trimmed = raw.trim();
    match trimmed.parse::<u16>() {
        Ok(0) => anyhow::bail!(
            "TASK_VIEWER_PORT is {trimmed}; set it to a port between 1 and 65535 or unset it"
        ),
        Ok(port) => Ok(port),
        Err(_) => anyhow::bail!(
            "TASK_VIEWER_PORT is {trimmed:?}; set it to a port between 1 and 65535 or unset it"
        ),
    }
}

/// Bind `addr` and log the listening line only once the bind has succeeded,
/// naming the address the kernel actually assigned. A failed bind carries the
/// address it tried, so a second instance on an occupied port reports that
/// port instead of a bare OS error.
async fn bind_and_log(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("task-viewer could not listen on http://{addr}"))?;
    let bound = listener.local_addr().unwrap_or(addr);
    tracing::info!("task-viewer listening on http://{bound}");
    Ok(listener)
}

/// Resolve on SIGTERM or Ctrl-C, logging one line when the signal arrives.
///
/// `ctrl_c` is injected so a test can hand in a future that fails to register
/// without touching the process's real Ctrl-C handling. A registration error is
/// never read as a received signal: with a SIGTERM handler installed the future
/// keeps waiting for that handler, and without one there is nothing left to
/// wait on, so the server stays up instead of draining on an error.
async fn shutdown_signal<F>(ctrl_c: F)
where
    F: Future<Output = std::io::Result<()>>,
{
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(terminate) => terminate,
            Err(error) => {
                tracing::error!(%error, "the SIGTERM handler could not be installed");
                match ctrl_c.await {
                    Ok(()) => tracing::info!("stop signal received; shutting down"),
                    Err(error) => {
                        tracing::error!(%error, "the Ctrl-C handler could not be installed");
                        std::future::pending::<()>().await;
                    }
                }
                return;
            }
        };
        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received; shutting down"),
            result = ctrl_c => match result {
                Ok(()) => tracing::info!("SIGINT received; shutting down"),
                Err(error) => {
                    tracing::error!(%error, "the Ctrl-C handler could not be installed");
                    terminate.recv().await;
                    tracing::info!("SIGTERM received; shutting down");
                }
            },
        }
    }
    #[cfg(not(unix))]
    {
        match ctrl_c.await {
            Ok(()) => tracing::info!("stop signal received; shutting down"),
            Err(error) => {
                tracing::error!(%error, "the Ctrl-C handler could not be installed");
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LOG_FILTER, bind_and_log, configured_port, log_filter, parse_port, shutdown_signal,
        warn_log_filter_fallback,
    };
    use std::ffi::OsStr;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt;

    /// The fields of one captured log event, flattened so a test can assert the
    /// message the event carried.
    struct LogFields(String);

    impl tracing::field::Visit for LogFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    /// A layer that captures every event's fields, so a test can read the log
    /// lines one start-up path emits.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedLogs {
        fn take(&self) -> Vec<String> {
            self.0
                .lock()
                .map(|mut lines| std::mem::take(&mut *lines))
                .unwrap_or_default()
        }
    }

    impl<S> tracing_subscriber::Layer<S> for CapturedLogs
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = LogFields(String::new());
            event.record(&mut fields);
            if let Ok(mut lines) = self.0.lock() {
                lines.push(fields.0);
            }
        }
    }

    /// Covers: an unset or blank `RUST_LOG` uses the documented default filter
    /// silently, so a blank value never narrows logging to `ERROR`.
    #[test]
    fn log_filter_defaults_when_unset_or_blank() {
        for raw in [None, Some(OsStr::new("")), Some(OsStr::new("  \t "))] {
            let (filter, warning) = log_filter(raw);
            assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER, "raw={raw:?}");
            assert!(warning.is_none(), "raw={raw:?}");
        }
    }

    /// Covers: a `RUST_LOG` the parser rejects falls back to the documented
    /// default and returns one warning naming the rejected value.
    #[test]
    fn log_filter_warns_about_a_rejected_value() {
        let (filter, warning) = log_filter(Some(OsStr::new("task_viewer=nope")));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        let warning = warning.expect("a rejected value warns");
        assert!(warning.contains("RUST_LOG"), "{warning}");
        assert!(warning.contains("task_viewer=nope"), "{warning}");
    }

    /// Covers: a `RUST_LOG` that is set but not valid UTF-8 falls back to the
    /// documented default and returns one warning naming `RUST_LOG`.
    #[cfg(unix)]
    #[test]
    fn log_filter_warns_about_a_non_unicode_value() {
        use std::os::unix::ffi::OsStrExt;

        let raw = OsStr::from_bytes(b"task_viewer=\xff");
        let (filter, warning) = log_filter(Some(raw));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        let warning = warning.expect("a non-Unicode value warns");
        assert!(warning.contains("RUST_LOG"), "{warning}");
        assert!(warning.contains("UTF-8"), "{warning}");
    }

    /// Covers: a valid `off` value is honored as written, so it disables the
    /// info level instead of falling back to the documented default.
    #[test]
    fn log_filter_honors_off() {
        let (filter, warning) = log_filter(Some(OsStr::new("off")));
        assert!(warning.is_none(), "a valid value draws no warning");
        assert_eq!(filter.to_string(), "off");
        assert_eq!(
            filter.max_level_hint(),
            Some(tracing::level_filters::LevelFilter::OFF)
        );
    }

    /// Covers: a valid `task_viewer=debug` value is honored as written.
    #[test]
    fn log_filter_honors_a_valid_directive() {
        let (filter, warning) = log_filter(Some(OsStr::new("task_viewer=debug")));
        assert!(warning.is_none(), "a valid value draws no warning");
        assert_eq!(filter.to_string(), "task_viewer=debug");
    }

    /// Covers: the warning `main` logs after `.init()` reaches the log, so a
    /// rejected `RUST_LOG` is reported rather than silently narrowed.
    #[test]
    fn log_filter_warning_reaches_the_log() {
        let (_, warning) = log_filter(Some(OsStr::new("task_viewer=nope")));
        let warning = warning.expect("a rejected value warns");
        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        warn_log_filter_fallback(&warning);
        drop(guard);

        let lines = captured.take();
        assert_eq!(lines.len(), 1, "one warning emits one line: {lines:?}");
        assert!(lines[0].contains("task_viewer=nope"), "{lines:?}");

        // The non-UTF-8 fallback goes through the same emission, so its
        // warning reaches the log the same way.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let (_, warning) = log_filter(Some(OsStr::from_bytes(b"task_viewer=\xff")));
            let warning = warning.expect("a non-Unicode value warns");
            let captured = CapturedLogs::default();
            let subscriber = tracing_subscriber::registry().with(captured.clone());
            let guard = tracing::subscriber::set_default(subscriber);
            warn_log_filter_fallback(&warning);
            drop(guard);

            let lines = captured.take();
            assert_eq!(lines.len(), 1, "one warning emits one line: {lines:?}");
            assert!(lines[0].contains("RUST_LOG"), "{lines:?}");
        }
    }

    /// Covers: a valid port is taken as given, with surrounding padding
    /// trimmed, and an unset value falls back to the documented port.
    #[test]
    fn port_parser_accepts_a_valid_value() {
        assert_eq!(parse_port("7880").expect("a valid port"), 7880);
        assert_eq!(parse_port(" 8080 ").expect("a padded port"), 8080);
        assert_eq!(parse_port("65535").expect("the highest port"), 65535);
        assert_eq!(
            configured_port(None).expect("the documented port"),
            crate::web::PORT
        );
        assert_eq!(
            configured_port(Some(OsStr::new("8080"))).expect("a configured port"),
            8080
        );
    }

    /// Covers: `TASK_VIEWER_PORT=0` is rejected instead of binding an ephemeral
    /// port the launcher could never find.
    #[test]
    fn port_parser_rejects_zero() {
        let error = parse_port("0").expect_err("zero must be rejected");
        let message = error.to_string();
        assert!(message.contains('0'), "{message}");
        assert!(
            configured_port(Some(OsStr::new(" 0 "))).is_err(),
            "a padded zero is rejected too"
        );
    }

    /// Covers: `TASK_VIEWER_PORT=70000` fails naming the value it could not
    /// represent as a port.
    #[test]
    fn port_parser_rejects_an_overflowing_value() {
        let error = parse_port("70000").expect_err("a value past u16 must be rejected");
        assert!(error.to_string().contains("70000"), "{error}");
    }

    /// Covers: `TASK_VIEWER_PORT=abc` fails naming the value rather than
    /// binding the documented port, and an empty value fails the same way.
    #[test]
    fn port_parser_rejects_a_non_numeric_value() {
        let error = parse_port("abc").expect_err("a non-numeric value must be rejected");
        assert!(error.to_string().contains("abc"), "{error}");
        assert!(parse_port("").is_err(), "an empty value must be rejected");
        assert!(
            configured_port(Some(OsStr::new("abc"))).is_err(),
            "the caller fails start-up instead of falling back"
        );
    }

    /// Covers: a `TASK_VIEWER_PORT` that is set but not valid UTF-8 fails
    /// start-up naming the variable instead of falling back to the documented
    /// port, the same way a non-Unicode `TASK_VIEWER_DB` is refused.
    #[cfg(unix)]
    #[test]
    fn configured_port_rejects_a_non_unicode_value() {
        use std::os::unix::ffi::OsStrExt;

        let raw = OsStr::from_bytes(b"80\xff80");
        let error =
            configured_port(Some(raw)).expect_err("a non-Unicode value must not reach the parser");
        let message = format!("{error:#}");
        assert!(message.contains("TASK_VIEWER_PORT"), "{message}");
        assert!(message.contains("UTF-8"), "{message}");
    }

    /// Covers: the listening line is emitted only after a successful bind and
    /// names the ephemeral port `local_addr()` reported rather than `:0`.
    #[tokio::test(flavor = "current_thread")]
    async fn bind_and_log_names_the_bound_address() {
        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let listener = bind_and_log(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        drop(guard);

        let lines = captured.take();
        assert_eq!(lines.len(), 1, "one bind emits one line: {lines:?}");
        assert!(
            lines[0].contains(&format!("http://127.0.0.1:{port}")),
            "the line names the bound address: {lines:?}"
        );
        assert_ne!(port, 0, "the kernel assigned a real port");
        assert!(
            !lines[0].contains(":0"),
            "the line never carries the requested port zero: {lines:?}"
        );
    }

    /// Covers: a bind on an occupied port logs no listening line and fails
    /// with an error naming the address it tried.
    #[tokio::test(flavor = "current_thread")]
    async fn bind_and_log_reports_the_address_when_the_port_is_taken() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("occupy an ephemeral port");
        let addr = occupied.local_addr().expect("the occupied address");

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let error = bind_and_log(addr)
            .await
            .expect_err("a taken port must fail the bind");
        drop(guard);

        let message = format!("{error:#}");
        assert!(message.contains(&addr.to_string()), "{message}");
        let lines = captured.take();
        assert!(
            lines.iter().all(|line| !line.contains("listening")),
            "a failed bind logs no listening line: {lines:?}"
        );
    }

    /// Covers: a `ctrl_c` registration error does not resolve the shutdown
    /// future, so the server keeps waiting for SIGTERM instead of stopping.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_signal_waits_when_ctrl_c_registration_fails() {
        let failed_registration = async {
            Err(std::io::Error::other(
                "the Ctrl-C handler could not be installed",
            ))
        };
        let waited = tokio::time::timeout(
            Duration::from_millis(100),
            shutdown_signal(failed_registration),
        )
        .await;
        assert!(
            waited.is_err(),
            "a registration error must leave the shutdown future pending"
        );
    }
}
