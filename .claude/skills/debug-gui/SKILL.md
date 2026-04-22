---
name: debug-gui
description: |
  GUI のキーバインド・ボタン・イベントが期待通り動かないときの切り分け手順。
  「キーを押しても何も起きない」「ボタンが効かない」「カーソルが動かない」等、
  UI のフィードバックだけでは原因が特定できないときに発動。
  トレース挿入 → 再ビルド → 実行 → ログ確認 → 該当層を修正の流れを提供する。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(./target/debug/*)
---

# GUI デバッグワークフロー

GUI のイベント（キー入力・ボタンクリック・ショートカット）が動作不明なときに、
どの層で止まっているかを切り分ける。

## 3 層モデル

GUI イベントは概ね以下の 3 層を通る。どの層で消えているかでアプローチが変わる。

```
┌─ 1. キー取り込み層 ───────────────┐
│ OS → winit → Vizia event pipeline │
│ Keymap / view の .event / focus   │
└──────────────────────────────────┘
           ↓
┌─ 2. emit 層 ────────────────────┐
│ KeymapEntry closure →           │
│ cx.emit(AppEvent::...)          │
└──────────────────────────────────┘
           ↓
┌─ 3. handler 層 ───────────────────┐
│ Model::event → event.map(...)    │
│ → 実際の state 変更              │
└──────────────────────────────────┘
           ↓
     Lens → View 再描画
```

## 手順

### 1. 期待動作を明確にする

「何を押したら何が起きるはずか」を書き出す。例:
- Space → AppEvent::PlayToggle → `is_playing` が反転、送信

### 2. 各層にトレースを仕込む

**handler 層（最上流で一番わかりやすい）**:

```rust
// daw_gui/src/app.rs の Model::event で、event.map の中に
event.map(|app_event, _| {
    tracing::info!(?app_event, "AppEvent received");
    // 以下既存のハンドリング
});
```

**emit 層（必要なら）**:

```rust
// Keymap の closure を個別に
KeymapEntry::new(AppEvent::Foo, |cx| {
    tracing::info!("keymap Foo fired");
    cx.emit(AppEvent::Foo);
})
```

Keymap はそれぞれ別 closure なので個別に log を仕込める。

**キー取り込み層（最下流、一番面倒）**:

```rust
// View::event を impl する場合
fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
    event.map(|window_event, _| {
        if let WindowEvent::KeyDown(code, key) = window_event {
            tracing::info!(?code, ?key, "keydown raw");
        }
    });
}
```

または global ウォッチャーを仕込む。

### 3. **再ビルドを明示**（必須）

```bash
cargo build -p daw_gui
```

`cargo clippy` / `cargo check` / `cargo test` だけでは **exe が更新されない**。
古いバイナリで検証すると「直したはずなのに動かない」で時間を溶かす。
（このプロジェクトで 2 回繰り返している。[feedback_build_after_clippy.md] 参照）

子プロセス側（daw_audio / daw_plugin_host）の挙動に関わるデバッグなら
`cargo build --workspace` で workspace 全体を rebuild する。

### 4. 実行してキー操作 → ログを確認

```bash
./target/debug/daw_gui.exe
```

（必要なら `DAW_CLAP_PATH=...` 等の env 付きで）

操作したら閉じて `grep` でログを絞る:

```bash
grep -E "AppEvent|keymap|keydown" <output-file>
```

### 5. どこで止まったかで切り分け

| 現象 | 原因の候補 | 対応 |
|---|---|---|
| keydown raw すら出ない | キーが winit まで届いていない / window focus が他にある / 他の consumer（Button 等）が飲んでいる | Button の navigable(false) を試す、global listener を使う、focus を明示的に View へ移す |
| keydown raw は出るが keymap fired が出ない | Keymap のエントリ漏れ / Modifier の組み合わせ違い / 同じ Code に複数エントリで競合 | `KeyChord::new(Modifiers::X, Code::Y)` を見直す、CTRL+H と plain H のような重複は Modifiers が優先される |
| keymap fired は出るが AppEvent received が出ない | `cx.emit` がどこにも届いていない / Model が build されていない / Lens の所有者が違う | `AppData::new(...).build(cx)` が Application::new 内で呼ばれているか、build するときの `cx` が root か確認 |
| AppEvent received は出るが画面が変わらない | handler 内の state 変更が反映されていない / Lens 先の型が Data を実装していない / `refresh_tracker_text` 忘れ | `dirty` フラグの扱い、Vizia Data trait の要件、`tracker_text: String` のような派生 String Lens を使う |

### 6. 仕込んだトレースの後始末

確認が終わったらトレースを削除するか、debug feature で囲む:

```rust
#[cfg(feature = "debug-gui")]
tracing::info!(?app_event, "AppEvent received");
```

残しておくと RT 外とはいえ毎キー log が出てうるさい。

## Vizia 固有のハマりどころ

- **`Keymap` はグローバル**: focused view に依存せず発火するはずだが、Button が focus を奪って
  Space / Enter を先に飲むことがある。Button に `.navigable(false)` を付けるか、
  Keymap を Window root で build する
- **`Lens::map` の Target は `Data` 必須**: Song のような複雑な型は `Data` 未実装で Binding 不能。
  派生する `String` や `u32` を AppData に持たせて Lens 化（daw_01 では `tracker_text: String`）
- **`#[lens(ignore)]`**: Sender 等の非 `Data` フィールドには付けないと derive で詰まる
- **Vizia 0.3.0 (crates.io) と main (GitHub) で API が違う**: Signal ベースのコード例を見つけたら
  それは main 用、0.3.0 では Lens を使う

## 参考コミット

- `4312dab` hjkl カーソル + ノート入力・編集（本ワークフローで bug を切り分けた）
- `ccf5b1b` hjkl カーソル移動（Song が Data 実装していないので tracker_text 方式に）
