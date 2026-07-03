# daw-ui (旧 gui_01) — daw_01/ui/

Rust 製・モデルを Clone しない immediate-mode GUI ライブラリ。GUI のみを扱い、audio / IPC には
一切関知しない。daw_01 に統合され `daw_01/ui/` に置かれる（旧 sibling repo gui_01）。

AHE ループ / hook / skill / 共通の coding principle は daw_01 root の `CLAUDE.md` と `.claude/` に
一本化済み。この CLAUDE.md は **UI ライブラリ固有の技術ガイド** (クレート構成・load-bearing
invariant・既知の罠) のみを残す。

設計の詳細は [docs/plan.html](docs/plan.html) を参照 (正本)。

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
make build                            # ルート Makefile が SSoT (実行 3 exe)
make test                             # テストを持つ package のみ (TEST_PKGS)
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

### 理想とベストプラクティスを追求する (使う側に boilerplate を強要しない)
- ユーザに同じ workaround を書かせる API は **設計欠陥のシグナル**。利用者全員が同じ boilerplate を書く状況になっていたら、ライブラリで吸収すべき
- 改善のためなら **破壊的 API 変更を恐れない**。単一 workspace + Edition 2024 の利点を活かし、breaking change を入れたら全 example / test / docs を **1 commit で一括更新** する
- 「audio thread に Edit を送るかも」のような **曖昧な future-proof のために現実の全ユーザに boilerplate を強要しない**。必要になってから別 method (`frame_to_edits` 等) を追加すれば十分
- 妥協 (短期 workaround) で進めるしかない場面では、**なぜ短期で済むか / 根本対処の方針** を memory / docs に明示的に残す

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
- **drag 系 widget での Alt 真値**: 上の「Modifiers が先に届く」 性質の裏返しとして、 ユーザが Alt + マウスを **同時に** 離した release frame では `pointer.modifiers.alt` が **既に false** になっていることがある (ModifiersChanged が MouseInput(Released) より先に dispatch されるため)。 drag 中は Alt が押されていたのに release で「カクッ」と grid に飛ぶ symptom の正体。 **対処**: drag session 構造体側で `last_alt: bool` を持ち、 continuation frame (`!primary_just_released`) で毎フレーム update、 release frame では update を skip して直前値を保持する。 **drag overlay (描画 preview) と release commit (Edit 発行) の両方が `nd.last_alt` を読む**。 `pointer.modifiers.alt` を直接見ると overlay と commit が異なる alt 値で snap 計算を走らせて乖離する。 過去 (M9 Phase 60) に sticky alt (`any_alt: 一度でも true なら以後永続 true`) で誤魔化した実装は「Alt を一瞬触っただけで以後永続 raw」 という UX 不自然性も持っていたため、 Phase 60 visual verify follow-up で `last_alt` 単一真値に統一して廃止。
- **drag 系 widget の snap は「絶対位置 snap」 (delta-snap NG)**: `view.snap.snap_beat_delta(raw_delta, alt, zoom)` で **delta だけを round** すると、 anchor が grid 外に既にずれている場合 (例: 前回 Alt+drag で +0.078 拍ずらした) に release してもずれが永久残る (raw_delta が grid 倍数なら snap 後も同じ delta を加算するだけ → user 視点「grid に吸着しない」)。 **正しい実装**: anchor 0 の **編集対象端の絶対位置** (Move/ResizeLeft = `start_beat` / ResizeRight = `start_beat + len_beats`) を `snap_beat(pivot + raw_delta, alt, zoom)` で round → 差分 (`adjusted_delta`) を全 anchor に適用。 これで (a) 単一選択は最終位置が grid 上、 (b) 複数選択は anchor 0 が grid 上に吸着しつつ相対関係維持、 (c) overlay と release commit が同じ helper を共有して描画 / commit 完全一致。 Cubase / Live と同じ「nearest grid alignment」 動作。 helper は widget 内 internal fn `compute_*_drag_beat_delta` (arrangement.rs / piano_roll.rs) として定義済。
- **drag 系 widget の short-click 化閾値**: arrangement / piano_roll の clip / note drag は短い drag を「click」 に格下げする閾値を持つが、 旧 16px は **過剰** で user の「ちょっとずらす / ちょっと伸ばす」 操作を一律 ignore して release で元位置 / 元長さに戻る symptom が出ていた (実用 DAW は 3-5px 程度)。 **正しい設計** は三条件 AND:
  1. **Resize (Left/Right) は閾値関係なく常に commit** (resize handle 上の click は意味がない)
  2. **Move + Alt 押下中は閾値 skip** (Alt は raw 微調整の明示意図)
  3. **Move + Alt なしは jitter 用 4px 閾値** (mouse jitter のみ ignore)
  実装は `let demote = matches!(nd.kind, ClipDragKind::Move) && !nd.last_alt && dist < 4.0;`。 user 視点で「drag したら反映される」 を保証しつつ jitter は ignore できる。
