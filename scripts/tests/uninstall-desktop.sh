#!/bin/bash
# Tests for scripts/uninstall-desktop.sh.
#
# Every case runs copies of the installer and the uninstaller inside a
# throwaway root and home, so no real app bundle, desktop entry, Dock
# preference, or state directory is touched. `uname` is stubbed to report
# Darwin or Linux, `defaults` and `killall` are recording stubs, and no case
# keeps the real /opt/homebrew or /usr/local bin directories on PATH unless it
# needs a tool from them.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAUNCHER_SOURCE="$REPO/scripts/task-viewer.sh"
INSTALLER_SOURCE="$REPO/scripts/install-desktop.sh"
UNINSTALLER_SOURCE="$REPO/scripts/uninstall-desktop.sh"
ICON_SOURCE="$REPO/task-viewer/static/task-viewer-icon.png"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/task-viewer-uninstall.XXXXXX")"
TMP="$(cd "$TMP" && pwd -P)"
FAILURES=0

trap 'rm -rf "$TMP"' EXIT

pass() {
    echo "ok - $1"
}

fail() {
    echo "not ok - $1"
    FAILURES=$((FAILURES + 1))
}

# A checkout-shaped root with the installer and uninstaller under scripts/, a
# private home, a stub bin directory, and the icon source the Linux install
# guards on. `$1` is the root; `$2` is the platform its uname stub reports.
make_root() {
    local root="$1" platform="$2"
    mkdir -p "$root/scripts" "$root/home" "$root/bin" \
        "$root/task-viewer/static" "$root/target/release"
    cp "$LAUNCHER_SOURCE" "$INSTALLER_SOURCE" "$UNINSTALLER_SOURCE" "$root/scripts/"
    cp "$ICON_SOURCE" "$root/task-viewer/static/task-viewer-icon.png"
    cat >"$root/bin/uname" <<EOS
#!/bin/bash
printf '%s\n' "$platform"
EOS
    chmod +x "$root/bin/uname"
}

# A `defaults` stub that serves the last imported plist on export and records
# each import. The uninstaller imports a binary plist; the next export hands it
# back, so a second run sees the tile already gone.
make_defaults_stub() {
    local root="$1"
    cat >"$root/bin/defaults" <<'EOS'
#!/bin/bash
case "$1" in
    export) cat "$DEFAULTS_STATE" ;;
    import)
        cat >"$DEFAULTS_STATE"
        printf 'import\n' >>"$DEFAULTS_LOG"
        ;;
esac
EOS
    chmod +x "$root/bin/defaults"
}

# A `killall` stub that records each invocation.
make_killall_stub() {
    local root="$1"
    cat >"$root/bin/killall" <<'EOS'
#!/bin/bash
printf '%s\n' "$*" >>"$KILLALL_LOG"
EOS
    chmod +x "$root/bin/killall"
}

# A `dockutil` stub that prints the documented tab-separated list form and drops
# the tile on --remove, recording the arguments it saw.
make_dockutil_stub() {
    local root="$1"
    cat >"$root/bin/dockutil" <<'EOS'
#!/bin/bash
case "$1" in
    --list)
        [ -f "$DOCKUTIL_STATE" ] || exit 0
        printf 'Task Viewer\tfile:///Applications/Task%%20Viewer.app/\tpersistent-apps\tlocal.task-viewer.launcher\n'
        printf 'Finder\tfile:///System/Library/CoreServices/Finder.app/\tpersistent-apps\tcom.apple.finder\n'
        ;;
    --remove)
        printf '%s\n' "$*" >>"$DOCKUTIL_LOG"
        rm -f "$DOCKUTIL_STATE"
        ;;
esac
EOS
    chmod +x "$root/bin/dockutil"
}

# `id` and `dscl` stubs that make login_account_home report $FAKE_LOGIN_HOME.
make_login_home_stubs() {
    local root="$1"
    printf '#!/bin/bash\nprintf "%%s\\n" "testuser"\n' >"$root/bin/id"
    cat >"$root/bin/dscl" <<'EOS'
#!/bin/bash
printf 'NFSHomeDirectory: %s\n' "$FAKE_LOGIN_HOME"
EOS
    chmod +x "$root/bin/id" "$root/bin/dscl"
}

# A Dock plist with one Task Viewer tile and one foreign tile.
write_dock_plist() {
    cat >"$1" <<'EOS'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>persistent-apps</key>
    <array>
        <dict>
            <key>tile-data</key>
            <dict>
                <key>file-label</key>
                <string>Task Viewer</string>
                <key>bundle-identifier</key>
                <string>local.task-viewer.launcher</string>
            </dict>
            <key>tile-type</key>
            <string>file-tile</string>
        </dict>
        <dict>
            <key>tile-data</key>
            <dict>
                <key>file-label</key>
                <string>Finder</string>
                <key>bundle-identifier</key>
                <string>com.apple.finder</string>
            </dict>
            <key>tile-type</key>
            <string>file-tile</string>
        </dict>
    </array>
</dict>
</plist>
EOS
}

