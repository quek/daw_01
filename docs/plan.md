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

**Phase 8 進捗 (2026-05-01)** — fader / knob 視覚 polish (Ableton 風):

| 成果物 | 状態 | コミット |
|---|---|---|
| fader thumb を 24×12 角丸 + border から **28×10 flat バー** (border 無し、極小角丸) に | ✅ | (本コミット) |
| knob を **Ableton 流デザイン** に redesign: 円本体 + 円周上 300° の可動範囲弧 + 中心→外円のインジケータ線 | ✅ | (本コミット) |
| 可動範囲弧を **2 色** で描画 (回転側 = cyan アクセント、非回転側 = 暗グレー) | ✅ | (本コミット) |
| インジケータ線を中心 (`cx, cy`) → 外円 (`r`) で描画、白色 width 4.0 で視認性確保 | ✅ | (本コミット) |
| 弧の polygon 近似を 5° → 2° step に細分化 (60 → 150 segments)、コーナーアーティファクト解消 | ✅ | (本コミット) |

**TODO (M3 残作業として残す)**:
- **Ableton Live により近づける視覚 polish**: 現状は構造的には近いが、配色・微調整 (border の視認性、非回転側弧と body 色の差、knob 全体の質感) で実機 Ableton の見た目とは差がある。後フェーズで実機並べて細部詰める

**設計判断**:
- **試行錯誤の経緯**: 当初は Phase 8 として「影 / tick marks / sweep arc fill」を入れたが視覚的にゴチャついた → 影 / tick 全削除して「フラット (Ableton 流)」へ方針転換 → fader thumb が薄すぎて掴めない / pan 中央が L100 に見える等 UX 課題 → bipolar (default_value 起点) arc + 外側 rest tick を経由 → 最終的に「中心インジケータ線 + 円周 2 色弧」の Ableton 流に落ち着く。各イテレーションは commit せず内部試行 (本コミットでは最終形のみ)
- **2 色弧の意味**: 回転側 (start → value) = "consumed range" cyan、非回転側 (value → end) = "remaining range" 暗グレー。ユーザが「現在どれくらい振れているか」を可動範囲全体に対する割合で視認できる
- **インジケータを中心起点に**: 単に外円の点を示すだけでなく、knob 全体の rotation を強く感じさせるため (Ableton も同様の構造)
- **fader thumb 28×10**: 当初の 28×4 (Ableton の極小バーを意識) は **掴めない** UX 課題 → 28×10 で掴みやすさと flat 感のバランス
- **Phase 8 の元プランから外したもの**:
  - **感度カーブ** (線形 → 非線形マッピング): UX 設計判断が大きいので別タスクへ。現状は線形のまま
  - **影 / tick marks / sweep arc fill** (元 Phase 8 案): 試行で視覚ゴチャツキを確認、フラット方針へ転換

---

**Phase 7 進捗 (2026-05-01)** — `LayoutPass` ergonomics 改善 (`rect()` + `compute_at()`):

| 成果物 | 状態 | コミット |
|---|---|---|
| `LayoutPass` 内部に `rects: HashMap<NodeId, Rect>` を保持 | ✅ | (本コミット) |
| `compute(root, w, h)` シグネチャ変更 (戻り値削除、内部 HashMap に格納) | ✅ | (本コミット) |
| `compute_at(root, w, h, origin: (f32, f32))` 追加 (origin オフセット適用) | ✅ | (本コミット) |
| `rect(node) -> Rect` 追加 (O(1) 引き、未 compute なら panic) | ✅ | (本コミット) |
| 既存 6 テストを `p.rect(node)` スタイルに更新、`rects()` ヘルパ廃止 | ✅ | (本コミット) |
| 単体テスト 1 本追加: `compute_at_applies_origin_offset` | ✅ | (本コミット) |
| mixer から `HashMap` import + `to_screen` クロージャ削除、`compute_at` + `rect` 利用 | ✅ | (本コミット) |
| 実機検証: 8 ch レイアウトが Phase 6 と同一表示 (regression なし) | ✅ | (本コミット) |

