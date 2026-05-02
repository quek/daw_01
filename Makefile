.PHONY: help build run test clippy clean release run-release fmt check

.DEFAULT_GOAL := run

help:
	@echo "daw_01 makefile targets (cargo ラッパー):"
	@echo ""
	@echo "  make build         workspace をビルド (debug)"
	@echo "  make run           daw_gui をビルド × 起動 (debug)"
	@echo "  make release       workspace を release でビルド"
	@echo "  make run-release   daw_gui をビルド × 起動 (release)"
	@echo "  make test          workspace 全テスト"
	@echo "  make clippy        clippy をエラー扱いで走らせる"
	@echo "  make check         cargo check (ビルド不要、型検査のみ)"
	@echo "  make fmt           cargo fmt"
	@echo "  make clean         target/ を削除"

build:
	cargo build --workspace

run: build
	cargo run -p daw_gui

release:
	cargo build --workspace --release

run-release: release
	cargo run -p daw_gui --release

test:
	cargo test --workspace

clippy:
	cargo clippy --workspace -- -D warnings

check:
	cargo check --workspace

fmt:
	cargo fmt --all

clean:
	cargo clean
