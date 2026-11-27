//! Launcher tests: drive `scripts/task-viewer.sh` through a bash subprocess.
#![cfg(test)]

use crate::test_support::unique_temp_dir;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The git variables a test process may inherit; the launcher probes must not
/// see them, so every run clears them explicitly.
const GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_INDEX_FILE",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// The launcher lives beside the crate, at the workspace root.
fn repo_launcher() -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("task-viewer sits inside the workspace root");
    repo.join("scripts/task-viewer.sh")
}

/// Copy the launcher under a test root, so `PROJECT_DIR` is that root and every
/// state, log, PID, stamp, and lock file lands under the test's own home.
fn stage_launcher(dir: &Path) -> PathBuf {
    let launcher = dir.join("scripts").join("task-viewer.sh");
    fs::create_dir_all(launcher.parent().expect("launcher parent")).expect("launcher scripts dir");
    fs::copy(repo_launcher(), &launcher).expect("copy launcher into temp dir");
    launcher
}

/// The launcher's state directory for a given home, mirroring its own rule.
fn state_dir(home: &Path, xdg: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/task-viewer")
    } else {
        xdg.join("task-viewer")
    }
}

/// Run a bash driver that sources the staged launcher.
fn run_driver(dir: &Path, driver: &str, envs: &[(&str, &str)]) -> std::process::Output {
    let path = dir.join("driver.sh");
    write_executable(&path, driver);
    let mut command = Command::new("bash");
    command
        .arg("--noprofile")
        .arg("--norc")
        .arg(&path)
        // Keep the launcher's state and log files out of the live directories.
        .env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state"));
    for (name, value) in envs {
        command.env(name, value);
    }
    for name in GIT_ENV {
        command.env_remove(name);
    }
    command.output().expect("run bash")
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod script");
}

/// Source the launcher without dispatching, then exercise `is_ours` against
/// the fake `ps`. The positional probe matches no dispatcher arm, so a missing
/// main guard would print usage and exit before the assertions run.
const DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# A function beats every PATH entry the launcher prepends, so a real procps in
# /usr/local/bin or /opt/homebrew/bin cannot answer in place of the fake.
ps() {
    case " $* " in
        *" -ww "*) printf '%s\n' "$FAKE_PS_ARGS" ;;
    esac
}
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
BINARY="$LONG_BINARY"
if ! is_ours 4242; then
    echo "is_ours rejected the full long path" >&2
    exit 10
fi
BINARY="$OTHER_BINARY"
if is_ours 4242; then
    echo "is_ours accepted a path ps never printed" >&2
    exit 11
fi
printf 'long-ok\nforeign-rejected\n'
"#;

#[test]
fn launcher_is_ours_reads_the_full_command_line() {
    let dir = unique_temp_dir("launcher_ps");
    fs::create_dir_all(&dir).expect("temp dir");
    let driver = dir.join("driver.sh");
    write_executable(&driver, DRIVER);

    let long_binary = dir
        .join("long")
        .join("x".repeat(220))
        .join("task-viewer")
        .to_string_lossy()
        .into_owned();
    assert!(
        long_binary.len() > 200,
        "the fixture path must exceed 200 bytes: {long_binary}"
    );
    let other_binary = dir
        .join("other")
        .join("task-viewer")
        .to_string_lossy()
        .into_owned();

    let launcher = dir.join("scripts").join("task-viewer.sh");
    fs::create_dir_all(launcher.parent().expect("launcher parent")).expect("launcher scripts dir");
    // Source a copy under the temp dir: the launcher derives PROJECT_DIR from
    // its own path, so its legacy cleanup stays inside the test's root instead
    // of deleting and moving files at the checkout root.
    fs::copy(repo_launcher(), &launcher).expect("copy launcher into temp dir");
    assert!(
        launcher.is_file(),
        "launcher copy missing at {}",
        launcher.display()
    );

    let mut command = Command::new("bash");
    command
        .arg("--noprofile")
        .arg("--norc")
        .arg(&driver)
        // Keep the launcher's state and log files out of the live directories.
        .env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("SCRIPT_PATH", &launcher)
        .env("LONG_BINARY", &long_binary)
        .env("OTHER_BINARY", &other_binary)
        .env("FAKE_PS_ARGS", &long_binary);
    for name in GIT_ENV {
        command.env_remove(name);
    }

    let output = command.output().expect("run bash");
    assert!(
        output.status.success(),
        "launcher probe failed ({:?})\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("long-ok"),
        "the long path must be recognized: {stdout}"
    );
    assert!(
        stdout.contains("foreign-rejected"),
        "a path ps never printed must be rejected: {stdout}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Stage a checkout-shaped root with an executable `target/release/task-viewer`
/// and return its launcher copy.
fn stage_root(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("target/release")).expect("release dir");
    write_executable(
        &dir.join("target/release/task-viewer"),
        "#!/bin/bash\nexit 0\n",
    );
    stage_launcher(dir)
}

/// Drive `is_ours` with a staged `ps` line for every printed form.
const IS_OURS_DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
ps() {
    case " $* " in
        *" -ww "*) printf '%s\n' "$FAKE_PS_ARGS" ;;
    esac
}
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
notify() { :; }
PATH="$PROJECT_DIR/target/release:$PATH"
check() {
    local name="$1" expected="$2"
    FAKE_PS_ARGS="$3"
    if is_ours 4242; then
        got=yes
    else
        got=no
    fi
    if [ "$got" = "$expected" ]; then
        printf '%s-ok\n' "$name"
    else
        printf '%s-FAIL (got %s)\n' "$name" "$got"
    fi
}
check absolute yes "$BINARY"
check absolute-arg yes "$BINARY --serve"
check relative yes "./target/release/task-viewer"
check relative-plain yes "target/release/task-viewer"
check bare yes "task-viewer"
check foreign-absolute no "/somewhere/else/task-viewer"
check foreign-relative no "../elsewhere/task-viewer"
check empty no ""
"#;