**設計判断**:
- **`compute` の戻り値を捨てて `rect()` にする**: zero external callers + Phase 6 で 1 caller (mixer) のみなので breaking change のコスト極小。HashMap collect の boilerplate を library 内側に閉じ込めるのが KISS
- **未 compute の node に `rect()` を呼ぶとパニック**: silent な default rect (0×0) より早期発見しやすい。`Option` を返す案は呼び出し側の `unwrap` を増やすだけ
- **`compute_at` を別メソッドに**: 大半のユースケースは origin = (0, 0) なので、よく使う方は引数なしで呼べる方が良い (`compute` は薄いラッパで `compute_at(root, w, h, (0.0, 0.0))` を呼ぶ)
- **`origin: (f32, f32)`** で十分。将来 `Rect` 渡しが欲しくなったら追加検討

---

**Phase 6 進捗 (2026-05-01)** — `examples/mixer` を 8 ch に拡張、LayoutPass の最初の実用例:

| 成果物 | 状態 | コミット |
|---|---|---|
| `MixerModel` の `[T; 3]` を `[T; 8]` に拡張 (faders / pans / mutes) + 初期値 | ✅ | (本コミット) |
| `build_ui` のチャンネルストリップを LayoutPass に置換 (row(8) × column(fader/pct/knob/pan/mute)) | ✅ | (本コミット) |
| `Padding::axis(20, 0)` + `Gap::xy(16, 0)` で外周と列間距離、`Gap::all(6)` で列内 widget 間距離 | ✅ | (本コミット) |
| `daw_ui_core` から `FlexDirection` / `NodeId` を re-export (`taffy::prelude` 経由) | ✅ | (本コミット) |
| ストリップヘッダラベルを 1 本に統合: "M3: 8ch チャンネルストリップ — drag / Ctrl+drag (1/10) / dbl-click でリセット" | ✅ | (本コミット) |
| ウィンドウ高さ 600 → 660、`strip_origin_y` 240 → 280 で左側 buttons との視覚衝突を解消 | ✅ | (本コミット) |
| 実機検証: 8 ch 列が等間隔で並ぶ、各 ch で Ctrl+drag / dbl-click が独立動作 | ✅ | (本コミット) |

**LayoutPass を実用例で使ったことで判明した ergonomics 課題** (Phase 7 で対応済み):

- ~~`NodeId → Rect` の HashMap collect が必要~~ → ✅ Phase 7 で `LayoutPass::rect(node) -> Rect` を追加
- ~~`compute` の戻りは原点 (0, 0) 起点。screen 座標に置く際は `to_screen` クロージャで足し算が必要~~ → ✅ Phase 7 で `compute_at(root, w, h, origin)` を追加

---

**Phase 5 進捗 (2026-05-01)** — `LayoutPass` 拡張 (per-side padding / per-axis gap / fixed + flex_grow):

| 成果物 | 状態 | コミット |
|---|---|---|
| 中立 `Padding { top, right, bottom, left }` 型 (`all` / `axis` / `ZERO`) | ✅ | (本コミット) |
| 中立 `Gap { x, y }` 型 (`all` / `xy` / `ZERO`) | ✅ | (本コミット) |
| `LayoutPass::flex` シグネチャを `flex(direction, gap: Gap, padding: Padding, children)` に置換 | ✅ | (本コミット) |
| `LayoutPass::leaf_grow(grow: f32)` で `flex_basis: 0` + `flex_grow` の残余分配 leaf を追加 | ✅ | (本コミット) |
| `flex` 親自身の size を `Dimension::percent(1.0) × percent(1.0)` に変更 (auto だと grow 子が 0px) | ✅ | (本コミット) |
| `Padding` / `Gap` を `crates/ui/src/lib.rs` から re-export | ✅ | (本コミット) |
| 単体テスト 6 本追加: column gap / per-side padding / per-axis gap / 2:1 grow / fixed + grow / padding shrinks grow area | ✅ | (本コミット) |

**設計判断**:
- **`Padding` / `Gap` は中立型**: taffy の `taffy::Rect<LengthPercentage>` は実装詳細で API には露出させない (`WindowBackend` trait 中立化方針と同じ)
- **`flex` の breaking 置換**: callers ゼロなので `flex_with` のような追加 API を作らず KISS に直接置換
- **`leaf_grow` cross-axis は taffy default の stretch**: 大半の column / row flex で十分、`leaf_with(w, h, grow)` のような general API は将来必要時に追加
- **flex 親 size を `percent(1, 1)` に変更**: auto + grow-only 子は 0px に潰れるため、root では利用可能領域全体を埋め、nested では親の content 領域に従う動作にした (ユーザは grow 子だけで構成しても期待通りの面積分配を得られる)
- **`Ui::cursor` / `next_y` は引き続き残す**: convenience method (`Ui::fader` 等) はそのまま。LayoutPass 拡張とは独立

