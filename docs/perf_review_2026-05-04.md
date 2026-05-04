# Performance Code Review — gui_01 全体 (M11 Phase 51 完了時点)

## Context

- **依頼**: 「パフォーマンスの観点で全体をコードレビューしてください」(2026-05-04)
- **対象**: `F:/dev/gui_01` 全 crate (commit f7f288a, M11 Phase 51 完了状態)
- **レビュー範囲**: `renderer` (wgpu 29 pipelines) + `ui` (frame / widget / scenegraph / heavy) + `platform` (winit 0.30) + `benches` 4 本 + 9 example
- **観点**: hot path での allocate / clone、長時間 (8h+) 運転での leak / cache grow、N=1000+ widget での scaling、60fps 維持
- **不変条件 (load-bearing)**: ユーザ Model に Clone/Hash/PartialEq 不要、メッセージ型禁止、derive 禁止、audio/IPC 触らない (`docs/plan.md` 参照)
- **このファイルの位置付け**: 報告書 + 修正提案 一体型。ユーザは下記 P0-P3 のうち修正したい範囲を選択して再依頼可能

## 全体 verdict

設計は **概ね健全**。Phase 45f の primitive 統合 / `heavy() + cached()` 二段キャッシュ / glyphon Buffer cache / rect・line instance pool / popup pipeline 独立化、いずれも良い設計判断。ただし以下 2 点に **真のホットパス問題** がある:

1. `Ui::with_widget_node` の cache hit/miss 経路で `Primitive` を Vec ごと毎フレーム clone (Glyph に String / Line に Vec を内包)
2. `arrangement.rs` の `tracks_for_draw.clone()` が release frame だけでなく **通常 frame でも 2 度** 発火

加えて 8 時間運転で **bounded でない grow 経路が 2 つ** (`TextRenderer` pool / `Scenegraph::nodes` HashMap)。それ以外は微小最適化レベル。

---

## 発見項目

### P0 — Critical (60fps / 8h 運転に直結)

#### P0-1: `Ui::with_widget_node` の cache hit/miss で `Primitive` を Vec 全 clone
- **ファイル**: [crates/ui/src/ui.rs:917-921, 935-940](crates/ui/src/ui.rs:917)
- **症状**: cache hit 時 `cached.primitives.iter().cloned()` で `Vec<Primitive>` 全要素を毎フレーム複製。`Primitive::Glyph(GlyphArea)` は `text: String`、`Primitive::Line(LineBatch)` は `segments: Vec<LineSegment>` を内包 ([crates/renderer/src/scene.rs:113-156](crates/renderer/src/scene.rs:113))。1000+ widget のフレームで毎 hit ごと **String alloc + Vec alloc が widget 数 × primitive 数だけ発火**。cache miss 時の `self.scene.primitives[p0..].to_vec()` も同じコスト。「cache hit が free」という設計の前提が崩れている。
- **修正方針**: `CachedCommands::primitives: Arc<Vec<Primitive>>` 化 ([crates/ui/src/scenegraph.rs:24](crates/ui/src/scenegraph.rs:24))。cache hit は `Arc::clone` (refcount のみ) で済ませ、`Scene::primitives` を `Vec<Primitive>` から `Vec<PrimitiveRef>` (Owned / Cached(Arc)) のような sum 型 に変更し、renderer の `enqueue_runs` で deref する。最低限の段階的改善案として `Arc<[Primitive]>` で要素 clone 自体を撲滅する手もある。
- **難度**: 中 (Scene の primitive 表現を見直す改修、renderer の `enqueue_runs` も追従、scene.rs / scenegraph.rs / ui.rs / pipelines/mod.rs を 1 commit で更新)
- **効果**: piano_roll 100k notes / arrangement 500 widget の cache hit frame 時間が劇的に短縮見込み

#### P0-2: `arrangement.rs` の `tracks_for_draw` が通常 frame でも毎フレーム 2 重 clone
- **ファイル**: [crates/ui/src/widgets/arrangement.rs:1380-1397](crates/ui/src/widgets/arrangement.rs:1380)
- **症状**: 1395 行で `tracks.to_vec()` (`pending_reorder_order = None` の通常 frame でも fresh allocate)、1397 行で `let tracks_owned = tracks_for_draw.clone()` で **2 度目の clone**。`ArrangementTrack` は `name: String` / `volume: f32` 等を持つ struct で、N=12 で 24 String alloc/frame、N=100 で 200 alloc/frame。daw_01 実利用で track 数が増える前提。
- **修正方針**: heavy() closure に渡す引数を `&[ArrangementTrack]` のまま借用で渡せる構造にする。`Cow<'_, [ArrangementTrack]>` で「reorder release frame のみ Owned」とし、それ以外は Borrowed。`tracks_owned` の独立変数を撤廃。
- **難度**: 低 (move semantics の見直しのみ、API 変更なし)
- **効果**: 通常 frame の alloc が track 数だけ消える、reorder 関連 P0 の唯一の clone 経路に限定

