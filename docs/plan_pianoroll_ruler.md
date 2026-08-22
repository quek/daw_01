<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_pianoroll_ruler

> **座標系の決定は改訂 (2026-06-08)**: 本 plan の「`start_beat` / notes は
> **clip-local**、caller が push-back 時に offset 加算」という座標決定は、
> ruler が clip-local bar (1,2,3…) を表示する = daw_01 FIXME #3 のバグそのもの
> だった。`docs/plan_pianoroll_song_absolute.md` で **view 全体を song-absolute
> に統一 (view 入口で `+clip.start_beat`、model 書き戻し出口で `-clip.start_beat`)**
> に破棄・置換する。本 plan の ruler **操作仕様** (plain click=seek / Shift+drag=loop
> 等) 自体は引き続き有効。

ピアノロール widget の ruler を arrangement widget と同じ操作セットに
揃え、 「ピアノロール上での playhead 移動 / loop 範囲設定」 を arrangement と
同等の UX で提供する。

## 動機

現状 (2026-05-15):

- `Ui::arrangement` の ruler は `#024` で実装済 (plain click/drag = seek、
  Shift+drag = loop range)。 [arrangement.rs:558,565,4520-4581](gui_01:crates/ui/src/widgets/arrangement.rs)
- `Ui::piano_roll` の ruler は M13 Phase 55 で **描画** が入ったが、
  **操作不可** (click/drag は何も emit しない)。 [piano_roll.rs:139](gui_01:crates/ui/src/widgets/piano_roll.rs)

= MIDI 編集中に「クリップ内の途中位置から再生したい」 「特定範囲だけ
ループ再生したい」 ができない。 一般的な DAW (Reaper MIDI editor /
Cubase Key editor / Bitwig Detail editor / Live MIDI clip editor) は
ピアノロールの ruler にも同操作を提供している。

## 最終形態

ピアノロール widget の ruler 領域 (`ruler_h > 0` のとき、 keyboard より右側、
grid と同じ x 範囲) で arrangement と完全に同じ操作セットを提供する:

| 操作 | 結果 |
|---|---|
| plain (= Shift 非保持) ruler click | press frame で playhead = click 位置の beat に seek (snap 適用、 `0.0` 以上 clamp、 alt で snap 一時無効) |
| plain ruler drag | press + continuation frame で連続 seek、 release は emit しない |
| `Shift` + ruler drag (NewRange) | loop 範囲を新規作成 (anchor = press beat、 cursor = drag 位置) |
| `Shift` + ruler drag (Start/End handle) | 既存 loop の左端 / 右端を drag |
| `Shift` + ruler drag (Middle) | 既存 loop を平行移動 (長さ維持) |
| `Alt` 押下中 | snap 一時無効 (Move / Resize と同 policy) |

座標系:
- `start_beat` / `len_beats` / `keyboard_w` は **clip-local beats** で
  ruler の x↔beat 変換は arrangement と同じ式 (`px_to_beat`)。
- `Note.start_beat` と同じ座標系 (clip 開始 = 0.0)。
- 但し `view.playhead_beat` は **song-global** (clip 開始からのオフセットを
  足した値を caller 側で渡している現行 piano_roll_view 仕様)。 widget は
  自分が描画する beat 範囲に対して同じ単位で push する。

snap:
- `PianoRollView.snap` (= `SnapConfig`) を使う。 arrangement と同じ helper
  `view.snap.snap_beat(raw, alt, zoom_x_px_per_beat)`。

## daw_01 側の wire

`daw_gui/src/view/piano_roll_view.rs::make_edit` に新 arm を追加 (widget の
edit-request enum が拡張される前提)。

```rust
// 概念図
NotesEditRequest::SetPlayheadBeat(beat) => {
    // arrangement_view と同じ idiom:
    // 1) playhead_beat = Some(beat) で UI 更新
    // 2) MainToChild::SeekTo { samples } で audio engine に同期
    let beat = beat.max(0.0);
    Edit::mutate(move |app: &mut AppData| {
        app.playhead_beat = Some(beat as f32);
        let sr = common::audio_bridge::SAMPLE_RATE as f64;
        let bpm = app.song.bpm.max(1.0) as f64;
        let samples = (beat * 60.0 / bpm * sr).max(0.0) as u64;
        app.send_audio(common::protocol::MainToChild::SeekTo { samples });
    })
}
NotesEditRequest::SetLoopRange { start, end } => {
    Edit::mutate(move |app: &mut AppData| {
        app.song.loop_start_beat = start;
        app.song.loop_end_beat = end;
        // 既存 AppEvent (例: SetLoopRange) があればそれを使う
    })
}
```

ピアノロール widget の view では `view.playhead_beat` が song-global なのに
対し、 ruler 上 click の beat は clip-local 単位で widget から渡される
（widget 内座標は `start_beat..start_beat + len_beats` の clip-local）。
caller (daw_01) は `selected_clip.start_beat` をオフセットとして加算し、
song-global beat に変換してから seek / loop 範囲に変換する。

```rust
// piano_roll widget が clip-local beat を push する場合の wire 概念
NotesEditRequest::SetPlayheadBeat(clip_local_beat) => {
    let clip_start = ...; // selected_clip の song-global start_beat
    let song_beat = (clip_start + clip_local_beat).max(0.0);
    // → 上記と同じ AppEvent / IPC
}
```

widget 側で song-global で push する API を選ぶか clip-local で push する
API を選ぶかは gui_01 Claude の設計判断に委ねる。 piano_roll widget は
そもそも clip context を知らない (notes と view しか持たない) ので、
**clip-local で push** が自然 (caller がオフセット加算する責務を持つ)。

## 受け入れ基準

- ピアノロールの ruler を click → playhead がそのピッチ位置の clip-local beat
  (= song-global = clip.start_beat + clip_local) にジャンプ
- 再生中でも click で playhead がジャンプ (= audio engine への seek IPC が
  飛ぶ)
- ruler drag で playhead が追従 (continuation frame で連続 emit)
- `Shift` + ruler drag で loop 範囲を新規作成 / 既存 loop の端 drag / 平行移動
  が arrangement と同じように動く
- snap が有効なら beat が snap される、 `Alt` で一時無効
- keyboard / grid / velocity lane / note 操作は既存挙動維持 (= regression なし)

## 非範囲

- マーカー機能 (numbered marker / loop name 等) は今回扱わない。
- ruler 右クリックコンテキストメニューも今回扱わない。
- ピアノロール内の ruler が **clip 範囲外** (clip 開始より前 / clip 終了より
  後) を含む場合の挙動は widget の現行 view 仕様に従う (= 同じ座標式で
  seek、 song-global で `< 0` のときは `0.0` clamp)。
