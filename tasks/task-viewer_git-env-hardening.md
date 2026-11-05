---
description: "Scrub the remaining inherited git environment and pin log output options so user configuration cannot change parsed paths, dates, or signatures."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Pin the git environment and log output

## Goal

Inherited environment variables and user git configuration cannot change the paths, dates, or signatures that harvest parses.

## Context

`scrub_git_env` in `task-viewer/src/harvest.rs` removes `GIT_DIR`, `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, `GIT_INDEX_FILE`, and `GIT_ALTERNATE_OBJECT_DIRECTORIES`, and sets `GIT_TERMINAL_PROMPT=0` and a stable `PATH`. It leaves `GIT_EXTERNAL_DIFF`, `GIT_CONFIG_*`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`, and `GIT_NAMESPACE`. A user `log.showSignature=true` can add signature output to `git log`, and `GIT_EXTERNAL_DIFF` can change diff behavior.

## Approach

Extend `scrub_git_env` to remove the remaining inherited git variables that can change read-only output, and add `GIT_OPTIONAL_LOCKS=0` so git never takes an optional write lock in the viewed repository. Pass `-c log.showSignature=false` and `-c log.decorate=false` to the log commands, and keep the existing `-z` and explicit formats. Add tests that assert the variables are removed and the config overrides are present on the command.

**Out of scope:** Ignoring the user's global git configuration entirely; `safe.directory` and similar settings must keep working.

## Acceptance

- A test calls `scrub_git_env` and asserts `GIT_EXTERNAL_DIFF`, `GIT_CONFIG_COUNT`, and `GIT_CEILING_DIRECTORIES` are removed and `GIT_OPTIONAL_LOCKS` is `0`.
- A test inspects a log command and asserts it carries `-c log.showSignature=false`.
- An existing harvest test with a global `log.showSignature=true`-style fixture still returns the expected dates.