---

**Phase 4d 進捗 (2026-05-01)** — fader / knob のダブルクリック・リセット + Ctrl + ドラッグ高精度モード:

| 成果物 | 状態 | コミット |
|---|---|---|
| 中立 `Modifiers` 型 + `AppEvent::ModifiersChanged` を `crates/platform/src/event.rs` に追加 | ✅ | (本コミット) |
| `winit_backend` で `WindowEvent::ModifiersChanged` を中立 `Modifiers` に変換して emit | ✅ | (本コミット) |
| `PointerFrame.modifiers` + `InputAccumulator.modifiers` で widget まで modifier 状態を配線 | ✅ | (本コミット) |
| `Ui::fader_at` / `fader` / `knob_at` / `knob` に `default_value: f32` を追加 (新シグネチャ) | ✅ | (本コミット) |
| `FaderState` / `KnobState` に `last_click: Option<ClickRecord>` + 新 `DragAnchor { pointer_y, value, ctrl }` | ✅ | (本コミット) |
| ダブルクリック判定: 300ms × 5px 以内の 2 回目 press → `default_value` リセット (drag は始めない) | ✅ | (本コミット) |
| Ctrl + drag: anchor の ctrl 状態に応じて `dv *= 0.1`、mid-drag toggle で再 anchor (jump 防止) | ✅ | (本コミット) |
| `examples/mixer` の呼び出しを新シグネチャに追従 (fader default=0.0 / knob default=0.5) | ✅ | (本コミット) |
| `trybuild` (`tests/ui/pass/basic.rs`) を新シグネチャに追従、no-Clone 制約は維持 | ✅ | (本コミット) |
| 単体テスト 12 本追加 (fader 6 + knob 6): 双方とも double-click / 閾値超過 / 距離超過 / Ctrl 1/10 / mid-drag toggle / triple-click | ✅ | (本コミット) |

**設計判断のポイント**:
- **Mid-drag ctrl toggle 時の再 anchor**: plan.md 旧版の sketch (`if pointer.ctrl { dv *= 0.1 }`) を cumulative-from-anchor の delta にそのまま乗じると、ctrl on/off 瞬間に押下からの全体 delta がスケール変わって値 jump する。anchor を `(現在 py, 現在 value, 現在 ctrl)` に張り直すことで return-to-press-position の cumulative 性質を保ったまま境界で滑らかに切り替わる。
- **Double-click 後は drag に入らない**: Live / Logic / Pro Tools 標準。リセットと drag の意図が混ざるのを防ぐ。state.last_click も同時に消すことで 3 連 click の 3 回目を新しい drag 開始扱いにする。
- **時刻ソース**: widget 内で直接 `Instant::now()` を呼ぶ。300ms 程度の精度で十分、`Ui::frame_time()` 配線は M4 (scenegraph) と一緒にやる方が筋が良い (今は不要)。
- **`KeyEvent` への modifier 追加は今回スコープ外**: text_input の Ctrl+C/V などは別フェーズで対応。

**Phase 4c 進捗 (2026-05-01)** — IME 統合:

| 成果物 | 状態 | コミット |
|---|---|---|
| `AppEvent::ImePreedit` / `ImeCommit` を `InputAccumulator` で蓄積、`ImeEvent` enum で UI 層へ | ✅ | (本コミット) |
| `FrameInput { pointer, keyboard, ime }` で UI 入力を一括化 (`InputAccumulator::take_input()`) | ✅ | (本コミット) |
| `Ui::take_ime_events_if_focused` / `Ui::request_ime` で focused widget が IME 候補位置を要求 | ✅ | (本コミット) |
| `UiHost::ime_request()` getter で App が候補位置を取得 | ✅ | (本コミット) |
| `WindowBackend::set_ime_allowed` / `set_ime_cursor_area` を winit_backend で実装 | ✅ | (本コミット) |
| `winit::WindowEvent::Ime` → `AppEvent::ImePreedit` / `ImeCommit` のマッピング | ✅ | (本コミット) |
| `text_input` が preedit を `state.preedit` に保持 + 描画、commit で working に挿入 | ✅ | (本コミット) |
| `text_input` が cursor 位置を `request_ime` で渡し、IME 候補ウィンドウが追従 | ✅ | (本コミット) |
| mixer App が `ime_request` の Some/None 切替を `set_ime_allowed` 差分で OS に伝達 | ✅ | (本コミット) |
| 単体テスト追加 (ime_events_delivered_only_to_focused / text_input_ime_preedit_then_commit) | ✅ | (本コミット) |

