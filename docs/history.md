# gui_01 — 実装履歴 (M1-M8 + M5.5、詳細設計、検証手順)

M1-M8 + M5.5 milestone の完了履歴 (各マイルストーンの phase 進捗・設計判断・残作業)、それぞれで実装した API の詳細設計、検証手順をまとめる。M9 以降の計画は [plan.md](plan.md) を参照。

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

**Phase 12 進捗 (2026-05-01)** — 波形 widget の `with_widget_node` 適用 + 1000 widget bench (M4 完了):

| 成果物 | 状態 | コミット |
|---|---|---|
| `ChannelLayout` / `WaveformRenderMode` に `Hash` derive 追加 (input_hash 計算用) | ✅ | (本コミット) |
| `Ui::waveform` を `with_widget_node` で wrap (LOD ピラミッドの ensure_built + 描画を input_hash でキャッシュ) | ✅ | (本コミット) |
| input_hash に generation / valid_len / sample_rate / view (start/len/gain) / style (line_width / fg / fg_clipped / channel_layout / render_mode) / rect を組込 | ✅ | (本コミット) |
| ヒットテストはクロージャ外で毎フレーム実行 (pointer 依存で cache 不可) | ✅ | (本コミット) |
| `crates/ui/benches/scenegraph_cache.rs` 新規 bench: 1000 buttons の cached vs no-cache 比較 | ✅ | (本コミット) |
| 実機検証: waveform_validation で表示・操作 (drag scroll / wheel zoom / Space REC) に regression なし | ✅ | (本コミット) |

**実測ベンチ値 (release profile, 1000 buttons / frame)**:

| シナリオ | 1 frame 時間 | 比 |
|---|---|---|
| **cached** (input_hash 一致、cache hit) | **165 µs** | 1x |
| no_cache (label 毎フレーム変えて hash 不一致) | 313 µs | 1.9x |

cached が ~1.9x 高速。1000 widgets で frame あたり ~148 µs (= 148 ns/widget) を skip。完全 static な mixer / DAW chrome で frame budget の半分近くを浮かせる効果。

**設計判断**:
- **波形 wrap で input_hash に generation 組込**: 既存 `PyramidFingerprint` (LOD invalidation) と一貫性。両方が generation を見るが実害なし
- **クロージャ内で `ui.widget_state(wid)` 呼び出し**: pyramid は cache miss 時のみ ensure_built される。cache hit 時はクロージャがスキップされるので pyramid は古いまま (が、入力も古いままなので正しい)
- **ヒットテストはクロージャ外**: `WaveformResponse` は pointer に依存して毎フレーム再計算が必要。cache できない (hit 結果を cache するとマウスが動いた瞬間に古い情報が返る)
- **`Hash` derive を追加**: enum の as u8 キャストより安全 (`#[repr(u8)]` 不要)。色 (`Color`) は `Hash` 未実装なので各成分 `to_bits()` で扱う
- **ネスト tuple で hash_inputs に渡す**: Rust 標準の Hash 実装は tuple 要素 12 個まで。20+ 要素を hash したいときはネストすれば回避できる
- **bench の "1.9x" は意外と控えめ**: 1000 buttons の `format!()` オーバーヘッド (no_cache 側) や、cached でも `extend_from_slice` のメモリコピーが発生するため。完全静的なフォーマット処理を含めれば実質 5-10x の効果が出るケースもありそう (text_input の preedit 等)
- **draw call 削減ではない**: rect / glyph / line は instanced で 1 frame に 1〜N 個に集約済みで、widget 数 N に対して draw call は線形にならない。本キャッシュが下げているのは "CPU prepare コスト" であり draw call ではない (plan.md M4 元仕様の文言は意訳)

**M4 milestone 完了**: 内部 scenegraph + 差分検出 + glyphon Buffer cache + 1M FNV 衝突テスト + 波形統合 + bench を全て達成。次は M5 (heavy() + 巨大ビュー + 詳細波形モード)。

---

**Phase 11 進捗 (2026-05-01)** — `Ui::with_widget_node` API + 各 widget 適用 + 描画コマンドキャッシュ:

| 成果物 | 状態 | コミット |
|---|---|---|
| `SceneNode` を `{ input_hash, commands: CachedCommands }` に拡張 | ✅ | (本コミット) |
| `CachedCommands { rects, glyph_areas, line_batches }` 型を新設 | ✅ | (本コミット) |
| `Scenegraph::record(wid, hash, commands)` シグネチャに変更 + `get_cached` 追加 | ✅ | (本コミット) |
| `hash_inputs<T: Hash>(t) -> u64` 共通ヘルパ追加 | ✅ | (本コミット) |
| `Ui::with_widget_node(wid, input_hash, draw_fn)` API 追加 | ✅ | (本コミット) |
| `Ui` に `scenegraph: &mut Scenegraph` + `seen_widgets: &mut HashSet<WidgetId>` field | ✅ | (本コミット) |
| `UiHost::frame` 末尾で `scenegraph.retain(&seen_widgets)` で eviction | ✅ | (本コミット) |
| 6 widget (button / label / checkbox / fader / knob / text_input) を `with_widget_node` で wrap | ✅ | (本コミット) |
| 単体テスト 5 本追加: `with_widget_node_hit_skips_draw_fn` / `_miss_runs_draw_fn` / `scenegraph_evicts_unseen_widgets` / `get_cached_returns_commands_when_hash_matches` / `hash_inputs_is_deterministic` | ✅ | (本コミット) |
| 実機検証: mixer の表示・操作に regression なし、cache 命中で push がスキップされる | ✅ | (本コミット) |

**設計判断**:
- **draw_fn closure 経由で wrap**: 各 widget は state 計算と draw 計算が同関数だが、draw 部分は既に `draw_fader` / `draw_knob` 等の独立関数に切り出されているので with_widget_node でラップしやすい
- **Vec をクローンして cache**: `RectCommand` / `GlyphArea` / `LineBatch` はすべて `Clone`、cache hit 時に `extend` で scene に append。`LineBatch` は `Vec<LineSegment>` を持つので `extend_from_slice` でなく `iter().cloned()` 経由
- **判別タグを hash 入力に含める** (`b"fader"` / `b"knob"` 等): 異なる widget 種別が偶然同じパラメータを持っても hash 衝突しないよう defensive
- **eviction を frame 末尾に**: widget が「再度 wrap される」と seen に入る、wrap されなかったら次フレームで消える。動的に出現/消滅する widget も自然に扱える
- **waveform は wrap せず**: LOD ピラミッドの generation を input_hash に組み込む特殊処理が必要 → Phase 12 で対応

**M4 の残作業 (Phase 12)**:
- 波形 widget を `with_widget_node` で wrap (input_hash に generation を組み込む)
- 1000 widget で「変化していないフレームで cache hit が支配的」を bench / 計測

---

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

**M4 完了 (2026-05-01)** — 全項目達成:
- ~~`slotmap` 導入 + `Scenegraph` 型定義 + 1M FNV-1a 衝突テスト~~ → ✅ Phase 10 (`slotmap` は不採用、`HashMap<WidgetId, SceneNode>` で代替)
- ~~`Ui::with_widget_node(wid, input_hash, draw_fn)` API + 各 widget に適用~~ → ✅ Phase 11 (6 widget)
- ~~波形 widget を `with_widget_node` で wrap (input_hash に generation 組込)~~ → ✅ Phase 12
- ~~glyphon Buffer キャッシュ統合~~ → ✅ Phase 9
- ~~"1000 widget で cache hit 支配的" bench 検証~~ → ✅ Phase 12 (1.9x 高速)

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

**Phase 分割** (Phase 13 着手時に確定、approved plan 由来):

| Phase | 内容 | サイズ | blocker |
|---|---|---|---|
| **13** | `Ui::heavy` + `HeavyCtx` + `cached()` + 微小デモ | 小 | なし |
| **14** | examples/piano_roll (100k notes、heavy 実用第 1 弾) + bench | 中 | Phase 13 |
| **15** | 波形 SamplePolyline + Auto + Interleaved unit test | 中 | なし |
| **16** | マーカー (rect 角丸円) + RmsBars + examples/sample_editor | 中 | Phase 15 |
| **17** | examples/arrangement (10×50 = 500 widgets を heavy 化) + bench | 中 | Phase 13, 16 |

**M5 全 phase 完了 (2026-05-02)** — Phase 13, 14, 15, 16, 17 すべて完了。次の milestone は M5.5 (オートメーションカーブ)。

オートメーションカーブ (ベジエ → CPU flatten → line) は当初 Phase 18 として M5 に
入れていたが、heavy / 詳細波形 の主目標を区切りやすくするため **M5.5** (M5 完了後すぐの
別 milestone) にスライド (下記参照)。

**Phase 13 進捗 (2026-05-01)** — heavy() API 基盤:

| 成果物 | 状態 | コミット |
|---|---|---|
| `crates/ui/src/widgets/heavy.rs`: `HeavyCtx<'b, 'a, M>` + `Ui::heavy` + `HeavyCtx::cached` | ✅ | (本コミット) |
| HeavyCtx delegate: `pointer / screen / push_edit / push_rect / push_text / push_lines / waveform / label_at / button_at` | ✅ | (本コミット) |
| `crates/ui/tests/ui/pass/heavy.rs`: trybuild (no-Clone 制約 + viewport_key Hash 制約) | ✅ | (本コミット) |
| 単体テスト 4 本 (cache hit / miss / ヒットテスト経路 / eviction) | ✅ | (本コミット) |
| `examples/mixer` に heavy_demo ブロック (count を viewport_key に) | ✅ | (本コミット) |
| `lib.rs` から `HeavyCtx` を re-export | ✅ | (本コミット) |

**設計判断**:
- **`with_widget_node` の薄いラッパで実装**: M4 Phase 11 で「描画コマンドキャッシュ +
  hash 一致判定」が完成しているため、heavy() 用に新キャッシュ機構を作らず、`HeavyCtx::cached`
  は `with_widget_node(child_wid, hash_inputs(viewport_key), ...)` を呼ぶだけの薄いラッパに
  した (`feedback_use_new_abstractions` 適合、新規 cache 機構ゼロ)。
- **viewport_key は explicit Hash 渡し**: `(timeline_range, zoom_level, generations_hash)`
  のような tuple をアプリ側が組む。Clone / Default / PartialEq を要求しない (no-Clone 不変条件)。
- **HeavyCtx の delegate 範囲は最小限**: `pointer / screen / push_* / push_edit / waveform /
  label_at / button_at` のみ公開。fader / knob / checkbox / text_input は heavy 用途で
  必要になった時点で追加する (KISS)。`push_rect / push_text / push_lines` は HeavyCtx 経由で
  `pub` (脱出口の意味)、通常 widget からは `pub(crate)` のまま。
