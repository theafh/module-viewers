---
description: "Fail when cargo is missing, build with --locked, prefer rustup's cargo, build once per restart, and print success only when it happened."
scope: scripts
created: 2026-11-13T13:25:45Z
updated: 2026-11-19T17:21:35Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Make the launcher build and install truthfully

## Goal

A missing `cargo` fails the start with a clear message instead of launching a possibly stale binary. The release build passes `--locked` in both `make build` and the launcher, so a manifest edit cannot rewrite the tracked lockfile. A PATH that puts Homebrew ahead of rustup does not select the wrong toolchain. A click on the Dock icon builds once instead of twice. The installer prints its success checkmark only after the icon-cache refresh succeeds. The no-python3 fallback either detaches the server from the process group or refuses. The toolchain channel is pinned.

## Context

- Build order: first of the launcher pair. Ships before `task-viewer_launcher-process-safety.md`, which takes the launcher as this task leaves it.
- `scripts/task-viewer.sh` returns success from `build_binary` when `cargo` is absent and the binary exists, runs `cargo build --release -p task-viewer` without `--locked`, prepends fixed Homebrew directories ahead of rustup's `~/.cargo/bin`, and builds twice on a Dock-icon click: `do_toggle` runs `if build_binary && is_outdated; then do_start`, and the `do_start` it calls opens with `build_binary || return 1`.
- `Makefile`'s `build` target runs `cargo build --release -p task-viewer` without `--locked`, so the release path `README.md` documents can rewrite `Cargo.lock` even after the launcher is fixed.
- `scripts/install-desktop.sh` prints a checkmark after a `|| true` icon-cache refresh.
- `rust-toolchain.toml` sets `channel = "stable"` with no version, so the compiler enforcing the deny-warnings gate can change without a commit. Its `[toolchain]` table is the only place a channel pin fits; `rust-version` is a Cargo `[package]` key, and `task-viewer/Cargo.toml` is the workspace's only package manifest because the root `Cargo.toml` is a `[workspace]` manifest with no `[package]` table.
- `FEATURES.md` and `README.md` promise the launcher builds the checked-out code.
- Order: this task ships first. [task-viewer_guardrail-doc-sync.md](task-viewer_guardrail-doc-sync.md) then states the pinned channel and the matching `rust-version` in its CHARTER and ARCHITECTURE toolchain passage, and [task-viewer_launcher-process-safety.md](task-viewer_launcher-process-safety.md) then locks the start/stop body and quotes the launcher's no-python3 branch, so shipping this task first leaves that sibling a settled call graph and a refreshed quote.
- Prior art: [task-viewer_build-efficiency.md](task-viewer_build-efficiency.md) (deferred) covers dependencies and profile; this task covers the launcher's build and install honesty.

## Approach

- Fail `build_binary` when `cargo` is absent, naming `make build`.
- Add `--locked` to the release build in both `scripts/task-viewer.sh` and the `build` target of `Makefile`.
- Put rustup's `~/.cargo/bin` ahead of the Homebrew directories in the launcher's PATH.
- Split the build from the start in `scripts/task-viewer.sh`: the body of `do_start` below its build call becomes a build-free start function, `do_start` stays `build_binary || return 1` followed by that function, and `do_toggle` calls `build_binary` once and then the build-free function itself. A Dock-icon click on a running server with a newer binary therefore builds once and restarts on it, instead of building in `do_toggle` and again in the `do_start` it calls. `restart` (`do_stop && do_start`) keeps its one build through `do_start`.
- Branch the installer's checkmark on the icon-cache status.
- On the no-python3 path, use `setsid` when available or refuse to start with an explanation.
- Pin `channel` to an exact version in `rust-toolchain.toml`'s `[toolchain]` table, and add the matching `rust-version` to `[package]` in `task-viewer/Cargo.toml`.

**Out of scope:**

- Launcher locking, PID-file ownership, and kill guards, which `task-viewer_launcher-process-safety.md` owns.
- The CHARTER and ARCHITECTURE toolchain passage, which `task-viewer_guardrail-doc-sync.md` owns once this task has shipped.

## Acceptance

- With `cargo` removed from PATH and a prebuilt binary present, `start` fails and names `make build`.
- The build command in the launcher contains `--locked`, verified by a grep-level test.
- `make -n build` prints a `cargo build --release -p task-viewer` command that carries `--locked`.
- A PATH with Homebrew first still selects `~/.cargo/bin/cargo`, verified by a resolution test.
- A `toggle` run against a running server whose binary is newer than `$STAMP_FILE` builds once, down from two today: a bash-subprocess test seeds `$PID_FILE` with a PID the fake `ps` reports as `$BINARY`, dates `$STAMP_FILE` older than `$BINARY`, and puts a fake `cargo` on PATH that appends one line per invocation to a log; after one `toggle` the log holds exactly one line, and the same test on the current launcher logs two.
- A failing icon-cache refresh prints no success checkmark.
- The no-python3 path either detaches the server or exits with a message; it does not start a server that dies with the process group.
- `rust-toolchain.toml` names a version and `task-viewer/Cargo.toml` carries a matching `rust-version`.
