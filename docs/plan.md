# Rust 製・モデルを Clone しない DAW 向け GUI ライブラリ — 設計計画 (波形表示 UI を M2 に前倒し)

## Context

- **目的**: Rust で DAW (Digital Audio Workstation) のシェル UI を書くための **GUI ライブラリ** をゼロから設計・実装する。
- **本ファイルの位置付け**: 旧計画 (`F:\dev\work\` 用 `rust-clone-daw-gui-zazzy-kazoo.md`) を `F:\dev\gui_01` 用に移植し、実装済みコード (M1 完了 + M2 実装中) に合わせて更新したもの。
- **このリビジョンの主たる変更**:
  1. **波形表示 UI を M2 に前倒し**。理由: ユーザの判断「波形が一番重いので早期検証したい」。波形が想定通り動くことを早期確定させ、後続の scenegraph / heavy() の前提を固める。
  2. 旧 M2 (Ui<'a> 充実 + 基本ウィジェット) → M3 にスライド。
  3. **プランの正本を `F:\dev\gui_01\docs\plan.md` に置く**。`~/.claude/plans/` のハッシュ命名はディレクトリリネームで紐付けが切れるため、git 管理下に正本を置いて消失を防ぐ。
  4. **DAW アレンジメントビューは波形が大量に並ぶ** (16 トラック × 8 クリップ = 128+ widgets) ため、性能評価は 1 widget の数値だけでなく **N 個 (8 / 64 / 128) のスケール** で見る方針を確定。
- **スコープは GUI のみ**: オーディオは別 exe (別プロセス) として走り、本ライブラリは関与しない。
- **核となる制約 = 「モデルを Clone しない」**:
  1. MIDI クリップ・サンプル・オートメーションなど大きなデータを毎フレーム複製しない
  2. アプリ側がドメイン型を IPC で audio exe に伝送する際の都合 (zero-copy / 部分 snapshot) を library が阻害しない
  3. iced の `Application::Message: Clone` のような型境界をユーザのドメイン型に伝染させない
- **ライブラリの責務**: `&GuiModel` を借りて UI を描画 / 入力からエディット (`Edit<M>`) を収集
- **ライブラリの非責務**: GUI Model の所有・apply・undo/redo / audio exe との IPC・SHM / audio Model の設計

---

## プランの保管場所 (新方針)

| 場所 | 役割 | 同期 |
|---|---|---|
| `F:\dev\gui_01\docs\plan.md` | **正本 (canonical)**, git 管理, リネームで消えない | 編集はこちらが第一 |
| `~/.claude/plans/ui-hashed-stearns.md` | Claude のプランモード作業領域、自動再開用 | 正本からコピーで追従 (手動同期) |

ExitPlanMode 後の最初のアクションで `~/.claude/plans/ui-hashed-stearns.md` の内容を `F:\dev\gui_01\docs\plan.md` にコピーし、git にコミット可能な状態にする。以後のプラン更新は `docs/plan.md` を編集対象とする。

---

## アーキテクチャ要旨 (Hybrid: 即時モード API + 内部 scenegraph + heavy() 脱出口)

DAW UI の二極性 (静的密集型 vs 巨大可変型) をどちらも捌くため:

- 公開 API は **即時モード** に統一 (`derive` マクロ・Lens 不要、`Application::Message: Clone` 伝染なし)
- 内部実装で **scenegraph + input hash** により静的 UI の再描画コストを削減 (M4)
- `ui.heavy(id, |hctx| ...)` 脱出口でピアノロール・タイムライン・大量波形群などは retained-mode 風に最適化 (M5)

```rust
// アプリループ
loop {
    let edits = lib.frame(&gui_model, |m, ui| {
        ui.fader("master", m.master_volume, |v| {
            Edit::mutate(move |m: &mut GuiModel| m.master_volume = v)
        });
        ui.heavy("piano_roll", |hctx| { /* 巨大ビュー */ });
    });
    for e in edits { e.apply(&mut gui_model); }
    app.notify_audio_process(&gui_model);
}
```

**ライフタイム不変条件**: `Ui<'a>` の `'a` は `&GuiModel` の借用と一致。`Edit<M>` は同寿命でフレーム内消費。GAT 不要。

**「Clone しない」をどう守るか**:
- ユーザ Model 型に `Clone` / `PartialEq` / `Hash` / `Default` を要求しない
- メッセージ型を導入しない (Edit は enum + `Box<dyn FnOnce>`)
- 内部 scenegraph の差分検出は **widget ID + プリミティブ末端値の hash** だけで行う
- derive マクロは禁止 (Lens 等)

---

## 基盤クレート選定 (現状確定)

| レイヤ | 採用 | 現状バージョン (M1 時点) |
|---|---|---|
| Window/Event | winit | 0.30.13 |
| Rendering | 自前 wgpu パイプライン | wgpu 29.0.1 |
| Text | glyphon (cosmic-text + swash) | glyphon 0.11.0 |
| Layout | taffy | 0.10.1 |
| Platform handle | raw-window-handle | 0.6.2 |
| Math/binding | bytemuck | 1.25.0 |
| 開発用 | criterion / trybuild | M2 で導入 |
| A11y | AccessKit | M6 |

シェーダは 4 本構成: instanced rect / textured quad / **line strip (波形・メータ・オートメーション)** / SDF glyph (現状は glyphon)。

---

## 現状ワークスペース構成 (F:\dev\gui_01)

```
F:\dev\gui_01\
├── Cargo.toml                       # workspace, edition=2024, rust-version=1.95
├── rust-toolchain.toml
├── docs\                            # ★ M2 開始前に作成、plan.md を置く
│   └── plan.md                      # ★ 本ファイルの正本
├── crates\
│   ├── platform\                    # daw-ui-platform (winit抽象)
│   │   └── src\{lib,event,window,winit_backend}.rs
│   ├── renderer\                    # daw-ui-renderer (wgpu)
│   │   └── src\
│   │       ├── lib.rs / device.rs / scene.rs
│   │       └── pipelines\
│   │           ├── mod.rs
│   │           ├── rect.rs + rect.wgsl   ← 角丸矩形 (実装済)
│   │           └── glyph.rs              ← glyphon 統合 (実装済)
│   ├── ui\                          # daw-ui-core (Ui/Edit/widgets)
│   │   └── src\{lib,edit,id,input,layout,ui}.rs
│   │       └── widgets\{mod,button,label}.rs
│   └── examples\mixer\              # daw-ui-example-mixer (動作確認)
│       └── src\main.rs
```

---

## マイルストーン (改訂版)

### M1 (完了 ✅ — 初期コミット時点)

- ✅ winit でウィンドウ起動 → wgpu surface 初期化 → clear
- ✅ `WindowBackend` trait + 中立 `AppEvent` enum
- ✅ instanced 角丸矩形パイプライン (`rect.wgsl`、4 隅個別半径 + ボーダー + AA)
- ✅ glyphon で日本語/絵文字描画
- ✅ taffy ラッパ (`LayoutPass`, vbox/hbox 基本)
- ✅ `Ui<'a, M>` / `UiHost<M>` / `frame()` API
- ✅ `Edit<M>` enum (`Mutate(Box<dyn FnOnce(&mut M) + Send + 'static>)`)
- ✅ `WidgetId` (FNV-1a 64bit、Model に `Hash` 不要)
- ✅ `PointerFrame` / ヒットテスト (`clicked` / `hovered` / `pressed_inside`)
- ✅ `button` / `label` ウィジェット
- ✅ examples/mixer: 3 ボタン + 1 万矩形ベンチ + 日本語ラベル

### M2 (★★ 波形表示 UI 早期検証 ★★)

**目的**: 波形表示が想定通り動くことを早期に確定する。性能・API・データフロー (LOD ピラミッド) を実コードで検証し、後続マイルストーンの前提を固める。詳細モード (サンプルエディタ) は M5 へ。

**進捗 (2026-05-01)**: **M2 DoD 全項目クリア**。

| 成果物 | 状態 | コミット |
|---|---|---|
| line strip パイプライン (`pipelines/line.rs` + `line.wgsl`) | ✅ | 7ddef3f |
| `Scene::line_batches` + `Renderer::render` 統合 | ✅ | 7ddef3f |
| LOD ピラミッド (完全再構築) | ✅ | 7ddef3f |
| **LOD インクリメンタル拡張** (録音中の `valid_len` 拡大) | ✅ | (本コミット) |
| `Ui::waveform()` (PeakLines + Stack/Overlay/FirstOnly) | ✅ | 7ddef3f |
| `examples/waveform_validation` (16×8 = 128 widgets グリッド) | ✅ | 7ddef3f → 8276ddd |
| **REC シミュレーション** (Space キーで toggle、`valid_len` 増加) | ✅ | (本コミット) |
| criterion ベンチ (`crates/ui/benches/waveform.rs`) | ✅ | 8276ddd |
| **trybuild** (no-Clone 制約の自動検証) | ✅ | 3c251c3 |
| (副次) `widget_state` の downcast バグ修正 + 回帰テスト | ✅ | 7ddef3f |

**実測ベンチ値 (release profile, 5.76M sample × 2ch)**:

| | 1 widget | 8 widgets | 16 widgets | 64 widgets | 128 widgets |
|---|---|---|---|---|---|
| LOD 初回構築 | **4.6 ms** | 37 ms | 74 ms | (未測) | (未測) |
| LOD 再利用 (毎フレーム) | **61 µs** | 491 µs | (未測) | 4.0 ms | **8.0 ms** |
| 60fps 予算占有率 (再利用) | 0.4% | 2.9% | — | 24% | 48% |

線形スケール (61〜63 µs/widget) が N=128 まで維持。これより重い (N=256+) 領域で heavy() (M5) の出番。

**Refactor の影響**: per-channel `Vec<Vec<MinMaxPair>>` 化 (インクリメンタル末尾 push のため) で flat 配列の indirection が増え、初期実装の 44 µs/widget から 61 µs/widget に約 1.4x の regression。代わりにインクリメンタル拡張がフレーム単位で安価になり、録音中もフレーム時間を維持できる。DoD < 100µs は依然クリア。

**主な成果物 (詳細)**:

1. **`pipelines/line.rs` + `line.wgsl` (line strip パイプライン)** ✅
   - `LinePipeline` struct (`new` / `prepare` / `render`)
   - 入力: `Vec<LineBatch>` (各バッチは `Vec<LineSegment>` + line_width + 任意の clip_rect)
   - 1 segment = 1 instance、6 頂点を頂点シェーダで quad に展開
   - フラグメントで 1px AA、batch ごとに scissor (波形 widget の clip rect)
   - `Scene::line_batches` を介して `Renderer::render` から呼ぶ

2. **LOD ピラミッド (生サンプル → min/max 多段ダウンサンプル)** ✅ (完全再構築 + インクリメンタル拡張)
   - **既存の `state: HashMap<WidgetId, Box<dyn WidgetState>>` を再利用**。`WaveformPyramid` は `WidgetState` の blanket impl 経由で乗る (新フィールド追加なし)。
   - 16 倍ずつ decimation するレベル列 (`MinMaxLevel { per_channel: Vec<Vec<MinMaxPair>>, decimation }`)
   - `(generation, valid_len, sample_rate, channels)` の fingerprint をキーに 3 通りに分岐:
     - 完全一致 → 何もしない
     - `valid_len` のみ増加 → `extend_to` でインクリメンタル拡張
     - それ以外 (generation / sample_rate / channels 変化、`valid_len` 縮小) → 完全再構築
   - インクリメンタル拡張は各レベルで「old 末尾 (部分埋まり) ペアを再計算 + 新ペアを末尾追加」を cascading で行い、必要なら新しい coarsest level を追加
   - 単体テストで「多段階 incremental extend == 最終 full rebuild」が pair 単位完全一致することを担保 (`incremental_extension_matches_full_rebuild`)

3. **`Ui::waveform()` プロトタイプ** ✅
   - 公開 API: `WaveformSource` / `WaveformView` / `WaveformStyle` / `WaveformResponse` / `SampleSlices` / `ChannelLayout` / `WaveformRenderMode`
   - 描画モード: **PeakLines のみ** (RMS / SamplePolyline / Auto は M5)
   - チャンネル: Mono / Planar / Interleaved すべて受け取り、Stack / Overlay / FirstOnly でレイアウト
   - クリップ rect での scissor、ヒットテスト (サンプル index 返し)

4. **`examples/waveform_validation/`** ✅
   - 1 分ステレオ (sample rate 48kHz, 5.76M サンプル) を生成
   - **16 トラック × 8 クリップ = 128 widgets の grid** で表示 (アレンジメントビュー想定)
   - drag = 全クリップ同期で横スクロール、wheel = カーソル位置 anchor のズーム
   - **Space キーで REC シミュレーション**: `valid_len` を経過時間に応じて伸ばし、インクリメンタル LOD 拡張をストレステスト
   - **HUD 表示**: フレーム時間 / view_start / view_len / spp / N widgets 数 / valid 秒数 / `● REC` 状態

5. **基盤の小規模整備** ✅
   - `criterion` 0.5 を `crates/ui/Cargo.toml` の `[dev-dependencies]` に導入、`benches/waveform.rs` で N=1/8/16/64/128 を測定
   - `trybuild` 1 を `[dev-dependencies]` に導入、`tests/no_clone_required.rs` ハーネスから `tests/ui/pass/{basic, waveform}.rs` を実行して、API シグネチャに `Clone`/`PartialEq`/`Hash`/`Default` の制約が紛れ込まないことを CI 固定

**M2 完了条件 (Definition of Done) — 全項目クリア**:
- [x] line パイプライン: 多数頂点を低コストで描画 (N=128 widgets × ~2560 segment = 約 33 万頂点で 8.0 ms 内)
- [x] LOD 初回構築: 5.76M サンプルで < 50ms (実測 4.6 ms)
- [x] LOD 再利用: `generation` 一致時の `Ui::waveform()` 呼び出し時間 < 100µs (実測 61 µs、N=128 まで線形)
- [x] 録音シミュレーション: インクリメンタル LOD 拡張で `valid_len` 増加に対応、目視で REC 中もフレーム時間が安定することを確認
- [x] examples/waveform_validation 起動 → スクロール/ズーム滑らか (128 widgets で目視確認済み)
- [x] `Ui::waveform()` API シグネチャに `Clone`/`Hash` 制約が無いことの trybuild 検証 (commit 3c251c3)
- [x] 重大な API 変更が必要な兆候があれば、M3 着手前に設計ドキュメントへ反映 (本ファイル更新で反映)

### M3 (Ui<'a> 充実 + 基本ウィジェット拡張) — 旧 M2 の内容

**Phase 1 進捗 (2026-05-01)** — fader + 入力周りバグ修正:

| 成果物 | 状態 | コミット |
|---|---|---|
| `Ui::fader_at` / `Ui::fader` (垂直スライダ、ドラッグで値編集) | ✅ | e649e5a |
| `FaderState`: `state` HashMap 経由のドラッグ状態保持 | ✅ | e649e5a |
| `examples/mixer` に 3 ch fader 追加 (drag → Edit → apply 確認) | ✅ | e649e5a |
| `trybuild`: `Ui::fader` / `Ui::fader_at` が non-Clone Model でコンパイル | ✅ | e649e5a |
| Button モデルを armed-state (`press_started_inside`) に再設計 | ✅ | e649e5a |
| Windows フォーカス取得クリックで cur_pos が None のまま MouseInput が届く問題 | ✅ 解消 | e649e5a |
| Edit apply 後のラベル staleness 問題 (had_edits → request_redraw) | ✅ 解消 | e649e5a |
| 単体テスト: button click が hover 直後 / press-release 別フレームで両方発火 | ✅ | e649e5a |

**Phase 2 進捗 (2026-05-01)** — knob:

| 成果物 | 状態 | コミット |
|---|---|---|
| `Ui::knob_at` / `Ui::knob` (円形ノブ、7 時 → 12 時 → 5 時の 300° スイープ、上下ドラッグで値編集) | ✅ | 56cfeec |
| `KnobState`: state HashMap 経由のドラッグ状態保持 | ✅ | 56cfeec |
| インジケータは line strip パイプラインで描画 (LineSegment 1 本) | ✅ | 56cfeec |
| `examples/mixer` に 3 ch pan knob を追加 (L/C/R ラベル含む) | ✅ | 56cfeec |
| `trybuild`: `Ui::knob` / `Ui::knob_at` が non-Clone Model でコンパイル | ✅ | 56cfeec |

**Phase 3 進捗 (2026-05-01)** — checkbox:

| 成果物 | 状態 | コミット |
|---|---|---|
| `Ui::checkbox_at` / `Ui::checkbox` (bool toggle、armed-state click) | ✅ | af223ca |
| `CheckboxState`: button と同じ `press_started_inside` モデル | ✅ | af223ca |
| 視覚: 16px 角丸枠 + チェック時の V 字 (line strip) + ラベル | ✅ | af223ca |
| `examples/mixer` に 3 ch mute checkbox を追加 | ✅ | af223ca |
| `trybuild`: `Ui::checkbox` / `Ui::checkbox_at` が non-Clone Model でコンパイル | ✅ | af223ca |

**Phase 4a 進捗 (2026-05-01)** — focus / keyboard 基盤:

| 成果物 | 状態 | コミット |
|---|---|---|
| `InputAccumulator` が `KeyEvent` を蓄積、`take_keyboard_events()` で取り出し | ✅ | 6549cd8 |
| `UiHost::focused: Option<WidgetId>` でキーボードフォーカスを保持、`focused_widget()` で参照 | ✅ | 6549cd8 |
| `Ui::is_focused` / `set_focus` / `clear_focus_if_focused` / `take_keyboard_events_if_focused` | ✅ | 6549cd8 |
| `UiHost::frame` シグネチャに `keyboard: Vec<KeyEvent>` 追加 | ✅ | 6549cd8 |
| クリック発生時に誰も `set_focus` を呼ばなければ blur (= フォーカス可能でない場所のクリック) | ✅ | 6549cd8 |
| 単体テスト 4 本: フォーカス維持 / blur / 再 set_focus / キーは focused だけに届く | ✅ | 6549cd8 |

**Phase 4b 進捗 (2026-05-01)** — text_input (ASCII):

| 成果物 | 状態 | コミット |
|---|---|---|
| `Ui::text_input_at` / `Ui::text_input` (ASCII 編集: char 挿入 / Backspace / 矢印 / Enter / Escape) | ✅ | (本コミット) |
| `TextInputState`: armed-state click + cursor_byte (UTF-8 char 境界対応) | ✅ | (本コミット) |
| 描画: 枠 + テキスト + cursor 縦線 (line strip)。focused 時は枠線色を強調 | ✅ | (本コミット) |
| クリック内側で focus 取得 + cursor 末尾へ。**外側 press で自己 blur** (即時に枠線解除) | ✅ | (本コミット) |
| `Ui::is_focused` を `pending_focus` ベースに変更 (set_focus 同フレームで即時反映) | ✅ | (本コミット) |
| `UiHost::focus_changed_in_last_frame()` getter を追加。アプリは had_edits と同様 request_redraw する | ✅ | (本コミット) |
| `examples/mixer` に title 編集 text_input。OS ウィンドウタイトルも `last_window_title` 差分で追従 | ✅ | (本コミット) |
| `trybuild` / 単体テスト 1 本 (click → focus → typing で text 変化) | ✅ | (本コミット) |

**残作業 (Phase 4c)**:

- IME 統合: `AppEvent::ImePreedit` / `ImeCommit` を focused widget へ流す
- `WindowBackend::set_ime_position` を winit `set_ime_cursor_area` で実装、preedit 表示
- text_input の preedit 描画 (下線 + テンポラリ表示) と committed text の差分 apply
- キーボード focus トラッキング (`UiHost` に focused widget) + `AppEvent::Keyboard` を focused widget へルーティング
- IME 候補位置の `WindowBackend::set_ime_position()` 実装 (winit `set_ime_cursor_area` を使う)
- `LayoutPass` 拡張: padding / gap / fixed size / proportional growth
- `examples/mixer` を 8ch (8 fader + 8 pan knob + 8 mute checkbox) に拡張
- **trybuild 検証拡張**: checkbox / text_input も non-Clone Model でコンパイルすることを固定
- 微調整: fader / knob の感度カーブ、見た目 (背景パネル、影、目盛り)
- **DAW 標準の値編集挙動 (fader / knob 共通)**:
  - **ダブルクリックでデフォルト値に戻る**: `fader_at` / `knob_at` に `default_value: f32` を追加。state に `last_click_time` を持たせ、~300ms × 数 px 以内の 2 回目クリックを検出 → `on_change(default_value)` 発行。
  - **Ctrl + ドラッグで高精度 (感度 1/10)**: `InputAccumulator` を拡張し modifier state (Ctrl/Shift/Alt) を track、`PointerFrame` に modifier フィールドを足す。fader/knob の drag 計算で `if pointer.ctrl { dv *= 0.1 }` で抑制。
  - 将来 text_input の数値編集 (ドラッグで増減) や knob のホイール調整にも同じ modifier 機構が乗る。

### M4 (内部 scenegraph + 差分検出) — 旧 M3

- 内部 scenegraph (`SlotMap<NodeId, SceneNode>`)
- input hash による差分検出 → 静的 UI の draw call 削減を計測
- 1000 ウィジェット級ミキサーで「変化していない部分の draw call が出ない」ことを確認
- glyphon の Buffer キャッシュ統合 (現状は毎フレーム作り直し)
- input hash 衝突テスト: ランダムなウィジェット入力 1M 件で衝突 0
- 波形ウィジェットの LOD ピラミッドは **M2 の構造のまま scenegraph 配下に再配置** (input_hash に generation を組み込む)

### M5 (heavy() + 巨大ビュー + 詳細波形モード)

- `ui.heavy("...", |hctx| ...)` 脱出口
  - `HeavyCtx::cached(viewport_key, draw_fn)`: ViewportKey が前フレームと同じなら GPU バッファ再利用
  - ヒットテストは毎フレーム実施 (キャッシュと独立)
- 波形ウィジェット **詳細モード**: SamplePolyline + サンプル点マーカー
  - 1 サンプル/ピクセル以下にズームしたとき自動切替 (`WaveformRenderMode::Auto`)
  - 円形マーカーは rect パイプラインを SDF circle に拡張 (or 別 instance shader)
  - 編集マーカー (loop start/end / cue points) を `waveform_marker()` 描画支援関数で
- 波形ウィジェット **RMS モード**: ±RMS バー塗りつぶし (rect 流用)
- 波形ウィジェット **任意 N チャンネル / Interleaved 入力** 対応
- examples/sample_editor: 1 サンプルファイルをロード → 全表示・ズーム・選択範囲ハイライト・カーソル
- examples/piano_roll: 100k notes をスクロール 120fps、heavy() の使い方サンプル
- オートメーションカーブ: ベジエ → CPU flatten → line strip パイプライン

### M6 (Phase 2)

- AccessKit 統合
- baseview バックエンド (`WindowBackend` 第 2 実装) — プラグインホスト版への布石
- プラグイン UI window 埋め込み API (raw-window-handle 受け渡し)
- vello サブシステム併用 (SVG アイコン用に必要なら)
- 波形編集オペレーション (trim / fade / split) のサンプル実装

---

## 波形表示 UI 詳細設計 (M2 で実装、M5 で詳細モード追加)

### 1. 公開 API (M2 で確定、後方互換を意識)

```rust
// crates/ui/src/widgets/waveform.rs

impl<'a, M> Ui<'a, M> {
    pub fn waveform<'s>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        source: WaveformSource<'s>,
        view: WaveformView,
        style: WaveformStyle,
    ) -> WaveformResponse;
}

pub struct WaveformSource<'s> {
    /// 生サンプル。フレーム間で同じ slice が来る限り、内部 LOD を再利用。
    pub samples: SampleSlices<'s>,
    /// 有効長 (録音中など samples.len() より小さい場合あり)。
    pub valid_len: usize,
    /// アプリが内容変更時にインクリメントする。一致なら LOD 再利用。
    pub generation: u64,
    pub sample_rate: u32,
}

pub enum SampleSlices<'s> {
    Mono(&'s [f32]),
    /// 各チャンネル個別 (planar)
    Planar(&'s [&'s [f32]]),
    /// インターリーブ (channels 個ずつ並ぶ)  — M5 で対応
    Interleaved { data: &'s [f32], channels: usize },
}

pub struct WaveformView {
    pub start_sample: u64,
    pub len_samples: u64,
    pub vertical_gain: f32,
}

pub struct WaveformStyle {
    pub fg: Color,
    pub fg_clipped: Color,             // |sample| > 1.0 を強調
    pub fill: Option<Color>,            // RMS 塗りつぶし — M5
    pub baseline: Option<Color>,
    pub channel_layout: ChannelLayout,  // Stack / Overlay / FirstOnly
    pub render_mode: WaveformRenderMode,
    pub line_width_px: f32,
}

pub enum WaveformRenderMode {
    /// pixel あたり 1 本の縦線 (min..max)。M2 で対応する唯一のモード。
    PeakLines,
    /// pixel あたり ±RMS バー — M5
    RmsBars,
    /// 1 サンプル/pixel 以下にズームしたとき: 折れ線 + サンプル点マーカー — M5
    SamplePolyline,
    /// samples_per_pixel から自動切替 — M5
    Auto,
}

pub struct WaveformResponse {
    pub hovered: bool,
    pub clicked_at: Option<WaveformHit>,
    pub dragging_at: Option<WaveformHit>,
}

pub struct WaveformHit {
    pub sample_index: u64,
    pub channel: usize,
    pub local_x_px: f32,
    pub local_y_px: f32,
}
```

**「Clone しない」原則の保ち方**:
- `WaveformSource<'s>` は **借用のみ** で構成 (`&[f32]` / `&[&[f32]]`)。`Clone`/`PartialEq` は要求しない。
- 内部 LOD ピラミッドは派生データ (min/max ペア) で、生サンプルのコピーではない。
- `generation: u64` を **唯一の不変キー** として再構築判定。bitwise hash や中身比較は行わない。
- `WaveformView` は **アプリ側がスクロール/ズーム状態を所有**。ライブラリは描画のみ。

### 2. 内部 LOD ピラミッド

実装方針: **既存の `state: HashMap<WidgetId, Box<dyn WidgetState>>` を再利用** する (新しいキャッシュ用フィールドを生やさない)。`WaveformPyramid` は `Any + Send + Sync` なので `WidgetState` の blanket impl が当たり、`Ui::widget_state::<WaveformPyramid>(wid)` で取り出せる。Single Source of Truth に沿う形。

```rust
// 既存
struct UiHost<M> {
    state: HashMap<WidgetId, Box<dyn WidgetState>>,
    // ... 波形ウィジェット用に新フィールドは追加しない
}

// crates/ui/src/widgets/waveform.rs (pub(crate))
struct WaveformPyramid {
    fingerprint: PyramidFingerprint,
    /// levels[0] が最も細かい (decimation = 16)、levels[last] が最も粗い。
    /// 生サンプルはピラミッドに含まない (毎フレーム借用される source.samples を直接走査)。
    levels: Vec<MinMaxLevel>,
}

struct PyramidFingerprint {
    generation: u64,
    valid_len: usize,
    sample_rate: u32,
    channels: u32,
}

struct MinMaxLevel {
    /// channels × pairs_per_channel ペア。チャンネル毎に連続。
    pairs: Vec<MinMaxPair>,
    pairs_per_channel: usize,
    decimation: u32,        // 16^k
}

#[repr(C)]
struct MinMaxPair { min: f32, max: f32 }
```

**注意**: M1 の `widget_state` ヘルパには `Box<dyn WidgetState>` 自身に blanket impl が当たって `as_any_mut` が外側 Box の TypeId を返すバグがあった (M1 では用例が無く潜在化)。M2 で明示 `&mut **entry` deref に修正し、回帰テストを追加 (commit 7ddef3f)。

**ピラミッド構築アルゴリズム**:
- ベース係数 r = 16 (チューニング可)
- レベル 1: 16 サンプルから min/max を 1 ペア → サイズ N/16
- レベル k+1: レベル k の隣接 r ペアの min/max を再計算 → サイズ N/16^(k+1)
- 最大レベル: ペア数が画面横幅の数倍 (例: 4096) を切るまで
- メモリ: 全レベル合計でおよそ N × 0.133 のスペースで済む (r=16, 等比和)

**ピラミッド再構築の判定**:
1. `(generation, valid_len, sample_rate, channels)` のいずれかが変化 → 完全再構築
2. `valid_len` のみ拡大 (録音中の追記) → **インクリメンタル拡張** (新規分のみ計算)
3. それ以外 → そのまま再利用

### 3. 描画レベル選択

ピクセル幅 W、表示サンプル数 N とすると `samples_per_pixel = N / W`。

| samples_per_pixel | 選択 | M2 対応 |
|---|---|---|
| `>= 16` | レベル k = floor(log_16(samples_per_pixel))、ピラミッドキャッシュ | ✅ |
| `1.5 .. 16` | レベル 0 (生サンプル) → ピクセル毎 min/max 走査 | ✅ |
| `< 1.5` | SamplePolyline (折れ線 + マーカー) | M5 (M2 では PeakLines のまま縮退) |

中間レベルの線形補間は行わない (DAW では一貫した min/max 表示が望ましい)。

### 4. line strip パイプラインへの流し込み

`Scene` を拡張 (M2 で実装済み):

```rust
pub struct Scene {
    pub clear_color: wgpu::Color,
    pub rects: Vec<RectCommand>,
    pub glyph_areas: Vec<GlyphArea>,
    pub line_batches: Vec<LineBatch>,    // ★ M2 で追加
}

pub struct LineSegment {
    pub a: [f32; 2],         // 始点 (px)
    pub b: [f32; 2],         // 終点 (px)
    pub color: Color,
}

pub struct LineBatch {
    pub segments: Vec<LineSegment>,
    pub line_width_px: f32,
    pub clip_rect: Option<Rect>,         // batch 単位の scissor (None なら全画面)
}
```

PeakLines モードでは 1 ピクセル = 1 segment (a=(x,y_top), b=(x,y_bottom)) で W 本作る。
GPU 側 (`pipelines/line.rs`) では 1 segment = 1 instance、6 頂点を頂点シェーダで quad に展開する形に統一 (M5 で SamplePolyline モードを足すときも同じパイプラインで segment 列を流す)。

### 5. インタラクションと Edit

- ライブラリは `WaveformResponse` で **位置情報のみ返す**。Edit はアプリ側が組み立てる:
  ```rust
  let resp = ui.waveform("clip0", rect, src, view, style);
  if let Some(hit) = resp.clicked_at {
      ui.push_edit(Edit::mutate(move |m: &mut MyModel| {
          m.set_playhead(hit.sample_index);
      }));
  }
  ```
- 選択範囲・プレイヘッドカーソルはアプリ側 state で持ち、ライブラリには `ui.push_rect()` で重ねる。
- スクロール/ズームのキー入力受信もアプリ側責務。ライブラリは「現在の `WaveformView` を描く」だけ。

### 6. heavy() との関係 (M5)

タイムライン上に多数のクリップ波形が並ぶケースでは `ui.heavy()` で囲み、`ViewportKey { range, zoom, generations }` でフレーム間キャッシュ:

```rust
ui.heavy("track_0_clips", |hctx| {
    let key = ViewportKey {
        range: m.timeline_range(),
        zoom_level: m.zoom_level(),
        clip_generations: m.clip_generations_hash(),
    };
    hctx.cached(key, || {
        for clip in m.clips_in_range() {
            hctx.waveform(clip.id, clip.rect, clip.source(), clip.view(), STYLE);
        }
    });
});
```

**heavy() の中でも LOD ピラミッドキャッシュは引き続き有効** (ピラミッドは `WidgetId` キーのため heavy 境界に影響されない)。

---

## 検証方法

### 共通 (全マイルストーン)
- `cargo build --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo test --workspace`

### M2 (波形 UI 早期検証 — 本マイルストーンの主役)

**性能ベンチ (criterion)**:
- LOD 初回構築: 5.76M サンプル (1 分 × 48kHz × stereo) で < 50ms
- LOD 再利用: `generation` 一致時の `Ui::waveform()` 呼び出し < 100µs
- 録音追記: 1ms 毎に valid_len を 48 サンプル拡大、フレーム時間 16.7ms 安定 1000 フレーム連続
- line strip 単独描画: 10 万頂点を 1 draw call で 60fps

**目視確認 (examples/waveform_validation)**:
- スクロール (左右ドラッグ) が滑らか (60fps)
- ズーム (マウスホイール) で LOD レベルが切り替わる (HUD で確認)
- 録音シミュレーション on にすると右端が伸びていく
- ステレオを Stack 表示・Overlay 表示で切替できる

**回帰防止 (trybuild)**:
- `Ui::waveform()` シグネチャに `Clone`/`Hash` 制約が登場しないことを doc test で監査
- ユーザ Model に `Clone` 等を実装しないコードがコンパイル成功 (M3 でより網羅的に)

**API レビュー**:
- M2 完了時点で `WaveformSource` / `WaveformView` / `WaveformStyle` / `WaveformResponse` の API シグネチャが M5 (詳細モード) でも破壊変更なしに拡張可能か、impl 担当者がチェックリストで確認

### M3 (Ui 充実)
- `cargo bench`: 1 万矩形描画 60fps、IME 入力レイテンシ 1 frame 以下
- examples/mixer 拡張: フェーダドラッグが滑らか、ボタン押下に視覚フィードバック
- trybuild: ユーザ Model に `Clone`/`PartialEq`/`Hash`/`Default` を実装していないコードがコンパイル成功

### M4 (scenegraph)
- input hash 衝突テスト: ランダム widget 入力 1M 件で衝突 0
- 「変化していないフレームで draw call 数が前フレームと同じ」を 1000 フレーム連続で確認
- 波形ウィジェットが scenegraph 統合後も M2 のベンチ値を維持

### M5 (heavy() + 詳細波形 + piano roll)
- examples/sample_editor: 5 分ステレオを全表示 → ズーム → 1 サンプル/ピクセル → 折れ線 + マーカーが見える
- examples/sample_editor: 詳細モードでクリック位置のサンプル index ±1 以内
- examples/piano_roll: 100k notes スクロール 120fps
- 多数クリップ表示 (10 トラック × 各 50 クリップ) を heavy() で 60fps

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

## 重要ファイル一覧

### M2 で新規作成
- `F:\dev\gui_01\docs\plan.md` — ✅ **本ファイル**、git 管理下の正本
- `crates/renderer/src/pipelines/line.rs` — ✅ line strip パイプライン
- `crates/renderer/src/pipelines/line.wgsl` — ✅ 線分 → quad 展開シェーダ
- `crates/ui/src/widgets/waveform.rs` — ✅ `Ui::waveform` + LOD ピラミッド (完全再構築 + インクリメンタル拡張)
- `crates/examples/waveform_validation/` — ✅ 16×8 = 128 widgets グリッドサンプル + REC シミュレーション
- `crates/ui/benches/waveform.rs` — ✅ criterion ベンチ (N=1/8/16/64/128)
- `crates/ui/tests/no_clone_required.rs` — ✅ trybuild ハーネス (no-Clone 制約の自動検証)
- `crates/ui/tests/ui/pass/basic.rs`, `tests/ui/pass/waveform.rs` — ✅ trybuild pass テスト

### M2 で改修
- `crates/renderer/src/scene.rs` — ✅ `Scene::line_batches`, `LineSegment`, `LineBatch` を追加
- `crates/renderer/src/device.rs` — ✅ `Renderer::render` に line パイプライン呼び出し
- `crates/renderer/src/pipelines/mod.rs` — ✅ `pub mod line;` 追加
- `crates/ui/src/lib.rs` — ✅ `widgets::waveform` の公開型を再 export
- `crates/ui/src/ui.rs` — ✅ `Ui::push_lines` 追加、`widget_state` の downcast バグ修正、回帰テスト追加
- `crates/ui/Cargo.toml` — ✅ `criterion` / `trybuild` を `[dev-dependencies]` に追加、`[[bench]]` 設定
- ルート `Cargo.toml` — ✅ `crates/examples/waveform_validation` をメンバー追加

### M2 で再利用した既存資産
- `crates/ui/src/widgets/button.rs` — `pressed_inside`/`hovered`/`clicked` の使い回し
- `crates/ui/src/id.rs` — `WidgetId::child` で per-widget cache key
- `crates/ui/src/input.rs` — `PointerFrame` をドラッグ判定に流用
- `crates/renderer/src/pipelines/rect.rs` — 同 wgsl パターン (uniform / instance vbuf / vertex draw) を line.rs でも踏襲
- `crates/ui/src/widgets/mod.rs` の `WidgetState` blanket impl — `WaveformPyramid` のキャッシュをそのまま乗せる

### M5 で追加予定
- `crates/ui/src/widgets/heavy.rs` — `HeavyCtx`、ViewportKey キャッシュ
- `crates/examples/sample_editor/`、`crates/examples/piano_roll/`
- `crates/ui/src/widgets/waveform.rs` の SamplePolyline / RmsBars / Auto 分岐 + マーカー描画支援

---

## ビルド構成 (確定)

- **Rust Edition: 2024** (`rust-toolchain.toml` で固定済)
- **rust-version: 1.95** (workspace ルートで固定済)
- **依存**: `[workspace.dependencies]` で一元管理、メンバー crate は `<crate>.workspace = true` で参照
- **更新方針**: マイルストーン毎に最新安定版を確認、breaking change があれば追従

---

## 履歴 (最近)

- 2026-04-30: M1 初期コミット (cd969a9) — winit + wgpu + glyphon + taffy で動く GUI ライブラリ骨格。
- 2026-05-01: 設計計画 + CLAUDE.md を git 管理下に追加 (5e14e44)。プラン正本を `docs/plan.md` に確定。
- 2026-05-01: M2 主要実装 (7ddef3f) — line strip パイプライン + `Ui::waveform` + LOD ピラミッド + `examples/waveform_validation` + `widget_state` バグ修正。
- 2026-05-01: M2 性能検証 (8276ddd) — criterion ベンチ追加、example を 128 widgets grid 化。実測で DoD クリア (LOD 初回 4ms, 再利用 44µs, N=128 で 5.86ms)。
- 2026-05-01: ドキュメント整理 (b8e320c) — `docs/plan.md` を実状に合わせて更新、README の参照を最新化。
- 2026-05-01: trybuild 導入 (3c251c3) — no-Clone 制約 (ユーザ Model に Clone/PartialEq/Hash/Default を要求しない) の回帰防止を CI 固定。
- 2026-05-01: **M2 完成** — インクリメンタル LOD 拡張 + REC シミュレーション。`MinMaxLevel` を per-channel `Vec<Vec>` に refactor (末尾 push 効率化)、`extend_to` で cascading boundary recompute + push、Space キー REC で実機検証可能に。indirection 増で 44→61 µs/widget の軽い regression を許容して incremental の利得を取る。M2 DoD 全項目クリア。
- 2026-05-01: **M3 Phase 1** — fader (垂直スライダ、つまみ限定ドラッグ) + button armed-state モデル + Windows focus-click cur_pos workaround + edit 後の追加 redraw パターン確立。実機検証で「ホバー直後の click が反応しない」「フォーカス取得クリックで反応しない」「fader を rect 全域でドラッグ可能」「fader 感度が rect.h 基準で上端に届かない」をユーザ報告から逐次修正。
- 2026-05-01: **M3 Phase 2** — knob (円形ノブ、line strip でインジケータ、上下ドラッグで値編集)。fader と同じドラッグパターン + 既存 rect (4 隅角丸 = 円) と line strip パイプラインの組み合わせで実装、新パイプライン不要。`examples/mixer` で 3 ch pan ノブとして動作確認。
- 2026-05-01: **M3 Phase 3** — checkbox (bool toggle、button と同じ armed-state モデル、line strip で V 字チェックマーク)。`examples/mixer` で 3 ch mute トグルとして動作確認。
- 2026-05-01: **M3 Phase 4a** — keyboard / focus 基盤。`InputAccumulator` が KeyEvent 蓄積、`UiHost::focused` でキーボードフォーカス保持、`Ui::set_focus` / `clear_focus_if_focused` / `is_focused` / `take_keyboard_events_if_focused`。クリックで誰も `set_focus` しなければ blur。`UiHost::frame` シグネチャに `keyboard: Vec<KeyEvent>` 追加 (既存 callers 全部追従)。単体テスト 4 本追加。これで text_input (4b) を載せる土台が揃った。
- 2026-05-01: **M3 Phase 4b** — text_input (ASCII 編集)。char 挿入 / Backspace / Arrow / Enter / Escape をサポート。`is_focused` を `pending_focus` チェックに変更して set_focus が同フレームで即時反映、widget は外側 press で自己 blur することで枠線切替の lag を 0 に。`UiHost::focus_changed_in_last_frame()` getter を追加し、アプリ側は had_edits と同様に request_redraw を呼べる。`examples/mixer` で title 編集 + OS ウィンドウタイトル追従を実装。単体テスト 1 本追加 (click → focus → typing で text 変化)。IME (Phase 4c) は次。