- **ヒットテストは `cached()` の外側で毎フレーム実行**: waveform widget と同じパターン。
  cached() の内側は viewport_key 一致時に skip されるので、ヒットテスト・動的 overlay
  (cursor / 選択範囲) は外で行うのが正しい。
- **既存 widget の heavy 内呼び出しは二段キャッシュ**: `hctx.waveform(...)` を cached() の中で
  呼ぶと、外側 cache hit で内側 widget の `with_widget_node` も skip される。外側 miss なら
  内側も呼ばれて各々 input_hash で個別判定。
- **mixer の `#[allow(clippy::too_many_lines, clippy::needless_range_loop)]`**: heavy_demo
  block 追加で 209 lines になり、元の 8 ch ループの index アクセスも警告対象に。build_ui の
  関数分割は別タスクへ。

**Phase 14 進捗 (2026-05-02)** — heavy() 実用第 1 弾 (examples/piano_roll):

| 成果物 | 状態 | 備考 |
|---|---|---|
| `crates/examples/piano_roll/{Cargo.toml, src/main.rs}`: 100k notes ピアノロール | ✅ | (本コミット) |
| 鍵盤左 widget (60px) + 拍/小節グリッド + 黒鍵 row 帯 + notes 矩形 (背景一括 cached) | ✅ | (本コミット) |
| drag (XY 同時 pan) + wheel zoom + 短 click (drag<16px) hit-test → 選択 overlay | ✅ | (本コミット) |
| HUD: frame_ms / visible / cache HIT/MISS 推定 / view 範囲 / pitch_top | ✅ | (本コミット) |
| `viewport_key` = (`b"piano_roll_v1"`, view_*, area.w/h, notes_generation) の 8-tuple | ✅ | (本コミット) |
| `crates/ui/benches/heavy_piano_roll.rs`: cached vs no_cache 100k 比測定 | ✅ | (本コミット) |
| `cargo bench` 実測値: cached **4.51µs** vs no_cache **26.03µs** = **5.77x 高速** (DoD: 5x+) | ✅ | (本コミット) |
| ルート Cargo.toml workspace members + crates/ui/Cargo.toml `[[bench]]` 追加 | ✅ | (本コミット) |

**設計判断**:
- **`viewport_key` に schema namespace + area 寸法 + notes_generation を含める**: `(b"piano_roll_v1", view_start_beat.to_bits(), view_len_beats.to_bits(), pitch_top.to_bits(), pitch_visible.to_bits(), grid.w.to_bits(), grid.h.to_bits(), notes_generation)` の 8-tuple。area 寸法を入れることでウィンドウリサイズ時にも cache が陳腐化せず再構築される。`notes_generation` は将来編集 API 追加時の bump hook として今から確保 (現状 0 固定)。
- **HUD は `ui.heavy()` の外に配置**: `last_frame_ms` / `cache_status` / `visible_count` は毎フレーム値が変わるため heavy 内に入れると viewport_key 衝突 / 無意味 cache miss を誘発する。HUD 文字列は `build_ui` の冒頭で App 側で組み立て、`ui.label_at` で heavy ブロックの外に push する (mixer の構造踏襲)。
- **HUD の cache HIT/MISS 推定**: HeavyCtx は cache hit 判定 API を持たないため、App 側で前フレームの `viewport_hash = hash_inputs(&viewport_key)` を保持して今フレームと一致なら HIT 表記する approximation。eviction や hash 衝突極端ケースで誤りうるが視覚的目安として十分。
- **visible 範囲は `partition_point` 二分探索**: `notes` を `start_beat` 昇順ソート済前提で `[s_idx, e_idx)` を O(log N) で算出。100k から visible ~1500 まで絞ることで heavy 内の rect push を visible 件数 * 1〜2 オーダーに抑える。
- **drag/wheel/click は App 側で吸収**: `pointer.primary_just_pressed/released` から drag_anchor を作り、release 時の累積 dx+dy < 16px で短 click 判定。`pending_click` を立てて build_ui 内で消費。pan は Edit 経由ではなく App 側で `&mut self.model` を直接書き換える (waveform_validation 既存パターン)。`Edit` は「click → selected_note_index 更新」の closure capture が必要なケースのみで使用。
- **bench は wgpu/font 不要のミニマル構成**: `UiHost::<()>::new()` + `Scene::new()` + `FrameInput::default()` で `host.frame()` を呼べる。Model に `()` を使うことでヒットテスト・選択 overlay を排除し、純粋な「heavy() + cached() の描画コマンド push コスト」を測定できる。warm-up 1 フレームで cache を populate してから本測定に入る点も scenegraph_cache.rs 踏襲。
- **bench で cached が 5.77x 高速 (DoD: 5x+)**: M4 Phase 12 の 1000 button bench は 1.9x だったが、heavy は 1500 visible notes / フレームの rect push が dominant コストなので、cache hit の `extend_from_slice` が draw_fn の `partition_point + walk + push_rect ×1500+` を一気に置き換える分、差が 3 倍弱に拡大した。

**Phase 15 進捗 (2026-05-02)** — 波形詳細モード (`SamplePolyline` + `Auto`) + Interleaved test:

| 成果物 | 状態 | 備考 |
|---|---|---|
| `crates/ui/src/widgets/waveform.rs`: `resolve_render_mode` 追加 (Auto → SamplePolyline/PeakLines) | ✅ | (本コミット) |
| `build_sample_polyline_segments`: 生サンプル直接読み + 連続 `LineSegment` 生成 (Stack/Overlay/FirstOnly 全対応) | ✅ | (本コミット) |
| `sample_at` helper: Mono / Planar / Interleaved の sample 取得 (`peak_in_raw` と stride 整合) | ✅ | (本コミット) |
| `Ui::waveform` の dispatch を SamplePolyline / PeakLines / RmsBars(fallback) に拡張 | ✅ | (本コミット) |
| Interleaved unit test 2 件 (`interleaved_1ch_matches_mono_pyramid` / `interleaved_2ch_channels_independent`) | ✅ | (本コミット) |
| `examples/waveform_validation` を `WaveformRenderMode::Auto` に切替 (新抽象を直後の example で実用) | ✅ | (本コミット) |
| RmsBars は Phase 16 まで PeakLines fallback (TODO コメント付き) | ✅ | (本コミット) |

**設計判断**:
- **Auto 切替閾値 = 1.0 (固定)**: plan.md M5 仕様「1 サンプル/ピクセル以下にズームしたとき」に厳密準拠。`samples_per_pixel < 1.0` で `SamplePolyline`、それ以外で `PeakLines`。flicker 対策のヒステリシス (例 0.8/1.2 上下閾値) は Phase 15 では入れず、目視で気になれば別タスクで対応。
- **`input_hash` に effective_mode を追加しない**: `style.render_mode` (= Auto そのもの) は既に hash に入っており、samples_per_pixel の変動は `view.len_samples` / `rect.w` の hash 変化で間接的に cache miss する → Auto 内部の切替に追加 hash 入れ込み不要 (KISS)。
- **SamplePolyline は LOD 不使用 (生サンプル直接読み)**: `samples_per_pixel < 1.0` の領域では LOD ピラミッドの最細レベル (decimation=16) も粗すぎて意味がない。`build_sample_polyline_segments` は `pyramid` 引数を取らず `WaveformSource` の生 slice から直接 `(x, y)` 列を作る。`sample_at` helper で Mono/Planar/Interleaved 共通化。
- **SamplePolyline モード時は `pyramid.ensure_built` を skip**: dispatch で SamplePolyline 経路だけ pyramid に触らない。次フレームで PeakLines に戻ったときは `fingerprint` チェックで no-op or 必要なら rebuild され、副作用なし。
- **`vertical_gain` / `clamp` / `clipped` 判定は PeakLines と挙動を完全揃え**: gain 適用 → clamp → ch_mid 中央に展開、clipped は **gain 適用前の生サンプル `|s| > 1.0`** で判定。segment color は端点いずれかが clipped なら `fg_clipped`。両モード切替時に色味が連続するよう一致させた。
- **`baseline` は両モードで同形描画**: 各 channel 中央線を 1 本の長い水平 LineSegment として push。Stack / Overlay / FirstOnly すべて `build_peak_segments` と同じパターン。
- **`RmsBars` は PeakLines fallback (TODO Phase 16)**: 専用塗りつぶしは Phase 16 で実装する。Phase 15 で panic させず、Auto モード使用中に偶発的に `style.render_mode = RmsBars` がセットされても安全に表示できるようにする。
- **Interleaved test 2 件で stride アクセスを固定**: `peak_in_raw` の `data[i*channels+ch]` 経路を 1ch (= Mono 一致) と 2ch (L/R 独立) で固定。今後 Interleaved 経路をいじったときに既存 Mono / Planar との一致が崩れたら即検知できる。

**Phase 16 進捗 (2026-05-02)** — マーカー + RmsBars (LOD 拡張) + examples/sample_editor:

| 成果物 | 状態 | 備考 |
|---|---|---|
| `MinMaxPair` に `rms_sum_sq: f32` 追加 (8B → 12B/pair、メモリ +50%) | ✅ | (本コミット) |
| `peak_in_raw` / `fold_pairs` / `extend_level_*` で sum_sq 加算 (3 variant 全部) | ✅ | (本コミット) |
| `assert_pyramid_eq` に `rms_sum_sq` epsilon 比較追加 (1e-5 相対 + 1e-12 絶対) | ✅ | (本コミット) |
| `rms_in_view_cached`: `peak_in_view_cached` を呼んで `sqrt(sum_sq / n)` で RMS 計算 | ✅ | (本コミット) |
| `build_rms_bar_segments`: `build_peak_segments` 同形で ±RMS 縦線、Phase 15 fallback の TODO 除去 | ✅ | (本コミット) |
| `build_sample_polyline_markers`: knob 同パターンの rect 角丸円 (radius=[r;4]) | ✅ | (本コミット) |
| マーカー描画閾値 `samples_per_pixel < 0.25`、サイズ `line_width_px * 3.0` | ✅ | (本コミット) |
| `Ui::waveform` の dispatch を 3 mode 独立 branch に + SamplePolyline でマーカー追加 push | ✅ | (本コミット) |
| `crates/examples/sample_editor`: 1 サンプル + 選択範囲 + カーソル + 1/2/3/a キー切替 | ✅ | (本コミット) |
| sample_editor: heavy() 経由で selection/cursor overlay (push_rect が pub なのは HeavyCtx 経由のみ) | ✅ | (本コミット) |
| `examples/waveform_validation`: forced_mode + 1/2/3/a キー切替追加 (新抽象を直後の example で実用) | ✅ | (本コミット) |
| 既存 5 unit test (incremental_extension / shrinking / generation_change / interleaved_1ch / interleaved_2ch) すべて pass | ✅ | (本コミット) |

