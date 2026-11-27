# Security

The constraints module-viewers holds to. Identity is `CHARTER.md`, structure is `ARCHITECTURE.md`, behaviour is `FEATURES.md`, and verification is `TESTING.md`.

`task-viewer` is one person, one machine, no account, and no outbound client. The constraints protect the repositories that person pointed at, the parsed rows in `~/.task-viewer`, and a browser-reachable server on a loopback port.

## Secrets and Credentials

The program holds no secret. It has no API key, token, account, or password. A change that introduces one rewrites this section first.

Durable state is the SQLite file `~/.task-viewer`, or the file named by `TASK_VIEWER_DB`. The git tree of this repository stays free of that file and of harvested rows.

## Trust Boundaries and Input

**Repository files are read-only input.** Frontmatter and git history are parsed into rows. The program does not write into the selected folder.

**The database file and the theme cookie are local state.** The database is created on launch when missing. Rows are replaced only by Update, by adding a repository, by rescanning one repository, and by relocating one repository to a new folder, and deleted only by Remove. The theme cookie `task_viewer_theme` holds `light` or `dark`, carries `Path=/`, a one-year `Max-Age`, and `SameSite=Lax`, and is set only by `POST /theme`, which passes the same-origin POST guard, or by the page's own theme script, so a page on another origin cannot set it.

**HTTP names repositories by id.** Add repository takes its path from the folder picker running on this machine. A request body cannot choose an arbitrary path.

**Rendered pages escape text.** Repository names, paths, and scan errors go through the template auto-escaper. The chart JSON escapes `<` before it is embedded.

## Data

Parsed rows stay in `~/.task-viewer`. The binary makes no outbound request. Chart code and styles are files in this repository, served by the same process.

A database file that is not task-viewer's is refused at start-up instead of being adopted. Before it creates or migrates anything, the binary reads the file's tables and columns and refuses one that carries a table or column no task-viewer schema version has, or one missing a table or column its stamped version must have while carrying one that version cannot have, naming the database path and the offending table or column and saying an empty file or a new path is needed. The refused file keeps its tables, rows, and `user_version`; only its journal mode may change, because the connection switches the file to WAL before the check runs. A file this build creates carries `PRAGMA application_id = 0x5456_0001`, so a copy can be recognized as task-viewer's.

On Unix, the database file this build opens is mode `600` whether it creates it or finds it already there, and a `-wal` or `-shm` sidecar beside it is mode `600` too, so no group or other user reads the repository and task paths it holds. A file this build refuses — a foreign schema, a stamp newer than this binary, or an unreadable version — keeps the mode it was found with, an existing parent directory keeps its own mode, and a parent the build creates takes the process umask's mode. A non-Unix build leaves every mode as the umask set it.

The desktop launcher keeps its state in a per-user directory outside the checkout: `~/Library/Logs/task-viewer/` on macOS and `${XDG_STATE_HOME:-~/.local/state}/task-viewer/` on Linux. It holds the PID file, the server log, which records repository paths, the build log, the binary stamp, and a lock directory that lives only while a launcher command runs. The directory is mode `700` and every file the launcher creates there is mode `600`; `ls -l` on it shows `drwx------` for the directory and `-rw-------` for each file. The launcher signals only a PID whose command line is this checkout's release binary.

`make install` writes the desktop launcher into the person's own desktop configuration. On macOS that is `~/Applications/Task Viewer.app` and a Task Viewer tile in the Dock's `persistent-apps` list (`com.apple.dock`), after which the Dock restarts. The Dock pin runs only when `$HOME` is the login account's own home, so an install with a redirected `HOME`, or one whose account home cannot be determined, still installs the bundle but reads and writes no Dock preference; `TASK_VIEWER_DOCK_PIN=force` pins from any home and `TASK_VIEWER_DOCK_PIN=skip` skips the pin. The macOS install builds the new bundle in a hidden `.Task Viewer.app.staging.*` directory beside `~/Applications/Task Viewer.app` and moves the previous bundle into a `.Task Viewer.app.backup.*` directory for the swap, so a failed install can leave one of those directories beside the bundle, and a successful swap whose backup cleanup fails leaves the `.Task Viewer.app.backup.*` directory behind and names it; both hold only the bundle the installer built from this checkout. On Linux it is `~/.local/share/applications/task-viewer.desktop` and `~/.local/share/icons/hicolor/256x256/apps/task-viewer.png`. These files carry the path to this checkout and nothing from the viewed repositories.

## Attack Surface

