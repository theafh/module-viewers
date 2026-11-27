---
description: "Stop reporting a moved checkout's running server as stopped and as another process, and never claim a stop while the server still serves the port."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-27T20:29:50Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Report a moved checkout's server truthfully

## Goal

When a `task-viewer` release binary is listening on the port but its argv no longer resolves to this checkout's `$BINARY` because the checkout was moved or renamed, `status` reports it running with the path it was started from, `start` names it as a task-viewer server from that path instead of "another process", and `stop` never exits 0 while the PID file or the port tools still name the server on the port.

## Context

`is_ours` recognises a process only when its resolved argv equals this checkout's binary:

```bash
[ "$resolved" = "$BINARY" ]
```

(`scripts/task-viewer.sh`, the tail of `is_ours`). After a checkout move, the running server's argv still names the old absolute path, so:

- `is_running` treats the recorded PID as foreign and deletes the PID file in its foreign-PID branch;
- `do_status` prints `task-viewer is not running` and exits 1;
- `do_stop` prints `task-viewer is not running` and returns 0 from its early return;
- `start_binary` finds the port holder and prints `Port 7880 is held by another process (PID: N)` in its port-holder branch, naming the user's own server as a stranger.

The port keeps answering and there is no supported way to stop the server short of finding the PID by hand. `make install` does not help: the new icon's toggle hits the same "port held" refusal.

The checkout-only rule is written down in the guardrail docs and must move with the code. `SECURITY.md` states "The launcher signals only a PID whose command line is this checkout's release binary." `FEATURES.md`'s launcher bullet states that the launcher recognizes its own server across the printed forms and "signals only such a process", and its `status` sentence says "the running branch prints the PID line and the `Access at:` URL while the stopped branch prints one line, and the exit code carries the answer." `TESTING.md`'s launcher process-safety case requires that `is_ours` "rejects a path outside the checkout".

Other live tasks edit the same launcher test file and launcher regions, including [task-viewer_launcher-port-conflict-test.md](task-viewer_launcher-port-conflict-test.md), which keeps and tests `start_binary`'s port-holder refusal, and [task-viewer_launcher-foreign-pid-stop-test.md](task-viewer_launcher-foreign-pid-stop-test.md), which makes `do_stop`'s `is_ours` guards load-bearing. The foreign-PID stop test follows this task, because it targets the widened `is_ours` and the changed `do_stop`; the port-conflict task is the soft companion with no direction and may land either side. Every live `task-viewer_launcher-*` task that adds cases to `task-viewer/src/launcher_tests.rs` rebases its cases onto the shared driver.

## Approach

1. Widen `is_ours` in `scripts/task-viewer.sh` instead of adding a second predicate, so every existing call site keeps one ownership test. A live process is ours when its resolved argv is this checkout's `$BINARY` (today's rule, unchanged) or when its whole `ps` line is a task-viewer release binary at another path. Match that second class only when the line's last `/target/release/task-viewer` either ends the line or is followed by whitespace, and the text before that match — the prefix, taken as one span with any spaces it holds — is non-empty, ends on a non-whitespace character, has a path-like first whitespace-separated token (it starts with `/` or `.`, or contains `/`), and carries no later whitespace-separated token that starts with `/` or `-`. A token after the matched binary, such as `--serve`, is a trailing argument and is ignored. This keeps a checkout path holding a space intact and keeps a line whose argument merely names such a binary, such as `python3 /old/mv/target/release/task-viewer` or `/usr/bin/python3 /old/mv/target/release/task-viewer`, foreign. The old directory no longer exists after a move, so recover the started-from path as that whole prefix with the matched `/target/release/task-viewer` appended, verbatim, spaces included, lexically normalised, and canonicalize only when the directory still exists; a relative or symlinked argv of this checkout then compares equal to `$BINARY`. A line whose binary does not end in `/target/release/task-viewer`, such as `/somewhere/else/task-viewer`, stays foreign, so the existing `foreign-absolute` and `foreign-relative` fixtures keep their meaning. Rewrite the `is_ours` doc comment to state the two classes.
2. Keep every `is_ours` call site on the single widened test: `is_running`'s PID-file check and its port-holder adoption, `is_listening`, `do_stop`'s two kill guards and its post-`kill -9` survivor test, and `start_binary`'s failed-start cleanup. The survivor test is load-bearing: a moved-class process that survives `kill -9` must reach the `task-viewer did not stop` branch and exit non-zero rather than fall through to `task-viewer stopped`.
3. `is_running` now reports the moved class as running, so it keeps the PID file instead of deleting it. `do_status` prints a third line naming the binary path the running server was started from, only when that path differs from `$BINARY`, so a same-checkout server keeps its two lines.
4. `start_binary` names the path instead of calling the server a stranger. The already-running branch prints the recorded started-from path when it differs from `$BINARY`, and the restart branch's `A newer task-viewer binary exists; restarting on it.` message names the running server's path when it differs. The port-holder branch stays unreached for this class because `is_running` now reports it running.
5. `do_stop` keeps its control flow and gains the class through `is_ours`: it signals the process, reports success only after `is_running` is false, and exits non-zero naming the PID when the process survives, matching the existing foreign-PID survivor path.
6. Update the documents and comment that state the old rule: the `is_ours` doc comment, the `SECURITY.md` sentence naming the checkout's release binary, the `FEATURES.md` launcher bullet (its ownership sentence and its `status` sentence), the `README.md` launcher sentence that says the launcher stops only the release binary from this checkout, and the `TESTING.md` launcher process-safety case that requires rejecting a path outside the checkout.
7. Add launcher cases in `task-viewer/src/launcher_tests.rs` using a driver whose `ps` output names another checkout's release binary, one under a path holding a space, one whose `ps` line carries a trailing argument, and one under a path that no longer exists so the verbatim recovery is exercised: `status` exits 0 and names the path, `stop` signals it and reports stopped, a moved-class survivor of `kill -9` exits non-zero, and `start` reports the moved path rather than "another process".

The port is shared by every checkout, so a task-viewer binary on it is this app's server and the launcher's stop owns it. An unrelated listener stays untouched.

**Out of scope:** removing an installed Dock tile or `.desktop` entry, which [make uninstall](task-viewer_make-uninstall.md) owns, changing the checkout path the launcher embeds at install time, and identifying a listener on a machine with neither `lsof` nor `ss` and no PID file, where the launcher cannot know the process.

## Acceptance

- With a server started from a checkout that was then moved, `status` exits 0, names its PID, and names the binary path it runs as a third line; a server started from this checkout still prints its two lines.
- `stop` in that state stops the task-viewer process and exits 0 only after the process is gone; a process that survives `kill -9` exits non-zero and names it. It never prints `task-viewer is not running` while the PID file or the port tools name that binary on the port.
- `start` in that state never calls the server "another process"; both the already-running and the restart branch name the task-viewer binary and its path.
- A release binary at another path whose checkout name holds a space is recognized and its path is reported intact.
- A moved-class process whose `ps` line carries a trailing argument after its release binary path, such as `/old/mv/target/release/task-viewer --serve`, is recognized: `status` reports it running and `stop` reaches it.
- A genuinely unrelated listener on the port still gets the existing "held by another process" refusal and is never signalled, and a process whose binary does not end in `/target/release/task-viewer` stays foreign to `is_ours`, as does one at `…/target/release/task-viewer-backup`.
- `SECURITY.md`, `FEATURES.md`, `README.md`, and `TESTING.md` each state the widened ownership rule once, with the checkout-only sentence gone from each.
- The new launcher cases and `make test` pass.
