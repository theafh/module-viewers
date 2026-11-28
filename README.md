# module-viewers

Local viewers for repositories that keep their work in plain files. The first viewer is `task-viewer`, a dashboard for repositories that use a `tasks/` tree.

This repository holds the programs, their guardrails, and the build tooling. It does not hold harvested task data. Each machine that runs a viewer keeps that data in the home directory of the person who runs it.

## task-viewer

`task-viewer` is one Rust binary. It serves a browser dashboard on `127.0.0.1:7880`. The person running it adds repository folders with a folder picker, then reads a dashboard of charts showing how tasks are created and shipped.

The configuration and the parsed chart rows live in a SQLite database at `~/.task-viewer`. Update re-reads every configured repository and replaces that repository's rows. Removing a repository from the dashboard deletes only its rows in that database. The source repository is left unchanged.

Set `TASK_VIEWER_SCAN_WORKERS` to change how many repositories Update scans at once; it takes a whole number from 1 to 32 and defaults to 4, a larger value is clamped to 32, and a value below 1 or one that is not a number falls back to 4 with a warning. Set `TASK_VIEWER_GIT_TIMEOUT_SECS` to change the deadline for one read-only git command; it defaults to 30 seconds, a whole number of seconds from 1 to 600 is used as given, a larger value is clamped to 600, and a value below 1 or one that is not a number falls back to 30 seconds with a warning.

## Run

From this directory:

```bash
make run
```

Open <http://127.0.0.1:7880>.

- **Dashboard** is the default tab and shows the charts. **Repositories** lists configured folders and is where Add repository lives.
- **Add repository** opens a folder picker. A folder is accepted when its working tree has a `tasks/` directory, or when a branch of the repository that holds it lists a markdown task under `tasks/`.
- **Project** filters every chart. All projects is the default. One project switches the radar charts from a spoke per project to a spoke per task scope.
- **Update** scans every configured repository again.
- **Light** and **Dark** switch the page appearance. The choice stays in the browser.

### Desktop launcher (Ubuntu / macOS)

```bash
make install
```

- **Linux:** application-menu entry you can pin to favorites.
- **macOS:** `~/Applications/Task Viewer.app`, pinned to the Dock automatically when `make install` runs from your own account's home.
  Click toggles start/stop and opens the browser when starting.

A reinstall builds the new bundle in a hidden `.Task Viewer.app.staging.*` directory beside `~/Applications/Task Viewer.app` and swaps it in only after its executable and `Info.plist` exist, so a failure before the swap leaves the previous app in place. A failed install can leave that staging directory beside the bundle, a restore that itself fails leaves the previous bundle in the `.Task Viewer.app.backup.*` directory the installer names, and a successful swap whose backup cleanup fails leaves that backup directory behind and names it. When the icon build fails, the install reports that the icon build was skipped and copies the PNG instead, so the bundle still carries an icon; when the Dock pin fails, it prints how to add the app by hand. The Dock pin runs only from the login account's own home: an install with a redirected `HOME`, or one whose account home cannot be determined, still installs the bundle and prints the manual-pin guidance without touching `com.apple.dock`. Set `TASK_VIEWER_DOCK_PIN=force` to pin from any home, or `TASK_VIEWER_DOCK_PIN=skip` to skip the pin.

Manual control:

```bash
make start
make stop
make status
```

The launcher keeps its state in `~/Library/Logs/task-viewer/` on macOS and in `${XDG_STATE_HOME:-~/.local/state}/task-viewer/` on Linux: the PID file, the server log, the build log, the binary stamp, and a lock directory it removes when the command finishes. The directory is mode `700` and every file the launcher creates there is mode `600`. A state path that would be relative — from a relative `XDG_STATE_HOME`, or a relative `HOME` on either platform — is refused before the directory is created, naming the variable that made it relative and saying the path must be absolute. `make start` and a click on the icon rebuild the release binary first, so after a source change they restart the server on the new code; a failed build is reported and nothing stale is started, and a missing `cargo` fails the start with a message naming `make build`. The launcher stops only a task-viewer release binary — this checkout's, or one started from any other checkout's path, a moved or renamed checkout included, whose started-from path it reports — and when another program holds port 7880, start reports that and leaves it running.

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
