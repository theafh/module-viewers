#!/bin/bash
# Uninstall the task-viewer desktop launcher that install-desktop.sh wrote.
# - Linux: the application-menu entry and its icon
# - macOS: ~/Applications/Task Viewer.app, its Dock tile, and any staging or
#   backup directory a failed atomic install left beside the bundle
#
# Idempotent: a missing path is success. Leaves ~/.task-viewer, the launcher's
# state directory, and the checkout alone.
#
# This script is self-contained on purpose. scripts/install-desktop.sh chmods the
# launcher and ends in a case that installs, and scripts/task-viewer.sh creates
# and chmods its state directory at top level, so neither can be sourced. The
# login-home guard mirrors login_account_home in install-desktop.sh; the
# state-directory rule and started_from_path mirror task-viewer.sh.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="$PROJECT_DIR/target/release/task-viewer"
PORT=7880
APP_NAME="Task Viewer"
BUNDLE_ID="local.task-viewer.launcher"

removed_any=0

# Print each path this run removed, and remember that something was removed.
remove_path() {
    local path="$1"
    if [ -e "$path" ] || [ -L "$path" ]; then
        rm -rf "$path"
        echo "Removed: $path"
        removed_any=1
    fi
}

# The login account's home directory, resolved from the account database rather
# than $HOME. Mirrors login_account_home in scripts/install-desktop.sh. Fails
# when the account name or its home cannot be read or is not absolute.
login_account_home() {
    local user home
    if ! user="$(id -un)" || [ -z "$user" ]; then
        return 1
    fi
    home=""
    if command -v dscl >/dev/null 2>&1; then
        home="$(dscl . -read "/Users/$user" NFSHomeDirectory 2>/dev/null)" || home=""
        home="${home#NFSHomeDirectory: }"
    fi
    if [ -z "$home" ] || [ "${home#/}" = "$home" ]; then
        home="$(eval echo "~$user" 2>/dev/null)" || home=""
    fi
    if [ -z "$home" ] || [ "${home#/}" = "$home" ]; then
        return 1
    fi
    printf '%s\n' "$home"
}

# The launcher's state directory. Mirrors the case in scripts/task-viewer.sh;
# this script only reads it and never creates or chmods it.
state_dir() {
    case "$(uname -s)" in
        Darwin) printf '%s\n' "$HOME/Library/Logs/task-viewer" ;;
        *)
            if [ -n "${XDG_STATE_HOME:-}" ]; then
                printf '%s\n' "$XDG_STATE_HOME/task-viewer"
            else
                printf '%s\n' "$HOME/.local/state/task-viewer"
            fi
            ;;
    esac
}

