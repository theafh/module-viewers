#!/bin/bash
# task-viewer launcher
# Usage: task-viewer.sh [start|stop|status|restart|toggle]
# Default (Dock): toggle — start if stopped, stop if running

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Resolve the checkout once, so a checkout reached through a symlink compares
# equal to the path `ps` prints for the binary it starts.
PROJECT_DIR="$(cd "$(dirname "$SCRIPT_DIR")" && pwd -P)"
BINARY="$PROJECT_DIR/target/release/task-viewer"
PORT=7880
# PORT alone picks the bind port. The server reads TASK_VIEWER_PORT, so drop any
# inherited value and hand it this launcher's own port; an exported
# TASK_VIEWER_PORT from a shell or a test then changes nothing.
unset TASK_VIEWER_PORT
export TASK_VIEWER_PORT="$PORT"

# Runtime state lives outside the checkout: the log records repository paths.
# The variable named here is the one the chosen branch expanded, so a relative
# path is reported against the value that actually made it relative: the Darwin
# branch never reads XDG_STATE_HOME, and an empty XDG_STATE_HOME falls back to
# HOME.
case "$(uname -s)" in
    Darwin)
        STATE_DIR="$HOME/Library/Logs/task-viewer"
        STATE_SOURCE=HOME
        ;;
    *)
        if [ -n "${XDG_STATE_HOME:-}" ]; then
            STATE_DIR="$XDG_STATE_HOME/task-viewer"
            STATE_SOURCE=XDG_STATE_HOME
        else
            STATE_DIR="$HOME/.local/state/task-viewer"
            STATE_SOURCE=HOME
        fi
        ;;