---

### P1 — High (頻度大、影響中)

#### P1-1: `pipelines/mod.rs::enqueue_runs` が run ごとに `Vec::collect()`
- **ファイル**: [crates/renderer/src/pipelines/mod.rs:67-87](crates/renderer/src/pipelines/mod.rs:67)
- **症状**: 同 type 連続を 1 run にまとめる際、`group.iter().filter_map(...).collect()` で Vec を毎 run allocate。base + popup の 2 pass × 平均 10-50 run/frame = **20-100 alloc/frame**。`Line` 系は `LineBatch.clone()` (内部 Vec<LineSegment> を伴う) を **filter_map 内で発火**させており alloc が更にネストする。
- **修正方針**: pipeline 側 (`rect/line/glyph.enqueue_run`) を **`&[Primitive]` のスライス + iterator** で受け取れるよう shape を変える。Vec collect を撲滅し、enum match を pipeline 内で直接行う。`LineBatch` は `Arc<[LineSegment]>` 化を検討 (P0-1 とセット)。
- **難度**: 中

#### P1-2: `frame_to_edits` 末尾の `focus_order_snapshot.clone()` が毎フレーム発火
- **ファイル**: [crates/ui/src/ui.rs:534](crates/ui/src/ui.rs:534)
- **症状**: `let focus_order_snapshot: Vec<(WidgetId, Rect)> = ui.focus_order.clone();` が毎フレーム実行。focusable widget が 50+ あると毎フレーム 50 element の Vec clone。`tab_navigate` / `arrow_navigate` は `&[(WidgetId, Rect)]` で受け取っている (line 545-553) ので、本来 clone 不要。
- **修正方針**: `let focus_order_snapshot = std::mem::take(&mut ui.focus_order);` または `&ui.focus_order[..]` で借用ベースに変更。`focus_order` の所有権が `ui.set_focus` で再 borrow に干渉する場合のみ take 化。
- **難度**: 低

#### P1-3: `pipelines/glyph.rs::prepare_renderer` で `keys: Vec<u64>` を毎 run alloc
- **ファイル**: [crates/renderer/src/pipelines/glyph.rs:168, 192-225](crates/renderer/src/pipelines/glyph.rs:168)
- **症状**: 168 行で `keys: Vec<u64> = glyph_areas.iter().map(buffer_key).collect()`、192-225 行で `text_areas: Vec<TextArea>` を再度 collect。run 数 × 2 alloc/frame。さらに `area.text.hash()` を **buffer_key 計算と cache lookup の 2 回** 実施。
- **修正方針**: `GlyphPipeline` に `key_scratch: Vec<u64>` / `text_areas_scratch: Vec<...>` を field として持たせ、`begin_frame` で `clear()` のみ実施し alloc を除去。`buffer_key` 結果を `GlyphArea` に lazy cache (Cell<Option<u64>>) する案もあるが、`GlyphArea: Clone` の挙動が変わるので段階的改修が無難。
- **難度**: 低

#### P1-4: `pipelines/rect.rs::enqueue_run` で `clip_rect` 切替時に span 単数化
- **ファイル**: [crates/renderer/src/pipelines/rect.rs:184-218, 248-260](crates/renderer/src/pipelines/rect.rs:184); 同じ構造が [line.rs](crates/renderer/src/pipelines/line.rs) にも
- **症状**: scroll_area / popup / split_view が広く使われると、各 widget 毎に異なる `clip_rect` が立つ。span 数 = clip 切替回数 = `set_scissor_rect` 呼び出し数。N=1000 widget で N 回の scissor 切替が発生する可能性。
- **修正方針**: instance buffer に `clip_rect` を pack して shader 内で discard する hardware clip 方式に切替。または **clip_rect でグループ sort** したうえで run boundary を再計算 (z-order を保てる範囲で)。前者は WGSL 修正が必要だが span 単数化を根絶できる。
- **難度**: 中-高 (hardware clip 方式は WGSL + RectInstance struct 変更を伴う)

