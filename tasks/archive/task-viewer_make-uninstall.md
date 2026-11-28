---
description: "Add make uninstall that removes the macOS app bundle, Dock tile, Linux desktop entry, and icon, idempotently."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T15:23:36Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Add make uninstall

## Goal

`make uninstall` removes the desktop launcher that `make install` wrote: the macOS app bundle and its Dock tile, or the Linux desktop entry and icon. It is idempotent, prints what it removed, and leaves the database and the launcher's state directory alone.

## Context

`Makefile` has `install install-desktop` and no `uninstall` target, and its `.PHONY` line names `install` and `install-desktop` but not `uninstall`. `scripts/install-desktop.sh` writes `~/Applications/Task Viewer.app` and a Dock `persistent-apps` entry on macOS, and `~/.local/share/applications/task-viewer.desktop` plus `~/.local/share/icons/hicolor/256x256/apps/task-viewer.png` on Linux. Both launchers embed the absolute checkout path. No target removes any of them, and no document offers a removal path: a reinstall replaces the bundle and drops the existing tile, so a reinstall cures a stale launcher but never removes it.

The Dock edit names the constant preference domain `com.apple.dock`, the installer reaches it through `python3` and a `plistlib` round trip, restarts the Dock with `killall Dock`, and exits non-zero when `python3` is missing, so a removal has to reach the same domain and guard the same cases (`scripts/install-desktop.sh`). `make test` runs an explicit script list, so a new test runs only once the `test` target names it.

Two tasks share this surface. [task-viewer_install-desktop-atomic-bundle.md](task-viewer_install-desktop-atomic-bundle.md) landed first: it stages the bundle in a directory beside `~/Applications/Task Viewer.app`, and its Context requires this task's removal set to cover a staging or backup directory a failed install leaves behind. Its documentation step also rewrites the same `README.md` desktop section, `FEATURES.md` `make install` bullet, `SECURITY.md` Data paragraph, and `TESTING.md` installer-case sentence this task's documentation step extends. [task-viewer_launcher-moved-checkout-ownership.md](task-viewer_launcher-moved-checkout-ownership.md) keeps removing an installed Dock tile or `.desktop` entry out of scope and defers it to this task. [task-viewer_dock-pin-home-guard.md](task-viewer_dock-pin-home-guard.md) is finished and archived: `login_account_home` and the `TASK_VIEWER_DOCK_PIN` guard are in `scripts/install-desktop.sh`, and it rewrote the same four documentation passages, so the removal path reuses its helper and lands on its wording.

## Approach