**残作業 (M3 残)**:

- `LayoutPass` 拡張: padding / gap / fixed size / proportional growth: ✅ Phase 5 で完了 (上記表参照)
- `examples/mixer` を 8ch (8 fader + 8 pan knob + 8 mute checkbox) に拡張: ✅ Phase 6 で完了 (上記表参照)
- 微調整: fader / knob の見た目: ✅ Phase 8 で完了 (Ableton 流の 2 色弧 + 中心インジケータ、fader thumb 28×10)
- **fader / knob を Ableton Live により近づける**: Phase 8 で構造的には近づいたが、配色・微調整は引き続き残作業。実機 Ableton と並べて細部詰める
- 微調整 (継続): fader / knob の感度カーブ (線形 → 非線形マッピング)、まだ未着手
- **text_input polish**: cursor / preedit 下線 / IME 候補位置の pixel-perfect 化。
  現状は `HackGen Console NF` 固定幅前提の近似 (ASCII=7 / CJK=14)。`cosmic-text` の
  `Buffer::layout_runs()` で実 measure を行い、preedit を別色 (yellow) で描き直すこと、
  proportional フォントでも正しく動かすこと、を別フェーズで対応する。
  ui crate から renderer の `FontSystem` にアクセスする経路を整える必要がある (Arc 共有 /
  measure trait 公開 / 別 FontSystem を持つ、のいずれか)。
- **DAW 標準の値編集挙動 (fader / knob 共通)**: ✅ Phase 4d で完了 (上記表参照)
- **将来の発展**: text_input の数値編集 (ドラッグで増減) や knob のホイール調整に
  同じ modifier 機構 (`pointer.modifiers.ctrl`) を載せる。`KeyEvent` への modifier
  追加 (Ctrl+C/V など) は別フェーズ。
- **`rtry` のまぜ書き入力対応**: `rtry` (別プロジェクト) のまぜ書き (mazegaki) 入力プロトコル
  に合わせた API を text_input に足す。preedit 区切りや候補編集の細かい制御が必要に
  なる見込み。`rtry` 側の入力プロトコルが固まってから設計。
- **GetText i18n 対応**: label / button / text_input 等の文字列を翻訳するための仕組み。
  `tr!("...")` 風マクロ + catalog 引きの API を別途設計。アプリ側 (例えば mixer) で
  ロケール切替を実演する。実装はサードパーティ crate (`gettext-rs` / `fluent` 等) との
  統合検討から。

### M4 (内部 scenegraph + 差分検出) — 旧 M3

**Phase 10 進捗 (2026-05-01)** — Scenegraph 基盤 + WidgetId 1M 衝突テスト:

| 成果物 | 状態 | コミット |
|---|---|---|
| `crates/ui/src/scenegraph.rs` 新規: `Scenegraph` (`HashMap<WidgetId, SceneNode>` 内蔵) と `SceneNode { input_hash: u64 }` | ✅ | (本コミット) |
| `Scenegraph::{new, unchanged, record, retain, len, is_empty}` API | ✅ | (本コミット) |
| `UiHost` に `scenegraph: Scenegraph` フィールド追加 (Phase 11 で widget から書き込まれる) | ✅ | (本コミット) |
| `lib.rs` から `Scenegraph` / `SceneNode` を re-export | ✅ | (本コミット) |
| Scenegraph 単体テスト 5 本: record / unchanged 一致 / 不一致 / overwrite / retain による eviction | ✅ | (本コミット) |
| WidgetId 1M 衝突テスト (`tests/widget_id_collision.rs`) — 1000 parents × 1000 children = 1M unique IDs | ✅ | (本コミット) |

