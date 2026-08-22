# plan_mixer_wheel_scroll — ミキサーを縦ホイールで横スクロールする

FIXME #12。「ミキサーをマウスホイールで横スクロールするようにしてください」。

## 現状 (2026-06-09)

ミキサーは既に横スクロール対応のレイアウトを持つ:
[mixer_strips.rs:145-151](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs) でトラック
ストリップ群を `ui.scroll_area("mixer_strips", ...)` 内に描画し、master / returns は
scroll_area の外（右端固定、
[mixer_strips.rs:134](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs)）。

しかし `scroll_area` の wheel 処理は
[gui_01 scroll_area.rs:117-118](gui_01:crates/ui/src/widgets/scroll_area.rs):

```
offset.0 (横) ← scroll.0 (wheel X)
offset.1 (縦) ← scroll.1 (wheel Y)
```

plain マウスホイールは Y 成分 (`scroll.1`) しか出さず、ミキサーは縦にあふれない
(`max_y = 0`) ため `offset.1` はクランプで動かない。横 `offset.0` は wheel X
(`scroll.0` = Shift+wheel / トラックパッド水平) でしか動かないので、**plain ホイールで横
スクロールできない**。

## 確定仕様 (grill-me 2026-06-09)

`scroll_area` を **「横だけあふれる領域では plain 縦ホイールを横スクロールに回す」**挙動に
する（標準的な横一列リストの挙動）。汎用的に正しく、ミキサー専用ではなく `scroll_area`
全体の改善。

これは gui_01 側の修正（**gui_01 #089**）。daw_01 側（ミキサーのレイアウト・master 固定）は
既に完成しており変更不要。

軸マッピングの完全な規則:

| 条件 | 縦ホイール (scroll.1) | 横ホイール (scroll.0) |
|---|---|---|
| 縦・横ともあふれ (`need_v && need_h`) | 縦 offset | 横 offset |
| 横だけあふれ (`need_h && !need_v`) | **横 offset**（新規） | 横 offset |
| 縦だけあふれ (`need_v && !need_h`) | 縦 offset | （無し） |

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | scroll_area wheel | `need_h && !need_v` のとき `scroll.1`（縦ホイール）を `offset.0`（横）へ回す。他条件は不変 | **gui_01 #089** |

## gui_01 要望

`docs/gui_01_conversation.md` #089 で `scroll_area` の wheel 軸マッピング拡張を要望（本 plan を
関連仕様として参照）。memory の方針に従い daw_01 側で interim 実装はしない。

## 受け入れ基準

- ミキサー上で plain マウスホイールを回すとトラックストリップが横スクロールする。
- master / returns は右端固定のまま（スクロールしない）。
- 縦にあふれる既存の scroll_area（縦リスト等）は従来どおり縦ホイール = 縦スクロール。

## 非範囲

- ミキサーのレイアウト（既に横スクロール枠 + master 固定で完成）。
- 横スクロール量の細かいチューニング（40px/ノッチ ≒ 半ストリップで可）。
- daw_01 側の interim 実装。