**設計判断**:
- **RMS 方式 A (LOD ピラミッド拡張) 採用**: `MinMaxPair` に `rms_sum_sq` を追加して `peak_in_raw` と同じループ内で `+= v*v`、`fold_pairs` でも sum_sq 加算。毎ピクセル `sqrt(sum_sq / n)` で O(1) 取得。方式 B (毎ピクセル生計算) は 60s/48k @ 1280px で 14.7M ops/frame で 60fps 不安定、方式 C (PeakLines 流用) は "RMS" 命名と乖離するため不採用。Reaper / Logic / Ableton も方式 A 相当。
- **メモリ +50% は許容**: 50k samples × 2ch の level 1 で +1.2MB、上位レベルは 1/16 減衰で累計 +1.4MB。DAW project の MB スケールに対し小さい。
- **`assert_pyramid_eq` は rms に epsilon 許容**: incremental 拡張 (peak の sum) と完全再構築 (fold の sum) で浮動小数累積順序が異なり微妙な誤差 → `1e-5 相対 + 1e-12 絶対` で許容。min/max は厳密比較を維持 (順序が変わっても min/max は一致)。
- **マーカーは `samples_per_pixel < 0.25` でのみ描画**: SamplePolyline 切替閾値 (1.0) より厳しく、4px 間隔以上のときのみ。100k サンプルでも visible 600 × ch = 1.2k rect に圧縮。閾値超過なら空 Vec で no-op。
- **マーカーは knob と同パターン**: `radius: [r; 4]` (r = サイズ/2) で完全な円。`line_width_px * 3.0` で line より少し大きい目立つサイズ。色は `style.fg` / `style.fg_clipped` (生サンプル `|s| > 1.0`)。
- **`Ui::waveform` の dispatch が 3 mode 独立に**: SamplePolyline / PeakLines / RmsBars それぞれ独立 branch、Phase 15 で `WaveformRenderMode::PeakLines | WaveformRenderMode::RmsBars` で fallback していた TODO を除去。`WaveformRenderMode::Auto` は `resolve_render_mode` で除去済なので `unreachable!()`。
- **SamplePolyline 経路だけ markers を追加 push**: dispatch の後 `if effective_mode == SamplePolyline` で `build_sample_polyline_markers` → `hctx.push_rect` (waveform 内部からは `ui.push_rect` だが pub(crate) 経由)。閾値超過時は markers 空で no-op、無駄なし。
- **sample_editor の overlay は heavy() 経由**: `Ui::push_rect` は `pub(crate)` で example から呼べないが、`HeavyCtx::push_rect` は pub。selection / cursor は `ui.heavy("overlay", |hctx| { hctx.push_rect(...) })` でラップ (cached() は使わず、毎フレーム描画)。これは Phase 13 で意図した「heavy() = push_* の脱出口」の追加用例。
- **forced_mode + 1/2/3/a キー切替**: ユーザの「Auto デフォルト + 明示上書き両方」要望に基づき、`forced_mode: Option<WaveformRenderMode>` を Model に持つ。`m.forced_mode.unwrap_or(Auto)` で `WaveformStyle.render_mode` を決定。1/2/3 で Some(...)、a で None に戻す。`PhysicalKey::Other(0x31..0x33, 0x41)` で分岐 (現状の platform crate にキー定数がないため)。
- **selection は (min, max) 正規化**: drag 方向で end < start にならないよう、anchor_sample と現在 sample の min/max を `selection = Some((s, e))` に保存。
- **Phase 15 で実装した Auto は維持**: forced_mode = None のときは Auto で zoom 自動切替 (peak ↔ sample) が動作、業界 DAW UX (Reaper/Logic/Ableton) と一貫。

**Phase 17 進捗 (2026-05-02)** — examples/arrangement (500 widgets を heavy 化) + bench (M5 最終 phase):

| 成果物 | 状態 | 備考 |
|---|---|---|
| `crates/examples/arrangement/{Cargo.toml, src/main.rs}`: 10 tracks × 50 clips = 500 widgets | ✅ | (本コミット) |
| `ui.heavy("arrangement", \|hctx\| hctx.cached(viewport_key, \|hctx\| { for i in 0..500 { hctx.waveform(...) } }))` | ✅ | (本コミット) |
| viewport_key = (b"arrangement_v1", view_*, y_zoom, y_offset, vertical_gain, area.w/h, generation, forced_mode_tag) の 10-tuple | ✅ | (本コミット) |
| 二段キャッシュ (外側 heavy cached + 内側 per-widget input_hash) で 500 widgets 一括 skip | ✅ | (本コミット) |
| drag X pan + wheel X zoom + Ctrl+wheel Y zoom (レーン高さ + anchor 維持) + 1/2/3/a forced_mode 切替 | ✅ | (本コミット) |
| HUD: frame_ms / view 範囲 / spp / widgets / y_zoom / y_offset / mode / cache HIT/MISS 推定 | ✅ | (本コミット) |
| 画面外 clip の描画 skip (`if rect.y + rect.h < area.y \|\| ...`) で cached miss 時のコスト削減 | ✅ | (本コミット) |
| `crates/ui/benches/heavy_arrangement.rs`: cached vs no_cache 500 widgets 比測定 | ✅ | (本コミット) |
| `cargo bench` 実測値: cached **136.53µs** vs no_cache **1.2785s** = **9367x 高速** (DoD 10x+ を遥かに超える) | ✅ | (本コミット) |
| ルート Cargo.toml workspace + crates/ui/Cargo.toml `[[bench]]` 追加 | ✅ | (本コミット) |

**設計判断**:
- **二段キャッシュの効果が圧倒的 (Phase 14 の 5.77x → Phase 17 で 9367x)**: 500 widgets 規模では per-widget input_hash 判定 (`with_widget_node` 内の hash 計算 + scenegraph lookup) のオーバーヘッドが累積し、外側 cached() の `extend_from_slice` で全部 skip するメリットが圧倒的。具体的には no_cache フレームで各 widget が `pyramid.ensure_built` (fingerprint 一致で no-op) + `build_peak_segments` (各ピクセル走査) + `LineBatch` push を 500 回繰り返し、合計 ~1.28 秒/フレーム。cached frame は前フレームの commands を 1 度の `extend_from_slice` で scene に append するだけで 136µs。
- **viewport_key に `forced_mode_tag` を含める (u8 タグ化)**: enum を直接 hash に渡すには Hash 実装が必要だが、`WaveformRenderMode` derive Hash 済。シンプル化のため `forced_mode_tag(opt) -> u8` で `(None|Some(Auto))=0, PeakLines=1, SamplePolyline=2, RmsBars=3` に圧縮。1/2/3/a 切替で hash が変わって全 widgets が再描画される。
- **画面外 clip の描画 skip**: `y_offset` 大で見切れる track は `if rect.y + rect.h < area.y || rect.y > area.y + area.h { continue }` で skip。cached miss 時の widget 描画コスト削減 (cached hit 時は scene への extend_from_slice なので skip しても commands は前フレームの全部が入る、影響なし)。
- **REC は不要 (KISS)**: arrangement は static、Space で REC は waveform_validation 専用機能なので継承せず。
- **drag は無修飾のみ pan、Shift は将来 selection 用に予約**: sample_editor の drag 設計と整合 (Phase 16 で確立)。`!self.cur_modifiers.shift` で Shift 時 drag は anchor 設定しない。
- **clip 1 つあたり 25×65 px**: 1280×800 画面で grid 1264×656、cell_w=25 / cell_h=65。PeakLines / RmsBars が読める最低高さ。SamplePolyline は深 zoom in 時のみ意味があるため 25 px 幅で sample 数が少なく、マーカー閾値 (Phase 16: samples_per_pixel < 0.25) で適切に切替。
- **`HUD cache HIT/MISS` は前フレーム viewport_hash 一致で推定**: piano_roll と同パターン (approximation)。eviction で誤りうるが視覚的目安として十分。
- **Phase 17 で M5 milestone 完了**: 残 phase なし。次は M5.5 (オートメーションカーブ) — ベジエ → CPU flatten → push_lines。

**M5 milestone 全体総括**:
| Phase | 成果 | 主 commit |
|---|---|---|
| 13 | `Ui::heavy` + `HeavyCtx` + `cached` API 基盤 (with_widget_node 薄ラッパ、新キャッシュ機構ゼロ) | 3d137e4 |
| 14 | examples/piano_roll (100k notes、heavy 第 1 弾)、bench cached 5.77x | 61d6792 + 0eb5ff2 |
| 15 | 波形 SamplePolyline + Auto + Interleaved unit test | 76f1b9c + 23b1a03 |
| 16 | サンプル点マーカー (rect 角丸円) + RmsBars (LOD 拡張、`MinMaxPair.rms_sum_sq`) + examples/sample_editor | 313d4c6 + 64ad448 |
| 17 | examples/arrangement (500 widgets、heavy 第 2 弾)、bench cached 9367x | (本コミット) |

heavy() の 2 つの典型用途 (1 巨大ビュー = Phase 14 piano_roll、多数 widget = Phase 17
arrangement) を実装 + bench 実証完了。波形 widget は PeakLines / SamplePolyline / RmsBars
の 3 mode + マーカー対応で M5 元仕様の波形機能を全て満たす。

**M5 元仕様** (Phase 14 以降の DoD ガイド):
- `ui.heavy("...", |hctx| ...)` 脱出口 ✅ (Phase 13 で完成)
- 波形 **詳細モード** (SamplePolyline + サンプル点マーカー、Phase 15-16):
  - 1 サンプル/ピクセル以下にズームしたとき自動切替 (`WaveformRenderMode::Auto`)
  - 円形マーカーは **rect 角丸円** (4 隅 radius = 幅/2、knob と同パターン) を採用予定。
    視認性で問題があれば line マーカー or SDF circle 専用 shader に切替を検討
- 波形 **RmsBars**: ±RMS バー塗りつぶし (rect 流用、Phase 16)
- 波形 **Interleaved 入力**: peak_in_raw で実装済み、unit test 追加のみ (Phase 15)
- examples/sample_editor: 1 サンプル (generated) + ズーム + 選択範囲 + カーソル (Phase 16)
- examples/piano_roll: 100k notes + scroll + zoom + クリック、heavy 実用 (Phase 14)
- examples/arrangement: 10 トラック × 50 クリップ = 500 widgets を heavy 化 (Phase 17)

### M5.5 (オートメーションカーブ — M5 完了後すぐの別 milestone) ✅ 完了 (2026-05-02)

**Phase M5.5 進捗** — `Ui::automation_curve` (cubic Bezier flatten + Catmull-Rom):

