#!/bin/bash
# task-viewer launcher script
# Usage: task-viewer.sh [start|stop|status|restart|toggle]
# Default (no args): toggle - starts if stopped, stops if running

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="$PROJECT_DIR/target/release/task-viewer"
PID_FILE="$PROJECT_DIR/.task-viewer.pid"
LOG_FILE="$PROJECT_DIR/.task-viewer.log"
PORT=7880

# Dock / GUI launches have a minimal PATH; pull in rustup cargo when needed.
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi
export PATH="/usr/local/bin:/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
# Always build/run from this repo's target/, not a redirected agent cache.
unset CARGO_TARGET_DIR
export CARGO_TARGET_DIR="$PROJECT_DIR/target"

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

is_running() {
    if [ -f "$PID_FILE" ]; then
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        rm -f "$PID_FILE"
    fi
    # Fallback: check if port is in use
    if lsof -i :"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

get_pid() {
    if [ -f "$PID_FILE" ]; then
        cat "$PID_FILE"
    else
        lsof -t -i :"$PORT" -sTCP:LISTEN 2>/dev/null
    fi
}

do_start() {
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

    # Build if binary doesn't exist
    if [ ! -x "$BINARY" ]; then
        echo "Building task-viewer..."
        notify "task-viewer" "Building release binary (first run)…"
        if ! command -v cargo >/dev/null 2>&1; then
            echo "cargo not found; cannot build task-viewer."
            notify "task-viewer" "Build failed: cargo not found. Run 'make build' in the project."
            return 1
        fi
        if ! cargo build --release -p task-viewer >>"$LOG_FILE" 2>&1; then
            echo "Build failed. Check $LOG_FILE"
            notify "task-viewer" "Build failed. See .task-viewer.log"
            return 1
        fi
        if [ ! -x "$BINARY" ]; then
            echo "Build finished but binary is missing at $BINARY"
            notify "task-viewer" "Build failed: binary missing"
            return 1
        fi
    fi

    echo "Starting task-viewer..."
    nohup "$BINARY" > "$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
    sleep 1

    if is_running; then
        echo "task-viewer started (PID: $(get_pid))"
        echo "Access at: http://127.0.0.1:$PORT"
        notify "task-viewer" "Running at http://127.0.0.1:$PORT"
        case "$(uname -s)" in
            Darwin) open "http://127.0.0.1:$PORT" >/dev/null 2>&1 || true ;;
            Linux)  xdg-open "http://127.0.0.1:$PORT" >/dev/null 2>&1 || true ;;
        esac
    else
        echo "Failed to start task-viewer. Check $LOG_FILE"
        notify "task-viewer" "Failed to start. See .task-viewer.log"
        rm -f "$PID_FILE"
        return 1
    fi
}

do_stop() {
    if ! is_running; then
        echo "task-viewer is not running"
        notify "task-viewer" "Already stopped"
        return 0
    fi

    pid=$(get_pid)
    echo "Stopping task-viewer (PID: $pid)..."
    kill "$pid" 2>/dev/null

    # Wait for graceful shutdown
    for i in {1..10}; do
        if ! is_running; then
            echo "task-viewer stopped"
            rm -f "$PID_FILE"
            notify "task-viewer" "Stopped"
            return 0
        fi
        sleep 0.5
    done

    # Force kill if still running
    echo "Force stopping..."
    kill -9 "$pid" 2>/dev/null
    rm -f "$PID_FILE"
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
    if is_running; then
        do_stop
    else
        do_start
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
