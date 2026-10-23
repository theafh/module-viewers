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

- **Add repository** opens a folder picker. The folder needs a `tasks/` directory.
- **Project** filters every chart. All projects is the default. One project switches the radar charts from a spoke per project to a spoke per task scope.
- **Update** scans every configured repository again.
- **Light** and **Dark** switch the dashboard appearance. The choice stays in the browser.

### Desktop launcher (Ubuntu / macOS)

```bash
./scripts/install-desktop.sh
# or
make install-desktop
```

- **Linux:** application-menu entry you can pin to favorites.
- **macOS:** `~/Applications/Task Viewer.app`, pinned to the Dock automatically.
  Click toggles start/stop and opens the browser when starting.

Manual control:

```bash
./scripts/task-viewer.sh start
./scripts/task-viewer.sh stop
./scripts/task-viewer.sh status
```

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
