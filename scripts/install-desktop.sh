#!/bin/bash
# Install task-viewer desktop launcher.
# - Linux: ~/.local/share/applications desktop entry (app menu / favorites)
# - macOS: ~/Applications/Task Viewer.app that can be pinned to the Dock
#
# Clicking the icon toggles the server (start if stopped, stop if running).

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
ICON_SOURCE="$PROJECT_DIR/task-viewer/static/task-viewer-icon.png"
LAUNCHER_SCRIPT="$PROJECT_DIR/scripts/task-viewer.sh"
PORT=7880
APP_NAME="Task Viewer"

chmod +x "$LAUNCHER_SCRIPT"

# Quote a path as one desktop-entry Exec argument, so a checkout path with a
# space or a shell character stays one argument. The quoting rule escapes
# " ` $ \ with a backslash; the string rule then doubles every backslash, and
# % becomes %% so it is not read as a field code.
desktop_exec_arg() {
    local value="$1"
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//\`/\\\`}
    value=${value//\$/\\\$}
    value=${value//\\/\\\\}
    value=${value//%/%%}
    printf '"%s"' "$value"
}

install_linux() {
    local applications_dir="$HOME/.local/share/applications"
    local icons_dir="$HOME/.local/share/icons/hicolor/256x256/apps"
    local desktop_file="$applications_dir/task-viewer.desktop"

    echo "Installing task-viewer desktop entry (Linux)..."
    echo "Project directory: $PROJECT_DIR"

    mkdir -p "$applications_dir"
    mkdir -p "$icons_dir"

    if [ -f "$ICON_SOURCE" ]; then
        # Desktop theme icons expect 256x256; source is 512x512.
        if command -v convert >/dev/null 2>&1 || command -v magick >/dev/null 2>&1; then
            if command -v magick >/dev/null 2>&1; then
                magick "$ICON_SOURCE" -resize 256x256 "$icons_dir/task-viewer.png"
            else
                convert "$ICON_SOURCE" -resize 256x256 "$icons_dir/task-viewer.png"
            fi
        else
            cp "$ICON_SOURCE" "$icons_dir/task-viewer.png"
        fi
        echo "✓ Icon installed to $icons_dir/task-viewer.png"
    else
        echo "⚠ Warning: Icon not found at $ICON_SOURCE"
    fi

    cat > "$desktop_file" << EOF
[Desktop Entry]
Version=1.0
Type=Application
Name=Task Viewer
Comment=Local dashboard for repository tasks trees
Exec=$(desktop_exec_arg "$LAUNCHER_SCRIPT")
Icon=task-viewer
Terminal=false
Categories=Development;Office;
StartupWMClass=task-viewer
Keywords=tasks;dashboard;charts;backlog;
EOF

    echo "✓ Desktop entry created at $desktop_file"

    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$applications_dir" 2>/dev/null || true
        echo "✓ Desktop database updated"
    fi

    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
        echo "✓ Icon cache updated"
    fi

    echo ""
    echo "Installation complete!"
    echo "Task Viewer should appear in your application menu; pin it to favorites if you want."
    echo "Clicking the icon toggles start/stop."
    echo "Also: make start | make stop | make status"
    echo "URL: http://127.0.0.1:$PORT"
}

build_macos_icns() {
    local png="$1"
    local icns_out="$2"
    local work
    work="$(mktemp -d "${TMPDIR:-/tmp}/task-viewer-iconset.XXXXXX")"
    local iconset="$work/AppIcon.iconset"
    mkdir -p "$iconset"

    # iconutil expects this exact set of names and sizes.
    sips -z 16 16 "$png" --out "$iconset/icon_16x16.png" >/dev/null
    sips -z 32 32 "$png" --out "$iconset/icon_16x16@2x.png" >/dev/null
    sips -z 32 32 "$png" --out "$iconset/icon_32x32.png" >/dev/null
    sips -z 64 64 "$png" --out "$iconset/icon_32x32@2x.png" >/dev/null
    sips -z 128 128 "$png" --out "$iconset/icon_128x128.png" >/dev/null
    sips -z 256 256 "$png" --out "$iconset/icon_128x128@2x.png" >/dev/null
    sips -z 256 256 "$png" --out "$iconset/icon_256x256.png" >/dev/null
    sips -z 512 512 "$png" --out "$iconset/icon_256x256@2x.png" >/dev/null
    sips -z 512 512 "$png" --out "$iconset/icon_512x512.png" >/dev/null
    sips -z 1024 1024 "$png" --out "$iconset/icon_512x512@2x.png" >/dev/null

    if iconutil -c icns "$iconset" -o "$icns_out" 2>/dev/null; then
        rm -rf "$work"
        return 0
    fi
    rm -rf "$work"
    return 1
}

install_macos() {
    local apps_dir="$HOME/Applications"
    local app_bundle="$apps_dir/Task Viewer.app"
    local contents="$app_bundle/Contents"
    local macos_dir="$contents/MacOS"
    local resources_dir="$contents/Resources"
    local launcher_bin="$macos_dir/task-viewer-launcher"

    echo "Installing Task Viewer.app for the Mac Dock..."
    echo "Project directory: $PROJECT_DIR"

    mkdir -p "$apps_dir"
    rm -rf "$app_bundle"
    mkdir -p "$macos_dir" "$resources_dir"

    # %q keeps a path with quotes or $ intact inside the generated script.
    cat > "$launcher_bin" << EOF
#!/bin/bash
# Task Viewer Dock launcher — toggles the local server.
exec $(printf '%q' "$LAUNCHER_SCRIPT") toggle
EOF
    chmod +x "$launcher_bin"

    cat > "$contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>task-viewer-launcher</string>
    <key>CFBundleIdentifier</key>
    <string>local.task-viewer.launcher</string>
    <key>CFBundleName</key>
    <string>Task Viewer</string>
    <key>CFBundleDisplayName</key>
    <string>Task Viewer</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

    if [ -f "$ICON_SOURCE" ]; then
        if build_macos_icns "$ICON_SOURCE" "$resources_dir/AppIcon.icns"; then
            echo "✓ Icon installed as AppIcon.icns"
        else
            cp "$ICON_SOURCE" "$resources_dir/AppIcon.png"
            echo "⚠ iconutil failed; copied PNG into Resources (Dock may show a generic icon)"
        fi
    else
        echo "⚠ Warning: Icon not found at $ICON_SOURCE"
    fi

    # Clear quarantine so a first click is less likely to be blocked.
    if command -v xattr >/dev/null 2>&1; then
        xattr -dr com.apple.quarantine "$app_bundle" 2>/dev/null || true
    fi

    echo "✓ App bundle created at $app_bundle"

    pin_to_dock "$app_bundle"

    echo ""
    echo "Installation complete!"
    echo "Clicking the Dock icon toggles start/stop."
    echo "Also: make start | make stop | make status"
    echo "URL: http://127.0.0.1:$PORT"
    echo ""
    echo "If macOS blocks the first launch: right-click Task Viewer.app → Open."
}

# Pin an .app to the Dock. Prefer dockutil; otherwise edit Dock prefs and restart Dock.
pin_to_dock() {
    local app_bundle="$1"

    if command -v dockutil >/dev/null 2>&1; then
        dockutil --remove "$APP_NAME" --no-restart 2>/dev/null || true
        dockutil --add "$app_bundle" --replacing "$APP_NAME"
        echo "✓ Added Task Viewer to the Dock"
        return 0
    fi

    # Drop every existing Task Viewer tile first so reinstall never doubles icons.
    # defaults stores file:// URLs, so a raw path grep misses the live tile.
    python3 - "$app_bundle" <<'PY'
import pathlib, plistlib, subprocess, sys
app = pathlib.Path(sys.argv[1]).resolve().as_uri()
raw = subprocess.check_output(["defaults", "export", "com.apple.dock", "-"])
data = plistlib.loads(raw)
kept = []
for tile in data.get("persistent-apps", []):
    td = tile.get("tile-data") or {}
    bid = td.get("bundle-identifier")
    label = td.get("file-label")
    if bid == "local.task-viewer.launcher" or label == "Task Viewer":
        continue
    kept.append(tile)
kept.append({
    "tile-data": {
        "file-data": {"_CFURLString": app, "_CFURLStringType": 15},
        "file-label": "Task Viewer",
        "bundle-identifier": "local.task-viewer.launcher",
    },
    "tile-type": "file-tile",
})
data["persistent-apps"] = kept
# Import via stdin so we never write a world-visible fixed path under /tmp.
subprocess.run(
    ["defaults", "import", "com.apple.dock", "-"],
    input=plistlib.dumps(data, fmt=plistlib.FMT_BINARY),
    check=True,
)
PY
    killall Dock 2>/dev/null || true
    echo "✓ Added Task Viewer to the Dock"
}

case "$(uname -s)" in
    Darwin)
        echo "Detected macOS — installing Dock app (Task Viewer.app)."
        install_macos
        ;;
    Linux)
        echo "Detected Linux — installing desktop entry."
        install_linux
        ;;
    *)
        echo "Unsupported platform: $(uname -s)"
        echo "Use the launcher directly: $LAUNCHER_SCRIPT"
        exit 1
        ;;
esac