esac
# The state directory is resolved before the launcher changes into the checkout,
# and every path built from it is used after that. A relative value would create
# the directory in the caller's working directory and then look for the lock
# somewhere else, so refuse it before anything is created. The state path must
# be absolute; this names the cause instead of falling back silently.
case "$STATE_DIR" in
    /*) ;;
    *)
        echo "The task-viewer state directory must be an absolute path, but $STATE_SOURCE makes it relative: $STATE_DIR" >&2
        echo "Set $STATE_SOURCE to an absolute path, then try again." >&2
        exit 1
        ;;
esac
PID_FILE="$STATE_DIR/task-viewer.pid"
LOG_FILE="$STATE_DIR/task-viewer.log"
BUILD_LOG="$STATE_DIR/build.log"
STAMP_FILE="$STATE_DIR/task-viewer.binary"
LOCK_DIR="$STATE_DIR/task-viewer.lock"

# Files this launcher creates are private to the person who runs it.
umask 077
# A state directory that cannot be created or made private has to fail here:
# the lock loop below can neither take the lock nor reclaim a stale one in it,
# so it would otherwise spin with no output and no exit.
if ! mkdir -p "$STATE_DIR"; then
    echo "Cannot create the task-viewer state directory: $STATE_DIR" >&2
    echo "Check that its parent is writable and that the path is a directory, then try again." >&2
    exit 1
fi
if ! chmod 700 "$STATE_DIR"; then
    echo "Cannot set permissions on the task-viewer state directory: $STATE_DIR" >&2
    exit 1
fi

# Earlier launchers kept both files at the repository root; adopt the PID of a
# server started that way and drop the old log.
if [ -f "$PROJECT_DIR/.task-viewer.pid" ]; then
    if [ ! -f "$PID_FILE" ]; then
        mv "$PROJECT_DIR/.task-viewer.pid" "$PID_FILE"
    else
        rm -f "$PROJECT_DIR/.task-viewer.pid"
    fi
fi
rm -f "$PROJECT_DIR/.task-viewer.log"
# umask only covers new files; repair modes left by older launchers.
chmod 600 "$PID_FILE" "$LOG_FILE" "$BUILD_LOG" "$STAMP_FILE" 2>/dev/null

# One command at a time. The lock is a directory because `mkdir` is atomic on
# macOS and Linux, and `flock` is absent on macOS. The PID recorded inside
# names the holder, and the command line beside it names the launcher that took
# it, so a holder that died is reclaimed instead of wedging the launcher forever.
LOCK_HELD=0
# The name this launcher runs under. A holder records its command line beside the
# lock PID, and a waiter compares that line, and the live process's own line,
# against this launcher's resolved path, so a live PID the system reused is
# reclaimed instead of waited for forever.
LAUNCHER_NAME=$(basename "${BASH_SOURCE[0]}")

# Canonicalise a path's directory and keep its final component, so a relative
# symlink target carrying `..` collapses here instead of surviving into a
# comparison.
canonical_path() {
    local path="$1" dir
    [ -n "$path" ] || return 1
    # `--` before the operand: a token that looks like an option (`-x`,
    # `--noprofile`) is a filename here, and dirname would reject it as a flag.
    dir=$(cd "$(dirname -- "$path")" 2>/dev/null && pwd -P) || return 1
    printf '%s\n' "$dir/$(basename -- "$path")"
}

# Resolve a token to the file it names, following the final component through
# `readlink` while it stays a symlink and joining a relative target to the
# directory that holds it. `readlink` rather than `realpath`, which the installed
# app's macOS 11 floor may lack. Prints nothing when the walk fails, and a walk
# longer than `RESOLVE_HOPS` is a failure too, so a symlink cycle ends as one
# instead of spinning the waiter forever.
RESOLVE_HOPS=40
resolve_path() {
    local path target hops=0
    path=$(canonical_path "$1") || return 1
    while [ -L "$path" ]; do
        if [ "$hops" -ge "$RESOLVE_HOPS" ]; then
            return 1
        fi
        hops=$((hops + 1))
        target=$(readlink "$path" 2>/dev/null) || return 1
        [ -n "$target" ] || return 1
        case "$target" in
            /*) path="$target" ;;
            *)  path="$(dirname "$path")/$target" ;;
        esac
        path=$(canonical_path "$path") || return 1
    done
    printf '%s\n' "$path"
}

# This launcher's own resolved path. `BASH_SOURCE[0]` may be a bare name when the
# launcher is reached through `PATH`, so resolve it through `command -v` first; an
# invoked name carrying a `/` already names a file.
launcher_self_path() {
    local self="${BASH_SOURCE[0]}"
    case "$self" in
        */*) ;;
        *) self=$(command -v "$self" 2>/dev/null) || self="${BASH_SOURCE[0]}" ;;
    esac
    resolve_path "$self"
}
LAUNCHER_SELF=$(launcher_self_path)

