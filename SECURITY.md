# Security

The constraints module-viewers holds to. Identity is `CHARTER.md`, structure is `ARCHITECTURE.md`, behaviour is `FEATURES.md`, and verification is `TESTING.md`.

`task-viewer` is one person, one machine, no account, and no outbound client. The constraints protect the repositories that person pointed at, the parsed rows in `~/.task-viewer`, and a browser-reachable server on a loopback port.

## Secrets and Credentials

The program holds no secret. It has no API key, token, account, or password. A change that introduces one rewrites this section first.

Durable state is the SQLite file `~/.task-viewer`, or the file named by `TASK_VIEWER_DB`. The git tree of this repository stays free of that file and of harvested rows.

## Trust Boundaries and Input

**Repository files are read-only input.** Frontmatter and git history are parsed into rows. The program does not write into the selected folder.

**The database file is local state.** It is created on launch when missing. Rows are replaced only by Update and by adding a repository, and deleted only by Remove.

**HTTP names repositories by id.** Add repository takes its path from the folder picker running on this machine. A request body cannot choose an arbitrary path.

**Rendered pages escape text.** Repository names, paths, and scan errors go through the template auto-escaper. The chart JSON escapes `<` before it is embedded.

## Data

Parsed rows stay in `~/.task-viewer`. The binary makes no outbound request. Chart code and styles are files in this repository, served by the same process.

The desktop launcher keeps its PID file and the server log, which records repository paths, in a per-user directory outside the checkout: `~/Library/Logs/task-viewer/` on macOS and `${XDG_STATE_HOME:-~/.local/state}/task-viewer/` on Linux. The directory is mode `700` and both files are mode `600`. The launcher signals only a PID whose command line is this checkout's release binary.

`make install` writes the desktop launcher into the person's own desktop configuration. On macOS that is `~/Applications/Task Viewer.app` and a Task Viewer tile in the Dock's `persistent-apps` list (`com.apple.dock`), after which the Dock restarts. On Linux it is `~/.local/share/applications/task-viewer.desktop` and `~/.local/share/icons/hicolor/256x256/apps/task-viewer.png`. These files carry the path to this checkout and nothing from the viewed repositories.

## Attack Surface

- The server binds `127.0.0.1:7880`. A `Host` other than loopback (`127.0.0.1`, `localhost`, `[::1]`, with optional `:7880`) is rejected.
- Static files come from `task-viewer/static/`.
- Remove asks for confirmation in the page and uses its own POST route.

## Dependencies

Dependencies stay pinned in `Cargo.lock`. Run `make audit` when they change.

## Per-Change Checklist

- [ ] No outbound network client introduced.
- [ ] No secret introduced.
- [ ] New durable writes stay in `~/.task-viewer` or `TASK_VIEWER_DB`, in the launcher's per-user state directory, or in the desktop install targets named under Data.
- [ ] Selected repositories are read, not written.
- [ ] Routes still name repositories by id.
- [ ] The listener stays on `127.0.0.1`.
- [ ] User-facing text stays escaped.
- [ ] Remove still asks for confirmation.