---

### P2 — Medium (8h 運転リスク / 中規模 alloc)

#### P2-1: `pipelines/glyph.rs::renderers` pool が unbounded grow + shrink しない
- **ファイル**: [crates/renderer/src/pipelines/glyph.rs:128-135, 71-72 のコメント](crates/renderer/src/pipelines/glyph.rs:71)
- **症状**: コメントで「shrink しない (allocate コストは grow 1 度だけ)」と意図的設計だが、popup spike (cascade menu / tooltip 多発) で run 数が一時的に膨らむと `TextRenderer` pool が膨張したまま戻らない。8h 運転で常用 N=10、ピーク N=100 のケースで N=100 を maintain。`TextRenderer` 1 つあたりの内部 buffer は小さいが、`TextAtlas` を共有しているとはいえ累積する。
- **修正方針**: `end_frame` で `next_renderer_idx < renderers.len() / 4` のとき `renderers.truncate(used * 2)` のような high-water-mark + decay 方式。または上限 cap (例: 32) を設けて超過 run は warning + batching。
- **難度**: 低

#### P2-2: `winit_backend.rs::MouseInput` で毎クリック OS query
- **ファイル**: [crates/platform/src/winit_backend.rs:170-185](crates/platform/src/winit_backend.rs:170)
- **症状**: Alt-Tab 復帰直後の `cur_pos = None` 状態を救うため、**毎 click** で `query_cursor_pos_in_window()` (Win32 GetCursorPos + ScreenToClient) を発行。通常クリックでは無駄な OS syscall。1 click = 2 syscall (press / release)。連打される click では累積コスト。
- **修正方針**: `cur_pos.is_none()` のときだけ query する条件を入れる。Alt-Tab 復帰時の synthetic move は `WindowEvent::Focused(true)` で 1 回だけ発火させる方式の方が筋が良い。
- **難度**: 低
- **注意**: 修正には Alt-Tab 復帰検証 (CLAUDE.md "既知の罠 winit 0.30") の手動テストが必須

#### P2-3: `frame_to_edits` 入口で複数 Vec / HashMap を `::new()`
- **ファイル**: [crates/ui/src/ui.rs:408, 417, 480, 500, 516](crates/ui/src/ui.rs:408)
- **症状**: `edits` / `pending_shortcuts` / `focus_order` / `popup_primitives` / `pending_clipboard_paste_bytes` を frame 入口で fresh `Vec::new()` / `HashMap::new()`。allocator は lazy なので空 Vec の時は alloc=0 だが、内容を push し始めた瞬間 fresh allocation。
- **修正方針**: `UiHost` に scratch buffer として持たせ、`frame_to_edits` 冒頭で `.clear()` のみ。capacity が再利用される。
- **難度**: 低
- **効果**: 8h 運転で grow せず、capacity が安定する利点も

#### P2-4: `Scenegraph::nodes` HashMap が `shrink_to_fit` しない
- **ファイル**: [crates/ui/src/scenegraph.rs:39, 67-68](crates/ui/src/scenegraph.rs:39)
- **症状**: `retain(&seen)` で削除はされるが、HashMap の内部 capacity は縮小しない。一時的に大量 widget (例: piano_roll で 100k notes 表示後に他タブへ切替) で capacity が膨張したまま固定。
- **修正方針**: `retain` 直後に `if nodes.len() < nodes.capacity() / 4 { nodes.shrink_to_fit() }` 条件付き呼び出し。N frame 間隔で実行 (毎 frame は overkill)。
- **難度**: 低

---

### P3 — Low / 観察事項

#### P3-1: `InputAccumulator::take_keyboard_events` で capacity が失われる
- **ファイル**: [crates/ui/src/input.rs:200-201, 205-206](crates/ui/src/input.rs:200)
- **症状**: `mem::take(&mut self.pending_keys)` で fresh `Vec::default()` に置き換え、戻り値 `Vec` は frame 内で消費後 drop。次フレームで `Vec::with_capacity(0)` から再起動。1 frame 数イベントなので影響は小だが、毎フレーム 1-2 alloc。
- **修正方針**: 受け取り側を `&mut Vec<KeyEvent>` に変えて drain / mem::swap で内部 capacity を還流させる。または `SmallVec<[KeyEvent; 8]>` で stack 化。
- **難度**: 低

