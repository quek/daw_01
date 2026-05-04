理想とベストプラクティスを追求する。
そのためには大胆に破壊して作り直す。

# Rust 製・モデルを Clone しない DAW 向け GUI ライブラリ — 設計計画 (M9 = Real DAW Validation / M1-M8 履歴は history.md)

## Context

- **目的**: Rust で DAW (Digital Audio Workstation) のシェル UI を書くための **GUI ライブラリ** をゼロから設計・実装する。GUI のみが本ライブラリの責務、audio / IPC は別プロセス。
- **本ファイルの位置付け**: M9 (Real DAW Validation) の計画。M1-M8 + M5.5 の完了履歴・詳細設計・検証手順は [history.md](history.md) に分離。**正本 (canonical)** は git 管理下の `F:\dev\gui_01\docs\plan.md` (リネームで紐付けが切れる `~/.claude/plans/` のハッシュ命名は使わない)。
- **核となる制約 = 「モデルを Clone しない」** (load-bearing):
  1. MIDI クリップ・サンプル・オートメーションなど大きなデータを毎フレーム複製しない
  2. アプリ側がドメイン型を IPC で audio exe に伝送する際の都合 (zero-copy / 部分 snapshot) を library が阻害しない
  3. iced の `Application::Message: Clone` のような型境界をユーザのドメイン型に伝染させない
- **ライブラリの責務**: `&GuiModel` を借りて UI を描画 / 入力からエディット (`Edit<M>`) を収集し、`UiHost::frame(&mut model, ...)` で内部 apply + 自動 `request_redraw`
- **ライブラリの非責務**: GUI Model の所有・undo/redo の policy / audio exe との IPC・SHM / audio Model の設計

---

## アーキテクチャ要旨 (Hybrid: 即時モード API + 内部 scenegraph + heavy() 脱出口)

DAW UI の二極性 (静的密集型 vs 巨大可変型) をどちらも捌くため:

- 公開 API は **即時モード** に統一 (`derive` マクロ・Lens 不要、`Application::Message: Clone` 伝染なし)
- 内部実装で **scenegraph + input hash** により静的 UI の再描画コストを削減 (M4 で実装済み)
- `ui.heavy(id, |hctx| ...)` 脱出口でピアノロール・タイムライン・大量波形群などは retained-mode 風に最適化 (M5 で実装済み)

```rust
// 起動時: WindowBackend を渡しておけば、edits 検出時に自動で request_redraw が呼ばれる
let mut ui_host: UiHost<GuiModel> = UiHost::with_window(window.clone());

// AppHost::on_render() で 1 フレーム毎に呼ぶ
// (frame() が `&mut model` を取り、edits を内部 apply、必要に応じて redraw 自動要求)
ui_host.frame(&mut gui_model, &mut scene, screen, input, |m, ui| {
    ui.fader("master", m.master_volume, 0.7, "master fader", |v| {
        Edit::mutate(move |m: &mut GuiModel| m.master_volume = v)
    });
    ui.heavy("piano_roll", |hctx| { /* 巨大ビュー */ });
});
renderer.render(&scene)?;
app.notify_audio_process(&gui_model);

// audio thread に Edit を送る / batch apply / undo stack 等の advanced 用途では
// `frame_to_edits(&model, ...) -> Vec<Edit<M>>` を使う (利用者が apply タイミングと
// request_redraw を制御)。
```

**ライフタイム不変条件**: `Ui<'a>` の `'a` は `&'a M` 借用と一致 (`UiHost::frame` は `&mut model` を内部で `&*model` reborrow して `Ui<'a>` に渡す)。`Edit<M>` は `'static` Box 化されてフレーム末尾に内部 apply 消費。GAT 不要。

**「Clone しない」をどう守るか**:
- ユーザ Model 型に `Clone` / `PartialEq` / `Hash` / `Default` を要求しない
- メッセージ型を導入しない (Edit は `Mutate(Box<dyn FnOnce>)` か `Undoable { Arc<dyn Fn>, Arc<dyn Fn>, label }`)
- 内部 scenegraph の差分検出は **widget ID + プリミティブ末端値の hash** だけで行う
- derive マクロは禁止 (Lens 等)

---

## 基盤クレート選定 (M8 完了時点)

| レイヤ | 採用 | 採用バージョン / 状態 |
|---|---|---|
| Window/Event | winit | 0.30.13 |
| Rendering | 自前 wgpu パイプライン | wgpu 29.0.1 |
| Text | glyphon (cosmic-text + swash) | glyphon 0.11.0 |
| Layout | taffy | 0.10.1 |
| Platform handle | raw-window-handle | 0.6.2 |
| Math/binding | bytemuck | 1.25.0 |
| Async runtime (内部) | pollster | 0.4.0 |
| PNG 出力 (snapshot 用) | png | 0.17 |
| Native file dialog | rfd | 0.15 (M8、feature `dialog`) |
| OS clipboard | arboard | 3 (M8、feature `clipboard`) |
| 開発用 (bench / trybuild) | criterion / trybuild | M2 で導入 |
| 第 2 windowing (プラグイン) | baseview | **凍結** (Phase 44 完了後に再評価、rwh 0.5/0.6 互換待ち) |
| SVG / 任意 path 描画 | vello | **凍結** (Phase 44 完了後に再評価、現状 rect/glyph/line で全 example 成立) |

