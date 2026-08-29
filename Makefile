.PHONY: help build run test test-nolaunch test-rt preflight-no-app clippy license-check audit clean release run-release fmt check fetch-ffmpeg fetch-ffmpeg-force ffmpeg-mirror worktree-rm worktree-rm-merged

# ライセンス検査スクリプト用の Python (stdlib のみ)。Windows の公式インストーラは
# `python`、Linux / macOS は `python3` が正なので、あるほうを使う。
# 明示したいときは `make license-check PYTHON=/usr/bin/python3.12`。
PYTHON ?= $(shell command -v python 2>/dev/null || command -v python3 2>/dev/null)

.DEFAULT_GOAL := release

# =====================================================================================
# Windows のログオン環境の復元 (r.md #81)
# 2026-07-03 に TMP/TEMP だけを埋める版で発覚、2026-08-29 に全面的に作り直した。
# =====================================================================================
#
# 【症状】Git for Windows の bash から MSYS2 の make を起動すると、msys-2.0.dll の
# ランタイム差で **POSIX 環境変数が 1 つも継承されない**。値ごと渡るのは Win32 側の
# 強制セット (PATH / SYSTEMDRIVE / SYSTEMROOT / WINDIR / MSYSTEM) だけで、HOME と TERM は
# 受信側ランタイムの合成値 (実測: TERM=BOGUS-TERM を渡しても xterm-256color、
# HOME=/tmp/bogus を渡しても /home/<user>)。recipe の native 子プロセス (cargo / rustc /
# daw exe / プラグイン DLL / GPU ドライバ) が見るのは、make と sh が足した
# MAKEFLAGS / MAKELEVEL / MFLAGS / PWD / SHLVL / _ を含めて 13 変数しかない。
# **「7 変数まで scrub される」という以前の記述は測定器由来の誤り**だった — recipe の中で
# PATH 解決される `env` は Git for Windows 側のバイナリで、それを呼ぶこと自体が 2 度目の
# cross-runtime exec になっていた (下の【検証のしかた】)。
#
# 【実害】すべて実測で確認済み。
#   - GPU ドライバ (nvwgf2umx.dll) が %ProgramData% を解決できず、cwd に
#     `NVIDIA Corporation/umdlogs` を作る。main + worktree で 9 箇所に堆積していた。
#     この復元ブロックの有無だけを変えた A/B で、出る / 出ないが再現する。
#   - Python の os.path.expanduser("~") がリテラル `~` を返す (ntpath は HOME を見ず、
#     USERPROFILE → HOMEDRIVE+HOMEPATH の順に見るため)。scripts/test_guards.py は
#     Git Bash から FAIL 0、make の recipe からは FAIL 27 だった。
#     解決不能な `~` を含むパスに書きに行くと、**リポジトリ内にリテラル `~` ディレクトリ**が
#     生える (`daw_guitestsfixtures/` と同じ種類のゴミ)。
#   - recipe 内の git がユーザーの ~/.gitconfig を丸ごと見失う (`git config --get user.name`
#     が空)。identity だけでなく core.excludesFile / alias / credential helper / safe.directory
#     も全部消える。scripts/cleanup_worktree.sh は make から起動され、その中で
#     git worktree remove / git branch -D を回している。
#   - GetTempPath が SYSTEMROOT (C:\WINDOWS) へ fallback し、tempfile を使うテスト 51 件が
#     PermissionDenied で全滅する (2026-07-03 の元症状)。
#
# 【復元の権威は known folder API ただ 1 つ】`cygpath -w -F <CSIDL>` は SHGetFolderPath を
# 叩くので env を一切参照せず、scrub 済みの環境でも正しい値を返す (実測)。
# SHGetFolderPath は Vista 以降 SHGetKnownFolderPath の薄いラッパなので、
# common/src/app_dirs.rs が使う dirs crate と同じ権威に行き着く。
# **レジストリからは取らない** — HKLM の Session Manager\Environment は live session と
# 食い違う値を持つ (実測: USERNAME=SYSTEM / TEMP=%SystemRoot%\TEMP)。
#
# 【書き戻さないもの】USERNAME / USERDOMAIN / LOGONSERVER / COMPUTERNAME / SESSIONNAME /
# NUMBER_OF_PROCESSORS / PROCESSOR_* / OS / PATHEXT。これらの権威は known folder API では
# なく GetUserNameW / GetComputerNameW / GetSystemInfo 等の別 API にあり、env が無ければ
# 読む側がその API へ落ちられる。**間違った値を入れるのは未定義のままより悪い** —
# 未定義なら呼び出し側が API にフォールバックするが、誤値は静かに間違った答えになる。
#
# 【大文字小文字の罠】Windows の env は case-insensitive だが make は case-sensitive で、
# しかも make に届く綴りは起動経路依存 (実測: 同じマシンでも PROGRAMFILES は全大文字なのに
# ProgramData / ProgramFiles(x86) は混在綴り)。だから「在るか」は候補綴りを全部見る。
# 1 綴りしか見ないと、健全な環境で綴り違いの二重登録を作る。
# 以前あった「括弧付きの名前は $(origin ...) でガードできない」という診断は**誤り**で、
# $(origin ProgramFiles(x86)) は正しく environment を返す。壊れていたのは
# $(origin PROGRAMFILES(X86)) = 綴り違いのほうで、括弧は無関係だった。
# ただし参照だけは $(ProgramFiles(x86)) では取れないので $(value ...) を使う。
#
# 【検証のしかた】**printenv / env で検証しないこと。** PATH 上の env は Git for Windows 側の
# バイナリで、MSYS2 make から見ると cross-runtime の子プロセスなので、自分が失った環境を
# 正直に報告してしまう (= 測定器が答えを作る)。確認は必ず **native プロセス**で行う:
#   make <target> のかわりに recipe へ  @python -c "import os;print(os.environ.get('LOCALAPPDATA'))"
#
# 【Linux / macOS】SYSTEMROOT が無いのでブロックごと展開されず、cygpath も走らない。
#
# 【コスト】実測 (5 回平均、make 起動込み)。健全な環境では $(shell) が 1 回も走らない。
#   健全 (cmd 経由)    : 75ms → 77ms   (+2ms、実質 no-op かつ値も不変 = 冪等)
#   scrub 済み (Git Bash): 66ms → 約 450ms  (cygpath 1 回あたり約 45ms × 10)
ifdef SYSTEMROOT

