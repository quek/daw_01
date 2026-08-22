# daw_01

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。詳細は [DESIGN.md](DESIGN.md)。

## 大原則

理想とベストプラクティスを追求する。
そのためは実装コストは無視して大胆に破壊して作り直す。

## 禁止事項

大原則を守るために次を禁止します。

- 実装コストで判断すること
- 実装難易度で判断すること
- 変更規模で判断すること
- コンパイルがとおらないことで判断すること
- 作業時間で判断すること

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

### `make test` は daw_gui を起動する (2026-08-22 に判明)

`daw_gui/tests/` の一部は **daw_gui 本体を `--script` で subprocess 起動**し、それが
daw_audio / daw_plugin_host まで spawn して audio device を開く。`--script` は窓を出さず
single-instance gate も素通りするので、**起動したことに誰も気付けない**。実機を触っている
最中に回すと、開いているプロジェクトの再生を壊す。

- **判定基準は 1 つだけ**: `grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs`。
  **名前で判断しない** — `pdc_real_vst3` / `sidechain_real_vst3` は smoke が付かないのに
  起動し、`arr_widget` / `pr_widget` / `font_picker` は起動しない。
  `--test` で名指ししても、この基準に当たる target なら起動する。
- **`make test` / `make run` / `make run-release` は前提条件で止まる**
  (`scripts/preflight_no_running_app.sh`)。daw_gui が起動していたら明示エラー。
  ユーザーが手で打っても効く。迂回は `DAW01_SKIP_PREFLIGHT=1`(理由は同スクリプト冒頭)。
- **起動を伴わない検証だけなら `make test-nolaunch`**。対象 target は Makefile が上の基準から
  機械的に導く (手書きの列挙にしない)。
- Claude 向けには `.claude/guards.jsonl` の `no-bulk-test-run` /
  `no-app-launching-test-target` が書く瞬間に block する。許可を得たうえで回すときは
  `DAW01_ALLOW_LAUNCH=1` を頭に付けて意図を明示する。
- 列挙が陳腐化しないよう、`scripts/test_guards.py` の `check_launching_targets_list()` が
  **毎回リポジトリから基準を再適用**して、ガードと Makefile のズレを検出する。

特定 crate/test だけを素早く確認したいときは `cargo check -p <crate>` /
`cargo test -p <crate> --test <name>` 等のピンポイント指定を使ってよい (これは Makefile の
scoping と矛盾しない、むしろ更に絞り込む方向)。避けるべきは `--workspace` の無条件多用。

### 依存の脆弱性 / 供給網攻撃 (`make audit`)

**`Cargo.lock` を commit していることが供給網攻撃に対する一次防御**である。lock を追跡して
いなければ、上流が汚染された瞬間に次のビルドで取り込む。だから `cargo update` は
「更新したい理由があるとき」だけ意図的に打ち、打ったら必ず `make audit` を通す。

実例 (2026-08-20): crates.io の **arrayref 0.3.10** が汚染された (**RUSTSEC-2026-0260**)。
typosquat の `proc-macro1` への依存が足され、その build script が **コンパイル中にリモートの
バイナリを取得して実行**する。同じ攻撃者が 23 分の間に `internment` 0.8.7 と
`append-only-vec` 0.1.9 も汚染。Rust Security Response Team は作者の端末 / 資格情報の侵害と
見ている。**daw_01 は無事だった** — lock の arrayref が 0.3.9 のままで、`cargo update` を
走らせていなかったから。運が良かっただけで検査は無かったので、`make audit` を足した。

```bash
make audit          # 依存の脆弱性 / yanked / 供給網攻撃の検査 (network 要)
```

- `scripts/lockfile_guard.py` … **ネットワーク不要・常に走る**。lock が git 追跡下か /
  `cargo metadata --locked` が通るか (lock と manifest の乖離) / **既知の汚染リリースが
  完全一致で入っていないか**。範囲 (semver) 判定を要さないものだけを厳密に見る。
- `cargo deny --all-features check advisories` … RustSec advisory DB との突き合わせ。
  **cargo-deny が無ければ `make audit` は明示エラーで落ちる。**「未インストールにつき skip」
  の緑は作らない — ライセンス検査と違い advisories には自前の代替が無く、semver range を
  自前実装すると間違えたときに false green になる (守ろうとしているものを壊す)。
  `cargo install --locked cargo-deny`。
- 方針は `deny.toml` の `[advisories]`。vulnerability / unsound / yanked は全部エラー、
  `unmaintained` は `"workspace"` (自分が直接選んだ依存だけ止める)。
  **`ignore` を足すときは必ず「RUSTSEC-ID: 理由 / 見直し期限」をコメントで書く。**
  無言の ignore は禁止。