A11y (AccessKit) は **本ライブラリでは採用しない方針** (2026-05-02 ユーザ判断、個人 DAW プロジェクトのため再評価対象外)。

シェーダは現状 3 本構成: instanced rect / line strip (波形・メータ・オートメーション) / SDF glyph (glyphon 統合)。textured quad は M2-M8 で必要にならず未実装。

---

## 現状ワークスペース構成 (F:\dev\gui_01、M8 完了時点)

```
F:\dev\gui_01\
├── Cargo.toml                       # workspace, edition=2024, rust-version=1.95
├── rust-toolchain.toml
├── docs\
│   ├── plan.md                      # ★ 本ファイル (M9 計画)
│   └── history.md                   # M1-M8 + M5.5 履歴 + 詳細設計
├── crates\
│   ├── platform\                    # daw-ui-platform (winit抽象、raw-window-handle 経由 trait bound)
│   │   └── src\{lib,event,window,winit_backend}.rs
│   ├── renderer\                    # daw-ui-renderer (wgpu 29、自前パイプライン rect/line/glyph + OffscreenRenderer)
│   │   └── src\{lib,device,offscreen,scene}.rs + pipelines\{mod,rect,line,glyph}.rs + *.wgsl
│   ├── ui\                          # daw-ui-core (Ui/UiHost/Edit/history/shortcut/clipboard/dialog/popup/time/viewport/widgets/scenegraph/heavy)
│   │   └── src\{lib,edit,history,shortcut,clipboard,dialog,id,input,layout,popup,scenegraph,time,ui,viewport}.rs
│   │       └── widgets\{mod,button,checkbox,drag_rect,dropdown,fader,heavy,knob,label,level_meter,
│   │                    menu,scroll_area,split_view,tab_view,text_input,time_grid,
│   │                    waveform,automation}.rs
│   └── examples\                    # 9 example (M7 で daw_prototype 追加、M8 で全 example に shortcut/file drop 等を配備)
│       ├── mixer\                   # 8ch fader / button / IME
│       ├── waveform_validation\     # 128 widget LOD ストレステスト + ViewportState1D
│       ├── sample_editor\           # 選択範囲 + カーソル + RmsBars + ViewportState1D
│       ├── piano_roll\              # 100k notes + heavy() cached (5.77x) — M9 Phase 41 で note edit 統合予定
│       ├── arrangement\             # 500 widgets + heavy() cached (9367x)
│       ├── automation\              # cubic Bezier flatten + Catmull-Rom 点ドラッグ
│       ├── embedded_host\           # OffscreenRenderer で PNG snapshot (プラグイン UI 埋め込み実証)
│       ├── sample_edit_ops\         # 波形 trim / linear fade in/out + ViewportState1D — M9 Phase 42 で Undoable 化予定
│       └── daw_prototype\           # M7 visual prototype + M8 shortcut/file drop/dialog 統合
```

外部 project: `F:\dev\daw_01` (実 DAW プロトタイプ) が gui_01 を path 依存で利用中。Phase 44 で実利用フィードバックを集約。

---

## マイルストーン (M9 以降)

M1-M8 + M5.5 は完了済み (詳細: [history.md](history.md))。M8 完了時点 (commit 2ab8ab3) の主要状態:

- M7+M8 の widget / 入力基盤 一式完備: scroll_area / menu_bar / context_menu / dropdown / tab_view / split_view / time_ruler / bar_beat_grid / level_meter / history stack (Undoable) / shortcut / clipboard / drag&drop / multi-select (rect drag) / file dialog
- 9 example 全部動作確認済み (mixer / waveform_validation / sample_editor / piano_roll / arrangement / automation / embedded_host / sample_edit_ops / daw_prototype)
- daw_01 (実 DAW プロトタイプ) が gui_01 を path 依存で利用開始
- 設計の不変条件 (no-Clone / メッセージ型禁止 / derive 禁止 / audio・IPC 不混入) 維持

### M9 (Real DAW Validation — `Edit::Undoable` ergonomic 実証 + daw_01 conversation 統合) — ✅ 完了 (2026-05-04)

**目的**: M8 で導入した `Edit::Undoable` の ergonomic を、note 編集 / audio buffer 編集の 2 ケースで実証する。boilerplate が出れば library helper で吸収。daw_01 で並行検証して library API の fitness function を回す。

