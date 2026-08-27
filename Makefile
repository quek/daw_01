.PHONY: help build run test test-nolaunch test-rt preflight-no-app clippy license-check audit clean release run-release fmt check fetch-ffmpeg fetch-ffmpeg-force ffmpeg-mirror worktree-rm worktree-rm-merged

# ライセンス検査スクリプト用の Python (stdlib のみ)。Windows の公式インストーラは
# `python`、Linux / macOS は `python3` が正なので、あるほうを使う。
# 明示したいときは `make license-check PYTHON=/usr/bin/python3.12`。
PYTHON ?= $(shell command -v python 2>/dev/null || command -v python3 2>/dev/null)

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
TEST_PKG_NAMES := common daw_gui daw_audio daw_plugin_host \
                  daw-ui-platform daw-ui-renderer daw-ui-core \
                  daw-ui-example-sample-edit-ops
TEST_PKGS := $(patsubst %,-p %,$(TEST_PKG_NAMES))
TEST_PKGS_NO_GUI := $(patsubst %,-p %,$(filter-out daw_gui,$(TEST_PKG_NAMES)))

# ---- daw_gui を起動しない test target (test-nolaunch 用) ----
# **手書きの列挙にしない。** 判定基準は 1 つだけ:
#   grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs
# これに当たる target は daw_gui 本体を `--script` で subprocess 起動し、daw_audio /
# daw_plugin_host まで spawn して audio device を開く。名前は基準ではない
# (pdc_real_vst3 / sidechain_real_vst3 は smoke が付かないのに起動し、arr_widget /
# pr_widget / font_picker は起動しない)。ここは grep -L (= 当たらない側) で反転して取る。
# 同じ基準を .claude/guards.jsonl の no-app-launching-test-target が列挙しており、
# scripts/test_guards.py の check_launching_targets_list() が両者のズレを検出する。
DAW_GUI_SAFE_TESTS := $(patsubst %,--test %,$(basename $(notdir \
    $(shell grep -L CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs 2>/dev/null))))