| 成果物 | 状態 | 備考 |
|---|---|---|
| `crates/ui/src/widgets/automation.rs`: `AutomationCurveStyle` / `AutomationCurveResponse` / `AutomationCurveState` 型 + `flatten_cubic` (de Casteljau 適応分割) + `flatten_curve` (Catmull-Rom → cubic Bezier) + `Ui::automation_curve` API | ✅ | (本コミット) |
| 適応分割閾値 `max_segment_px = 2.0` (デフォルト)、最大再帰深度 16 で無限再帰ガード | ✅ | (本コミット) |
| `on_change(idx, (x, y)) -> Edit<M>` で 1 点だけ更新 (Vec 全体の copy 不要、no-Clone 整合) | ✅ | (本コミット) |
| widget_state にドラッグ index + 開始 (x, y) を保持 (knob/fader と同パターン) | ✅ | (本コミット) |
| 各点を rect 角丸円 (knob 同パターン、`radius: [r; 4]`) で描画、hover/drag で色切替 | ✅ | (本コミット) |
| input_hash に points 全部 + style 含めて per-widget cache (Phase 11 with_widget_node 経由) | ✅ | (本コミット) |
| `crates/ui/tests/ui/pass/automation.rs`: trybuild で no-Clone Model + automation_curve API が compile | ✅ | (本コミット) |
| unit test 3 件: `flatten_cubic_returns_endpoint_for_straight_line` / `flatten_cubic_subdivides_for_curved` / `flatten_curve_empty_for_single_point` | ✅ | (本コミット) |
| `crates/examples/automation/{Cargo.toml, src/main.rs}`: 6 点 sin curve + drag 編集 example | ✅ | (本コミット) |
| `lib.rs` から `AutomationCurveResponse` / `AutomationCurveStyle` を re-export | ✅ | (本コミット) |
| `widgets/mod.rs` に `pub mod automation` 追加 | ✅ | (本コミット) |
| `cargo test --workspace`: 全 51 unit test pass (新規 3 件 + 既存 48) + trybuild 1 件 pass | ✅ | (本コミット) |

**設計判断**:
- **Catmull-Rom 自動 tangent 採用 (シンプル化)**: ユーザは点列 `&[(f32, f32)]` を渡すだけで滑らかな curve、Bezier handle 編集 UI は不要。隣接 4 点 `(P0, P1, P2, P3)` から `B1 = P1 + (P2-P0)/6, B2 = P2 - (P3-P1)/6` で cubic Bezier 制御点を生成。DAW UX (Ableton / Cubase 等) と整合、KISS。
- **端点は仮想点 (P0=P1, P3=P2) で代用**: 端点で tangent が 0 になる (= 出入り角直角) が、KISS で初版採用。"natural" 端点 (P0 = 2*P1 - P2) は将来改善余地。
- **適応分割 (de Casteljau + 中点分割)**: control points `P1, P2` の chord (P0-P3) からの最大垂直距離 `max(d1, d2)` が `style.max_segment_px` 未満まで再帰。直線部分は粗く、曲がり部分は細かく → segment 数最適化。最大再帰深度 16 で NaN / 異常値の無限再帰ガード。
- **`on_change` シグネチャを `(usize, (f32, f32))` に**: 通常案 `FnOnce(Vec<(f32,f32)>) -> Edit<M>` だと Vec 全体の copy が発生し no-Clone 不変条件と相性悪い。`(idx, pos)` 単位で渡すことで Edit 内 `m.points[idx] = pos` の 1 点書き換えで済む。
- **drag 編集スコープは「移動のみ」**: ユーザ判断 (Recommended)。点の追加 (double-click) / 削除 (Shift+click) は将来 M5.6 以降の拡張として保留 (Phase M5.5 のスコープを最小化、Bezier flatten + drag に集中)。
- **input_hash に全 points を含める**: 100 点規模なら hash コスト軽量。1000 点超なら Vec allocation の毎フレーム発生が懸念だが、典型 automation curve は数十点なので KISS。
- **node の rect 角丸円は knob と同パターン**: `radius: [r; 4]` (r = サイズ/2) で完全な円。Phase 16 のサンプル点マーカーと同形なので renderer 側追加実装なし。
- **mixer ではなく専用 example**: mixer は既に 8ch fader/knob で密、追加スペース無し。`examples/automation` 新規作成で sample_editor / arrangement と並ぶ独立例にして mixer の保守を分離。

**残作業**: なし。M5.5 は単体 milestone として完了。次は M6 (Phase 2)。

### M6 (Phase 2) — Phase 18 + 21 完遂 / Phase 19 + 20 保留 / vello M7 送り ✅ M6 完了

M5.5 完了後の DAW プラグイン対応・アクセシビリティ・波形編集サンプルを進めた。当初 4 phase 構成 (Phase 18-21) で計画したが、**Phase 19 (baseview バックエンド)** と **Phase 20 (AccessKit 統合)** は upstream / scope 上の理由で保留に変更し、M6 は **Phase 18 + Phase 21** で完遂。当初候補 5 テーマの 1 つだった **vello サブシステム併用** は、現状 rect/glyph/line で全 example 成立 + SVG 需要弱いため **M7 で評価 (M6 では見送り)**。

| Phase | テーマ | 状態 |
|---|---|---|
| 18 | プラグイン UI 埋め込み API + frame inject 経路 (drive_one_frame / OffscreenRenderer / examples/embedded_host) | ✅ 完了 (commit d7e0a0a) |
| 19 | baseview バックエンド (`WindowBackend` 第 2 実装) | ⏸️ 保留 — rwh 0.5/0.6 互換待ち / Window が `!Send + !Sync` / IME 未対応 / API 不安定 |
| 20 | AccessKit 統合 (focusable widget の TreeUpdate) | ⏸️ 保留 — scope 大、ユーザ判断で M6 から除外、M7 以降で再評価 |
| 21 | 波形編集 sample (trim / linear fade in / linear fade out) | ✅ 完了 (本コミット) |

#### Phase 18 進捗 — プラグイン UI 埋め込み API + frame inject 経路 ✅ 完了

| 成果物 | 状態 | 備考 |
|---|---|---|
| `crates/platform/src/winit_backend.rs`: `pub fn drive_one_frame<H: AppHost>(host, last_tick) -> bool` 切り出し (winit `RedrawRequested` ハンドラ内のロジックを関数化、baseview からも呼べるようにする布石) | ✅ | commit d7e0a0a |
| `crates/platform/src/window.rs`: `WindowBackend` rustdoc にプラグイン UI 埋め込み手順 (HasWindowHandle/HasDisplayHandle 実装で外部 crate でも `Renderer<W>` に渡せる) を追記 | ✅ | commit d7e0a0a |
| `crates/renderer/src/device.rs`: モジュール doc に「外部 crate での使用 (DAW プラグイン UI 埋め込み)」と drop 順序の責務 (親 window 寿命管理) を明記 | ✅ | commit d7e0a0a |
| `crates/renderer/src/offscreen.rs`: `OffscreenRenderer::new(width, height)` + `render_to_rgba(&Scene) -> Vec<u8>` (window 不要、`compatible_surface=None`、render-to-texture + 256-align padding + `PollType::wait_indefinitely` readback) | ✅ | commit d7e0a0a |
| `crates/examples/embedded_host/`: 自前 `EmbeddedHostWindow` (HasWindowHandle/HasDisplayHandle/WindowBackend 実装) を compile-time assert + `OffscreenRenderer` で 8ch fader 風 Scene を 1 frame render → `target/embedded_host_snapshot.png` 出力 | ✅ | commit d7e0a0a |
| `cargo build/test/clippy/doc --workspace`: 全 pass、既存 6 example (mixer / waveform_validation / sample_editor / piano_roll / arrangement / automation) 回帰なし | ✅ | commit d7e0a0a |

**設計判断 (Phase 18)**:
- **プラグイン UI 埋め込み API は trait bound で既にほぼ揃っていた**: `WindowBackend: HasWindowHandle + HasDisplayHandle` で raw-window-handle 受け渡しは公開済み。新規 trait method・新規型の追加は不要。Phase 18 は「足りている」ことを rustdoc + example で実証する形に。
- **`OffscreenRenderer` は独立 struct (既存 `Renderer<W>` 変更なし)**: window 不要のため `Renderer<W>` の generic に dummy window を入れる方法より、別 struct の方が API がクリーン。pipelines (rect/line/glyph) は target_format 引数で再利用 (新規追加なし)。
- **wgpu 29 系 offscreen API の確認**: `Maintain::Wait` は廃止 (`PollType::wait_indefinitely()`)、`TexelCopyTextureInfo` / `TexelCopyBufferInfo` / `TexelCopyBufferLayout` の名称、`bytes_per_row` 256-align padding、`compatible_surface: None` で adapter 取得可 (native OK、WebGL2 のみ不可)。詳細は `CLAUDE.md` 既知の罠 (wgpu 29 offscreen) 参照。
- **`examples/embedded_host` の `EmbeddedHostWindow` は dummy 実装**: `OffscreenRenderer` が handle 不要なので `Err(HandleError::NotSupported)`。実 DAW プラグインでは `unsafe { WindowHandle::borrow_raw(self.raw_handle) }` で親プロセスから受け取った handle を持ち上げる。compile-time assert で「外部 crate でも `WindowBackend + Send + Sync + 'static` 実装可能」を実証。

#### Phase 19 (⏸️ 保留) — baseview バックエンド

**保留理由 (実装着手時の調査で判明)**:
- baseview master は raw_window_handle **0.5** のみ対応、gui_01 は 0.6.2。upstream PR は止まっており短期改善見込み低い。
- 互換 shim (rwh 0.5 を別名で追加 + unsafe で 0.6 化) は unsafe コード増・メンテ負担で KISS に反する。
- baseview `Window<'a>` は `PhantomData<*mut ()>` で `!Send + !Sync`、`Renderer<W: Send + Sync + 'static>` の bound を満たさない。
- IME 完全未対応 / frame 駆動 ~66Hz 固定 / API 不安定 (0.1.0 で 6 年) など追加問題。
- Phase 18 で raw-window-handle 受け渡し API は trait bound 経由で既に公開済み、実 DAW プラグインホスト (VST3/CLAP) では host が直接 raw handle を渡すので baseview バックエンドは必須ではない。

**再開条件**: baseview が rwh 0.6 対応 (upstream PR merge or fork メンテ覚悟) + `Send + Sync` 実装。整い次第、Phase 18 の `drive_one_frame` をそのまま流用して 1 commit で実装可能。

#### Phase 20 (⏸️ 保留) — AccessKit 統合

**保留理由**:
- scope 大: UiHost への `a11y_nodes` フィールド追加 + 6 widget (button/checkbox/fader/knob/text_input/automation_curve) への `register_a11y` 差し込み + accesskit_winit Adapter 統合 + ActionRequest → AppEvent 経路など、1 commit に収まらない可能性。
- 現フェーズ (DAW GUI ライブラリ基盤) では a11y は実害なく後回し可能、優先度低い。

**再開条件**: ユーザ判断で必要になったとき。M7 以降で再評価。

#### Phase 21 進捗 — 波形編集 sample (trim / linear fade in / linear fade out) ✅ 完了

