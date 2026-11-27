//! Process-level warning test: the built binary carries the production
//! process-wide warning gate, so eight `/charts.json` renders held in flight
//! together log one row's after-today warning exactly once.
//!
//! `task-viewer` declares no library target, so this test reaches the binary
//! only through `env!("CARGO_BIN_EXE_task-viewer")` and carries its own
//! per-test temp-dir helper on the `task-viewer_<name>_<pid>_<nanos>_<n>`
//! pattern the inline suites use.
//!
//! The child starts with `TASK_VIEWER_TEST_CHART_BARRIER_DIR` and
//! `TASK_VIEWER_TEST_CHART_BARRIER_COUNT` armed, so each `/charts.json`
//! blocking closure writes a marker named by its thread id and waits until all
//! eight markers exist. Only that closure is armed: the child-start readiness
//! `GET /` render returns at once, and it runs against an empty database, so it
//! cannot supply the counted line.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// How many chart exports the test holds in flight together.
const RENDERS: usize = 8;
/// How long one server start may take before the test calls it hung.
const START_WINDOW: Duration = Duration::from_secs(20);
/// How long the barrier's deadline plus a read margin may take.
const RESPONSE_WINDOW: Duration = Duration::from_secs(30);

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
        let dir = unique_temp_dir(name);
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
/// together cannot take each other's port.
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
}

impl Server {
    fn start(db: &Path, barrier_dir: &Path, log: &Path) -> Self {
        for port in candidate_ports() {
            let file = std::fs::File::create(log).expect("server log");
            let child = Command::new(env!("CARGO_BIN_EXE_task-viewer"))
                .env("TASK_VIEWER_DB", db)
                .env("TASK_VIEWER_PORT", port.to_string())
                .env("TASK_VIEWER_TEST_CHART_BARRIER_DIR", barrier_dir)
                .env("TASK_VIEWER_TEST_CHART_BARRIER_COUNT", RENDERS.to_string())
                .env("RUST_LOG", "task_viewer=info")
                .stdout(Stdio::from(file.try_clone().expect("log clone")))
                .stderr(Stdio::from(file))
                .spawn()
                .expect("spawn the built binary");
            let mut server = Self {
                child,
                port,
                log: log.to_path_buf(),
            };
            match server.wait_until_serving() {
                Startup::Serving => return server,
                Startup::AddressInUse => continue,
            }
        }
        panic!("the built binary could not bind any candidate port");
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
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn open_database(path: &Path) -> sqlx::SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .expect("open the child's database")
}

/// Seed one repository and one live row created after today, once the child has
/// created the schema. The row's created day is two days ahead so a run that
/// crosses midnight still charts it as after-today.
fn seed_future_row(path: &Path) {
    let created = (chrono::Utc::now().date_naive() + chrono::Days::new(2))
        .format("%Y-%m-%d")
        .to_string();
    let seed = async move {
        let pool = open_database(path).await;
        let id = sqlx::query("INSERT INTO repositories (name, path, created_at) VALUES (?, ?, ?)")
            .bind("warn-once")
            .bind("/tmp/warn-once")
            .bind("2026-01-01T00:00:00Z")
            .execute(&pool)
            .await
            .expect("seed the repository")
            .last_insert_rowid();
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/warn-once_future.md")
        .bind("warn-once")
        .bind("live")
        .bind(created)
        .bind("2026-01-01")
        .execute(&pool)
        .await
        .expect("seed the future row");
        pool.close().await;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(seed);
}

/// Send the eight `/charts.json` requests on eight connections before reading
/// any response, so all eight renders are in flight together, then read each
/// response's status.
fn eight_chart_requests(port: u16) -> Vec<u16> {
    let request =
        "GET /charts.json?project=all HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let mut streams = Vec::with_capacity(RENDERS);
    for _ in 0..RENDERS {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect the server");
        stream
            .set_read_timeout(Some(RESPONSE_WINDOW))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(RESPONSE_WINDOW))
            .expect("write timeout");
        stream
            .write_all(request.as_bytes())
            .expect("send the chart request");
        streams.push(stream);
    }
    streams
        .into_iter()
        .map(|mut stream| {
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("read the chart response");
            response
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|status| status.parse::<u16>().ok())
                .expect("a status line")
        })
        .collect()
}

/// Wait for the counted warning to reach the child's log, then return the whole
/// log. The warning is written before its response completes, so a completed
/// response already proves it, and the wait only removes a write-ordering race.
fn wait_for_log(log: &Path, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(needle) || Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn marker_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

/// Covers: the child-start readiness probe's `GET /` render answers 200 before
/// the test seeds, so its empty-database render cannot supply the counted line;
/// the barrier then holds all eight `/charts.json` closures until the eighth
/// arrives, each response is 200, and the created-after-today line appears
/// exactly once in the child's log. A gate that dedupes only within one thread
/// logs the line once per blocking thread and fails this count.
#[test]
fn charts_json_warns_once_per_row_across_parallel_renders() {
    let scratch = Scratch::new("chart_warn_once");
    let db = scratch.path().join("test.db");
    let barrier_dir = scratch.path().join("barrier");
    std::fs::create_dir_all(&barrier_dir).expect("barrier dir");
    let log = scratch.path().join("server.log");
    let server = Server::start(&db, &barrier_dir, &log);

    // The readiness render has completed against an empty database; only now
    // does the row that warns exist.
    seed_future_row(&db);
    let statuses = eight_chart_requests(server.port);
    assert_eq!(
        statuses,
        vec![200; RENDERS],
        "every chart export answers 200\n{}",
        server.log_text()
    );

    let text = wait_for_log(&log, "task created after today");
    let created = text
        .lines()
        .filter(|line| line.contains("task created after today"))
        .count();
    assert_eq!(
        created, 1,
        "the created-after-today line appears exactly once, not once per blocking thread\n{text}"
    );
    assert!(
        !text.contains("test chart barrier deadline passed"),
        "the barrier gathered every render before its deadline\n{text}"
    );
    assert_eq!(
        marker_count(&barrier_dir),
        RENDERS,
        "the barrier directory holds all eight markers"
    );
}