# 候補綴りのどれかが「未定義でない」なら在る。command line / override も尊重するので
# `make LOCALAPPDATA=D:\x` を勝手に上書きしない。
win_env_have = $(strip $(foreach n,$(1),$(filter-out undefined,$(origin $(n)))))

# 「環境が MSYS2 に作り直された」印。HOME の判定に使う (下記) ので、USERPROFILE を
# 再構成する **前に** 取っておく。
_WIN_ENV_USERPROFILE_WAS_MISSING := $(if $(call win_env_have,USERPROFILE),,1)

# CSIDL 1 個につき cygpath は 1 回だけ。ALLUSERSPROFILE と ProgramData のように
# **同じ known folder を指す別名**が 3 組あるので、値の出どころを 1 つにまとめる (SSoT)。
win_kf = $(if $(_WIN_KF_$(1)),,$(eval _WIN_KF_$(1) := $(shell cygpath -w -F $(1))))$(_WIN_KF_$(1))

# $(1)=書き戻す綴り / $(2)=CSIDL / $(3)=同義の綴り (空可)
define win_env_folder
ifeq ($$(call win_env_have,$(1) $(3)),)
$(1) := $$(call win_kf,$(2))
export $(1)
endif
endef

# CSIDL は MS 公式の env ↔ CSIDL 対応 (USMT の Recognized environment variables) どおり。
# 値はこのマシンで実測して一致を確認済み。
$(eval $(call win_env_folder,APPDATA,26,))
$(eval $(call win_env_folder,LOCALAPPDATA,28,))
$(eval $(call win_env_folder,USERPROFILE,40,))
$(eval $(call win_env_folder,ProgramData,35,PROGRAMDATA))
$(eval $(call win_env_folder,ALLUSERSPROFILE,35,))
$(eval $(call win_env_folder,PROGRAMFILES,38,ProgramFiles))
$(eval $(call win_env_folder,ProgramW6432,38,PROGRAMW6432))
$(eval $(call win_env_folder,ProgramFiles(x86),42,PROGRAMFILES(X86)))
$(eval $(call win_env_folder,COMMONPROGRAMFILES,43,CommonProgramFiles))
$(eval $(call win_env_folder,CommonProgramW6432,43,COMMONPROGRAMW6432))
$(eval $(call win_env_folder,CommonProgramFiles(x86),44,COMMONPROGRAMFILES(X86)))

