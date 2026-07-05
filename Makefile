.PHONY: help build run test clippy clean release run-release fmt check fetch-ffmpeg worktree-rm worktree-rm-merged

.DEFAULT_GOAL := release

# --- TMP/TEMP の書き戻し (Claude Code の Git Bash → MSYS2 make 対策、2026-07-03 発覚) ---
# Git for Windows の bash から MSYS2 の make を起動すると、msys-2.0.dll ランタイム差で
# recipe 環境が HOME/MSYSTEM/PATH/SYSTEMDRIVE/SYSTEMROOT/TERM/WINDIR の 7 変数まで scrub
# され TMP/TEMP が消える。native な cargo / テスト exe は GetTempPath が SYSTEMROOT
# (C:\WINDOWS) へ fallback し、tempfile を使うテスト 51 件が PermissionDenied で全滅する。
# 欠落時のみ workspace 内の target/tmp を割り当てて export する (user profile の推測は
# しない — この文脈の HOME は /home/<user> = C:\msys64\home\<user> で実在しない)。
# 通常のシェルでは TMP 定義済みなので素通り、Linux は SYSTEMROOT が無いのでブロックごと
# 素通り。MSYS2 sh は native 子プロセスへの exec 時に TMP/TEMP を Windows 形式へ自動変換する。
ifdef SYSTEMROOT
ifeq ($(origin TMP),undefined)
TMP := $(CURDIR)/target/tmp
TEMP := $(TMP)
export TMP TEMP
$(shell mkdir -p "$(TMP)")
endif
endif

# 実行に必要な 3 つの exe (= runtime product)。ui/crates/examples/* (daw-ui-example-*) は
# 実行に不要なので build / run / release / run-release では作らない (FIXME #65)。examples も
# コンパイル検証したい clippy / check は --workspace のまま残す。
RUN_PKGS := -p daw_gui -p daw_audio -p daw_plugin_host

# `cargo test` は #[test] が 0 個の [[bin]] target でもビルド + リンクを必ず行う。
# ui/crates/examples/* は winit/wgpu 一式に依存する手動デモで、#[test] を持つのは
# sample_edit_ops のみ (他は自動テスト 0、check/clippy の --workspace が引き続き
# コンパイル検証を担う)。実際にテストを持つ package だけを明示列挙する。
# common / daw-ui-platform / daw-ui-renderer は RUN_PKGS には無いが実テストを持つので必須
# (欠かすとカバレッジが静かに落ちる)。ara-sys は #[test] 0 個なので対象外。
# 新規 member 追加/初めて #[test] を足すときはこの列挙も更新すること。
TEST_PKGS := -p common -p daw_gui -p daw_audio -p daw_plugin_host \
             -p daw-ui-platform -p daw-ui-renderer -p daw-ui-core \
             -p daw-ui-example-sample-edit-ops

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
	@echo "  make build         実行 3 exe (daw_gui/daw_audio/daw_plugin_host) をビルド (debug)"
	@echo "  make run           daw_gui をビルド × 起動 (debug)"
	@echo "  make release       実行 3 exe (daw_gui/daw_audio/daw_plugin_host) を release ビルド"
	@echo "  make run-release   daw_gui をビルド × 起動 (release)"
	@echo "  make test          テストを持つ package のみ実行 (TEST_PKGS、#[test]0個の examples 等は除外)"
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
	cargo build $(RUN_PKGS)

run: build
	cargo run -p daw_gui

release: fetch-ffmpeg
	cargo build --release $(RUN_PKGS)

run-release: release
	cargo run -p daw_gui --release

# daw_gui/script を有効化して --script 系 smoke テスト (required-features 宣言済み) も
# 含めて全件回す。TEST_PKGS 以外 (#[test] 0 個の examples + ara-sys) はスキップする。
# build 依存は必須: script smoke は実 daw_gui.exe を spawn し、それが daw_audio.exe /
# daw_plugin_host.exe を子プロセス起動する。`cargo test` はこれら runtime バイナリの
# 生成を保証しない (テストハーネス版のみ) ので、クリーンな target では build なしだと
# 「daw_audio.exe が見つかりません」で落ちる (2026-07-03 の cargo clean 後に発覚)。
test: build
	cargo test $(TEST_PKGS) --features daw_gui/script

clippy:
	cargo clippy --workspace -- -D warnings

# アーキテクチャ不変条件の機械検査 (CLAUDE.md「アーキテクチャ不変条件」/
# docs/plan_arch_refactor.md §11)。違反は列挙のみ (exit 0)。
# CI / commit ゲートでは ARCH_LINT_STRICT=1 を付ける。
arch-lint:
	/usr/bin/bash scripts/arch_lint.sh

check: fetch-ffmpeg
	cargo check --workspace

fmt:
	cargo fmt --all

clean:
	cargo clean

# cleanup_worktree.sh は「bash の絶対パス」+「script の絶対パス」で起動する。理由 (2026-06-21):
#   素の cmd.exe では PATH 上の最初の `bash` が WSL の C:\Windows\System32\bash.exe に解決される
#   (System PATH が User PATH の MSYS2 より先、Git は cmd\ に bash を持たない)。recipe を裸の
#   `bash scripts/...` で書くと WSL bash が起動し、Linux FS 上で相対パスも /f/... も解決できず
#   "/bin/bash: scripts/cleanup_worktree.sh: No such file" (Error 127) で落ちる。
#   そこで PATH 経由の語 `bash` を使わず、make 自身の runtime が解決する実 bash を絶対パスで指す
#   ($(BASH))。Windows では MSYS2 の bash、Linux では system bash (/usr/bin/bash) になる。script は bash 必須
#   (BASH_SOURCE + `< <(...)` プロセス置換)。script は ARG で渡す (PATH/shebang 経由でないので
#   ここでも WSL に逸れない)。CURDIR は make 自身の cwd で常に正しいので絶対パス化に使う。
# 削除は明示・手動のみ。git hook には決して繋がない ([[feedback_no_auto_worktree_delete]]、
# script ヘッダの "deliberately NOT wired into a git hook" 参照)。
BASH := /usr/bin/bash
CLEANUP_WT := $(CURDIR)/scripts/cleanup_worktree.sh

# マージ済み worktree を安全に削除する (vendored ffmpeg を巻き込まず、rust-analyzer /
# daw exe のロックを外し、git worktree 解除 + branch 削除まで一括)。手動で消したいときだけ使う。
# 使い方: make worktree-rm NAME=fixme-64-...   (未マージ/dirty は拒否。FORCE=1 で強制)
worktree-rm:
	@[ -n "$(NAME)" ] || { echo "usage: make worktree-rm NAME=<worktree-name> [FORCE=1]"; exit 1; }
	$(BASH) "$(CLEANUP_WT)" --name "$(NAME)" $(if $(FORCE),--force,)

# マージ済み worktree を全部削除する。判定 (branch_merged_into_main): git cherry main
# branch が '+' 行を出さない = branch 固有の非マージコミットが全て main に patch-id 一致
# (squash/rebase/ff/通常 merge を網羅)。作業をコミットして revert しただけの net-zero
# ブランチ (固有コミットが '+' で残る) は誤削除しない。tip == main HEAD のマージ完了
# worktree (`git push . branch:main` で feature tip がそのまま main HEAD になる統合フローの
# 結果) も削除対象 — これが「マージしたのに消えない」の正体だった。未保存/dirty/locked は
# remove_one のガードが守る。さらに git 登録が外れた空ディレクトリ
# (.claude/worktrees/<dir>) も掃除する (prune_orphan_dirs)。
worktree-rm-merged:
	$(BASH) "$(CLEANUP_WT)" --all
