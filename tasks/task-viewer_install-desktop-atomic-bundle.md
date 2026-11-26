---
description: "Build the macOS app bundle in a temp path and swap it in only after it is complete, so a failed reinstall cannot destroy a working icon."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T20:01:46Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Install the desktop bundle atomically

## Goal

A `make install` that fails on macOS before the swap leaves the previously working app bundle in place. A failing icon step reports that the icon build was skipped and a PNG was used, so the bundle still installs complete. A failing Dock pin prints the manual-pin guidance instead of a bare traceback.

## Context

`scripts/install-desktop.sh` runs `set -e` and deletes the working bundle before rebuilding it:

```bash
rm -rf "$app_bundle"
```

A `mkdir` or disk failure after that line aborts with the working bundle already gone.

The icon step does not abort. `build_macos_icns` is called only as the `if` condition of the icon branch, and bash ignores `set -e` inside a function called in a tested condition, so a failing `sips` falls through to `iconutil` and then to the PNG fallback while the bundle is still written. The only message is `⚠ iconutil failed; copied PNG into Resources`, which blames `iconutil` and never says the icon build was skipped.

The Dock pin fails badly. `dockutil --add` is unguarded, so a failing `dockutil` exits right after `✓ App bundle created` with no message. Without `dockutil`, the Python block raises an uncaught `CalledProcessError` from `defaults export` or `defaults import`, or a `plistlib` error from an unreadable Dock plist, and the traceback prints after `✓ App bundle created`, so it reads like a crash rather than "the Dock pin failed".

Reinstall is the repair path for a stale icon, and it is the step that can destroy the thing it repairs. The staging and backup directories this task adds sit beside `~/Applications/Task Viewer.app`, the same bundle `tasks/task-viewer_make-uninstall.md` removes; that task's removal set has to cover a staging or backup directory a failed install leaves behind, so this task lands first and the uninstall task follows it.

## Approach

1. Build the new bundle under a temp directory beside the target, and validate that its executable and `Info.plist` exist before touching the installed bundle.
2. Swap atomically: move the old bundle aside, move the new one into place, then remove the backup; restore the old bundle if a move fails, and cover an interrupt between the two moves with a trap so the install path is never left empty.
3. Clear quarantine (`xattr -dr com.apple.quarantine`) on the installed bundle after the swap, so the newly moved-in copy loses the flag rather than the backup.
4. Set a flag when a `sips` resize fails, skip `iconutil` on that flag, keep the PNG copy as the fallback, and print that the icon build was skipped and a PNG was used, so the bundle still carries an icon file.
5. Share one "pin it by hand" guidance function whose wording names no missing tool, and route every pin failure through it: a failing `dockutil --add`, a failing `defaults export`, a `plistlib` error from an unreadable Dock plist, and the `defaults import` failure. The missing-`python3` branch keeps its own python3 line and then calls the shared guidance, so no traceback is the only message.
6. Add cases to `scripts/tests/launcher-build-truth.sh` with stubs: a failure that aborts before the swap (such as a failing `cat > "$launcher_bin"` heredoc write) leaves the previous bundle intact; a failing `sips` prints the icon-skip message and installs a usable new bundle; a failing `defaults` and a failing `dockutil` each print the guidance; a clean run replaces the bundle.
7. Record the atomic install, the icon-skip behaviour, and the staging directory in `README.md`'s desktop section, `FEATURES.md` section 1's `make install` bullet, `SECURITY.md`'s Data paragraph naming the staging path beside `~/Applications/Task Viewer.app`, and `TESTING.md`'s installer-case sentence.

**Out of scope:** the Linux `.desktop` write, and the Dock's own data format.

## Acceptance

- A failure before the swap (such as a failing `cat > "$launcher_bin"` write) leaves the previous bundle usable.
- A failing `sips` prints the icon-skip message and still installs a working bundle.
- A failing `dockutil --add` and a failing `defaults import` each print the manual guidance and do not look like a crash.
- A clean run replaces the bundle and pins it as before.
- The new cases in `scripts/tests/launcher-build-truth.sh` pass under `make test`.
- `README.md`'s desktop section, `FEATURES.md` section 1's `make install` bullet, `SECURITY.md`'s Data paragraph, and `TESTING.md`'s installer-case sentence each describe the atomic install and the staging path, with no stale description left.
