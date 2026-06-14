理想とベストプラクティスを追求する。
そのためは実装コストは無視して大胆に破壊して作り直す。

# daw_01

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。詳細は [DESIGN.md](DESIGN.md)。

## プロジェクト構成

Cargo workspace (Edition 2024)。

```
common/            -- 共有型・IPC プロトコル・shared memory・データモデル
daw_gui/           -- GUI プロセス (daw-ui = winit + wgpu + 自作 immediate-mode UI)
daw_audio/         -- Audio Engine プロセス (CPAL)
daw_plugin_host/   -- Plugin Host プロセス (CLAP/VST3)
ui/                -- UI ライブラリ daw-ui (旧 gui_01)。crates/{platform,renderer,ui} + examples
```

UI ライブラリ daw-ui は `ui/` に統合済み (旧 sibling repo gui_01)。同一 workspace・同一
セッションで直接編集する。UI 固有の技術ガイド・既知の罠は `ui/CLAUDE.md` 参照。

## Development Workflow

```bash
cargo build --workspace
cargo run -p daw_gui            # GUI 起動（Audio/Plugin プロセスを子プロセスとして起動）
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

### ビルドと検証の区別（重要）

- `cargo clippy` / `cargo check` / `cargo test` は**実行バイナリを生成しない** or 生成してもテストビルドのみ。
  手動で `./target/debug/daw_gui.exe` を走らせる前に、必ず `cargo build` を明示する
- 子プロセス（daw_audio / daw_plugin_host）の挙動を変えたときは、子プロセスのバイナリも再生成が必要。
  `cargo build -p <crate>` または `cargo build --workspace`
- `cargo run -p daw_gui` は dependency crate も自動ビルドしてくれるが、既に起動中のプロセスのバイナリは上書きされない場合がある（Windows の ERROR 5）。必要なら先に全プロセスを終了

### commit したら必ず release build（厳守）

- **全 commit の後に release build (`cargo build --workspace --release`) を必ず通す**。
  release は debug と別 profile なので、debug-clean でも release で壊れることがある。
- 自動化は **git-native post-commit hook** で行う（`.githooks/post-commit` →
  `scripts/release_build_bg.ps1` を detached 起動。`core.hooksPath = .githooks`）。
  commit の起動手段（Bash/PowerShell ツール・手動・`!` passthrough）に依存せず必ず発火する。
  ビルドは非ブロッキング、結果は `target/release-build.log`、失敗時は
  `target/.release-build-failed` marker + ダイアログ。
- Claude 自身も commit 後に `cargo build --workspace --release` を実行して green を確認する
  （hook の background build と cargo の target lock で直列化されるので二重ビルドにはならない）。
  `target/.release-build-failed` があれば別作業に進まず即修正する。
- 旧 Claude-Code PostToolUse hook（matcher `"Bash"`）は Bash ツール経由 / hook 有効 session の
  commit にしか発火せず取りこぼしていた（2026-06-08 に FIXME 5 commit で release build が
  走らず発覚）。git hook へ移行済み。

### IPC 境界で送る型

- `MainToChild` / `ChildToMain` 等の protocol 型、およびそれが保持する内側の型すべてに
  `#[derive(bincode::Encode, bincode::Decode)]` が必要
- Song / Track / Clip / Row 等のモデル型も protocol 経由で渡す場合は bincode derive を追加する

### GUI デバッグ

- UI のキーバインド・イベントは可視フィードバックが無いと動いたか判別不能
- 迷ったら `AppData::handle_event` 冒頭に `tracing::info!(?event, "received")` を仕込んでログで確認
- 確認後は削除するか、debug feature で囲う（`[debug-gui]` skill 参照）

### gui_01 (daw-ui) アーキテクチャ要点

- **path 依存**: `daw-ui-platform` / `daw-ui-renderer` / `daw-ui-core` は workspace で
  `path = "ui/crates/*"` 指定 (統合済み)。直接の依存は `winit 0.30` / `raw-window-handle 0.6` も追加
- **AppData は plain mutable struct**: Signal/Memo/derive は使わない。派生は method
  (`app.track_headers() -> Vec<TrackHeader>`) として毎フレーム計算。重ければ view 側で 1 frame 分キャッシュ
- **イベント dispatch**: view から `Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::X))`、
  background thread から `EventLoopProxy<AppEvent>::send_event`。`impl Model` 的な trait 接続は不要
