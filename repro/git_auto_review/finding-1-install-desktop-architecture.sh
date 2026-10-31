#!/bin/bash
# Reproduction for finding 1: the macOS bundle's executable is a shell script,
# so LSArchitecturePriority has no universal binary to order and the native_arch
# probe that feeds it must go. Fails while either is still present.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo/scripts/install-desktop.sh"

if [ ! -f "$script" ]; then
    echo "FAIL: missing $script"
    exit 1
fi

if grep -n 'LSArchitecturePriority\|native_arch' "$script"; then
    echo "FAIL: install-desktop.sh still sets an architecture priority"
    exit 1
fi

if ! bash -n "$script"; then
    echo "FAIL: install-desktop.sh is not valid bash"
    exit 1
fi

echo "PASS: install-desktop.sh leaves the architecture to the launcher build"
