---
description: "Add Artemis-style task-viewer start/stop/toggle scripts and macOS Dock / Linux menu install with the chart icon."
scope: task-viewer
created: 2026-10-23T16:18:37
updated: 2026-10-23T16:18:37
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Desktop start/stop install for task-viewer

## Goal

Give `task-viewer` the same kind of launcher and desktop install Artemis uses: a shell control for start, stop, status, restart, and toggle, plus an install script that places a chart-icon Dock app on macOS or an application-menu entry on Linux that toggles the local server on and off.

Builds on the shipped binary and port from [task-viewer_initial-dashboard.md](task-viewer_initial-dashboard.md).

## Context

- Control script: `scripts/task-viewer.sh` (release binary under `target/release/task-viewer`, port `7880`, pid/log files at the repo root).
- Install script: `scripts/install-desktop.sh` (macOS `Task Viewer.app` under the user's Applications folder, or a Linux `.desktop` entry).
- Chart icon source: `task-viewer/static/task-viewer-icon.png` (from the favicon chart artwork).
- `make install-desktop` runs the install script; `FEATURES.md` product scope and `README.md` document the launcher.

## Approach

1. Add `scripts/task-viewer.sh` mirroring the Artemis launcher: PATH setup for Dock/GUI launches, build-on-missing release binary with `cargo build --release -p task-viewer`, start via `nohup`, stop with graceful then force kill, default `toggle`, open the browser on start, and desktop notifications.
2. Add `scripts/install-desktop.sh`: Linux desktop entry plus hicolor icon; macOS app bundle with toggle launcher, `AppIcon.icns` from the chart PNG, quarantine clear, and Dock pin.
3. Wire `make install-desktop`, ignore `.task-viewer.pid` / `.task-viewer.log`, and record the behaviour in `FEATURES.md` and `README.md`.

**Out of scope:**

- Changing the HTTP server, harvest, or chart logic inside the `task-viewer` crate.
- Running as a background daemon independent of an explicit start or Dock click.

## Acceptance

- [x] `scripts/task-viewer.sh` supports `start`, `stop`, `status`, `restart`, and `toggle` (default), targeting port `7880`.
- [x] `scripts/install-desktop.sh` installs a macOS Dock app or Linux application-menu entry whose click runs the launcher toggle, using the chart icon.
- [x] `make install-desktop` invokes the install script; pid/log paths are gitignored.
- [x] `FEATURES.md` and `README.md` describe the desktop launcher and manual start/stop commands.
