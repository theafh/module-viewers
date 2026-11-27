---
description: "Pin the Dock only when HOME is the login account's home, add a force/skip override, and keep sandboxed install runs out of the live com.apple.dock."
scope: task-viewer
created: 2026-11-27T09:43:52Z
updated: 2026-11-27T13:24:40Z
status: implemented
implemented-by: "Andreas Hoffmann"
design-extended: true
reported-by: "Andreas Hoffmann"
---

# Refuse the Dock pin outside the login account's home

## Goal

`scripts/install-desktop.sh` pins the Dock only for the login account's own install. When `$HOME` differs from the account's home, or that home cannot be determined, the macOS install still builds and installs `~/Applications/Task Viewer.app`, prints the existing manual-pin guidance, and finishes with status 0 without reading or writing `com.apple.dock`. `TASK_VIEWER_DOCK_PIN=force` pins from a throwaway home, and `TASK_VIEWER_DOCK_PIN=skip` skips the pin.

## Context

`pin_to_dock()` in `scripts/install-desktop.sh` reaches the live Dock preference domain through both branches: `dockutil --add "$app_bundle" --replacing "$APP_NAME"`, and the `python3` fallback that runs `defaults export com.apple.dock` / `defaults import com.apple.dock`. Neither branch consults `$HOME`, and `defaults` addresses the login account's preference domain regardless of it, so any run with a redirected `HOME` rewrites the person's real Dock.

The motivating case: a reproduction run under `$TMPDIR` installed into a throwaway home and pinned its bundle into the live Dock. That bundle had an empty `Contents/Resources` and a launcher exec'ing a stub, so the real tile lost its icon and did nothing on click.

`install_macos` calls `pin_to_dock "$app_bundle"` bare under `set -e`, so a non-zero return aborts the install after the bundle is already in place; a skipped pin returns 0. `dock_pin_guidance` prints `Cannot pin Task Viewer to the Dock automatically.` plus the bundle path, and `FEATURES.md`'s `make install` bullet already promises that a failed Dock pin prints how to add the app by hand.

`scripts/tests/launcher-build-truth.sh` runs a copy of the installer with `HOME="$root/home"` and relies on a `dockutil` stub first on `PATH` in the cases that assert pinning. Under the new guard those runs skip the pin unless they set the override. The live [task-viewer_make-uninstall.md](task-viewer_make-uninstall.md) will reuse this branch for the removal path, so the home check belongs in a named function it can call; that removal path stays with the uninstall task.

The affected documentation passages are `FEATURES.md`'s `make install` bullet, `SECURITY.md`'s Data paragraph that names the desktop write targets, `README.md`'s `### Desktop launcher (Ubuntu / macOS)` section, and the `TESTING.md` sentence that enumerates the installer cases `make test` runs.

## Approach

1. Add a `login_account_home` helper to `scripts/install-desktop.sh` that prints the login account's home directory. It resolves the account name with `user="$(id -un)"` under an `if !` guard that returns non-zero when that lookup fails, then prints the home from `dscl . -read "/Users/$user" NFSHomeDirectory` when `dscl` exists, falling back to `eval echo "~$user"`. It accepts a result only when it is an absolute path and returns non-zero otherwise, so the printed home comes from the account database rather than `$HOME`.
2. Read the pin mode from `TASK_VIEWER_DOCK_PIN`, defaulting to `auto`, and branch at the top of `pin_to_dock()` before either Dock mechanism runs:
   - `force` pins as today;
   - `skip` prints `Skipping the Dock pin: TASK_VIEWER_DOCK_PIN=skip.` followed by `dock_pin_guidance`, and returns 0;
   - `auto` pins only when `$HOME` equals `login_account_home`; otherwise it prints `Skipping the Dock pin: HOME ($HOME) is not the login account's home.` followed by `dock_pin_guidance`, and returns 0. A failed detection takes the same skip path, so an unreadable account never falls back to writing the live Dock.
3. Keep the existing `dockutil` and `python3` failure behaviour unchanged once pinning is attempted, so a real pin failure still prints the guidance and makes `make install` exit non-zero.
4. In `scripts/tests/launcher-build-truth.sh`, add four `case_*` functions and call each of them by name in the invocation list at the bottom of the file beside the existing calls, because the suite runs a case only through such a call and a defined-but-uncalled case leaves the suite green:
   - `case_redirected_home_skips_dock_pin` runs the installer with `HOME` under a throwaway root, a `defaults` stub first on `PATH` that appends its arguments to a log, and no `dockutil`, then asserts the log was never created;
   - `case_login_home_pins_dock` stubs `id` to print a fake account and `dscl` to print `NFSHomeDirectory: $root/home`, runs with `HOME="$root/home"`, and asserts a `dockutil` stub recorded `--add`;
   - `case_unknown_login_home_skips_dock_pin` stubs `dscl` to fail and `id` to print an account name that has no home directory, runs with `HOME` under a throwaway root and `defaults` and `dockutil` stubs first on `PATH` that each append to their own log, and asserts the install still exits 0;
   - `case_dock_pin_skip_override_skips` runs with `TASK_VIEWER_DOCK_PIN=skip` and a recording `dockutil` stub and asserts the log does not exist.
5. Set `TASK_VIEWER_DOCK_PIN=force` on the cases that assert pinning: `case_missing_python3_fails_install_with_explanation`, `case_failing_dockutil_prints_guidance`, `case_failing_defaults_prints_guidance`, and `case_clean_install_replaces_bundle`.
6. Rewrite the four documentation passages in place, each stating the condition once: the pin runs only from the login account's home, a redirected `HOME` skips it and prints the manual guidance, and the override names both modes.

**Out of scope:** extending the same guard to the removal path, which the live uninstall task owns, and repairing a Dock tile that an earlier sandboxed run already replaced, which a reinstall from the login account corrects.

## Acceptance

- `bash scripts/tests/launcher-build-truth.sh` passes, its output carries the new mismatch case's `ok -` line, and the case runs the installer from a throwaway `HOME`, a `defaults` stub first on `PATH`, and no `dockutil`, and asserts the stub's log does not exist, the output carries `is not the login account's home`, the exit status is 0, and `$root/home/Applications/Task Viewer.app/Contents/MacOS/task-viewer-launcher` exists.
- A case stubs `id` and `dscl` so `login_account_home` reports the throwaway home, runs with `HOME` equal to it and a recording `dockutil` stub, asserts the stub's log holds `--add`, and its `ok -` line appears in the suite output.
- A case stubs `dscl` to fail and `id` to print an account name that has no home directory, runs with `HOME` under a throwaway root and `defaults` and `dockutil` stubs first on `PATH` that each append to their own log, and asserts exit status 0, the output carries `Cannot pin Task Viewer to the Dock automatically.`, and neither stub log exists, and its `ok -` line appears in the suite output.
- A case runs with `TASK_VIEWER_DOCK_PIN=skip` and a recording `dockutil` stub and asserts the log does not exist while the install still exits 0.
- The four pin-asserting cases set `TASK_VIEWER_DOCK_PIN=force` and still observe their expected `dockutil` call or pin-failure guidance.
- `FEATURES.md`, `SECURITY.md`, `README.md`, and `TESTING.md` each carry one statement of the condition and the override, with `TESTING.md`'s `make test` sentence naming the new cases and no stale claim that a redirected `HOME` pins.
- `make test` passes.