#### P3-2: `pipelines/mod.rs::enqueue_runs` 戻り値の `Vec<RunHandle>` も毎フレーム alloc
- **ファイル**: [crates/renderer/src/pipelines/mod.rs:56](crates/renderer/src/pipelines/mod.rs:56)
- **症状**: P1-1 と同所、戻り値 Vec も毎 pass alloc (base + popup で 2 alloc/frame)。
- **修正方針**: `&mut Vec<RunHandle>` を渡して append 形式に。または `Renderer` 側 scratch buffer。
- **難度**: 低

#### P3-3: `daw_prototype` の `sim_phase` 永続回転で常時 redraw
- **ファイル**: [crates/examples/daw_prototype/src/main.rs](crates/examples/daw_prototype/src/main.rs) (ファイル全体、level meter 駆動部)
- **症状**: visual prototype のため意図的に animation 駆動だが、user が放置している idle 時にも CPU を使い続ける。実 DAW では「peak 値変化時のみ redraw」が望ましい。
- **修正方針**: example 限定の話。daw_prototype は demo 用なので現状維持で OK。daw_01 実装側で対応する方針が筋。
- **難度**: N/A (library 修正不要)

---

## 確認した OK 領域

- **glyphon `Buffer` cache** ([crates/renderer/src/pipelines/glyph.rs:30-38, 173-188](crates/renderer/src/pipelines/glyph.rs:30)): `(text, font_size, line_height)` で hash key、`EVICT_AFTER_FRAMES = 300` で約 5 秒未使用 evict。設計健全
- **rect / line instance pool** ([crates/renderer/src/pipelines/rect.rs:160-175, 220-233](crates/renderer/src/pipelines/rect.rs:160)): `MAX_INSTANCES` 固定 pre-allocate、`begin_frame::clear()` で reuse、`upload` 1 回/frame
- **`Scene::primitives` 統合 (Phase 45f)**: z-order 正常、run grouping 単純で blockerなし
- **`heavy() + cached()`** ([crates/ui/src/widgets/heavy.rs](crates/ui/src/widgets/heavy.rs)): `with_widget_node` 薄ラッパで複雑性低、eviction 効く (`heavy_evicts_when_not_called_for_a_frame` test 済)
- **`history.rs` ring buffer**: capacity 100 で bounded、leak なし
- **`InputAccumulator` drain pattern** ([crates/ui/src/input.rs:200-217](crates/ui/src/input.rs:200)): 全体構造は良好、capacity は P3-1 の通り微小最適化余地のみ
- **popup pipeline 独立化 (Phase 44a)** ([crates/renderer/src/device.rs:202-297](crates/renderer/src/device.rs:202)): popup_rect/line/glyph で base buffer 干渉を回避、設計健全
- **`Cargo.toml` release profile** ([Cargo.toml:61-63](Cargo.toml:61)): `lto = "thin"` / `codegen-units = 1` — DAW 用途で適切
- **bench カバレッジ**: `waveform.rs` は **既に N=1, 8, 64, 128 まで multi-widget 測定済** ([crates/ui/benches/waveform.rs:117](crates/ui/benches/waveform.rs:117))
- **`text_input.rs::TextInputState::preedit: String`** ([crates/ui/src/widgets/text_input.rs:31](crates/ui/src/widgets/text_input.rs:31)): widget state field で再利用、preedit 中のみ更新 (毎フレーム alloc ではない)
- **`piano_roll/main.rs` の sort**: `make_demo_notes` 関数内 (setup-time のみ)、main loop で実行されない

---

## 不採用と判断した指摘

レビュー過程で agent から提案されたが、コード verify の結果 **影響なし** または **既に解消済**:

- ❌ "waveform bench multi-widget 不足" — 既に N=128 まで対応済 (memory `project_multi_waveform.md` の指摘は解消済み)
- ❌ "piano_roll 毎フレーム sort" — `make_demo_notes` 内の setup time、main loop 不在
- ❌ "text_input.rs:31 preedit alloc per-frame" — state field で再利用される (`TextInputState`)
- ❌ "OffscreenRenderer の poll が main thread block" — DAW UI は frame-sync で意図通り、snapshot 用途は async 化不要

---

## 実装スコープ (user 確認後、2026-05-04)

