#!/bin/bash
# Tests for the launcher's build and install honesty.
#
# Every case runs a copy of the launcher (or the installer) inside a throwaway
# root and home, so no real build, server, or desktop file is touched. The
# launcher is sourced where a case needs to replace ps, lsof, kill, or the
# detach step, which is the same hook its own dispatch guard offers.
#
# Every launcher case pins XDG_STATE_HOME under its own root, and each case
# that keeps /usr/bin on PATH also keeps failing notification stubs first on
# it, so neither a notification nor a state write reaches the person's live
# desktop or launcher directory. The two detach cases instead drop the desktop
# tools entirely by running with only their own minimal bin directory on PATH.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAUNCHER_SOURCE="$REPO/scripts/task-viewer.sh"
INSTALLER_SOURCE="$REPO/scripts/install-desktop.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/task-viewer-launcher.XXXXXX")"
# TMPDIR often ends with a slash; the launcher prints normalized absolute paths,
# so normalize this root too before comparing them.
TMP="$(cd "$TMP" && pwd -P)"
FAILURES=0

# A decoy XDG_STATE_HOME stands in for the live one a person's environment may
# export. Every launcher case overrides it with a path under its own root, so a
# case that forgets writes here instead and the sentinel case fails on Linux,
# while the throwaway HOME keeps macOS state under the root either way.
SENTINEL_STATE="$TMP/sentinel-state"
mkdir -p "$SENTINEL_STATE"
export XDG_STATE_HOME="$SENTINEL_STATE"

trap 'rm -rf "$TMP"' EXIT

pass() {
    echo "ok - $1"
}

fail() {
    echo "not ok - $1"
    FAILURES=$((FAILURES + 1))
}

# A checkout-shaped root: scripts/, a prebuilt release binary, a private home,
# an empty directory the launcher may use as its PATH fallback, and failing
# notification stubs that stand in for the desktop tools.
make_root() {
    local root="$1" cmd
    mkdir -p "$root/scripts" "$root/target/release" "$root/home" "$root/empty" "$root/stubs"
    cp "$LAUNCHER_SOURCE" "$INSTALLER_SOURCE" "$root/scripts/"
    printf '#!/bin/bash\nexit 0\n' >"$root/target/release/task-viewer"
    chmod +x "$root/target/release/task-viewer"
    # Each stub records the call and exits non-zero, so a notification fails
    # here instead of reaching the person's desktop on either platform.
    for cmd in osascript notify-send; do
        cat >"$root/stubs/$cmd" <<'EOS'
#!/bin/bash
printf '%s\n' "${0##*/}" >>"$NOTIFY_CALL_LOG"
exit 1
EOS
        chmod +x "$root/stubs/$cmd"
    done
}

# The launcher's state directory for a given home, mirroring its own rule. `$2`
# is the XDG_STATE_HOME the case runs the launcher with, because the launcher
# reads that value rather than this script's own environment.
state_dir_for() {
    local home="$1" xdg="${2:-}"
    case "$(uname -s)" in
        Darwin) printf '%s\n' "$home/Library/Logs/task-viewer" ;;
        *)      printf '%s\n' "${xdg:-$home/.local/state}/task-viewer" ;;
    esac
}

# Run the staged launcher's status command with its state under the throwaway
# root and the failing notification stubs first on PATH.
run_status() {
    local root="$1"
    HOME="$root/home" XDG_STATE_HOME="$root/home/state" \
        NOTIFY_CALL_LOG="$root/notify.log" \
        TASK_VIEWER_PATH_FALLBACK="$root/empty" \
        PATH="$root/stubs:/usr/bin:/bin" \
        bash "$root/scripts/task-viewer.sh" status
}

case_launcher_build_is_locked() {
    if grep -q 'cargo build --release --locked -p task-viewer' "$LAUNCHER_SOURCE"; then
        pass "launcher build passes --locked"
    else
        fail "launcher build passes --locked"
    fi
}

case_make_build_is_locked() {
    local out
    out=$(cd "$REPO" && make -n build 2>&1)
    if printf '%s\n' "$out" | grep -q 'cargo build --release --locked -p task-viewer'; then
        pass "make build passes --locked"
    else
        fail "make build passes --locked"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_missing_cargo_fails() {
    local root="$TMP/missing-cargo" out status tool
    make_root "$root"
    out=$(HOME="$root/home" XDG_STATE_HOME="$root/home/state" \
        NOTIFY_CALL_LOG="$root/notify.log" \
        TASK_VIEWER_PATH_FALLBACK="$root/empty" \
        PATH="$root/stubs:$root/empty:/usr/bin:/bin" \
        bash "$root/scripts/task-viewer.sh" start 2>&1)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -q 'make build'; then
        pass "start fails and names make build when cargo is missing"
    else
        fail "start fails and names make build when cargo is missing (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
    # The failure notifies, and the stub has to take that call: a log naming the
    # platform's tool proves the notification failed here, not on the desktop.
    case "$(uname -s)" in
        Darwin) tool=osascript ;;
        *)      tool=notify-send ;;
    esac
    if grep -qx "$tool" "$root/notify.log" 2>/dev/null; then
        pass "the build-failure notification stops at the failing $tool stub"
    else
        fail "the build-failure notification stops at the failing $tool stub"
    fi
}