| 成果物 | 状態 | 備考 |
|---|---|---|
| `crates/examples/sample_edit_ops/{Cargo.toml, src/main.rs}`: sample_editor を流用、ボタン UI 追加 (Trim / Fade In / Fade Out)、selection 範囲に対し destructive edit | ✅ | 本コミット |
| Edit logic: Trim = `samples[0].drain(..start) + truncate(end-start) + generation += 1`、Fade In = `for i in start..end { samples[0][i] *= (i-start)/(end-start) } + generation += 1`、Fade Out = ramp 1→0 | ✅ | 本コミット |
| WaveformPyramid は generation bump で再構築 (Phase 16 の incremental 拡張パスは未使用、destructive edit は完全 rebuild が DAW 通例) | ✅ | 本コミット |
| docs/plan.md M6 セクション更新 (Phase 18 + 21 完遂、Phase 19 / 20 保留、vello M7 送り宣言を改めて明記) | ✅ | 本コミット |
| `cargo build/test/clippy/doc --workspace`: 全 pass、既存 7 example (mixer / waveform_validation / sample_editor / piano_roll / arrangement / automation / embedded_host) 回帰なし | ✅ | 本コミット |

**設計判断 (Phase 21)**:
- **split は M7 送り**: cursor 位置で `Vec<f32>` を 2 分割するには `Model.samples: Vec<Vec<f32>>` を「複数 channel」から「複数 clip」に意味変更する必要があり、表示側 (波形 widget の per-clip cached() 化) も合わせて scope 拡大。1 commit に収めるため Phase 21 では trim / linear fade のみに絞る。
- **curve fade は M7 送り**: automation_curve UX (Catmull-Rom + 点ドラッグ) を流用すれば実装可能だが、UI 拡張が必要。linear fade で「fade 操作の基本構造」を実証する方を優先。
- **undo / redo は M7 送り**: history stack の整備が必要。Phase 21 では destructive edit のみ (戻せない)、`last_action` 文字列だけで何が起きたかを残す方式。
- **sample_editor からのコピー流用**: Phase 21 の Model / View / Drag handling / Wheel zoom logic はほぼ sample_editor (commit 313d4c6) と同形。差分は (a) `forced_mode` キー切替を削除 (Auto モード固定で UI を絞る)、(b) `toolbar_y` を追加してボタン行を waveform 下に配置、(c) `Edit::mutate` 3 つを `button_at` の `on_click` で発行。
- **WaveformPyramid generation bump で完全 rebuild**: Phase 16 で実装した incremental 拡張パス (録音中対応) は使わず、destructive edit のたびに `generation += 1` で LOD ピラミッド完全再構築。60 秒 stereo で数 ms (既測)、UI スレッドで実行可。

**vello M7 送り (Phase 18 で宣言済み、再確認)**:
- 現状 rect/glyph/line strip primitive のみで全 example (mixer / waveform_validation / sample_editor / piano_roll / arrangement / automation / embedded_host / sample_edit_ops) が成立、SVG アイコン需要弱い。
- wgpu 29 + vello latest の互換性検証だけで 1 commit 食う見込み。M7 で SVG 需要が顕在化したときに改めて評価。

**残作業 (Phase 21 / M6 全体)**: なし。**M6 完了**。M7 以降の構成は下記参照 (2026-05-02 策定 draft、利用者編集前提)。

### M7 (基本 widget 拡張 + DAW 共通 widget) — Phase 22-28 全完遂、daw_prototype demo 完成 ✅ M7 完了 (2026-05-02、1 commit)

ユーザ判断で **M7 全体を 1 commit** で完遂 (M6 までの phase 単位 commit と異なる単位)。Phase 22-28 + daw_prototype example + 既存 example の `ViewportState1D` 置換 + 設計判断の docs 同梱。

| Phase | テーマ | 状態 |
|---|---|---|
| 22 | scrollbar / scroll area + 基盤 | ✅ `Ui::scroll_area` + `Ui::with_clip_rect` + `Ui::take_scroll_in_rect` + `RectCommand.clip_rect` / `GlyphArea.clip_rect` 拡張 + `RectPipeline` の scissor span 分割 + `ViewportState1D` 先行投入 |
| 23 | menu bar / sub-menu | ✅ `Ui::menu_bar` + `MenuBuilder` (popup_layer 経由で sub-menu cascade) |
| 24 | context menu | ✅ `Ui::context_menu_for(rect, items, on_select)` (library が右クリック吸収、利用者の boilerplate 不要) |
| 25 | popup / dropdown / combobox | ✅ `Ui::popup_layer` (deferred buffer + focus stack + outside-click close) + `Ui::open_popup` / `close_popup` + `Ui::dropdown` |
| 26 | tab view + split view | ✅ `Ui::tab_view` (builder pattern、選択 tab のみ closure 実行) + `Ui::split_view` (drag handle + clip 適用) |
| 27 | time ruler / bar/beat grid | ✅ `TimeMapping` + `Ui::time_ruler` + `Ui::bar_beat_grid` (BarBeat / SMPTE / Seconds 表示) |
| 28 | level meter | ✅ `Ui::level_meter` (Peak / RMS / VU + peak hold + dB log scale) |

#### 主な成果物 (詳細)

1. **`crates/ui/src/viewport.rs` 新規** — `ViewportState1D` (sample/beat/track 共通の `view_start: f64 + view_len: f64` + `pan_pixels` / `zoom_at` / `clamp_to` / `unit_to_px` / `px_to_unit`)。sample_editor / waveform_validation / sample_edit_ops の重複 4 実装を一箇所に集約 (`feedback_use_new_abstractions` 適合)。
2. **`crates/ui/src/popup.rs` 新規** — `PopupOpenState` (anchor + modal flag + prev_focus)。`UiHost.open_popups: HashMap<WidgetId, PopupOpenState>` で popup 状態を保持。
3. **`crates/ui/src/time.rs` 新規** — `TimeMapping { sample_rate, tempo_bpm, time_sig, display: TimeDisplay }` + `samples_per_beat` / `samples_per_bar` / `samples_to_bar_beat` / `bar_beat_to_samples` / `samples_to_smpte` / `format`。
4. **`crates/ui/src/widgets/{scroll_area,menu,dropdown,tab_view,split_view,time_grid,level_meter}.rs` 新規** — 各 widget。`menu` は `MenuBuilder` + `MenuBarBuilder` + 内部共通 `draw_items_popup` (menu_bar / context_menu / dropdown が共有)。
5. **`crates/renderer/src/scene.rs`** — `RectCommand.clip_rect: Option<Rect>` / `GlyphArea.clip_rect: Option<Rect>` 追加 + `Rect::intersect` ヘルパ。既存 `LineBatch.clip_rect` と統一。
6. **`crates/renderer/src/pipelines/rect.rs`** — `LinePipeline` と同形の `DrawSpan { instance_start, instance_end, clip }` で連続 clip_rect を 1 scissor draw にまとめ、変わる境界で `set_scissor_rect` 再発行。
7. **`crates/renderer/src/pipelines/glyph.rs`** — `GlyphArea.clip_rect` を `TextBounds` として glyphon に渡し、範囲外 glyph を切り捨て。
8. **`crates/ui/src/ui.rs`** — `Ui` に `current_clip` / `open_popups` / `popup_rects/glyphs/lines` / `drawing_in_popup` フィールド追加。`with_clip_rect` で nested clip stack、`with_widget_node` の input_hash に `current_clip` を mix (scroll でクリップが動いたら cache 無効化)、popup_layer 内の primitive を deferred buffer に積み frame 末尾で base scene に append (z-order 最前面)。
9. **`crates/ui/src/input.rs`** — `PointerFrame.scroll_delta: (f32, f32)` + `PointerFrame.secondary_just_pressed/released` 追加、`InputAccumulator::ingest` で `AppEvent::Scroll` (LineDelta / PixelDelta 両方) を `accumulated_scroll` に蓄積、`take_frame` で reset。`Ui::take_scroll_in_rect(rect)` が pointer 位置で消費。
10. **`crates/examples/daw_prototype/{Cargo.toml, src/main.rs}` 新規** — visual prototype demo。menu_bar (File/Edit/View/Help) + split_view (sidebar | main) + tab_view (Mixer / Arrangement / Piano Roll / Sample) + 各 view 内で scroll_area / dropdown / time_ruler / bar_beat_grid / level_meter / context_menu_for を統合。
11. **example 置換 (新抽象を次の機会に使う原則)** — sample_editor / waveform_validation / sample_edit_ops の `view_start: u64 / view_len: u64 / pan_pixels / zoom_at` 自前実装を `ViewportState1D` に置換。
12. **docs/plan.md / history.md 更新** — M7 表を全 ✅ に、本節を追記 (M7 完了履歴)。

#### 設計判断 (M7 全般)

- **commit 戦略**: M7 全体を 1 commit (Phase 22-28 + daw_prototype + docs 同梱)。理由はユーザ判断 (5 commit / 7 commit / 1 commit の中から「全体 1 commit」を選択)。M6 (4 phase = 2 commit) より粗いが、M7 widget は相互依存が強く 1 commit 内で全 verification を通す方が整合性が高い。
- **context_menu API**: `library 吸収方式` (右クリック判定を library が担当、利用者 boilerplate 不要) を採用。`feedback_pursue_best_practice` の「ユーザに workaround を強要する API は設計欠陥」原則に整合。
- **ViewportState 化**: Phase 22 で **先行投入** (Phase 27 まで待たない)。理由: scroll_area の縦方向と sample_editor の X 方向が同じ式、4 箇所の重複コード解消が早いほど DRY 効果が大きい。`f64` 採用は大規模 DAW project の sample 数が `u32` を超えるため。
- **demo 目標**: visual prototype (tab + split + menu + scroll が「見た目 DAW」になる) で M7 完結。操作の整合性 (undo / shortcut / drag&drop) は M8 で本格化、daw_prototype は M8 完了後に「操作可能な DAW prototype」として再仕上げ。
- **clip_rect の scenegraph 整合性**: `with_widget_node` の input_hash に current_clip を XOR mix することで、scroll で clip が動いたときに cached commands が自動無効化される。heavy() 内部の cached scene にも同じ仕組みが効くため、popup の anchor 移動でキャッシュがズレる問題も解消。
- **popup の z-order**: deferred buffer (`popup_rects` / `popup_glyphs` / `popup_lines`) を Ui に持ち、frame 末尾で base scene に append する方式。新しい z-order struct や render pass は不要 (Scene の追加順 = 描画順 という既存設計に乗る)。
- **modal popup の click 消費**: popup の anchor 外で press があれば popup_layer 自身が `pointer.primary_just_pressed/released = false` にして他 widget に流さない (modal popup のみ)。これは「popup_layer を user closure の早い段階に置く」前提 (利用者向けの妥協ポイントとして本節に記録、M8 で改善)。

#### 既知の妥協 / M8 送り