/// Covers: `is_ours` accepts a staged `ps` line printing the binary as
/// `./target/release/task-viewer`, one printing the absolute binary followed by
/// an argument, and one printing a bare `task-viewer` name resolved on `PATH`,
/// and rejects a path outside the checkout.
#[test]
fn launcher_is_ours_resolves_every_printed_form() {
    let dir = unique_temp_dir("launcher_is_ours");
    let launcher = stage_root(&dir);

    let output = run_driver(
        &dir,
        IS_OURS_DRIVER,
        &[(
            "SCRIPT_PATH",
            launcher.to_str().expect("the temp path is valid UTF-8"),
        )],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    for marker in [
        "absolute-ok",
        "absolute-arg-ok",
        "relative-ok",
        "relative-plain-ok",
        "bare-ok",
        "foreign-absolute-ok",
        "foreign-relative-ok",
        "empty-ok",
    ] {
        assert!(
            stdout.lines().any(|line| line == marker),
            "missing {marker} in: {stdout}",
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Covers: every printed form `is_ours` accepts still resolves when the checkout
/// path holds a space, so the whole-line comparison carries the absolute forms
/// and the token walk keeps the relative and bare forms.
#[test]
fn launcher_is_ours_accepts_a_checkout_path_with_a_space() {
    let dir = unique_temp_dir("launcher_is_ours_space").join("My Projects");
    let launcher = stage_root(&dir);

    let output = run_driver(
        &dir,
        IS_OURS_DRIVER,
        &[(
            "SCRIPT_PATH",
            launcher.to_str().expect("the temp path is valid UTF-8"),
        )],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    for marker in [
        "absolute-ok",
        "absolute-arg-ok",
        "relative-ok",
        "relative-plain-ok",
        "bare-ok",
        "foreign-absolute-ok",
        "foreign-relative-ok",
        "empty-ok",
    ] {
        assert!(
            stdout.lines().any(|line| line == marker),
            "missing {marker} under a space path in: {stdout}",
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Aim a stop at a live unrelated process and record every signal the launcher
/// sends. The real `kill` stays available for the liveness check.
const FOREIGN_STOP_DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
notify() { :; }
port_pid() { :; }
ps() {
    case " $* " in
        *" -ww "*) printf '%s\n' "/bin/sleep 60" ;;
    esac
}
sleep 60 &
foreign=$!
echo "$foreign" >"$PID_FILE"
kill() {
    printf '%s\n' "$*" >>"$KILL_LOG"
    return 0
}
do_stop
echo "status=$?"
unset -f kill
if kill -0 "$foreign" 2>/dev/null; then
    echo "foreign-alive"
else
    echo "foreign-dead"
fi
kill -9 "$foreign" 2>/dev/null || true
wait "$foreign" 2>/dev/null || true
"#;

/// Covers: a stop aimed at a PID belonging to an unrelated program sends it no
/// signal, so neither the graceful `kill` nor the `kill -9` fires.
#[test]
fn launcher_stop_leaves_a_foreign_process_alone() {
    let dir = unique_temp_dir("launcher_foreign_stop");
    let launcher = stage_root(&dir);
    let kill_log = dir.join("kills.log");

    let output = run_driver(
        &dir,
        FOREIGN_STOP_DRIVER,
        &[
            (
                "SCRIPT_PATH",
                launcher.to_str().expect("the temp path is valid UTF-8"),
            ),
            (
                "KILL_LOG",
                kill_log.to_str().expect("the temp path is valid UTF-8"),
            ),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("task-viewer is not running"),
        "a foreign PID must read as not running: {stdout}",
    );
    assert!(
        stdout.lines().any(|line| line == "status=0"),
        "the stop must report success: {stdout}",
    );
    assert!(
        stdout.lines().any(|line| line == "foreign-alive"),
        "the unrelated process must survive: {stdout}",
    );
    let signals = fs::read_to_string(&kill_log).unwrap_or_default();
    assert!(
        signals.is_empty(),
        "no signal may reach the unrelated process, logged: {signals}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Hold a PID the launcher must read as its own server while its kill does
/// nothing, so the stop has to report a survivor.
const SURVIVOR_DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
notify() { :; }
sleep() { :; }
ps() {
    case " $* " in
        *" -ww "*) printf '%s\n' "$BINARY" ;;
    esac
}
kill() { return 0; }
echo 4242 >"$PID_FILE"
do_stop
echo "status=$?"
"#;

/// Covers: a stop whose process survives `kill -9` exits non-zero and does not
/// print "stopped".
#[test]
fn launcher_stop_reports_a_survivor() {
    let dir = unique_temp_dir("launcher_survivor");
    let launcher = stage_root(&dir);

    let output = run_driver(
        &dir,
        SURVIVOR_DRIVER,
        &[(
            "SCRIPT_PATH",
            launcher.to_str().expect("the temp path is valid UTF-8"),
        )],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.lines().any(|line| line == "status=1"),
        "a survivor must exit non-zero: {stdout}",
    );
    assert!(
        stdout.contains("did not stop"),
        "the survivor must be reported: {stdout}",
    );
    assert!(
        !stdout.contains("task-viewer stopped"),
        "a survivor must not read as stopped: {stdout}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Launch a real process the launcher reads as its own server, then force the
/// start's listen probe to fail so the timeout cleanup runs.
const START_TIMEOUT_DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
notify() { :; }
sleep() { :; }
port_pid() { :; }
port_answers() { return 1; }
is_listening() { return 1; }
start_detached() {
    "$BINARY" 300 >/dev/null 2>&1 &
    echo $! >"$PID_FILE"
    echo $! >"$PID_LOG"
    return 0
}
start_binary
echo "status=$?"
# The stubbed sleep made the probe loops instant; restore the real one so the
# reap can be observed.
unset -f sleep
server=$(cat "$PID_LOG" 2>/dev/null)
for _ in {1..40}; do
    if ! kill -0 "$server" 2>/dev/null; then
        echo "server-dead"
        exit 0
    fi
    sleep 0.05
done
echo "server-alive"
"#;

/// Covers: a forced start timeout leaves no live server process.
#[cfg(unix)]
#[test]
fn launcher_start_timeout_terminates_the_server() {
    let dir = unique_temp_dir("launcher_start_timeout");
    let launcher = stage_root(&dir);
    // A binary that blocks without arguments, so the timeout has a live process
    // to clean up. `ps` prints the argv path, which is what `is_ours` reads.
    let sleep_binary = ["/bin/sleep", "/usr/bin/sleep"]
        .iter()
        .find(|path| Path::new(path).exists())
        .expect("a system sleep binary");
    fs::remove_file(dir.join("target/release/task-viewer")).expect("replace the staged binary");
    std::os::unix::fs::symlink(sleep_binary, dir.join("target/release/task-viewer"))
        .expect("stage a blocking binary");

    let pid_log = dir.join("pid.log");
    let output = run_driver(
        &dir,
        START_TIMEOUT_DRIVER,
        &[
            (
                "SCRIPT_PATH",
                launcher.to_str().expect("the temp path is valid UTF-8"),
            ),
            (
                "PID_LOG",
                pid_log.to_str().expect("the temp path is valid UTF-8"),
            ),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.lines().any(|line| line == "status=1"),
        "a timed-out start must fail: {stdout}",
    );
    assert!(
        stdout.contains("Failed to start"),
        "the timeout must be reported: {stdout}",
    );
    assert!(
        stdout.lines().any(|line| line == "server-dead"),
        "the timed-out server must be gone: {stdout}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Covers: a failed start under a checkout path holding a space signals the
/// process it launched. The whole-line comparison is what lets the cleanup
/// recognize that process; a token split at the path's space never matches, so
/// the server would survive.
#[cfg(unix)]
#[test]
fn launcher_start_timeout_under_a_space_path_terminates_the_server() {
    let dir = unique_temp_dir("launcher_space_start_timeout").join("My Projects");
    let launcher = stage_root(&dir);
    // A binary that blocks without arguments, so the timeout has a live process
    // to clean up. `ps` prints the argv path, which is what `is_ours` reads.
    let sleep_binary = ["/bin/sleep", "/usr/bin/sleep"]
        .iter()
        .find(|path| Path::new(path).exists())
        .expect("a system sleep binary");
    fs::remove_file(dir.join("target/release/task-viewer")).expect("replace the staged binary");
    std::os::unix::fs::symlink(sleep_binary, dir.join("target/release/task-viewer"))
        .expect("stage a blocking binary");

    let pid_log = dir.join("pid.log");
    let output = run_driver(
        &dir,
        START_TIMEOUT_DRIVER,
        &[
            (
                "SCRIPT_PATH",
                launcher.to_str().expect("the temp path is valid UTF-8"),
            ),
            (
                "PID_LOG",
                pid_log.to_str().expect("the temp path is valid UTF-8"),
            ),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.lines().any(|line| line == "status=1"),
        "a timed-out start must fail: {stdout}",
    );
    assert!(
        stdout.contains("Failed to start"),
        "the timeout must be reported: {stdout}",
    );
    assert!(
        stdout.lines().any(|line| line == "server-dead"),
        "the timed-out server under a space path must be gone: {stdout}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Covers: a `status` invocation run while another invocation holds the lock
/// waits for the holder to release it before reading the PID file, so it never
/// reads a concurrent start's half-written file.
#[test]
fn launcher_status_waits_for_the_lock_holder() {
    let dir = unique_temp_dir("launcher_lock_wait");
    let launcher = stage_root(&dir);
    let home = dir.join("home");
    let xdg = dir.join("state");
    let lock = state_dir(&home, &xdg).join("task-viewer.lock");
    fs::create_dir_all(&lock).expect("hold the lock");
    fs::write(lock.join("pid"), format!("{}\n", std::process::id())).expect("holder pid");

    let mut child = Command::new("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg(&launcher)
        .arg("status")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &xdg)
        .env_remove("TASK_VIEWER_PORT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn status");

    // A status that ignored the lock would finish in well under this window.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll status") {
            panic!("status finished while another invocation held the lock: {status:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    fs::remove_dir_all(&lock).expect("release the lock");
    let output = wait_for_output(child, Duration::from_secs(10));
    let stdout = String::from_utf8_lossy(&output.stdout);
    // A stopped server is a non-zero status, so a script can branch on it.
    assert_eq!(
        output.status.code(),
        Some(1),
        "a stopped status must exit 1 ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("task-viewer is not running"),
        "status must read the released PID file: {stdout}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Covers: `status` exits 0 while a server is running and 1 while it is not,
/// so a script can branch on the command instead of parsing its text, and each
/// branch prints its own lines: the running branch the PID and the URL, the
/// stopped branch one line.
#[test]
fn launcher_status_exit_code_reports_whether_the_server_runs() {
    let dir = unique_temp_dir("launcher_status_exit");
    let launcher = stage_launcher(&dir);
    // `is_running` is overridden rather than faked through `ps`, so the test
    // measures `do_status`'s own contract and nothing about process lookup.
    let driver = format!(
        r#"#!/bin/bash
set -u
set -- source-guard-probe
. "{launcher}"
get_pid() {{ echo 4242; }}
is_running() {{ return 0; }}
do_status || exit 10
echo "---stopped---"
is_running() {{ return 1; }}
if do_status; then exit 11; fi
exit 0
"#,
        launcher = launcher.display(),
    );
    let output = run_driver(&dir, &driver, &[]);
    assert!(
        output.status.success(),
        "status exit codes are wrong ({:?})\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        "task-viewer is running (PID: 4242)\n\
         Access at: http://127.0.0.1:7880\n\
         ---stopped---\n\
         task-viewer is not running\n",
        "each status branch must print its own lines",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Force the outdated-binary restart branch while the stop helper reports that
/// it could not take the old server down.
const RESTART_STOP_FAILS_DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
notify() { :; }
is_running() { return 0; }
is_outdated() { return 0; }
do_stop() { return 1; }
start_binary
echo "status=$?"
"#;

/// Covers: a restart whose stop fails exits non-zero and prints no "already
/// running" line, instead of reporting the surviving old server as a success.
#[test]
fn launcher_restart_reports_a_failed_stop() {
    let dir = unique_temp_dir("launcher_restart_stop_fail");
    let launcher = stage_root(&dir);

    let output = run_driver(
        &dir,
        RESTART_STOP_FAILS_DRIVER,
        &[(
            "SCRIPT_PATH",
            launcher.to_str().expect("the temp path is valid UTF-8"),
        )],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.lines().any(|line| line == "status=1"),
        "a failed restart must exit non-zero: {stdout}",
    );
    assert!(
        stdout.contains("restarting on it"),
        "the restart branch must run: {stdout}",
    );
    assert!(
        !stdout.contains("already running"),
        "a failed stop must not read as an already-running success: {stdout}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Hold a lock whose live `ps` read comes back empty, as a restricted system
/// would answer, and run `acquire_lock` in the background.
const EMPTY_LIVE_PS_DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
notify() { :; }
# A live read that fails prints nothing, which must not read as "not a holder".
ps() { return 0; }
sleep 60 &
holder=$!
mkdir -p "$LOCK_DIR"
printf '%s\n' "$holder" >"$LOCK_DIR/pid"
printf '%s\n' "bash $SCRIPT_PATH status" >"$LOCK_DIR/cmd"
(
    acquire_lock
    printf 'acquired\n' >"$ACQUIRED_LOG"
    release_lock
) &
waiter=$!
# Fixed window: the waiter must stay blocked on the live holder.
for _ in {1..40}; do
    [ -e "$ACQUIRED_LOG" ] && break
    sleep 0.05
done
if [ -e "$ACQUIRED_LOG" ]; then
    echo "reclaimed-empty-read"
elif [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" != "$holder" ]; then
    echo "lock-changed"
else
    echo "waited"
fi
# Release the lock; the waiter must now acquire it and proceed.
rm -rf "$LOCK_DIR"
for _ in {1..200}; do
    [ -e "$ACQUIRED_LOG" ] && break
    sleep 0.05
done
if [ -e "$ACQUIRED_LOG" ]; then
    echo "acquired-after-release"
else
    echo "never-acquired"
fi
kill -9 "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
wait "$waiter" 2>/dev/null || true
"#;

/// Covers: a lock whose recorded PID is alive and whose recorded line names the
/// launcher is still a holder when the live `ps` read comes back empty, so
/// `acquire_lock` keeps waiting and only proceeds once the lock is released.
#[test]
fn launcher_lock_waits_when_the_live_read_is_empty() {
    let dir = unique_temp_dir("launcher_empty_live_ps");
    let launcher = stage_root(&dir);
    let acquired = dir.join("acquired.log");

    let output = run_driver(
        &dir,
        EMPTY_LIVE_PS_DRIVER,
        &[
            (
                "SCRIPT_PATH",
                launcher.to_str().expect("the temp path is valid UTF-8"),
            ),
            (
                "ACQUIRED_LOG",
                acquired.to_str().expect("the temp path is valid UTF-8"),
            ),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver failed ({:?})\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.lines().any(|line| line == "waited"),
        "an empty live read must keep the waiter waiting: {stdout}",
    );
    assert!(
        stdout.lines().any(|line| line == "acquired-after-release"),
        "the waiter must acquire once the lock is released: {stdout}",
    );
    assert!(
        !stdout.contains("reclaimed-empty-read") && !stdout.contains("never-acquired"),
        "the waiter must neither reclaim early nor fail to acquire: {stdout}",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Wait for a child to finish, then collect its output, failing the test if it
/// overruns the window instead of hanging the suite.
fn wait_for_output(mut child: Child, window: Duration) -> std::process::Output {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll the child").is_some() {
            return child.wait_with_output().expect("collect the output");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the child did not finish inside {window:?}");
}
