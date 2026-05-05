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
| 47c   | track header の ↑/↓/× button 削除 + Delete shortcut で track 削除 | drag&drop reorder (Phase 46) + Delete shortcut で機能が重複したため、track header から `↑` (MoveTrackUp) / `↓` (MoveTrackDown) / `×` (DeleteTrack) buttons を削除。`HeaderRowLayout.buttons` を `[Rect; 5]` → `[Rect; 2]` に縮小 (M / S のみ残す)、`HeaderRowLayout.inner` field も未使用化したので削除。Name area が削減分 (5 buttons → 2 buttons = -72px) 広くなる。Delete shortcut handler を if/else 拡張: `selected_clips` 非空 → `DeleteClips`、空 + `selected_track` Some → `DeleteTrack`。`MoveTrackUp/Down` / `DeleteTrack` Edit variants は context_menu (Rename/Delete) / 将来の keyboard 用に **残存** (API 非 breaking、widget 内 emit のみ削除) | ✅ 完了 |
| 48    | 縦ズーム (`track_row_h` 動的変更)             | `ArrangementEditRequest::SetTrackRowH(f32)` 新 variant。**`Alt+wheel`** で track_row_h を `factor = (-dy * 0.005).exp()` で乗算して発行 (zoom_x と同じ exp curve)。既存の `Ctrl+wheel = SetZoomX` / `Shift+wheel = SetScrollX` / `plain wheel = SetTrackTop` と独立 modifier。daw_prototype: `SetTrackRowH(h)` ハンドラで `h.clamp(16.0, 96.0)` + `track_top` 上限再計算 + `data_generation` bump。integration test (UiHost::frame_to_edits + Alt 修飾 + scroll_delta) で SetTrackRowH 発火 + SetTrackTop 不発火 + exp curve で row_h 更新を確認。macOS の `Option+scroll = system 横スクロール` 衝突は将来 macOS 対応時に winit event 受信側で対処 | ✅ 完了 |
| 49    | track volume drag 中の live update + Undoable wrap (#011 症状 1) | daw_01 #011 で報告の「mixer fader (live update) と arrangement track volume band (release-only) の挙動非対称」を修正。`fader_at` (`crates/ui/src/widgets/fader.rs:215-237`) と同パターンを採用: drag 中の各 frame で `SetTrackVolume { prev: anchor, next }` Mutate 発火 (`TrackVolumeDragSession.last_emitted_volume` で同値発火を抑制) + release frame で `Edit::with_inverse` で Undoable wrap (forward = anchor→end / inverse = end→anchor、Ctrl+Z で 1 回 undo)。release frame の `pointer.primary_just_released` 判定で drag-中 Mutate を suppress (二重発火回避)。`make_edit` の trait bound に `Clone` を追加 (Undoable の forward/inverse 2 closure に分配するため、daw_prototype + trybuild basic.rs の closure literal は capture が自動 Clone なので追加対応不要)。daw_prototype 検証用に Arrangement タブ下部に `mini mixer strip` (各 track の `Ui::fader_at`) を追加 — `arr_tracks[i].volume` を共有する 2 つの UI が **drag 中も双方向リアルタイム追従** することを 1 画面で確認可能 (DAW 慣習で arrangement 上 / mixer 下) | ✅ 完了 |
| 50    | track reorder release frame の optimistic preview (#011 症状 2) | daw_01 #011 で報告の「Edit::ReorderTracks の deferred apply で release 直後 frame に旧順序の lanes が描画される 1 frame 遅延」を修正。release frame で `pending_reorder_order: Option<Vec<u32>>` を計算 → `tracks_for_draw: Vec<ArrangementTrack>` を新順序で組み立てて cached layer + per-track header loop の両方で使用 (release Edit は同じ `pending_reorder_order` を再利用、二重計算回避)。`viewport_key` に `pending_reorder_hash` を含めて release frame で cache miss 強制 (新順序での再描画)。tuple Hash 上限 (12 要素) 超えのため nested tuple `((existing 12 fields), pending_reorder_hash)` に変更。`pending_reorder_order` に含まれない track は元順序のまま末尾に keep (防御)。daw_prototype は連続再描画 (sim_phase アニメ) のため 1 frame 遅延の可視確認は困難だが、ロジックとして release frame で同 frame 内に新順序が反映される | ✅ 完了 |

**設計判断**:

- **drag&drop preview の見た目**: dragging track header を半透明 (`alpha 0.6`) でカーソル位置に追従 + drop indicator (横 line、target track の上下) を描画。release で `ReorderTracks(new_order)` 発行
- **clip volume の slider 位置**: 「clip 内 bottom band」 vs 「clip 上に重ねる外部 widget」の 2 案。前者を採用 (clip と一体感、外部 widget だと clip 数 = widget 数で重い)
- **縦ズームの modifier**: **`Alt+wheel`** を採用 (Ableton / Reaper 標準と整合、1 modifier で ergonomic、gui_01 内 Alt 未使用)。`Ctrl+Shift+wheel` (Bitwig 派生) は 2 modifier で却下

**完了条件 (DoD)** — すべて達成 (2026-05-04):

- ✅ Phase 46-48 全完了 (Phase 47 → 47b は user 要望で track header volume に再設計。clip gain は将来 phase で乗算追加予定)
- ✅ `cargo build --workspace` / `cargo test --workspace` (216 unit test) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- 🔲 `cargo run --bin daw_prototype` で track drag&drop / **track header volume slider** / **Alt+wheel** 縦ズーム を実機確認 (user 側で実施)

### M11 (Library widget 拡充: drag-reorder list) — 進行中

**目的**: daw_01 conversation `#012 [Replied]` (2026-05-04) で要望された `Ui::reorderable_list` widget を新設し、汎用 drag&drop reorder を library API として提供する。daw_01 track_inspector の Chain section (MIDI FX / FX 内 plugin 順序入替) の `push_rect` 直呼びを置換可能にし、Phase 5 仕上げで残った「view layer の `push_rect` 直呼び 0 件」DoD を達成可能にする。

**動機**: M10 完了時点で gui_01 の list 系 widget は 2 種:
- `Ui::list_view` (M9 Phase 45d): 単純 scroll list (drag-reorder 内蔵せず、#007 で別 widget 路線確定済)
- `Ui::arrangement` 内蔵 track reorder (M10 Phase 46/50): timeline 専用、外に出ていない

両者の中間 = 「汎用 drag-reorder list」が欠落していたが、`#012` の要望と M10 で外出し済の pure helper (`compute_reorder_target_index` / `apply_reorder`) を組み合わせれば短工数で実装可能。

| Phase | テーマ | 主な成果物 | 状態 |
|---|---|---|---|
| 51 | `Ui::reorderable_list` 新設 | **公開型** (`ReorderableListStyle` / `ReorderableListResponse` / `ReorderableListEditRequest::Reorder(Vec<usize>)`)、`Ui::reorderable_list<T, F, R>` method (scroll_area wrapper + drag session state + commit-by-release で Reorder 1 度発行)。**`drag_handle_w`** parameter 対応 (`0.0` で row 全体 drag = Bitwig 風、`> 0.0` で row 左端 N px だけ drag 起点 = Logic / Cubase 風グリップ、残り領域は row callback の button_at 等が消費可)。**drop indicator** 横 line (target 位置に row 全幅、viewport clamp 付き)。**dragging row 半透明背景** (`row_bg_dragging`)。**release frame optimistic preview** (`pending_order` を 1 frame 保持して新順序で描画、Edit 適用 1 frame 遅延の visual 揺れを抑える、arrangement Phase 50 と同パターン)。`apply_reorder` を **`<T: Clone>`** に generic 化 (既存 `&[u32]` caller は単相化で無修正、`reorderable_list` は `&[usize]` で利用)。pure helper `compute_reorder_target_index` (anchor + target index 計算、id 概念無し) はそのまま流用。`make_edit: Fn(...) -> Edit<M> + Clone + Send + Sync + 'static` (M10 Phase 49 の `Ui::arrangement` と同 trait bound、Undoable Edit の forward/inverse 2 closure に分配するため `Clone` 必要)。**unit test 7 件** (visible row 数 / virtualization / 短 release で click 格下げ / drag release で Reorder 発行 / drag_handle_w 範囲外 press 無視 / drag_handle_w 範囲内 press / 空 list)。**trybuild basic.rs に reorderable_list 呼び出し追加** (no-Clone Model 制約 CI 固定)。**daw_prototype Demo Dialog に Plugin Chain demo** 1 セクション追加 (list_view デモの下、5 plugin chain を drag で並び替え) — memory `feedback_use_new_abstractions.md` 「新抽象は次の機会に使う」原則。`crates/ui/src/widgets/reorderable_list.rs` 669 LOC (test 含む) | ✅ 完了 |
| 52 | `Ui::text_input_at_focused` 新設 (daw_01 #013) | rename UI / inline edit の「メニュー → text_input 表示 → 即タイプ可能」(Logic / Bitwig / Cubase 慣習の F2 rename) を 1 関数で実現する `Ui::text_input_at_focused<F>(id, rect, text, on_change) -> TextInputResponse` を新設。`text_input_at` (既存) は不変 (breaking なし)。**初回 show 判定は frame counter なしに既存 `Scenegraph` の eviction 機構を活用**: `Scenegraph::contains(wid)` を pub method で出し、`Ui::was_widget_visible_last_frame(wid)` `pub(crate)` helper 経由で「前フレームに `with_widget_node` で描画されたか」を判定。`!was_visible_last_frame` のときに `set_focus(wid)` + `cursor_byte = text.len()` を実行 → caller 側の boolean flag 不要。完全に非表示 (フレーム飛ばし) → 戻ったときも再 focus される (= eviction で初回扱い)。**unit test 3 件** (初回 show で focus 取得 / 連続 visible では caller の手動 set_focus を上書きしない / 不可視 → 再表示で再 focus)。**trybuild basic.rs に呼び出し追加** (no-Clone Model 制約 CI 固定)。**daw_prototype の `arr_rename_just_started: bool` フィールド + 関連 boilerplate (BeginRenameTrack 2 箇所での set / overlay 描画箇所での `WidgetId::ROOT.child((b"text_input", &id))` 再現 + `set_focus`) を完全削除** し、`text_input_at_focused` 1 行に置換 — memory `feedback_pursue_best_practice` 「ユーザに workaround を強要する API は設計欠陥」と `feedback_use_new_abstractions` 「新抽象を次の機会に使う」を同時に満たす | ✅ 完了 |

**設計判断**:

- **`Reorder(Vec<usize>)` 元 index ベース**: chain plugin は識別子を持たない (`Vec<PluginInstance>` で index ベース管理) ので index が自然。`new_items[i] = items[order[i]]` semantics で commit-by-release で 1 度発行。stable id を持つデータ向けには将来 `key_of: Fn(&T) -> K` overload を追加する余地を残す (本 phase ではスコープ外)。
- **release frame `pending_order` を 1 frame 保持**: arrangement Phase 50 (track reorder) と同パターンで、release Edit が deferred apply されることによる「release 直後 frame で旧順序が描かれる」visual 揺れを抑える。次フレームで `take()` して消費する単純実装。
- **`drag_handle_w` を style に持たせる**: `0.0` (= row 全体 drag) を default にしつつ、`> 0.0` で row 左端だけを drag 起点にできる設計値。残り領域は row callback の button_at 等が click を消費する想定 (chain row の "GUI / ×" ボタン等)。
- **drop indicator は scroll_area 内で push_rect 直呼び**: drag float row (popup_layer 経由) は採用せず、scroll_area の closure 内で row 群と同じ z-order に描画する単純実装 (modal や popup の上に出ない、リスト内に閉じる方が DAW UX に整合)。

**完了条件 (DoD)**:

- ✅ Phase 51-52 完了
- ✅ `cargo build --workspace` / `cargo test --workspace` (226 unit test、+10 件 = reorderable_list 7 + text_input 3) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- 🔲 `cargo run --bin daw_prototype` で Demo Dialog → Plugin Chain reorder + track header 右クリック → Rename → 即タイプ可能 (Phase 52) 動作確認 (user 側で実施)

### M12 (Performance optimization — perf_review_2026-05-04 P0 系) — 進行中

**目的**: 2026-05-04 の全体 perf review ([docs/perf_review_2026-05-04.md](perf_review_2026-05-04.md)) で発見された P0 critical 2 件を修正する。`Ui::with_widget_node` の cache hit/miss 経路で `Primitive` を Vec ごと毎フレーム clone している (Glyph に String / Line に Vec を内包) のと、`arrangement.rs` の `tracks_for_draw.clone()` が release frame 以外でも 2 度発火する問題。

**動機**: scenegraph cache の「cache hit が free」前提が、`Primitive::clone()` の中で String/Vec の二次 alloc が発生していたため崩れていた。1000+ widget のフレームで毎 hit ごと alloc が widget × primitive 数だけ発火し、heavy() で expecting する 60fps 維持が脅かされる状態。

| Phase | テーマ | 主な成果物 | 状態 |
|---|---|---|---|
| 53 | `Primitive` 内コンテナを `Arc` 化 (P0-1) | `GlyphArea::text: String` → `Arc<str>`、`LineBatch::segments: Vec<LineSegment>` → `Arc<[LineSegment]>` に **breaking 変更**。`Primitive::clone()` を refcount のみに圧縮し、`with_widget_node` の cache hit/miss 経路 (scenegraph) で発火する `cached.primitives.iter().cloned()` / `to_vec()` が String/Vec の二次 alloc 無しで済むようにする。renderer pipeline 側 (`area.text.hash()` / `&area.text` で `Buffer::set_text` / `batch.segments.iter()` 等) は `Arc<T>` の deref で全て無変更。`line.rs:174` の `for seg in &batch.segments` だけ `&Arc<[T]>` が `IntoIterator` でないので `batch.segments.iter()` に変更。`menu.rs:634` の `g.text.as_str()` は `Arc<str>::as_str` が unstable のため `g.text.as_ref()` に変更。構築側 (~30 箇所、widget / example / test 全部) は `text: foo.to_string()` → `text: foo.into()` (`&str → Arc<str>`) / `segments: my_vec` → `segments: my_vec.into()` (`Vec → Arc<[T]>`) のように `.into()` を 1 つ追加。すでに `Arc<str>` だった `ArrangementClip.name` / `note.lyric` は `clone()` で refcount bump に変更 (旧 `to_string().into()` の二重 alloc を撲滅)。daw_01 grep で直接構築箇所無しを確認、conversation 通知不要 | ✅ 完了 |
| 54 | `arrangement.rs` の `tracks_for_draw` 冗長 clone 撤廃 (P0-2) | `tracks_for_draw: Vec<ArrangementTrack>` → `Arc<[ArrangementTrack]>` 化。heavy() closure (`'static` 要求) と per-track header loop の両方で同じデータが必要だが、Arc にすれば 2 度目以降は **refcount のみ** で済むため、`tracks_owned = tracks_for_draw.clone()` の deep clone (N tracks × M clips per track の比例 alloc) を **撲滅**。1 度目 (`Arc::from(tracks)` または reorder build → `.into()`) は `'static` 要件で不可避 (Cow は `'static` ライフタイムで成立しない)。closure 内 (`draw_lanes_bg` / `draw_clips` / `draw_selection_overlay` / `draw_drag_preview` 等) は `&[ArrangementTrack]` を取るシグネチャのまま、`&tracks_owned` (= `&Arc<[T]>`) が deref coercion で `&[T]` に変換されるので無修正。`tracks_owned.len()` も Arc<[T]> deref で動く。daw_01 影響: gui_01 が export するのは `ArrangementTrack` 構造体のみで `tracks_for_draw` は private、breaking 影響無し | ✅ 完了 |

**設計判断**:

- **内部 field Arc 化** (現行) vs **variant payload Arc 化** (`Primitive::Glyph(Arc<GlyphArea>)`) の二択で前者を採用: (a) renderer 側 API が無変更、(b) `Rect` variant は Copy のままなので局所的、(c) `GlyphArea::clip_rect` 等の field 直接参照を保てる
- **`LineBatch: Default`** は維持: `Arc<[T]>: Default` が std で `Arc::from(&[])` 相当を提供しており、derive(Default) がそのまま動く
- **`tracks_for_draw` を Arc<[T]> 化 (Phase 54) 採用、Cow 不採用**: perf_review 当初案 (`Cow<'_, [ArrangementTrack]>`) は heavy() closure の `'static` 要件で成立しない。さらに per-track header loop が closure 後に同じ data を使うため、closure 内に move しきれない (closure と per-track loop の両方で owned コピーが必要)。Arc<[T]> なら 2 度目以降は refcount のみで P0-2 の目的 (=「2 度目の clone」撲滅) を達成

**完了条件 (DoD)** — すべて達成 (2026-05-04):

- ✅ Phase 53-54 完了
- ✅ `cargo build --workspace` / `cargo test --workspace` (226 unit test、Phase 52 から件数同じ = 既存 test の API 互換維持) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- 🔲 `cargo bench -p daw-ui-core --bench scenegraph_cache` で cache hit frame の per-frame µs が削減されること (期待 30-70% 短縮、bench 環境差で揺れるため数値確認は user 側で実施)
- 🔲 `cargo run --bin daw_prototype` で arrangement track reorder / clip drag / volume drag の regression なしを目視確認 (user 側で実施)

### M13 (Library widget polish: time_ruler / bar_beat_grid 統合) — 進行中

**目的**: `Ui::arrangement` / `Ui::piano_roll` を library `time_ruler` / `bar_beat_grid` に乗せ替えて、daw_01 conversation #014 の 3 要望 (a) 小節番号テキスト表示、(b) `time_sig` 対応 grid (3/4・5/4・6/8 等で bar 線が正しい拍位置に出る)、(c) piano_roll の上部 ruler 領域新設 を 1 commit で達成する。CLAUDE.md「新しく入れた抽象は次の機会に使う」原則と memory `feedback_pursue_best_practice`「ユーザに workaround を強要する API は設計欠陥」を同時に満たす (daw_01 が「自前で別途 ruler を組む」回避策を強いられていた状況を library 内で吸収)。

**動機**: M7 Phase 27 で `Ui::time_ruler` / `Ui::bar_beat_grid` (拍/小節縦線 + 小節番号テキスト + `time_sig` 対応式 `numerator * 4 / denominator`) を library に追加していたが、`Ui::arrangement` / `Ui::piano_roll` widget は内部で独自に bar/beat 縦線を描画していた (= `b.rem_euclid(4) == 0` の 4 拍ハードコード、ruler に小節番号テキスト無し、piano_roll は ruler 領域自体無し)。library 抽象が arrangement / piano_roll でしか使い道がない上に、それらの widget が library 抽象を使っていない設計欠陥を解消する。

| Phase | テーマ | 主な成果物 | 状態 |
|---|---|---|---|
| 55 | `Ui::arrangement` / `Ui::piano_roll` を library `time_ruler` / `bar_beat_grid` に統合 (daw_01 #014) | `ArrangementView` / `PianoRollView` に **`bpm: f32` + `time_sig: (u8, u8)`** を追加、`PianoRollView` には **`ruler_h: f32`** も追加 (PianoRollView は Default impl 無いので breaking、3 caller を 1 commit で全更新)。`ArrangementStyle::ruler_label_color` / `PianoRollStyle::ruler_bg` + `ruler_label_color` を新設 (Default 経由で非 breaking)。`HeavyCtx::time_ruler` / `HeavyCtx::bar_beat_grid` delegate 追加 (cached layer 内で呼ぶ enabler)。`arrangement::draw_ruler_bg` 関数を削除し `hctx.time_ruler` に置換 (小節番号テキスト出る)。`arrangement::draw_lanes_bg` の bar/beat 縦線描画ループを削除し `hctx.bar_beat_grid` に置換 (3/4・6/8 等で bar 線が time_sig 対応)。`piano_roll::draw_grid_background` の (c) 拍縦線セクションを削除し `hctx.bar_beat_grid` に置換、レイアウトに ruler 領域を新設 (`grid.y = rect.y + ruler_h`、ruler は keyboard_w から始まる、`ruler_h: 0.0` で旧 piano_roll 互換)。**4 拍ハードコード `b.rem_euclid(4) == 0` を 3 箇所すべて撲滅**。viewport_key を **v2 化**、arrangement は 3 つ目の nested tuple として `(bpm, time_sig.0, time_sig.1)` を、piano_roll は `ruler_h.to_bits()` + 12 要素目に `(bpm, time_sig.0, time_sig.1)` 組タプルとして追加 (tuple Hash impl 12 要素上限に収める)。**library `time_ruler` の BarBeat label format を `mapping.format` (= "1.1", "2.1", ... 形式) から `format!("{bar_num}")` (= "1", "2", "3", ... 形式) に変更** (daw_01 #014 が期待する小節番号のみの表示、Seconds/SMPTE は `mapping.format` 経由のまま)。`ruler_h > 0.0` ガードで ruler を skip 可能 (旧 piano_roll 互換 + arrangement の test_view 互換)。**unit test 6 件追加** (arrangement: 3/4 で bar 線が 3 拍ごと / "1" "2" "3" ラベル / time_sig 切替で bar 線 set 変化、piano_roll: ruler_h=0 で label 無し / ruler_h=20 で "1" "2" 出る / grid bar 線の y_top が >=ruler_h)。daw_prototype `arr_view` / examples/piano_roll `view` / trybuild `basic.rs` の 3 caller を全更新 (`PianoRollView` Default impl 無し breaking)。**daw_01 への影響**: gui_01 commit と同期で `ArrangementView` / `PianoRollView` 構築箇所に `bpm` + `time_sig` (+ `ruler_h`) を追加する必要あり (daw_01 conversation #014 で受領後の追従指示を記入) | ✅ 完了 |

**設計判断**:

- **`bpm: f32` + `time_sig: (u8, u8)` を直接 view に持つ案 A** を採用 (`TimeMapping` を直接受ける案 B は不採用): daw_01 caller の薄さを優先 (`bpm: app.song.bpm, time_sig: app.song.time_sig` の 2 行で済む)。`sample_rate` は widget 内部で `48_000.0` ダミー値を合成 (BarBeat 表示の bar 線位置計算では比例定数として打ち消されるため不要、Seconds/SMPTE 切替時は将来別 API で受ける)。
- **`HeavyCtx` に `time_ruler` / `bar_beat_grid` delegate を追加**: `hctx.cached(viewport_key, |hctx| ...)` 内から `Ui::time_ruler` を直接呼べないため (HeavyCtx は Ui ではない)。既存 `label_at` / `button_at` / `context_menu_for` delegate と同パターン。`with_widget_node` のネスト (cached layer wid 内で time_ruler wid) は既存 `HeavyCtx::cached` 自身の `with_widget_node` 呼び出しと同じパターンで問題なし。
- **`ruler_h > 0.0` ガード**: arrangement / piano_roll 両方で ruler 領域が空のとき `time_ruler` 呼び出しを skip。これがないと ruler 内 tick 線がすべて bar_line color (= tick_color として共有) で出てしまい、cached layer 全体の primitive 数が増える + lanes/grid と区別できないノイズが入る。`piano_roll` は元々 ruler 無し (旧互換) なので必須、`arrangement` も既存 `ruler_h: 0.0` test_view との互換のため同じガードを入れる。
- **library `time_ruler` の BarBeat ラベル変更**: `mapping.format(s)` の出力 `"1.1"` (= bar.beat) は daw_01 #014 が期待する小節番号のみ "1", "2", "3" と異なる。`time_grid.rs` 内で `match mapping.display { TimeDisplay::BarBeat => format!("{bar_num}"), _ => mapping.format(s) }` に変更。Seconds/SMPTE 表示は引き続き `mapping.format` を経由する。daw_prototype piano_roll_tab デモ (= 唯一の既存 caller) でも "1", "2" 表示になるが visible な regression は無い (簡略化方向)。
- **piano_roll の ruler は `keyboard_w` から始まる**: keyboard 上に小節番号は出さず grid と同じ x 範囲のみ (DAW 慣習)。`arrangement` の `header_w` から ruler 始まりと平行。
- **piano_roll の viewport_key で `time_sig` を `(u32, u32)` 組として 1 要素化**: 既存 10 要素 + ruler_h + bpm + time_sig = 13 要素は tuple Hash impl 12 上限超過。`(view.bpm.to_bits(), u32::from(time_sig.0), u32::from(time_sig.1))` を 1 要素にまとめて 12 要素ぴったりに収める。
- **`PianoRollView` の Default impl は維持して追加しない** (M9 Phase 45c 既存設計): caller に明示的な値設定を強制する設計を継続、3 fields 追加で 3 caller を 1 commit 全更新する (M9 Phase 45c と同パターン)。
- **daw_prototype の piano_roll_tab はスコープ外**: 同関数は `Ui::piano_roll` を未使用 (生 `push_rect` + `time_ruler` 直呼びの単体デモ)、Phase 55 では触らない。`Ui::piano_roll` 化は `DawModel.notes` / `selected_note_ids` / `notes_generation` 追加が必要なので別 phase 候補としてメモ。

**完了条件 (DoD)** — すべて達成 (2026-05-04):

- ✅ Phase 55 完了
- ✅ `cargo build --workspace` / `cargo test --workspace` (**232 unit test、Phase 54 から +6 件**) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- 🔲 `cargo run --bin daw_prototype` で arrangement タブの ruler に "1", "2", "3" 等の小節番号が出ること、time_sig (3, 4) 切替で bar 線が 3 拍ごとに移動することを目視確認 (user 側で実施)
- 🔲 daw_01 conversation #014 を `[Open]` → `[Replied]` に更新 (本 commit と同 sequence で別 step、`daw_01` ディレクトリでは commit しない)

### M14 (Modal popup の input masking + `button_at_clicked` — daw_01 #015) — 進行中

**目的**: daw_01 conversation `#015 [Open]` (2026-05-05) で報告された plugin_picker (`Ui::modal` + `Ui::list_view` で構築) の 2 件のバグを 1 commit で修正する。 (a) ✕ ボタンクリックが反応しない、(b) modal 内 list_view で wheel scroll が効かない。両者とも gui_01 側 root cause、API 2 点の追加で完結。

**動機**:
- (a) `button_at` の `on_click: FnOnce() -> Edit<M>` は `&mut Ui` を取れないため、click closure 内で `close_modal` を呼べない。Edit が daw_01 側 boolean を flip しても gui_01 内部の `open_popups` HashMap (popup state) は不変 → `modal.rs:87` の `is_modal_open` が true のまま → modal 描画継続。「click が無視される」と見える symptom の正体。menu item `m.item("New", |ui| { ui.push_edit(...) })` (M9 P1-5) で確立した「click handler が `&mut Ui` を取る」 pattern を button にも提供する必要があった。
- (b) daw_01 `root.rs:73` で `arrangement_view::draw` が `plugin_picker::draw` より先に呼ばれ、`arrangement.rs:1783` の `take_scroll_in_rect(lanes)` が pointer (modal panel 内) の scroll_delta を消費 → modal の list_view が呼ぶ頃には (0, 0)。modal panel と arrangement.lanes 矩形が overlap するため発生 (1280×720 で modal 中央 (640, 360) と lanes (440, 88, 840, 368) がかぶる)。同種の問題は `take_drag_rect_in_rect` / `take_double_click_in_rect` でも潜在 (今回は未報告だが将来必ず出る)。

| Phase | テーマ | 主な成果物 | 状態 |
|---|---|---|---|
| 56 | `button_at_clicked` 新設 + 全 take_* で modal-aware masking | **(a) `Ui::button_at_clicked(id, text, rect) -> bool`** を `crates/ui/src/widgets/button.rs` に新設 (`#[must_use]`)。click された frame で `true` を返す Edit-less 版。既存 `button_at` の本体をこちらに移し、`button_at` は `if button_at_clicked(...) { push_edit(on_click()) }` で 1 行 wrapper にリファクタ (DRY、hit-test 挙動の乖離防止)。menu item と同じ「click handler が `&mut Ui` を必要とする」pattern を button に提供 (modal の `close_modal` / `set_focus` / 複数 `push_edit` / 動的 popup 開閉)。**(b) `Ui::pointer_blocked_by_modal_popup()` helper** を `pub(crate)` で `crates/ui/src/ui.rs` に追加 — `drawing_in_popup` でない caller かつ pointer.pos が `open_popups` の `modal=true` な anchor 内にあれば `true`。`take_scroll_in_rect` / `take_drag_rect_in_rect` / `take_double_click_in_rect` 冒頭で `if self.pointer_blocked_by_modal_popup() { return /* zero/None */; }` で早期 return。modal popup の下に隠れている widget は pointer 入力を一切消費しない (overlay の意味が失われる問題を解消)。popup_layer 内 (modal の body) は `drawing_in_popup=true` なので通常通り消費可能。**unit test 5 件**追加 (button: press+release inside で click=true / press outside → release inside で click=false、ui: take_scroll で modal 下 (0,0) と popup_layer 内 (0,-3) の対比 / take_drag_rect で modal 下は drag 開始しない / take_double_click で modal 下は None)。`Ui::button_at` 既存 caller は無修正 (返り値追加のみ非破壊)、daw_01 plugin_picker.rs の ✕ ボタンは `button_at_clicked` + `close_modal` への書き換えが必要 (conversation #015 で daw_01 Claude に指示)。wheel 側は daw_01 側コード変更不要 (gui_01 修正のみで効くようになる) | ✅ 完了 |
| 57 | `text_input` の commit を NumpadEnter でも検出 (daw_01 #016) | DAW 数値入力 (BPM / time_sig / 拍数 / ピッチ等) でテンキー Enter を多用する業界慣習 (Cubase / REAPER / Logic 全部 numpad Enter で commit) に合わせる。`PhysicalKey` enum (`crates/platform/src/event.rs`) に **`NumpadEnter` variant を追加** (`Enter` 直後)。`winit_backend.rs::map_phys_key` に **`KeyCode::NumpadEnter => PhysicalKey::NumpadEnter`** マッピング追加 (旧実装は `PhysicalKey::Other(_)` に fallthrough → text_input の `_` arm で `\r` が control char filter で削られて何も起きなかった)。`text_input.rs:154-156` の commit 判定を **`PhysicalKey::Enter \| PhysicalKey::NumpadEnter => committed = true`** に拡張。`shortcut.rs::format_key` の exhaustive match に `NumpadEnter => "NumpadEnter"` arm を追加 (将来 shortcut binding に使う際の表示)。**unit test 2 件**追加 (text_input: NumpadEnter で committed=true / Enter でも引き続き committed=true 回帰防止)。Numpad0-9 / NumpadDecimal は winit 側で NumLock on 時に `KeyEvent.text = Some("0")` 等を emit するため text_input の `_ => ev.text` fallthrough で従来通り insert される (要 daw_01 側で NumLock on 時の動作確認、 NumLock off は数字入力外なので scope 外)。daw_01 側コード変更不要 (gui_01 path 依存再ビルドのみで効く) | ✅ 完了 |
| 58 | `text_input` の OS 標準 selection + Ctrl+A / Shift+Arrow / Delete / cut/copy/paste + shortcut layer の typing_focus 対応 + cursor / selection x 位置の実 advance 化 | **(a)** `TextInputState` に `anchor_byte` (selection の anchor 端) と `last_focused` (gained_focus 検知用) を追加。selection は egui `CCursorRange` 流の **anchor + cursor 2 点 (anchor==cursor で no-selection)** で表現。`prev_char_boundary` / `next_char_boundary` / `delete_range` を free fn 化。**(b)** `text_input_at` 内で「`was_focused == true && last_focused == false` で gained_focus」と判定し **anchor=0, cursor=text.len() で全選択** (click focus / programmatic focus / `text_input_at_focused` の 3 経路を 1 箇所で処理 = OS の F2 rename 標準挙動)。`text_input_at_focused` は `set_focus(wid)` のみ呼ぶ形に簡素化 (cursor 末尾設定は廃止、gained_focus path に委譲)。**(c)** キー処理を全部 `replace_range(min..max, new)` 1 形式に正規化 — 文字入力 / Backspace / Delete / IME Commit / Paste で selection あれば範囲削除 → insert、なければ単独操作。Shift+Arrow は anchor 固定で cursor のみ char 境界単位で動かして範囲拡張、修飾なし Arrow は selection あれば collapse (Left=min / Right=max)、なければ単独移動。**(d)** **`crates/ui/src/shortcut.rs` に `is_typing_only_shortcut(name) -> bool` を追加** (matches: `select_all` / `delete` / `cut` / `copy` / `paste`)。 `crates/ui/src/ui.rs` の `UiHost` に `last_typing_focus: bool` を追加し、`frame_to_edits` 冒頭の shortcut layer で **`last_typing_focus` が立っていれば** typing-only shortcut を `pending_shortcuts` に積まず `keyboard_events` に残す。frame 末尾で `last_typing_focus = typing_focus` を反映 (`focus_changed_in_last_frame` と同列の last-frame state)。**新 API: `Ui::take_typing_shortcut(name)`** — `keyboard_events` から `shortcut_map.matches(ev, mods) == Some(name)` の最初の Pressed event を消費して true。focus 中の text widget が `Ctrl+A` / `Delete` / `Ctrl+X/C/V` を取り出す統一窓口 (`shortcut.rs:209` の `M9 Phase 45e bug fix` 予告を解消)。**`heavy.rs` に delegate 追加**。**(e)** text_input の paste は `take_typing_shortcut("paste")` + `take_clipboard_paste()` で OS clipboard 経由、cut/copy は `set_clipboard_text(s)` で書き出し。`pending_clipboard_paste` 準備ロジックを **typing 中の paste shortcut も provider read を走らせる** よう拡張 (typing_paste_pending フラグ)。**(f)** `draw_text_input` を `(focused, cursor_byte, anchor_byte, preedit)` signature に変更。背景の上 / テキストの下に **selection 範囲の半透明矩形** (`Color::rgba(0.30, 0.50, 0.85, 0.45)`) を `push_rect` で描画 (focused かつ preedit 空のときのみ)。`input_hash` に `anchor_byte_for_draw` を含めて selection 状態変化で cache invalidate。**(g) cursor / selection x 位置を実 advance で計算 (M3 残作業の解消)**: ui crate に `cosmic-text 0.18.2` 直接依存を追加 (renderer の glyphon 0.11 が使う同 version で揃え)、新ファイル `crates/ui/src/text_metrics.rs` に `TextMetrics { font_system, scratch }` を新設して `measure_advance(text, font_size) -> f32` で 1 行 shape の line_w を返す実装。`UiHost.text_metrics` フィールド追加 + `Ui.text_metrics: &'a mut TextMetrics` + `Ui::measure_text(&str, f32) -> f32` API 公開。`text_input.rs` の `approx_text_width` (ASCII 7px / CJK 14px 固定概算) を **完全削除**、cursor / selection / preedit underline / IME request_ime 全箇所を `ui.measure_text(...)` に置換。proportional system font (Segoe UI 等) の "m" (~11px) や "i" (~4px) の実 advance に基づくので **pixel-accurate** に並ぶ (旧概算では `yfyfyfmmmmmmmmmmm` で 40-50px 単位の cursor ずれが出ていた)。renderer 側 `GlyphPipeline` の `FontSystem` とは別 instance だが、同じ system fonts を読むので shape 結果は一致 (キャッシュは別)。**unit test 12 件追加**: 全選択 → タイプで上書き / Ctrl+A → Delete で空文字列 / Delete (no selection) で 1 char 削除 / Backspace で範囲削除 / Shift+ArrowLeft で範囲拡張 → 'X' で末尾 char 置換 / Ctrl+C で text 不変 + clipboard 書込 / Ctrl+X で範囲削除 + clipboard 書込 / Ctrl+V で範囲を clipboard 内容で置換 / `typing_focus_blocks_global_delete_shortcut` (text_input focused フレームで `take_shortcut("delete")` が false) / TextMetrics: 空文字列 0 / 長い text の方が幅広 / 'm' は 'i' より幅広。**impact**: piano_roll / arrangement の `take_shortcut("delete")` は無修正で正しい挙動 (text_input focused 中は false / 非 focused 中は true)。daw_01 側コード変更不要 (gui_01 path 依存再ビルドのみで効く)。**break**: `text_input_at_focused` の挙動「初回 show で cursor 末尾」→「初回 show で全選択」(OS の F2 rename と一致、CLAUDE.md「破壊的 API 変更を恐れない」)。 | ✅ 完了 |

**設計判断**:

- **`button_at_clicked` を新設、`button_at` の signature 変更は不採用**: `button_at` を `FnOnce(&mut Ui<M>) -> ()` に breaking 変更する案は menu item `m.item` の pattern と整合するが、既存 `button_at` callers (mixer / track_inspector / transport / examples / trybuild など何十か所) を 1 commit で全更新する破壊コストが過大。`button_at_clicked` を別 method として追加すれば「Edit を返したい場合」と「Ui 操作したい場合」の 2 用途で signature を分離でき、menu item の `&mut Ui` pattern と方向性は同じ。
- **builtin close button (`ModalStyle::show_close_button: bool`) は不採用**: daw_01 plugin_picker の Rescan + ✕ レイアウト都合と相反、style に hardcode された見た目を強制する。daw_01 が title row 内の ✕ 位置を完全制御できる方が良い (CLAUDE.md「要件にない変更を入れない」)。
- **`Ui::sync_modal(bool)` も不採用**: on_close 二重発火 + boolean ↔ popup state 二重管理を温存するので筋が悪い。「button click → close_modal → on_close fires → Edit (ClosePluginPicker) で boolean false」の一方向フローで完結する設計 (現状 modal API の意図) を維持。
- **`take_drag` / `take_double_click` にも masking を適用**: 一貫性 (3 関数のうち 1 つだけ漏れると次のバグ報告が必ず来る — `feedback_pursue_best_practice`「ユーザに workaround を強要する API は設計欠陥」原則)。コスト極小 (helper 1 関数 + 3 箇所に 1 行ずつ)。

**完了条件 (DoD)**:

- ✅ Phase 56 完了 (commit b25dff6)
- ✅ Phase 57 完了 (commit 5bdd24a)
- ✅ Phase 58 完了
- ✅ `cargo build --workspace` / `cargo test --workspace` (**Phase 56 で 254+9+1+1=265、Phase 57 で text_input +2 = 256+9+1+1=267、Phase 58 で text_input +8 / ui +1 / text_metrics +3 = 268+9+1+1=279**) / `cargo clippy --workspace --tests -- -D warnings` / `cargo test -p daw-ui-core --test no_clone_required` 全 ✅
- ✅ daw_01 conversation #015 / #016 を `[Open]` → `[Replied]` に更新 (`daw_01` ディレクトリでは commit しない、daw_01 側 user / 別 Claude が plugin_picker.rs / transport.rs 修正と同 commit で確定)
- 🔲 user 目視確認: Phase 56 (modal demo の wheel / close button)、 Phase 57 (text_input の BPM 入力欄でテンキー Enter commit)、 Phase 58 (text_input の全選択 / Ctrl+A / Shift+Arrow / Delete / cut/copy/paste、daw_prototype の rename UI で観察) — daw_01 側で動作確認待ち

---

## 凍結 (M9+M10+M11 完了後の再評価対象)

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
