#!/bin/bash
# task-viewer launcher
# Usage: task-viewer.sh [start|stop|status|restart|toggle]
# Default (Dock): toggle — start if stopped, stop if running

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="$PROJECT_DIR/target/release/task-viewer"
PORT=7880

# Runtime state lives outside the checkout: the log records repository paths.
case "$(uname -s)" in
    Darwin) STATE_DIR="$HOME/Library/Logs/task-viewer" ;;
    *)      STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/task-viewer" ;;
esac
PID_FILE="$STATE_DIR/task-viewer.pid"
LOG_FILE="$STATE_DIR/task-viewer.log"
BUILD_LOG="$STATE_DIR/build.log"
STAMP_FILE="$STATE_DIR/task-viewer.binary"

# Files this launcher creates are private to the person who runs it.
umask 077
mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR"

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
chmod 600 "$PID_FILE" "$LOG_FILE" "$BUILD_LOG" 2>/dev/null

# Dock / GUI launches have a minimal PATH; pull in rustup cargo when needed.
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi
export PATH="/usr/local/bin:/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
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

# True when the PID is this checkout's release binary. A live PID alone is not
# enough: PIDs are reused, and another program may own the port.
is_ours() {
    local candidate="$1"
    [ -n "$candidate" ] || return 1
    local args
    args=$(ps -p "$candidate" -o args= 2>/dev/null | sed 's/[[:space:]]*$//')
    [ "$args" = "$BINARY" ]
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
        if [ -x "$BINARY" ]; then
            return 0
        fi
        echo "cargo not found; cannot build task-viewer."
        notify "task-viewer" "Build failed: cargo not found. Run 'make build' in the project."
        return 1
    fi
    if [ ! -x "$BINARY" ]; then
        echo "Building task-viewer..."
        notify "task-viewer" "Building release binary (first run)…"
    fi
    if ! cargo build --release -p task-viewer >"$BUILD_LOG" 2>&1; then
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

do_start() {
    build_binary || return 1
    if is_running && is_outdated; then
        echo "A newer task-viewer binary exists; restarting on it."
        do_stop
    fi
    if is_running; then
        echo "task-viewer is already running (PID: $(get_pid))"
        echo "Access at: http://127.0.0.1:$PORT"
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
        echo "Failed to start task-viewer. Check $LOG_FILE"
        notify "task-viewer" "Failed to start. See $LOG_FILE"
        rm -f "$PID_FILE"
        return 1
    fi
}

# Double-fork + setsid so the binary is not in the Dock launcher's process group.
start_detached() {
    if ! command -v python3 >/dev/null 2>&1; then
        nohup "$BINARY" >"$LOG_FILE" 2>&1 &
        echo $! >"$PID_FILE"
        return 0
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

    pid=$(get_pid)
    echo "Stopping task-viewer (PID: $pid)..."
    kill "$pid" 2>/dev/null || true

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
    rm -f "$PID_FILE" "$STAMP_FILE"
    echo "task-viewer stopped"
    notify "task-viewer" "Stopped"
}

do_status() {
    if is_running; then
        echo "task-viewer is running (PID: $(get_pid))"
        echo "Access at: http://127.0.0.1:$PORT"
    else
        echo "task-viewer is not running"
    fi
}

do_toggle() {
    if ! is_running; then
        do_start
        return
    fi
    # Build first, so a click after a source change restarts the server on
    # the new code instead of stopping it. A failed build still stops.
    if build_binary && is_outdated; then
        do_start
    else
        do_stop
    fi
}

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