**設計判断**:
- **`slotmap` 不採用**: 元 plan は `SlotMap<NodeId, SceneNode>` を想定したが、`WidgetId` 自体が安定キー (`Eq + Hash` 既備) で世代管理が不要 → `HashMap<WidgetId, SceneNode>` で十分。外部 deps 追加を回避 (`Cargo.toml` 不変)
- **Phase 10 は型定義のみ**: 各 widget への適用 (Phase 11) を分離することで API レビュー余地を残す。仮に Phase 11 で API が変わっても本コミットの差分は小さい
- **1M テストは sequential seed**: `rand` crate を入れない。FNV-1a の品質確認は uniform-ish な入力でも十分 (悪意ある衝突攻撃は本ライブラリの脅威モデルに含まれない)
- **`SceneNode` は最小**: Phase 11 で `commands: Vec<DrawCommand>` を追加する想定で、今は `input_hash` のみ
- **Eviction API は宣言のみ**: `retain(&seen)` を Phase 10 で定義しておくが、Phase 11 で widget が `seen` を埋めるようになって初めて意味がある

---

**Phase 9 進捗 (2026-05-01)** — glyphon Buffer キャッシュ:

| 成果物 | 状態 | コミット |
|---|---|---|
| `GlyphPipeline` 内に `HashMap<u64, CachedBuffer>` を導入 | ✅ | (本コミット) |
| `(text, font_size, line_height)` を `DefaultHasher` で u64 ハッシュ化 → cache key | ✅ | (本コミット) |
| 同一キーは `Buffer` を再利用、新規キーのみ `Buffer::new` + shaping | ✅ | (本コミット) |
| 5 秒 (300 frame @ 60fps) 未使用 entry の eviction | ✅ | (本コミット) |
| `buffer_key` の単体テスト 4 本: 同一入力 / text 違い / font_size 違い / line_height 違い | ✅ | (本コミット) |
| 実機検証: mixer の表示・操作に regression なし | ✅ | (本コミット) |

**M4 の残作業 (Phase 11-12 で対応)**:
- ~~`slotmap` 導入 + `Scenegraph` 型定義 + 1M FNV-1a 衝突テスト~~ → ✅ Phase 10 で完了 (`slotmap` は不採用、`HashMap<WidgetId, SceneNode>` で代替)
- `Ui::with_widget_node(wid, input_hash, draw_fn)` API + 各 widget に適用 → Phase 11
- "1000 widget で draw call 増えない" bench 検証 → Phase 11
- 波形 LOD ピラミッドを scenegraph 配下に統合 (input_hash に generation 組込) → Phase 12

**設計判断**:
- **Phase 9 を最初に**: `slotmap` 基盤を先に作っても適用前は busywork。glyphon キャッシュは renderer 内部に閉じて measurable な改善を出せるので最初に成果が出る
- **キャッシュキーに 3 要素全部**: text のみだと metrics 切替で古い Buffer が再利用される懸念。`(text, font_size, line_height)` で identity を作るのが安全
- **Eviction は時間ベース (5 秒)**: LRU や size-bound より単純。text_input のランダム入力にも対応
- **borrow パターン**: `glyphon::Buffer` を `Clone` させずに、cache 更新 (mutable borrow) → TextArea 構築 (immutable borrow into cache) → text_renderer.prepare の順で安全に通す
- **Scene / GlyphArea の API は不変**: 既存 callers / widgets / examples は無変更で恩恵を受ける

---

**M4 元仕様 (plan.md 当初)**:

