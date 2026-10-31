#!/bin/bash
# Reproduction for finding 2: the five harvest outcome arms must live once in
# store_outcome and be shared by the parallel join loop and refresh_one.
# Fails while the arms are duplicated and the helper is absent.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
src="$repo/task-viewer/src/web.rs"

if [ ! -f "$src" ]; then
    echo "FAIL: missing $src"
    exit 1
fi

status=0

if ! grep -q '^async fn store_outcome(' "$src"; then
    echo "FAIL: no store_outcome helper"
    status=1
fi

calls="$(grep -c 'store_outcome(pool, repository_id, outcome)' "$src" || true)"
if [ "$calls" -lt 2 ]; then
    echo "FAIL: store_outcome has $calls call site(s); expected the join loop and refresh_one"
    status=1
fi

helper="$(awk '/^async fn store_outcome\(/{flag=1} /^async fn refresh_one\(/{flag=0} flag' "$src")"
scan="$(awk '/^async fn scan_core\(/{flag=1} /^async fn store_outcome\(/{flag=0} flag' "$src")"
refresh="$(awk '/^async fn refresh_one\(/{flag=1} /^fn pick_folder\(/{flag=0} flag' "$src")"

while IFS= read -r message; do
    count="$(printf '%s\n' "$helper" | grep -F -c "$message" || true)"
    if [ "$count" -ne 1 ]; then
        echo "FAIL: '$message' appears $count time(s) in store_outcome; expected once"
        status=1
    fi
done <<'MESSAGES'
That folder has no tasks directory.
Git could not read the history, so the rows were left as they were.
The scan could not read the repository.
The scan stopped before it finished.
The parsed rows could not be saved.
MESSAGES

for region in scan refresh; do
    case "$region" in
        scan) body="$scan" ;;
        refresh) body="$refresh" ;;
    esac
    if printf '%s\n' "$body" | grep -q 'set_scan_error'; then
        echo "FAIL: $region still writes an outcome itself instead of calling store_outcome"
        status=1
    fi
done

if [ "$status" -ne 0 ]; then
    exit 1
fi

echo "PASS: the five outcome arms live once in store_outcome and both callers use it"