- **immediate-mode + heavy() escape hatch**: 通常 widget は毎フレーム再構築だが、
  `ui.heavy(id, |hctx| { hctx.cached(viewport_key, |hctx| { ... }) })` で粗粒度キャッシュ。
  ピアノロール / アレンジビュー等の大量描画はこの中で `push_rect / push_text / push_lines` を呼ぶ
- **Edit<M>**: `Box<dyn FnOnce(&mut M) + Send + 'static>`。view 内クロージャから直接モデル変更可
- **WindowBackend**: `daw-ui-platform::WindowBackend` trait を満たす型を `Renderer<W>` に渡す。
  daw_gui は `view/window.rs::DawGuiWindow` で winit::Window をラップ
- **イベントループ**: `view/runner.rs::Runner` が `winit::ApplicationHandler<AppEvent>` を実装。
  WindowEvent → `daw_ui_platform::AppEvent` 変換 + InputAccumulator ingest、user_event →
  `AppData::handle_event` dispatch、IME 差分管理、Win32 cursor 位置補正
- **キーボードショートカット**: `Runner::dispatch_shortcut` が WindowEvent::KeyboardInput を
  直接見て、focus 中の widget が無いとき AppEvent を発火 (Space/P/V/Ctrl+S/Ctrl+Z/Delete 等)
- **ダブルクリック**: gui_01 v1 には built-in 検出無し。`AppData::last_click: Option<(Instant, x, y)>`
  に最終クリックを記録し、各 view の入力ハンドラで 400ms+5px 以内なら double 判定
- **背景スレッド**: autosave / playhead poll / MIDI / IPC bridge / VOICEVOX synth / plugin DB
  rescan は std::thread + EventLoopProxy。`tokio::time::sleep` は不可、`std::thread::sleep` を使う
- **HeavyCtx の API**: `push_rect`, `push_text`, `push_lines`, `push_edit`, `button_at`, `label_at`,
  `waveform`。`Ui::push_edit` は `pub(crate)` なので、view から Edit を流す際は heavy ブロック内で
  `hctx.push_edit(...)` を使う

### 子プロセスとクロスプロセス HWND（Windows）

- `HWND` は `windows` crate で `HWND(*mut c_void)`。IPC 越しに渡すには `u64` にキャストして bincode
- CLAP プラグイン GUI は daw_gui が所有する **別 top-level HWND** にホストする
  (`view/plugin_embed.rs::PluginHostWindow`)。daw_gui のメインウィンドウとは独立、winit/wgpu
  surface と干渉しない。`daw_plugin_host` へ HWND を `u64` で送る → `clap_plugin_gui.set_parent`
  でプラグインが子ウィンドウ化
- WNDPROC は `extern "system" fn` で Rust 状態にアクセスできないので、`GWLP_USERDATA` に
  `Arc<AtomicBool>` 等を leak で貼り付け、Drop で `Arc::from_raw` して回収する
- プラグインのウィンドウメッセージは **作ったスレッドのキュー** に入る。daw_plugin_host は
  `#[tokio::main]` 側とは別に **専用 std::thread「plugin-main」** を立て、そこで
  `GetMessageW` ポンプを回す。同じスレッドで CLAP の `@[main-thread]` 呼び出しも直列化する

### CLAP GUI 仕様の落とし穴

- `clap_plugin_gui.set_size` は「前回セッションで保存したサイズを復元」用。**初回 open では
  呼ばない**（`get_size` の戻り値をコンテナ側に反映するだけ）。これで VCV Rack 等が
  `gui.show` を拒否するケースを防げる
- `gui.show` が `false` を返しても、`create` + `set_parent` が成功していれば GUI は実際に
  動いているケースがある（VCV Rack）。ログ警告に留め、即 destroy しない
- `set_parent` と `show` の間に `PeekMessage` + `DispatchMessageW` でポンプを 1 回回すと、
  プラグインが内部で `PostMessage` した初期化メッセージが処理され、show が通るケースがある
- ホスト側 `clap_host_gui` は `host_data` に `&mut Host` のポインタを仕込み、
  callback 内で復元する（`Box<Host>` は heap 固定なのでポインタが安定）

## Reflection の確認 (AHE 自律改善ループ)

