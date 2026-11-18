#!/bin/bash
# Record or verify the per-file SHA-256 digests of the vendored `sqlx-sqlite`
# tree.
#
# `record` writes one `<digest>  <path>` line per file under
# `vendor/sqlx-sqlite`, in path order, taken from `git ls-files`, so a fresh
# clone reproduces the list and an ignored file such as the `*.orig` that
# `.gitignore` matches stays out.
#
# `verify` recomputes the digests over the working tree's non-ignored file set,
# tracked and untracked alike, taken from `git ls-files -co --exclude-standard`,
# and fails on an added, a missing, or a changed file.
#
# The vendored crate sits outside `Cargo.lock`'s checksum coverage and outside
# `cargo audit`'s scan, so these digests are what pins its bytes.
#
# Usage: vendor-sqlx-sqlite-sha256.sh [record|verify]
# Set VENDOR_SQLX_SQLITE_ROOT to run against a checkout other than this one.

set -u

TREE="vendor/sqlx-sqlite"
RECORD="vendor/sqlx-sqlite.sha256"

ROOT="${VENDOR_SQLX_SQLITE_ROOT:-}"
if [ -z "$ROOT" ]; then
    ROOT=$(git rev-parse --show-toplevel) || exit 1
fi
cd "$ROOT" || exit 1

hash_file() {
    # A missing or unreadable file prints no line, which `verify` reports as a
    # missing digest instead of aborting before the comparison runs.
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$1" || true
    else
        shasum -a 256 -- "$1" || true
    fi
}

digest_tree() {
    git ls-files -z "$@" -- "$TREE" | while IFS= read -r -d '' path; do
        hash_file "$path"
    done
    # The pipeline's status is the loop's, so a failed listing would otherwise
    # read as an empty tree. Propagate `git ls-files`'s own status instead.
    local listing=${PIPESTATUS[0]}
    return "$listing"
}

record() {
    local tmp mode
    # `mktemp` creates 0600 and `mv` carries that mode onto the record, so the
    # file would land less readable than the redirect this replaced left it.
    # Capture the umask and give the temp file the mode a plain creation would.
    mode=$(( 0666 & ~$(umask) ))
    tmp=$(mktemp "${TMPDIR:-/tmp}/vendor-sqlx-sqlite.XXXXXX") || exit 1
    if ! digest_tree >"$tmp"; then
        rm -f "$tmp"
        echo "error: could not list $TREE; $RECORD left unchanged" >&2
        exit 1
    fi
    chmod "$(printf '%04o' "$mode")" "$tmp" || exit 1
    mv "$tmp" "$RECORD" || exit 1
    echo "recorded $(wc -l <"$RECORD" | tr -d ' ') digests in $RECORD"
}

verify() {
    if [ ! -f "$RECORD" ]; then
        echo "error: $RECORD is missing; run $0 record" >&2
        exit 1
    fi
    local current report
    current=$(mktemp "${TMPDIR:-/tmp}/vendor-sqlx-sqlite.XXXXXX") || exit 1
    # shellcheck disable=SC2064
    trap "rm -f '$current'" EXIT
    digest_tree -co --exclude-standard >"$current" || exit 1

    report=$(awk '
        NR == FNR { rec[substr($0, 67)] = substr($0, 1, 64); recpaths[++n] = substr($0, 67); next }
        { cur[substr($0, 67)] = substr($0, 1, 64); curpaths[++m] = substr($0, 67) }
        END {
            bad = 0
            for (i = 1; i <= n; i++) {
                p = recpaths[i]
                if (!(p in cur)) { printf "missing  %s\n", p; bad = 1 }
                else if (cur[p] != rec[p]) { printf "changed  %s\n", p; bad = 1 }
            }
            for (i = 1; i <= m; i++) {
                p = curpaths[i]
                if (!(p in rec)) { printf "added    %s\n", p; bad = 1 }
            }
            exit bad
        }
    ' "$RECORD" "$current")
    local status=$?

    if [ "$status" -ne 0 ]; then
        printf '%s\n' "$report" >&2
        echo "error: $TREE does not match $RECORD" >&2
        exit 1
    fi
    echo "ok: $TREE matches $RECORD"
}

case "${1:-verify}" in
    record) record ;;
    verify) verify ;;
    *)
        echo "usage: $0 [record|verify]" >&2
        exit 2
        ;;
esac