# PUBLIC には CSIDL が無い (KNOWNFOLDERID には FOLDERID_Public として在る)。
# CSIDL_COMMON_DESKTOPDIRECTORY(25) の親から取る。**葉の名前 (Desktop) を書かずに**
# POSIX 形式の親を切るので、プロファイル置き場が既定以外でも、空白入りでも壊れない。
ifeq ($(call win_env_have,PUBLIC Public),)
PUBLIC := $(shell p=$$(cygpath -u -F 25); cygpath -w "$${p%/*}")
export PUBLIC
endif

# 以下は known folder から **推測ゼロで導出**できるもの。追加の API 問い合わせは要らない。
# USERPROFILE が取れなかったときに HOMEDRIVE=`:` のような誤値を作らないよう、非空のときだけ
# 導出する (誤値は未定義より悪い)。取れなかった事実は下のカナリアが USERPROFILE 側で捕まえる。
ifneq ($(USERPROFILE),)
ifeq ($(call win_env_have,HOMEDRIVE),)
HOMEDRIVE := $(firstword $(subst :, ,$(USERPROFILE))):
export HOMEDRIVE
endif
ifeq ($(call win_env_have,HOMEPATH),)
HOMEPATH := $(subst $(HOMEDRIVE),,$(USERPROFILE))
export HOMEPATH
endif
endif
ifeq ($(call win_env_have,COMSPEC ComSpec),)
COMSPEC := $(SYSTEMROOT)\system32\cmd.exe
export COMSPEC
endif

# HOME は「未定義」ではなく MSYS2 が /etc から作り直した値 (C:\msys64\home\<user>) が
# 入っているので origin では判定できない。**USERPROFILE を再構成したとき = 環境が
# 作り直されたと分かったときだけ**、同じ根拠で HOME も戻す。健全な環境では、ユーザーが
# HKCU\Environment に置いた HOME を尊重して触らない。
# **POSIX 形式でなければならない** — Windows パスを sh の HOME に入れると `cd ~` が壊れる。
# MSYS2 は native 子プロセスへの exec 時に HOME を Windows 形式へ自動変換するので、
# native 側は C:\Users\<user> を受け取る (実測)。
# 実測 (before → after): `cd ~` /home/ancient → /c/Users/ancient、
# `git config --get user.name` 空 → 'Tahara Yoshinori'、native HOME
# C:\msys64\home\ancient → C:\Users\ancient、expanduser('~') `~` → C:\Users\ancient。
ifeq ($(_WIN_ENV_USERPROFILE_WAS_MISSING),1)
HOME := $(shell cygpath -u -F 40)
export HOME
endif