# True when a command line names this launcher. Every whitespace-separated token
# is resolved and compared with this launcher's own resolved path, because a real
# holder's line can carry the launcher behind an interpreter
# (`bash --noprofile --norc <launcher> status`). A token the walk cannot resolve
# to an existing file, and a bare name that carries no `/` at all, falls back to
# the basename comparison, so a holder recorded as `bash ./task-viewer.sh` from
# its own directory, or invoked by its bare name, is still waited for; a token
# that does resolve to another existing file is decided by the walk, so an alias
# pointing at a different launcher is not mistaken for this one.
names_launcher() {
    local line="$1" token resolved
    [ -n "$line" ] || return 1
    # Word splitting is the point: each token is compared on its own.
    for token in $line; do
        if resolved=$(resolve_path "$token") && [ -e "$resolved" ]; then
            [ "$resolved" = "$LAUNCHER_SELF" ] && return 0
            # A path that exists and is not this launcher is another program's.
            case "$token" in
                */*) continue ;;
            esac
        fi
        [ "$(basename -- "$token")" = "$LAUNCHER_NAME" ] && return 0
    done
    return 1
}

release_lock() {
    [ "$LOCK_HELD" = 1 ] || return 0
    LOCK_HELD=0
    # Remove only the directory that still records this shell: a reclaimed lock
    # can have been renamed away and replaced by another holder's.
    if [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" = "$$" ]; then
        rm -rf "$LOCK_DIR"
    fi
}

# Enter the critical section only while `$LOCK_DIR/pid` still names this shell. A
# waiter that read a stale PID before this shell recorded its own can rename the
# directory aside in the gap between `mkdir` and the writes, so ownership is
# re-acquired here instead of running beside the holder that took the name.
require_lock_ownership() {
    while [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" != "$$" ]; do
        LOCK_HELD=0
        acquire_lock
    done
}

# How many consecutive passes may fail to take the lock and fail to reclaim it
# before the launcher gives up. A state directory that turns unwritable after
# start-up reaches this, and each pass sleeps LOCK_RETRY_SLEEP, so the bound is
# about two seconds instead of an endless silent loop.
LOCK_FAILURE_LIMIT=20
LOCK_RETRY_SLEEP=0.1

# Reclaim a stale lock by renaming it aside first: exactly one waiter wins the
# rename, and a check-then-delete would let two waiters both act on one stale
# observation and the loser delete the fresh lock the winner just created. The
# renamed directory is then checked against the dead PID that justified the
# reclaim, so a waiter slow enough to rename a fresh lock the winner already
# created puts that lock back instead of deleting it.
acquire_lock() {
    local holder holder_cmd self_cmd stale missing_retries=0 live_cmd
    local failed_passes=0
    while :; do
        if mkdir "$LOCK_DIR" 2>/dev/null; then
            LOCK_HELD=1
            printf '%s\n' "$$" >"$LOCK_DIR/pid"
            # Record the command line beside the PID, so a waiter can tell this
            # launcher's holder from a live PID the system reused. `ps` can fail on
            # a restricted system; the invoked path still names this launcher.
            self_cmd=$(ps -ww -p $$ -o args= 2>/dev/null)
            [ -n "$self_cmd" ] || self_cmd="$0"
            printf '%s\n' "$self_cmd" >"$LOCK_DIR/cmd"
            return 0
        fi
        holder=$(cat "$LOCK_DIR/pid" 2>/dev/null)
        holder_cmd=$(cat "$LOCK_DIR/cmd" 2>/dev/null)
        # A live PID is a holder only when both its recorded command line and the
        # live process's own command line name this launcher. The recorded line
        # survives from a dead holder, so the live line decides; a live read that
        # fails (empty) is not read as "not a holder", because reclaiming a real
        # holder would run two commands at once.
        if [ -n "$holder" ] && names_launcher "$holder_cmd" &&
            kill -0 "$holder" 2>/dev/null; then
            live_cmd=$(ps -ww -p "$holder" -o args= 2>/dev/null)
            if [ -z "$live_cmd" ] || names_launcher "$live_cmd"; then
                # A real holder is building or waiting; it is never aborted.
                failed_passes=0
                sleep "$LOCK_RETRY_SLEEP"
                continue
            fi
        fi
        # No recorded PID or command line is the short window between `mkdir` and
        # the writes, so give the holder a moment before treating it as stale. An
        # absent lock directory is not that window: the failed `mkdir` above could
        # not create it, so there is nothing to wait for.
        if [ -d "$LOCK_DIR" ] &&
            { [ -z "$holder" ] || [ -z "$holder_cmd" ]; }; then
            if [ ! -w "$STATE_DIR" ]; then
                # The wait is a grace only while the reclaim below can still run.
                # An unwritable state directory cannot rename the lock aside, so
                # every pass here takes nothing and reclaims nothing: count it
                # toward the failure bound, which stops the launcher in about two
                # seconds instead of after 50 ticks of grace on every cycle.
                failed_passes=$((failed_passes + 1))
                if [ "$failed_passes" -ge "$LOCK_FAILURE_LIMIT" ]; then
                    echo "Cannot take or reclaim the task-viewer lock in: $STATE_DIR" >&2
                    echo "Check that the state directory is writable, then try again." >&2
                    exit 1
                fi
                sleep "$LOCK_RETRY_SLEEP"
                continue
            fi
            if [ "$missing_retries" -lt 50 ]; then
                missing_retries=$((missing_retries + 1))
                sleep "$LOCK_RETRY_SLEEP"
                continue
            fi
        fi
        missing_retries=0
        stale="$LOCK_DIR.stale.$$.$RANDOM"
        if mv "$LOCK_DIR" "$stale" 2>/dev/null; then
            failed_passes=0
            if [ "$(cat "$stale/pid" 2>/dev/null)" = "$holder" ]; then
                rm -rf "$stale"
            elif [ ! -e "$LOCK_DIR" ]; then
                mv "$stale" "$LOCK_DIR" 2>/dev/null || rm -rf "$stale"
            else
                # A fresh holder took the name in the gap. Its lock stands, so the
                # renamed copy is discarded rather than nested inside it.
                rm -rf "$stale"
            fi
        else
            # Neither the lock nor its reclaim could be created: the state
            # directory is unwritable or full. Stop instead of spinning.
            failed_passes=$((failed_passes + 1))
            if [ "$failed_passes" -ge "$LOCK_FAILURE_LIMIT" ]; then
                echo "Cannot take or reclaim the task-viewer lock in: $STATE_DIR" >&2
                echo "Check that the state directory is writable, then try again." >&2
                exit 1
            fi
            sleep "$LOCK_RETRY_SLEEP"
        fi
    done
}

# Dock / GUI launches have a minimal PATH; pull in rustup cargo when needed.
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi
# rustup's cargo comes first so a Homebrew cargo ahead of it cannot select a
# different toolchain. A test replaces the fallback directories through
# TASK_VIEWER_PATH_FALLBACK to control which cargo the launcher resolves.
PATH_FALLBACK="${TASK_VIEWER_PATH_FALLBACK:-/usr/local/bin:/opt/homebrew/bin}"
export PATH="$HOME/.cargo/bin:$PATH_FALLBACK:$PATH"
# Always build/run from this repo's target/, not a redirected agent cache.
unset CARGO_TARGET_DIR
export CARGO_TARGET_DIR="$PROJECT_DIR/target"
# Cursor often exports GIT_DIR; harvest must not inherit it.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY
unset GIT_INDEX_FILE GIT_ALTERNATE_OBJECT_DIRECTORIES

cd "$PROJECT_DIR" || exit 1

notify() {
    local title="$1"
    local message="$2"
    case "$(uname -s)" in
        Darwin)
            osascript -e "display notification \"$message\" with title \"$title\"" >/dev/null 2>&1 || true
            ;;
        Linux)
            if command -v notify-send >/dev/null 2>&1; then
                notify-send "$title" "$message" >/dev/null 2>&1 || true
            fi
            ;;
    esac
}

# Collapse `.` and `..` components without touching the filesystem, so a path
# recorded for a checkout that has since moved still compares equal to the same
# path written plainly. An absolute path keeps its leading `/`; a relative one
# keeps a leading `..` that has nothing to cancel.
normalize_path() {
    local path="$1" part last out absolute=0
    local -a parts=()
    case "$path" in
        /*) absolute=1 ;;
    esac
    local IFS='/'
    # A component holding `*`, `?`, or `[` must survive verbatim, so globbing is
    # off while the loop word-splits the path.
    set -f
    for part in $path; do
        case "$part" in
            ''|.) ;;
            ..)
                last=$((${#parts[@]} - 1))
                if [ "$last" -ge 0 ] && [ "${parts[$last]}" != ".." ]; then
                    unset "parts[$last]"
                elif [ "$absolute" -eq 0 ]; then
                    parts+=("..")
                fi
                ;;
            *) parts+=("$part") ;;
        esac
    done
    set +f
    if [ "${#parts[@]}" -eq 0 ]; then
        if [ "$absolute" -eq 1 ]; then
            printf '/\n'
        else
            printf '.\n'
        fi
        return 0
    fi
    out=$(IFS=/; printf '%s' "${parts[*]}")
    if [ "$absolute" -eq 1 ]; then
        printf '/%s\n' "$out"
    else
        printf '%s\n' "$out"
    fi
}

# Print the binary path a live PID was started from when that path is a
# task-viewer release binary, and fail for any other process. `ps` prints the
# argv the process was started with, so two classes count:
#
# - this checkout's `$BINARY`, compared as the whole line so a checkout path
#   holding a space stays intact, with a trailing argument allowed;
# - another checkout's release binary, matched on the line's last
#   `/target/release/task-viewer` at an argument boundary. The text before it is
#   one span, spaces included, and is the started-from directory only when it
#   ends on a non-whitespace character, opens with a path-like token (starts
#   with `/` or `.`, or holds a `/`), and carries no later token that starts
#   with `/` or `-`. That keeps a checkout path holding a space intact and keeps
#   a line whose argument merely names such a binary, such as
#   `python3 /old/mv/target/release/task-viewer`, foreign. Tokens after the
#   binary are trailing arguments and are ignored. The directory no longer
#   exists after a move, so the path is recovered verbatim and lexically
#   normalised, and canonicalised only while its directory still exists.
#
# Failing both, fall back to the line's first token: an absolute path as
# written, a path holding a `/` resolved against this checkout (every
# launcher-started server inherits it as its working directory), or a bare name
# resolved on PATH. A token that resolves to nothing is not ours.
started_from_path() {
    local candidate="$1"
    [ -n "$candidate" ] || return 1
    local args token resolved dir path prefix rest first part
    # -ww keeps procps from truncating a long command line to the terminal
    # width, so Linux reads the same full path macOS prints.
    args=$(ps -ww -p "$candidate" -o args= 2>/dev/null) || return 1
    [ -n "$args" ] || return 1
    # Compare the whole line at an argument boundary before splitting it, so a
    # path whose own name holds a space still matches the binary it names.
    case "$args" in
        "$BINARY"|"$BINARY "*) printf '%s\n' "$BINARY"; return 0 ;;
    esac
    rest=${args##*"/target/release/task-viewer"}
    if [ "$rest" != "$args" ]; then
        case "$rest" in
            ''|[[:space:]]*)
                prefix=${args%"$rest"}
                prefix=${prefix%"/target/release/task-viewer"}
                # The prefix must end on a path character, not whitespace.
                case "$prefix" in
                    *[![:space:]]) ;;
                    *) prefix="" ;;
                esac
                if [ -n "$prefix" ]; then
                    first=${prefix%%[[:space:]]*}
                    case "$first" in
                        /*|.*|*/*) ;;
                        *) prefix="" ;;
                    esac
                fi
                if [ -n "$prefix" ]; then
                    # A component holding `*`, `?`, or `[` must survive verbatim,
                    # so globbing is off while the loop word-splits the prefix.
                    set -f
                    for part in ${prefix#"$first"}; do
                        case "$part" in
                            /*|-*) prefix="" ;;
                        esac
                    done
                    set +f
                fi
                if [ -n "$prefix" ]; then
                    path=$(normalize_path "$prefix/target/release/task-viewer")
                    if [ -d "$(dirname -- "$path")" ]; then
                        resolved=$(canonical_path "$path") || resolved=""
                        [ -n "$resolved" ] && path="$resolved"
                    fi
                    printf '%s\n' "$path"
                    return 0
                fi
                ;;
        esac
    fi
    token=${args%%[[:space:]]*}
    [ -n "$token" ] || return 1
    case "$token" in
        /*) resolved="$token" ;;
        */*)
            dir=$(cd "$PROJECT_DIR/$(dirname "$token")" 2>/dev/null && pwd -P) || return 1
            resolved="$dir/$(basename "$token")"
            ;;
        *) resolved=$(command -v "$token" 2>/dev/null) || return 1 ;;
    esac
    [ "$resolved" = "$BINARY" ] || return 1
    printf '%s\n' "$BINARY"
}

