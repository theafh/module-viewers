//! Process-level launcher tests: two invocations racing one launcher copy, a
//! stale lock left by a dead holder, and the launcher's own bind port.
//!
//! Each test stages a checkout-shaped root: a `scripts/task-viewer.sh` copy, a
//! `target/release/task-viewer` symlink to `env!("CARGO_BIN_EXE_task-viewer")`,
//! and no-op `cargo`, `open`, `xdg-open`, `osascript`, and `notify-send` in
//! `<temp home>/.cargo/bin`, which the launcher's PATH reaches first. `HOME` and
//! `XDG_STATE_HOME` sit inside the same temp dir, so the PID, log, stamp, and
//! lock files stay out of live directories, and the copy's `PORT` line names a
//! free loopback port this test never shares with a running dashboard.
//!
//! `task-viewer` declares no library target, so this test carries its own
//! per-test temp-dir helper on the `task-viewer_<name>_<pid>_<nanos>_<n>`
//! pattern the inline suites use.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long one launcher command may take: a start waits up to ten seconds for
/// its server to own the port, and the loser waits out the winner's lock first.
const COMMAND_WINDOW: Duration = Duration::from_secs(60);
const SERVER_WINDOW: Duration = Duration::from_secs(15);

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

/// The launcher lives beside the crate, at the workspace root.
fn repo_launcher() -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("task-viewer sits inside the workspace root");
    repo.join("scripts/task-viewer.sh")
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod script");
}

/// One checkout-shaped root, its private home, and the port its launcher copy
/// probes and binds.
struct Root {
    dir: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    launcher: PathBuf,
    port: u16,
}

impl Root {
    fn new(name: &str, port: u16) -> Root {
        let dir = unique_temp_dir(name);
        let home = dir.join("home");
        let xdg = dir.join("state");
        fs::create_dir_all(home.join(".cargo/bin")).expect("fake tool dir");
        fs::create_dir_all(dir.join("scripts")).expect("scripts dir");
        fs::create_dir_all(dir.join("target/release")).expect("release dir");

        // The copy names the free port in its `PORT` line, which the launcher
        // hands to the server as `TASK_VIEWER_PORT`, so probes and bind agree.
        let source = fs::read_to_string(repo_launcher()).expect("read the launcher");
        let staged = source.replacen("PORT=7880\n", &format!("PORT={port}\n"), 1);
        assert_ne!(staged, source, "the launcher must carry one PORT line");
        let launcher = dir.join("scripts/task-viewer.sh");
        fs::write(&launcher, staged).expect("write the launcher copy");

        symlink(
            env!("CARGO_BIN_EXE_task-viewer"),
            dir.join("target/release/task-viewer"),
        )
        .expect("stage the built binary");

        for tool in ["cargo", "open", "xdg-open", "osascript", "notify-send"] {
            write_executable(&home.join(".cargo/bin").join(tool), "#!/bin/bash\nexit 0\n");
        }

        Root {
            dir,
            home,
            xdg,
            launcher,
            port,
        }
    }

    /// The launcher's state directory for this root's home, by its own rule.
    fn state_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Logs/task-viewer")
        } else {
            self.xdg.join("task-viewer")
        }
    }

    fn pid_file(&self) -> PathBuf {
        self.state_dir().join("task-viewer.pid")
    }

    fn lock_dir(&self) -> PathBuf {
        self.state_dir().join("task-viewer.lock")
    }

    fn command(&self) -> Command {
        let mut command = Command::new("bash");
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg(&self.launcher)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.xdg)
            // The launcher owns its port and its database path; an inherited
            // value from the test runner must not reach the server.
            .env_remove("TASK_VIEWER_PORT")
            .env_remove("TASK_VIEWER_DB")
            .env("RUST_LOG", "warn")
            .current_dir(&self.dir)
            .stdin(Stdio::null());
        command
    }

    fn spawn(&self, subcommand: &str) -> Child {
        self.command()
            .arg(subcommand)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the launcher copy")
    }

    fn read_pid(&self) -> Option<u32> {
        let text = fs::read_to_string(self.pid_file()).ok()?;
        text.trim().parse().ok()
    }
}

/// Wait for a child to finish, then collect its output, failing the test if it
/// overruns the window instead of hanging the suite.
fn wait_output(mut child: Child, window: Duration) -> Output {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll the child").is_some() {
            return child.wait_with_output().expect("collect the output");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the launcher did not finish inside {window:?}");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One `GET /` carrying the loopback `Host` a same-machine browser sends.
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

fn wait_for_server(port: u16, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if root_status(port) == Some(200) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("no server answered GET / with 200 on port {port} inside {window:?}");
}

fn answers(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .expect("run kill -0")
        .success()
}

/// The PID named in a `task-viewer started (PID: N)` line.
fn started_pid(output: &Output) -> Option<u32> {
    let text = stdout(output);
    let marker = "task-viewer started (PID: ";
    let rest = text.split_once(marker)?.1;
    rest.split_once(')')?.0.trim().parse().ok()
}

/// A PID that is certainly not alive: a child this test already reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("true")
        .spawn()
        .expect("spawn a short-lived child");
    let pid = child.id();
    child.wait().expect("reap the short-lived child");
    pid
}

