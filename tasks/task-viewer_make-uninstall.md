---
description: "Add make uninstall that removes the macOS app bundle, Dock tile, Linux desktop entry, and icon, idempotently."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T17:27:02Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Add make uninstall

## Goal

`make uninstall` removes the desktop launcher that `make install` wrote: the macOS app bundle and its Dock tile, or the Linux desktop entry and icon. It is idempotent, prints what it removed, and leaves the database and the launcher's state directory alone.

## Context

`Makefile` has `install install-desktop` and no `uninstall` target, and its `.PHONY` line names `install` and `install-desktop` but not `uninstall`. `scripts/install-desktop.sh` writes `~/Applications/Task Viewer.app` and a Dock `persistent-apps` entry on macOS, and `~/.local/share/applications/task-viewer.desktop` plus `~/.local/share/icons/hicolor/256x256/apps/task-viewer.png` on Linux. Both launchers embed the absolute checkout path. No target removes any of them, and no document offers a removal path: a reinstall replaces the bundle and drops the existing tile, so a reinstall cures a stale launcher but never removes it.

The Dock edit names the constant preference domain `com.apple.dock`, the installer reaches it through `python3` and a `plistlib` round trip, restarts the Dock with `killall Dock`, and exits non-zero when `python3` is missing, so a removal has to reach the same domain and guard the same cases (`scripts/install-desktop.sh`). `make test` runs an explicit script list, so a new test runs only once the `test` target names it.

Two tasks share this surface. [task-viewer_install-desktop-atomic-bundle.md](archive/task-viewer_install-desktop-atomic-bundle.md) landed first: it stages the bundle in a directory beside `~/Applications/Task Viewer.app`, and its Context requires this task's removal set to cover a staging or backup directory a failed install leaves behind. Its documentation step also rewrites the same `README.md` desktop section, `FEATURES.md` `make install` bullet, `SECURITY.md` Data paragraph, and `TESTING.md` installer-case sentence this task's documentation step extends. [task-viewer_launcher-moved-checkout-ownership.md](archive/task-viewer_launcher-moved-checkout-ownership.md) keeps removing an installed Dock tile or `.desktop` entry out of scope and defers it to this task. [task-viewer_dock-pin-home-guard.md](archive/task-viewer_dock-pin-home-guard.md) is implemented in the working tree: it adds `login_account_home` and the `TASK_VIEWER_DOCK_PIN` guard to the installer and rewrites the same four documentation passages, so the removal path reuses its helper and lands on its wording.

## Approach

1. Add an executable `scripts/uninstall-desktop.sh` and a `make uninstall` target that invokes it as a path, the way `install install-desktop` invokes its script, and add `uninstall` to the `.PHONY` line.
2. On macOS, remove the app bundle, then drop the Task Viewer tile from `com.apple.dock` `persistent-apps` with the installer's own branch — `dockutil --remove` when `dockutil` exists, otherwise the `python3`/`plistlib` edit — using its bundle-identifier and label filter, and restart the Dock only when a tile was actually removed. Apply the installer's `login_account_home` guard and `TASK_VIEWER_DOCK_PIN` override to this removal branch too, so a redirected `HOME` never edits the live `com.apple.dock`. Treat a missing bundle or tile as success. When neither tool is available, or the plist edit fails, print the guidance the installer's missing-`python3` branch prints, naming the missing tool and the Dock domain, keep the bundle removal reported, and exit non-zero.
3. On macOS, remove the staging and backup directories that a failed atomic install leaves beside `~/Applications/Task Viewer.app`.
4. On Linux, remove the `.desktop` file and the icon, and refresh the desktop database and the icon cache with the same guarded `update-desktop-database` and `gtk-update-icon-cache` calls the installer makes; treat missing files as success.
5. Print each path removed and a final line saying the database and logs were kept.
6. Do not stop a running server: read the launcher's PID file in its state directory and tell the user to run `make stop` first when that PID names a live process. The read removes nothing from the state directory.
7. Exit non-zero with the installer's `Unsupported platform:` message on a host that is neither macOS nor Linux.
8. Add `scripts/tests/uninstall-desktop.sh`, which builds its own throwaway root and home and stubs `uname`, `defaults`, and `killall` so no run reaches the real Dock: install into it, run uninstall, assert the files are gone, run uninstall again and assert success. Add the script to the `test` target.
9. Document `make uninstall` in `README.md`'s Desktop launcher section, in the `FEATURES.md` `make install` bullet, in `SECURITY.md`'s Data paragraph naming the install targets, and in the `TESTING.md` sentence that enumerates the scripts `make test` runs.

**Out of scope:** removing `~/.task-viewer`, the launcher's state directory, or the checkout, and stopping a running server.

## Acceptance

- `make uninstall` removes the app bundle and the Dock tile on macOS, and the desktop entry and icon on Linux.
- The first run prints each path it removed, then a line saying the database and logs were kept.
- A second run succeeds, reports nothing left to remove, and restarts no Dock when no tile was removed.
- The database and the launcher's state directory are untouched.
- On a macOS run with neither `dockutil` nor `python3`, the Dock step names the missing tool and the `com.apple.dock` domain, the bundle removal is still reported, and the command exits non-zero.
- On a macOS run with a redirected `HOME`, the Dock removal skips the live `com.apple.dock` and the command exits 0, proven by a case in `scripts/tests/uninstall-desktop.sh`.
- A host that is neither macOS nor Linux exits non-zero naming the platform.
- `scripts/tests/uninstall-desktop.sh` is listed in the `make test` target and passes there.
- `README.md`, `FEATURES.md`, `SECURITY.md`, and `TESTING.md` each describe `make uninstall`, with the `TESTING.md` suite sentence naming the new script instead of its three-script list.
- The macOS removal also clears the staging and backup directories a failed atomic install leaves beside the bundle.
