.PHONY: build run test lint format fmt fix audit install-desktop

TEST_RUSTFLAGS=-D warnings

build:
	CARGO_TARGET_DIR="$(CURDIR)/target" cargo build --release -p task-viewer

run:
	CARGO_TARGET_DIR="$(CURDIR)/target" cargo run -p task-viewer

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

install-desktop:
	./scripts/install-desktop.sh
