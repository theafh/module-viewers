#!/bin/bash
# Fail when `rsa` is reachable from this workspace's build graph.
#
# `.cargo/audit.toml` ignores RUSTSEC-2023-0071 on the grounds that `rsa` enters
# `Cargo.lock` through the optional `sqlx-mysql` backend, which this workspace
# never builds. `cargo tree -i rsa` is the recheck: it prints the reverse
# dependency listing on standard output when `rsa` is in the graph, and prints
# nothing there while it is not.
#
# Reachability is read from standard output rather than the exit status. In the
# healthy state cargo writes `warning: nothing to print.` to standard error and
# exits zero, so a captured `2>&1` would turn the healthy state red. A non-zero
# exit is a failure of the check itself, not a reachability signal, so it fails
# the check instead of passing it.
#
# Set CHECK_RSA_ROOT to run against a checkout other than this one.

set -u

ROOT="${CHECK_RSA_ROOT:-}"
if [ -z "$ROOT" ]; then
    ROOT=$(git rev-parse --show-toplevel) || exit 1
fi
cd "$ROOT" || exit 1

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is not on PATH, so rsa's reachability cannot be checked" >&2
    exit 1
fi

if ! listing=$(cargo tree -i rsa 2>/dev/null); then
    echo "error: cargo tree -i rsa failed, so rsa's reachability cannot be checked" >&2
    exit 1
fi

if [ -n "$listing" ]; then
    echo "error: rsa is reachable from the build graph, so RUSTSEC-2023-0071's ignore in .cargo/audit.toml no longer holds:" >&2
    printf '%s\n' "$listing" >&2
    exit 1
fi

echo "ok: rsa is outside the build graph"
