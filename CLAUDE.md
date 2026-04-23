# daw_01

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。詳細は [DESIGN.md](DESIGN.md)。

## プロジェクト構成

Cargo workspace (Edition 2024)。

```
common/            -- 共有型・IPC プロトコル・shared memory・データモデル
daw_gui/           -- GUI プロセス (Vizia)
daw_audio/         -- Audio Engine プロセス (CPAL)
daw_plugin_host/   -- Plugin Host プロセス (CLAP/VST3)
```

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

### IPC 境界で送る型

- `MainToChild` / `ChildToMain` 等の protocol 型、およびそれが保持する内側の型すべてに
  `#[derive(bincode::Encode, bincode::Decode)]` が必要
- Song / Track / Clip / Row 等のモデル型も protocol 経由で渡す場合は bincode derive を追加する

### GUI デバッグ

- UI のキーバインド・イベントは可視フィードバックが無いと動いたか判別不能
- 迷ったら `AppData::event` 冒頭に `tracing::info!(?app_event, "received")` を仕込んでログで確認
- 確認後は削除するか、debug feature で囲う（`[debug-gui]` skill 参照）

### Vizia 0.3 の既知の罠（実ビルドでハマった項目）

- メソッド名: `Slider::on_changing` ではなく `on_change`。callback は `(ex, value)` で emit
- `Alignment::Bottom` は存在しない。`BottomCenter` / `BottomLeft` / `BottomRight` のいずれか
- `List` のデフォルトテーマが `list list-item { height: 30px }` を指定しているため、
  per-row ラベルに空白が入る。`cx.add_stylesheet(&'static str)` でインライン CSS オーバーライド。
  ただし `height: auto` にすると Skia の `matrix.invert().unwrap()` で panic する
  （`draw.rs:35` 付近）ので、必ず固定 Pixel にする
- `#[derive(Data)]` が無い型（今回の `Song` 等）は `Binding` に渡せない。事前レンダリングした
  `String` や `Vec<String>` を Lens で bind する
- `cx.spawn(|proxy| ...)` は **std::thread** を回すので `tokio::time::sleep` などは使えない。
  `std::thread::sleep` + `proxy.emit(...)` でやる。`ContextProxy::emit` は UI が閉じられると
  `Err` を返すのでそれで抜ける

### AppEvent に f32 を乗せる罠

`Keymap` API が `Hash + Eq` を要求するため、`#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]`
を付けた `AppEvent` に `f32` を含めるとコンパイルエラー。`u32` (`f32::to_bits`) で運んで
受信側で `f32::from_bits` で戻すパターンを踏襲する（`SetMasterGain`, `Tick(_, peak_l, peak_r)` 参照）。

### 子プロセスとクロスプロセス HWND（Windows）

- `HWND` は `windows` crate で `HWND(*mut c_void)`。IPC 越しに渡すには `u64` にキャストして bincode
- CLAP プラグインウィンドウを daw_gui に埋め込むとき、daw_gui が所有する top-level HWND を作り、
  `daw_plugin_host` へ `u64` で送る → `clap_plugin_gui.set_parent` でプラグインが子ウィンドウ化
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

## 応答・コミット

- 応答は日本語
- コミットメッセージは日本語
- 技術用語は英語のまま使用可

## Coding Principles

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

### 外部 API の挙動を先に理解する
- 推測で実装→失敗→修正のサイクルは、調査→実装より遅い
- CLAP, clap-sys, cpal, Vizia, windows crate の挙動はドキュメント・ソースで確認してから組む

### エラーを握りつぶさない
- `?` を安易に `ok()` / `unwrap_or_default()` に置き換えない
- FFI・CLAP コールバック・IPC のエラーは根本原因を調査してから対処

### 要件にない変更を入れない
- 既存の挙動を勝手に変えない
- バグ修正ついでのリファクタリングは別コミット

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

## Debugging Methodology

- **実データから始める**: コードパス推論より実データ観察が速い
- **フルサイクルで検証する**: 個別関数が正しくても、パイプライン全体が壊れていれば無意味
- **上流→下流の順で調査する**: UI/コマンド → Model → IPC → Plugin Host → プラグイン本体
- **UI イベントは常時ログ無し**: GUI のキーバインド・ボタンクリックは、何も起きないとき「キー拾えてない」「emit されてない」「handler が間違い」の 3 層で切り分ける必要がある。`tracing::info!` を各層に仕込む

## 参照プロジェクト

- `F:\dev\sing_like_coding` — 前作 Rust DAW。IPC, CLAP ホスト, オーディオエンジンの参照実装
- `%APPDATA%\REAPER\Scripts\yoshino\voicevox\` — VOICEVOX API 統合の参照実装 (Lua)
