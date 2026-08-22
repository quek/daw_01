<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_ruler_density.md

ルーラ (`Ui::time_ruler`) と bar/beat grid (`Ui::bar_beat_grid`) を、
表示倍率 (= 1 bar あたりの px 幅) に応じてラベル / tick / 線を自動間引きする。

## 1. 現状の問題

- `Ui::time_ruler` (gui_01 `crates/ui/src/widgets/time_grid.rs:131-156`)
  は viewport 内の全 bar に対して bar label (`"1"`, `"2"`, ...) を 1 つずつ
  push する。
- 結果、 arrangement view を強くズームアウトすると 1 bar あたり数 px に
  なり、 隣り合う label が完全に重なって読めなくなる。
- bar tick (`time_grid.rs:103-120`) も同様に viewport 内の全 bar / beat
  を残らず描画するため、 zoom 小では tick 間が密集して bar/beat が判別
  できない。
- `Ui::bar_beat_grid` (`time_grid.rs:184-222`) も同じ問題があり、 zoom
  小では beat 縦線が密集して描画コストとノイズが増える。

## 2. 期待する最終挙動

ユーザー視点:

- ズームアウトすると bar label が **読める間隔のまま自動で間引き** され
  る (1 bar → 2 bar → 4 bar → 8 bar → 16 bar ... のように 2 のべき乗で
  step up)。
- ズームインすると逆方向に細かくなり、 1 bar 表示が十分広ければ beat
  ラベルや小数 beat も描画する余地を残す (Phase 2 で検討、 今回は bar
  ラベルのみ対象)。
- beat tick (label を持たない短い tick) は 1 beat あたり 4 px 以上
  確保できるときだけ描画。 それ未満では消す。
- bar tick は label と同じ間引き step に従う。
- `bar_beat_grid` の beat 縦線も、 1 beat あたり 4 px 未満なら描画しない
  (= bar 縦線のみ残る)。

DAW 比較:
- Reaper: ruler は zoom に応じて自動的に "1, 2, 3" → "1, 5, 10" → "1,
  10, 20" と段階的に label を間引く。
- Ableton Live: 同様。 zoom 小では bar 番号が `1, 17, 33, ...` のように
  対数的にスキップ。
- Cubase / Pro Tools: 同様。

## 3. 想定 API (gui_01 への要望)

### 3.1. `TimeRulerStyle` に追加

```rust
pub struct TimeRulerStyle {
    // 既存 field ...
    pub bg: Color,
    pub tick_color: Color,
    pub label_color: Color,
    pub bar_tick_height: f32,
    pub beat_tick_height: f32,

    // 追加: ラベルが重ならないための最小間隔 (px)。 1 bar の表示幅が
    // この値未満なら、 描画 step を 2 bar / 4 bar / 8 bar ... と
    // 2 倍ずつ skip して、 隣接 label が必ずこの距離以上離れるよう
    // にする。 default は 60.0 (= 4 桁 bar 番号 + 余白程度)。
    pub min_label_spacing_px: f32,

    // 追加: beat tick を描画する最小 1 beat 表示幅 (px)。 これ未満
    // では beat tick を描かず bar tick のみ表示。 default は 4.0。
    pub min_beat_tick_px: f32,
}
```

### 3.2. `BarBeatGridStyle` に追加

```rust
pub struct BarBeatGridStyle {
    // 既存 field ...
    pub bar_color: Color,
    pub beat_color: Color,
    pub bar_line_width: f32,
    pub beat_line_width: f32,

    // 追加: beat 縦線を描画する最小 1 beat 表示幅 (px)。 これ未満
    // では beat 縦線を描かず bar 縦線のみ。 default は 4.0。
    pub min_beat_line_px: f32,
}
```

### 3.3. 内部実装の方向

`time_ruler` 内部で:

```rust
// 1 bar の表示 px 幅
let px_per_bar = (mapping.samples_per_bar() / viewport.view_len) as f32 * rect.w;

// label step (bar 単位): 1, 2, 4, 8, 16, ...
let mut label_step: i64 = 1;
if px_per_bar > 0.0 && style.min_label_spacing_px > 0.0 {
    while (px_per_bar * label_step as f32) < style.min_label_spacing_px {
        label_step *= 2;
        if label_step > (1 << 20) { break; }  // safety
    }
}

// label loop: bar % label_step == 0 だけを描く
for bar in bar_index_start..=bar_index_end {
    if bar.rem_euclid(label_step) != 0 { continue; }
    // ... label 描画
}
```

`bar tick` も `label_step` で間引き、 `beat tick` は `min_beat_tick_px`
に対して 1 beat 表示幅を比較して on/off。

`bar_beat_grid` の beat 線も `min_beat_line_px` に従って on/off。

## 4. テスト方針

gui_01 側で:
- `time_ruler` を `viewport.view_len` を 10 倍ずつ変えて呼び、 描画
  された label 数が `min_label_spacing_px` を満たす最大数になることを
  確認 (= snapshot test or scenegraph node 数 check)。
- `bar_beat_grid` も同様に beat 線数を測定。

daw_01 側では特別な対応不要 (= TimeRulerStyle::default() の field 値
変更だけで効くはず)。 path 依存再ビルドで取り込み。

## 5. 影響範囲

- daw_01 で time_ruler / bar_beat_grid を呼んでいる場所:
  - `daw_gui/src/view/audio_editor.rs` (この PR で追加した ruler)
  - `daw_gui/src/view/arrangement_view.rs` (Ui::arrangement 内蔵 ruler)
  - `daw_gui/src/view/piano_roll_view.rs` (Ui::piano_roll 内蔵 ruler)
- gui_01 で:
  - `crates/ui/src/widgets/time_grid.rs` (本 plan の修正対象)
  - `crates/ui/src/widgets/arrangement.rs` (内部で time_ruler 呼び出し)
  - `crates/ui/src/widgets/piano_roll.rs` (内部で time_ruler 呼び出し)
- TimeRulerStyle / BarBeatGridStyle の Default impl 変更は **後方互換**
  (= field 追加で既存 caller は default 値を使う)。 daw_01 は path
  依存再ビルドで自動取り込み、 個別変更不要。