# True when the PID is a task-viewer release binary: this checkout's, or one
# started from any other checkout's path, a moved or renamed checkout included.
# A live PID alone is not enough: PIDs are reused, and another program may own
# the port. A listener whose command line is not a release binary stays foreign.
is_ours() {
    started_from_path "$1" >/dev/null 2>&1
}

# The PID listening on the port, from lsof or else ss (minimal Linux installs
# often lack lsof). Empty when neither tool is installed.
port_pid() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -t -i :"$PORT" -sTCP:LISTEN 2>/dev/null | head -1
    elif command -v ss >/dev/null 2>&1; then
        ss -Hltnp "sport = :$PORT" 2>/dev/null | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1
    fi
}

has_port_tool() {
    command -v lsof >/dev/null 2>&1 || command -v ss >/dev/null 2>&1
}

# True when something accepts connections on the port, whoever owns it.
port_answers() {
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null
}

is_running() {
    local candidate
    if [ -f "$PID_FILE" ]; then
        candidate=$(cat "$PID_FILE")
        if is_ours "$candidate"; then
            return 0
        fi
        rm -f "$PID_FILE"
    fi
    # Adopt a release binary started by hand so stop can reach it. Any other
    # listener on the port is left alone.
    candidate=$(port_pid)
    if is_ours "$candidate"; then
        echo "$candidate" >"$PID_FILE"
        return 0
    fi
    return 1
}

