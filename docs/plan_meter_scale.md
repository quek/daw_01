# plan_meter_scale — メーターを Ableton Live 風に作り直す

## Context

mixer のレベルメーターは gui_01 `level_meter` widget。ユーザーと grill-me で詰めた結論を**唯一の仕様**として
ここに固定する。方針は一貫して **Ableton Live と同じ見た目**。

> 経緯メモ: 初版 (#073) は **mono バー + 線形スケール** で実装したが、ユーザー要望は
> **ステレオ L/R + 非線形スケール + 全チャンネル目盛り**。#073 を改訂する形で gui_01 #074 を出す。
> 当初 daw_01 側で目盛り/数値を自前描画しかけた件は SSoT 違反として全撤去済 (widget が所有)。

## 確定仕様 (grill-me 2026-06-04)

| # | 項目 | 内容 |
|---|---|---|
| 1 | ステレオ | 各 ch メーターは **L/R 2 本のバー** |
| 2 | 対象 | **全 ch** (track / return / group / master) に**フル目盛り** |
| 3 | レイアウト | 左→右で **`[tick | L バー | R バー | dB 数字]`** (Ableton 配置) |
| 4 | スケール形状 | **非線形** (top-weighted、 上を引き伸ばし下を圧縮)。下記カーブ表 |
| 5 | dB 値 | `+6, 0, -6, -12, -18, -24, -30, -36, -42, -48, -54, -60` (各値に tick + 数字) |
| 6 | 0dB | **L/R 両バーを横切る横線で強調** + 0 のラベル |
| 7 | 数値ピーク | 各メーター上端に**最大到達 dB** (`-inf`/`{:.1}`、 0dB 超は赤)、**メーター click で reset** |
| 8 | fader | **現状のまま独立** (統合しない)。スケールは meter 専用 |
| 9 | 幅 | **ストリップは広げない**。現 80px に収める (fader+メーターを左寄せ/再センタリング) |
| 10 | 色 | 現状の 緑→黄→橙→赤(clip) 色帯を維持 |
| 11 | 所有 | バー・目盛り・0dB 線・数値・色帯すべて **widget が同一マッピングで所有** (SSoT)。daw_01 は style/rect を渡すだけ・自前描画しない |

### 非線形カーブ (dB → 高さ。 上=1.0 / 下=0.0)

breakpoint を piecewise-linear 補間 (ラベル値がそのまま breakpoint なので、 数字は必ず tick に乗る)。
**初期値。実機で見てユーザーが視覚調整する前提** (数値だけでは判断不可とのこと)。

| dB | 高さ | dB | 高さ |
|---|---|---|---|
| +6 | 1.00 | -30 | 0.40 |
| 0 | 0.89 | -36 | 0.31 |
| -6 | 0.79 | -42 | 0.23 |
| -12 | 0.68 | -48 | 0.15 |
| -18 | 0.59 | -54 | 0.07 |
| -24 | 0.49 | -60 | 0.00 |

(上の間隔広/下の間隔狭。 例 +6→0 = 0.11、 -54→-60 = 0.07。 fader は別 (このカーブ非適用))

## gui_01 #074 要望 (level_meter をステレオ + 非線形スケールに作り直す)

#073 で入れた `scale` / `peak_readout` を活かしつつ、 **mono → ステレオ**、 **線形 → 非線形 (breakpoint 表)**、
**0dB 横線**、 **tick|L|R|数字 配置** に拡張する。

1. **ステレオ meter**: 1 call で **L/R 2 本のバー**を描く (`level_meter_stereo(id, rect, l, r, ballistic, style)`
   等)。`scale = Some` のとき rect 内を **`[tick ガター | L バー | R バー | 数字ガター]`** にレイアウト。
2. **非線形マッピング**: `db_to_fraction` を **breakpoint piecewise-linear** に差し替え可能にする
   (`MeterScale` に `curve: &'static [(f32 /*db*/, f32 /*frac*/)]`、 default = 上記表)。バー塗り・tick・
   数字・0dB 線・peak hold 線すべてこの**同一カーブ**で位置決め (SSoT)。
3. **0dB 横線**: `emphasize_zero` true のとき、 0dB の高さに **L/R 両バーを横切る横線** を描く + 0 ラベルを明色。
4. **数値ピーク** (#073 と同じ): rect 上端帯に最大到達 dB、 click で reset、 0dB 超で赤。
5. **コンパクトに収まること**: tick ガター ~6px + 数字ガター ~18px で、 L/R 4px×2 と合わせ rect 幅 ~32–36px。
   daw_01 は現 80px ストリップ内に fader(18) と並べて収める (広げない)。
6. 色帯 (緑/黄/橙/赤) は #073 のまま。`scale = None` は従来どおり clean bar。

## daw_01 側 (gui_01 #074 landing 後・style/rect のみ)

- 各 strip (track/return/group/master) のメーターを **ステレオ meter call 1 本**に統一し、
  `scale: Some(...)` + `peak_readout: true` を渡す。L/R 値を渡す。
- メーター rect を ~32–36px に取り、 fader と合わせて現 80px に左寄せ/センタリングで収める (strip は広げない)。
- **daw_01 は目盛り/数値/0dB 線を一切自前描画しない** (`mixer_strips.rs` は style/rect を渡すだけ)。
- カーブは実機で見てユーザーと視覚調整 (breakpoint 表を gui_01 と詰める)。
