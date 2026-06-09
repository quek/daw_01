# plan_inspector_scrub — インスペクタの数値入力をドラッグ編集可能にする

FIXME #15。「インスペクタに表示する値入力は BPM のようにマウスドラッグでも値を変えられる
ようにしてください」。テキスト / 画像クリップの X / Y 等の変形フィールドも含む。

## 現状 (2026-06-09)

インスペクタの数値フィールドは大半が `ui.text_input_at`（クリックしてタイプ専用）。クリップ
種別ごとに 3 系統ある（[track_inspector.rs](F:/dev/daw_01/daw_gui/src/view/track_inspector.rs)）:

- **オーディオクリップ**: Gain(dB) / Pan / Pitch（:316,342,368）+ Fade In / Fade Out（:404,446）。
  各 field は個別の名前付き edit-buffer（`clip_gain_db_edit_text` 等）。
- **画像クリップ**: X / Y / W / H / Opacity / Rotation（:548,592,636,680,724,773）+
  Fade In / Fade Out（:828,870）。各 field は名前付き edit-buffer（`clip_image_x_edit_text`
  等）+ オートメーション "A" トグル。
- **テキストクリップ**: `emit_num_row` helper（:1205-1261、内部で `text_input_at` :1222）が
  **25 個の数値行を一括 emit**（:1263-1287: X / Y / W / H / Rot(°) / Size(px) / Opacity /
  Fill RGBA / Outline RGBA + Width / Shadow RGBA + Offset XY + Blur / Fade In / Fade Out）。
  buffer は `clip_text_num_edits: HashMap<TextNumField, String>` 1 本で共有。

一方、グループ変形インスペクタは既に `ui.scrubable_number_at`（:999）でドラッグスクラブ +
クリックでタイプの両対応になっており、BPM 入力（transport）と同じ操作感。これが参照実装。

文字列 / 選択フィールドは数値でない: テキスト本文（:1123）/ フォント名 Font family（:1149）/
Align ドロップダウン（:1181）。

## 確定仕様 (grill-me 2026-06-09)

**インスペクタの全数値フィールドを `scrubable_number_at`（ドラッグ + タイプ両対応）へ統一する**。
参照実装はグループ変形（既に scrubable_number + per-param 感度 + "A" トグルで動作）で、これと
同一 idiom に揃える。

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | テキストクリップ数値 | `emit_num_row` helper（:1222）の `text_input_at` を `scrubable_number_at` へ。**1 箇所の変更で 25 行全部**（X/Y/W/H/Rot/Size/Opacity/Fill+Outline+Shadow RGBA/Fade）がドラッグ対応になる | **daw_01**（本 plan） |
| 2 | 画像クリップ数値 | X/Y/W/H/Opacity/Rotation（:548-773）と Fade In/Out（:828,870）の `text_input_at` を `scrubable_number_at` へ | **daw_01**（本 plan） |
| 3 | オーディオクリップ数値 | Gain/Pan/Pitch（:316,342,368）と Fade In/Out（:404,446）の `text_input_at` を `scrubable_number_at` へ | **daw_01**（本 plan） |
| 4 | edit-buffer 整理 | scrubable_number が編集状態を自前で持つため、画像の名前付き buffer（`clip_image_*_edit_text`）/ テキストの `clip_text_num_edits` マップ / オーディオの各 buffer を撤去できる（既存 idiom 準拠、bespoke buffer を新設しない方針と一致） | **daw_01**（本 plan） |

- オートメーション "A" トグルはグループ変形・画像・テキストと同様に scrubable_number と共存。
- drag 感度は単位に追従（X/Y/W/H/Offset = px、Rotation = 度、Opacity / RGBA = 0–1 の細かい
  ステップ、Gain = dB、Pitch = semitone、Fade = beat）。range / clamp は各 field 現行どおり。
- 文字列（本文・Font family）と Align ドロップダウンは不変。

## 受け入れ基準

- 上記すべての数値フィールド（オーディオ / 画像 / テキストクリップ）が、BPM と同じく**ドラッグで
  値が変わり**、クリックで直接タイプもできる。
- ドラッグ / タイプの開始・終了で undo が 1 ステップに bracket される（グループ変形の既存挙動と
  同じ）。
- 既存の range / clamp・オートメーション "A" トグルが従来どおり機能する。

## 非範囲

- 文字列フィールド（テキスト本文・Font family 名）と Align ドロップダウン。
- グループ変形インスペクタ（既に scrubable_number、参照実装）。
- 新しい数値フィールドの追加。