get_pid() {
    cat "$PID_FILE" 2>/dev/null
}

# True once our process is the one listening on the port. Without lsof or ss
# the listener has no name; do_start refuses a port that already answers, and
# a server that cannot bind exits, so our live process plus an answer counts.
is_listening() {
    is_running || return 1
    if has_port_tool; then
        [ "$(port_pid)" = "$(get_pid)" ]
    else
        port_answers
    fi
}

# True when the binary on disk is newer than the one the running server was
# started from, for example after a source change. A server started without the
# launcher has no stamp and counts as outdated.
is_outdated() {
    [ ! -f "$STAMP_FILE" ] || [ "$BINARY" -nt "$STAMP_FILE" ]
}

# Build the release binary from this checkout. cargo does nothing when the
# sources are unchanged, so every start runs the code that is checked out
# rather than whatever an earlier `make build` left behind.
build_binary() {
    if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo not found; cannot build task-viewer. Run 'make build'."
        notify "task-viewer" "Build failed: cargo not found. Run 'make build' in the project."
        return 1
    fi
    if [ ! -x "$BINARY" ]; then
        echo "Building task-viewer..."
        notify "task-viewer" "Building release binary (first run)…"
    fi
    if ! cargo build --release --locked -p task-viewer >"$BUILD_LOG" 2>&1; then
        echo "Build failed. Check $BUILD_LOG"
        notify "task-viewer" "Build failed. See $BUILD_LOG"
        return 1
    fi
    if [ ! -x "$BINARY" ]; then
        echo "Build finished but binary is missing at $BINARY"
        notify "task-viewer" "Build failed: binary missing"
        return 1
    fi
}