# True when the recorded Dock plist keeps Finder and has no Task Viewer tile.
dock_plist_dropped_task_viewer() {
    python3 - "$1" <<'EOS'
import plistlib, sys
with open(sys.argv[1], "rb") as handle:
    data = plistlib.load(handle)
labels = [ (tile.get("tile-data") or {}).get("file-label") for tile in data.get("persistent-apps", []) ]
sys.exit(0 if "Finder" in labels and "Task Viewer" not in labels else 1)
EOS
}

# Count the Dock restarts recorded by the killall stub.
killall_count() {
    if [ -f "$1" ]; then
        wc -l <"$1" | tr -d ' '
    else
        printf '0\n'
    fi
}

# The Dock-removal case: install, uninstall, and assert the bundle and tile are
# gone and the Dock restarted once.
case_dock_removal() {
    local root="$TMP/dock" out status before after
    make_root "$root" Darwin
    make_defaults_stub "$root"
    make_killall_stub "$root"
    make_login_home_stubs "$root"
    write_dock_plist "$root/dock.plist"

    HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/install-desktop.sh" >/dev/null 2>&1
    before=$(killall_count "$root/killall.log")

    out=$(HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?
    after=$(killall_count "$root/killall.log")

    if [ "$status" -eq 0 ] &&
        [ ! -e "$root/home/Applications/Task Viewer.app" ] &&
        printf '%s\n' "$out" | grep -q "Removed: $root/home/Applications/Task Viewer.app" &&
        printf '%s\n' "$out" | grep -q 'The database and logs were kept' &&
        dock_plist_dropped_task_viewer "$root/dock.plist" &&
        [ "$after" -eq $((before + 1)) ]; then
        pass "uninstall removes the bundle and Dock tile and restarts the Dock once"
    else
        fail "uninstall removes the bundle and Dock tile and restarts the Dock once (status $status, killall $before->$after)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The idempotence case: a second uninstall reports nothing left to remove and
# restarts no Dock.
case_idempotent_second_run() {
    local root="$TMP/idempotent" out status before after
    make_root "$root" Darwin
    make_defaults_stub "$root"
    make_killall_stub "$root"
    make_login_home_stubs "$root"
    write_dock_plist "$root/dock.plist"

    HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/install-desktop.sh" >/dev/null 2>&1
    HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" >/dev/null 2>&1
    before=$(killall_count "$root/killall.log")

    out=$(HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?
    after=$(killall_count "$root/killall.log")

    if [ "$status" -eq 0 ] &&
        printf '%s\n' "$out" | grep -q 'Nothing left to remove' &&
        [ "$after" -eq "$before" ]; then
        pass "a second uninstall reports nothing left to remove and restarts no Dock"
    else
        fail "a second uninstall reports nothing left to remove and restarts no Dock (status $status, killall $before->$after)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# A redirected HOME must skip the live Dock and still exit 0.
case_redirected_home_skips_dock() {
    local root="$TMP/redirected" out status
    make_root "$root" Darwin
    make_defaults_stub "$root"
    make_killall_stub "$root"
    make_login_home_stubs "$root"
    write_dock_plist "$root/dock.plist"
    mkdir -p "$root/login-home"

    out=$(HOME="$root/home" FAKE_LOGIN_HOME="$root/login-home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?

    if [ "$status" -eq 0 ] &&
        printf '%s\n' "$out" | grep -q 'is not the login account' &&
        [ ! -e "$root/defaults.log" ] &&
        [ ! -e "$root/killall.log" ]; then
        pass "a redirected HOME skips the Dock removal and exits 0"
    else
        fail "a redirected HOME skips the Dock removal and exits 0 (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# With neither dockutil nor python3 the Dock step names the missing tool and the
# domain, still reports the bundle removal, and exits non-zero.
case_missing_tools_reports_and_fails() {
    local root="$TMP/no-tools" out status cmd bundle
    make_root "$root" Darwin
    make_login_home_stubs "$root"
    bundle="$root/home/Applications/Task Viewer.app"
    mkdir -p "$bundle/Contents"
    printf 'bundle\n' >"$bundle/Contents/marker"

    # Only the commands the uninstaller calls, so neither dockutil nor python3
    # can be found. bash itself runs by its absolute path.
    for cmd in dirname basename rm cat ps grep; do
        ln -s "$(command -v "$cmd")" "$root/bin/$cmd"
    done
    out=$(HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        PATH="$root/bin" \
        "$(command -v bash)" "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?

    if [ "$status" -ne 0 ] &&
        [ ! -e "$bundle" ] &&
        printf '%s\n' "$out" | grep -qF "Removed: $bundle" &&
        printf '%s\n' "$out" | grep -q 'python3' &&
        printf '%s\n' "$out" | grep -q 'com.apple.dock'; then
        pass "a missing python3 names the tool and the Dock domain, reports the bundle, and fails"
    else
        fail "a missing python3 names the tool and the Dock domain, reports the bundle, and fails (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# A live server named by the PID file gets a make stop hint and the run still
# exits 0.
case_running_server_hint() {
    local root="$TMP/running" out status pid state
    make_root "$root" Darwin
    make_defaults_stub "$root"
    make_killall_stub "$root"
    make_login_home_stubs "$root"
    write_dock_plist "$root/dock.plist"
    state="$root/home/Library/Logs/task-viewer"
    mkdir -p "$state" "$root/target/release"
    printf '#!/bin/bash\nexit 0\n' >"$root/target/release/task-viewer"
    chmod +x "$root/target/release/task-viewer"

    bash -c 'exec -a "$1" sleep 30' _ "$root/target/release/task-viewer" &
    pid=$!
    disown "$pid" 2>/dev/null || true
    printf '%s\n' "$pid" >"$state/task-viewer.pid"

    out=$(HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DEFAULTS_STATE="$root/dock.plist" DEFAULTS_LOG="$root/defaults.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:$(dirname "$(command -v python3)"):/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?
    kill "$pid" 2>/dev/null || true

    if [ "$status" -eq 0 ] &&
        printf '%s\n' "$out" | grep -q "still running (PID: $pid)" &&
        printf '%s\n' "$out" | grep -q 'make stop'; then
        pass "a live server gets a make stop hint and the run exits 0"
    else
        fail "a live server gets a make stop hint and the run exits 0 (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The Linux case seeds the icon source, installs, then removes the entry and icon.
case_linux_removal() {
    local root="$TMP/linux" out status desktop icon
    make_root "$root" Linux
    desktop="$root/home/.local/share/applications/task-viewer.desktop"
    icon="$root/home/.local/share/icons/hicolor/256x256/apps/task-viewer.png"

    HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
        bash "$root/scripts/install-desktop.sh" >/dev/null 2>&1
    if [ ! -f "$desktop" ] || [ ! -f "$icon" ]; then
        fail "the Linux case installs the desktop entry and icon before uninstalling"
        return
    fi

    out=$(HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?
    second_status=0
    if [ "$status" -eq 0 ]; then
        HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
            bash "$root/scripts/uninstall-desktop.sh" >/dev/null 2>&1 ||
            second_status=$?
    fi

    if [ "$status" -eq 0 ] && [ "$second_status" -eq 0 ] &&
        [ ! -e "$desktop" ] && [ ! -e "$icon" ] &&
        printf '%s\n' "$out" | grep -qF "Removed: $desktop" &&
        printf '%s\n' "$out" | grep -qF "Removed: $icon" &&
        printf '%s\n' "$out" | grep -q 'The database and logs were kept'; then
        pass "uninstall removes the Linux desktop entry and icon"
    else
        fail "uninstall removes the Linux desktop entry and icon (status $status, second run $second_status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The dockutil branch: the tile is counted from the tab-separated list form and
# the Dock restarts once a tile is actually removed.
case_dockutil_removal() {
    local root="$TMP/dockutil" out status
    make_root "$root" Darwin
    make_killall_stub "$root"
    make_login_home_stubs "$root"
    make_dockutil_stub "$root"
    printf 'tile\n' >"$root/dockutil.state"

    out=$(HOME="$root/home" FAKE_LOGIN_HOME="$root/home" \
        DOCKUTIL_STATE="$root/dockutil.state" DOCKUTIL_LOG="$root/dockutil.log" \
        KILLALL_LOG="$root/killall.log" \
        PATH="$root/bin:/usr/bin:/bin" \
        bash "$root/scripts/uninstall-desktop.sh" 2>&1)
    status=$?

    if [ "$status" -eq 0 ] &&
        [ ! -f "$root/dockutil.state" ] &&
        grep -q -- '--remove Task Viewer' "$root/dockutil.log" &&
        [ "$(killall_count "$root/killall.log")" -eq 1 ] &&
        grep -qx 'Dock' "$root/killall.log"; then
        pass "the dockutil branch removes the tile and restarts the Dock once"
    else
        fail "the dockutil branch removes the tile and restarts the Dock once (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
        for log in dockutil killall; do
            [ -f "$root/$log.log" ] && sed 's/^/    /' "$root/$log.log"
        done
    fi
}

case_dock_removal
case_dockutil_removal
case_idempotent_second_run
case_redirected_home_skips_dock
case_missing_tools_reports_and_fails
case_running_server_hint
case_linux_removal

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES failure(s)"
    exit 1
fi
