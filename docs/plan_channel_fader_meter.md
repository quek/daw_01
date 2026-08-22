<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_channel_fader_meter — fader と meter を単一 widget で「同一 dB→y 写像」に統一する

## Context

mixer strip の Volume fader と L/R level meter は「同じ dB スケールで見える」べき
（Ableton のトラック fader+メーター）。コミット `d2b1477` (#081 / #082) で
**dB→fraction のカーブ**は `MeterScale::default()` に統一されたが、**fraction→ピクセル y**
の写像は各 widget が別々の内部 inset で計算しており、一致していない。

### 不一致の真因（一次情報・2026-06-07 確定）

両 widget は同じ outer rect（`y=fader_top`, `h=fader_h`）と同じ `MeterScale` カーブを使うが、
frac=0..1 を写すピクセル領域が違う:

| widget | frac=0..1 を写す y 領域 | 上 inset | 下 inset | 使用可能高さ |
|---|---|---|---|---|
| `fader_at` (`fader_geometry`) | `[rect.y+8, rect.y+h−8]` | 8 (`TRACK_PAD`) | 8 | `h−16` |
| `level_meter_stereo` (scale+readout 有) | `[rect.y+22, rect.y+h−6]` | **22** (readout帯 16 + `SCALE_VPAD` 6) | 6 | `h−28` |

→ 0dB (frac 0.89) で fader 塗り上端とメーターの 0dB 横線が **約 13px ズレ**、しかも
使用可能高さが違う（`h−16` vs `h−28`）ため **ズレ量が dB ごとに変動**する。
最大要因はメーター上端の peak-readout 帯（16px、fader 側には無い）。

「2 つの widget に同じ `MeterScale` を渡す」(#082) ではカーブしか共有できず、
**画素写像が 1 箇所所有でない**限りズレは再発する。

## 確定仕様 (grill-me 2026-06-07)

| # | 項目 | 内容 |
|---|---|---|
| 1 | 所有 | gui_01 に **単一 widget `channel_fader_meter`** を新設。1 rect 内で fader ハンドル・L/R バー・dB 目盛り・0dB 線・peak を **ただ一つの dB→ピクセル y 写像**から配置する。ズレは構造的に発生不能（SSoT）。`fader_at` / `level_meter_stereo` は汎用部品として残す |
| 2 | peak readout | **共有 dB→y 領域の上に専用帯**として確保（fader/meter とも帯の下から +6dB 開始）。Ableton 同様、各 ch 上端に最大到達 dB、click で reset。fader 列には readout チップはかからない（meter 列幅中央） |
| 3 | 目盛り | tick / 0dB 線 / 数字は **meter 列のみ**。fader トラックにグリッドは引かない。列構成 `[fader | tick | L | R | 数字]`（現状維持）。高さ一致だけ保証し、ハンドル 0dB とメーター 0dB 線が同高 |
| 4 | fader 挙動 | DAW 標準: 下端 = `−∞`(frac0 → 無音) / 上端 = `+6dB` / ダブルクリック = `0dB`(unity) リセット / Ctrl+drag = 1/10 微調整。curve 下端 −60dB 以下は frac0 に収束（−60..−∞ は不可分、許容） |
| 5 | 幅 | **80px strip を広げない**。現 `group_w = FADER_W(18) + METER_GAP(2) + METER_SCALE_W(35) = 55px` を widget が内部分割で踏襲 |
| 6 | カーブ | `MeterScale::default()`（plan_meter_scale.md の breakpoint 表）を fader/meter/arrangement volume band が共有（SSoT）。実機で視覚調整する暫定値のまま |

### dB→ピクセル y 写像（widget が 1 箇所所有）

```
band_top   = rect.y + READOUT_BAND_H            // peak 専用帯 (peak_readout 時のみ)
region.y   = band_top + VPAD                    // +6dB ラベルが切れない上余白
region.h   = (rect.y + rect.h - VPAD) - region.y // −60 ラベルが切れない下余白
y(frac)    = region.y + region.h * (1.0 - frac)  // ← fader/meter/tick/0dB線が全部これ
```

- fader: ハンドル中心 = `y(scale.db_to_frac(volume_db))`。塗りは region 下端から上へ。
- meter: バー塗り = `region.h * frac` を region 下端から上へ。0dB 線・tick・数字も `y(frac)`。
- thumb 高 (10px) の食み出しは上 = readout 帯、下 = VPAD(6 ≥ 5) の範囲に収まり clip しない。
- `band_top` も `VPAD` も 1 箇所で決まるので fader と meter は必ず同高。

## gui_01 #083 要望 API

```rust
pub fn channel_fader_meter<F>(
    &mut self,
    id: impl Hash,
    rect: Rect,           // group 全体 (例: 55px)。widget が内部で fader / meter に分割
    fader_w: f32,         // 左の fader 列幅 (例: 18.0)。残りが meter (tick|L|R|数字)
    volume_db: f32,       // フェーダ現在値 (dB)。f32::NEG_INFINITY = 無音
    default_db: f32,      // ダブルクリック reset 先 (= 0.0 unity)
    l: f32,               // L peak linear (-1..1)、毎フレーム
    r: f32,               // R peak linear
    ballistic: MeterBallistic,
    style: LevelMeterStyle, // scale: Some(_) 必須。fader/meter 両方がこの 1 つの curve を共有
    label: &'static str,    // undo history ラベル ("Track Volume" / "Master Volume")
    on_change: F,           // on_change(new_db) -> Edit<M>。frac0 → NEG_INFINITY
) -> ChannelFaderMeterResponse
where F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static;

pub struct ChannelFaderMeterResponse {
    pub fader: FaderResponse, // .dragging / .displayed_value(dB) / .hovered（gesture edge 用）
    // meter の peak-reset click は widget 内部で消費済み
}
```

- `style.scale` の `MeterScale` を fader ハンドルと meter バー両方に適用 → コードで一致保証。
- `style.peak_readout = true` のとき上端帯を確保し、それを除いた領域を `region` とする。
  `false` なら帯 0、region は VPAD だけ内側。
- 内部レイアウト（左→右）: `[fader_w | METER_GAP | tick gutter | L | R | 数字 gutter]`。
  meter 部分の tick/L/R/数字 配分・色帯・0dB 線・peak readout は既存 `level_meter_stereo`
  のロジックをそのまま `region` に対して使う。
- hit-test: fader thumb 内の press → fader drag（既存 `fader_at` の drag/dblclick/Ctrl ロジック再利用）。
  それ以外（meter 部分）の press → peak reset。x 位置で分岐、空間的に重ならない。
- undo/redo は dB 空間（既存 `fader_at` の inverse 機構）。

## daw_01 側（#083 landing 後）

- `mixer_strips.rs::draw_strip` の **`ui.fader_at(...)` + `ui.level_meter_stereo(...)` の 2 呼び出しを
  `ui.channel_fader_meter(...)` 1 呼び出しに統一**。group rect (`group_x, fader_top, group_w, fader_h`)
  と `fader_w = FADER_W` を渡す。`fader_db` の手計算と `style` 構築はそのまま流用。
- `push_param_gesture_edges` には `resp.fader.dragging` を渡す。
- `arrangement_view.rs` は `MeterScale` カーブで volume band を描く現状のまま不変（fader widget は使っていない）。
- `daw_01` 側の dB↔amp 変換（音声ドメイン）は残す。dB↔frac（カーブ）は widget が所有。

## plan_meter_scale.md との関係

plan_meter_scale.md 確定仕様 **#8「fader 現状のまま独立（統合しない）」は本 plan で破棄**。
fader は meter と同一カーブ・同一 dB→y 写像を共有する（#081/#082 で着手 → 本 plan で完成）。