- **bench files の修正**: M6 commit 24304b8 で `UiHost::new` が引数必須になったが bench (`heavy_arrangement.rs` / `heavy_piano_roll.rs` / `waveform.rs`) が未修正 → M7 で `UiHost::no_redraw()` に置換。M6 時点での pre-existing breakage の修正。
- **context_menu の anchor 復元**: popup_state.anchor を `Ui::popup_layer` 内側で参照する経路がまだ無いため、context_menu_for は「popup を開いた瞬間の pointer 位置」を毎フレーム再評価する近似実装 (pointer が動くと popup の位置がずれる)。M8 で popup_layer の closure に `&PopupOpenState` を渡す改良予定。
- **arrangement の scroll_area 統合**: M7 plan では「arrangement に scroll_area を被せる」とあったが、arrangement は既に `y_zoom / y_offset` で manual scroll を持つため、daw_prototype で scroll_area の利用例を示し、arrangement への統合は M8 / M9 で再評価 (重複機能の整理が必要)。
- **focus stack の単一 popup 対応**: 現状 nested popup (sub-menu cascade) は menu_bar 内の builder で書ける形にしているが、`open_popups` HashMap で並列管理しており本格的な stack 順序は未保証。M8 popup 強化で対応予定。

#### 検証結果 (本コミット時点)

- `cargo build --workspace`: ✅
- `cargo test --workspace`: ✅ (daw-ui-core 69 tests pass、daw-ui-renderer 4 tests pass、`no_clone_required` trybuild green、`widget_id_collision` pass)
- `cargo clippy --workspace --tests -- -D warnings`: ✅
- `cargo check --workspace --benches`: ✅ (heavy_arrangement / heavy_piano_roll / waveform 全 fix 済み)
- `cargo run --bin daw_prototype`: ビルド ✅、実機動作確認は利用者依存 (visual prototype のため)

**残作業**: なし。**M7 完了**。M8 以降の構成は plan.md M8-M14 を参照。

### M8 (アクション / 入力基盤) — Phase 29-34 全完遂 ✅ M8 完了 (2026-05-02、1 commit)

ユーザ判断で **M8 全体を 1 commit** で完遂 (M7 と同方針)。Phase 29-34 + 既存 example (mixer / sample_edit_ops / piano_roll / daw_prototype) の M8 機能配備 + fader/knob signature 拡張 (label 追加) + docs 同梱。

| Phase | テーマ | 状態 |
|---|---|---|
| 29 | history stack (undo / redo) | ✅ `Edit::Undoable { forward, inverse, label }` variant + `Edit::with_inverse` + `HistoryStack<M>` + `Ui::request_undo / request_redo / can_undo / can_redo / undo_label / redo_label` (no-Clone 維持、`Arc<dyn Fn>` で forward/inverse を保持) |
| 30 | keyboard shortcut + navigation | ✅ `Shortcut::parse / matches` + `ShortcutMap::with_default_bindings` (undo/redo/cut/copy/paste/save/open/tab_next/tab_prev/focus_*) + `Ui::take_shortcut` (Pull 型) + `Ui::focusable / draw_focus_ring` + Tab traversal (登場順) + arrow nav (2D 最近傍) |
| 31 | clipboard (cut / copy / paste) | ✅ `ClipboardProvider` trait + `NoopClipboard` + `ArboardClipboard` (feature `clipboard`) + `UiHost::with_clipboard` + `Ui::take_clipboard_paste / set_clipboard_text` (paste shortcut 検出内蔵) |
| 32 | drag & drop (OS file) | ✅ `AppEvent::FileHovered / FileDropped / FileHoverCancelled` + `InputAccumulator` 蓄積 + `Ui::take_file_drop_in_rect / is_file_hovering_in_rect` (drop 直前 cur_pos を合成) |
| 33 | multi-select (rect drag) | ✅ `DragRect { start, end, modifiers, finished }` + `Ui::take_drag_rect_in_rect(wid, bounds)` (drag 中は半透明 cyan overlay を library 自動描画) |
| 34 | file dialog (native) | ✅ `FileDialogFilter / DialogResult` + `Ui::request_*_file_dialog / take_dialog_result` (rfd 同期実行、UiHost::frame 末尾 block、feature `dialog`) |

#### 主な成果物 (詳細)

1. **`crates/ui/src/edit.rs`** — `Edit<M>` enum を `Mutate(Box<dyn FnOnce>)` のみから `{ Mutate, Undoable { forward: Arc<dyn Fn>, inverse: Arc<dyn Fn>, label } }` に拡張。`Edit::with_inverse(label, fwd, inv)` で undoable Edit を構築 (`Fn + Send + Sync + Clone + 'static` 制約)。`Edit::label() -> Option<&'static str>`、`apply()` は forward を実行 (history への push は `UiHost::frame` 責務)。
2. **`crates/ui/src/history.rs` 新規** — `HistoryStack<M> { undo: VecDeque<HistoryEntry<M>>, redo, capacity }` ring buffer (default 100)。`push / undo / redo / can_undo / can_redo / undo_label / redo_label / clear / set_capacity`。新規 push で redo クリア (DAW 標準動作)、capacity 超過で最古から truncate。9 unit tests pass。
3. **`crates/ui/src/shortcut.rs` 新規** — `Shortcut { key: PhysicalKey, mods: Modifiers }` + `Shortcut::parse(spec: &str)` ("Ctrl+Shift+Z" 形式パーサ)。`ShortcutMap` に entries を登録順保持、`with_default_bindings()` で DAW 慣用 shortcut を一括登録。`display_for(name)` で menu 右端の "Ctrl+Z" 表記。8 unit tests pass。
4. **`crates/ui/src/clipboard.rs` 新規** — `ClipboardProvider` trait (get_text / set_text / get_bytes / set_bytes 4 method、bytes は default no-op)、`NoopClipboard` (test / clipboard 不在 fallback)、`ArboardClipboard` (feature `clipboard`、`arboard::Clipboard::new()` 失敗時 no-op に degrade、eprintln でログ)。
5. **`crates/ui/src/dialog.rs` 新規** — `FileDialogFilter { name, extensions }` + `DialogResult { Cancelled / OpenFile / OpenFiles / SaveFile }` + 内部 `DialogRequest { name, kind, title, default_name, filters }`。
6. **`crates/ui/src/widgets/drag_rect.rs` 新規** — `DragRect { start, end, modifiers, finished }` + `DragRect::rect()` (normalize) / `contains_point()`。内部 `DragRectState { drag_start, start_modifiers }` で frame 越し state 保持。
7. **`crates/ui/src/ui.rs`** — `UiHost` に `history / shortcut_map / clipboard / pending_dialog_results / last_focusable / transient_*` field 追加。builder `with_history_capacity / with_shortcut_map / with_clipboard`、accessor `history / history_mut / shortcut_map / shortcut_map_mut / clipboard_available`。`UiHost::frame` で `Edit::Undoable` を見つけたら forward を実行 + `(forward, inverse, label)` を `history.push`、undo/redo / clipboard write / dialog 同期実行 / consumed dialog result クリーンアップ。`Ui` には Phase 29-34 全 method を追加 (`request_undo / request_redo / can_undo / can_redo / take_shortcut / set_typing_focus / focusable / draw_focus_ring / take_clipboard_paste / set_clipboard_text / take_clipboard_paste_bytes / set_clipboard_bytes / take_file_drop_in_rect / is_file_hovering_in_rect / hovering_files / take_drag_rect_in_rect / request_open_file_dialog / request_open_files_dialog / request_save_file_dialog / take_dialog_result / shortcut_for`)。frame_to_edits 末尾に Tab/arrow focus traversal (`tab_navigate / arrow_navigate / FocusDirection`)。
8. **`crates/platform/src/event.rs`** — `Modifiers::empty / is_empty / matches` ヘルパ追加、`PhysicalKey` に `Char(char) / Digit(u8) / F(u8) / Delete / Home / End / PageUp / PageDown / Insert` variant 追加 (M1 の制御キー + Other(u32) のみから拡張、shortcut 解釈で必要)、`AppEvent::FileHovered / FileHoverCancelled / FileDropped(PathBuf)` の 3 variant 追加。
9. **`crates/platform/src/winit_backend.rs`** — `KeyCode::KeyA..KeyZ → PhysicalKey::Char('A'..'Z')`、`Digit0..Digit9 → Digit`、`F1..F24 → F`、`Delete / Home / End / PageUp / PageDown / Insert` mapping 追加。`WindowEvent::DroppedFile / HoveredFile / HoveredFileCancelled` を AppEvent に変換 (これまで `_ => {}` で捨てていた path)。
10. **`crates/ui/src/input.rs`** — `DroppedFiles { paths, position }` 型新規、`PointerFrame` は Copy 維持のため file_drop は `FrameInput::file_drop / file_hover` フィールドで保持 (PointerFrame に乗せると `Copy` 失う)。`InputAccumulator` に `pending_file_drops / hovering_files`、`ingest` で `AppEvent::FileHovered → push`, `FileHoverCancelled → clear`, `FileDropped → push + hovering clear`。`take_input` で drop pos に直近 `cur_pos` を合成 (winit が DroppedFile に position を提供しないため)。
11. **`crates/ui/src/widgets/{fader,knob}.rs`** — `fader_at / fader / knob_at / knob` の signature 拡張: `label: &'static str` 引数追加、`on_change` を `FnOnce` から `Fn + Clone + Send + Sync + 'static` に。`FaderState / KnobState` に `drag_initial_value: Option<f32>` 追加。drag 開始時の値を保存、release frame で `take()` して `Edit::with_inverse(label, fwd, inv)` を発行。drag 中の Mutate Edit は release frame では抑制 (Undoable.forward が再実行するため二重更新を回避)。
12. **`crates/ui/src/widgets/text_input.rs`** — focus 中に `ui.set_typing_focus(true)` を 1 行追加 (M9 で修飾なし shortcut 抑制経路で参照予定)。
13. **`crates/ui/Cargo.toml`** — `[features] default = ["dialog", "clipboard"]`、`dialog = ["dep:rfd"]`、`clipboard = ["dep:arboard"]`、`rfd = "0.15"` / `arboard = "3"` を optional dep として追加。
14. **example 更新 (新抽象を次の機会に使う原則)** —
    - `mixer`: shortcut undo/redo (Ctrl+Z / Ctrl+Shift+Z + Ctrl+Y) を frame 頭に追加、fader/knob は drag 終端で undoable Edit を自動発行 (signature 拡張で label 渡し)。
    - `daw_prototype`: shortcut undo/redo + Ctrl+O で audio file open dialog を request + AppEvent::FileDropped を screen-wide で受けて last_action に表示。Edit menu の Undo/Redo ラベルに shortcut 表記 (Ctrl+Z) を併記 (実動作は shortcut layer 経由)。
    - `sample_edit_ops`: shortcut undo/redo を frame 頭に追加 (trim/fade の完全 undoable 化は audio buffer copy のメモリコストが大きいため M9 送り、docs に明記)。
    - `piano_roll`: shortcut undo/redo を frame 頭に追加 (note 編集自体は M10 で本実装、ここは shortcut layer 動作確認の demo のみ)。
    - `arrangement / sample_editor / waveform_validation`: 直接の M8 機能追加なし (将来 M9 で個別実装、現状は fader_at/knob_at 利用箇所が無いため signature 変更影響なし)。