**動機**: `Arc<dyn Fn(&mut M) + Send + Sync>` ベースの Undoable は理論上は no-Clone を守るが、`Vec<MidiNote>` / `Vec<f32>` 級の **重い inverse** を要するケース (note multi-select delete、audio trim/fade) でユーザに boilerplate を強要しないかは未検証。「新しく入れた抽象は次の機会に使う」(`feedback_use_new_abstractions.md`) を Undoable に適用するタイミング。M9 を見送って theming / animation / 信号処理 widget に直行すると、Undoable の前提が崩れたまま機能拡張が積み重なるリスク。

**並行**: daw_01 から M7/M8 実利用フィードバックが 11 項目 (P0-P3) 届いている。詳細プランは [plan_daw01_feedback.md](plan_daw01_feedback.md)。Phase 41-44 の前提となる API 整備として P0-P1 を先行実施することが多い (例: P0-1 Shortcut::parse 記号受理は piano_roll の `Shift+/` shortcut で必要、P1-3 HeavyCtx delegate は piano_roll の rect-select で必要、P1-4 double-click は clip → Piano Roll タブ UX で必要)。

| Phase | テーマ                                                                 | 主な成果物                                                                                                                                                                                                                                                        | 状態                        |
|-------|------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------|
| 41    | piano_roll の note edit + multi-select 統合 + library widget 化 (主軸) | `Edit::with_inverse` / `Edit::snapshot_inverse` で note add/delete/move/resize/select、Ui::take_drag_rect_in_rect で rect multi-select、Ui::set_cursor 公開、`Ui::piano_roll` library widget + `NotesEditRequest` enum で API 完結                                | ✅ 完了 (41pre+a+b+c+d+e+f) |
| 42    | sample_edit_ops の trim/fade を Undoable 化                            | trim/fade in/out の 3 ボタンを `Edit::snapshot_inverse` 化。audio buffer の inverse 戦略は **混在採用**: trim = full snapshot (`Vec<Vec<f32>>` + viewport/selection/cursor)、fade in/out = 範囲 snapshot (`Vec<f32>` の e-s 個 + range + direction enum)          | ✅ 完了                     |
| 43    | debug overlay (validation 用)                                          | `Ui::debug_overlay(rect, frame_ms)` + `UiHost::last_frame_stats() -> FrameStats` で cache_hits/misses/widget_count/scenegraph_size/history_depth を画面右上に popup z-order の半透明 overlay。Ctrl+F1 を default shortcut binding (`debug_overlay_toggle`) に追加 | ✅ 完了                     |
| 44a   | popup pass の renderer pipeline 独立化                                 | Phase 43 で発見した「popup pass の prepare で base pass の rect/line/glyph buffer が上書きされる」問題を、renderer に `popup_rect / popup_line / popup_glyph` 3 つの独立 pipeline インスタンスを追加して根本解決                                                  | ✅ 完了                     |
| 44b   | Undoable ergonomic 評価 + 必要なら API 改善                            | call site 分類: `Edit::snapshot_inverse` 7 件 (Phase 41d で library 化済、ergonomic 良好) + fader/knob の drag release pattern 2 件 (3 件未満で premature abstraction 回避)。**追加 helper 不要、現 API で十分** を確定                                          | ✅ 完了                     |
| 44c   | daw_01 Note schema 統合 (f64 + lyric)                                  | **案 A 採用** (CLAUDE.md「大胆に破壊して作り直す」原則): gui_01 Note を `start_beat: f64` / `len_beats: f64` 化し、`lyric: Option<Arc<str>>` を追加。Phase 44b の API stability 確約は撤回。daw_01 が adapter なしで gui_01 widget を直接使える状態に     | ✅ 完了                     |
| 44d   | daw_01 conversation #002-#004 対応                                     | piano_roll の rect select を **Alt+drag → Shift+drag (加算)** に breaking 置換 (daw_01 旧自前実装慣習 + DAW 業界標準に合致)。`next = prev ∪ rect_inside` で「加算」を実現、排他は空白 click + Shift+drag の 2 ステップ。daw_prototype に **clip dbl-click → Piano Roll タブ遷移** デモを追加 (M9 P1-4 `take_double_click_in_rect` の活用例)。daw_01 conversation に 3 件返信 | ✅ 完了                     |
| 45a   | daw_01 conversation #008 — `Ui::panel` helper                          | `Ui::panel(id, rect, fill, radius)` + `Ui::panel_with_border(...)` を新設 (`crates/ui/src/widgets/panel.rs`)。`heavy + cached + push_rect` の boilerplate を 1 行に圧縮、cache key は rect/fill/border/radius の `[u32; 14]` で size/color 変化で自動 invalidate。daw_prototype の footer 背景を panel に置換 (利用例)。6 unit test pass | ✅ 完了                     |
| 45b   | daw_01 conversation #009 — `toggle_button_at`                          | `Ui::toggle_button_at(id, text, rect, value, &style, on_toggle)` + `ToggleButtonStyle { off_color, on_color, hint_band: Option<Color>, hint_band_h, border, border_width, radius, font_size, text_color }` を新設 (`crates/ui/src/widgets/toggle_button.rs`)。`button` と同じ armed-state click 判定、`value=true && Some(hint_band)` で rect 下端 hint_band_h px に色帯 (DAW M=赤 / S=黄 慣習)。`mixer` example の Mute checkbox を toggle_button + ON 時赤帯 hint band に置換。5 unit test pass (click → on_toggle(!value) / hint band 表示条件 / 色 swap) | ✅ 完了                     |
| 45c   | daw_01 conversation #006 — piano_roll velocity lane + playhead 内蔵    | `PianoRollView` に `velocity_lane_h: f32` (`0.0`=disabled) と `playhead_beat: Option<f64>` (`None`=disabled) を追加 (**breaking**、Default impl が無いため 3 caller を 1 commit で追従更新: `examples/piano_roll`, `tests/ui/pass/basic.rs`, 内部 `test_view`)。`PianoRollStyle` に `playhead_color` / `velocity_lane_bg` / `velocity_bar_color` / `velocity_bar_width_px` 追加 (Default 経由なので非 breaking)。velocity lane は `cached` 内、playhead は `cached` 外で毎フレーム描画。private helper `draw_velocity_lane` / `draw_playhead_line` を切り出し (Phase 45e arrangement で再利用予定)。piano_roll example で `playhead_beat: Some(2.0)` を有効化 (visible デモ)。5 unit test pass (vel_h on/off rect 差分 / velocity 0 skip / playhead in/out of range / playhead+vel 同時) | ✅ 完了                     |
| 45d   | daw_01 conversation #007 — `Ui::modal` + `Ui::list_view`               | `Ui::open_modal/close_modal/is_modal_open/modal()` を `popup_layer` 上の薄いラッパとして実装 (画面中央 panel 計算 + `update_popup_anchor` で毎フレーム更新 + ESC `take_shortcut("escape")` + outside click は popup_layer の標準挙動 + `on_close: Option<Box<dyn FnOnce() -> Edit<M>>>`)。`Ui::list_view` は `scroll_area` 上の薄いラッパで、表示 row 範囲のみ row callback (range skip 軽量 virtualization)、`row` callback は `&mut Ui<'_, M>` (P1-5 一貫)。`update_popup_anchor` ヘルパも追加 (anchor だけ更新、`prev_focus` 維持)。daw_prototype に footer "Demo Dialog" ボタン → modal + list_view デモを追加 (新抽象を次の機会に使う原則)。10 unit test pass (modal: open/close 切替 / ESC close + on_close / outside click close + on_close / overlay+panel 描画 / body 内 close、list_view: 全 row 呼び出し / 画面外 skip / hover+click 検出 / selected 描画 / empty list) | ✅ 完了                     |
| 45e   | daw_01 conversation #005 — `Ui::arrangement` widget (大物)             | 確定 API (`#005 [Replied]`) 通り `crates/ui/src/widgets/arrangement.rs` を新設 (~960 LOC、`Default` 付き `ArrangementView` / `ArrangementStyle` / `ArrangementResponse` で部分上書き ergonomic)。4 layer 統合実装: 描画基盤 (cached: ruler / lanes / clips / track selected bg / mute=赤・solo=黄 hint band。cached 外: selection overlay / drag preview / playhead / loop band) + drag (Move / ResizeLeft / ResizeRight、track 跨ぎ `MoveClipDelta`、Shift+drag rect select、commit-by-release、drag<16px は click 格下げ、double-click `DoubleClickClip` / `DoubleClickEmpty`、release frame の `pointer.pos` 不安定対策で `ClipDragSession.last_mouse` / `LoopDragSession.last_mouse_x` を drag 中各 frame で更新) + ruler (loop band Start / End / Middle / NewRange の 4 mode drag → `SetLoopRange` 発行) + header (`button_at` × 4 + `toggle_button_at` × 2 + Name button click → `SelectTrack` / Name dbl-click → `BeginRenameTrack` (UX 改善、background click も SelectTrack)、`Response.track_header_rects` を載せる)。lanes/ruler/header 内 hover 無で `set_cursor(Default)` (cursor reset)。`draw_playhead_line` を `crates/ui/src/widgets/playhead.rs` に `pub(crate) fn` 切り出し、piano_roll / arrangement で共有。`shortcut.rs` の default binding から無修飾 Arrow (`focus_up/down/left/right`) を削除 (text_input cursor キーが効くように)。daw_prototype の `draw_arrangement_tab` (92 LOC self-drawn) を `ui.arrangement(...)` 呼び出しに置換、`DawModel` に `arr_tracks` / `arr_view` / `arr_selected_clips` / `arr_selected_track` / `arr_rename_target` / `arr_rename_just_started` を追加し、`BeginRenameTrack` 受信で `arr_rename_target = Some(id)` + 初回 frame `ui.set_focus(text_input_wid)` → 該当 header rect 上に `text_input_at` 重ね描画 + Enter (`response.committed`) で確定 + ESC でキャンセル、on_change は track 名のみ更新 (overlay 消去とは分離)。track header 右クリックは `track_header_rects` ループで `context_menu_for(rect, ["Rename","Delete"], ...)` 発行。`arr_view.data_generation += 1;` を全編集 Edit で bump (cache 無効化)。trybuild 拡張 (`tests/ui/pass/basic.rs` で `Vec<ArrangementTrack>` / `Vec<ClipKey>` フィールドを追加、`ui.arrangement(...)` を全 17 variants の make_edit でコンパイル確認)。15 unit test pass (clip_to_rect / track_index_from_y / clip_hit Move / ResizeLeft / ResizeRight / 範囲外 / loop_band_hit_kind Start / End / Middle / 範囲外 / rects_intersect / Default 健全性 × 2 / drag_preview_geometry × 2) | ✅ 完了                     |
| 45f   | scene primitive call-order interleave (Phase 45e で発覚した z-order bug の根本修正) | 旧設計の `Scene { rects, glyph_areas, line_batches }` × `popup_*` (3 列 × 2) を **`primitives: Vec<Primitive>` × `popup_primitives` (1 列 × 2)** に統合。`Primitive::Rect/Glyph/Line` enum で call order を保つ。renderer は同 type 連続 primitive を 1 つの "run" にまとめて batch、各 run ごとに drawcall (state 切り替えコストは run 数 = 10-50 / frame で無視できる)。各 pipeline に `begin_frame` / `enqueue_run` / `upload` / `render_run` API を追加、`RectRun` / `LineRun` / `GlyphRun` で span/instance/pool index を保持。glyph は `glyphon::TextRenderer` の内部 buffer 上書き制約に対応するため `Vec<TextRenderer>` を pool 化、frame 内で run 数まで grow (allocate コストは grow 1 度だけ、shrink せず再利用)。`RunHandle` / `enqueue_runs` / `render_runs` を `pipelines/mod.rs` に共通化 (device.rs と offscreen.rs で DRY)。`scenegraph::CachedCommands` を `primitives: Vec<Primitive>` 1 本に統一。`ui::Ui` の `popup_rects/popup_glyphs/popup_lines` を `popup_primitives: Vec<Primitive>` に統一。test 用 helper (`scene.rect_count()` / `iter_rects()` / `rects_vec()` 等) を `Scene` に追加し、全 widget test を migration。これで **後から push した panel が button glyph の上に正しく描画**される (Phase 45e の text_input 背景透明 / clip drag が apply されない / cache 無効化されない bug が連鎖的に解消)。M9 Phase 44a で導入した popup pass 独立 pipeline は維持 (popup primitive を base pass 上に重ねる前提)                                | ✅ 完了                     |
| 45g   | daw_01 conversation #010 — `scroll_area` scrollbar drag bug fix         | `scroll_area.rs:108-117` で drag 判定用 thumb_rect を `offset = 0.0` で計算していたのを修正。**wheel 適用後の現在 `state.offset`** で thumb_rect を計算 (drag hit-test と描画で同一 rect を再利用する構造、乖離不能)。これにより scrolled 任意位置の thumb で drag 開始可。daw_01 側 (mixer / inspector / plugin_picker の scroll_area 利用箇所) は再ビルドのみで反映、コード変更不要。regression test `drag_starts_at_scrolled_thumb_position` を追加 (`UiHost::frame_to_edits` 4 frame シミュレート: wheel で offset 進める → scrolled thumb 位置で press → drag move + release → offset.1 > 200 を verify、旧 bug なら drag 不成立で 200 のまま) | ✅ 完了                     |

