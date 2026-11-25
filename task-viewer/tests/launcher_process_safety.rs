//! Process-level launcher tests: two invocations racing one launcher copy, a
//! stale lock left by a dead holder, a lock held by a live process that is not
//! this launcher, a lock renamed away before its holder entered the critical
//! section, and the launcher's own bind port.
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
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long one launcher command may take: a start waits up to ten seconds for
/// its server to own the port, and the loser waits out the winner's lock first.
const COMMAND_WINDOW: Duration = Duration::from_secs(60);
const SERVER_WINDOW: Duration = Duration::from_secs(15);
/// How long a lock reclaim may take. A waiter that reads the holder's recorded
/// command line reclaims at once; one that waits on the live PID instead never
/// returns, which this window turns into a failure rather than a hung suite.
const RECLAIM_WINDOW: Duration = Duration::from_secs(20);

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

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod script");
}

/// One checkout-shaped root, its private home, and the port its launcher copy
/// probes and binds. Dropping it stops any server the case started and removes
/// the whole temp root, so a panicking assertion unwinds through the same
/// cleanup a passing case takes.
struct Root {
    /// The unique temp root, which is `dir` itself unless the case nests the
    /// checkout under a path holding a space.
    root: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    launcher: PathBuf,
    port: u16,
}

impl Root {
    fn new(name: &str, port: u16) -> Root {
        let root = unique_temp_dir(name);
        Root::from_dir(root.clone(), root, port)
    }

    /// A root whose own path holds a space, as `install-desktop.sh` supports.
    fn new_under_a_space(name: &str, port: u16) -> Root {
        let root = unique_temp_dir(name);
        let dir = root.join("My Projects");
        Root::from_dir(root, dir, port)
    }

    fn from_dir(root: PathBuf, dir: PathBuf, port: u16) -> Root {
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

        #[cfg(unix)]
        symlink(
            env!("CARGO_BIN_EXE_task-viewer"),
            dir.join("target/release/task-viewer"),
        )
        .expect("stage the built binary");

        for tool in ["cargo", "open", "xdg-open", "osascript", "notify-send"] {
            write_executable(&home.join(".cargo/bin").join(tool), "#!/bin/bash\nexit 0\n");
        }

        Root {
            root,
            dir,
            home,
            xdg,
            launcher,
            port,
        }
    }

