//! Launcher tests: drive `scripts/task-viewer.sh` through a bash subprocess.
#![cfg(test)]

use crate::test_support::unique_temp_dir;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The git variables a test process may inherit; the launcher probes must not
/// see them, so every run clears them explicitly.
const GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_INDEX_FILE",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// The launcher lives beside the crate, at the workspace root.
fn repo_launcher() -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("task-viewer sits inside the workspace root");
    repo.join("scripts/task-viewer.sh")
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod script");
}

/// Stand in for procps: a long command line comes back empty unless `-ww`
/// asks for the full width, which is the truncation the launcher must avoid.
const FAKE_PS: &str = r#"#!/bin/sh
case " $* " in
    *" -ww "*) printf '%s\n' "$FAKE_PS_ARGS" ;;
esac
"#;

/// Source the launcher without dispatching, then exercise `is_ours` against
/// the fake `ps`. The positional probe matches no dispatcher arm, so a missing
/// main guard would print usage and exit before the assertions run.
const DRIVER: &str = r#"#!/bin/bash
set -u
set -- source-guard-probe
# shellcheck source=/dev/null
. "$SCRIPT_PATH"
BINARY="$LONG_BINARY"
if ! is_ours 4242; then
    echo "is_ours rejected the full long path" >&2
    exit 10
fi
BINARY="$OTHER_BINARY"
if is_ours 4242; then
    echo "is_ours accepted a path ps never printed" >&2
    exit 11
fi
printf 'long-ok\nforeign-rejected\n'
"#;

#[test]
fn launcher_is_ours_reads_the_full_command_line() {
    let dir = unique_temp_dir("launcher_ps");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).expect("fake bin dir");
    write_executable(&bin.join("ps"), FAKE_PS);
    let driver = dir.join("driver.sh");
    write_executable(&driver, DRIVER);

    let long_binary = dir
        .join("long")
        .join("x".repeat(220))
        .join("task-viewer")
        .to_string_lossy()
        .into_owned();
    assert!(
        long_binary.len() > 200,
        "the fixture path must exceed 200 bytes: {long_binary}"
    );
    let other_binary = dir
        .join("other")
        .join("task-viewer")
        .to_string_lossy()
        .into_owned();

    let launcher = repo_launcher();
    assert!(
        launcher.is_file(),
        "launcher missing at {}",
        launcher.display()
    );

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("bash");
    command
        .arg("--noprofile")
        .arg("--norc")
        .arg(&driver)
        .env("PATH", path)
        // Keep the launcher's state and log files out of the live directories.
        .env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("SCRIPT_PATH", &launcher)
        .env("LONG_BINARY", &long_binary)
        .env("OTHER_BINARY", &other_binary)
        .env("FAKE_PS_ARGS", &long_binary);
    for name in GIT_ENV {
        command.env_remove(name);
    }

    let output = command.output().expect("run bash");
    assert!(
        output.status.success(),
        "launcher probe failed ({:?})\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("long-ok"),
        "the long path must be recognized: {stdout}"
    );
    assert!(
        stdout.contains("foreign-rejected"),
        "a path ps never printed must be rejected: {stdout}"
    );

    let _ = fs::remove_dir_all(&dir);
}
