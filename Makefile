.PHONY: help build run test clippy clean release run-release fmt check fetch-ffmpeg worktree-rm worktree-rm-merged

.DEFAULT_GOAL := release

# 実行に必要な 3 つの exe (= runtime product)。ui/crates/examples/* (daw-ui-example-*) は
# 実行に不要なので release では作らない (FIXME #65)。examples もコンパイル検証したい
# test / clippy / check は --workspace のまま残す。
RUN_PKGS := -p daw_gui -p daw_audio -p daw_plugin_host

# ---- vendored FFmpeg (third_party/ffmpeg は gitignore、各マシンで fetch) ----
# ABI は avcodec-61 / avformat-61 / avutil-59 / swscale-8 / swresample-5 (= ffmpeg 7.1)
# を維持すること (vendored binding daw_gui/ffmpeg/binding_ffmpeg_7.1.rs と一致させるため)。
# BtbN は asset 名に版サフィックスを付け替えるので URL は固定せず latest リリースの
# asset 一覧から n7.1 win64 LGPL shared を探して取得する。
FFMPEG_DIR := third_party/ffmpeg
FFMPEG_API := https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/tags/latest
FFMPEG_MATCH := n7\.1.*win64-lgpl-shared

help:
	@echo "daw_01 makefile targets (cargo ラッパー):"
	@echo ""
	@echo "  make build         workspace をビルド (debug)"
	@echo "  make run           daw_gui をビルド × 起動 (debug)"
	@echo "  make release       実行 3 exe (daw_gui/daw_audio/daw_plugin_host) を release ビルド"
	@echo "  make run-release   daw_gui をビルド × 起動 (release)"
	@echo "  make test          workspace 全テスト"
	@echo "  make clippy        clippy をエラー扱いで走らせる"
	@echo "  make check         cargo check (ビルド不要、型検査のみ)"
	@echo "  make fmt           cargo fmt"
	@echo "  make fetch-ffmpeg  third_party/ffmpeg を取得 (無ければ DL、各マシン 1 回)"
	@echo "  make clean         target/ を削除"
	@echo "  make worktree-rm NAME=<name>   マージ済み worktree を安全に削除 (junction 安全 + ロック解除 + branch 削除)"
	@echo "  make worktree-rm-merged       マージ済み worktree を全部削除"

# third_party/ffmpeg を取得する (gitignore なので checkout では入らない)。
# avcodec.lib があれば skip (idempotent)。再取得は: rm -rf third_party/ffmpeg && make fetch-ffmpeg
fetch-ffmpeg:
	@if [ -f "$(FFMPEG_DIR)/lib/avcodec.lib" ]; then \
		echo "FFmpeg present: $(FFMPEG_DIR)"; \
	else \
		set -e; \
		echo "Resolving asset from $(FFMPEG_API)"; \
		url=$$(curl -fsSL "$(FFMPEG_API)" | grep -oE 'https://[^"]+\.zip' | grep -iE '$(FFMPEG_MATCH)' | head -n1); \
		[ -n "$$url" ] || { echo "ERROR: n7.1 win64 lgpl-shared asset not found in BtbN latest release"; exit 1; }; \
		tmp="$(FFMPEG_DIR)_dl_tmp"; \
		rm -rf "$$tmp"; mkdir -p "$$tmp"; \
		echo "Downloading $$url"; \
		curl -fL -o "$$tmp/ffmpeg.zip" "$$url"; \
		echo "Extracting"; \
		unzip -q "$$tmp/ffmpeg.zip" -d "$$tmp"; \
		inner=$$(find "$$tmp" -maxdepth 1 -type d -name 'ffmpeg-*' | head -n1); \
		[ -n "$$inner" ] || { echo "ERROR: extracted ffmpeg-* folder not found"; exit 1; }; \
		rm -rf "$(FFMPEG_DIR)"; \
		mkdir -p "$(FFMPEG_DIR)"; \
		cp -r "$$inner/bin" "$$inner/lib" "$$inner/include" "$(FFMPEG_DIR)/"; \
		rm -rf "$$tmp"; \
		[ -f "$(FFMPEG_DIR)/lib/avcodec.lib" ] || { echo "ERROR: avcodec.lib missing after fetch"; exit 1; }; \
		echo "FFmpeg fetched into $(FFMPEG_DIR)"; \
	fi

build: fetch-ffmpeg
	cargo build --workspace

run: build
	cargo run -p daw_gui

release: fetch-ffmpeg
	cargo build --release $(RUN_PKGS)

run-release: release
	cargo run -p daw_gui --release

test: fetch-ffmpeg
	cargo test --workspace

clippy:
	cargo clippy --workspace -- -D warnings

check: fetch-ffmpeg
	cargo check --workspace

fmt:
	cargo fmt --all

clean:
	cargo clean

# マージ済み worktree を安全に削除する (junction を辿って vendored ffmpeg を消す事故を防ぎ、
# rust-analyzer / daw exe のロックを外し、git worktree 解除 + branch 削除まで一括)。
# マージ時は .githooks が自動でこれを呼ぶ。手動で消したいときだけ使う。
# 使い方: make worktree-rm NAME=fixme-64-...   (未マージ/dirty は拒否。FORCE=1 で強制)
worktree-rm:
	@[ -n "$(NAME)" ] || { echo "usage: make worktree-rm NAME=<worktree-name> [FORCE=1]"; exit 1; }
	bash scripts/cleanup_worktree.sh --name "$(NAME)" $(if $(FORCE),--force,)

# マージ済み (自分の commit が全部 main に入っている) worktree を全部削除する。
worktree-rm-merged:
	bash scripts/cleanup_worktree.sh --all