    /// Stop any server this root started, without panicking: `Drop` runs while
    /// a failing assertion unwinds, so a second panic here would abort the test
    /// process instead of reporting the original failure.
    fn stop_any_server(&self) {
        let Some(pid) = self.read_pid() else {
            return;
        };
        if let Ok(mut child) = self
            .command()
            .arg("stop")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + COMMAND_WINDOW;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        // The launcher's stop window can still leave a detached server behind;
        // the test's own cleanup sends the signal the launcher would have.
        if process_alive_for_cleanup(pid) {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
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
        let mut command = self.staged_env("bash");
        command.arg("--noprofile").arg("--norc").arg(&self.launcher);
        command
    }

    /// `bash` in this root's staged environment, with the launcher's `HOME`,
    /// `XDG_STATE_HOME`, and cleared overrides already in place.
    fn staged_env(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
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

    /// `bash` running `script` with the launcher copy reachable as `$1`. Sourcing
    /// it skips the dispatcher, so a test drives the lock helpers without
    /// starting or stopping a server.
    fn source_command(&self, script: &str) -> Command {
        let mut command = self.staged_env("bash");
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(script)
            .arg("bash")
            .arg(&self.launcher);
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

/// Every path out of a case, a panicking assertion included, stops the server
/// and removes the temp root, so a failed run leaves no detached server on the
/// port and no directory on disk.
impl Drop for Root {
    fn drop(&mut self) {
        self.stop_any_server();
        let _ = fs::remove_dir_all(&self.root);
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

/// One `GET /` carrying the bare loopback `Host`, which is the form the server's
/// allowlist accepts: it is built from the compile-time 7880, so the
/// `Host: 127.0.0.1:<port>` a browser sends on the port this test binds is
/// refused. The helper therefore proves the server answers, not that a browser
/// can load the page.
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

/// Whether `pid` names a live process, read through `kill -0`. An assertion
/// path calls this, so a `kill -0` that cannot run panics with a clear message
/// instead of answering "not alive": a spawn failure must never make a
/// "must be gone" assertion hold vacuously.
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .expect("run kill -0")
        .success()
}

/// A non-panicking liveness read for `Root`'s cleanup, which runs while a
/// failing assertion unwinds: a `kill -0` that cannot run reads as "unknown"
/// and returns `false`, so `Drop` never panics a second time and aborts the
/// test process. A `kill -0` that does run still decides.
fn process_alive_for_cleanup(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
}

/// Covers: a lock directory whose recorded PID is not alive and carries no
/// recorded command line is reclaimed, and two invocations racing on one stale
/// lock leave exactly one running server. This is the missing-command retry:
/// the launcher waits out its grace and reclaims without a `cmd` file.
#[test]
fn a_stale_lock_without_a_command_line_is_reclaimed_once() {
    let port = free_loopback_port();
    let root = Root::new("launcher_stale_lock_no_cmd", port);
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
}

/// Covers: a lock directory whose recorded PID is not alive and whose recorded
/// command line names this launcher is reclaimed by the next invocation, which
/// starts the server, and two invocations racing on one stale lock leave exactly
/// one running server. The recorded launcher line is what makes this case reach
/// the `kill -0` branch: a dead PID under a launcher-shaped line is reclaimed by
/// the failed liveness check. Delete that check and the empty live command line
/// reads as a real holder, so the case stalls to the reclaim window and fails.
#[test]
fn a_stale_lock_with_a_launcher_command_line_is_reclaimed_once() {
    let port = free_loopback_port();
    let root = Root::new("launcher_stale_lock", port);
    fs::create_dir_all(root.lock_dir()).expect("stage the stale lock");
    fs::write(root.lock_dir().join("pid"), format!("{}\n", dead_pid()))
        .expect("record a dead holder");
    // The dead holder's line names this launcher, so only the liveness check
    // separates it from a real holder that must be waited for.
    fs::write(
        root.lock_dir().join("cmd"),
        format!("bash {} start\n", root.launcher.display()),
    )
    .expect("record a launcher-shaped command line");

    let first = root.spawn("start");
    let second = root.spawn("start");
    let first = wait_output(first, COMMAND_WINDOW);
    let second = wait_output(second, COMMAND_WINDOW);
    wait_for_server(port, SERVER_WINDOW);

    let pid = assert_one_start_won(&first, &second, &root);
    stop_and_assert(&root, pid);
}

/// Covers: a lock whose recorded PID belongs to a live process that is not this
/// launcher is reclaimed instead of waited for. The recorded command line is what
/// separates a reused PID from a real holder, so a waiter that tested only
/// `kill -0` would hold the command on the unrelated process until the window
/// expires, and this test fails on that timeout.
#[test]
fn a_reused_pid_lock_is_reclaimed() {
    let port = free_loopback_port();
    let root = Root::new("launcher_reused_pid", port);

    // A live process that is certainly not this launcher, standing in for a PID
    // the system reused, with its own command line recorded in the lock.
    let mut squatter = Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn the unrelated live process");
    fs::create_dir_all(root.lock_dir()).expect("stage the reused-PID lock");
    fs::write(root.lock_dir().join("pid"), format!("{}\n", squatter.id()))
        .expect("record the live PID");
    fs::write(root.lock_dir().join("cmd"), "sleep 120\n")
        .expect("record the unrelated command line");

    let status = wait_output(root.spawn("status"), RECLAIM_WINDOW);
    let _ = squatter.kill();
    let _ = squatter.wait();

    assert_eq!(
        status.status.code(),
        Some(1),
        "a stopped status must exit 1: {}\n{}",
        stdout(&status),
        stderr(&status),
    );
    assert!(
        stdout(&status).contains("task-viewer is not running"),
        "the reclaimed lock must let status read the launcher's own state: {}",
        stdout(&status),
    );
    assert!(
        !root.lock_dir().exists(),
        "the reclaimed lock must be released once the command finishes",
    );
}

/// Covers: a lock whose recorded PID belongs to a live unrelated process while
/// the recorded command line *does* name the launcher is still reclaimed. The
/// recorded line survives from a killed holder, so a waiter that trusted it
/// alone would wait on a reused PID forever; the live process's own line decides.
/// This case hangs on the recorded-line-only implementation and fails on the
/// window instead.
#[test]
fn a_reused_pid_whose_recorded_line_names_the_launcher_is_reclaimed() {
    let port = free_loopback_port();
    let root = Root::new("launcher_reused_launcher_pid", port);

    let mut squatter = Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn the unrelated live process");
    fs::create_dir_all(root.lock_dir()).expect("stage the reused-PID lock");
    fs::write(root.lock_dir().join("pid"), format!("{}\n", squatter.id()))
        .expect("record the live PID");
    // The dead holder's recorded line names the launcher, so a recorded-line-only
    // rule cannot tell it from a real holder.
    fs::write(
        root.lock_dir().join("cmd"),
        format!("bash {} status\n", root.launcher.display()),
    )
    .expect("record a launcher-shaped command line");

    let status = wait_output(root.spawn("status"), RECLAIM_WINDOW);
    let _ = squatter.kill();
    let _ = squatter.wait();

    assert_eq!(
        status.status.code(),
        Some(1),
        "a stopped status must exit 1: {}\n{}",
        stdout(&status),
        stderr(&status),
    );
    assert!(
        stdout(&status).contains("task-viewer is not running"),
        "the reclaimed lock must let status read the launcher's own state: {}",
        stdout(&status),
    );
    assert!(
        !root.lock_dir().exists(),
        "the reclaimed lock must be released once the command finishes",
    );
}

/// Covers: an invocation through a symlinked alias named `tv`, whose basename the
/// holder's recorded command line does not contain and whose relative target
/// carries `..`, waits on the live holder reached by its canonical path instead
/// of reclaiming it. The alias is still running after a fixed window and finishes
/// once the holder releases the lock.
#[cfg(unix)]
#[test]
fn a_symlinked_alias_waits_on_a_live_holder() {
    let port = free_loopback_port();
    let root = Root::new("launcher_alias_wait", port);
    let bin = root.dir.join("bin");
    fs::create_dir_all(&bin).expect("alias dir");
    let alias = bin.join("tv");
    symlink("../scripts/task-viewer.sh", &alias).expect("stage the alias");

    // A canonical-path invocation takes the lock and holds it for a fixed span.
    let ready = root.dir.join("holder.ready");
    let script = r#"
source "$1"
acquire_lock
printf 'ready\n' >"$2"
sleep 4
release_lock
"#;
    let holder = root
        .source_command(script)
        .arg(&ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the canonical holder");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready.exists(),
        "the holder must take the lock inside the window"
    );

    let mut waiter = root
        .staged_env("bash")
        .arg(&alias)
        .arg("status")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the alias");

    // The alias must wait on the live holder rather than reclaim its lock.
    let window = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < window {
        if let Some(status) = waiter.try_wait().expect("poll the alias") {
            let _ = holder.wait_with_output();
            panic!("the alias reclaimed a live holder's lock: {status:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let output = wait_output(waiter, SERVER_WINDOW);
    let holder_output = wait_output(holder, SERVER_WINDOW);
    assert!(
        holder_output.status.success(),
        "the holder failed: {}\n{}",
        stdout(&holder_output),
        stderr(&holder_output),
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "the alias must run status once the lock is released: {}\n{}",
        stdout(&output),
        stderr(&output),
    );
    assert!(
        stdout(&output).contains("task-viewer is not running"),
        "the alias must read the released state: {}",
        stdout(&output),
    );
}

/// Covers: a holder whose lock was renamed away before it entered the critical
/// section re-acquires it instead of running beside the new holder. The script
/// takes the lock, simulates a waiter renaming it aside and a fresh holder taking
/// the name, then requires the ownership re-check to make the lock this shell's
/// again; without that re-check the lock stays the other holder's and the script
/// exits non-zero. It also asserts the launcher's own dispatcher calls the
/// re-check after taking the lock, because a sourced copy skips the dispatcher:
/// without that half, deleting the dispatcher's call leaves this test green.
#[test]
fn a_lock_renamed_away_before_entry_is_reacquired() {
    let port = free_loopback_port();
    let root = Root::new("launcher_ownership", port);
    let stale_pid = dead_pid();

    // The structural half: read the dispatcher's code lines, so a comment or an
    // indentation change does not matter but a missing or reordered call does. A
    // trailing comment and a widened token gap are noise too, so each compared
    // line drops a trailing comment and collapses its runs of whitespace.
    fn strip_comment(line: &str) -> &str {
        let mut single = false;
        let mut double = false;
        let mut escaped = false;
        for (index, ch) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if double => escaped = true,
                '\'' if !double => single = !single,
                '"' if !single => double = !double,
                '#' if !single && !double => return &line[..index],
                _ => {}
            }
        }
        line
    }

    fn normalize_line(line: &str) -> String {
        strip_comment(line)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    let source = fs::read_to_string(repo_launcher()).expect("read the launcher");
    let dispatcher_code = |text: &str| -> Vec<String> {
        let guard = normalize_line(r#"if [ "${BASH_SOURCE[0]}" = "$0" ]; then"#);
        let mut inside = false;
        let mut nested = 0;
        let mut code = Vec::new();
        for line in text.lines() {
            let line = normalize_line(line);
            if !inside {
                inside = line == guard;
                continue;
            }
            if line == "fi" {
                if nested == 0 {
                    break;
                }
                nested -= 1;
                continue;
            }
            if line.starts_with("if ") && line.ends_with("then") {
                nested += 1;
            }
            if !line.is_empty() && !line.starts_with('#') {
                code.push(line);
            }
        }
        code
    };
    let dispatcher = dispatcher_code(&source);
    let acquire = dispatcher.iter().position(|line| line == "acquire_lock");
    let ownership = dispatcher
        .iter()
        .position(|line| line == "require_lock_ownership");
    assert!(
        matches!((acquire, ownership), (Some(acquired), Some(rechecked)) if acquired < rechecked),
        "the dispatcher must call acquire_lock and then require_lock_ownership, so a \
         command re-acquires a lock a waiter renamed away before entry; its code reads:\n{}",
        dispatcher.join("\n"),
    );

    let script = r#"
source "$1"
acquire_lock
# A waiter renamed this shell's lock aside and a fresh holder took the name.
mv "$LOCK_DIR" "$LOCK_DIR.displaced" || exit 3
mkdir "$LOCK_DIR" || exit 3
printf '%s\n' "$2" >"$LOCK_DIR/pid"
printf '%s\n' "sleep 1" >"$LOCK_DIR/cmd"
require_lock_ownership
if [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" != "$$" ]; then
    echo "the lock is not this shell's after the re-check" >&2
    exit 1
fi
printf 'owned %s\n' "$$"
release_lock
"#;

    let output = wait_output(
        root.source_command(script)
            .arg(stale_pid.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the sourced launcher"),
        RECLAIM_WINDOW,
    );

    assert!(
        output.status.success(),
        "the ownership re-check did not make the lock this shell's: {}\n{}",
        stdout(&output),
        stderr(&output),
    );
    assert!(
        stdout(&output).contains("owned"),
        "the re-check must leave this shell holding the lock: {}",
        stdout(&output),
    );
    assert!(
        !root.lock_dir().exists(),
        "the re-acquired lock must be released at the end of the script",
    );
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
}

/// Covers: a checkout whose path holds a space still lets the launcher recognize
/// the server it started, so `status` exits 0 while it serves and `stop` takes it
/// down and leaves no listener on the port.
#[test]
fn a_checkout_path_with_a_space_recognizes_its_server() {
    let port = free_loopback_port();
    let root = Root::new_under_a_space("launcher_space_path", port);

    let start = wait_output(root.spawn("start"), COMMAND_WINDOW);
    assert!(
        start.status.success(),
        "start failed: {}\n{}",
        stdout(&start),
        stderr(&start),
    );
    wait_for_server(port, SERVER_WINDOW);
    let pid = root.read_pid().expect("the started server's PID file");
    assert!(process_alive(pid), "the started server must be alive");

    let status = wait_output(root.spawn("status"), COMMAND_WINDOW);
    assert_eq!(
        status.status.code(),
        Some(0),
        "a running server under a space path must read as running: {}\n{}",
        stdout(&status),
        stderr(&status),
    );
    assert!(
        stdout(&status).contains("task-viewer is running"),
        "status must name the running server: {}",
        stdout(&status),
    );

    stop_and_assert(&root, pid);
}

/// Covers: a panicking assertion after a start unwinds through `Root`'s `Drop`,
/// which stops the detached server and removes the temp root, so the port stops
/// answering and no directory is left behind.
#[test]
fn a_panic_after_start_leaves_no_listener_and_no_temp_dir() {
    let port = free_loopback_port();
    let root_dir = unique_temp_dir("launcher_panic");
    let watched = root_dir.clone();
    let pid = Arc::new(AtomicU64::new(0));
    let recorded = Arc::clone(&pid);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let root = Root::from_dir(watched.clone(), watched, port);
        let start = wait_output(root.spawn("start"), COMMAND_WINDOW);
        assert!(
            start.status.success(),
            "start failed: {}\n{}",
            stdout(&start),
            stderr(&start),
        );
        wait_for_server(port, SERVER_WINDOW);
        let server = root.read_pid().expect("the started server's PID file");
        recorded.store(u64::from(server), Ordering::SeqCst);
        panic!("a deliberate panic must still clean up");
    }));

    assert!(outcome.is_err(), "the deliberate panic unwinds");
    assert!(
        !root_dir.exists(),
        "the temp root is removed when the panicking case unwinds",
    );
    let server = pid.load(Ordering::SeqCst);
    assert_ne!(server, 0, "the server started before the panic");
    assert!(
        !process_alive(u32::try_from(server).expect("the recorded PID fits u32")),
        "the started server is gone after the panic",
    );
    assert!(
        root_status(port).is_none(),
        "no listener answers on the copy's port after the panic",
    );
}
