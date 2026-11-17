#!/bin/bash
# Tests for the `make audit` gate's vendored-tree and rsa checks.
#
# Every case runs the script under test against a throwaway git root, so no
# real vendored file is edited and no real cargo run is needed. The rsa cases
# put a fake `cargo` first on PATH that prints the reverse listing, or only the
# `nothing to print.` warning on standard error, which is what the real command
# does in each state.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VENDOR_SCRIPT="$REPO/scripts/vendor-sqlx-sqlite-sha256.sh"
RSA_SCRIPT="$REPO/scripts/check-rsa-reachability.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/task-viewer-audit-gate.XXXXXX")"
FAILURES=0

trap 'rm -rf "$TMP"' EXIT

pass() {
    echo "ok - $1"
}

fail() {
    echo "not ok - $1"
    FAILURES=$((FAILURES + 1))
}

# A checkout-shaped root: a small vendored tree, an ignored `*.orig`, and an
# index that already lists the tracked files.
make_root() {
    local root="$1"
    mkdir -p "$root/vendor/sqlx-sqlite/src" "$root/bin"
    printf 'libsqlite3-sys = ">=0.30.1, <0.40.0"\n' >"$root/vendor/sqlx-sqlite/Cargo.toml"
    printf 'pub fn lib() {}\n' >"$root/vendor/sqlx-sqlite/src/lib.rs"
    printf 'ignored packaging leftover\n' >"$root/vendor/sqlx-sqlite/Cargo.toml.orig"
    printf '*.orig\n' >"$root/.gitignore"
    git -C "$root" init -q
    git -C "$root" add -A
}

vendor() {
    local root="$1"
    shift
    VENDOR_SQLX_SQLITE_ROOT="$root" bash "$VENDOR_SCRIPT" "$@" 2>&1
}

case_record_covers_tracked_files_in_order() {
    local root="$TMP/record" out status expected actual
    make_root "$root"
    out=$(vendor "$root" record)
    status=$?
    expected=$(git -C "$root" ls-files vendor/sqlx-sqlite)
    actual=$(sed 's/^[0-9a-f]\{64\}  //' "$root/vendor/sqlx-sqlite.sha256")
    if [ "$status" -eq 0 ] &&
        [ "$actual" = "$expected" ] &&
        ! printf '%s\n' "$actual" | grep -q 'Cargo.toml.orig'; then
        pass "record lists the tracked files in path order and skips the ignored *.orig"
    else
        fail "record lists the tracked files in path order and skips the ignored *.orig (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_clean_tree_passes() {
    local root="$TMP/clean" out status
    make_root "$root"
    vendor "$root" record >/dev/null
    out=$(vendor "$root" verify)
    status=$?
    if [ "$status" -eq 0 ] && printf '%s\n' "$out" | grep -q '^ok: '; then
        pass "verify passes on an unchanged tree"
    else
        fail "verify passes on an unchanged tree (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_changed_byte_fails() {
    local root="$TMP/changed" out status
    make_root "$root"
    vendor "$root" record >/dev/null
    printf ' ' >>"$root/vendor/sqlx-sqlite/Cargo.toml"
    out=$(vendor "$root" verify)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -q 'changed  vendor/sqlx-sqlite/Cargo.toml'; then
        pass "verify fails when a vendored file's bytes change"
    else
        fail "verify fails when a vendored file's bytes change (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_added_file_fails() {
    local root="$TMP/added" out status
    make_root "$root"
    vendor "$root" record >/dev/null
    printf 'extra\n' >"$root/vendor/sqlx-sqlite/extra.rs"
    out=$(vendor "$root" verify)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -q 'added    vendor/sqlx-sqlite/extra.rs'; then
        pass "verify fails when a file is added under the vendored tree"
    else
        fail "verify fails when a file is added under the vendored tree (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_missing_file_fails() {
    local root="$TMP/missing" out status
    make_root "$root"
    vendor "$root" record >/dev/null
    rm "$root/vendor/sqlx-sqlite/src/lib.rs"
    out=$(vendor "$root" verify)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -q 'missing  vendor/sqlx-sqlite/src/lib.rs'; then
        pass "verify fails when a recorded file is deleted"
    else
        fail "verify fails when a recorded file is deleted (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_missing_record_fails() {
    local root="$TMP/no-record" out status
    make_root "$root"
    out=$(vendor "$root" verify)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -q 'is missing'; then
        pass "verify fails when the recorded digest file is absent"
    else
        fail "verify fails when the recorded digest file is absent (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# A fake cargo that prints the reverse listing on standard output.
make_reachable_cargo() {
    cat >"$1/cargo" <<'EOS'
#!/bin/bash
printf 'rsa v0.9.10\n└── sqlx-mysql v0.8.6\n'
EOS
    chmod +x "$1/cargo"
}

# A fake cargo that prints only the healthy-state warning on standard error.
make_unreachable_cargo() {
    cat >"$1/cargo" <<'EOS'
#!/bin/bash
printf 'warning: nothing to print.\n' >&2
EOS
    chmod +x "$1/cargo"
}

case_rsa_outside_graph_passes() {
    local root="$TMP/rsa-clean" out status
    make_root "$root"
    make_unreachable_cargo "$root/bin"
    out=$(CHECK_RSA_ROOT="$root" PATH="$root/bin:/usr/bin:/bin" bash "$RSA_SCRIPT" 2>&1)
    status=$?
    if [ "$status" -eq 0 ] && printf '%s\n' "$out" | grep -q 'outside the build graph'; then
        pass "the rsa check passes when only the nothing-to-print warning is written"
    else
        fail "the rsa check passes when only the nothing-to-print warning is written (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_rsa_reachable_fails() {
    local root="$TMP/rsa-reachable" out status
    make_root "$root"
    make_reachable_cargo "$root/bin"
    out=$(CHECK_RSA_ROOT="$root" PATH="$root/bin:/usr/bin:/bin" bash "$RSA_SCRIPT" 2>&1)
    status=$?
    if [ "$status" -ne 0 ] && printf '%s\n' "$out" | grep -q 'rsa is reachable'; then
        pass "the rsa check fails when the reverse listing names rsa"
    else
        fail "the rsa check fails when the reverse listing names rsa (status $status)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

case_record_covers_tracked_files_in_order
case_clean_tree_passes
case_changed_byte_fails
case_added_file_fails
case_missing_file_fails
case_missing_record_fails
case_rsa_outside_graph_passes
case_rsa_reachable_fails

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES audit-gate test(s) failed"
    exit 1
fi
echo "all audit-gate tests passed"