# Start the server on the binary already built. The build stays with the
# callers, so one Dock click builds once instead of once per call.
start_binary() {
    local started
    if is_running && is_outdated; then
        echo "A newer task-viewer binary exists; restarting on it."
        # Name the path a moved checkout's server runs from, so a restart does
        # not read as replacing an anonymous process.
        started=$(started_from_path "$(get_pid)" 2>/dev/null)
        if [ -n "$started" ] && [ "$started" != "$BINARY" ]; then
            echo "The running server was started from: $started"
        fi
        # A stop that cannot take the old server down is not a restart: report it
        # instead of falling through to the already-running success below.
        do_stop || return 1
    fi
    if is_running; then
        echo "task-viewer is already running (PID: $(get_pid))"
        echo "Access at: http://127.0.0.1:$PORT"
        started=$(started_from_path "$(get_pid)" 2>/dev/null)
        if [ -n "$started" ] && [ "$started" != "$BINARY" ]; then
            echo "Started from: $started"
        fi
        case "$(uname -s)" in
            Darwin) open "http://127.0.0.1:$PORT" >/dev/null 2>&1 || true ;;
            Linux)  xdg-open "http://127.0.0.1:$PORT" >/dev/null 2>&1 || true ;;
        esac
        notify "task-viewer" "Already running at http://127.0.0.1:$PORT"
        return 0
    fi

    local holder
    holder=$(port_pid)
    if [ -n "$holder" ]; then
        echo "Port $PORT is held by another process (PID: $holder); not starting task-viewer."
        notify "task-viewer" "Port $PORT is in use by another program"
        return 1
    fi
    # A listener the tools cannot name (no lsof or ss, or another user's).
    if port_answers; then
        echo "Port $PORT is held by another process; not starting task-viewer."
        notify "task-viewer" "Port $PORT is in use by another program"
        return 1
    fi

    echo "Starting task-viewer..."
    # Remember which build this server runs, so a later rebuild is noticed.
    touch -r "$BINARY" "$STAMP_FILE"
    # Leave the Dock/.app process group. macOS tears that group down when the
    # launcher script exits; nohup alone still dies with the group. Artemis
    # often survives because it was already started outside the Dock session.
    if ! start_detached; then
        echo "Failed to start task-viewer. Check $LOG_FILE"
        notify "task-viewer" "Failed to start. See $LOG_FILE"
        rm -f "$PID_FILE"
        return 1
    fi
    # Capture the PID of the server this start launched once, before the probe
    # loop: `is_running` deletes the PID file when it reads the recorded PID as
    # foreign, which is exactly what the detacher's record-then-exec gap produces,
    # so a cleanup that re-read the file would have no handle on the process it
    # started.
    local started_pid=""
    for _ in {1..20}; do
        started_pid=$(get_pid)
        [ -n "$started_pid" ] && break
        sleep 0.1
    done
    # Wait for our process to own the port; a bind failure ends it instead.
    for _ in {1..20}; do
        if is_listening; then
            break
        fi
        sleep 0.5
    done

    if is_listening; then
        echo "task-viewer started (PID: $(get_pid))"
        echo "Access at: http://127.0.0.1:$PORT"
        notify "task-viewer" "Running at http://127.0.0.1:$PORT"
        case "$(uname -s)" in
            Darwin) open "http://127.0.0.1:$PORT" >/dev/null 2>&1 || true ;;
            Linux)  xdg-open "http://127.0.0.1:$PORT" >/dev/null 2>&1 || true ;;
        esac
    else
        # The server never took the port but is still running: stop the one this
        # start launched before reporting the failure, so a timeout leaves no
        # live server behind.
        if is_ours "$started_pid"; then
            kill "$started_pid" 2>/dev/null || true
            for _ in {1..10}; do
                if ! is_ours "$started_pid"; then
                    break
                fi
                sleep 0.5
            done
            if is_ours "$started_pid"; then
                kill -9 "$started_pid" 2>/dev/null || true
            fi
        fi
        echo "Failed to start task-viewer. Check $LOG_FILE"
        notify "task-viewer" "Failed to start. See $LOG_FILE"
        rm -f "$PID_FILE"
        return 1
    fi
}

