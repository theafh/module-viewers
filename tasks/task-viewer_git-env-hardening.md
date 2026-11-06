---
description: "Scrub the remaining inherited git environment and pin log output options so user configuration cannot change parsed paths, dates, or signatures."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T16:18:48
status: ready
reported-by: "Andreas Hoffmann"
---

# Pin the git environment and log output

## Goal

Inherited environment variables and user git configuration cannot change the paths, dates, or signatures that harvest parses.

## Context

`scrub_git_env` in `task-viewer/src/harvest.rs` removes `GIT_DIR`, `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, `GIT_INDEX_FILE`, and `GIT_ALTERNATE_OBJECT_DIRECTORIES`, and sets `GIT_TERMINAL_PROMPT=0` and a stable `PATH`. It leaves `GIT_EXTERNAL_DIFF`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`, and `GIT_NAMESPACE`. A user `log.showSignature=true` can add signature output to `git log`, and `GIT_EXTERNAL_DIFF` can change diff behavior.

## Approach

Extend `scrub_git_env` to also remove `GIT_EXTERNAL_DIFF`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`, `GIT_NAMESPACE`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, and `GIT_CONFIG_NOSYSTEM`, alongside the `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS` it already removes, and set `GIT_OPTIONAL_LOCKS=0` so git never takes an optional write lock in the viewed repository. Removing the three `GIT_CONFIG_*` redirections leaves git on the user's default global and system config files, so `safe.directory` keeps working while no inherited value can point git at a different config file or ref namespace. Build the arguments for every `git log` harvest runs in one test-callable helper, `git_log_args`, which returns `-c log.showSignature=false` and `-c log.decorate=false` ahead of the flags its caller supplies, and have `blob_days`, `archived_leave_lookup`, and `run_git_log` in `task-viewer/src/harvest.rs` take their log arguments from it; keep the existing `-z` on the archive walk and the explicit `--format` on every log command. Add tests that assert the variables are removed and the config overrides are present on the command.

**Out of scope:** Making git ignore the user's global and system configuration (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`), since `safe.directory` and similar settings must keep working.

## Acceptance

- A test calls `scrub_git_env` and asserts `GIT_EXTERNAL_DIFF`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`, and `GIT_NAMESPACE` are removed and `GIT_OPTIONAL_LOCKS` is `0`.
- A test calls `git_log_args` with the flags every production `git log` call site in `task-viewer/src/harvest.rs` supplies and asserts that every returned argument list carries the pair `-c` followed by `log.showSignature=false` and the pair `-c` followed by `log.decorate=false`, selected by value rather than by argument position or count, and that both pairs precede the `log` subcommand.
- A test obtains the argument list each production `git log` call site in `task-viewer/src/harvest.rs` assembles — through a test-callable per-site argv builder or a fake `git` wrapper that records its argv — and asserts that each one carries the pair `-c` followed by `log.showSignature=false` and the pair `-c` followed by `log.decorate=false`, selected by value rather than by argument position or count, and that both pairs precede the `log` subcommand.