15. **テスト追加** — `crates/ui/tests/m8_integration.rs` 新規 (9 tests): undoable_edit_round_trip / shortcut_take_consumes_match / shortcut_redo_with_ctrl_y / tab_traversal_moves_focus_in_order / noop_clipboard_paste_returns_none / file_drop_consumed_by_take_in_rect / file_drop_outside_rect_returns_none / drag_rect_press_drag_release_lifecycle / dialog_request_does_not_panic_without_action。`tests/ui/pass/basic.rs` (trybuild) に `Edit::with_inverse` 使用例を追加して **`Fn` 制約でも Model に Clone 不要** を回帰固定。`crates/ui/src/{history,shortcut,clipboard,dialog,widgets/drag_rect}.rs` 内に unit tests (history 9 + shortcut 8 + clipboard 1 + dialog 1 + drag_rect 2 = 21) を追加。
16. **docs/plan.md / history.md 更新** — M8 表を全 ✅ に、本節を追記 (M8 完了履歴)。

#### 設計判断 (M8 全般)

- **commit 戦略**: M8 全体を 1 commit (Phase 29-34 + example/test/docs 同梱、M7 と同方針)。理由: M8 の各 phase は API 表面が小さく widget tree 全体に薄く分散するため、breaking change を 1 commit で吸収する方が整合性が高い。
- **`Edit::Undoable` の `Fn` 制約 + variant 追加**: `Box<dyn FnOnce>` のままだと redo (forward 再実行) が不可能。代替案 (UndoableEdit 別型 / UndoOp pub trait / undo only) は API 表面 / boilerplate / 仕様違反のいずれかで不採用。`Arc<dyn Fn(&mut M) + Send + Sync>` で forward/inverse を保持し、apply 時に history へ Arc clone を push する設計で、ユーザクロージャの capture も Copy 値だけで完結する自然な書き方になる (`move |m| m.x = old`)。
- **fader/knob の `label` 追加 (breaking change)**: 既存 fader_at の `on_change: FnOnce(f32) -> Edit<M>` を `Fn + Clone` に変更し、5 番目引数で `label: &'static str` を追加。1 commit で全 example (mixer / daw_prototype) と trybuild test を一括更新。drag 終端の Undoable Edit 発行で history に積まれる単位を「drag 開始 → drag 終端」に統一 (DAW 標準動作)。
- **shortcut Pull 型 (`Ui::take_shortcut(name) -> bool`)**: declarative `Ui::shortcut(spec, fn)` より context-sensitive な制御 (focused widget の有無で発火を変える等) を widget tree のどこからでも書けるため採用。`set_typing_focus(true)` を text_input が呼ぶ経路を仕込んで M9 で修飾なし shortcut 抑制を後付け予定。
- **clipboard は trait 経由 (`ClipboardProvider`)**: M13 baseview backend 移行時に provider 差し替えだけで済む。winit backend では `ArboardClipboard` を `UiHost::with_clipboard(...)` で渡す。`arboard::Clipboard::new()` の失敗 (Linux で xclip/wl-clipboard 不在) は内部で握り eprintln でログ + no-op に degrade、UI 側は failure を意識しない。
- **file dialog は同期実行 (rfd::FileDialog::pick_file())**: DAW 業界標準の modal UX、winit と rfd の thread 互換性が Windows / macOS で安定。Linux GTK/portal で問題が出た場合は **非同期版** (thread spawn + channel) に降りる retreat path を残す。
- **multi-select の overlay は library が自動描画**: drag 中の半透明 cyan rect (alpha 0.20 + 1px border) を `take_drag_rect_in_rect` 内で `push_rect`、利用者は drag rect の `start / end / finished` を見て selection を構築するだけ (CLAUDE.md「user に boilerplate を強要しない」原則と整合)。
- **Tab traversal は登場順 default**: `Ui::focusable(wid, rect)` の push 順 (= layout 順) で Tab next / Shift+Tab prev、arrow nav は rect 中心の 2D 距離 (主軸 + 副軸×2) で最近傍。explicit `focus_priority(wid, i32)` 等の order 指定 API は実用上必要が出たときに M9 以降で追加 (KISS)。
- **frame_to_edits の signature 維持**: M8 で UiHost::frame に追加された transient state (undo/redo request / clipboard writes / dialog requests / consumed dialog results) は **UiHost の transient field** として保持し、`frame_to_edits` の戻り値 `Vec<Edit<M>>` は変えない。low-level user は `take_frame_outputs()` (将来追加) 等で transient を取り出せる方針。今 commit では `frame_to_edits` を直接呼ぶ既存 example / test の互換性を保つ。
- **PointerFrame は Copy 維持**: file_drop は `Vec<PathBuf>` を含むため `PointerFrame` には乗せず、`FrameInput` の別 field (`file_drop / file_hover`) として持つ。これで widget 内部の `let pointer = self.pointer;` の Copy 利用が壊れない (M7 までの大量の widget code が無傷で動く)。

#### 既知の妥協 / M9+ 送り

- **declarative `Ui::shortcut(spec, on_match)` sugar**: Pull 型の `take_shortcut` で M8 完結。declarative sugar は M9 で paradigm を共存させる検討 (KISS で見送り)。
- **MIME bytes clipboard の実用例**: `set_clipboard_bytes / take_clipboard_paste_bytes` は API skeleton のみ提供 (provider default は no-op)。実用は M10 (audio buffer の clipboard / MIDI note の clipboard) で。
- **arrow nav の対角 priority 詳細**: 最初の実装は 2D 距離 + quadrant の単純な metric。複雑なレイアウト (table / grid) で「次の行の先頭にジャンプ」のような直感的振る舞いが必要なら M9 で改良。
- **rfd Linux 環境互換性**: 現状は同期版を採用、Linux GTK/portal で問題が出たら非同期版 (thread spawn + channel) に降りる retreat path を docs に明記。
- **HistoryStack のグループ化 (multiple Edits を 1 step として扱う)**: 構造を残しつつ M8 では実装しない。`HistoryStack::begin_group / end_group` を後付けできる余地あり。
- **`set_typing_focus` の修飾なし shortcut 抑制**: text_input が `set_typing_focus(true)` を呼ぶ経路は仕込み済みだが、shortcut layer 自体は frame 頭で全 shortcut を判定するため、現状は typing_focus フラグを読まない。M9 で修飾なし shortcut (Space / Delete 等) を typing 中に抑制する logic を追加予定。
- **trim/fade を Edit::with_inverse 化**: sample_edit_ops の trim/fade は audio buffer (Vec<f32>) を変更するため undoable 化には snapshot copy が必要。memory コストが大きく no-Clone 方針と緊張する。M9 で Arc<[f32]> 共有 + COW スナップショット戦略で再検討。
- **UiHost::take_frame_outputs() (low-level API)**: `frame_to_edits` 経由の low-level user 向けに transient 取り出し API を追加するべきだが、現状 example で frame_to_edits を直接使うのは bench / no_clone_required pass だけなので、本 commit では未実装。M9 で必要に応じて追加。
- **arrangement / sample_editor / waveform_validation の M8 機能配備**: docs/plan.md M8 表に書かれていた個別配備 (file drop / dialog / rect select の各 example での実装) は時間制約で daw_prototype に集約。M9 で個別 example にも展開。

#### 検証結果 (本コミット時点)

- `cargo build --workspace`: ✅
- `cargo test --workspace`: ✅ (daw-ui-core 90 unit + 9 m8_integration + 1 widget_id_collision + 1 no_clone_required trybuild = 101 tests pass)
- `cargo test -p daw-ui-core --test no_clone_required`: ✅ (`Edit::with_inverse` の `Fn` 制約でも Model に Clone 不要を trybuild で固定)
- `cargo clippy --workspace --tests -- -D warnings`: ✅
- 実機 (`cargo run --bin daw_prototype` 等): ビルド ✅、shortcut Ctrl+Z/Y/O / file drop / dialog の動作確認は利用者依存