case_rustup_cargo_wins() {
    local root="$TMP/path-order" resolved
    make_root "$root"
    mkdir -p "$root/home/.cargo/bin" "$root/homebrew"
    printf '#!/bin/bash\nexit 0\n' >"$root/home/.cargo/bin/cargo"
    printf '#!/bin/bash\nexit 0\n' >"$root/homebrew/cargo"
    chmod +x "$root/home/.cargo/bin/cargo" "$root/homebrew/cargo"
    resolved=$(HOME="$root/home" XDG_STATE_HOME="$root/home/state" \
        NOTIFY_CALL_LOG="$root/notify.log" \
        TASK_VIEWER_PATH_FALLBACK="$root/homebrew" \
        PATH="$root/stubs:$root/homebrew:/usr/bin:/bin" \
        bash -c 'source "$1"; command -v cargo' _ "$root/scripts/task-viewer.sh" 2>/dev/null)
    if [ "$resolved" = "$root/home/.cargo/bin/cargo" ]; then
        pass "a Homebrew-first PATH still resolves rustup cargo"
    else
        fail "a Homebrew-first PATH still resolves rustup cargo (got '$resolved')"
    fi
}

case_toggle_builds_once() {
    local root="$TMP/toggle" runner out builds
    make_root "$root"
    mkdir -p "$root/home/.cargo/bin"
    cat >"$root/home/.cargo/bin/cargo" <<'EOS'
#!/bin/bash
printf '%s\n' "$*" >>"$BUILD_COUNT_LOG"
EOS
    chmod +x "$root/home/.cargo/bin/cargo"
    cat >"$root/run.sh" <<'EOS'
#!/bin/bash
set -u
source "$1/scripts/task-viewer.sh"
# A running server whose binary is newer than the stamp.
echo 4242 >"$PID_FILE"
touch -t 200001010000 "$STAMP_FILE"
# ps names the seeded PID as this checkout's binary; no lsof or ss is found.
ps() {
    local pid=""
    while [ $# -gt 0 ]; do
        if [ "$1" = "-p" ]; then
            pid="$2"
            shift 2
        else
            shift
        fi
    done
    if [ "$pid" = "4242" ]; then
        printf '%s\n' "$BINARY"
    fi
}
# Stopping the seeded server ends its ownership at once.
kill() {
    rm -f "$PID_FILE"
    return 0
}
# The start itself is not what this case measures; fail it fast.
start_detached() {
    return 1
}
do_toggle
EOS
    chmod +x "$root/run.sh"
    out=$(BUILD_COUNT_LOG="$root/build.log" HOME="$root/home" \
        XDG_STATE_HOME="$root/home/state" \
        NOTIFY_CALL_LOG="$root/notify.log" \
        TASK_VIEWER_PATH_FALLBACK="$root/empty" \
        PATH="$root/stubs:$root/home/.cargo/bin:/usr/bin:/bin" \
        bash "$root/run.sh" "$root" 2>&1)
    builds=0
    if [ -f "$root/build.log" ]; then
        builds=$(wc -l <"$root/build.log" | tr -d ' ')
    fi
    if [ "$builds" = "1" ] && printf '%s\n' "$out" | grep -q 'restarting on it'; then
        pass "one toggle builds once and restarts on the new binary"
    else
        fail "one toggle builds once and restarts on the new binary (builds: $builds)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_icon_cache_failure_prints_no_checkmark() {
    local root="$TMP/installer" out status
    make_root "$root"
    mkdir -p "$root/bin"
    printf '#!/bin/bash\nprintf "Linux\\n"\n' >"$root/bin/uname"
    printf '#!/bin/bash\nexit 1\n' >"$root/bin/gtk-update-icon-cache"
    chmod +x "$root/bin/uname" "$root/bin/gtk-update-icon-cache"
    out=$(HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
        bash "$root/scripts/install-desktop.sh" 2>&1)
    status=$?
    if [ "$status" -eq 0 ] &&
        printf '%s\n' "$out" | grep -q 'Desktop entry created' &&
        ! printf '%s\n' "$out" | grep -q 'Icon cache updated'; then
        pass "a failed icon-cache refresh prints no success checkmark"
    else
        fail "a failed icon-cache refresh prints no success checkmark (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_desktop_database_failure_prints_no_checkmark() {
    local root="$TMP/installer-db" out status ok_out
    make_root "$root"
    mkdir -p "$root/bin"
    printf '#!/bin/bash\nprintf "Linux\\n"\n' >"$root/bin/uname"
    printf '#!/bin/bash\nexit 1\n' >"$root/bin/update-desktop-database"
    chmod +x "$root/bin/uname" "$root/bin/update-desktop-database"
    out=$(HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
        bash "$root/scripts/install-desktop.sh" 2>&1)
    status=$?
    printf '#!/bin/bash\nexit 0\n' >"$root/bin/update-desktop-database"
    chmod +x "$root/bin/update-desktop-database"
    ok_out=$(HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
        bash "$root/scripts/install-desktop.sh" 2>&1)
    if [ "$status" -eq 0 ] &&
        printf '%s\n' "$out" | grep -q 'Desktop entry created' &&
        printf '%s\n' "$out" | grep -q 'Desktop database refresh failed' &&
        ! printf '%s\n' "$out" | grep -q 'Desktop database updated' &&
        printf '%s\n' "$ok_out" | grep -q 'Desktop database updated'; then
        pass "a failed desktop-database refresh prints no success checkmark"
    else
        fail "a failed desktop-database refresh prints no success checkmark (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The Dock pin needs python3. A host without it must hear why the install
# stopped and where the bundle it just built lives, not bash's raw error.
case_missing_python3_fails_install_with_explanation() {
    local root="$TMP/installer-python" out status bundle cmd bash_bin
    make_root "$root"
    mkdir -p "$root/bin"
    printf '#!/bin/bash\nprintf "Darwin\\n"\n' >"$root/bin/uname"
    chmod +x "$root/bin/uname"
    # Only the commands the Darwin branch reaches, so neither python3 nor
    # dockutil can be found. bash itself is called by its absolute path.
    for cmd in dirname chmod mkdir rm cat; do
        ln -s "$(command -v "$cmd")" "$root/bin/$cmd"
    done
    bash_bin=$(command -v bash)
    bundle="$root/home/Applications/Task Viewer.app"
    out=$(HOME="$root/home" PATH="$root/bin" \
        "$bash_bin" "$root/scripts/install-desktop.sh" 2>&1)
    status=$?
    if [ "$status" -ne 0 ] &&
        printf '%s\n' "$out" | grep -q 'python3' &&
        printf '%s\n' "$out" | grep -qF "$bundle" &&
        ! printf '%s\n' "$out" | grep -q 'command not found'; then
        pass "a missing python3 fails the install and names python3 and the bundle"
    else
        fail "a missing python3 fails the install and names python3 and the bundle (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_detach_refuses_without_python3_or_setsid() {
    local root="$TMP/detach-refuse" out pidfile
    make_root "$root"
    mkdir -p "$root/minbin"
    pidfile="$(state_dir_for "$root/home" "$root/home/state")/task-viewer.pid"
    out=$(HOME="$root/home" XDG_STATE_HOME="$root/home/state" bash -c '
        source "$1"
        PATH="$2/minbin"
        start_detached
        echo "status=$?"
    ' _ "$root/scripts/task-viewer.sh" "$root" 2>&1)
    if printf '%s\n' "$out" | grep -q 'status=1' &&
        printf '%s\n' "$out" | grep -q 'Cannot detach' &&
        printf '%s\n' "$out" | grep -q 'python3' &&
        [ ! -f "$pidfile" ]; then
        pass "detach refuses with an explanation when python3 and setsid are missing"
    else
        fail "detach refuses with an explanation when python3 and setsid are missing"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_detach_uses_setsid() {
    local root="$TMP/detach-setsid" out pidfile
    make_root "$root"
    mkdir -p "$root/minbin"
    cat >"$root/minbin/setsid" <<'EOS'
#!/bin/bash
printf '%s\n' "$*" >"$SETSID_LOG"
exit 0
EOS
    chmod +x "$root/minbin/setsid"
    pidfile="$(state_dir_for "$root/home" "$root/home/state")/task-viewer.pid"
    out=$(HOME="$root/home" XDG_STATE_HOME="$root/home/state" SETSID_LOG="$root/setsid.log" bash -c '
        source "$1"
        PATH="$2/minbin"
        start_detached
        echo "status=$?"
    ' _ "$root/scripts/task-viewer.sh" "$root" 2>&1)
    # The background job creates the log before it writes it, so wait for the
    # content rather than for the file.
    local matched=0
    for _ in {1..40}; do
        if grep -q "nohup $root/target/release/task-viewer" "$root/setsid.log" 2>/dev/null; then
            matched=1
            break
        fi
        sleep 0.05
    done
    if printf '%s\n' "$out" | grep -q 'status=0' &&
        [ -s "$pidfile" ] &&
        [ "$matched" -eq 1 ]; then
        pass "detach uses setsid when python3 is missing"
    else
        fail "detach uses setsid when python3 is missing"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The suite's own environment may export a live XDG_STATE_HOME. The sentinel
# stands in for it: a launcher case that forgets to pin its state under its root
# writes into the sentinel instead, and this case catches the leak. macOS derives
# the state directory from HOME alone, so the sentinel check carries its weight
# on Linux while the state-under-root check holds on both.
case_launcher_state_stays_under_root() {
    local root="$TMP/state-sentinel" state
    make_root "$root"
    state="$(state_dir_for "$root/home" "$root/home/state")"
    run_status "$root" >/dev/null 2>&1
    if [ -z "$(find "$SENTINEL_STATE" -mindepth 1 -print -quit)" ] && [ -d "$state" ]; then
        pass "a launcher case leaves the sentinel XDG_STATE_HOME untouched"
    else
        fail "a launcher case leaves the sentinel XDG_STATE_HOME untouched"
        find "$SENTINEL_STATE" -mindepth 1 | sed 's/^/    leaked: /'
    fi
}

# The PID and stamp files the launcher derives resolve under the case's own root,
# so a stop can never reach the person's live PID file.
case_state_files_resolve_under_root() {
    local root="$TMP/state-paths" state out
    make_root "$root"
    state="$(state_dir_for "$root/home" "$root/home/state")"
    out=$(HOME="$root/home" XDG_STATE_HOME="$root/home/state" \
        NOTIFY_CALL_LOG="$root/notify.log" \
        TASK_VIEWER_PATH_FALLBACK="$root/empty" \
        PATH="$root/stubs:/usr/bin:/bin" \
        bash -c 'source "$1"; printf "%s\n%s\n" "$PID_FILE" "$STAMP_FILE"' \
        _ "$root/scripts/task-viewer.sh" 2>&1)
    if printf '%s\n' "$out" | grep -qxF "$state/task-viewer.pid" &&
        printf '%s\n' "$out" | grep -qxF "$state/task-viewer.binary"; then
        pass "the launcher's PID and stamp files resolve under the case root"
    else
        fail "the launcher's PID and stamp files resolve under the case root"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# status never notifies, so neither stub may be called. A change that adds a
# notification to the status path trips this instead of popping on the desktop.
case_status_raises_no_notification() {
    local root="$TMP/no-notification" out
    make_root "$root"
    out=$(run_status "$root" 2>&1)
    if [ "$(PATH="$root/stubs:/usr/bin:/bin" command -v osascript)" = "$root/stubs/osascript" ] &&
        [ "$(PATH="$root/stubs:/usr/bin:/bin" command -v notify-send)" = "$root/stubs/notify-send" ]; then
        pass "the failing notification stubs are first on the case PATH"
    else
        fail "the failing notification stubs are first on the case PATH"
    fi
    if printf '%s\n' "$out" | grep -q 'task-viewer is not running' &&
        [ ! -s "$root/notify.log" ]; then
        pass "a status run invokes neither notification stub"
    else
        fail "a status run invokes neither notification stub"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_toolchain_is_pinned() {
    local channel rust_version
    channel=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$REPO/rust-toolchain.toml")
    rust_version=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' "$REPO/task-viewer/Cargo.toml")
    if [[ "$channel" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] && [ "$channel" = "$rust_version" ]; then
        pass "rust-toolchain.toml pins $channel and Cargo.toml matches"
    else
        fail "rust-toolchain.toml pins a version and Cargo.toml matches (channel '$channel', rust-version '$rust_version')"
    fi
}

case_launcher_build_is_locked
case_make_build_is_locked
case_missing_cargo_fails
case_rustup_cargo_wins
case_toggle_builds_once
case_icon_cache_failure_prints_no_checkmark
case_desktop_database_failure_prints_no_checkmark
case_missing_python3_fails_install_with_explanation
case_detach_refuses_without_python3_or_setsid
case_detach_uses_setsid
case_launcher_state_stays_under_root
case_state_files_resolve_under_root
case_status_raises_no_notification
case_toolchain_is_pinned

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES launcher test(s) failed"
    exit 1
fi
echo "all launcher tests passed"
