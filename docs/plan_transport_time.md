<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_transport_time — トランスポートに bar.beat + 時間 を併記する

FIXME #4。「普通の DAW みたいに再生時間を表示したい」。

## 現状 (2026-06-08)

トランスポートは再生位置を **生 beat 値**でしか出していない
([transport.rs:551-563](F:/dev/daw_01/daw_gui/src/view/transport.rs)):

```rust
let playhead = app.playhead_beat
    .map(|b| format!("\u{25b6} {b:7.2}"))         // "▶ 123.45" (raw beat)
    .unwrap_or_else(|| "\u{25a0}   --".to_string());
```

= 小節も時間も出ず、professional DAW の標準デュアル表示 (音楽的位置 + 絶対時間)
に達していない。

## 確定仕様 (grill-me 2026-06-08)

トランスポートに **音楽的位置 `bar.beat` と絶対時間 `分:秒.ms` を併記**する。
例: `▶ 9.1  |  00:13.714`。停止時は `■  --.-  |  --:--.---`。

| # | 項目 | 内容 |
|---|---|---|
| 1 | SSoT | 再生位置は `app.playhead_beat` (beat 単位) **一本**。bar.beat も time も同じ source から導出 (二重持ちしない) |
| 2 | bar.beat | `song.time_sig` で beat → (bar, beat) 変換。**1-based** (bar1 始まり、beat1 始まり) = アレンジ / piano roll のルーラと同基準 (gui_01 `samples_to_bar_beat` と同じ式を使い表示を一致させる) |
| 3 | time | beat → 秒は **再生エンジンと同じ beat↔sample↔秒 写像**を使う ([common/src/timing.rs](F:/dev/daw_01/common/src/timing.rs))。tempo automation がある曲でも playback と表示がズレないよう、定数 bpm 近似でなく engine と同じ mapping を SSoT にする。`分:秒.ms` 形式 (`{:02}:{:02}.{:03}`) |
| 4 | 配置 | 現 beat-only readout を本デュアル表示に置換 (transport bar 内、現在の playhead label 位置)。再生/停止アイコン (▶/■) は維持 |
| 5 | 視覚調整 | 桁数 / 区切り / 等幅は実機で微調整 (bar.beat に sub-beat tick を足すかは見てから判断、初期は bar.beat) |

### 変換式

```text
beats_per_bar = song.time_sig から (gui_01 samples_to_bar_beat と同式)
bar  = floor(playhead_beat / beats_per_bar) + 1
beat = playhead_beat - (bar-1)*beats_per_bar + 1          // 1-based
sec  = timing による playhead_beat → samples → /sample_rate   // engine と同写像
mm:ss.ms = floor(sec/60) : floor(sec%60) . floor((sec%1)*1000)
```

## 実装方針

- `transport.rs` の playhead format を bar.beat + time のデュアル文字列へ。
- bar.beat 変換 helper は gui_01 の `samples_to_bar_beat` と式を揃える
  (アレンジ / piano roll ルーラと bar 番号が一致すること)。可能なら
  `common` 側に beat→(bar,beat) helper を 1 つ置いて SSoT 化し、transport と
  他表示が同じ関数を引く。
- time 変換は `common/src/timing.rs` の既存 beat↔sample 写像を使う
  (tempo automation 対応・playback 一致)。

## 受け入れ基準

- 再生位置に応じて bar.beat と 分:秒.ms が両方更新される。
- bar.beat がアレンジ / piano roll ルーラの小節番号と一致する。
- time が実際の再生音の経過時間と一致する (tempo automation 曲でもズレない)。
- 停止時はプレースホルダ表示、再生アイコンは ▶/■ で状態を示す。

## 非範囲

- SMPTE timecode (HH:MM:SS:FF) は今回出さない (grill で bar.beat + 分:秒.ms を選択)。
- 表示位置の click で表示単位を切替える等の対話機能は今回扱わない。
