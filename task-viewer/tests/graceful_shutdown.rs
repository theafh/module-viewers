//! Process-level shutdown test: the built binary stops accepting, drains, and
//! exits on its own when it receives SIGTERM or SIGINT, inside the launcher's
//! five-second stop window, and a store that is in flight when the signal
//! arrives either finishes or rolls back.
//!
//! `task-viewer` declares no library target, so this test reaches the binary
//! only through `env!("CARGO_BIN_EXE_task-viewer")` and carries its own
//! per-test temp-dir helper on the `task-viewer_<name>_<pid>_<nanos>_<n>`
//! pattern the inline suites use. It also carries its own `GRACE` mirror,
//! because `web::SHUTDOWN_GRACE` is not importable from here.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// The launcher's stop window: ten polls at half a second before `kill -9`.
const STOP_WINDOW: Duration = Duration::from_secs(5);
/// How long one server start may take before the test calls it hung.
const START_WINDOW: Duration = Duration::from_secs(20);

/// The drain a stopped server gets, mirroring `web::SHUTDOWN_GRACE` in
/// `task-viewer/src/web.rs`, the single source this file cannot import because
/// the binary declares no library target. The past-grace run proves the two
/// agree: it exits no earlier than this value, so lowering `SHUTDOWN_GRACE`
/// alone fails the run, and it still exits inside `STOP_WINDOW`, so raising it
/// past the launcher's window fails the run too.
const GRACE: Duration = Duration::from_secs(4);

static NEXT: AtomicU64 = AtomicU64::new(0);
static NEXT_PORT: AtomicU64 = AtomicU64::new(0);

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