セッション開始時、`SessionStart` hook が出す **Required Action** を必ず triage してから user の依頼に入る。
2 系統ある:
- **reflection 候補**（user 修正 / rework 検出）… save (memory) / discard で終端。
- **AHE backlog**（`~/.claude/projects/F--dev-daw-01/ahe_backlog.md` の OPEN 行 = metrics から検出した
  再発フリクション）… `/promote-reflection` skill で **skill / command / memory に昇格**、hook は user 承認キュー、
  不要なら dismiss。**memory で終わらせない**（それが旧ループの欠陥 = 提案が actuate せず memory だけ増えた）。
  backlog は per-project user dir、全 worktree 共有・git 外。`done` / `dismissed` は終端で再浮上しない。

ループ構造（observe → reflect → **actuate** → close）:
1. 各 tool 呼び出しを `PostToolUse` hook (`scripts/log_metric.ps1`) が metrics jsonl に追記
2. session 終了時に `Stop` hook (`scripts/reflect.ps1`) がパターン検出 → backlog に keyed row を **upsert**
   （id でデデュープ、status / sessions 付き、truncate しない。旧 `reflection_latest.md` への prose 追記は廃止）
3. 次 session 開始時、`SessionStart` hook が OPEN 行を Required Action として強制提示（= 唯一効く forcing function を
   actionable な系統に向けた。旧構造ではこの強制力が memory 系統だけに刺さっていた）
4. `/promote-reflection` で各行を終端（promote / hook 承認依頼 / dismiss）。hook 登録の変更だけは
   user 承認が要るので backlog の "hook requests" 節に ready-to-paste spec を書いて依頼する

詳細設計は `docs/plan_ahe.md` (章 1.5 autonomy spectrum + 章 3 H13/H14/H15)。

### hook 配置ポリシー

- AHE hook (PreToolUse / PostToolUse / Stop) は **`.claude/settings.json`** に集約する。
  このファイルは git 追跡対象なので、新規 worktree でも何もせず hook が有効になる。
- `.claude/settings.local.json` は **マシン固有 permissions allowlist 専用** に残す
  (gitignore 対象、harness が worktree 開始時に同期する想定)。
- hook を追加したくなったら必ず `settings.json` 側に追加すること。
  `settings.local.json` に hook を書くと、harness の同期次第で worktree に伝わらず
  AHE ループが片肺になる。
- **例外: release-build-on-commit は git-native hook (`.githooks/post-commit`)**。
  AHE 系の「観測」hook と違い、これは commit の起動手段すべて（手動・`!`・PowerShell ツール）で
  発火する必要があるため、Claude-Code の PostToolUse（Bash ツールしか拾えない）ではなく
  git hook に置く。`core.hooksPath = .githooks` で tracked・worktree 共通。
- `.claude/settings.json` の編集は harness の auto-mode classifier に self-modification として
  ブロックされる。settings.json の hook を増減したいときはユーザーに依頼すること。

## 応答・コミット

- 応答は日本語
- コミットメッセージは日本語
- 技術用語は英語のまま使用可

## Coding Principles

### 最終形まで実装する

フェーズ分けをせずに最終形を一気に完成させる。
「Phase 1 完成しました。Phase 2 に進みますか」などはだめ。
ゴールまで完走する。

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