# **ディレクトリ形式の test target** (`tests/<name>/main.rs` = 複数モジュールを 1 バイナリに
# 統合したもの、現状 app_state) は上の glob に映らない。しかも target 名はファイル名では
# なく **ディレクトリ名** なので、単に glob を足すだけでは `main` という存在しない target を
# 渡してしまう。ここを落とすと `make test-nolaunch` が該当バイナリを**黙って丸ごと素通り**
# する (2026-08-27 に発覚: app_state の 94 件が一度も回っていなかった。`make test` は
# `--test` 列を渡さないので影響を受けず、差分に気付けなかった)。
# 判定基準は同じく CARGO_BIN_EXE_daw_gui だが、対象は main.rs 単体ではなく
# **ディレクトリ配下すべて** (サブモジュール側が起動しうる)。
DAW_GUI_SAFE_TESTS += $(shell for d in daw_gui/tests/*/; do \
    [ -f "$$d/main.rs" ] || continue; \
    grep -rq CARGO_BIN_EXE_daw_gui "$$d" || echo "--test $$(basename $$d)"; \
  done 2>/dev/null)

# ---- vendored FFmpeg (third_party/ffmpeg は gitignore、各マシンで fetch) ----
# ABI は avcodec-61 / avformat-61 / avutil-59 / swscale-8 / swresample-5 (= ffmpeg 7.1)
# を維持すること (vendored binding daw_gui/ffmpeg/binding_ffmpeg_7.1.rs と一致させるため)。
# 取得元の pin (URL / sha256) と取得ロジックは scripts/fetch_ffmpeg.sh が SSoT。
# ここに URL を二重化しない。ミラーの用意は scripts/prepare_ffmpeg_mirror.sh、
# 置き場所と手順は docs/ffmpeg_mirror.md。
# (取得先ディレクトリも script 側の既定。上書きは FFMPEG_DIR 環境変数で。)

help:
	@echo "daw_01 makefile targets (cargo ラッパー):"
	@echo ""
	@echo "  make build         実行 3 exe (daw_gui/daw_audio/daw_plugin_host) をビルド (debug)"
	@echo "  make run           daw_gui をビルド × 起動 (debug)"
	@echo "  make release       実行 3 exe (daw_gui/daw_audio/daw_plugin_host) を release ビルド"
	@echo "  make run-release   daw_gui をビルド × 起動 (release)"
	@echo "  make test          テストを持つ package のみ実行 (TEST_PKGS、#[test]0個の examples 等は除外)"
	@echo "  make test-rt       RT (audio thread) の無確保検査 (rt-assert feature、make test から呼ばれる)"
	@echo "  make clippy        clippy をエラー扱いで走らせる"
	@echo "  make license-check ライセンス表示の検査 (REUSE 準拠 / 依存の GPLv3 互換性)"
	@echo "  make audit         依存の脆弱性 / 供給網攻撃の検査 (network 要、cargo-deny 必須)"
	@echo "  make check         cargo check (ビルド不要、型検査のみ)"
	@echo "  make fmt           cargo fmt"
	@echo "  make fetch-ffmpeg  third_party/ffmpeg を取得 (無ければ DL、各マシン 1 回)"
	@echo "  make fetch-ffmpeg-force  third_party/ffmpeg を取り直す"
	@echo "  make ffmpeg-mirror ミラー用の成果物を dist/ffmpeg-mirror/ に用意 (上げはしない)"
	@echo "  make clean         target/ を削除"
	@echo "  make worktree-rm NAME=<name>   マージ済み worktree を安全に削除 (junction 安全 + ロック解除 + branch 削除)"
	@echo "  make worktree-rm-merged       マージ済み worktree を全部削除"

# third_party/ffmpeg を取得する (gitignore なので checkout では入らない)。
# 実体は scripts/fetch_ffmpeg.sh (URL 固定 + sha256 検証 + ミラーへのフォールバック)。
# avcodec.lib があれば skip (idempotent)。取り直しは make fetch-ffmpeg-force。
fetch-ffmpeg:
	@$(BASH) "$(CURDIR)/scripts/fetch_ffmpeg.sh"

# 既存の third_party/ffmpeg を取り直す (pin を上げたときなど)。
# 新しい方が展開・検証できてから入れ替えるので、失敗しても既存を壊さない。
fetch-ffmpeg-force:
	$(BASH) "$(CURDIR)/scripts/fetch_ffmpeg.sh" --force

# ミラー用の成果物 (BtbN バイナリ + 対応するソース) を dist/ffmpeg-mirror/ に用意する。
# **アップロードはしない**。手順は docs/ffmpeg_mirror.md。
ffmpeg-mirror:
	$(BASH) "$(CURDIR)/scripts/prepare_ffmpeg_mirror.sh"

build: fetch-ffmpeg
	cargo build $(RUN_PKGS)

run: preflight-no-app build
	cargo run -p daw_gui

release: fetch-ffmpeg
	cargo build --release $(RUN_PKGS)

run-release: preflight-no-app release
	cargo run -p daw_gui --release

# 実行系 target の前提条件。daw_gui が起動していたら明示エラーで止める
# (詳細と迂回方法は scripts/preflight_no_running_app.sh の冒頭コメント)。
preflight-no-app:
	@$(BASH) "$(CURDIR)/scripts/preflight_no_running_app.sh" "$(MAKECMDGOALS)"

# daw_gui/script を有効化して --script 系 smoke テスト (required-features 宣言済み) も
# 含めて全件回す。TEST_PKGS 以外 (#[test] 0 個の examples + ara-sys) はスキップする。
# build 依存は必須: script smoke は実 daw_gui.exe を spawn し、それが daw_audio.exe /
# daw_plugin_host.exe を子プロセス起動する。`cargo test` はこれら runtime バイナリの
# 生成を保証しない (テストハーネス版のみ) ので、クリーンな target では build なしだと
# 「daw_audio.exe が見つかりません」で落ちる (2026-07-03 の cargo clean 後に発覚)。
# preflight は **prerequisite に置く**。recipe の 1 行目に置くと build / test-rt が先に
# 走ってしまい、実機が動いている最中に 40 秒ビルドしてから止まる (2026-08-22 に実測)。
# build 自体も daw_gui 起動中は ERROR 5 で落ちうるので、先に止めるのが正しい。
test: preflight-no-app build test-rt
	cargo test $(TEST_PKGS) --features daw_gui/script

# 起動を伴わない検証だけを回す。`make test` が前提条件で止まる状況 (実機を触っている
# 最中) でも安全に通せる。対象 target は上の DAW_GUI_SAFE_TESTS が基準から導く。
test-nolaunch: test-rt
	cargo test $(TEST_PKGS_NO_GUI)
	cargo test -p daw_gui --features daw_gui/script --lib --bins $(DAW_GUI_SAFE_TESTS)

# RT (audio thread) の無確保検査。 `rt-assert` は非 default feature なので、
# 上の `test` の feature 集合ではテストが **コンパイルすらされない**。
# feature を `test` 側に足すのでは不十分: script smoke が spawn する
# daw_audio.exe は `make build` 産 (feature 無し) なので、別 target で
# daw_audio 単体を feature 付きで回す。
#
# 有効化されるもの:
# - `assert_no_alloc` の #[global_allocator] フック (Rust 側の確保を検出)
# - `signalsmith-sys/alloc-count` (vendored C++ エンジンの確保を検出。
#   Rust の allocator フックは C++ の確保を **一切見られない**)
test-rt:
	cargo test -p daw_audio --features rt-assert

# `--all-targets` = lib / bin に加えて **test / bench / example** も lint する。
# これが無いと `#[cfg(test)]` がコンパイルされず、テストコードの lint が
# ゲートを素通りする (2026-08-22 に発覚。実際 11 件が溜まっていた)。
# ビルドはするが**実行はしない**ので、daw_gui を起動する test target を持つ
# crate でもアプリは立ち上がらない。
clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# ライセンス表示の機械検査 (r.md #60)。clippy / arch-lint と同格の常設ゲート。
#   1. SPDX 式の評価器の自己検査 — ここが壊れると 3 が静かに false green になる
#   2. REUSE Specification 3.3 適合 (REUSE.toml の一括宣言が全ファイルを覆っているか、
#      一括宣言が先頭にあるか、第三者コードが個別宣言で覆われているか)
#   3. 依存クレートが deny.toml の allow で満たせるか + THIRD-PARTY-NOTICES.md の鮮度
# 1-3 は Python stdlib だけで動くので **どの環境でも必ず走る** (検査を skip して
# 「緑に見えるが表示が壊れている」状態を作らない)。公式ツール (reuse / cargo-deny) は
# 入っていれば追加で走らせる = より厳しい検査に上書きされることはあっても緩まない。
license-check:
	@[ -n "$(PYTHON)" ] || { echo "ERROR: python が見つかりません。make license-check PYTHON=/path/to/python3" >&2; exit 1; }
	"$(PYTHON)" scripts/dep_licenses.py --self-test
	"$(PYTHON)" scripts/reuse_lint.py
	"$(PYTHON)" scripts/dep_licenses.py --check
	@if command -v reuse >/dev/null 2>&1; then \
		echo "--- reuse lint ---"; reuse lint; \
	else \
		echo "note: reuse 未インストール (pipx install reuse) — 自前検査のみで続行"; \
	fi
	@if command -v cargo-deny >/dev/null 2>&1; then \
		echo "--- cargo deny check licenses ---"; cargo deny --all-features check licenses; \
	else \
		echo "note: cargo-deny 未インストール (cargo install --locked cargo-deny) — 自前検査のみで続行"; \
	fi

# 依存の脆弱性 / 供給網攻撃の検査 (r.md #60 追補)。**license-check とは分ける**
# (advisory DB の取得にネットワークが要る / 回す頻度が違う)。
#
# 2026-08-20、crates.io の arrayref 0.3.10 が汚染された (RUSTSEC-2026-0260)。typosquat の
# proc-macro1 への依存が足され、その build script が **コンパイル中にリモートのバイナリを
# 取得して実行**する。このリポジトリが無事だったのは Cargo.lock を commit していて迂闊な
# `cargo update` を走らせなかったからで、検査は存在しなかった。それを埋める。
#
# **cargo-deny が無ければ明示エラーで落とす。「未インストールにつき skip」の緑は作らない。**
# ライセンス検査は Python の自前実装が同じ不変条件を見ているので skip しても穴が開かないが、
# advisories には自前の代替が無い。semver range の判定を自前で書くと、間違えたときに
# 「緑に見えて素通し」= false green になる — 守ろうとしているものそのものを壊す。
# 範囲判定を要さない厳密な検査 (lock が追跡下か / manifest と同期しているか / 既知の汚染
# リリースが入っていないか) だけ scripts/lockfile_guard.py が **ネットワーク無しで必ず** 走る。
audit:
	@[ -n "$(PYTHON)" ] || { echo "ERROR: python が見つかりません。make audit PYTHON=/path/to/python3" >&2; exit 1; }
	"$(PYTHON)" scripts/lockfile_guard.py --self-test
	"$(PYTHON)" scripts/lockfile_guard.py
	@command -v cargo-deny >/dev/null 2>&1 || { 		echo "ERROR: cargo-deny が入っていません。advisories の検査は skip しません。" >&2; 		echo "       インストール: cargo install --locked cargo-deny" >&2; 		exit 1; 	}
	cargo deny --all-features check advisories

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