- 内部 scenegraph (`SlotMap<NodeId, SceneNode>`)
- input hash による差分検出 → 静的 UI の draw call 削減を計測
- 1000 ウィジェット級ミキサーで「変化していない部分の draw call が出ない」ことを確認
- glyphon の Buffer キャッシュ統合 (現状は毎フレーム作り直し): ✅ Phase 9 で完了
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
- 2026-05-01: **M3 Phase 4b** — text_input (ASCII 編集)。Ui::text_input_at / text_input、TextInputState (cursor_byte + armed-state click)、char 挿入 / Backspace / 矢印 / Enter / Escape。is_focused を pending_focus ベースに変更して same-frame の click→focus を即時反映、外側 press で widget が自己 blur することで枠線解除も即時。UiHost に focus_changed_in_last_frame() を追加してアプリ側の追加 redraw 用。mixer の title を編集可能化 + OS タイトル追従 (last_window_title 差分)。単体テスト追加。
- 2026-05-01: **M3 Phase 4c** — IME 統合 (基本)。InputAccumulator が ImePreedit / ImeCommit を蓄積、FrameInput で pointer/keyboard/ime をまとめて Ui::frame へ。Ui::take_ime_events_if_focused + Ui::request_ime、UiHost::ime_request()。WindowBackend::set_ime_allowed / set_ime_cursor_area を winit_backend で実装、winit の WindowEvent::Ime → AppEvent::ImePreedit/ImeCommit マッピングも winit_backend 側で完了。text_input が preedit を state に保持して描画 + cursor 位置で IME 候補ウィンドウを要求。mixer App は ime_request の Some/None 切替で OS への set_ime_allowed を差分で呼ぶ。単体テスト 2 本追加 (IME-only-to-focused / preedit-then-commit)。フォントを **HackGen Console NF (固定幅)** に切り替え、全テキスト (prefix + preedit + suffix) を 1 つの GlyphArea で描画して並びを正確化。pixel-perfect な cursor / 下線 / IME 候補位置と preedit 色分け復活は cosmic-text の measure 統合 (別フェーズ) で対応予定。`rtry` のまぜ書き入力対応 / GetText i18n 対応も TODO に明記。これで M3 の入力周り (focus / 通常キー / IME 基本) は機能しており、残りは LayoutPass 拡張・8ch mixer 拡張・上記 polish。
- 2026-05-01: **M3 Phase 4d** — fader / knob のダブルクリック・リセット + Ctrl + ドラッグ高精度モード。中立 `Modifiers` 型 + `AppEvent::ModifiersChanged` を platform crate に追加し、`winit_backend` が `WindowEvent::ModifiersChanged` を変換して emit、`PointerFrame.modifiers` まで配線。`Ui::fader_at` / `fader` / `knob_at` / `knob` に `default_value: f32` 引数を追加、state を `DragAnchor { pointer_y, value, ctrl } + last_click: Option<ClickRecord>` に拡張。300ms × 5px 以内の 2 回目 press で `default_value` リセット、Ctrl+drag で `dv *= 0.1`、mid-drag で Ctrl on/off したときは anchor を再構築して値 jump を防ぐ。例 (mixer) の呼び出しと trybuild の pass を新シグネチャに追従、単体テスト 12 本追加 (fader 6 + knob 6: double-click / 閾値超過 / 距離超過 / Ctrl 1/10 感度 / mid-drag toggle / triple-click)。これで M3 の値編集 UX が DAW として成立するレベルまで揃った。
- 2026-05-01: **M3 Phase 5** — `LayoutPass` 拡張。中立 `Padding { top, right, bottom, left }` / `Gap { x, y }` 型を導入、`LayoutPass::flex` シグネチャを `flex(direction, gap: Gap, padding: Padding, children)` に置換 (zero callers なので breaking OK)、`leaf_grow(grow: f32)` で `flex_basis: 0` + `flex_grow` による残余空間の比例分配をサポート。flex 親の size を `Dimension::auto` から `Dimension::percent(1.0) × percent(1.0)` に変えて、grow-only 子で構成しても親が利用可能領域を埋めるように修正 (auto だと fit-content = 0px に潰れる)。`Padding` / `Gap` を `crates/ui/src/lib.rs` から re-export。単体テスト 6 本追加 (column gap / per-side padding / per-axis gap / 2:1 grow / fixed + grow / padding shrinks grow area)。これで chrome layout の表現力が flex_grow / fixed / per-side padding まで揃った。残る M3 タスクは 8ch mixer 拡張、fader/knob 視覚 polish、text_input pixel-perfect (cosmic-text measure 統合)、i18n。
- 2026-05-01: **M3 Phase 6** — `examples/mixer` を 3 ch から 8 ch に拡張、LayoutPass の最初の実用例として `Padding::axis(20, 0)` + `Gap::xy(16, 0)` で 8 列の row、各列内で `Gap::all(6)` の column flex を組む。`taffy::prelude::{FlexDirection, NodeId}` を `daw_ui_core` から re-export して、利用側が taffy を直接依存に入れずに済むようにした。手動 rect 計算 (`fader_left + i * (fader_w + 16.0)`) が消え、列レイアウトは LayoutPass 1 箇所で済む。ウィンドウ縦サイズ 600 → 660、ストリップ原点 y 240 → 280 で左側 button 群との視覚衝突を解消、ストリップヘッダを Phase 4d の操作 (Ctrl+drag / dbl-click) を含む 1 本にまとめた。`MixerModel` 配列を `[T; 8]` に拡張、初期値はリセットの動作確認しやすいよう各 ch で異なる値に。LayoutPass の利用で見えた ergonomics 課題 (NodeId → Rect の HashMap 引き / origin offset の手動足し算) は別タスクで API 改善するか判断。
- 2026-05-01: **M3 Phase 7** — `LayoutPass` ergonomics 改善。`compute` シグネチャを戻り値なしに変えて内部 `HashMap<NodeId, Rect>` に格納、`rect(node) -> Rect` で O(1) 引きできるようにした。`compute_at(root, w, h, origin)` を追加して screen 座標オフセットを内部で適用、利用側の `to_screen` クロージャと `into_iter().collect::<HashMap<_,_>>()` の boilerplate を library 内側に閉じ込める。breaking change だが zero external callers + Phase 6 で 1 caller (mixer) のみなので影響極小。mixer から `std::collections::HashMap` import が消え、build_ui のチャンネルストリップ部分が約 10 行短くなった。テスト 1 本追加 (`compute_at_applies_origin_offset`) で 33 unit + 1 trybuild、既存 6 layout テストは `rects()` ヘルパ経由から `p.rect(node)` 直呼びに簡素化。実機検証で Phase 6 と同一表示 (regression なし) を確認。
- 2026-05-01: **M3 Phase 8** — fader / knob 視覚 polish。fader thumb を 24×12 角丸+border から 28×10 flat バー (border 無し) に、knob を Ableton 流の「円本体 + 300° 可動範囲弧 (回転側 cyan / 非回転側 暗グレー の 2 色) + 中心→外円インジケータ線 (白、太め)」にデザイン変更。弧の polygon 近似を 2° step に細分化してコーナーアーティファクト解消。試行錯誤の途中 (影/tick/sweep arc fill 案、bipolar default_value 起点 arc 案、外側 rest tick 案) は最終形のみ commit。実機 Ableton との細部詰め (配色、質感) は別タスクとして残す。感度カーブ (非線形マッピング) は別タスク。
- 2026-05-01: **M4 Phase 9** — glyphon Buffer キャッシュ。`GlyphPipeline` 内に `HashMap<u64, CachedBuffer>` を導入し、`(text, font_size, line_height)` の `DefaultHasher` ハッシュをキーとして `Buffer` を再利用。同一 text の繰り返し描画 (mixer の ch label 等) で `Buffer::new` + shaping を回避し、毎フレーム N 回 → 0〜1 回 (新規時のみ) に削減。5 秒 (300 frame) 未使用 entry は eviction。`Scene` / `GlyphArea` の API は不変、widgets / examples は無変更で恩恵を受ける。`buffer_key` の単体テスト 4 本追加 (renderer crate 初の単体テスト群)。M4 の最初のサブ phase、scenegraph 本体は Phase 10 以降で。
- 2026-05-01: **M4 Phase 10** — Scenegraph 基盤 + WidgetId 1M 衝突テスト。`crates/ui/src/scenegraph.rs` に `Scenegraph` (`HashMap<WidgetId, SceneNode>` 内蔵) と `SceneNode { input_hash: u64 }` 型を新設、API は `unchanged` / `record` / `retain` / `len` / `is_empty`。元 plan の `SlotMap<NodeId, SceneNode>` は `WidgetId` が安定キーなので不要と判断、`slotmap` 依存追加を回避 (Cargo.toml 不変)。`UiHost` に `scenegraph: Scenegraph` フィールドを追加 (Phase 10 では宣言のみ、Phase 11 で widget から書き込み)。`tests/widget_id_collision.rs` で 1000 parents × 1000 children = 1M unique IDs の衝突 0 を担保。Scenegraph 単体テスト 5 本 + 衝突テスト 1 本で計 6 本追加。Phase 11 で `Ui::with_widget_node` API + 各 widget 適用に進む。