# TMP / TEMP は **復元ではない**。known folder API に「ユーザーの一時ディレクトリ」に
# 相当する権威が無く、実値は HKCU\Environment にしか無いので、上の「推測で書かない」方針
# では再構成できない。かといって欠落のまま native 子プロセスへ渡すと GetTempPath が
# SYSTEMROOT へ落ちて書き込み不能になる。よってここは**方針として** checkout 内の
# target/tmp を割り当てる:
#   - checkout ごとに隔離される (worktree を並列に回しても一時ファイルが混ざらない)
#   - make clean (= cargo clean) が掃除する
# 以前ここに「未保存インポートの実データが落ちて make clean が黙って消す」という実害が
# あったが、それは TMP の指す先ではなく LOCALAPPDATA が無いことが原因で、daw_gui が
# common::app_dirs (= SHGetKnownFolderPath) 経由になった今は起きない。
ifeq ($(origin TMP),undefined)
TMP := $(CURDIR)/target/tmp
TEMP := $(TMP)
export TMP TEMP
$(shell mkdir -p "$(TMP)")
endif

# --- カナリア ---
# 「復元ブロックが動いていない」状態は、今と同じく **無症状で何か月も続く**
# (guards.jsonl 消失は 5 日、arch-lint の backslash 欠落は数か月、どちらも症状ゼロだった)。
# だから復元セットが埋まらなかったら **落とす**。cargo-deny / arch-lint と同じ
# 「skip の緑を作らない」原則。
# **置き場所は parse time でなければならない。** recipe の先頭に置く案は、canary を持たない
# target (help / clean / fmt / worktree-rm) を素通りさせる。ここが唯一漏れない位置。
# 空値を export したまま進むのは未定義より悪い (Rust の var_os は Some("") を返すので
# fallback が働かず、空パスへ連結しに行く) ので、空も「欠落」として扱う。
WIN_ENV_REQUIRED := APPDATA LOCALAPPDATA USERPROFILE ProgramData ALLUSERSPROFILE \
                    PROGRAMFILES ProgramW6432 ProgramFiles(x86) \
                    COMMONPROGRAMFILES CommonProgramW6432 CommonProgramFiles(x86) \
                    PUBLIC HOMEDRIVE HOMEPATH COMSPEC TMP TEMP
WIN_ENV_MISSING := $(strip $(foreach v,$(WIN_ENV_REQUIRED),$(if $(value $(v)),,$(v))))
ifneq ($(WIN_ENV_MISSING),)
$(error Windows 環境の復元に失敗しました。空のまま残った変数: $(WIN_ENV_MISSING) -- cygpath は PATH にありますか (command -v cygpath)。空の環境変数は未定義より危険なので停止します)
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
# docs/plan_arch_refactor.md §11)。**exit 0 = 「違反ゼロ、または
# scripts/arch_lint_baseline.txt に記録済みのものだけ」** — baseline に無い違反が
# 1 件でもあれば exit 1 (行単位 ratchet)。ARCH_LINT_STRICT=1 は baseline 済みの負債も落とす。
# サイズ budget (FILE-BUDGET / FN-BUDGET / FN-NESTING) と行分類 (コメント内の言及を
# 違反に数えない判定) は scripts/loc_budget.py が持つので **python が要る**。
# **python が無い / 壊れている (Windows Store のスタブ等) と arch-lint は全面停止する** —
# サイズ budget だけでなく RT-INFINITE / POSITIONAL-KEY / LEGACY-PROTOCOL / UNTAGGED /
# BLOB-IN-PROTOCOL / COMMON-DEPS / UI-DOMAIN も止まる。cargo-deny (上の audit) と同じ
# 「skip の緑を作らない」原則で、これは意図した挙動。
# 検出と self-test は script 側が持つ (直接 bash で叩く経路 = /arch-review skill でも同じ
# 保証が要るため)。ここは Makefile 冒頭で解決済みの PYTHON を渡すだけ。
arch-lint:
	PYTHON="$(PYTHON)" /usr/bin/bash scripts/arch_lint.sh

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