新しい汚染事件を知ったら `scripts/lockfile_guard.py` の `KNOWN_COMPROMISED` に
`(name, version, 出典)` を 1 行足す (完全一致なので誤判定が無い)。

### vendored FFmpeg（fresh machine / 手動 worktree で必須）

- `third_party/ffmpeg`（BtbN n7.1 win64 LGPL shared）は **gitignore** で checkout には入らない。
  fresh なマシン（main checkout 自身）では **`make fetch-ffmpeg`** で取得する（idempotent。`make build` /
  `test` / `check` の前提条件にも入れてある）。
- **取得は「latest から発見」ではなく URL + sha256 固定**（2026-08-22 に方針を反転）。
  旧方針は「BtbN の asset 名変更に耐えるよう URL を固定しない」だったが、実際に起きたのは
  **asset 名の変更ではなく asset の消滅**だった（latest に残るのは master / n8.1 / n9.0 だけで
  n7.1 系はゼロ）。発見方式は「見つからなければ落ちる」ので原理的に対応できず、
  third_party を持たないマシンが何もビルドできない状態になっていた。
  BtbN の保持ポリシーは「月末ビルドは 2 年、日次は直近 14 本」なので、固定した URL もいずれ
  404 になる。よって **固定 + 自前ミラーへのフォールバック**をセットで持つ。
  実装は `scripts/fetch_ffmpeg.sh`（pin の SSoT。Makefile に URL を二重化しない）、
  ミラーの作り方と LGPL 上の義務は `docs/ffmpeg_mirror.md`。
  ミラーに置くのはバイナリだけでなく **対応するソース一式**（FFmpeg 本体 + BtbN のビルド
  レシピ + DLL に静的リンクされる外部ライブラリ）。GPL-3.0 §6(d) の義務で、`make ffmpeg-mirror`
  が用意する（**アップロードは自動化しない。外部に出る操作は人がやる**）。
- **Claude Code の worktree（`--worktree` / EnterWorktree / subagent）は自動コピー**: リポジトリ直下の
  `.worktreeinclude`（`/third_party/`）により、新 worktree 作成時に main checkout から ffmpeg が
  **実コピー**される（junction ではない）。よって Claude が作る worktree では `make fetch-ffmpeg` は不要。
  ただし `.worktreeinclude` は「main にある物を持ち込む」だけなので、main checkout 自身が未取得なら
  先に `make fetch-ffmpeg` しておくこと。手動 `git worktree add` で作った worktree は `.worktreeinclude`
  を経由しないので従来どおり `make fetch-ffmpeg` する。参考: https://code.claude.com/docs/en/worktrees
- git に無いので **rm / `git worktree remove` で third_party junction を辿ると本体が消えて
  復元不能**になる（2026-06-14 に worktree 削除が内部の third_party junction を辿って本体を
  削除した事故あり → `make fetch-ffmpeg` で復旧）。**`.worktreeinclude` 経由は実コピーなのでこの
  junction ハザードは無い**が、手動で junction を張った場合は worktree を消す前に内部の reparse
  point を `cmd //c rmdir <junction>` で外してから削除すること。

### ビルドと検証の区別（重要）

- `cargo clippy` / `cargo check` / `cargo test` は**実行バイナリを生成しない** or 生成してもテストビルドのみ。
  手動で `./target/debug/daw_gui.exe` を走らせる前に、必ず `cargo build` を明示する
- 子プロセス（daw_audio / daw_plugin_host）の挙動を変えたときは、子プロセスのバイナリも再生成が必要。
  `cargo build -p <crate>` または `cargo build --workspace`
- `cargo run -p daw_gui` は dependency crate も自動ビルドしてくれるが、既に起動中のプロセスのバイナリは上書きされない場合がある（Windows の ERROR 5）。必要なら先に全プロセスを終了

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
  daw_gui は `daw_ui_platform::WinitWindow` (上流の正準実装) を直接使う
  (Phase 4 で手写しミラー DawGuiWindow を撤去。TSF/IME 配線・入力 mapping も上流に一本化)
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

### プラグインエディタ窓と Win32（Windows）

- **エディタ窓は daw_plugin_host が所有する owner 無しの top-level**
  (`daw_plugin_host/src/editor_window.rs`)。plugin-main スレッドで `CreateWindowExW` し、
  同じスレッドで CLAP `clap_plugin_gui.set_parent` / VST3 `IPlugView::attached(kPlatformTypeHWND)`
  を呼んでプラグイン側を子ウィンドウ化する。設計正本は `docs/plan_plugin_editor_topwindow.md`
