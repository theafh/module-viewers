//! Process-level shutdown test: the built binary stops accepting, drains, and
//! exits on its own when it receives SIGTERM or SIGINT, inside the launcher's
//! five-second stop window.
//!
//! `task-viewer` declares no library target, so this test reaches the binary
//! only through `env!("CARGO_BIN_EXE_task-viewer")` and carries its own
//! per-test temp-dir helper on the `task-viewer_<name>_<pid>_<nanos>_<n>`
//! pattern the inline suites use.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The launcher's stop window: ten polls at half a second before `kill -9`.
const STOP_WINDOW: Duration = Duration::from_secs(5);
const START_WINDOW: Duration = Duration::from_secs(20);

static NEXT: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock should be after unix epoch")
        .as_nanos();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "task-viewer_{name}_{}_{}_{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// A free loopback port, so this test runs beside a dashboard on 7880.
fn free_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("listener addr").port()
}

/// One `GET /` carrying the loopback `Host` a same-machine browser sends. The
/// bare name is accepted with or without the documented port, so an isolated
/// server on a free port answers it.
fn root_status(port: u16) -> Option<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response.lines().next()?.split_whitespace().nth(1)?;
    status.parse().ok()
}

fn wait_until_serving(child: &mut Child, port: u16, log: &PathBuf) {
    let deadline = Instant::now() + START_WINDOW;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll the server") {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            panic!("the server exited before serving with {status:?}\n{text}");
        }
        if root_status(port) == Some(200) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let text = std::fs::read_to_string(log).unwrap_or_default();
    panic!("the server never answered GET / with 200\n{text}");
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -{signal} {pid} succeeded");
}

/// Wait for the server to exit by itself, failing the test if it needs the
/// forced kill the launcher would send.
fn wait_for_exit(child: &mut Child, window: Duration) -> ExitStatus {
    let deadline = Instant::now() + window;
    loop {
        if let Some(status) = child.try_wait().expect("poll the server") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server did not exit inside the launcher's five-second window");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Spawn the built binary on a free port with its own database, wait until it
/// serves, send `signal`, and assert it exits on its own with a success status.
fn run_and_stop(signal: &str) {
    let dir = unique_temp_dir(&format!("shutdown_{}", signal.to_lowercase()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let port = free_loopback_port();
    let log_path = dir.join("server.log");
    let log = std::fs::File::create(&log_path).expect("server log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_task-viewer"))
        .env("TASK_VIEWER_DB", dir.join("test.db"))
        .env("TASK_VIEWER_PORT", port.to_string())
        .env("RUST_LOG", "task_viewer=info")
        .stdout(Stdio::from(log.try_clone().expect("log clone")))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn the built binary");

    wait_until_serving(&mut child, port, &log_path);
    let started = Instant::now();
    send_signal(child.id(), signal);
    let status = wait_for_exit(&mut child, STOP_WINDOW);
    let elapsed = started.elapsed();
    let text = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        status.success(),
        "the server exited with {status:?} on SIG{signal}\n{text}"
    );
    assert!(
        elapsed < STOP_WINDOW,
        "the server exited {elapsed:?} after SIG{signal}, inside the launcher's window"
    );
}

/// Acceptance: SIGTERM and SIGINT each let the server exit on its own with a
/// success status inside the launcher's five-second window.
#[test]
fn task_viewer_exits_on_sigterm_and_sigint() {
    for signal in ["TERM", "INT"] {
        run_and_stop(signal);
    }
}
