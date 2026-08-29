# daw_01

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。詳細は [DESIGN.md](DESIGN.md)。

## 大原則

理想とベストプラクティスを追求する。
そのためには実装コストは無視して大胆に破壊して作り直す。

## 禁止事項

大原則を守るために、**何を作るかの判断**に次を持ち込むことを禁止します。

- 実装コストで判断すること
- 実装難易度で判断すること
- 変更規模で判断すること
- コンパイルがとおらないことで判断すること
- 作業時間で判断すること

**順序と分け方は別**。統合順・並列 worktree の切り方・大改造の着手前確認は、規模と衝突の実測を
根拠に決めてよい (むしろ決めること)。禁じているのは「安いほうを**選ぶ**」ことであって、
「安全な順に**積む**」ことではない。→ [最終形まで実装する](#最終形まで実装する) /
[妥協を選択肢に上げない](#妥協を選択肢に上げない) が同じ規則の別の面。

## プロジェクト構成

Cargo workspace (Edition 2024)。実行時は独立した 3 プロセスが協調する。

```
common/            -- 共有型・IPC プロトコル・shared memory・データモデル
daw_gui/           -- GUI プロセス。Song ドキュメントの SSoT (daw-ui = winit + wgpu + 自作 UI)
daw_audio/         -- Audio Engine プロセス (CPAL)
daw_plugin_host/   -- Plugin Host プロセス (CLAP/VST3)
ui/                -- UI ライブラリ daw-ui (旧 sibling repo gui_01)。crates/{platform,renderer,ui}
```

daw-ui は同一 workspace・同一セッションで直接編集する。UI 固有の技術ガイド・既知の罠・
load-bearing invariant は [ui/CLAUDE.md](ui/CLAUDE.md)。

## Development Workflow

**Makefile が SSoT。素の `cargo build --workspace` / `cargo test --workspace` は直接使わない**
(ui/crates/examples/* の自動テスト0個の crate まで毎回フルビルド+リンクする無駄が生じる。
2026-07-03 に発覚: このセクションが素の cargo コマンドを指示していたせいで、Claude 自身も
毎回 `--workspace` を使い、Makefile 側の scoping 最適化が実質使われていなかった)。

```bash
make build       # 実行 3 exe (daw_gui/daw_audio/daw_plugin_host) をビルド (debug)
make run         # daw_gui をビルド × 起動 (Audio/Plugin プロセスを子プロセスとして起動)
make test        # テストを持つ package のみ実行 (TEST_PKGS、examples 等 #[test]0個は除外)
make test-nolaunch # 上のうち **daw_gui を起動しない target だけ**を実行 (下記)
make clippy      # clippy をエラー扱いで (--workspace、examples のコンパイル検証も兼ねる)
make check       # cargo check --workspace (型検査のみ、ビルド不要)
make arch-lint   # アーキテクチャ不変条件の機械検査 (下記「アーキテクチャ不変条件」節)
make license-check # ライセンス表示の機械検査 (REUSE 準拠 / 依存の GPLv3 互換性)
make audit       # 依存の脆弱性 / 供給網攻撃の検査 (network 要。下記「依存の脆弱性」節)
```

特定 crate/test だけを素早く確認したいときは `cargo check -p <crate>` /
`cargo test -p <crate> --test <name>` 等のピンポイント指定を使ってよい (Makefile の scoping と
矛盾しない、むしろ更に絞り込む方向)。避けるべきは `--workspace` の無条件多用。

### `make test` は daw_gui を起動する

`daw_gui/tests/` の一部は **daw_gui 本体を `--script` で subprocess 起動**し、それが daw_audio /
daw_plugin_host まで spawn して audio device を開く。窓を出さず single-instance gate も素通りする
ので **起動したことに誰も気付けない**。実機を触っている最中に回すと、開いているプロジェクトの
再生を壊す。

- **判定基準は 1 つだけ**: `grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs`。**名前で判断しない**
  — `pdc_real_vst3` / `sidechain_real_vst3` は smoke が付かないのに起動し、`arr_widget` /
  `pr_widget` / `font_picker` は起動しない。`--test` で名指ししても基準に当たる target なら起動する。
- **起動を伴わない検証だけなら `make test-nolaunch`** (対象は Makefile が上の基準から機械的に
  導く。手書きの列挙にしない)。許可を得て回すときは `DAW01_ALLOW_LAUNCH=1` を頭に付ける。
- `make test` / `make run` / `make run-release` は `scripts/preflight_no_running_app.sh` が前提条件で
  止める。**ユーザーが手で打っても効く**。迂回 (`DAW01_SKIP_PREFLIGHT=1`) と、プロセス一覧が
  取れない環境で緑に見せない設計は同スクリプト冒頭。
- 書く瞬間の block は `.claude/guards.jsonl` の `no-bulk-test-run` / `no-app-launching-test-target`。
  ガードの target 一覧と Makefile のズレは `scripts/test_guards.py` の
  `check_launching_targets_list()` が基準から再導出して検査する。

### 依存の脆弱性 / 供給網攻撃 (`make audit`)

**`Cargo.lock` を commit していることが供給網攻撃に対する一次防御**である。lock を追跡して
いなければ、上流が汚染された瞬間に次のビルドで取り込む。だから `cargo update` は
「更新したい理由があるとき」だけ意図的に打ち、打ったら必ず `make audit` を通す。

- `scripts/lockfile_guard.py` … **ネットワーク不要・常に走る**。lock が git 追跡下か /
  `cargo metadata --locked` が通るか / **既知の汚染リリースが完全一致で入っていないか**。
  新しい汚染事件を知ったら `KNOWN_COMPROMISED` に `(name, version, 出典)` を 1 行足す。
- `cargo deny --all-features check advisories` … **cargo-deny が無ければ `make audit` は明示
  エラーで落ちる。**「未インストールにつき skip」の緑は作らない — advisories には自前の代替が
  無く、semver range を自前実装すると false green になる (守ろうとしているものを壊す)。
- 方針は `deny.toml` の `[advisories]`。vulnerability / unsound / yanked は全部エラー、
  `unmaintained` は `"workspace"`。**`ignore` を足すときは必ず「RUSTSEC-ID: 理由 / 見直し期限」を
  コメントで書く。** 無言の ignore は禁止。

なぜ検査を足したか (2026-08-20 の **arrayref 0.3.10** 汚染 = RUSTSEC-2026-0260。daw_01 が無事
だったのは lock を commit していて `cargo update` を打っていなかったからで、運が良かっただけ) は
`scripts/lockfile_guard.py` 冒頭。指摘ごとの triage 記録は
[docs/dependency_audit.md](docs/dependency_audit.md)。

### vendored FFmpeg（fresh machine / 手動 worktree で必須）

- `third_party/ffmpeg`（BtbN n7.1 win64 LGPL shared）は **gitignore** で checkout には入らない。
  fresh なマシンでは **`make fetch-ffmpeg`**（idempotent。`build` / `test` / `check` の前提条件）。
- **取得は「latest から発見」ではなく URL + sha256 固定 + 自前ミラーへのフォールバック**。
  実装は `scripts/fetch_ffmpeg.sh`（pin の SSoT。Makefile に URL を二重化しない）。なぜ発見方式を
  捨てたか（起きたのは asset 名の変更ではなく **asset の消滅**で、発見方式は原理的に対応できない）、
  ミラーに置く「対応するソース一式」の GPL-3.0 §6(d) 上の義務、**アップロードを自動化しない**理由は
  [docs/ffmpeg_mirror.md](docs/ffmpeg_mirror.md) §1 / §4 / §5。
- **Claude Code の worktree は `.worktreeinclude`（`/third_party/`）で main checkout から
  実コピー**されるので `make fetch-ffmpeg` は不要（main checkout 自身が未取得なら先に取る）。
  手動 `git worktree add` はこの経路を通らないので従来どおり取得する。
- git に無いので **`rm` / `git worktree remove` が third_party junction を辿ると本体ごと消え、
  復元できない**（2026-06-14 に実際に起きた）。実コピーの worktree にこのハザードは無いが、手動で
  junction を張ったら消す前に `cmd //c rmdir <junction>` で外すこと。事故と復旧は
  [docs/ffmpeg_mirror.md](docs/ffmpeg_mirror.md) §6。

### ビルドと検証の区別（重要）

- `cargo clippy` / `cargo check` / `cargo test` は**実行バイナリを生成しない** or 生成しても
  テストビルドのみ。`./target/debug/daw_gui.exe` を走らせる前に、必ずビルドを明示する
- 子プロセス（daw_audio / daw_plugin_host）の挙動を変えたときは、子プロセスのバイナリも再生成が
  必要。**`make build`**（1 crate に閉じるなら `cargo build -p <crate>` まで絞ってよい）
- `cargo run -p daw_gui` は dependency crate も自動ビルドしてくれるが、既に起動中のプロセスの
  バイナリは上書きされない場合がある（Windows の ERROR 5）。必要なら先に全プロセスを終了

### IPC 境界で送る型

- protocol 型 (`AudioCommand` / `AudioEvent` / `PluginCommand` / `PluginEvent`)、および
  それが保持する内側の型すべてに `#[derive(bincode::Encode, bincode::Decode)]` が必要
- Song / Track / Clip / Row 等のモデル型も protocol 経由で渡すなら bincode derive を追加する
- 足したら **`make build`** で 3 exe を揃える。子 exe が古いと decode に失敗し、
  「再生が止まる」形で出る ([[feedback_workspace_build_for_protocol_changes]])

### daw-ui (旧 gui_01) の使い方

- **AppData は plain mutable struct**: Signal/Memo/derive は使わない。派生は method
  (`app.track_headers() -> Vec<TrackHeader>`) として毎フレーム計算し、重ければ view 側で 1 frame
  分キャッシュ
- **イベント dispatch**: view から `Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::X))`、
  background thread から `EventLoopProxy<AppEvent>::send_event`。`impl Model` 的な trait 接続は不要
- **immediate-mode + heavy() escape hatch**: 大量描画 (ピアノロール / アレンジ) は
  `ui.heavy(id, |hctx| hctx.cached(viewport_key, |hctx| { ... }))` の中で `push_rect` / `push_text` /
  `push_lines` / `push_edit` を呼ぶ (`Ui::push_edit` は `pub(crate)` なので、view から Edit を流すのは
  heavy ブロック内から)
- **背景スレッド** (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX synth / plugin DB
  rescan) は `std::thread` + `EventLoopProxy`。**`tokio::time::sleep` は使えない**
- **ダブルクリック検出は built-in が無い**。`AppData::last_click: Option<(Instant, x, y)>` に最終
  クリックを記録し、各 view の入力ハンドラで 400ms+5px 以内なら double 判定
- **UI のキーバインド・イベントは可視フィードバックが無いと動いたか判別不能**。迷ったら
  `AppData::handle_event` 冒頭に `tracing::info!(?event, "received")` を仕込んでログで確認し、
  確認後は削除するか debug feature で囲う (`[debug-gui]` / `[debug-ui]` skill)
- イベントループ (`daw_gui/src/view/runner.rs::Runner`)、ショートカット
  (`Runner::dispatch_shortcut`)、`WindowBackend` の配線 (上流の正準実装
  `daw_ui_platform::WinitWindow` を直接使う) は該当ファイルを読む。ライブラリ側の API・
  load-bearing invariant・既知の罠は [ui/CLAUDE.md](ui/CLAUDE.md)

### プラグインエディタ窓と Win32（Windows）

設計正本は [docs/plan_plugin_editor_topwindow.md](docs/plan_plugin_editor_topwindow.md)。

- **エディタ窓は daw_plugin_host が作る top-level で、owner は daw_gui の本体窓**
  (`daw_plugin_host/src/editor_window.rs`)。**daw_gui が窓を「作って」はいけない** — 窓が daw_gui の
  プロセスに属すると JUCE の `Process::isForegroundProcess()` (前面窓の **プロセス ID** 比較) が
  false になり cascade サブメニューが即 dismiss される (FIXME #31 の真因)。**禁止されるのは
  「窓の所属プロセス」であって「所有関係」ではない**。
- **owner と `WS_EX_TOOLWINDOW` は必ずセットで、owner が先**。片方だけ入れると Alt+Tab から消えた
  のに背後へ潜れる = 戻す手段が無い状態になる。owner は `CreateWindowExW` の `hWndParent` で
  **作成時に決める**。帰結として **owner の HWND は u64 の platform window handle として IPC を
  渡る** — PID から窓を探す発見方式は採らない ([[project_ffmpeg_fetch_n71_gone]] と同じ形。
  所有者は 1 か所で決まるべき)。
- **エディタ窓を操作している間、daw_gui は foreground プロセスですらない** (意図的)。「アプリ
  全体がアクティブか」は daw_gui 内の情報だけでは判定できないので、エディタ窓の WNDPROC が
  `WM_ACTIVATEAPP` を拾い `PluginEvent::HostWindowsActive` で報告する (r.md #49)。
- プラグインのウィンドウメッセージは **作ったスレッドのキュー**に入る。daw_plugin_host は
  `#[tokio::main]` とは別に専用 std::thread「plugin-main」で `GetMessageW` ポンプを回し、CLAP の
  `@[main-thread]` 呼び出しも同じスレッドで直列化する。窓もそのスレッドで作る。
- JUCE の述語の読み解きと 2026-08-22 の撤回、`GWLP_HWNDPARENT` を使わない理由、WNDPROC から
  Rust 状態へ届かせる `GWLP_USERDATA` + leak/`Arc::from_raw` の作法、リサイズ・フォーカス転送・
  ジオメトリ永続化の契約は、上記設計正本 §背景 / §1-1 / §2-5。

### CLAP GUI 仕様の落とし穴

初回 open では `set_size` を呼ばない / `gui.show` が false でも即 destroy しない /
`set_parent` と `show` の間にメッセージポンプを 1 回回す / `clap_host_gui` は `host_data` に
`&mut Host` のポインタを仕込んで復元する。4 件とも VCV Rack での実測が根拠で、それぞれの理由は
[docs/plan_plugin_editor_topwindow.md](docs/plan_plugin_editor_topwindow.md) §9。

## Reflection の確認 (AHE 自律改善ループ)

セッション開始時、`SessionStart` hook が出す **Required Action** を必ず triage してから user の依頼に入る。
2 系統ある:
- **reflection 候補**（user 修正 / rework 検出）… save (memory) / discard で終端。
- **AHE backlog**（`~/.claude/projects/F--dev-daw-01/ahe_backlog.md` の OPEN 行 = metrics から検出した
  再発フリクション）… `/promote-reflection` skill で **guard (guards.jsonl) / skill / command / memory に昇格**、
  新規 hook 種別は user 承認キュー、不要なら dismiss。**memory で終わらせない**（それが旧ループの欠陥 =
  提案が actuate せず memory だけ増えた）。backlog は per-project user dir、全 worktree 共有・git 外。
  `done` / `dismissed` は終端で再浮上しない。

#### Guard layer（メモリ → 能動的強制力）= 旧ループ最大の欠陥の修正

feedback メモリは `<system-reminder>` の **受動的背景**として recall されるだけで「ミスの瞬間」に
強制力を持たない。action-time に効くのは PreToolUse hook だけなので、**`.claude/guards.jsonl`
(git 追跡下) に 1 行足すのがメモリを強制力に変える主経路**である（新規スクリプトも
settings.json 編集も承認も要らない = classifier ブロックを回避できる。これが loop 自律化の鍵）。

- `scripts/guard_engine.py` が全ルールを適用する。`warn` = stdout/exit 0、`block` = stderr/exit 2
  （取消）。ルール書式 (tool / field / all / none / file_glob) と `source`（由来メモリへの逆リンク
  = SSoT）は同ファイル冒頭。
- 発火は `guard_hits.jsonl` に記録され、`scripts/reflect.py` が **3 つ以上の異なる session で
  発火した warn を自動で block へ昇格**する。昇格は **`<state>/guard_state.json`（git 外の
  overlay）** に書き、**追跡ファイルは一切書き換えない**。
- **ルール本体は追跡下 / 昇格状態は git 外**。この 2 つを 1 ファイルに同居させていたせいで
  レジストリが丸ごと消え、**5 日間 pattern guard が全滅していたのに fail-open で症状ゼロ**
  だった（2026-08-22）。事故と分離の設計は `scripts/guard_engine.py` docstring の "Registry" 節、
  パス導出は `scripts/ahe_paths.py`（マシン固有パスをハードコードしない）。
- **`escalate: false` を外さないこと。** substring レベルのマッチャは nudge としては妥当でも、
  block にすると正当な作業まで取り消す。理由は各ルールの直前にコメントで書いてある。
- 正規表現 1 本で表せない security block と、cwd・件数との関係でしか判定できないもの
  (`worktree_outside` / `ask_multi`) は code 側。**pattern guard は data、logic guard は code。**

ループ構造は observe（`PostToolUse` で `log_metric.py` + `guard_engine.py` が記録）→ reflect
（`Stop` で `reflect.py` が warn の自動昇格 + Bash 連続失敗を backlog へ upsert）→ **actuate**
（次 session の `SessionStart` が OPEN 行を提示 → `/promote-reflection` で終端）→ close。設計の
出発点は [docs/plan_ahe.md](docs/plan_ahe.md) 章 1.5 + 章 3 (H13/H14/H15)。

### hook 配置ポリシー

- **PowerShell 禁止。bash を既定**とし、**JSON を構造的にパースする必要があるものだけ Python
  (stdlib のみ、jq 不要)** で書く (Linux でも動くこと。PS 5.1 の encoding 地獄 + Windows 専用を
  排除。[[feedback_no_powershell_cross_platform]])。新規 .ps1 と bash からの powershell 起動は
  guard が block する。
- AHE hook (PreToolUse / PostToolUse / Stop) は **`.claude/settings.json`** に集約する。git 追跡
  対象なので新規 worktree でも何もせず有効になる。`settings.local.json` に hook を書くと harness の
  同期次第で worktree に伝わらず AHE ループが片肺になる (こちらは **マシン固有 permissions
  allowlist 専用**、gitignore 対象)。
- `.claude/settings.json` の編集は harness の auto-mode classifier に self-modification として
  ブロックされる。hook を増減したいときはユーザーに依頼すること。

## 応答・コミット

- 応答は日本語
- コミットメッセージは日本語
- 技術用語は英語のまま使用可

## Coding Principles

### 最終形まで実装する

**禁じているのは「途中で報告して承認を待つこと」であって、計画を段階に割ることではない。**
大規模改修を `docs/plan_*.md` で段階に割り、並列 worktree の統合順まで決めるのはむしろ推奨。
だめなのは「Phase 1 完成しました。Phase 2 に進みますか」で手を止めること。着手したらゴールまで
完走する ([[feedback_dont_stop_prematurely]])。実装方針 / 分割単位 / 命名 / テストの粒度、一次情報を
読めば決まること、同じ root cause の同件修正 ([[feedback_sibling_occurrence_check]]) は聞かずに進む。

**止まって聞く場面は次の 4 つだけ**:
- **着手前** — UI の見せ方・操作 (閉じ方 / 移動 / リサイズ / 永続範囲 / 並び / 背後操作) を確定
  させる。省くとイメージ違いで全書き直しになる ([[feedback_grill_ui_presentation_first]])。
- **着手前** — 要件が 2 通りに読め、どちらを取るかで作るものが変わるとき。**1 問ずつ、上流から、
  番号付きの選択肢で** ([[feedback_one_question_at_a_time]] / [[feedback_numbered_question_options]])。
- **commit の直前** — 実機 / 視覚の sign-off ([[feedback_confirm_before_commit]])。自動検証だけで
  先に commit しない。
- **完全に手詰まりのとき** — 権限・外部要因で先へ進めず、こちらで解けないと確定したとき。

### ベストプラクティスを追求する
- Rust Edition 2024 / 各 crate は最新版
- `let-else` で早期リターン、`?` 演算子を `match` より優先
- `unsafe extern` ブロック（Edition 2024 で必須）

### KISS / DRY
- 最小限の実装で目的を達成する。不要な抽象化を作らない
- 1 関数 1 責務
- 3 回繰り返されたら抽象化を検討

### Single Source of Truth
- 同じデータを複数箇所に複製しない
- 「この値は誰が所有し、誰が更新するか」を明確にしてから実装する
- **規範も同じ**。同じ規則を 2 か所に書かない。機械が持てるものは機械に持たせ (Makefile /
  guards.jsonl / arch_lint.sh)、散文は 1 行要約 + リンクにする。**原文を引用して再掲しない** —
  片側だけ更新されて静かに食い違う

### まず調べる
- 設計提案・前提確認・実装方針を書く前に、必ず一次情報を調査する
- 公式ドキュメント (Ardour manual / REAPER manual / clap repo / cpal docs / windows API docs 等)、spec ファイル (clap/ext/*.h / VST3 spec)、参照実装ソース (sing_like_coding / gui_01 / clap-host / nih-plug 等) を読む
- ユーザーの発言は調査の方向ヒントとして扱い、最終根拠は一次情報で取る。引用 URL や行番号付きで書く
- 推測で書かない

#### 調査結果の採用条件 — 反証を通っていないものを根拠にしない

**引用付きで集めた時点ではまだ根拠にならない。** その調査を**反証する側を独立に立て**、
反証が潰せなかった主張だけを設計判断の根拠にする。潰すべき失敗パターンは決まっている:

- **実行して確かめずに書いた主張** — 「make の target にする」が結論なのに make 経由で一度も
  実行していない、等。結論が使われる**その経路で**動かして確かめる。
- **測定器そのものが交絡している主張** — 環境を観測するのに交絡した環境から観測している、等。
  `env -i` 相当の対照を取る ([[feedback_diagnostics_can_lie]])。
- **範囲の誇張** — 「全数検査」が実際は 2 割。**母数と分母を必ず数える**。
- **隣の項目との衝突に気付いていない** — その推奨が、同じ backlog の別項目が未解決だと動かない。

「調べた」と「確かめた」は別で、これは
[外部 API の挙動を先に理解する](#外部-api-の挙動を先に理解する) と
[[feedback_called_is_not_worked]] (「呼んだ」と「効いた」は別) と同じ規則の別の面。
r.md #79-#85 では 8 本の調査に 1 本ずつ反証を当てて **主要結論 6 本が覆った** (#82 の「劣化
コピー」は `git log -S` で片側更新の drift と判明、「全数検査」は実際 19.7% 等)。全 6 例は
[docs/plan_rmd_79_85_index.md](docs/plan_rmd_79_85_index.md)。同型の実測は
[[project_rmd_39_43_parallel]] にもあり、2 回続けて起きているので構造として扱う。

### 外部 API の挙動を先に理解する
- 推測で実装→失敗→修正のサイクルは、調査→実装より遅い
- CLAP / clap-sys / cpal / winit / wgpu / gui_01 (daw-ui) / windows crate の挙動はドキュメント・ソースで確認してから組む

### エラーを握りつぶさない
- `?` を安易に `ok()` / `unwrap_or_default()` に置き換えない
- FFI・CLAP コールバック・IPC のエラーは根本原因を調査してから対処

### テスト

**自動で確かめられることをユーザーに頼まない。** このワークスペースに js テストは無い
(`daw_gui/tests/scripts/*.js` は `--script` モードのシナリオ記述であってテストではない)。
- Rust の `#[test]` で書けるものは書いて自分で回す (`cargo test -p <crate> --test <name>`)。
- GUI / IPC / 再生を跨ぐ切り分けは `daw_gui --script <js>` の headless モードで自分でやる。同じ
  実機操作を何度も頼まない ([[feedback_prefer_headless_verification]])。**頼むのは最終 sign-off
  だけ**で、揃う前に実機確認を要求しない ([[feedback_no_redundant_verification]])。
- 逆に**自明な修正に回帰テストを書かない**。本番の算術をテストへ写して突き合わせるだけの
  テストは特に禁止 ([[feedback_no_tests_for_simple_cases]])。

### 妥協を選択肢に上げない

**大原則 (冒頭) は「実測してから妥協を選ぶ」ではない。そもそも妥協を選ばない。**
(これは **何を作るかの選択**についての規則。統合順や分け方に規模・衝突の実測を使うのは
[禁止事項](#禁止事項) のとおり別の話。)

出すべき問いは 2 つだけ — どれが **理想** か? / 理想を実現するには何を破壊する必要があるか?
出してはいけない問い (= principle 違反) — どれが **実装コストが低い** か? / **影響範囲が狭い** か? /
**caller boilerplate が少ない** か? / **現実的** か?

「実装コスト」「影響範囲」「連鎖する」「許容範囲」「現実的に」「妥協」 — これらが思考に出てきた**時点で**、 理想以外の選択肢を比較対象に上げてしまっている。 PreToolUse の guard engine (`scripts/guard_engine.py` + `guards.jsonl` の compromise-smell-ja/en ルール) がこれらのキーワードを Edit / Write の対象 string に見つけたら、 警告を emit する (block はしない、 思考の中断点として作用)。

#### 実例 (2026-05-25, この principle を破った)

gui_01 #045 Phase 74 で `isize` raw vs `HANDLE` 型受け の選択時、 「workspace windows bump は連鎖、 caller boilerplate +1 行は許容範囲」 と書いて raw 値受けを推奨。 ユーザーから 「実装コストは考えずに理想的なものを」 と明示的に指示があったのに違反した。 正しい思考: 理想 = `HANDLE` 型受け、 破壊 = workspace bump、 終わり。 詳細: `~/.claude/projects/F--dev-daw-01/memory/feedback_pursue_ideal_only.md`。

## Real-Time Audio の制約（最重要）

オーディオコールバック（daw_audio の再生スレッド、および CLAP process() に至るパス）
では以下を厳守する。違反するとドロップアウト・クラックルが起きる。

- **ヒープ確保禁止**: `Vec::new()`, `format!()`, `String`, `.collect()`, `Box::new()` を呼ばない。バッファは再生開始前に確保して使い回す
- **ロック禁止**: 再生スレッドでブロッキングロックを取らない。UI ↔ 再生スレッド間はロックフリーキューや Atomic で渡す
- **I/O 禁止**: ファイル I/O・ログ出力・println! を呼ばない
- **システムコール最小化**: `Instant::now()` は許容、`thread::sleep` は避ける

## アーキテクチャ不変条件

2026-07-03 の全体改修 ([docs/plan_arch_refactor.md](docs/plan_arch_refactor.md)) で確立。
**`make arch-lint` が機械検査**し、`/arch-review` skill が定期監査する。これらに触れる変更は
plan_arch_refactor.md を先に読む。書く瞬間の強制は `.claude/guards.jsonl` の `arch-*` ルール
(plan §11 の 5 件: INFINITE / positional tuple key / push_undo_snapshot 直呼び / untagged 追加 /
MainToChild 復活)。**「何を違反とみなすか」の SSoT は `scripts/arch_lint.sh`**、ガードはその
write-time ミラー。サイズ budget の測り方と「コメント内の言及は違反に数えない」の行分類だけは
`scripts/loc_budget.py` が持つ (Rust の字句解析が要る)。その帰結として **python が無い / 壊れて
いると `make arch-lint` は全面停止する** (「skip の緑を作らない」原則)。

**`make arch-lint` の exit 0 は「違反ゼロ、または `scripts/arch_lint_baseline.txt` に記録済みの
ものだけ」を意味する。** baseline に無い違反が 1 件でもあれば exit 1 (行単位 ratchet)。
以前は違反があっても常に exit 0 だったので、終了コードだけ見て「OK」と報告され続けていた。
**恒久的に正当な箇所は baseline ではなく行内マーカー** `// arch-lint: allow-<check>` (区別しないと
負債が「正当」として永久に隠れる)。baseline の書式 (行番号ではなく内容ハッシュ / サイズ budget
だけ第 3 field が計測値の天井 / 件数 baseline にしない理由 / 直した行は削除する) と
`ARCH_LINT_EMIT_BASELINE` / `ARCH_LINT_STRICT` は `scripts/arch_lint_baseline.txt` 冒頭が正本。

> **arch-lint のパターンにバックスラッシュを使わないこと。** POSIX ブラケット式
> (`[(]` `[]]` `[[:space:]]`) と `grep -w` で書く。make (MSYS2) 経由だと grep へ渡す argv の
> バックスラッシュが落ちるため。実測した argv と、**8 チェック中 6 つが無言で無効化されたまま
> 「OK (違反なし)」を出していた** 2026-08-22 の偽グリーンは `scripts/arch_lint.sh` 冒頭
> 「正規表現にバックスラッシュを使わない」節。同ファイルの canary が毎回これを再検査する。

1. **安定 id addressing**: プロセス境界・イベント・永続参照に positional index を使わない。
   device = `PluginInstance.id` (u64、shmem 名・worker dispatch・plugin host bookkeeping も同じ id)、
   send = `Send.id`、note/point/audio event = 要素 id。**「削除/並べ替えで参照を貼り替える補償
   コード」を書き始めたら設計が誤り** (旧 ReorderChain の 3 プロセス貫通再キーが反例)。
2. **wire は blob-less**: `LoadSong` の Song は `state`/`ara_archive` を構造的に除外
   (PluginInstance の手書き bincode Encode)。protocol に `Vec<f32>`/`Arc<[u8]>` の bulk を
   直載せしない (専用 message / WAV materialize / id 参照で運ぶ。16MB wire 上限は防御であって
   「大きくして解決」しない)。
3. **宛先は型で表現**: IPC は `AudioCommand`/`AudioEvent`/`PluginCommand`/`PluginEvent`。
   単一 enum (旧 MainToChild/ChildToMain) に戻さない。「相手が無視する variant の no-op arm」が
   生えたら分割が壊れているサイン。
4. **RT スレッドは無限待ち・確保・解放をしない**: 他プロセスの完了待ちは有界
   (`DISPATCH_TIMEOUT_MS`) + quarantine (`common/src/plugin_ref.rs` の poisoning contract)。
   map 再構築・pool 生成/破棄等の重い作業は off-thread で構築し rtrb ring で swap。
5. **Song 編集の副作用は単一の口**: undo snapshot / dirty / epoch / 子プロセス sync 予約は
   `edit_song()` チョークポイントが無条件で担う。手動 `push_undo_snapshot`・whitelist・
   view からの song 直接可変参照を追加しない。
6. **live と export は同じ render 関数** (`render_master_buffer`): master fx / master gain を
   含む「1 buffer を描く」処理を二重実装しない。
7. **fingerprint handshake**: wire を渡る型を新ファイルへ切り出したら `common/build.rs` の
   `WIRE_SOURCES` に必ず追加 (protocol 変更の検出網に穴が開く)。
8. **daw-ui core はドメイン知識を持たない**: DAW 固有 widget (arrangement / piano_roll) は
   `daw_gui/src/widgets/` で `common::model` 直結。mirror 型・翻訳 request enum を作らない。
9. **サイズ budget**: **実コード行 (ncloc = 空白・コメント・doc comment を除いた物理行)** で
   1 ファイル **1,000 行** / 1 関数 **300 行** / インデント **6 段**。超過したら分割してから
   足す (app.rs 25k 行の再発防止)。**テストコードは対象外** (`#[cfg(test)]` の付いた item、
   `#[cfg(test)] mod X;` が指すファイル、`tests/` `benches/` 直下)。
   測り方の SSoT は `scripts/loc_budget.py` (Rust の字句解析。`wc -l` ではない —
   物理行で測っていた頃は「テストを厚くすると分割を迫られる / doc を書くと分割を迫られる」
   逆インセンティブになり、tests を別ファイルへ移すだけの commit が実際に 2 件生えた)。
   現在値は `python scripts/loc_budget.py --report`。r.md #76。

**不変条件 2 / 5 / 6 / 8 に対応する arch-lint チェックは無い** (機械検査があるのは 1 / 3 / 9 と
untagged / RT の INFINITE)。この 4 件は上の本文が唯一の強制手段なので、圧縮しないこと。

## FFI / CLAP 境界のセキュリティ

- ポインタの null / 境界チェックを必ず行う
- 整数キャストは `saturating_add`, `try_from` を優先
- MIDI デバイス、プラグインが書き込むイベント配列はサイズ上限を検証
- `from_raw_parts` / `copy_nonoverlapping` は長さの妥当性を検証してから使う

## FFI 境界の dead code 判定を 推測でしない

FFI 境界 (= D3D11 / wgpu / CLAP / cpal / windows API) のコードを「**自分の側で対応する呼び出しが見当たらないから dead**」 と判定して削除するな。 相手側 (= wgpu, driver, OS) が**内部で**その protocol / state を消費している可能性が常にある。

実例 (2026-05-26、 この principle を破った):
- `c2ae697 chore(video): worker 側の dead keyed-mutex Acquire/Release を削除`
- 私が「daw_01 の main thread で `IDXGIKeyedMutex::AcquireSync` を呼んでない → worker 側の Acquire/Release は self-lock で無意味」 と判定して削除
- 実際は **wgpu の DX12 / Vulkan import 側が内部的に keyed-mutex protocol を消費していた**。 worker 側削除で wgpu からは「mutex が永久に worker 側 hold」 と見え、 imported texture が全 pixel 透過に
- 症状: preview window がダーク背景のみ表示、 動画フレーム見えず
- 反転 commit: `6b5eebd`

正しいやり方:
1. 「相手側で何が起きるか」 を 一次情報 (= wgpu source, driver docs, windows API docs) で確認してから削除
2. 削除前に **必ず実機 smoke test** で 「削除しても挙動が変わらない」 ことを目視確認
3. 不明なら **削除しない**。 「dead に見えるけど挙動に必要」 のパターンは FFI で日常的に起きる

詳細: `~/.claude/projects/F--dev-daw-01/memory/feedback_no_dead_judgment_at_ffi.md`

## Visual regression smoke test

video preview の暗転 / 全 pixel 透過 / 一様 fill 等の **visual regression** は `cargo build` /
`cargo test` / `cargo clippy` を全部すり抜ける。**video preview / texture sampling /
shared-handle 周りに触れる変更は、commit 前に必ずこれを通す。**

```bash
cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4
# exit 0 = 描画されている / exit 1 = blank・一様・透過
```

なぜ要るか (`c2ae697` が build/test/clippy すべて green のまま preview を全 pixel 透過の quad に
し、発覚まで 6-7 時間かかった)、fixture の作り方 (pin した FFmpeg + libopenh264 を使う理由)、
histogram 判定の閾値、終了時に `AppEvent::Quit` で通常の shutdown を通す理由は
`daw_gui/src/smoke_test.rs` の module doc が正本。

## Debugging Methodology

- **実データから始める**: コードパス推論より実データ観察が速い
- **フルサイクルで検証する**: 個別関数が正しくても、パイプライン全体が壊れていれば無意味
- **上流→下流の順で調査する**: UI/コマンド → Model → IPC → Plugin Host → プラグイン本体
- **UI イベントは常時ログ無し**: GUI のキーバインド・ボタンクリックは、何も起きないとき「キー拾えてない」「emit されてない」「handler が間違い」の 3 層で切り分ける必要がある。`tracing::info!` を各層に仕込む

## 参照プロジェクト

- `ui/` — 自作 GUI ライブラリ daw-ui。API は crate doc-comments、サンプルは
  `ui/crates/examples/{mixer, arrangement, piano_roll, ...}`、設計正本は
  [ui/docs/plan.html](ui/docs/plan.html)。
- `sing_like_coding` (作者ローカルの別リポジトリ) — 前作 Rust DAW。IPC, CLAP ホスト,
  オーディオエンジンの参照実装
- `%APPDATA%\REAPER\Scripts\<user>\voicevox\` (作者ローカル) — VOICEVOX API 統合の参照実装 (Lua)
- clap-host / clap-validator / nih-plug 等の clone 先パスつき一覧は
  `.claude/skills/research-similar-impl/references.md`
