# module-viewers

Local viewers for repositories that keep their work in plain files. The first viewer is `task-viewer`, a dashboard for repositories that use a `tasks/` tree.

This repository holds the programs, their guardrails, and the build tooling. It does not hold harvested task data. Each machine that runs a viewer keeps that data in the home directory of the person who runs it.

## task-viewer

`task-viewer` is one Rust binary. It serves a browser dashboard on `127.0.0.1:7880`. The person running it adds repository folders with a folder picker, then reads eight charts of how tasks are created and shipped.

The configuration and the parsed chart rows live in a SQLite database at `~/.task-viewer`. Update re-reads every configured repository and replaces that repository's rows. Removing a repository from the dashboard deletes only its rows in that database. The source repository is left unchanged.

## Run

From this directory:

```bash
make run
```

Open <http://127.0.0.1:7880>.

- **Dashboard** is the default tab and shows the charts. **Repositories** lists configured folders and is where Add repository lives.
- **Add repository** opens a folder picker. The folder needs a `tasks/` directory.
- **Project** filters every chart. All projects is the default. One project switches the radar charts from a spoke per project to a spoke per task scope.
- **Update** scans every configured repository again.
- **Light** and **Dark** switch the page appearance. The choice stays in the browser.

### Desktop launcher (Ubuntu / macOS)

```bash
make install
```

- **Linux:** application-menu entry you can pin to favorites.
- **macOS:** `~/Applications/Task Viewer.app`, pinned to the Dock automatically.
  Click toggles start/stop and opens the browser when starting.

Manual control:

```bash
make start
make stop
make status
```

The launcher writes its PID file and the server log to `~/Library/Logs/task-viewer/` on macOS and to `${XDG_STATE_HOME:-~/.local/state}/task-viewer/` on Linux. `make start` and a click on the icon rebuild the release binary first, so after a source change they restart the server on the new code; a failed build is reported and nothing stale is started. The launcher stops only the release binary from this checkout; when another program holds port 7880, start reports that and leaves it running.

## Build

```bash
make build
make test
make lint
```

The release binary is `target/release/task-viewer`.

## Layout

```text
task-viewer/          # dashboard binary, templates, and static assets
CHARTER.md            # identity and boundaries
ARCHITECTURE.md       # structure and data rules
FEATURES.md           # behaviour ledger
SECURITY.md           # trust boundaries
TESTING.md            # how the suite runs
```