### まず調べる
- 設計提案・前提確認・実装方針を書く前に、必ず一次情報を調査する
- 公式ドキュメント (Ardour manual / REAPER manual / clap repo / cpal docs / windows API docs 等)、spec ファイル (clap/ext/*.h / VST3 spec)、参照実装ソース (sing_like_coding / gui_01 / clap-host / nih-plug 等) を読む
- ユーザーの発言は調査の方向ヒントとして扱い、最終根拠は一次情報で取る。引用 URL や行番号付きで書く
- 推測で書かない

### 外部 API の挙動を先に理解する
- 推測で実装→失敗→修正のサイクルは、調査→実装より遅い
- CLAP / clap-sys / cpal / winit / wgpu / gui_01 (daw-ui) / windows crate の挙動はドキュメント・ソースで確認してから組む

### エラーを握りつぶさない
- `?` を安易に `ok()` / `unwrap_or_default()` に置き換えない
- FFI・CLAP コールバック・IPC のエラーは根本原因を調査してから対処

### 要件にない変更を入れない
- 既存の挙動を勝手に変えない
- バグ修正ついでのリファクタリングは別コミット

### 妥協を選択肢に上げない

冒頭の **「理想とベストプラクティスを追求する。 そのためは大胆に破壊して作り直す。」** は、 **実測してから妥協を選ぶ** ではない。 **そもそも妥協を選ばない**。

選択肢を比較する時に出すべき問い:
- どれが **理想** か?
- 理想を実現するには何を破壊する必要があるか?

出してはいけない問い (= principle 違反):
- どれが **実装コストが低い** か?
- どれが **影響範囲が狭い** か?
- どれが **caller boilerplate が少ない** か?
- どれが **現実的** か?

「実装コスト」「影響範囲」「連鎖する」「許容範囲」「現実的に」「妥協」 — これらが思考に出てきた**時点で**、 理想以外の選択肢を比較対象に上げてしまっている。 PreToolUse hook (`scripts/check_antipattern.ps1`) がこれらのキーワードを Edit / Write の対象 string に見つけたら、 警告を emit する (block はしない、 思考の中断点として作用)。

#### 実例 (2026-05-25, この principle を破った)

gui_01 #045 Phase 74 で `isize` raw vs `HANDLE` 型受け の選択時、 「workspace windows bump は連鎖、 caller boilerplate +1 行は許容範囲」 と書いて raw 値受けを推奨。 ユーザーから 「実装コストは考えずに理想的なものを」 と明示的に指示があったのに違反した。 正しい思考: 理想 = `HANDLE` 型受け、 破壊 = workspace bump、 終わり。 詳細: `~/.claude/projects/F--dev-daw-01/memory/feedback_pursue_ideal_only.md`。

## Real-Time Audio の制約（最重要）

オーディオコールバック（daw_audio の再生スレッド、および CLAP process() に至るパス）
では以下を厳守する。違反するとドロップアウト・クラックルが起きる。

- **ヒープ確保禁止**: `Vec::new()`, `format!()`, `String`, `.collect()`, `Box::new()` を呼ばない。バッファは再生開始前に確保して使い回す
- **ロック禁止**: 再生スレッドでブロッキングロックを取らない。UI ↔ 再生スレッド間はロックフリーキューや Atomic で渡す
- **I/O 禁止**: ファイル I/O・ログ出力・println! を呼ばない
- **システムコール最小化**: `Instant::now()` は許容、`thread::sleep` は避ける

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

video preview の暗転 / 全 pixel 透過 / 一様 fill 等の **visual regression** は
`cargo build` / `cargo test` / `cargo clippy` 全 pass でもすり抜ける (= 実例:
`c2ae697` は build/test/clippy clean なのに preview を fully-transparent quad
にした、 6-7 時間費やして発覚)。 これを 1 コマンドで catch するために
`daw_gui --smoke-test <fixture.mp4>` を導入。

```bash
cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4
# exit 0 = preview rendered visible content (= healthy ~20 000 unique colors)
# exit 1 = preview blank / uniform / transparent (= unique_colors < 1000)
```

仕組み:
1. background thread が programmatic に `ImportVideo` → `TogglePreviewWindow`
   → `Play` を発火、 1.5s 再生
2. preview window の client area を Win32 `PrintWindow(PW_RENDERFULLCONTENT)`
   で pixel capture
3. histogram 解析: unique RGB ≥ 1000 / black pixels ≤ 95%、 を assertion
4. 結果を `std::process::exit(0 or 1)` で返す

video preview / texture sampling / shared-handle 周りに触れる変更は **必ず
commit 前にこれを通す**。 詳細は `daw_gui/src/smoke_test.rs`。

## Debugging Methodology

- **実データから始める**: コードパス推論より実データ観察が速い
- **フルサイクルで検証する**: 個別関数が正しくても、パイプライン全体が壊れていれば無意味
- **上流→下流の順で調査する**: UI/コマンド → Model → IPC → Plugin Host → プラグイン本体
- **UI イベントは常時ログ無し**: GUI のキーバインド・ボタンクリックは、何も起きないとき「キー拾えてない」「emit されてない」「handler が間違い」の 3 層で切り分ける必要がある。`tracing::info!` を各層に仕込む

## 参照プロジェクト

- `ui/` — 自作 GUI ライブラリ daw-ui (旧 gui_01, 統合済み)。同一 workspace・同一セッションで
  直接編集する。API は crate doc-comments、サンプルは `ui/crates/examples/{mixer, arrangement,
  piano_roll, ...}`、UI 固有の技術ガイド・既知の罠は `ui/CLAUDE.md`、設計正本は `ui/docs/plan.html`。
- `F:\dev\sing_like_coding` — 前作 Rust DAW。IPC, CLAP ホスト, オーディオエンジンの参照実装
- `%APPDATA%\REAPER\Scripts\yoshino\voicevox\` — VOICEVOX API 統合の参照実装 (Lua)
