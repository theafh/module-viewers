---
description: "Fail when cargo is missing, build with --locked, prefer rustup's cargo, build once per restart, and print success only when it happened."
scope: scripts
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Make the launcher build and install truthfully

## Goal

A missing `cargo` fails the start with a clear message instead of launching a possibly stale binary. The release build uses `--locked`, so a manifest edit cannot rewrite the tracked lockfile. A PATH that puts Homebrew ahead of rustup does not select the wrong toolchain. A click on the Dock icon builds once instead of twice. The installer prints its success checkmark only after the icon-cache refresh succeeds. The no-python3 fallback either detaches the server from the process group or refuses. The toolchain channel is pinned.

## Context

- `scripts/task-viewer.sh` returns success from `build_binary` when `cargo` is absent and the binary exists, runs `cargo build --release -p task-viewer` without `--locked`, prepends fixed Homebrew directories ahead of rustup's `~/.cargo/bin`, and builds twice on a Dock-icon click: `do_toggle` builds before deciding, then the `do_start` it calls builds again.
- `scripts/install-desktop.sh` prints a checkmark after a `|| true` icon-cache refresh.
- `rust-toolchain.toml` sets `channel = "stable"` with no version, so the compiler enforcing the deny-warnings gate can change without a commit.
- `FEATURES.md` and `README.md` promise the launcher builds the checked-out code.
- Prior art: [task-viewer_build-efficiency.md](archive/task-viewer_build-efficiency.md) (deferred) covers dependencies and profile; this task covers the launcher's build and install honesty.

## Approach

- Fail `build_binary` when `cargo` is absent, naming `make build`.
- Add `--locked` to the release build.
- Put rustup's `~/.cargo/bin` ahead of the Homebrew directories in the launcher's PATH.
- Restructure the `toggle` path, the one the Dock icon runs (the launcher script the installer writes with a `toggle` argument in `scripts/install-desktop.sh`), so it builds once: `do_toggle` builds and then calls `do_start`, which builds again. Leave `restart` (`do_stop && do_start`) alone; it already builds once.
- Branch the installer's checkmark on the icon-cache status.
- On the no-python3 path, use `setsid` when available or refuse to start with an explanation.
- Pin `channel` and add `rust-version` in `rust-toolchain.toml`, mirrored in the manifest.

## Acceptance

- With `cargo` removed from PATH and a prebuilt binary present, `start` fails and names `make build`.
- The build command in the launcher contains `--locked`, verified by a grep-level test.
- A PATH with Homebrew first still selects `~/.cargo/bin/cargo`, verified by a resolution test.
- A click on the Dock icon (the default `toggle` path, not the `restart` subcommand, which already builds once) runs one `cargo build`, down from two today.
- A failing icon-cache refresh prints no success checkmark.
- The no-python3 path either detaches the server or exits with a message; it does not start a server that dies with the process group.
- `rust-toolchain.toml` names a version and `Cargo.toml` carries a matching `rust-version`.
- `make lint` passes.
