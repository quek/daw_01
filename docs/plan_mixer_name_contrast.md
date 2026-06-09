# plan_mixer_name_contrast — ミキサーのトラック名のコントラストを上げる

FIXME #14。「ミキサーのトラック名のコントラストが低くて読みにくいです」。

## 現状 (2026-06-09)

ミキサーストリップのトラック名は `draw_strip` 内で
([mixer_strips.rs:391-397](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs))
`ui.label_at(.., name, .., if is_master { COLOR_TEXT } else { COLOR_TEXT_DIM })` で
描画され、**非マスタートラックは `COLOR_TEXT_DIM`**（`{0.65,0.68,0.72}` 暗めのグレー、
[mixer_strips.rs:54](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs)）。名前が乗る背景は
`COLOR_STRIP_BG`（定義 [mixer_strips.rs:46](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs) = `{0.18,0.18,0.22}` 暗、strip 背景への適用は :228）で、
暗グレー文字 on 暗背景でコントラストが低い。

## 確定仕様 (grill-me 2026-06-09)

**全トラック名を `COLOR_TEXT`**（`{0.92,0.93,0.96}` 明るい）で描画する。名前はストリップの
主ラベルであり、master / 非 master で輝度を分ける理由がない。名前が乗る背景は無色の暗
ストリップ（トラック色ストライプは別領域）なので、**固定の明色で十分**コントラストが
取れる（gui_01 #060 のような輝度ベース自動コントラスト機構は不要）。

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | mixer | `draw_strip` のトラック名色を `if is_master { COLOR_TEXT } else { COLOR_TEXT_DIM }` から一律 `COLOR_TEXT` へ | **daw_01**（本 plan） |

## 受け入れ基準

- ミキサーの全トラック名（master / 通常 / return / group）が明るい色で、暗背景に対して
  はっきり読める。
- 他の dim 表示（send 宛先名ラベル [mixer_strips.rs:581](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs)、
  sends セクション divider [:547](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs) など `COLOR_TEXT_DIM` を使う他箇所）は不変。

## 非範囲

- トラック名以外の文字色。
- トラック色ストライプ（ユーザー指定色）の扱い。
