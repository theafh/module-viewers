.PHONY: build run start stop status restart test lint format fmt fix audit install install-desktop

TEST_RUSTFLAGS=-D warnings
LAUNCHER=./scripts/task-viewer.sh

# Force the repo-local target dir even when the shell exports another
# CARGO_TARGET_DIR (Cursor agent sandboxes often do). The Dock launcher runs
# $(CURDIR)/target/release/task-viewer only.
build:
	CARGO_TARGET_DIR="$(CURDIR)/target" cargo build --release -p task-viewer
	@ls -la "$(CURDIR)/target/release/task-viewer"
	@echo "Release binary: $(CURDIR)/target/release/task-viewer"

run:
	CARGO_TARGET_DIR="$(CURDIR)/target" cargo run -p task-viewer

start:
	$(LAUNCHER) start

stop:
	$(LAUNCHER) stop

status:
	$(LAUNCHER) status

restart:
	$(LAUNCHER) restart

test:
	RUSTFLAGS="$(TEST_RUSTFLAGS)" CARGO_TARGET_DIR="$(CURDIR)/target" cargo test -p task-viewer

lint:
	CARGO_TARGET_DIR="$(CURDIR)/target" cargo clippy -p task-viewer --all-targets -- -D warnings

format fmt:
	cargo fmt -p task-viewer

fix:
	CARGO_TARGET_DIR="$(CURDIR)/target" cargo clippy -p task-viewer --all-targets --fix --allow-dirty -- -D warnings
	cargo fmt -p task-viewer

audit:
	cargo audit

install install-desktop:
	./scripts/install-desktop.sh