- **daw_gui を owner にしてはいけない**。`GetAncestor(.., GA_ROOTOWNER)` が daw_gui に解決すると、
  JUCE (Scaler 2 等) が cascade サブメニューを `Process::isForegroundProcess()` 判定で即 dismiss
  する。旧実装 (daw_gui が窓を作り HWND を IPC で子プロセスへ渡す) がこれを踏んでいた (FIXME #31)
- 上の帰結として **HWND は IPC を渡らない**。protocol に HWND の field は無い。
  `gui_set_parent_hwnd(u64)` の `u64` は plugin format 非依存にするためのプロセス内表現で、
  プロセス境界とは無関係 (`HWND` は `windows` crate で `HWND(*mut c_void)`)。
  daw_gui 内で HWND を持ち回る箇所 (preview 窓の owner / file dialog の owner-modal 化) も
  すべて同一プロセス内
- **エディタ窓を操作している間、daw_gui は非フォーカスどころか foreground プロセスですらない**
  (上記のとおり意図的)。「アプリ全体がアクティブか」は daw_gui 内の情報だけでは原理的に
  判定できないので、エディタ窓の WNDPROC が `WM_ACTIVATEAPP` を拾い
  `PluginEvent::HostWindowsActive` で daw_gui へ報告する (r.md #49 アイドル省電力)
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
  再発フリクション）… `/promote-reflection` skill で **guard (guards.jsonl) / skill / command / memory に昇格**、
  新規 hook 種別は user 承認キュー、不要なら dismiss。**memory で終わらせない**（それが旧ループの欠陥 =
  提案が actuate せず memory だけ増えた）。backlog は per-project user dir、全 worktree 共有・git 外。
  `done` / `dismissed` は終端で再浮上しない。

#### Guard layer（メモリ → 能動的強制力）= 旧ループ最大の欠陥の修正

feedback メモリは `<system-reminder>` の **受動的背景** として recall されるだけで「ミスの瞬間」に
強制力を持たない（= 「メモリに書いた、でも同じミスを繰り返す」）。唯一 action-time に効くのは
PreToolUse hook。従来はメモリを hook 化するのに bespoke スクリプト + settings.json 編集
(classifier ブロック) が要り重すぎて actuate せず、backlog で dismiss → 再発していた
（例: cd-prefix 再発は guard 経路が無く dismiss され続けた）。

**解決 = データ駆動の汎用ガードエンジン (Python・追加依存なし・cross-platform)**:
- **`.claude/guards.jsonl`（リポジトリ追跡下）** に 1 行 1 ルール。各行は feedback メモリ 1 件の
  能動的強制（`source` でメモリへ逆リンク = SSoT）。
- `scripts/guard_engine.py`（PreToolUse）が全ルールを適用。**一度だけ** settings.json に登録すれば、
  以後ガード追加は **guards.jsonl に 1 行追記するだけ**（新規スクリプトも settings 編集も承認も不要
  = classifier ブロック回避）。これが loop 自律化の鍵。`warn`=stdout/exit0、`block`=stderr/exit2（取消）。
- 発火は `guard_hits.jsonl` に記録 → `reflect.py` が **warn ガードが 3 つ以上の異なる session で発火したら
  自動で warn→block に昇格**（人手 triage 不要で actuate）。昇格状態は
  **`<state>/guard_state.json`（git 外の overlay）** に書き、**追跡ファイルは一切書き換えない**。
- 正規表現 1 本で表せない security block（`check_destructive_delete.py` の per-statement 分割）は
  code hook のまま。**pattern guard は data、logic guard は code** の分離。
  cwd との関係でしか判定できないもの（`worktree_outside` / `cd_redundant` / `ask_multi`）も
  engine 側の logic field で、ルール行は action/msg だけを供給する。

#### なぜ追跡下なのか（2026-08-22 の消失事故）

`guards.jsonl` は元々 user dir にあった。理由は「reflect.py が昇格をこのファイルに書き戻すので、
git に入れると全 worktree が毎 Stop で dirty になる」。つまり **性質の違う 2 つ（人が書いたルール
本体 ＝ 恒久的なプロジェクト知識 / 昇格状態 ＝ 実行時状態）が 1 ファイルに同居**していて、
可変な方が恒久的な方をバージョン管理の外へ引きずり出していた。

結果、レジストリが丸ごと消えた。`guard_hits.jsonl` の最終発火が 08-17、発覚が 08-22。
**5 日間、全セッションでパターンガードが 1 件も効いていなかったのに症状がゼロ**だった
（engine が `if not isfile: return 0` の fail-open で、「無い」と「該当しない」が区別できなかった）。

対処は分離:
- ルール本体 → `.claude/guards.jsonl`（追跡。CLAUDE.md や `.claude/hooks/` と同じ class）
- 昇格状態 → `<state>/guard_state.json`（git 外。消えても `guard_hits.jsonl` から再計算できる）
- レジストリ不在・空・全行 parse 失敗は **session ごとに 1 回、目に見える警告**を出す（黙って通さない）
- パス導出は `scripts/ahe_paths.py` に集約。repo root は `__file__` から、state dir は
  **main checkout の slug** から導出する（マシン固有パスをハードコードしない）

**`escalate: false` を外さないこと。** substring レベルのマッチャは nudge としては妥当でも、
block にすると正当な作業まで取り消す。理由は各ルールの直前にコメントで書いてある。

ループ構造（observe → reflect → **actuate** → close）:
1. `PostToolUse` で `scripts/log_metric.py` が metrics jsonl に追記し、`scripts/guard_engine.py` が
   `guards.jsonl` の各ルールをその tool 呼び出しに対し発火・`guard_hits.jsonl` に記録
2. `Stop` で `scripts/reflect.py` が **warn ガードの自動 block 昇格** + Bash 連続失敗を backlog に upsert
   （id でデデュープ、status / sessions 付き、truncate しない。ノイズだった read/edit/bash hotspot 検出は撤去）
3. 次 session 開始時、`SessionStart` hook が OPEN 行を Required Action として強制提示
4. `/promote-reflection` で各行を終端。**機械化できる再発は guard (guards.jsonl 追記) が主経路で
   settings.json 不要・承認不要**。新規 hook 種別の登録だけは user 承認が要るので backlog の
   "hook requests" 節に ready-to-paste spec を書いて依頼する

詳細設計は `docs/plan_ahe.md` (章 1.5 autonomy spectrum + 章 3 H13/H14/H15)。

### hook 配置ポリシー

- **hook / スクリプトは PowerShell 禁止。bash を既定**とし、**JSON を構造的にパースする必要がある
  ものだけ Python (stdlib のみ、jq 不要)** で書く (Linux でも動くこと。PS 5.1 の encoding 地獄 +
  Windows 専用を排除。memory [[feedback_no_powershell_cross_platform]])。
  - JSON 解析が要る → Python: `guard_engine.py` (ルール DB) / `reflect.py` (hits/metrics 解析 +
    ルール再シリアライズ) / `log_metric.py` (Windows パスの `\` を含む JSON 出力) /
    `check_destructive_delete.py` (security block・確実な command 抽出 + `\b` 正規表現)。
  - JSON 不要 (テキスト/git/build) → bash: `cleanup_worktree.sh` 等。新規 .ps1 は guard が block する。
- AHE hook (PreToolUse / PostToolUse / Stop) は **`.claude/settings.json`** に集約する。
  このファイルは git 追跡対象なので、新規 worktree でも何もせず hook が有効になる。
- `.claude/settings.local.json` は **マシン固有 permissions allowlist 専用** に残す
  (gitignore 対象、harness が worktree 開始時に同期する想定)。
- hook を追加したくなったら必ず `settings.json` 側に追加すること。
  `settings.local.json` に hook を書くと、harness の同期次第で worktree に伝わらず
  AHE ループが片肺になる。
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

### テスト
- js テストで対応できるものはユーザ確認を依頼しない

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

2026-07-03 の全体改修 (`docs/plan_arch_refactor.md`) で確立。**`make arch-lint` が機械検査**し、
`/arch-review` skill が定期監査する。これらに触れる変更は plan_arch_refactor.md を先に読む。
書く瞬間の強制は `.claude/guards.jsonl` の `arch-*` ルール (plan §11 の 5 件: INFINITE /
positional tuple key / push_undo_snapshot 直呼び / untagged 追加 / MainToChild 復活)。
**「何を違反とみなすか」の SSoT は `scripts/arch_lint.sh`**、ガードはその write-time ミラー。

> **arch-lint のパターンにバックスラッシュを使わないこと。** make (MSYS2) 経由だと
> grep/sed へ渡す引数のバックスラッシュが落ち、`\( \s \b \[` を含むパターンが無言で
> 別物になる。2026-08-22 に発覚 — 8 チェック中 6 つが無効化され、違反 7 行を抱えたまま
> 「OK (違反なし)」を出していた。POSIX ブラケット式 (`[(]` `[]]` `[[:space:]]`) と
> `grep -w` で書く。arch_lint.sh は冒頭に canary を持ち、検査器自身が効いていなければ
> exit 1 する (違反ゼロの報告を無条件に信じない)。

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
9. **god file budget**: 手書き .rs は 1 ファイル 3,000 行以内。超過したら分割してから足す
   (app.rs 25k 行の再発防止)。

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
- `sing_like_coding` (作者ローカルの別リポジトリ) — 前作 Rust DAW。IPC, CLAP ホスト,
  オーディオエンジンの参照実装
- `%APPDATA%\REAPER\Scripts\<user>\voicevox\` (作者ローカル) — VOICEVOX API 統合の参照実装 (Lua)
