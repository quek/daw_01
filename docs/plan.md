# Rust 製・モデルを Clone しない DAW 向け GUI ライブラリ — 設計計画 (M7 以降の計画 / M1-M6 履歴は history.md)

## Context

- **目的**: Rust で DAW (Digital Audio Workstation) のシェル UI を書くための **GUI ライブラリ** をゼロから設計・実装する。GUI のみが本ライブラリの責務、audio / IPC は別プロセス。
- **本ファイルの位置付け**: M7 以降の計画。M1-M6 + M5.5 の完了履歴・詳細設計・検証手順は [history.md](history.md) に分離。**正本 (canonical)** は git 管理下の `F:\dev\gui_01\docs\plan.md` (リネームで紐付けが切れる `~/.claude/plans/` のハッシュ命名は使わない)。
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
    ui.fader("master", m.master_volume, |v| {
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
- メッセージ型を導入しない (Edit は enum + `Box<dyn FnOnce>`)
- 内部 scenegraph の差分検出は **widget ID + プリミティブ末端値の hash** だけで行う
- derive マクロは禁止 (Lens 等)

---

## 基盤クレート選定 (M6 完了時点)

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
| 開発用 (bench / trybuild) | criterion / trybuild | M2 で導入 |
| 第 2 windowing (プラグイン) | baseview | **M13 で再評価** (M6 Phase 19 で rwh 0.5/0.6 互換待ち) |
| SVG / 任意 path 描画 | vello | **M9 で評価** (M6 で M7 送り、現状 rect/glyph/line で全 example 成立) |

A11y (AccessKit) は **本ライブラリでは採用しない方針** (2026-05-02 ユーザ判断、M6 Phase 20 保留 → 不採用)。理由: scope 大、現フェーズで a11y は実害なく後回し可能、本格 OSS 公開時に再評価する余地は残すが本計画には含めない。

シェーダは現状 3 本構成 (M6 完了時点): instanced rect / **line strip (波形・メータ・オートメーション)** / SDF glyph (glyphon 統合)。textured quad は当初 4 本構成として計画したが、M2-M6 で必要にならず未実装。アイコン / 画像描画は M9 で vello (SVG) / icon font / textured quad のいずれかを評価して選定。

---

## 現状ワークスペース構成 (F:\dev\gui_01、M7 完了時点)

```
F:\dev\gui_01\
├── Cargo.toml                       # workspace, edition=2024, rust-version=1.95
├── rust-toolchain.toml
├── docs\
│   ├── plan.md                      # ★ 本ファイル (M8+ 計画)
│   └── history.md                   # M1-M7 + M5.5 履歴 + 詳細設計
├── crates\
│   ├── platform\                    # daw-ui-platform (winit抽象、raw-window-handle 経由 trait bound)
│   │   └── src\{lib,event,window,winit_backend}.rs
│   ├── renderer\                    # daw-ui-renderer (wgpu 29、自前パイプライン rect/line/glyph + OffscreenRenderer)
│   │   └── src\{lib,device,offscreen,scene}.rs + pipelines\{mod,rect,line,glyph}.rs + *.wgsl
│   ├── ui\                          # daw-ui-core (Ui/UiHost/Edit/popup/time/viewport/widgets/scenegraph/heavy)
│   │   └── src\{lib,edit,id,input,layout,popup,scenegraph,time,ui,viewport}.rs
│   │       └── widgets\{mod,button,checkbox,dropdown,fader,heavy,knob,label,level_meter,
│   │                    menu,scroll_area,split_view,tab_view,text_input,time_grid,
│   │                    waveform,automation}.rs
│   └── examples\                    # 9 example (M7 で daw_prototype を追加)
│       ├── mixer\                   # 8ch fader / button / IME
│       ├── waveform_validation\     # 128 widget LOD ストレステスト + ViewportState1D
│       ├── sample_editor\           # 選択範囲 + カーソル + RmsBars + ViewportState1D
│       ├── piano_roll\              # 100k notes + heavy() cached (5.77x)
│       ├── arrangement\             # 500 widgets + heavy() cached (9367x)
│       ├── automation\              # cubic Bezier flatten + Catmull-Rom 点ドラッグ
│       ├── embedded_host\           # OffscreenRenderer で PNG snapshot (プラグイン UI 埋め込み実証)
│       ├── sample_edit_ops\         # 波形 trim / linear fade in/out + ViewportState1D
│       └── daw_prototype\           # M7 visual prototype: menu_bar + tab + split + scroll + ruler + meter
```

---


## マイルストーン (M7 以降)

M1-M6 + M5.5 は完了済み (詳細: [history.md](history.md))。M6 完了時点 (commit 24304b8) の主要状態:

- 全 8 example 動作確認済み (mixer / waveform_validation / sample_editor / piano_roll / arrangement / automation / embedded_host / sample_edit_ops)
- `UiHost::with_window` で edits の自動 apply + redraw、利用者の boilerplate ゼロ
- プラグイン UI 埋め込み API (raw-window-handle 経由) + OffscreenRenderer (PNG snapshot) 完備
- scenegraph + heavy() cached で大規模ビュー (100k notes / 500 widgets / arrangement 9367x キャッシュ高速化) 達成
- 設計の不変条件 (no-Clone / メッセージ型禁止 / derive 禁止 / audio・IPC 不混入) 維持

### M7 (基本 widget 拡張 + DAW 共通 widget、✅ 完了)

DAW プロトタイプとして使える最低限のコア building block + DAW UI で頻出する時間軸 / metering widget を揃えた milestone。詳細は [history.md](history.md) M7 節参照。

| Phase | テーマ | 状態 |
|---|---|---|
| 22 | scrollbar / scroll area | ✅ `Ui::scroll_area<F>` + clip_rect 全 primitive 拡張 + ViewportState1D 先行投入 |
| 23 | menu bar / sub-menu | ✅ `Ui::menu_bar` + `MenuBuilder` (popup_layer 経由) |
| 24 | context menu | ✅ `Ui::context_menu_for(rect, items, on_select)` (library 吸収方式、右クリック自動検出) |
| 25 | popup / dropdown | ✅ `Ui::popup_layer` (deferred buffer + focus stack + outside-click close) + `Ui::dropdown` |
| 26 | tab view + split view | ✅ `Ui::tab_view` (builder pattern) + `Ui::split_view` (drag handle) |
| 27 | time ruler / bar/beat grid | ✅ `TimeMapping` + `Ui::time_ruler` + `Ui::bar_beat_grid` |
| 28 | level meter | ✅ `Ui::level_meter` (Peak / RMS / VU + peak hold + dB log scale) |

**完成 demo**: `daw_prototype` example で全 M7 widget を統合 (`cargo run --bin daw_prototype`)。

---

### M8 (アクション / 入力基盤、✅ 完了)

ユーザ操作を支える基盤機能の集約。undo/redo / shortcut / clipboard / drag&drop / multi-select / file dialog を **1 commit で完遂** (M7 と同方針)。詳細は [history.md](history.md) M8 節参照。

| Phase | テーマ | 状態 |
|---|---|---|
| 29 | history stack (undo / redo) | ✅ `Edit::Undoable { forward, inverse, label }` variant + `Edit::with_inverse(label, fwd, inv)` + `HistoryStack<M>` (default 100 step ring buffer) + `Ui::request_undo / request_redo / can_undo / can_redo / undo_label / redo_label` (no-Clone 維持) |
| 30 | keyboard shortcut + navigation | ✅ `Shortcut::parse("Ctrl+Shift+Z")` + `ShortcutMap::with_default_bindings()` + `Ui::take_shortcut(name)` (Pull 型) + `Ui::focusable(wid, rect)` + `Ui::draw_focus_ring(rect)` + Tab / Shift+Tab 登場順 traversal + arrow nav の 2D 最近傍 |
| 31 | clipboard (cut / copy / paste) | ✅ `ClipboardProvider` trait + `NoopClipboard` + `ArboardClipboard` (feature `clipboard`) + `Ui::take_clipboard_paste / set_clipboard_text` (paste shortcut 検出内蔵、request/response paradigm) |
| 32 | drag & drop (OS file) | ✅ `AppEvent::FileHovered / FileDropped / FileHoverCancelled` + `InputAccumulator` 蓄積 + `Ui::take_file_drop_in_rect / is_file_hovering_in_rect` (drop 直前の cur_pos を合成 position として配信) |
| 33 | multi-select (rect drag) | ✅ `DragRect { start, end, modifiers, finished }` + `Ui::take_drag_rect_in_rect(wid, bounds)` (drag 中は半透明 cyan overlay を library 自動描画) |
| 34 | file dialog (native) | ✅ `FileDialogFilter / DialogResult` + `Ui::request_open_file_dialog / request_open_files_dialog / request_save_file_dialog / take_dialog_result` (rfd 同期実行、UiHost::frame 末尾 block、modal UX、feature `dialog`) |

**設計判断 (M8 全般)**:
- **Edit::Undoable は `Fn` 制約 + variant 拡張**: `Box<dyn FnOnce>` では redo (forward 再実行) 不可のため `Arc<dyn Fn(&mut M) + Send + Sync>` を 2 本持つ。fader/knob drag のような頻度高い更新は既存 `Edit::Mutate` (FnOnce) で history に乗せず、drag 終端でのみ Undoable Edit を発行する DAW 標準動作 (fader/knob の `label: &'static str` 引数追加で既存 example も全更新)
- **Shortcut は Pull 型 (`Ui::take_shortcut(name) -> bool`)**: declarative `Ui::shortcut(spec, fn)` より context-sensitive (focused widget の有無等) を制御しやすい。`set_typing_focus(true)` を text_input が呼ぶことで修飾なし shortcut の抑制経路を確保
- **Clipboard は trait 経由 (`ClipboardProvider`)**: M13 baseview backend 移行時に provider を差し替えるだけで feature gate を引きずらない。winit backend では `ArboardClipboard::new()` を `UiHost::with_clipboard(...)` で渡す。失敗時は no-op に degrade (eprintln でログ)
- **File dialog は同期 (rfd::FileDialog::pick_file)**: DAW 業界標準の modal UX (Logic / Cubase / Bitwig 同等)、winit と rfd の thread 互換性も Windows / macOS で問題なし。Linux GTK/portal で問題が出た場合は非同期版へ降りる retreat path を確保
- **Multi-select の overlay は library が自動描画**: drag 中の半透明 cyan rect (alpha 0.20 + 1px border) を `take_drag_rect_in_rect` 内で `push_rect`、利用者の boilerplate ゼロ
- **Tab traversal は登場順 default**: `Ui::focusable(wid, rect)` の push 順で Tab next / Shift+Tab prev、arrow nav は rect 中心の 2D 距離 (主軸 + 副軸×2) で最近傍。explicit `focus_priority` は M9 送り (KISS)

---

### M9 (描画 / theming + アニメーション + アイコン、未着手)

見た目品質を一気に上げる milestone。M6 で M7 送りにした vello はここで評価。

| Phase | テーマ | 主な成果物 |
|---|---|---|
| 35 | theming システム | `Theme` struct (color palette / font hierarchy / state スタイル)、`Ui` field に持たせ widget から参照 |
| 36 | dark / light theme 切替 | runtime に theme 切替、preference 連動、`Theme::default_dark()` / `default_light()` |
| 37 | アニメーション | state transition (linear / easing / spring)、`AnimatedValue<T>` で補間、frame で update |
| 38 | vello 統合 + SVG アイコン | `Ui::icon(svg_path)` で任意 SVG、解像度独立、wgpu 29 + vello latest 互換性検証 |
| 39 | gradient / shadow / blur | rect に gradient fill、box-shadow 描画、modern UI 表現力 |

**設計判断 (M9 全般)**:
- theming は builder 形式の `Theme` を `Ui` field、widget は `ui.theme.button.fill` のようにアクセス
- アニメーションは widget state に `AnimatedValue<T>` 保持、frame ごとに `update(dt)`
- vello は **feature gate `vello`** で optional (wgpu 29 互換性破綻時に切り離せるよう)
- アイコンは vello / icon font (Lucide / Iconoir / 自前 PUA) のどちらかを M9 で議論決定

---

### M10 (DAW 固有 widget — 信号処理系、未着手)

DAW 固有の音響信号可視化 widget を library に組み込む (time ruler / level meter は M7 に移動済み)。残りは MIDI / FFT / EQ 等の信号処理系。現状 example 内で個別実装している piano roll などを library widget 化。

| Phase | テーマ | 主な成果物 |
|---|---|---|
| 40 | piano keyboard widget | MIDI 入力可視化、scale highlight、note range |
| 41 | MIDI piano roll widget | 現状 example のみ → library widget 化、note add/edit/resize/multi-select |
| 42 | spectrogram | FFT + LOD、time-frequency 表示、sample editor / mastering tool 用 |
| 43 | EQ curve / filter response | filter parameter から curve 描画、Bezier flatten 流用 (M5.5 automation_curve と同基盤) |

---

### M11 (テキスト / 多言語、未着手)

日本人ユーザの IME 体験向上、CJK / emoji フォント、複数行 text editor、i18n 基盤。

| Phase | テーマ | 主な成果物 |
|---|---|---|
| 44 | IME 強化 | preedit cursor 位置精度、proportional font 対応、IME font fallback |
| 45 | font fallback (CJK / emoji) | cosmic-text の fallback chain 設定、emoji color font |
| 46 | 複数行 text editor | `Ui::multiline_text_edit`、line wrap |
| 47 | 文字選択 (drag / triple-click / shift+arrow) | text_input + multiline で共通 |
| 48 | i18n 基盤 | locale 切替、文字列 lookup table、unicode normalization |

---

### M12 (OS 統合、未着手)

window / dialog / cursor の OS 統合整備。multi-window は plugin GUI を別 window で出すケースで重要。

| Phase | テーマ | 主な成果物 |
|---|---|---|
| 49 | multi-window | 複数 window を 1 application で管理、各 window に独立 UiHost |
| 50 | file dialog 強化 | filtered (拡張子)、directory chooser、recent files |
| 51 | cursor 詳細 | text/pointer/resize/crosshair 等、winit のフルセット公開 |
| 52 | fullscreen / borderless | プレゼン / live performance 用 |

---

### M13 (プラットフォーム第 2 実装、未着手)

M6 で保留した baseview (Phase 19) を再開し、実 plugin host 環境で raw-window-handle 受け渡し API (Phase 18 で実装済み) の動作検証を行う。AccessKit (M6 Phase 20) は不採用方針 (上記基盤クレート表参照)。

| Phase | テーマ | 主な成果物 |
|---|---|---|
| 53 | baseview 再評価 | M6 Phase 19 再開、rwh 0.6 対応または fork パターン採用判断 |
| 54 | 実 plugin host 検証 | REAPER / Bitwig 等で raw-window-handle 受け渡し動作確認、CLAP / VST3 wrapper |
| 55 | プラグイン UI host integration sample | baseview backend + Renderer<W> で plugin GUI window を実 host にロード、入力 / IME / cursor 動作確認 |

---

### M14 (開発体験 + リリース整備、未着手)

OSS 公開と将来の DAW 開発者向け tooling。

| Phase | テーマ | 主な成果物 |
|---|---|---|
| 56 | debug overlay | frame ms / widget count / cache hit rate / scenegraph size を画面表示、Ctrl+F1 toggle |
| 57 | widget tree inspector | 現フレームの widget 構造を可視化、hit-test 視覚化 |
| 58 | snapshot test framework | OffscreenRenderer + PNG diff、CI 自動化 |
| 59 | widget catalog example | 全 widget の playground、visual regression test 用 |
| 60 | rustdoc 充実 | user guide、tutorial、API doc、examples 別 docs |
| 61 | CI + benchmark regression | Github Actions、criterion baseline 比較、PR 自動チェック |

---


## 設計上の不変条件

1. ライブラリ提供 API は **ユーザ Model 型に `Clone`/`PartialEq`/`Hash`/`Default` を要求しない**。差分検出は ID + プリミティブ末端値の hash でだけ行う。
2. メッセージ型は導入しない (Edit は enum or `Box<dyn FnOnce>`)。`Application::Message: Clone` 伝染を構造的に防ぐ。
3. `derive` マクロは禁止 (Lens 等)。
4. ライブラリは **audio / IPC / プロセス間通信に一切関知しない**。Edit を返すところで責務を切る。
5. heavy() 以外でも viewport culling は前提 (1000 ウィジェット級は通常パスで耐える)。
6. `Ui<'a>` の `'a` で借用ライフタイムを統一し、GAT を使わない。
7. **波形ウィジェット固有**:
   - `WaveformSource` は借用のみ。`samples: &[f32]` の Clone は禁止。
   - LOD ピラミッドは派生データ (min/max ペア) で、生サンプルのコピーは禁止。
   - 再構築判定は `generation: u64` のみ。中身 hash や bitwise 比較は禁止。
   - 録音中の追記 (`valid_len` 拡大) はインクリメンタル拡張で扱う。

---


## ビルド構成 (確定)

- **Rust Edition: 2024** (`rust-toolchain.toml` で固定済)
- **rust-version: 1.95** (workspace ルートで固定済)
- **依存**: `[workspace.dependencies]` で一元管理、メンバー crate は `<crate>.workspace = true` で参照
- **更新方針**: マイルストーン毎に最新安定版を確認、breaking change があれば追従

---