# Collapse `.` and `..` components without touching the filesystem. Mirrors
# normalize_path in scripts/task-viewer.sh.
normalize_path() {
    local path="$1" part last out absolute=0
    local -a parts=()
    case "$path" in
        /*) absolute=1 ;;
    esac
    local IFS='/'
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

# Resolve a path's directory with pwd -P and join the final component back.
# Mirrors canonical_path in scripts/task-viewer.sh.
canonical_path() {
    local path="$1" dir
    [ -n "$path" ] || return 1
    dir=$(cd "$(dirname -- "$path")" 2>/dev/null && pwd -P) || return 1
    printf '%s\n' "$dir/$(basename -- "$path")"
}

# Print the binary path a live PID was started from when that path is a
# task-viewer release binary, and fail for any other process. Mirrors
# started_from_path in scripts/task-viewer.sh: this checkout's $BINARY, or
# another checkout's /target/release/task-viewer, at an argument boundary.
started_from_path() {
    local candidate="$1"
    [ -n "$candidate" ] || return 1
    local args token resolved dir path prefix rest first part
    args=$(ps -ww -p "$candidate" -o args= 2>/dev/null) || return 1
    [ -n "$args" ] || return 1
    case "$args" in
        "$BINARY"|"$BINARY "*) printf '%s\n' "$BINARY"; return 0 ;;
    esac
    rest=${args##*"/target/release/task-viewer"}
    if [ "$rest" != "$args" ]; then
        case "$rest" in
            ''|[[:space:]]*)
                prefix=${args%"$rest"}
                prefix=${prefix%"/target/release/task-viewer"}
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

# Warn when a live server still owns the launcher, without stopping it. The read
# creates nothing in the state directory.
warn_if_running() {
    local dir pid
    dir="$(state_dir)"
    [ -f "$dir/task-viewer.pid" ] || return 0
    pid="$(cat "$dir/task-viewer.pid" 2>/dev/null)" || return 0
    [ -n "$pid" ] || return 0
    if started_from_path "$pid" >/dev/null 2>&1; then
        echo "task-viewer is still running (PID: $pid); run make stop first if you want it stopped."
    fi
}

# Guidance for a Dock removal that cannot be automated. Unlike the installer's
# add-oriented guidance, this names the domain and says to remove the tile.
dock_removal_guidance() {
    local reason="$1"
    echo "Cannot remove the Task Viewer tile from the Dock automatically: $reason."
    echo "Remove the Task Viewer tile from the Dock by hand; it is stored in the com.apple.dock persistent-apps list."
}

# Remove the Task Viewer tile from com.apple.dock, restarting the Dock only when
# a tile was actually removed. Returns non-zero when the removal cannot be done
# and the user has to remove the tile by hand.
remove_dock_tile() {
    local removed=0 before after
    if command -v dockutil >/dev/null 2>&1; then
        if dockutil --list >/dev/null 2>&1; then
            before=$(dockutil --list 2>/dev/null | grep -cxF "$APP_NAME" || true)
            dockutil --remove "$APP_NAME" --no-restart >/dev/null 2>&1 || true
            after=$(dockutil --list 2>/dev/null | grep -cxF "$APP_NAME" || true)
            if [ "${before:-0}" -gt "${after:-0}" ]; then
                removed=1
            fi
        else
            if dockutil --remove "$APP_NAME" --no-restart >/dev/null 2>&1; then
                removed=1
            fi
        fi
        if [ "$removed" -eq 1 ]; then
            killall Dock 2>/dev/null || true
        fi
        return 0
    fi

    if ! command -v python3 >/dev/null 2>&1; then
        dock_removal_guidance "python3 is not available"
        return 1
    fi

    if ! removed=$(python3 - "$APP_NAME" "$BUNDLE_ID" 2>/dev/null <<'PY'
import plistlib, subprocess, sys
name, bundle_id = sys.argv[1], sys.argv[2]
raw = subprocess.check_output(["defaults", "export", "com.apple.dock", "-"])
data = plistlib.loads(raw)
kept = []
removed = 0
for tile in data.get("persistent-apps", []):
    td = tile.get("tile-data") or {}
    if td.get("bundle-identifier") == bundle_id or td.get("file-label") == name:
        removed += 1
        continue
    kept.append(tile)
if removed:
    data["persistent-apps"] = kept
    # Import via stdin so no world-visible fixed path is written under /tmp.
    subprocess.run(
        ["defaults", "import", "com.apple.dock", "-"],
        input=plistlib.dumps(data, fmt=plistlib.FMT_BINARY),
        check=True,
    )
print(removed)
PY
    ); then
        dock_removal_guidance "the com.apple.dock plist edit failed"
        return 1
    fi

    if [ "${removed:-0}" -gt 0 ] 2>/dev/null; then
        killall Dock 2>/dev/null || true
    fi
    return 0
}

uninstall_macos() {
    local apps_dir="$HOME/Applications"
    local app_bundle="$apps_dir/Task Viewer.app"
    local dir dock_status=0
    local pin_mode="${TASK_VIEWER_DOCK_PIN:-auto}"

    echo "Removing Task Viewer.app for the Mac Dock..."

    remove_path "$app_bundle"

    # A failed atomic install leaves a staging or backup directory beside the
    # bundle; a successful swap whose backup cleanup failed leaves the backup.
    for dir in "$apps_dir"/.Task\ Viewer.app.staging.* "$apps_dir"/.Task\ Viewer.app.backup.*; do
        [ -e "$dir" ] || continue
        remove_path "$dir"
    done

    case "$pin_mode" in
        force)
            remove_dock_tile || dock_status=$?
            ;;
        skip)
            echo "Skipping the Dock removal: TASK_VIEWER_DOCK_PIN=skip."
            ;;
        *)
            local login_home
            if ! login_home="$(login_account_home)"; then
                echo "Skipping the Dock removal: cannot determine the login account's home."
            elif [ "$HOME" != "$login_home" ]; then
                echo "Skipping the Dock removal: HOME ($HOME) is not the login account's home."
            else
                remove_dock_tile || dock_status=$?
            fi
            ;;
    esac

    if [ "$dock_status" -ne 0 ]; then
        return "$dock_status"
    fi
    return 0
}

uninstall_linux() {
    local applications_dir="$HOME/.local/share/applications"
    local icons_dir="$HOME/.local/share/icons/hicolor/256x256/apps"

    echo "Removing the task-viewer desktop entry (Linux)..."

    remove_path "$applications_dir/task-viewer.desktop"
    remove_path "$icons_dir/task-viewer.png"

    if command -v update-desktop-database >/dev/null 2>&1; then
        if update-desktop-database "$applications_dir" 2>/dev/null; then
            echo "Desktop database updated"
        else
            echo "Desktop database refresh failed; the menu entry may stay stale until you log in again."
        fi
    fi

    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        if gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null; then
            echo "Icon cache updated"
        else
            echo "Icon cache refresh failed; the menu icon may stay stale until you log in again."
        fi
    fi
}

warn_if_running

status=0
case "$(uname -s)" in
    Darwin)
        uninstall_macos || status=$?
        ;;
    Linux)
        uninstall_linux
        ;;
    *)
        echo "Unsupported platform: $(uname -s)"
        echo "Use the launcher directly: $PROJECT_DIR/scripts/task-viewer.sh"
        exit 1
        ;;
esac

if [ "$removed_any" -eq 0 ]; then
    echo "Nothing left to remove."
fi
echo "The database and logs were kept."

exit "$status"