**設計判断 (M9 全般)**:

- **library widget 化は Phase 41e で完了** (2026-05-03 修正): 当初は「validation 段階で API を locked-down するとフィードバックが取りづらいため後回し」と決めたが、CLAUDE.md「理想とベストプラクティスを追求する。そのためは大胆に破壊して作り直す」方針に基づき、validation 中も breaking 変更を恐れず逐次反映する判断に変更。Phase 42-43 で発見した改善は widget API を breaking 変更で逐次反映する。daw_01 が gui_01 の Note 型を直接 import していないため、breaking 影響は軽微。
- **history group の API 形**: `begin_group("multi-delete") ... end_group()` の明示開閉と、`Edit::group(label, vec_of_edits)` の構造化のどちらが ergonomic かは Phase 41 で実証。
- **audio buffer inverse 戦略**: 案 A (full `Arc<[f32]>` snapshot) / 案 B (差分のみ Vec) / 案 C (`Arc<Vec<f32>>` COW) の 3 案を Phase 42 で検証、Phase 41 の Vec<Note> 戦略との整合性を基準に選定。
- **debug overlay は本格 prod tool ではなく validation 用**: 旧 M14 Phase 56 を validation 期間中に必要なので先行実装。
- **library helper の判定基準**: 「3 回以上繰り返される pattern なら library 吸収、それ以下は example に local helper を残す」を Phase 44 で適用。Phase 41-43 の Edit::with_inverse call site をすべて enumerate して評価。