**残作業**: なし。**M8 完了**。M9 (theming + animation + icons) に進む。

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
- 2026-05-01: **M4 Phase 11** — `Ui::with_widget_node` API + 6 widget 適用 + 描画コマンドキャッシュ。`SceneNode` を `{ input_hash, commands: CachedCommands }` に拡張、`CachedCommands { rects, glyph_areas, line_batches }` で per-widget の描画コマンドを保持。`Ui::with_widget_node(wid, input_hash, draw_fn)` API を追加: hash 一致なら draw_fn をスキップして前フレームの commands を scene に append、不一致なら draw_fn を実行して scene 末尾差分を新規 commands として記録。`UiHost::frame` 末尾で `scenegraph.retain(&seen_widgets)` で動的に出現/消滅する widget を扱う。6 widget (button / label / checkbox / fader / knob / text_input) を全て wrap、各 widget が `hash_inputs((b"<種別>", rect.x.to_bits(), ..., 状態フラグ))` で input_hash 計算。判別タグ (`b"fader"` 等) で異種 widget 間の偶然の hash 衝突を防ぐ。waveform は LOD generation の特殊処理が必要なので Phase 12 で。with_widget_node 単体テスト 3 本 + scenegraph 追加テスト 2 本 = 5 本追加 (合計 48 unit/test)、clippy 増分 0、mixer 実機で表示・操作に regression なし。
- 2026-05-01: **M4 Phase 12 — milestone 完了** — 波形 widget の `with_widget_node` 適用 + 1000 widget bench。`ChannelLayout` / `WaveformRenderMode` に `Hash` derive 追加 (input_hash で使用)、`Ui::waveform` を with_widget_node で wrap (input_hash に generation / valid_len / sample_rate / view / style / rect を組込、ヒットテストはクロージャ外で毎フレーム実行)。Rust 標準 Hash の tuple 上限 (12 要素) 回避のため hash_inputs に nested tuple を渡す形。`crates/ui/benches/scenegraph_cache.rs` 新規 bench で 1000 buttons の cached vs no-cache を比較、cached 165 µs vs no-cache 313 µs で 1.9x 高速 (148 ns/widget の CPU prepare コスト削減)。waveform_validation で 128 widgets の波形描画 + drag scroll / wheel zoom / Space REC に regression なし。M4 milestone 全項目達成、次は M5 (heavy() + 巨大ビュー + 詳細波形モード) へ。
- 2026-05-01: **chore(clippy)** — Rust 1.95 系の新 clippy ルールで噴出した 30+ 件の警告を解消し `cargo clippy --workspace --tests -- -D warnings` を再び通すための前提整備。`ignored_unit_patterns` (ui.rs テスト内 `|_, ui|` を `|(), ui|` に 17 件)、`uninlined_format_args` (layout / fader / knob テストの `format!("{}", x)` を `"{x}"` に 7 件)、`borrow_as_ptr` (winit_backend の `&mut pt` を `&raw mut pt`)、`match_wildcard_for_single_variants` (device.rs に wgpu 29 系 `CurrentSurfaceTexture::Validation` を明示)、`collapsible_if` / `match_same_arms` / `default_trait_access` × 4 / `cast_possible_wrap` (`screen.{w,h}.try_into().unwrap_or(i32::MAX)`) / `iter_last_double_ended` (filter().last() → rfind()) / `inline_always` (waveform hot path 用 `#[inline(always)]` に `#[allow]` 併記) / `needless_range_loop` (waveform / mixer の index アクセス) / `too_many_lines` (text_input / waveform_validation / mixer の build_ui 系大関数に `#[allow]`、関数分割は別タスク) / `missing #[must_use]` (id.rs::child)。機能変更なし、Phase 13 とは独立の chore commit として分離。
- 2026-05-01: **M5 Phase 13** — heavy() API 基盤。`Ui::heavy(id, |hctx|)` + `HeavyCtx<'b, 'a, M>` + `cached(viewport_key, draw_fn)` を `crates/ui/src/widgets/heavy.rs` に新設。`HeavyCtx::cached` は M4 Phase 11 の `with_widget_node(child_wid, hash_inputs(viewport_key), ...)` を呼ぶだけの薄いラッパで、新キャッシュ機構ゼロ (`feedback_use_new_abstractions` 適合)。HeavyCtx は `pointer / screen / push_edit / push_rect / push_text / push_lines / waveform / label_at / button_at` を delegate (最小範囲、KISS)、`push_*` は heavy 内では `pub` (脱出口の意味、通常 widget からは引き続き `pub(crate)`)。ヒットテスト・動的 overlay は cached() の外で毎フレーム実行のパターン (waveform widget と同型)。viewport_key は explicit Hash 渡し (Clone 不要)、no-Clone 不変条件維持。`examples/mixer` に heavy_demo ブロック (m.count を viewport_key に、cache hit/miss を実機目視確認用) を 1 つ仕込み。trybuild (`tests/ui/pass/heavy.rs`、no-Clone + viewport_key Hash 制約) と単体テスト 4 本 (cache hit / miss / ヒットテスト経路 / eviction) で動作担保、合計 50 unit/test pass。clippy 増分 0、`cargo run --bin mixer` で表示・操作に regression なし。次は Phase 14 (examples/piano_roll、heavy() 実用第 1 弾)。

---

## M9 Phase 41 (Real DAW Validation — note edit + library widget 化、完了 2026-05-03)

**目的**: M8 で導入した `Edit::Undoable` の ergonomic を note 編集ケースで実証 (Phase 41a-d)、その経験を踏まえて piano_roll を library widget 化 (Phase 41e)。

**進捗**: Phase 41 全 7 commits 完遂 (41pre + 41a + 41b + 41c + 41d + 41e + 41f)。

| 成果物 | 状態 | コミット |
|---|---|---|
| 41pre: HeavyCtx に input/popup/shortcut/clipboard/history pull API 14 method delegate | ✅ | 63b361f |
| 41a: piano_roll に Note id 導入 + 複数対応 add/delete を Edit::with_inverse 化 | ✅ | 8c2c49e |
| 41b: 複数対応 move/resize Undoable + Ui::set_cursor (EwResize/Move) | ✅ | c81e685 |
| 41c: rect multi-select (Alt+drag) + selection state Undoable | ✅ | b0ac62d |
| 41d: Edit::snapshot_inverse helper を library 化 (5 helper の Arc capture pattern を吸収) | ✅ | c0fe6b6 |
| 41e: piano_roll を crates/ui/src/widgets/piano_roll.rs に library widget 化 | ✅ | 8878388 |
| 41f: docs(M9 Phase 41) 完了記録 + Phase 44 評価項目更新 | ✅ | (本コミット) |

**主な学び**:

- **Edit::with_inverse + Arc<...> capture の boilerplate** は 5 ペア (add/delete/move/resize/select) で顕在化 → `Edit::snapshot_inverse(label, snapshot, forward, restore_from)` helper で吸収成功 (41d)。snapshot を Arc 化して 2 closure に共有する pattern を 1 関数に集約。
- **note の identity は `id: u32` 不変** が必須。natural key (start_beat, pitch) は move で変わる、index は delete でずれる。`PianoRollModel::next_note_id` で生成時 unique 採番、編集中も保持。multi-select identity が安定する。
- **multi-delete = 1 Edit で完結** するため history group API は Phase 41 では不要だった (複数対応 helper で N notes の delete/move/resize/select が単一 `Edit::snapshot_inverse` に集約)。`begin_group / end_group` の need が出るのは「異なる種類の Edit を 1 step に」のケース、Phase 42 (audio trim → fade 連続) で実需が出たら別途実装判断。
- **library widget 化は callback パターン (`make_edit: Fn(NotesEditRequest) -> Edit<M>`) で API 簡潔化** (41e)。当初 5 callback 案 (on_add / on_delete / ...) ではなく単一 callback + ADT (`NotesEditRequest` enum 5 variants) で吸収。`automation_curve::on_change` callback と同形のパターン precedent。`NotesEditRequest` は 1 frame で消費される一時 ADT で、Application::Message のように Model に保存される / Clone 伝染する性質はなく、メッセージ型禁止の不変条件と矛盾しない。
- **drag 中は library overlay 描画 + release で初めて Undoable Edit 発行** (commit-by-release pattern)。drag 中 Mutate Edit 発行や `MoveContinue` variant 追加が不要。history も「drag 1 step = 1 Edit」で綺麗に保たれる。
- **selected_note_ids は `&[NoteId]` immutable borrow + push_edit ベース更新** (41e)。`UiHost::frame` の closure シグネチャ `for<'a> FnOnce(&'a M, &mut Ui<'a, M>)` が `model: &M` (immutable) のため、`&mut model.selected_note_ids` を取れない (borrow checker E0596)。selection 変更は `NotesEditRequest::Select` を push_edit で発行 → frame 末で apply、次フレームで反映。これは no-Clone 不変条件と整合する正しい設計だった。
- **daw_01 (path 依存先) は gui_01 の `Note` 型を直接 import していない** ため、library 化 commit は daw_01 build に影響なし (12 ファイルが daw_ui_core を import するが Note 関連型は 0)。daw_01 は piano_roll を独自 NoteBox 型 (f64 + lyric) で実装しており、schema 不一致は Phase 44 で統合判断 (Note の f64 化 / lyric 追加 / Arc 化 等)。
- **trybuild の no_clone_required 担保** に `Ui::piano_roll` 呼び出しを追加し、`Note` を Vec で持つ non-Clone Model でコンパイルできることを CI 固定。

**設計判断 (Phase 41 中の重要決定)**:

- `Ui::set_cursor(CursorIcon)` は **「最後勝ち」semantics** (cursor stack push/pop は不要、DAW UX で同 frame 複数 widget の cursor 競合は実用上発生しない、`UiHost::with_window` で `set_cursor_request` callback も自動 set)。
- `HeavyCtx` 包括 14 method delegate (41pre): rect-select / context_menu / shortcut consume / clipboard / file drop / scroll / history が heavy 内で書けない問題を 1 commit で塞ぐ。各 method は 1 行 forward なので LOC コスト低い、heavy 抽象の漏れを防ぐ目的を優先。
- 複数対応 helper は **最初から `Vec<Note>` / `Vec<MoveDelta>` ベース** (single note は `vec![note]` で呼ぶ): DAW では multi-select が常態。後付けで multi 化すると call site が double。
- **library widget 化を Phase 41e で完遂** (plan.md L142 の「後回し」方針を 2026-05-03 に修正): CLAUDE.md「理想とベストプラクティスを追求する。そのためは大胆に破壊して作り直す」方針に基づき、validation 中も breaking 変更を恐れず逐次反映。daw_01 への影響軽微 (Note 未 import) が確認できたため。
- **view 状態は user 側、widget は値渡し `PianoRollView`**: pan/zoom 更新は app 層責務、widget は描画と note drag のみ担う。view を mutate する API を入れるとスコープが膨らむ。
- **Edit factory 5 個 (`make_*_notes_edit`) は example に残す**: forward / inverse closure 内で `m.notes` / `m.selected_note_ids` / `m.notes_generation` を mutate する必要があり、generic 化には `NotesModel` trait が必要だが、daw_01 のような独自 schema (NoteBox / lyric / f64) を持つアプリで impl 不可能になり拡張性を損なう。library 側は `NotesEditRequest` enum を介した callback パターンで責務分離。

**残作業 (Phase 42-44)**:

- Phase 42: sample_edit_ops の trim/fade を `Edit::snapshot_inverse` 化、audio buffer (`Vec<f32>`) の inverse 戦略を 3 案 (full snapshot / 差分のみ Vec / Arc COW) から選定。
- Phase 43: `Ui::debug_overlay` で frame_ms / scenegraph_size / cache_hit_rate / widget_count / history_depth を画面右上に半透明 overlay (Ctrl+F1 toggle)。
- Phase 44: Phase 41-43 の `Edit::with_inverse` / `Edit::snapshot_inverse` 全 call site の boilerplate を計測 + library helper 追加判断、daw_01 との Note schema 統合判断。

**Phase 44 評価項目への引き継ぎ**:

- `Edit::snapshot_inverse` の汎用性: Vec<Note> で 5 ペア吸収済。Phase 42 の Vec<f32> で同 helper が再利用できるか検証。
- `NotesEditRequest` enum + 単一 callback パターン: callback 5 個個別ではなく ADT で簡潔化の precedent、Phase 42 の audio buffer 系で類似パターンを使うか判断材料。
- `daw_ui_core::Note` schema (id: u32, f32, no lyric) と daw_01 NoteBox schema (note: u32 = index 内部 id, f64, lyric: Option<String>) の不一致: Phase 44 で統合判断 (`f64` 化 / `lyric: Option<Arc<str>>` 追加 / id 命名統一)。

**LOC**:

- library: `crates/ui/src/widgets/piano_roll.rs` 新設 ~1565 LOC (公開型 9 + 純粋関数 6 + Style default + `Ui::piano_roll` 本体 + tests 23 ケース)。
- example: `crates/examples/piano_roll/src/main.rs` 1480 → 720 LOC に縮小。
- 計画 (plan_phase41.md) は library +400 / example -600 / test +200 = net +0 を見込んでいたが、実装で純粋関数を library に出した形 (Edit factory は example 残し) になり、別の LOC 配分になった。
