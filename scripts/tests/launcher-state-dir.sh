#!/bin/bash
# Tests for the launcher's state-directory failure handling.
#
# A state directory that cannot be created, is a regular file, or turns
# unwritable must fail the launcher fast with a message naming it, instead of
# spinning in the lock loop. Every case runs a copy of the launcher inside a
# throwaway root and home, so no real state directory, log, PID, or lock file is
# read or written.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LAUNCHER_SOURCE="$REPO/scripts/task-viewer.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/task-viewer-state-dir.XXXXXX")"
# TMPDIR often ends with a slash; the launcher prints normalized absolute paths,
# so normalize this root too before comparing them.
TMP="$(cd "$TMP" && pwd -P)"
FAILURES=0

# A case may leave a directory read-only; restore write access before deleting.
trap 'chmod -R u+rwX "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

pass() {
    echo "ok - $1"
}

fail() {
    echo "not ok - $1"
    FAILURES=$((FAILURES + 1))
}

# A checkout-shaped root: scripts/, a prebuilt release binary, a private home,
# and a private state parent. Nothing here resolves to a live directory.
make_root() {
    local root="$1"
    mkdir -p "$root/scripts" "$root/target/release" "$root/home" "$root/state"
    cp "$LAUNCHER_SOURCE" "$root/scripts/"
    printf '#!/bin/bash\nexit 0\n' >"$root/target/release/task-viewer"
    chmod +x "$root/target/release/task-viewer"
}

# The launcher's state directory for a given home, mirroring its own rule.
state_dir_for() {
    case "$(uname -s)" in
        Darwin) printf '%s\n' "$1/Library/Logs/task-viewer" ;;
        *)      printf '%s\n' "${XDG_STATE_HOME:-$1/.local/state}/task-viewer" ;;
    esac
}

# Run the staged launcher's status command, timing it. Sets `out`, `status`, and
# `elapsed` (whole seconds) for the caller.
run_status() {
    local root="$1" started
    started=$(date +%s)
    out=$(HOME="$root/home" XDG_STATE_HOME="$root/state" \
        bash "$root/scripts/task-viewer.sh" status 2>&1)
    status=$?
    elapsed=$(($(date +%s) - started))
}

# A home the launcher cannot write to. The state directory lives inside it on
# both platforms, so the failed `mkdir` is the first thing that happens.
case_unwritable_home_fails_fast() {
    local root="$TMP/readonly-home" state
    make_root "$root"
    chmod 500 "$root/home"
    state="$root/home/Library/Logs/task-viewer"
    if [ "$(uname -s)" != "Darwin" ]; then
        state="$root/home/.local/state/task-viewer"
    fi
    # No XDG_STATE_HOME override: the state directory has to sit under this home.
    out=$(env -u XDG_STATE_HOME HOME="$root/home" \
        bash "$root/scripts/task-viewer.sh" status 2>&1)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -qF "$state"; then
        pass "an unwritable home fails fast and names the state directory"
    else
        fail "an unwritable home fails fast and names the state directory (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# A state directory path that is a regular file: `mkdir -p` cannot create it.
case_state_dir_is_a_file_fails_fast() {
    local root="$TMP/state-is-a-file" state
    make_root "$root"
    state="$(state_dir_for "$root/home")"
    mkdir -p "$(dirname "$state")"
    : >"$state"
    run_status "$root"
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -qF "$state"; then
        pass "a state directory that is a file fails fast and names it"
    else
        fail "a state directory that is a file fails fast and names it (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The state directory becomes unwritable after start-up, so the lock loop is
# reached and has to stop instead of spinning. The driver is bounded by the
# shell test's own window as well.
case_unwritable_lock_stops_the_loop() {
    local root="$TMP/unwritable-lock" started elapsed state
    make_root "$root"
    state="$(state_dir_for "$root/home")"
    started=$(date +%s)
    out=$(HOME="$root/home" XDG_STATE_HOME="$root/state" bash -c '
        source "$1"
        chmod 500 "$STATE_DIR"
        acquire_lock
        printf "acquired\n"
    ' _ "$root/scripts/task-viewer.sh" 2>&1)
    status=$?
    elapsed=$(($(date +%s) - started))
    if [ "$status" -ne 0 ] &&
        printf '%s\n' "$out" | grep -qF "$state" &&
        ! printf '%s\n' "$out" | grep -q '^acquired$' &&
        [ "$elapsed" -le 10 ]; then
        pass "an unwritable lock stops the loop with a message naming the state directory"
    else
        fail "an unwritable lock stops the loop with a message naming the state directory (status $status, ${elapsed}s)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# A writable state directory keeps the old behaviour: the directory is created
# private, `status` reads it, and the lock is released on exit.
case_writable_state_dir_works() {
    local root="$TMP/writable" state mode
    make_root "$root"
    state="$(state_dir_for "$root/home")"
    run_status "$root"
    if [ "$status" -eq 1 ] && printf '%s\n' "$out" | grep -q 'task-viewer is not running'; then
        if [ -d "$state" ] && [ ! -e "$state/task-viewer.lock" ]; then
            # macOS `ls` appends an extended-attribute flag, so read the mode
            # bits from stat, which both platforms answer.
            mode=$(stat -f '%Lp' "$state" 2>/dev/null || stat -c '%a' "$state" 2>/dev/null)
            if [ "$mode" = "700" ]; then
                pass "a writable state directory still serves status and releases the lock"
            else
                fail "a writable state directory is created private (mode $mode)"
            fi
        else
            fail "a writable state directory is created and the lock is released"
        fi
    else
        fail "a writable state directory still serves status (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_unwritable_home_fails_fast
case_state_dir_is_a_file_fails_fast
case_unwritable_lock_stops_the_loop
case_writable_state_dir_works

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES launcher state-directory test(s) failed"
    exit 1
fi
echo "all launcher state-directory tests passed"
