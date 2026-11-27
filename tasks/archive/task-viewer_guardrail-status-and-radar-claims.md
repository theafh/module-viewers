---
description: "Correct the status-line and radar drill-down claims in the guardrail docs, the launcher comment, and the launcher test so all four match the code."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-27T01:48:34Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Correct two guardrail claims that do not match the code

## Goal

`FEATURES.md` and `TESTING.md` state what `status` prints and what a radar spoke's drill-down carries, so a reader is not promised behaviour the code does not have. Two supporting changes keep the docs honest: the stopped-branch comment in `scripts/task-viewer.sh` stops claiming the text is identical, and a launcher assertion pins what each branch prints.

## Context

**Status lines.** The launcher paragraph in `FEATURES.md` says: "`status` exits 0 while the server runs and 1 while it does not, so a script can branch on it the way it would on `systemctl is-active`, while the two lines it prints are the same either way." The launcher bullet in `TESTING.md` repeats "printing the same lines either way". The code prints two lines while running and one while stopped:

```bash
if is_running; then
    echo "task-viewer is running (PID: $(get_pid))"
    echo "Access at: http://127.0.0.1:$PORT"
else
    echo "task-viewer is not running"
```

(the `do_status` branches in `scripts/task-viewer.sh`). The comment in the stopped branch splits its false claim across two lines, `# ... The text` and `# stays the same; only the exit code carries the answer.` No test asserts either branch's full line set: the stopped line and the running branch's `task-viewer is running` header are matched only by containment in the launcher tests, and the `Access at:` line is pinned nowhere.

**Radar drill-down.** The chart-drill-down paragraph in `FEATURES.md` says "a radar spoke carries its kind and slice and no window". The code carries different fields by mode:

- All projects: `{ project: String(dot.repository_id), kind: kind }` — the spoke's repository, no slice, no page project.
- One project: `{ kind: kind, scope: dot.label }` — the page's project, kind, and scope.

(the radar spoke link builders in `dashboard.js`). The sentence is only true for the single-project mode, and a test written from it would assert the wrong contract for the default All-projects view.

## Approach

1. Rewrite the `FEATURES.md` status sentence to say the running branch prints the PID and the URL while the stopped branch prints one line, and that the exit code carries the answer; phrase it so it stays true once the moved-checkout status output in [task-viewer_launcher-moved-checkout-ownership.md](../task-viewer_launcher-moved-checkout-ownership.md) lands and adds the binary path to the running output. Fix the two stopped-branch comment lines in `scripts/task-viewer.sh` to match.
2. Rewrite the `TESTING.md` launcher bullet's "printing the same lines either way" to state what each branch prints, and add an assertion for the running and stopped line sets in `launcher_tests.rs`. The bullet is co-edited: [task-viewer_launcher-foreign-pid-stop-test.md](../task-viewer_launcher-foreign-pid-stop-test.md), [task-viewer_launcher-port-conflict-test.md](../task-viewer_launcher-port-conflict-test.md), and [task-viewer_test-free-port-race.md](../task-viewer_test-free-port-race.md) append coverage to the same bullet, so this rewrite keeps their additions and each of those edits keeps this status clause.
3. Split the chart-drill-down radar sentence in `FEATURES.md`: with all projects selected a spoke carries its repository and kind; with one project selected it carries the page's project, kind, and scope; neither carries a window.
4. Leave the code paths unchanged.

**Out of scope:** making `status` print the same number of lines, and changing the drill-down links.

## Acceptance

- `rg 'same either way|same lines either way' FEATURES.md TESTING.md` returns nothing.
- `FEATURES.md` and `TESTING.md` each state what the running branch prints (the PID line and the `Access at:` URL) and that the stopped branch prints one line, with the exit code carrying the answer.
- `rg 'stays the same; only the exit code carries the answer' scripts/task-viewer.sh` returns nothing, and the stopped-branch comment describes the one-line output.
- `rg 'kind and slice' FEATURES.md` returns nothing, and the chart-drill-down paragraph names the All-projects spoke's repository-and-kind link and the single-project spoke's project, kind, and scope link separately, with neither carrying a window.
- The launcher test asserts each branch's lines, and `make test` passes.