1. Add an executable `scripts/uninstall-desktop.sh` and a `make uninstall` target that invokes it as a path, the way `install install-desktop` invokes its script, and add `uninstall` to the `.PHONY` line. The script is self-contained: it carries its own copy of the installer's `login_account_home`/`TASK_VIEWER_DOCK_PIN` guard and of the launcher's state-directory resolution rather than sourcing either script, because `scripts/install-desktop.sh` chmods the launcher at top level and ends in a `case "$(uname -s)"` that installs, and `scripts/task-viewer.sh` creates and chmods `$STATE_DIR` at top level. Neither existing script changes, and each copied rule carries a comment naming the function it mirrors.
2. On macOS, remove the app bundle, then drop the Task Viewer tile from `com.apple.dock` `persistent-apps` with the installer's own branch — `dockutil --remove` when `dockutil` exists, otherwise the `python3`/`plistlib` edit — using its bundle-identifier and label filter, and restart the Dock only when a tile was actually removed. Apply the installer's `login_account_home` guard and `TASK_VIEWER_DOCK_PIN` modes to this removal branch: `auto` (the default) skips the Dock removal unless `HOME` is the login account's home, `force` removes the tile from any home, and `skip` removes no tile. Treat a missing bundle or tile as success. When neither tool is available, or the `com.apple.dock` plist edit fails, print removal guidance of its own — not the installer's add-oriented `dock_pin_guidance`: name the missing tool (or the failed `com.apple.dock` edit), name the `com.apple.dock` domain, and tell the user to remove the Task Viewer tile from the Dock by hand — keep the bundle removal reported, and exit non-zero.
3. On macOS, remove the staging and backup directories that a failed atomic install leaves beside `~/Applications/Task Viewer.app`.
4. On Linux, remove the `.desktop` file and the icon, and refresh the desktop database and the icon cache with the same guarded `update-desktop-database` and `gtk-update-icon-cache` calls the installer makes; treat missing files as success.
5. Print each path removed and a final line saying the database and logs were kept.
6. Do not stop a running server, and do not let one block the removal: when `<state dir>/task-viewer.pid` exists, read it without creating, chmodding, or removing anything in the state directory — resolve that directory the way the launcher does (Darwin `~/Library/Logs/task-viewer`, else `$XDG_STATE_HOME/task-viewer`, else `~/.local/state/task-viewer`) — and when that PID is a live task-viewer release binary by the launcher's own `started_from_path` test, print `task-viewer is still running (PID: <pid>); run make stop first if you want it stopped.` and continue the removal to a normal exit 0.
7. Exit non-zero with the installer's `Unsupported platform:` message on a host that is neither macOS nor Linux.
8. Add `scripts/tests/uninstall-desktop.sh`, which builds its own throwaway root and home and stubs `uname` to report `Darwin` or `Linux`, so no run reaches the real Dock. Each case carries its own stub set:
   - **Dock-removal case**: `id` printing a user name and `dscl` printing `NFSHomeDirectory: $FAKE_LOGIN_HOME` with `FAKE_LOGIN_HOME` set to the throwaway home, so the auto guard reaches the Dock branch; a `defaults` stub whose `export` serves the plist last passed to `import` (initially one carrying a Task Viewer tile — `bundle-identifier` `local.task-viewer.launcher`, `file-label` `Task Viewer` — beside one foreign tile) and whose `import` copies its stdin to `$DEFAULTS_LOG`; a `killall` stub appending its arguments to `$KILLALL_LOG`; and a `PATH` without `dockutil`, so the `python3`/`plistlib` branch runs. Install into the throwaway root, run uninstall, then assert the bundle is gone, the imported plist keeps the foreign tile and drops the Task Viewer tile, and `$KILLALL_LOG` holds one `Dock` line.
   - **Idempotence case**: the same stub set, with uninstall run a second time; assert exit 0, a `nothing left to remove` line, and a `$KILLALL_LOG` line count unchanged from the first run.
   - **Redirected-`HOME` skip case**: `id`/`dscl` reporting a login home that differs from `HOME`, with the same `defaults` and `killall` recorders; assert exit 0 and that neither `$DEFAULTS_LOG` nor `$KILLALL_LOG` exists.
   - **No-tool case**: the Dock-removal stub set with `PATH` limited to the stub `bin` directory plus symlinks to the utilities the script calls, and `bash` invoked by absolute path, so neither `dockutil` nor `python3` is found; assert exit non-zero, the missing tool named, and `com.apple.dock` named.
   - **Running-server case**: the idempotence stub set with a `task-viewer.pid` naming a live stub process whose `ps` line the liveness test accepts; assert the run prints the `make stop` line and exits 0.
   - **Linux case**: `uname` reporting `Linux`; seed `task-viewer/static/task-viewer-icon.png` in the throwaway root, the way `case_icon_resize_failure_falls_back_to_png` seeds it, so `scripts/install-desktop.sh` takes its `if [ -f "$ICON_SOURCE" ]` icon branch; install, assert the `.desktop` file and the installed icon exist, then run uninstall and assert both are gone, and assert a second run exits 0.
   Add the script to the `test` target.
9. Document `make uninstall` in `README.md`'s Desktop launcher section, in the `FEATURES.md` `make install` bullet, in `SECURITY.md`'s Data paragraph naming the install targets, and in the `TESTING.md` sentence that enumerates the scripts `make test` runs.

**Out of scope:** removing `~/.task-viewer`, the launcher's state directory, or the checkout, and stopping a running server.

## Acceptance

- `make uninstall` removes the app bundle and the Dock tile on macOS, and the desktop entry and icon on Linux, with the macOS half proven by the Dock-removal case in `scripts/tests/uninstall-desktop.sh` and the Linux half proven by the Linux case there.
- The first run prints each path it removed, then a line saying the database and logs were kept.
- A second run succeeds, reports nothing left to remove, and restarts no Dock when no tile was removed, proven by the idempotence case in `scripts/tests/uninstall-desktop.sh`.
- The database and the launcher's state directory are untouched.
- A run whose `task-viewer.pid` names a live task-viewer process prints the `make stop` line and still exits 0, proven by a case in `scripts/tests/uninstall-desktop.sh`.
- On a macOS run with neither `dockutil` nor `python3`, the Dock step names the missing tool and the `com.apple.dock` domain, the bundle removal is still reported, and the command exits non-zero, proven by the no-tool case in `scripts/tests/uninstall-desktop.sh`.
- On a macOS run with a redirected `HOME` in the default `auto` mode, the Dock removal skips the live `com.apple.dock` and the command exits 0, proven by a case in `scripts/tests/uninstall-desktop.sh`.
- A host that is neither macOS nor Linux exits non-zero naming the platform.
- `scripts/tests/uninstall-desktop.sh` is listed in the `make test` target and passes there.
- `README.md`, `FEATURES.md`, `SECURITY.md`, and `TESTING.md` each describe `make uninstall`, with the `TESTING.md` sentence that enumerates the scripts `make test` runs naming the new script.
- The macOS removal also clears the staging and backup directories a failed atomic install leaves beside the bundle.