このターンで実装するのは **#1 + #7 の 2 件** (user 確認: #6 は除外):
- **#1 = P0-2** `arrangement.rs` の `tracks_for_draw.clone()` 撤廃 (通常 frame の冗長 clone を消す)
- **#7 = P0-1** `Primitive` 内の重コンテナを `Arc` 化 (Glyph の String / Line の Vec を refcount に置き換える)

それ以外の P1 / P2 / P3 項目は **将来 phase で別途検討** (このターンでは触らない)。

### 実装順序と方針

P0-1 が影響範囲広いので **最初** にやり、その上で P0-2 (各 commit 独立)。

#### Step 1: P0-1 — `Primitive` 内コンテナを Arc 化 (シンプル路線)

**方針**: `Primitive` enum 自体や `Scene::primitives: Vec<Primitive>` の構造は維持。重コンテナだけ Arc 化することで `Primitive::clone()` を refcount のみに圧縮する。

- [crates/renderer/src/scene.rs:113-127](crates/renderer/src/scene.rs:113): `GlyphArea::text: String` → `Arc<str>` に変更
- [crates/renderer/src/scene.rs:139-148](crates/renderer/src/scene.rs:139): `LineBatch::segments: Vec<LineSegment>` → `Arc<[LineSegment]>` に変更 (ただし `LineBatch` 構築側で `segments.push()` していると変更が大きいので、必要なら一旦 `Vec` で組み立てて最後に `Arc::from` する build helper を提供)
- [crates/renderer/src/pipelines/glyph.rs:182](crates/renderer/src/pipelines/glyph.rs:182): `Buffer::set_text(&mut font_system, &area.text, ...)` は `&str` deref で動く (`Arc<str>` の deref で OK)、ただし `area.text.hash(&mut h)` ([buffer_key:33](crates/renderer/src/pipelines/glyph.rs:33)) も `Arc<str>::hash` が `str::hash` と同じく content-based でなければならない (これは Rust 標準で OK)
- [crates/renderer/src/pipelines/line.rs](crates/renderer/src/pipelines/line.rs): `batch.segments.iter()` 利用箇所、`Arc<[T]>::iter` で動く
- [crates/renderer/src/pipelines/mod.rs:75-78](crates/renderer/src/pipelines/mod.rs:75): `LineBatch::clone()` は Arc clone のみで refcount only に
- [crates/ui/src/ui.rs](crates/ui/src/ui.rs): `push_text` 等で `GlyphArea { text: format!(...), ... }` を書いている箇所を `text: format!(...).into()` (= `Arc::<str>::from(String)`) に置換
- [crates/ui/src/widgets/*.rs](crates/ui/src/widgets/): 全 widget で `GlyphArea` / `LineBatch` を構築する箇所を grep して 1 commit で更新
- 既存 `Primitive::clone()` 呼び出し箇所 ([ui.rs:917, 921, 936, 938](crates/ui/src/ui.rs:917)) はそのまま、コストだけが下がる

**daw_01 への影響**:
- `GlyphArea::text` / `LineBatch::segments` の型変更は **API breaking** だが、daw_01 が直接 `GlyphArea` / `LineBatch` を構築している箇所が少なければ影響は軽微。grep で確認後、必要なら conversation file ([F:/dev/daw_01/docs/gui_01_conversation.md](F:/dev/daw_01/docs/gui_01_conversation.md)) 経由で「P0-1 で `GlyphArea::text: String → Arc<str>` / `LineBatch::segments: Vec → Arc<[LineSegment]>` を breaking 変更、push_text caller は `text.into()` で済む」と通知 (memory `feedback_no_daw_01_edit.md` / `feedback_no_daw_01_commit.md` 遵守、daw_01 ディレクトリは編集しない)

**検証**: `cargo bench -p daw-ui-core --bench scenegraph_cache` の cached_call 時間が短縮されること、`cargo bench --bench heavy_arrangement` / `--bench heavy_piano_roll` で回帰しないこと、`no_clone_required` trybuild が pass。

#### Step 2: P0-2 — `arrangement.rs` の冗長 track clone 撤廃

- [crates/ui/src/widgets/arrangement.rs:1380-1397](crates/ui/src/widgets/arrangement.rs:1380): `tracks_for_draw` 構築後の `let tracks_owned = tracks_for_draw.clone();` を削除し、heavy() closure には `tracks_for_draw` を直接 move
- 通常 frame は `pending_reorder_order = None` なので 1395 行の `tracks.to_vec()` も避けたいが、heavy() closure は `'static` 要求 (内部 closure を move する) なので **1 度の `to_vec()` は不可避**。それでも `tracks_owned` の重複 clone は撲滅できる
- `draw_lanes_bg` / `draw_clips` / `draw_drag_preview` / `draw_selection_overlay` は `&[ArrangementTrack]` を受け取れるよう shape を変える (現状もそう、確認のみ)

**検証**: `cargo test --workspace` の arrangement 関連 unit test (15 件) が pass、`daw_prototype` で reorder 動作の regression なし。

### 各 step は別 commit

CLAUDE.md「docs と code を同じ commit で更新」原則のため、各 step で `docs/plan.md` の M11 (or 新 milestone) のテーブルに進捗行を 1 行追加する。コミットメッセージは日本語、フォーマットは既存に倣う (例: `feat(M12 Phase 53): Primitive 内コンテナを Arc 化 (Glyph::text / Line::segments)`)。


## 推奨修正順 (期待効果 / 難易度バランス)

| 順 | 項目 | 効果 | 難度 | コメント |
|---|---|---|---|---|
| 1 | **P0-2** arrangement track clone 撤廃 | 大 | 低 | 1 widget 限定だが daw_01 実利用に直結、即効性あり |
| 2 | **P1-2** focus_order_snapshot.clone 撤廃 | 中 | 低 | 1 行修正、focusable widget 数に比例した alloc が消える |
| 3 | **P1-3** glyph keys/text_areas を scratch field 化 | 中 | 低 | run 数 × 2 alloc 削減 |
| 4 | **P2-3** frame_to_edits の Vec/HashMap を UiHost scratch 化 | 中 | 低 | 8h 運転 stability 向上 |
| 5 | **P2-1** TextRenderer pool に decay or cap | 中 | 低 | popup spike 後の grow を抑制 |
| 6 | **P2-4** Scenegraph HashMap 条件付き shrink_to_fit | 小-中 | 低 | 大量 widget 出現後の capacity 還流 |
| 7 | **P0-1** Primitive を Arc 化 + Scene 表現変更 | **特大** | 中 | 最大効果だが影響範囲広い、設計変更を伴う |
| 8 | **P1-1** enqueue_runs の collect 撲滅 | 中 | 中 | P0-1 とセットで設計するのが筋 |
| 9 | **P1-4** rect/line の clip_rect を hardware clip 化 | 大 | 高 | WGSL 改修、最後にやる候補 |
| 10 | **P2-2** winit Alt-Tab query の条件化 | 小 | 低 | Alt-Tab 動作の手動 regression テスト必須 |
| 11 | P3 系 | 微 | 低 | 余裕があれば |

短期は **1-6 (低難度の 6 件)** をまとめて 1 commit、中期は **7-8 (P0-1 + P1-1)** を 1 commit、長期は **9 (P1-4 hardware clip)** を別 milestone で計画 — の 3 段構えが現実的。

---

## 検証方法

### bench で測れる範囲
- `cargo bench -p daw-ui-core --bench scenegraph_cache` — cache hit/miss 時の per-frame µs 比較 (P0-1 効果を直接測定)
- `cargo bench -p daw-ui-core --bench heavy_arrangement` — N=500 widget の primitive clone 削減効果 (P0-1, P0-2)
- `cargo bench -p daw-ui-core --bench heavy_piano_roll` — 100k notes cache hit 時の改善 (P0-1)
- `cargo bench -p daw-ui-core --bench waveform` — 既存 N=128 multi-widget bench で renderer hot path 全体への影響を観察

### 手動検証 (8h 運転)
- `cargo run --release --bin daw_prototype` を起動して 1 時間放置、Win10 タスクマネージャでメモリ trend を観察。修正前と修正後で trend 比較 (P2-1, P2-3, P2-4)
- `cargo run --release --bin piano_roll` で 100k notes 表示 → 別タブ → 戻る、を繰り返して `Scenegraph::nodes` capacity が膨張せず縮小するか
- Alt-Tab 復帰後の最初のクリックが正しく hit-test される (P2-2 修正後の regression、CLAUDE.md "既知の罠 winit 0.30")

### CI ガード
- `cargo test --workspace` (現 216 unit test + trybuild) が pass すること
- `cargo clippy --workspace --tests -- -D warnings` で warning 0
- `cargo test -p daw-ui-core --test no_clone_required` (no-Clone 制約の trybuild) が pass、特に `Primitive` を Arc 化したときに **ユーザ Model に Clone 要求が伝染しない** ことを確認
