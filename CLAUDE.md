# gui_01

Rust 製・モデルを Clone しない DAW 向け GUI ライブラリ。GUI のみを扱い、audio / IPC には一切関知しない。

設計の詳細は [docs/plan.md](docs/plan.md) を参照 (正本)。

## クレート構成

Cargo workspace (Edition 2024)。

```
crates/platform/  (daw-ui-platform) -- winit 抽象、WindowBackend trait + 中立 AppEvent
crates/renderer/  (daw-ui-renderer) -- wgpu パイプライン (rect / glyph / line)
crates/ui/        (daw-ui-core)     -- Ui<'a, M> / Edit<M> / widgets / LayoutPass
crates/examples/  (mixer / waveform_validation 等) -- 動作確認サンプル
```

## Development Workflow

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
cargo run --bin mixer                 # mixer 動作確認
cargo run --bin waveform_validation   # 波形 UI 動作確認
cargo bench -p daw-ui-core            # criterion ベンチ
cargo test -p daw-ui-core --test no_clone_required   # trybuild (no-Clone 制約)
```

### ビルドと検証の区別

`cargo clippy` / `cargo check` / `cargo test` は **実行 exe を生成しない** (or test 用のみ)。
example を実機検証する前に必ず `cargo run --bin <name>` または `cargo build` を明示する。

## 設計上の不変条件 (load-bearing)

実装中は必ず守る。緩めない。

- ユーザ Model 型に `Clone` / `PartialEq` / `Hash` / `Default` を要求しない
- メッセージ型を導入しない。`Edit<M>` は `Box<dyn FnOnce(&mut M)>` で、`Application::Message: Clone` 伝染を構造的に防ぐ
- `derive` マクロ (Lens 等) 禁止
- 差分検出は widget ID + プリミティブ末端値の hash のみ。ユーザ Model の構造体全体は触らない
- `Ui<'a>` の `'a` で借用ライフタイムを統一、GAT は使わない (stable Rust)
- ライブラリは audio / IPC / プロセス間通信に一切関知しない。`Edit` を返したところで責務を切る

これらは `crates/ui/tests/no_clone_required.rs` (trybuild) で CI 固定済み。

## 応答・コミット

- 応答は日本語
- コミットメッセージは日本語
- 技術用語は英語のまま使用可

## Coding Principles

### 最新の安定版を使う
- Rust Edition 2024 / 各 crate は最新版
- deprecated な API を新規コードで使わない
- `let-else` で早期リターン、`?` を `match` より優先

### KISS / DRY
- 最小限の実装で目的を達成する。不要な抽象化を作らない
- 同じロジックを複数箇所に書かない
- 1 関数 1 責務、3 回繰り返されたら抽象化を検討

### Single Source of Truth
- 同じデータを複数箇所に複製しない
- 「この値は誰が所有し、誰が更新するか」を明確にしてから実装する

### 外部 API の挙動を先に理解する
- 推測で実装→失敗→修正のサイクルは、調査→実装より遅い
- wgpu / winit / glyphon / taffy はバージョン間で breaking change が多い。**該当バージョン** のドキュメント・examples を確認してから書く (後述「既知の罠」も参照)

### エラーを握りつぶさない
- `?` を安易に `ok()` / `unwrap_or_default()` に置き換えない
- wgpu / winit からのエラーは根本原因を調査してから対処

### 要件にない変更を入れない
- 既存の挙動を勝手に変えない
- バグ修正ついでのリファクタリングは別コミット
- 仕様外のデフォルト値・初期状態・キーバインドを勝手に入れない

### 新しく入れた抽象は次の機会に使う
- 新しい API / 抽象 (型/トレイト/モジュール) を入れた直後のタスクでは、その抽象を実用するのが既定
- 「KISS で見送る」と判断する場合は、なぜ今ここで使わないかを **明示的に書く**
- 例: `LayoutPass` の Phase 5 で `Padding` / `Gap` / `flex_grow` を入れたら、Phase 6 (mixer 拡張) では使う

## Debugging Methodology

- **実データから始める**: コード推論より実データ観察が速い
- **フルサイクルで検証する**: 個別関数が正しくても、`Ui::frame` → render → 表示 のサイクル全体が壊れていれば無意味
- **上流→下流の順で調査する**: OS イベント → winit AppEvent → InputAccumulator → Ui::frame → widget → Edit → Model → render
- **UI イベントは可視フィードバック必須**: GUI のクリック・キーバインドは「動いた / 動いてない」が見えない。`tracing::info!` を該当層に仕込んで切り分ける ([.claude/skills/debug-ui/SKILL.md](.claude/skills/debug-ui/SKILL.md) 参照)

## 既知の罠 (winit / wgpu / glyphon / taffy)

実ビルド/動作で踏んだ落とし穴。新規実装で再発させない。

### winit 0.30
- **Alt-Tab 復帰直後のクリック** で OS が `WM_MOUSEMOVE` を送らず、winit の `cur_pos` が更新されないまま `MouseInput` が来る。`crates/platform/src/winit_backend.rs` で OS にカーソル位置を問い合わせて synthetic な `PointerMoved` を先に流す対処を入れてある。Alt-Tab 後の最初のクリックの hit-test で空振りする症状を見たら、まずこの workaround を疑う。
- **修飾キー**: `WindowEvent::ModifiersChanged(Modifiers)` は `mods.state()` で `ModifiersState` を取り、`control_key()` / `shift_key()` / `alt_key()` / `super_key()` の **新スタイル名** を使う。0.29 以前の `ctrl()` 等は使えない。
- **Modifiers** は MouseInput より先に届く前提で `InputAccumulator` 単独で track してよい。`MouseInput` に modifier を載せるパスは作らない (二重管理を避ける)。

### wgpu (29.x 系)
- リサイズ中の surface 再構成で `SurfaceError::Outdated` が稀に発生。`render` の戻りがエラーでもログに出して次フレームまで生かす設計。

### taffy 0.10
- **`Dimension::auto()` の flex 親に `flex_basis: 0` の grow-only 子** を入れると fit-content = 0px に潰れて `flex_grow` の比例分配が起きない。`LayoutPass::flex` は親の size を `Dimension::percent(1.0)` で「親の利用可能領域いっぱい」にしてこれを回避している (Phase 5 で発見)。
- **`FlexDirection` / `NodeId` は `taffy::prelude` 経由でしか pub されていない**。`pub use taffy::{...}` ではなく `pub use taffy::prelude::{FlexDirection, NodeId}`。

### glyphon (cosmic-text)
- `Buffer::layout_runs()` で実 measure を取りたい場合、ui crate 側に `FontSystem` への参照経路 (Arc 共有 / measure trait 公開 / 別 FontSystem) が必要。現状 renderer に閉じている。proportional フォントの cursor / preedit pixel-perfect 化は M3 残作業。
- 全テキスト (prefix + preedit + suffix) を **1 つの `GlyphArea`** で描画する方が並びの計算が安定する (HackGen Console NF などの固定幅前提なら ASCII=7 / CJK=14 で近似可能)。

### immediate-mode + Edit queue の必然
- `edits` が出たフレームの scene は **古い model 値** で積まれている (描画クロージャ後に apply されるため)。`on_render` で `had_edits` 検出時に `request_redraw` を 1 回追加で呼んで適用後の値で描き直す。`UiHost::focus_changed_in_last_frame()` も同じ理由で同パターン。

### widget state の downcast
- `state: HashMap<WidgetId, Box<dyn WidgetState>>` から型復元するとき、`Box<dyn WidgetState>` 自身に WidgetState の blanket impl が当たって外側 Box の TypeId を返すバグに注意。`&mut **entry` で明示的に deref してから `as_any_mut().downcast_mut::<S>()` する (M2 で修正、回帰テスト済)。

## 参考リソース

- 設計の正本: [docs/plan.md](docs/plan.md)
- フィードバック / 過去知見: `~/.claude/projects/F--dev-gui-01/memory/`
- skill 一式: [.claude/skills/](.claude/skills) (`implement` / `debug-ui` / `research-similar-impl` / `review`)