do_start() {
    build_binary || return 1
    start_binary
}

# Double-fork + setsid so the binary is not in the Dock launcher's process group.
start_detached() {
    if ! command -v python3 >/dev/null 2>&1; then
        # nohup alone stays in the Dock launcher's process group, which macOS
        # tears down when the launcher exits. setsid leaves that group, so use
        # it when present and refuse rather than start a server that dies.
        if command -v setsid >/dev/null 2>&1; then
            setsid nohup "$BINARY" >"$LOG_FILE" 2>&1 &
            echo $! >"$PID_FILE"
            return 0
        fi
        echo "Cannot detach task-viewer: neither python3 nor setsid is available."
        echo "Install python3, or start the server by hand: nohup $BINARY &"
        notify "task-viewer" "Cannot detach: install python3 or setsid"
        return 1
    fi
    python3 - "$BINARY" "$LOG_FILE" "$PID_FILE" "$PROJECT_DIR" <<'PY'
import os, sys

binary, logfile, pidfile, project = sys.argv[1:5]
if os.fork() > 0:
    os._exit(0)
os.setsid()
if os.fork() > 0:
    os._exit(0)
os.chdir(project)
# Keep a restrictive umask: umask(0) made the log and PID file world-writable.
os.umask(0o077)
devnull = open("/dev/null", "rb")
os.dup2(devnull.fileno(), 0)


def private_file(path):
    # Truncate on each start. fchmod also tightens a file an older launcher
    # created, which the mode argument alone leaves as it was.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.fchmod(fd, 0o600)
    return fd


log = private_file(logfile)
os.dup2(log, 1)
os.dup2(log, 2)
pid = private_file(pidfile)
os.write(pid, str(os.getpid()).encode())
os.close(pid)
os.execv(binary, [binary])
PY
}

