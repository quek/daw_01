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

### M9 (Real DAW Validation — `Edit::Undoable` ergonomic 実証、Phase 41-43 + 44a 完了 / Phase 44b-c 残作業)

**目的**: M8 で導入した `Edit::Undoable` の ergonomic を、note 編集 / audio buffer 編集の 2 ケースで実証する。boilerplate が出れば library helper で吸収。daw_01 で並行検証して library API の fitness function を回す。

**動機**: `Arc<dyn Fn(&mut M) + Send + Sync>` ベースの Undoable は理論上は no-Clone を守るが、`Vec<MidiNote>` / `Vec<f32>` 級の **重い inverse** を要するケース (note multi-select delete、audio trim/fade) でユーザに boilerplate を強要しないかは未検証。「新しく入れた抽象は次の機会に使う」(`feedback_use_new_abstractions.md`) を Undoable に適用するタイミング。M9 を見送って theming / animation / 信号処理 widget に直行すると、Undoable の前提が崩れたまま機能拡張が積み重なるリスク。

**並行**: daw_01 から M7/M8 実利用フィードバックが 11 項目 (P0-P3) 届いている。詳細プランは [plan_daw01_feedback.md](plan_daw01_feedback.md)。Phase 41-44 の前提となる API 整備として P0-P1 を先行実施することが多い (例: P0-1 Shortcut::parse 記号受理は piano_roll の `Shift+/` shortcut で必要、P1-3 HeavyCtx delegate は piano_roll の rect-select で必要、P1-4 double-click は clip → Piano Roll タブ UX で必要)。

| Phase | テーマ                                                                 | 主な成果物                                                                                                                                                                                                                                                        | 状態                        |
|-------|------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------|
| 41    | piano_roll の note edit + multi-select 統合 + library widget 化 (主軸) | `Edit::with_inverse` / `Edit::snapshot_inverse` で note add/delete/move/resize/select、Ui::take_drag_rect_in_rect で rect multi-select、Ui::set_cursor 公開、`Ui::piano_roll` library widget + `NotesEditRequest` enum で API 完結                                | ✅ 完了 (41pre+a+b+c+d+e+f) |
| 42    | sample_edit_ops の trim/fade を Undoable 化                            | trim/fade in/out の 3 ボタンを `Edit::snapshot_inverse` 化。audio buffer の inverse 戦略は **混在採用**: trim = full snapshot (`Vec<Vec<f32>>` + viewport/selection/cursor)、fade in/out = 範囲 snapshot (`Vec<f32>` の e-s 個 + range + direction enum)          | ✅ 完了                     |
| 43    | debug overlay (validation 用)                                          | `Ui::debug_overlay(rect, frame_ms)` + `UiHost::last_frame_stats() -> FrameStats` で cache_hits/misses/widget_count/scenegraph_size/history_depth を画面右上に popup z-order の半透明 overlay。Ctrl+F1 を default shortcut binding (`debug_overlay_toggle`) に追加 | ✅ 完了                     |
| 44a   | popup pass の renderer pipeline 独立化                                 | Phase 43 で発見した「popup pass の prepare で base pass の rect/line/glyph buffer が上書きされる」問題を、renderer に `popup_rect / popup_line / popup_glyph` 3 つの独立 pipeline インスタンスを追加して根本解決                                                  | ✅ 完了                     |
| 44b   | Undoable ergonomic 評価 + 必要なら API 改善                            | Phase 41-43 で書いた `Edit::with_inverse` 全 call site の boilerplate を計測、3 回以上繰り返されるパターンが見つかれば library helper を追加、なければ「現 API で十分」を確定                                                                                    | 未着手                      |
| 44c   | daw_01 Note schema 統合判断                                            | gui_01 Note (id: u32, f32, no lyric) と daw_01 NoteBox (note: u32 = index, f64, lyric: Option<String>) の不一致をどう統合するか方針決定                                                                                                                          | 未着手                      |

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

**完了条件 (DoD)**:

- Phase 41-43 のすべての `Edit::with_inverse` が boilerplate コメントなしで自然に書けるか、library helper で吸収できている
- daw_01 で 1 操作 (例: track 追加 + undo) を実装し、API が破綻しないことを確認
- `cargo build --workspace` / `cargo test --workspace` / `cargo clippy --workspace --tests -- -D warnings` 全 ✅
- `cargo run --bin piano_roll` で N notes を rect 選択 → Delete → Ctrl+Z で 1 step 復元、Ctrl+Shift+Z で再 delete 可
- `cargo run --bin sample_edit_ops` で trim → Ctrl+Z 復元、fade → Ctrl+Z → Ctrl+Shift+Z 可
- 全 example で Ctrl+F1 → debug overlay toggle 動作

---

## 凍結 (Phase 44 完了後に再評価)

下記 milestone はすべて凍結。**Phase 44 で Undoable ergonomic 検証 + daw_01 フィードバック取得が完了した後に、改めて優先順位を見直す**。一覧は traceability のために残すが、phase 番号は新 M9 と衝突するため参考扱い (canonical な phase 番号は新 M9 のみ)。

### 凍結: 旧 M9 (描画 / theming + アニメ + アイコン)

- theming システム (`Theme` struct、color palette / state スタイル)
- dark / light theme 切替
- アニメーション (`AnimatedValue<T>` / spring)
- vello 統合 + SVG アイコン
- gradient / shadow / blur

### 凍結: 旧 M10 (DAW 固有 widget — 信号処理系)

- piano keyboard widget (単体)
- MIDI piano roll widget (library widget 化) ※**新 M9 Phase 41 で note edit 含めて先行検証中、validation 後に widget 化判断**
- spectrogram (FFT + LOD)
- EQ curve / filter response (Bezier flatten 流用)

### 凍結: 旧 M11 (テキスト / 多言語)

- IME 強化 (preedit cursor 精度、proportional font 対応)
- font fallback (CJK / emoji)
- 複数行 text editor
- 文字選択 (drag / triple-click / shift+arrow)
- i18n 基盤

### 凍結: 旧 M12 (OS 統合)

- multi-window (各 window に独立 UiHost)
- file dialog 強化 (filter / directory chooser / recent files)
- cursor 詳細 (text/pointer/resize/crosshair の full set) ※**新 M9 Phase 41 で最小実装**
- fullscreen / borderless

### 凍結: 旧 M13 (プラットフォーム第 2 実装)

- baseview 再評価 (rwh 0.6 対応または fork パターン)
- 実 plugin host 検証 (REAPER / Bitwig / CLAP / VST3 wrapper)
- プラグイン UI host integration sample

### 凍結: 旧 M14 (開発体験 + リリース整備)

- debug overlay ※**新 M9 Phase 43 で先行実装**
- widget tree inspector (frame の widget 構造可視化、hit-test 視覚化)
- snapshot test framework (OffscreenRenderer + PNG diff、CI 自動化)
- widget catalog example
- rustdoc 充実 (user guide / tutorial / API doc)
- CI + benchmark regression (Github Actions / criterion baseline)

### 不採用

- AccessKit (M6 Phase 20): 個人 DAW プロジェクトのため対象外、再評価対象外。

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