**ergonomic 検証ポイント**:

- `Edit::with_inverse` 1 ブロックで note add/delete/move/resize が書けるか (Phase 41)
- multi-select delete で `Vec<Note>` を inverse に capture するとき `Arc<Vec<Note>>` で wrap が必須になる頻度 (`Fn + Send + Sync` 制約) (Phase 41)
- `begin_group / end_group` API が直感的に書けるか、Edit 発行ごとに group を開閉する形が冗長にならないか (Phase 41)
- 「重い data inverse」用に library helper (例: `Edit::snapshot_inverse(label, fwd, restore_from)`) を追加すべきか (Phase 42 / 44)
- Vec<f32> の Arc 共有時の `Send + Sync` 制約が問題になるか (Phase 42)

**完了条件 (DoD)** — すべて達成 (2026-05-04):

- ✅ Phase 41-43 のすべての `Edit::with_inverse` が library helper (`Edit::snapshot_inverse`) で吸収済 (Phase 44b で確認、3 件未満 helper 不要 ergonomic も verify)
- ✅ daw_01 で arrangement / piano_roll / mixer / inspector / browser 等の操作が gui_01 widget で動作 (Phase 44c-45g で会話ベース統合、conversation #001-#010 すべて [Replied])
- ✅ `cargo build --workspace` / `cargo test --workspace` (200 unit test) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- ✅ `cargo run --bin piano_roll` で N notes 操作 → Undo / Redo 確認済
- ✅ `cargo run --bin sample_edit_ops` で trim / fade Undo / Redo 確認済
- ✅ 全 example で Ctrl+F1 → debug overlay toggle 動作
- ✅ `cargo run --bin daw_prototype` で arrangement / piano_roll / mixer / sample タブ + modal demo / clip drag / Rename overlay / scrollbar drag (Phase 45g) 動作

### M10 (Arrangement 機能拡張) — ✅ 完了 (2026-05-04)

**目的**: M9 Phase 45e で実装した `Ui::arrangement` widget の **欠けている daw 機能** を追加する。Phase 45e 動作確認で user 要望が出た 3 機能 (drag&drop track 並び替え / clip volume / 縦ズーム) を library API として shipping 確定する。

**動機**: M9 Phase 45e 完了時点で arrangement widget は「最小限の DAW timeline」(track header / clip drag / loop band / playhead) を提供するが、実用には:
- track 並び替えが ↑/↓ button のみで遅い (drag&drop が DAW 慣習)
- clip ごとの volume 調整ができない (DAW core 機能)
- track の縦ズームができない (画面が狭いとき不便)

これら 3 つは conversation #005 確定 API の **拡張** であり、API breaking 変更も恐れず 1 commit ずつで shipping (CLAUDE.md「破壊して作り直す」原則)。

| Phase | テーマ                                       | 主な成果物                                                                                                                                                                                                                                  | 状態      |
|-------|----------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------|
| 46    | drag&drop で track 並び替え                   | `ArrangementEditRequest::ReorderTracks(Vec<u32>)` 新 variant (新順での `track.id` 列)。`ArrangementResponse.reordering: Option<u32>` (drag 中 track id) 追加。`ArrangementStyle` に `reorder_drop_indicator` / `reorder_drop_indicator_h` / `reorder_drag_alpha` 追加 (Default 経由非 breaking)。`ArrangementState.track_reorder: Option<TrackReorderSession>` 追加 (anchor_track_id / anchor_index / anchor_mouse_y / last_mouse_y、release frame の pos 不安定対策で last_mouse_y を毎 frame update、既存 ClipDragSession と同パターン)。track header rect 上で M/S/Up/Dn/Del button rect 非 hit + primary press → reorder セッション開始、Name button area は include (drag start zone)。release で `dy >= 16px` なら `apply_reorder(cur_ids, anchor_index, target)` で新順発行、`< 16px` は click 格下げ (button_at の SelectTrack / Rename trigger に任せる)。drag 中は cached 外で **半透明 dragging row 複製** (`reorder_drag_alpha=0.6`、track_selected_bg を base color) と **横 line drop indicator** (header_pane + lanes 全幅、target row top に `reorder_drop_indicator_h=2px`) を float 描画。pure helper `compute_reorder_target_index(anchor_idx, mouse_y, header_top, track_top, row_h, n_tracks)` + `apply_reorder(ids, anchor_idx, target)` を切り出して 9 unit test (上端外 / 下端 clamp / self+next no-op / above keeps / below offsets / scroll / row_h=0 / apply 基本 / OOB safe)。`header_row_layout(row) -> HeaderRowLayout` 共通 helper を抽出 (per-track loop + reorder press detection で DRY)。daw_prototype に `ReorderTracks(order)` ハンドラ (id lookup + Vec rebuild、N_TRACKS=12 で O(n^2) 受容) と trybuild basic.rs に variant 追加 (1 commit で全 caller 追従) | ✅ 完了 |
| 47    | ~~clip volume 編集~~ → Phase 47b で再設計     | 当初 clip 底辺の volume band を実装 (commit 7f22500) したが、user 要望は **track header volume** だった (DAW 慣習: mixer fader と同じ位置)。clip gain は将来 `effective = track.vol * clip.vol` 乗算で再導入予定として revert (commit d7306ef) | ⏪ Revert |
| 47b   | track header volume slider                     | `ArrangementTrack.volume: f32` (`0.0..=1.0`、`1.0` で unity) + `ArrangementEditRequest::SetTrackVolume { track, prev, next }` + `ArrangementResponse.dragging_track_volume: Option<u32>` (Phase 47b commit f3603a6)。track header rect 内 buttons の **下に horizontal slider band** を描画 (`HeaderRowLayout.volume_band: Option<Rect>` で進歩的開示: `btn_h + band_gap + band_h <= inner.h` のときだけ表示、default `track_volume_band_h=4` で row_h >= 34 必要 = 32px では非表示、Phase 48 Alt+wheel 縦ズームで表示できる progressive disclosure)。`TrackVolumeDragSession` state (track_id / anchor_volume / band_rect / last_mouse_x、release frame の pos 不安定対策)。press 振り分け: volume band 内 → 最高 priority で TrackVolumeDragSession、その他で Name/row 背景 → reorder。drag 中は cached の band 描画と並行して preview volume を band fill 幅に反映 (display_v 切替の 1 panel 呼び出しでリアルタイム feedback)。release で `volume_from_mouse_x(last_mouse_x, band_x, band_w)` で 0..1 マップ + `SetTrackVolume` 発行。`ArrangementStyle` に `track_volume_band_h` / `track_volume_band_track` / `track_volume_band_fill` 追加 (Default 経由非 breaking)。pure helper `volume_from_mouse_x` を 3 unit test (basic / clamp 範囲外 / zero width safe) + `header_row_layout` band 表示判定 3 test (default 32px 非表示 / 34px+48px 表示 / band_h=0 disable) = +6 unit test。daw_prototype: `DawTrack.volume: f32` 追加 + `SetTrackVolume` ハンドラ + `track_row_h: 36.0` (band 即視のため) | ✅ 完了 |
| 48    | 縦ズーム (`track_row_h` 動的変更)             | `ArrangementEditRequest::SetTrackRowH(f32)` 新 variant。**`Alt+wheel`** で track_row_h を `factor = (-dy * 0.005).exp()` で乗算して発行 (zoom_x と同じ exp curve)。既存の `Ctrl+wheel = SetZoomX` / `Shift+wheel = SetScrollX` / `plain wheel = SetTrackTop` と独立 modifier。daw_prototype: `SetTrackRowH(h)` ハンドラで `h.clamp(16.0, 96.0)` + `track_top` 上限再計算 + `data_generation` bump。integration test (UiHost::frame_to_edits + Alt 修飾 + scroll_delta) で SetTrackRowH 発火 + SetTrackTop 不発火 + exp curve で row_h 更新を確認。macOS の `Option+scroll = system 横スクロール` 衝突は将来 macOS 対応時に winit event 受信側で対処 | ✅ 完了 |

**設計判断**:

- **drag&drop preview の見た目**: dragging track header を半透明 (`alpha 0.6`) でカーソル位置に追従 + drop indicator (横 line、target track の上下) を描画。release で `ReorderTracks(new_order)` 発行
- **clip volume の slider 位置**: 「clip 内 bottom band」 vs 「clip 上に重ねる外部 widget」の 2 案。前者を採用 (clip と一体感、外部 widget だと clip 数 = widget 数で重い)
- **縦ズームの modifier**: **`Alt+wheel`** を採用 (Ableton / Reaper 標準と整合、1 modifier で ergonomic、gui_01 内 Alt 未使用)。`Ctrl+Shift+wheel` (Bitwig 派生) は 2 modifier で却下

**完了条件 (DoD)** — すべて達成 (2026-05-04):

- ✅ Phase 46-48 全完了 (Phase 47 → 47b は user 要望で track header volume に再設計。clip gain は将来 phase で乗算追加予定)
- ✅ `cargo build --workspace` / `cargo test --workspace` (216 unit test) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- 🔲 `cargo run --bin daw_prototype` で track drag&drop / **track header volume slider** / **Alt+wheel** 縦ズーム を実機確認 (user 側で実施)

---

## 凍結 (M9+M10 完了後の再評価対象)

下記 milestone は **凍結**。M9 (Real DAW Validation) + M10 (Arrangement 機能拡張) 完了の状態で、改めて優先順位を見直す。canonical な phase 番号は M9-M10 のみ。

### 凍結: 描画 / theming + アニメ + アイコン (旧 M9)

- theming システム (`Theme` struct、color palette / state スタイル)
- dark / light theme 切替
- アニメーション (`AnimatedValue<T>` / spring)
- vello 統合 + SVG アイコン
- gradient / shadow / blur

### 凍結: DAW 固有 widget — 信号処理系 (旧 M10)

- piano keyboard widget (単体、piano_roll と独立した「鍵盤だけ」widget)
- spectrogram (FFT + LOD)
- EQ curve / filter response (Bezier flatten 流用)

### 凍結: テキスト / 多言語 (旧 M11)

- IME 強化 (preedit cursor 精度、proportional font 対応)
- font fallback (CJK / emoji)
- 複数行 text editor
- 文字選択 (drag / triple-click / shift+arrow)
- i18n 基盤

### 凍結: OS 統合 (旧 M12 残り)

- file dialog 強化 (filter / directory chooser / recent files、現状 rfd 0.15 minimum)
- cursor 詳細 (Resize 8 方向 / Help / NotAllowed 等の full set、現状 Move / EwResize / Default のみ)
- fullscreen / borderless (DAW ライブモード等)

### 凍結: 開発体験 + リリース整備 (旧 M14)

- widget tree inspector (frame の widget 構造可視化、hit-test 視覚化)
- snapshot test framework (OffscreenRenderer + PNG diff、CI 自動化)
- widget catalog example
- rustdoc 充実 (user guide / tutorial / API doc)
- CI + benchmark regression (Github Actions / criterion baseline)

### 不採用

- **AccessKit** (旧 M6 Phase 20、2026-05-02 ユーザ判断): 個人 DAW プロジェクトのため対象外、再評価対象外。
- **baseview / 実 plugin host 検証 / プラグイン UI host integration sample** (旧 M13、2026-05-04 ユーザ判断): daw_01 で plugin editor は **別 stack** で動作中 (`Win32 / X11 embedding` 等)。gui_01 は「ホスト DAW のメイン UI」専用に割り切り、plugin renderer 用途は drop。
- **multi-window** (旧 M12、2026-05-04 ユーザ判断): plugin editor 別 stack なら gui_01 で複数 window を持つ動機は薄い (mixer 別 window 等のホスト UI 分離は単一 window + tab/split で代替)。drop。
- **`embedded_host` example** (M9 Phase 21 OffscreenRenderer の PNG snapshot は維持、plugin host 連携サンプルとしての立ち位置は drop): plugin host 用途を drop したことに合わせて、example は OffscreenRenderer の **動作確認** 単体例として位置付け直す (実 plugin に embed する手順は提供しない)。

---

## 設計上の不変条件

1. ライブラリ提供 API は **ユーザ Model 型に `Clone`/`PartialEq`/`Hash`/`Default` を要求しない**。差分検出は ID + プリミティブ末端値の hash でだけ行う。
2. メッセージ型は導入しない (Edit は `Mutate(Box<dyn FnOnce>)` または `Undoable { Arc<dyn Fn>, Arc<dyn Fn>, label }`)。`Application::Message: Clone` 伝染を構造的に防ぐ。
3. `derive` マクロは禁止 (Lens 等)。
4. ライブラリは **audio / IPC / プロセス間通信に一切関知しない**。Edit を返すところで責務を切る。
5. heavy() 以外でも viewport culling は前提 (1000 ウィジェット級は通常パスで耐える)。
6. `Ui<'a>` の `'a` で借用ライフタイムを統一し、GAT を使わない。
7. **波形ウィジェット固有**:
   - `WaveformSource` は借用のみ。`samples: &[f32]` の Clone は禁止。
   - LOD ピラミッドは派生データ (min/max ペア) で、生サンプルのコピーは禁止。
   - 再構築判定は `generation: u64` のみ。中身 hash や bitwise 比較は禁止。
   - 録音中の追記 (`valid_len` 拡大) はインクリメンタル拡張で扱う。
8. **Undoable Edit (M8 以降)**:
   - `Edit::with_inverse(label, forward, inverse)` は `Fn + Send + Sync + 'static` を要求するが、ユーザ Model 型に `Clone` を要求してはならない (closure 内でフィールドを set する形で書ける)。
   - 重い data の inverse capture でユーザに `Arc<...>` boilerplate を強要する状況になったら、library helper で吸収する (M9 Phase 44 で評価)。

---

## ビルド構成 (確定)

- **Rust Edition: 2024** (`rust-toolchain.toml` で固定済)
- **rust-version: 1.95** (workspace ルートで固定済)
- **依存**: `[workspace.dependencies]` で一元管理、メンバー crate は `<crate>.workspace = true` で参照
- **更新方針**: マイルストーン毎に最新安定版を確認、breaking change があれば追従

---