- The server binds `127.0.0.1:7880`, or the port named by `TASK_VIEWER_PORT` on that same loopback address. The binary reads that variable on every build, release included, and the launcher drops an inherited value and exports its own port, so a non-default port is for an isolated test. The host allowlist and the same-origin origins stay derived from the default port, so a server on another port binds but rejects every request whose `Host` names that port. A `Host` other than loopback (`127.0.0.1`, `localhost`, `[::1]`, with optional `:7880`) is rejected.
- Every response carries `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and `Content-Security-Policy: frame-ancestors 'none'`. The layers sit outside the `Host` check, so the 403 that rejects a foreign `Host` carries them too, and they cover static files and the router's own 404.
- A state-changing `POST` is accepted only when its `Origin` names a loopback origin (`http://127.0.0.1:7880`, `http://localhost:7880`, or `http://[::1]:7880`), or, when `Origin` is absent or the opaque value `null`, when `Sec-Fetch-Site` is `same-origin`, `none`, or absent. A `cross-site` or `same-site` `Sec-Fetch-Site` is refused, which covers a page on another loopback port, while an older browser sends neither header and still submits. Every other `POST` gets 403 before its route runs.
- A dashboard read that names a non-empty `flash` parameter changes server state, because reading the banner removes it, so it is accepted only from the dashboard's own page: a loopback `Origin`, or, when `Origin` is absent or the opaque value `null`, a `Sec-Fetch-Site` that is `same-origin`, `none`, or absent. A `cross-site` or `same-site` read gets 403 before the banner is taken, which covers a no-`Origin` subresource load from another loopback port. The guard covers every method that reaches the dashboard handler, `GET` and `HEAD` alike, and decodes the query the way that handler does, so a percent-encoded key such as `fl%61sh` names the parameter too. The redirect Update leaves is a top-level navigation, which sends `same-origin` or `none` (or, on an older browser, neither header), so it still renders.
- A page query names a banner only by a code from the closed set `Banner` declares. A `notice` or `error` value outside that set, or a code placed in the wrong severity's field, renders nothing, so a link cannot place its own text on the dashboard or the Repositories page. The Update outcome, its per-repository failure text included, travels through the one-shot flash slot rather than the query.
- Static files come from `task-viewer/static/`. Their responses carry `Cache-Control: public, max-age=31536000, immutable`, and every `/static` URL the templates emit carries a version derived from the asset bytes as read when the process starts. The version is fixed for the process lifetime, while `/static` is served from disk on each request, so an asset edited under the running process is served at the unchanged URL to a client that fetches it. Editing a static asset therefore needs a server restart before the browser sees the new bytes. Page responses carry `Cache-Control: no-store`.
- Remove asks for confirmation on its own server-rendered page at `GET /repositories/{id}/delete`, which names the repository and its folder and carries a form posting to that same path, so a browser without scripting deletes nothing until the form is posted. The POST is the only mutation on the path.

## Dependencies

Dependencies stay pinned in `Cargo.lock`. Run `make audit` when they change. It runs three checks: `cargo audit` over the lockfile, `scripts/vendor-sqlx-sqlite-sha256.sh verify`, which recomputes the per-file SHA-256 digests under `vendor/sqlx-sqlite` and fails on an added, a missing, or a changed file against the recorded `vendor/sqlx-sqlite.sha256`, and `scripts/check-rsa-reachability.sh`, which runs `cargo tree -i rsa` and fails when the reverse listing names `rsa`.

`Cargo.lock` also pins `rsa` 0.9.10 through `sqlx-mysql`, an optional `sqlx` backend that this workspace never builds. `cargo audit` reports RUSTSEC-2023-0071 against that crate, and the advisory has no fixed upgrade. `.cargo/audit.toml` records it in `[advisories] ignore`, and `make audit` rechecks reachability with `cargo tree -i rsa`, which prints nothing while `rsa` stays outside the build graph, so the ignore cannot outlive its justification. A dependency change that pulls `rsa` into a built target removes the ignore and fixes the advisory instead.

The SQLite driver is `sqlx-sqlite` 0.8.6 vendored at `vendor/sqlx-sqlite`; its only change from the published crate is a widened `libsqlite3-sys` bound (`>=0.30.1, <0.40.0`), matching sqlx's own upstream bump. That bound lets `Cargo.lock` carry `libsqlite3-sys` 0.39.1, whose bundled SQLite 3.54.1 carries the fix for RUSTSEC-2026-0459, and a database test pins the floor at 3.54.0. A path-patched crate carries no `Cargo.lock` checksum and `cargo audit` never scans it, so `vendor/sqlx-sqlite.sha256` records one SHA-256 line per tracked file and `make audit` verifies it, which turns the gate red when a vendored byte changes. Remove the vendored copy and the `[patch.crates-io]` entry once a released `sqlx` carries the widened bound.

## Per-Change Checklist

- [ ] No outbound network client introduced.
- [ ] No secret introduced.
- [ ] New durable writes stay in `~/.task-viewer` or `TASK_VIEWER_DB`, in the launcher's per-user state directory, or in the desktop install targets named under Data.
- [ ] Selected repositories are read, not written.
- [ ] Routes still name repositories by id.
- [ ] The listener stays on `127.0.0.1`.
- [ ] Cross-site `POST`s stay rejected before their route runs.
- [ ] Every response still carries the security headers: `X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`, and the framing-only `Content-Security-Policy`.
- [ ] User-facing text stays escaped.
- [ ] Remove still asks for confirmation.
