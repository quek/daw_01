# Piano Roll 鍵盤レーン クリックでピッチプレビュー再生

ピアノロール左の鍵盤レーン (keyboard lane, `KEYBOARD_W` 領域) のキーをクリックすると、
そのピッチの音をプレビュー再生する (一般的な DAW のピアノロール / 鍵盤の挙動)。

- 鍵盤クリックの検出は gui_01 widget 内部 (鍵盤は widget が描画、hit-test も widget の責務)
  → **gui_01 #055** で widget が押下ピッチを `PianoRollResponse` で返す API が必要。
- 鳴らす音は daw_01 側で実装。

## 最終形態 (完成イメージ)

- 鍵盤レーンのキーを **押す** → そのピッチの note-on をそのトラックの音源プラグインへ送る。
- マウスを **離す** → note-off。
- 押したまま **上下の別キーへドラッグ** (glissando) → 古いピッチ note-off + 新ピッチ note-on。
- そのトラックに音源プラグインが無い場合は無音 (将来ビルトイン test tone を足す余地あり、v0 は無音)。
- velocity は固定 (例 100) でよい。

## gui_01 #055 で必要な widget 拡張

現状:
- 鍵盤レーンは描画のみ。grid hit-test は `grid.contains(px, py)` で鍵盤領域を **除外**
  (`crates/ui/src/widgets/piano_roll.rs:1355` 他)。
- `PianoRollResponse` (同 :420-449) に鍵盤 click を返す field 無し
  (`hovered` は「grid 内、keyboard 領域は除く」)。
- 鍵盤 rect は計算済 (`kbd = Rect { x, y+ruler_h, w: kbd_w, h: main_h }`, 同 :1332) だが
  押下判定に未使用。

要望 (最終形態):
- `PianoRollResponse` に `keyboard_active_pitch: Option<u8>` を追加する。
  - 鍵盤レーンを押している間、その時点でカーソルが乗っているキーの pitch を `Some(p)`。
  - 押していない/鍵盤外なら `None`。
  - 押下中に別キーへ drag したらフレームごとに最新キーの pitch に追従する (glissando 対応)。
- これにより daw_01 は前フレーム値と差分を取り、
  - `None → Some(p)`: note-on(p)
  - `Some(a) → Some(b)` (a≠b): note-off(a) + note-on(b)
  - `Some(a) → None`: note-off(a)
  を導出できる (held-value + caller diff、sustain と glissando の両方を最小 field で表現)。
- grid 側の note 編集・rect select とは独立 (鍵盤レーンの press は note drag を開始しない)。

## daw_01 側の実装

1. `AppData` にプレビュー状態 `preview_pitch: Option<u8>` を保持。
2. piano_roll_view が `resp.keyboard_active_pitch` を読み、前回 (`app.preview_pitch`) と差分:
   - 変化があれば `AppEvent::PreviewNoteOff(old)` / `AppEvent::PreviewNoteOn(new)` を dispatch。
3. プレビュー note は **そのトラックの音源プラグイン** へ送る。既存の MIDI/ノート再生経路を
   調査して、再生中でなくても単発 note-on/off を audio engine 経由でプラグインに届ける
   IPC を使う (要調査: 既存の live MIDI input / VOICEVOX 以外の単発発音経路)。
   - RT 安全性: 再生スレッドへは既存のロックフリー経路で渡す。新たな alloc/lock を足さない。
4. clip 未選択 / トラックに音源なし → no-op (無音)。

## 注意

- 本機能は gui_01 #055 の API 追加が前提。#055 解決前は daw_01 側は着手しない
  (interim 実装に走らない — feedback_gui_01_request_before_interim)。
- 音源送出経路は #055 解決後に daw_audio / daw_plugin_host 側を調査して確定する。