- **隣接 resize widget の共有境界は in-rect 優先 (後勝ち + 外側拡張ハンドルは内側を奪う)**: clip / note のように左右端 resize ハンドルを rect の **内外 ±handle_px** に張る widget は、 隣接要素 (`A.right == B.left`、 連続同音 note / 接触 clip) があると B の左端外側ハンドル `[B.left-px, B.left)` が **A の rect 内部に食い込む**。 hit-test を「visible 走査で match ごとに後勝ち上書き」 で書くと、 cursor が A の rect 内 (`A.right-px ≤ cx < A.right`) でも後ろの B が常に勝ち **A の右端を一切掴めない** (piano_roll #053 / M14 Phase 82)。 **正しい設計**: match を「rect 内部 (in-rect)」 と「外側拡張のみ (outer)」 の 2 tier に分け **in-rect を outer に無条件優先** (同 tier は resize edge への水平距離が近い方、 同距離は後勝ち)。 各 widget が「自分の rect 側ハンドル px を所有」 し、 共有境界は半開区間で後者 (B) 内側になる。 piano_roll は `note_hit` / `note_hover_cursor` 共有の internal `note_hit_in` で実装済 (hover カーソルが指す要素 = drag で掴む要素 を構造保証)。 **arrangement の `clip_hit` も M14 Phase 125 (#101) で同じ in-rect 優先ループに統一済** (旧「後勝ち上書き」 で隣接 clip の A 右端が B に奪われていた bug を解消)。 今後 左右端 resize ハンドルを内外に張る新 widget を足すときは、 この 2 tier (in-rect 無条件優先 / 同 tier は edge 距離) を最初から踏襲する。

### TSF (Windows IME / `ITextStoreACP` — M15)
text_input を TSF text store として OS IME に公開し、rtry (Try-Code TIP) のまぜ書き `GetText` / MS-IME 再変換を成立させる経路 (`crates/platform/src/tsf/`、Windows 限定)。設計は [docs/plan_tsf_ime.html](docs/plan_tsf_ime.html)。
- **`AssociateFocus` 必須 (`SetFocus` だけでは不可)**: `ITfThreadMgr::SetFocus(doc_mgr)` は thread の focus doc を設定するだけで **document を HWND に束縛しない**。window が OS focus を得ると msctf は CUAS の既定 document を使い、TIP の編集が我々の `ITextStoreACP` に届かない。症状: rtry ログ `ShiftStart(-10) shifted=0` / `TSF read failed, using postbuf fallback`、まぜ書きが postbuf の backspace 再現で「ねこ→ね」 のようにズレる (= store が空に見えている)。`ITfThreadMgr::AssociateFocus(hwnd, doc_mgr)` で束縛して解決。focus 取得時に `AssociateFocus` + `SetFocus` の両方を呼ぶ (前者は次の focus 変化で効くため後者で即時反映)。
- **winit はデフォルト IME 無効**: 生成時に `set_ime_allowed(window, false)` される。focus 中に app が `set_ime_allowed(true)` を呼ばないと IME を ON にできない (TSF doc focus だけでは不足)。既存の「app が `ime_request()` で IME enable を駆動」 contract は不変で、TSF は純粋に additive (daw_01/mixer/piano_roll は無改修で TSF を得る)。
- **STA apartment は winit が保証**: winit が `OleInitialize` で event loop スレッドを STA 化するので `CoInitializeEx(APARTMENTTHREADED)` は `S_FALSE` (既に STA) を返す = 正常 (`did_coinit` で balance)。`RPC_E_CHANGED_MODE` (既に MTA) 時のみ TSF を諦め winit IMM に fallback。TSF COM は STA / `Rc` 保持で **非 Send** なので `WinitWindow` (Send 要求) に持たせず UI スレッド thread-local (`TsfSlot`: Untried/Failed/Active、初期化は 1 度きり試行) に置く。
- **ACP ⇔ byte と invariant 型の Default**: TSF は UTF-16 code-unit offset (ACP)、widget は UTF-8 byte。`AcpMap` で相互変換 (サロゲートは char 先頭へ丸め)。**`#[derive(Default)]` で空 `Vec` になり `len()-1` が underflow した実バグ** → sentinel `[0]` を持つ Default を手実装 + `saturating_sub` 防御。invariant を持つ型は `build()` だけでなく **Default/空構築もテストする**。
- **COM shim の lint**: windows API の wildcard import / `#[implement]` マクロ生成 (`inline_always`) / ACP i32 cast / out-param raw pointer は不可避なので COM module 単位で `#![allow(...)]`。
- **検証は実機 + rtry ログ必須**: 単体テストは純粋ロジック (AcpMap/DocState) のみ。COM 経路は `examples/text_input_ime` (IMM 不介入＝TSF のみ) を rtry 有効化で起動し、gui_01 側 trace + rtry の `%TEMP%\rtry_debug.log` (`text before cursor = '...'` が非空か) で往復を確認する。

### wgpu (29.x 系)
- リサイズ中の surface 再構成で `SurfaceError::Outdated` が稀に発生。`render` の戻りがエラーでもログに出して次フレームまで生かす設計。
- **offscreen rendering** (Phase 18 で `OffscreenRenderer` を実装した際に確定):
  - `Maintain::Wait` は 28 以前の API。29 では **`PollType::wait_indefinitely()`** に置換 (`device.poll(PollType::wait_indefinitely()).unwrap()` が定型)。`Maintain` を import すると型エラー。
  - `compatible_surface: None` で adapter 取得可 (native は OK、WebGL2 のみ surface 必須)。プラグイン UI 埋め込みや snapshot 用途で window なしに使える。
  - `copy_texture_to_buffer` の引数は **`TexelCopyTextureInfo` / `TexelCopyBufferInfo` / `TexelCopyBufferLayout`** (29 の新名称)。`ImageCopyTexture` 等の旧名は使えない。
  - `bytes_per_row` は **`COPY_BYTES_PER_ROW_ALIGNMENT` (= 256) の倍数必須**。`unpadded.div_ceil(256) * 256` で staging buffer に padding し、readback 後に row 単位で詰め直す。`Queue::write_texture` には適用されない。
  - `map_async` + `poll(Wait)` 順序: コールバック登録 → `device.poll(PollType::wait_indefinitely())` の順。逆にするとコールバックが永遠に呼ばれない。
  - `DeviceDescriptor` の `trace: Trace::Off` / `experimental_features: ExperimentalFeatures::disabled()` フィールドが 29 で必須 (省略不可)。`device.rs` / `offscreen.rs` 双方で全フィールド明示。
  - sRGB 二重変換は **起きない**: `Rgba8UnormSrgb` で render → そのまま PNG `ColorType::Rgba` に渡せる (PNG decoder は sRGB 仮定でデコードするので一致)。バイト単位で snapshot 比較するなら `Rgba8Unorm` (linear) を選ぶ判断もあり。
- **uniform buffer の LAST WRITE WINS trap** (M14 Phase 78 で発覚): pipeline instance が **1 つだけ** uniform buffer を保持して同 encoder 内で複数の draw call から `queue.write_buffer` で値を書き換えると、 GPU は submit 時に **最後の write** の値を全 draw が読む (= 各 draw が異なる uniform を期待しても全部同じ値を見る)。 `queue.write_buffer` は deferred で encoder の draw 順とは無関係に submit 直前に 1 度書く。 対処: (a) **per-call で `device.create_buffer`** して各 draw が独自 buffer を参照 (`pipelines/text_effect.rs::run_blur_pass` / `run_composite_pass` 参照)、 (b) **dynamic offset uniform** で 1 buffer + 複数 offset、 (c) **encoder.copy_buffer_to_buffer** で encoder 内に書き込みを order する。 multi-pass / per-instance uniform を扱う pipeline では **(a) を default 設計** にする。 symptom: 複数 effect で全部が最後の効果に化ける、 視覚的に「動いてるように見える」 が pixel 単位 verify で破綻が見つかる ← この性質ゆえ visual smoke「見える」 で OK にせず pixel verify を徹底すること (memory: `feedback_no_excuse_pixel_verify`)。
- **LAST WRITE WINS の対: 別 submit なら安全** (M14 Phase 93 で確定): 上の trap は **1 つの submit (encoder) 内**で buffer を多重 write して多重 draw が読む場合のみ起きる。 `queue.write_buffer` は次の `queue.submit` で「その submit までに積まれた write を、 command 実行の **前** に flush」 するので、 `write(A) → submit(A) → write(B) → submit(B)` は各 submit が個別の値を読む。 = **別 submit ごとに begin_frame/upload/render/submit を完結させる経路 (例: `composite_scene_to_texture` を呼ぶ毎に独自 encoder を submit) は、 既存 pipeline (rect/line/glyph/texture) を main `render()` と流用しても screen uniform を破壊しない**。 専用 pipeline を増やす (= GlyphPipeline の FontSystem 二重ロード等) 必要はない。 multi-submit-per-frame な新経路を足すときは「同 submit 内に複数 write が無いか」 だけ確認すればよい。

### taffy 0.10
- **`Dimension::auto()` の flex 親に `flex_basis: 0` の grow-only 子** を入れると fit-content = 0px に潰れて `flex_grow` の比例分配が起きない。`LayoutPass::flex` は親の size を `Dimension::percent(1.0)` で「親の利用可能領域いっぱい」にしてこれを回避している (Phase 5 で発見)。
- **`FlexDirection` / `NodeId` は `taffy::prelude` 経由でしか pub されていない**。`pub use taffy::{...}` ではなく `pub use taffy::prelude::{FlexDirection, NodeId}`。

### glyphon (cosmic-text)
- `Buffer::layout_runs()` で実 measure を取りたい場合、ui crate 側に `FontSystem` への参照経路 (Arc 共有 / measure trait 公開 / 別 FontSystem) が必要。現状 renderer に閉じている。proportional フォントの cursor / preedit pixel-perfect 化は M3 残作業。
- 全テキスト (prefix + preedit + suffix) を **1 つの `GlyphArea`** で描画する方が並びの計算が安定する (HackGen Console NF などの固定幅前提なら ASCII=7 / CJK=14 で近似可能)。

### immediate-mode + Edit queue (M6 で UiHost が自動対処、利用者の boilerplate 不要)
- 旧設計 (M5 まで): `edits` が出たフレームの scene は **古い model 値** で積まれている (描画クロージャ後に apply されるため)。利用者は `for e in edits { e.apply(&mut model) }` + `had_edits` 判定 + `window.request_redraw()` の boilerplate を全 example で書く必要があった (sample_edit_ops で漏れて発覚)。
- M6 commit 以降: `UiHost::with_window(Arc<W>)` で構築 + `ui.frame(&mut model, ...)` で **apply 内蔵 + 自動 `request_redraw`**。利用者の boilerplate は **完全排除**。
- audio thread 連携等の advanced 用途では `frame_to_edits(&model, ...) -> Vec<Edit<M>>` を使う (利用者が apply タイミングと request_redraw 制御)。
- offscreen / headless test では `UiHost::no_redraw()` を使う。

### widget state の downcast
- `state: HashMap<WidgetId, Box<dyn WidgetState>>` から型復元するとき、`Box<dyn WidgetState>` 自身に WidgetState の blanket impl が当たって外側 Box の TypeId を返すバグに注意。`&mut **entry` で明示的に deref してから `as_any_mut().downcast_mut::<S>()` する (M2 で修正、回帰テスト済)。

### text_input の buffer 動作 (M14 Phase 59 で uncontrolled 化)
- **`text_input_at` は `was_focused == true` 中は `TextInputState.buffer_text` を source-of-truth にする** (uncontrolled mode)。`text` 引数は **gained_focus 時の初期値** としてのみ使われ、 focus 中の typing は buffer を mutate する (毎フレーム reset しない)。 これにより piano_roll の歌詞 inline 編集 (#017) など「commit するまで model に書かない」 UX が caller boilerplate なしで実現可能。
- **focus 中に外部から `text` 引数が変わっても buffer は反映しない** (= ユーザの typing が消えない)。 controlled 動作 (per-keystroke の model 更新) を望むなら caller が on_change で model 更新する既存パターンで OK (text == buffer なので挙動完全互換)。
- **再表示時の state stale**: 同一 widget id を invisible 期間後に再 show する場合、 `state.last_focused == true` (前 session の終了状態) が残ると gained_focus が発火せず buffer / 全選択 reset が走らない。 `text_input_at_focused` は `was_widget_visible_last_frame == false` 時に `state.last_focused = false` を強制 reset することで対処済 (歌詞編集の「分配済 1 文字 + 全選択」 を保証)。
- **lyric 描画は cached の外**: piano_roll の lyric は `draw_lyrics` 独立 fn で selection overlay の **後** に描画 (cached 内 `draw_notes` で描くと selection の黄色 fill に隠れるため)。 `font_size = (note_h * 0.75).clamp(7.0, lyric_font_px)` で note 高さスケール、 `lyric_font_px` は MAX cap として解釈。

## 参考リソース

- 設計の正本: [docs/plan.html](docs/plan.html)
- フィードバック / 過去知見: daw_01 の `~/.claude/projects/F--dev-daw-01/memory/`
  （旧 gui_01 memory `F--dev-gui-01` からの移行は統合作業の follow-up）
- skill: UI デバッグは daw_01 root の `.claude/skills/debug-ui`。実装 / レビュー / 調査は
  root の `implement` / `review` / `research-similar-impl` を使う