/// Assert the outcome of two racing starts: one server, one start report, one
/// already-running report, and the loser leaving the winner's PID file alone.
fn assert_one_start_won(first: &Output, second: &Output, root: &Root) -> u32 {
    let (winner, loser) = if started_pid(first).is_some() {
        (first, second)
    } else {
        (second, first)
    };
    let pid = started_pid(winner).unwrap_or_else(|| {
        panic!(
            "neither start reported a server\nfirst: {}\nsecond: {}",
            stdout(first),
            stdout(second)
        )
    });
    assert!(
        stdout(loser).contains("already running"),
        "the losing start must report an already-running server\nloser: {}\nstderr: {}",
        stdout(loser),
        stderr(loser),
    );
    assert!(
        !stdout(loser).contains("task-viewer started"),
        "the losing start must not report a start: {}",
        stdout(loser),
    );
    let recorded = root.read_pid().expect("the winner's PID file");
    assert_eq!(
        recorded, pid,
        "the loser must leave the winner's PID file in place",
    );
    assert!(process_alive(pid), "the started server must be alive");
    pid
}

/// Stop the server and assert the stop removed the PID file and the process.
fn stop_and_assert(root: &Root, pid: u32) {
    let stop = wait_output(root.spawn("stop"), COMMAND_WINDOW);
    assert!(
        stop.status.success(),
        "stop failed: {}\n{}",
        stdout(&stop),
        stderr(&stop),
    );
    assert!(
        stdout(&stop).contains("task-viewer stopped"),
        "stop must report the stop: {}",
        stdout(&stop),
    );
    assert!(
        !root.pid_file().exists(),
        "the stop must remove the PID file",
    );
    assert!(!process_alive(pid), "the stopped server must be gone");
    assert!(
        root_status(root.port).is_none(),
        "the port must stop answering once the server is stopped",
    );
}

/// Covers: two concurrent `start` invocations leave one running server on the
/// copy's port, one reporting a start and the other an already-running server,
/// the loser leaves the winner's PID file in place, and a following `stop`
/// removes that file and leaves no live process.
#[test]
fn concurrent_starts_serialize_on_one_lock() {
    let port = free_loopback_port();
    let root = Root::new("launcher_race", port);

    // Both invocations start together against one launcher copy: the lock
    // serializes them, so the second sees the first's server and reports it.
    let first = root.spawn("start");
    let second = root.spawn("start");
    let first = wait_output(first, COMMAND_WINDOW);
    let second = wait_output(second, COMMAND_WINDOW);
    wait_for_server(port, SERVER_WINDOW);
    let pid = assert_one_start_won(&first, &second, &root);
    stop_and_assert(&root, pid);

    let _ = fs::remove_dir_all(&root.dir);
}

/// Covers: a lock directory whose recorded PID is not alive is reclaimed by the
/// next invocation, which starts the server, and two invocations racing on one
/// stale lock leave exactly one running server.
#[test]
fn a_stale_lock_is_reclaimed_once() {
    let port = free_loopback_port();
    let root = Root::new("launcher_stale_lock", port);
    fs::create_dir_all(root.lock_dir()).expect("stage the stale lock");
    fs::write(root.lock_dir().join("pid"), format!("{}\n", dead_pid()))
        .expect("record a dead holder");

    let first = root.spawn("start");
    let second = root.spawn("start");
    let first = wait_output(first, COMMAND_WINDOW);
    let second = wait_output(second, COMMAND_WINDOW);
    wait_for_server(port, SERVER_WINDOW);

    let pid = assert_one_start_won(&first, &second, &root);
    assert!(
        !root.lock_dir().exists(),
        "the reclaimed lock directory must be gone once the holder releases",
    );
    stop_and_assert(&root, pid);

    let _ = fs::remove_dir_all(&root.dir);
}

/// Covers: an exported `TASK_VIEWER_PORT` does not change the port the
/// launcher's server binds.
#[test]
fn inherited_task_viewer_port_does_not_move_the_bind() {
    let port = free_loopback_port();
    let inherited = free_loopback_port();
    let root = Root::new("launcher_port", port);

    let start = wait_output(
        root.command()
            .arg("start")
            .env("TASK_VIEWER_PORT", inherited.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the launcher copy"),
        COMMAND_WINDOW,
    );
    assert!(
        start.status.success(),
        "start failed: {}\n{}",
        stdout(&start),
        stderr(&start),
    );
    wait_for_server(port, SERVER_WINDOW);
    assert!(
        !answers(inherited),
        "the inherited TASK_VIEWER_PORT must not receive the server",
    );

    let pid = root.read_pid().expect("the started server's PID file");
    stop_and_assert(&root, pid);

    let _ = fs::remove_dir_all(&root.dir);
}