do_stop() {
    if ! is_running; then
        echo "task-viewer is not running"
        notify "task-viewer" "Already stopped"
        return 0
    fi

    # Read the PID once: a signal must reach the server this stop found, never a
    # PID that exited and was reused between the check and the kill.
    pid=$(get_pid)
    echo "Stopping task-viewer (PID: $pid)..."
    if is_ours "$pid"; then
        kill "$pid" 2>/dev/null || true
    fi

    for _ in {1..10}; do
        if ! is_running; then
            echo "task-viewer stopped"
            rm -f "$PID_FILE" "$STAMP_FILE"
            notify "task-viewer" "Stopped"
            return 0
        fi
        sleep 0.5
    done

    echo "Force stopping..."
    if is_ours "$pid"; then
        kill -9 "$pid" 2>/dev/null || true
    fi
    # A kill that did not take is not a stop: re-test before claiming one.
    for _ in {1..10}; do
        if ! is_ours "$pid"; then
            break
        fi
        sleep 0.1
    done
    if is_ours "$pid"; then
        echo "task-viewer did not stop (PID: $pid is still running)"
        notify "task-viewer" "Stop failed: the server survived"
        return 1
    fi
    rm -f "$PID_FILE" "$STAMP_FILE"
    echo "task-viewer stopped"
    notify "task-viewer" "Stopped"
}

do_status() {
    local started
    if is_running; then
        echo "task-viewer is running (PID: $(get_pid))"
        echo "Access at: http://127.0.0.1:$PORT"
        # A server started from a checkout that has since moved runs from a path
        # this checkout no longer names; say where it was started from instead
        # of leaving that implicit.
        started=$(started_from_path "$(get_pid)" 2>/dev/null)
        if [ -n "$started" ] && [ "$started" != "$BINARY" ]; then
            echo "Started from: $started"
        fi
    else
        echo "task-viewer is not running"
        # A stopped server is a non-zero status, so a script can branch on
        # `make status` the way it would on `systemctl is-active`. This branch
        # prints one line; the running branch adds the PID and the URL, plus the
        # started-from path when the server runs from another checkout's path.
        return 1
    fi
}

do_toggle() {
    if ! is_running; then
        do_start
        return
    fi
    # Build once, so a click after a source change restarts the server on the
    # new code instead of stopping it. A failed build still stops.
    if build_binary && is_outdated; then
        start_binary
    else
        do_stop
    fi
}

# Dispatch only when executed, so a test can source this file to reach the
# helpers without starting or stopping a server.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    # The whole command runs under one lock: `status` reaches is_running, which
    # writes and deletes the PID file, and `restart` runs a stop and a start as
    # one command. The helpers take no lock of their own, so do_start's restart
    # path runs inside this one instead of nesting on it.
    acquire_lock
    require_lock_ownership
    trap release_lock EXIT
    case "${1:-toggle}" in
        start)   do_start ;;
        stop)    do_stop ;;
        status)  do_status ;;
        restart) do_stop && do_start ;;
        toggle)  do_toggle ;;
        *)
            echo "Usage: $0 {start|stop|status|restart|toggle}"
            echo "  start   - Start task-viewer"
            echo "  stop    - Stop task-viewer"
            echo "  status  - Check if task-viewer is running"
            echo "  restart - Restart task-viewer"
            echo "  toggle  - Start if stopped, stop if running (default)"
            exit 1
            ;;
    esac
fi