/// A scratch directory removed on every path out of the test, a panic
/// included, so a failed run leaves nothing behind.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        Self::at(unique_temp_dir(name))
    }

    fn at(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One `GET /` carrying the loopback `Host` a same-machine browser sends. The
/// bare name is accepted with or without the documented port, so an isolated
/// server on a candidate port answers it.
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

/// Candidate listen ports for one server. The test never binds a listener to
/// reserve one: it hands the child a candidate and takes the port the child
/// actually bound from its post-bind listening line, so two runs started
/// together cannot take each other's port. The candidates start away from the
/// documented 7880 and step past each other per server start.
fn candidate_ports() -> impl Iterator<Item = u16> {
    let slot = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    let base = 20_000 + (u64::from(std::process::id()) % 16_000) + 16 * (slot % 1_000);
    (0..16).map(move |step| ((base + step) % 16_000 + 20_000) as u16)
}

/// What one attempt to start the child produced.
enum Startup {
    Serving,
    AddressInUse,
}

/// A spawned server the test owns. Dropping it kills and waits for the child,
/// so a panic anywhere after the spawn leaves no `task-viewer` process behind.
struct Server {
    child: Child,
    port: u16,
    log: PathBuf,
    marker: Option<PathBuf>,
}

impl Server {
    /// Start the child on the first candidate port it can bind, retrying the
    /// next candidate when it reports that one is in use.
    fn start(scratch: &Scratch, name: &str, db: &Path, pause_ms: Option<u64>) -> Self {
        Self::start_on(scratch, name, db, pause_ms, candidate_ports())
    }

    /// Start the child on the first of `candidates` it can bind, so a test can
    /// put a listener on the first candidate and observe the retry.
    fn start_on(
        scratch: &Scratch,
        name: &str,
        db: &Path,
        pause_ms: Option<u64>,
        candidates: impl Iterator<Item = u16>,
    ) -> Self {
        let log = scratch.path().join(format!("{name}.log"));
        let marker = pause_ms.map(|_| scratch.path().join(format!("{name}.store-open")));
        if let Some(marker) = &marker {
            let _ = std::fs::remove_file(marker);
        }
        for port in candidates {
            let file = std::fs::File::create(&log).expect("server log");
            let mut command = Command::new(env!("CARGO_BIN_EXE_task-viewer"));
            command
                .env("TASK_VIEWER_DB", db)
                .env("TASK_VIEWER_PORT", port.to_string())
                .env("RUST_LOG", "task_viewer=info")
                .stdout(Stdio::from(file.try_clone().expect("log clone")))
                .stderr(Stdio::from(file));
            if let (Some(pause_ms), Some(marker)) = (pause_ms, &marker) {
                command
                    .env("TASK_VIEWER_TEST_STORE_PAUSE_MS", pause_ms.to_string())
                    .env("TASK_VIEWER_TEST_STORE_MARKER", marker);
            }
            let child = command.spawn().expect("spawn the built binary");
            let mut server = Self {
                child,
                port,
                log: log.clone(),
                marker: marker.clone(),
            };
            match server.wait_until_serving() {
                Startup::Serving => return server,
                Startup::AddressInUse => continue,
            }
        }
        panic!("the built binary could not bind any candidate port");
    }

    fn marker(&self) -> Option<&Path> {
        self.marker.as_deref()
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The port named by the child's post-bind listening line, which is the
    /// only place the test learns what the child actually bound.
    fn listening_port(&self) -> Option<u16> {
        let text = self.log_text();
        let line = text
            .lines()
            .find(|line| line.contains("task-viewer listening on http://"))?;
        let address = line
            .split("task-viewer listening on http://")
            .nth(1)?
            .trim();
        address
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|address| address.port())
    }

    fn wait_until_serving(&mut self) -> Startup {
        let deadline = Instant::now() + START_WINDOW;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll the server") {
                let text = self.log_text();
                if text.contains("could not listen on") && text.contains("in use") {
                    return Startup::AddressInUse;
                }
                panic!("the server exited before serving with {status:?}\n{text}");
            }
            if let Some(port) = self.listening_port() {
                self.port = port;
                if root_status(port) == Some(200) {
                    return Startup::Serving;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "the server never answered GET / with 200\n{}",
            self.log_text()
        );
    }

    /// Wait for the server to exit by itself, failing the test if it needs the
    /// forced kill the launcher would send.
    fn wait_for_exit(&mut self, window: Duration) -> ExitStatus {
        let deadline = Instant::now() + window;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll the server") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("the server did not exit inside the launcher's five-second window");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(
        status.success(),
        "kill -{signal} {pid} failed with {status:?}"
    );
}

/// Wait for the store hook's marker, so the signal provably arrives while a
/// store is open rather than after a guessed delay.
fn wait_for_marker(marker: &Path, deadline: Instant) {
    while Instant::now() < deadline {
        if marker.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "the store hook never wrote its marker at {}",
        marker.display()
    );
}

/// Ask the running child to run an Update. The request carries the loopback
/// `Host` and the documented loopback `Origin` the request guard requires; the
/// response is irrelevant, so the request runs on its own thread and the test
/// proceeds as soon as the store reports itself open.
fn trigger_update(port: u16) {
    std::thread::spawn(move || {
        let body = "project=all";
        let request = format!(
            "POST /scan HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1:7880\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.write_all(request.as_bytes());
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
    });
}

async fn open_database(path: &Path) -> sqlx::SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .expect("open the child's database")
}

/// One stored task row, as the charts would read it back.
type StoredTask = (String, String, String, Option<String>);

fn read_tasks(path: &Path) -> Vec<StoredTask> {
    let read = async {
        let pool = open_database(path).await;
        let rows = sqlx::query("SELECT path, kind, created, ship_date FROM tasks ORDER BY path")
            .fetch_all(&pool)
            .await
            .expect("read the stored rows");
        pool.close().await;
        rows.iter()
            .map(|row| {
                (
                    row.get::<String, _>("path"),
                    row.get::<String, _>("kind"),
                    row.get::<String, _>("created"),
                    row.get::<Option<String>, _>("ship_date"),
                )
            })
            .collect()
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(read)
}

/// Seed one repository once the child has created the schema, so the Update
/// the test triggers has something to harvest and store.
fn seed_repository(path: &Path, name: &str, folder: &Path) {
    let seed = async {
        let pool = open_database(path).await;
        sqlx::query("INSERT INTO repositories (name, path, created_at) VALUES (?, ?, ?)")
            .bind(name)
            .bind(folder.to_string_lossy().to_string())
            .bind("2026-11-19T00:00:00Z")
            .execute(&pool)
            .await
            .expect("seed the repository");
        pool.close().await;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(seed)
}

/// Covers: SIGTERM and SIGINT each let the server exit on its own with a
/// success status inside the launcher's five-second window.
#[test]
fn task_viewer_exits_on_sigterm_and_sigint() {
    for signal in ["TERM", "INT"] {
        let scratch = Scratch::new(&format!("shutdown_{}", signal.to_lowercase()));
        let db = scratch.path().join("test.db");
        let mut server = Server::start(&scratch, "server", &db, None);
        let started = Instant::now();
        send_signal(server.child.id(), signal);
        let status = server.wait_for_exit(STOP_WINDOW);
        let elapsed = started.elapsed();
        assert!(
            status.success(),
            "the server exited with {status:?} on SIG{signal}\n{}",
            server.log_text()
        );
        assert!(
            elapsed < STOP_WINDOW,
            "the server exited {elapsed:?} after SIG{signal}, past the launcher's {STOP_WINDOW:?} window"
        );
    }
}

/// Covers: a store that is in flight when the signal arrives finishes inside
/// the grace and commits its rows, while a store held past the grace rolls
/// back, leaving the repository's rows exactly as they stood before the run.
#[test]
fn task_viewer_finishes_an_in_flight_store_and_rolls_back_one_held_past_the_grace() {
    let scratch = Scratch::new("shutdown_in_flight");
    let db = scratch.path().join("test.db");
    let folder = scratch.path().join("repo");
    std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
    std::fs::write(
        folder.join("tasks/scope_live.md"),
        "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Live\n",
    )
    .expect("task file");

    // Run one: the store pause sits inside the grace, so the drain waits for
    // the store and its rows commit before the server exits.
    let mut inside = Server::start(&scratch, "inside", &db, Some(2_500));
    seed_repository(&db, "repo", &folder);
    trigger_update(inside.port);
    let marker = inside.marker().expect("the in-flight run pauses its store");
    wait_for_marker(marker, Instant::now() + START_WINDOW);
    let started = Instant::now();
    send_signal(inside.child.id(), "TERM");
    let status = inside.wait_for_exit(STOP_WINDOW);
    let elapsed = started.elapsed();
    assert!(
        status.success(),
        "the server exited with {status:?} on SIGTERM\n{}",
        inside.log_text()
    );
    assert!(
        elapsed < GRACE,
        "the in-flight store outlived the {GRACE:?} grace: {elapsed:?}"
    );
    // The store was still open when the signal landed, so the drain had to wait
    // for it. A run that exited before the pause could elapse would leave the
    // same committed rows yet prove nothing about the drain.
    assert!(
        elapsed >= Duration::from_millis(1_000),
        "the drain did not wait for the in-flight store: exited after {elapsed:?}"
    );
    drop(inside);

    let committed = read_tasks(&db);
    assert_eq!(
        committed,
        vec![(
            "tasks/scope_live.md".to_string(),
            "live".to_string(),
            "2026-05-01".to_string(),
            None,
        )],
        "the in-flight store's rows are in the child's database after it exits"
    );

    // Run two: the store pause runs past the grace, so the process exits with
    // its transaction open and the delete inside it rolls back.
    let mut past = Server::start(&scratch, "past", &db, Some(6_000));
    trigger_update(past.port);
    let marker = past.marker().expect("the past-grace run pauses its store");
    wait_for_marker(marker, Instant::now() + START_WINDOW);
    let started = Instant::now();
    send_signal(past.child.id(), "TERM");
    let status = past.wait_for_exit(STOP_WINDOW);
    let elapsed = started.elapsed();
    assert!(
        status.success(),
        "the server exited with {status:?} on SIGTERM\n{}",
        past.log_text()
    );
    assert!(
        elapsed >= GRACE,
        "the server exited {elapsed:?} before the mirrored {GRACE:?} grace"
    );
    assert!(
        elapsed < STOP_WINDOW,
        "the server exited {elapsed:?} past the launcher's {STOP_WINDOW:?} window"
    );
    drop(past);

    assert_eq!(
        read_tasks(&db),
        committed,
        "the store held past the grace rolled back, leaving the repository's rows exactly as they stood"
    );
}

/// Covers: the test binds no listener of its own; when the first candidate
/// port is already taken, the child reports it and the test starts the next
/// candidate, then serves on the port the child's own listening line names.
#[test]
fn the_server_retries_the_next_candidate_when_a_port_is_taken() {
    let scratch = Scratch::new("shutdown_port_retry");
    let db = scratch.path().join("test.db");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let taken_port = taken.local_addr().expect("occupied address").port();
    let candidates =
        std::iter::once(taken_port).chain(candidate_ports().filter(|port| *port != taken_port));
    let server = Server::start_on(&scratch, "server", &db, None, candidates);
    assert_ne!(
        server.port, taken_port,
        "the server bound a candidate other than the taken port"
    );
    assert_eq!(
        root_status(server.port),
        Some(200),
        "the retried server serves on the port its listening line named"
    );
}

/// Covers: a panic after the server starts leaves no `task-viewer` process
/// running and no scratch directory behind.
#[test]
fn a_panic_leaves_no_server_and_no_scratch_dir() {
    let dir = unique_temp_dir("shutdown_panic");
    let watched = dir.clone();
    let pid = std::sync::Arc::new(AtomicU64::new(0));
    let recorded = std::sync::Arc::clone(&pid);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let scratch = Scratch::at(watched);
        let db = scratch.path().join("test.db");
        let server = Server::start(&scratch, "server", &db, None);
        recorded.store(u64::from(server.child.id()), Ordering::SeqCst);
        panic!("a deliberate panic must still clean up");
    }));
    assert!(outcome.is_err(), "the deliberate panic unwinds");
    assert!(!dir.exists(), "the scratch directory is removed on a panic");
    let pid = pid.load(Ordering::SeqCst);
    assert_ne!(pid, 0, "the server started before the panic");
    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .expect("probe the process");
    assert!(
        !alive.status.success(),
        "the server process is gone after the panic"
    );
}
