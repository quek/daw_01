# plan_group_highlight_remove — グループトラックの色ハイライトを撤去する

FIXME #5。「グループトラックのハイライト表示をなくして他のトラックと同じように
表示してほしい」。

## 現状 (2026-06-08)

グループトラック (= 他 track の `parent_group_id` に指される track) は
**背景色のベタ塗りハイライト**で区別されている:

- **mixer** ([mixer_strips.rs:45-50,220-221](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs)):
  `bg = if entry.is_group { COLOR_GROUP_BG } else { COLOR_STRIP_BG }`。
  `COLOR_GROUP_BG = {0.18, 0.22, 0.30}` (青寄り tint) vs
  `COLOR_STRIP_BG = {0.18, 0.18, 0.22}` (neutral)。
- **arrangement** (gui_01 widget): caller は `parent_id` / `depth` /
  `collapsed` を渡すだけ ([arrangement_view.rs:328-330](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs))、
  widget が `parent_id` 逆引きで `is_group_track` を判定し **背景色を切替える**
  ([gui_01 arrangement.rs:188-192](gui_01:crates/ui/src/widgets/arrangement.rs))。

## 確定仕様 (grill-me 2026-06-08)

**色ハイライトのみ撤去**し、グループトラックも他トラックと同じ neutral 背景で
描画する。「グループであること」の識別は**構造的手掛かり** (階層インデント /
折りたたみ三角 ▶/▼ / 子の括り) だけで担わせる。色のベタ塗りハイライトという
視覚ノイズを廃する。

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | mixer | `COLOR_GROUP_BG` を撤去し group strip も `COLOR_STRIP_BG` に統一。`is_group` 分岐を背景色から外す (他の strip 構成は不変) | **daw_01** (本 plan) |
| 2 | arrangement | group track row の背景着色を外し neutral 背景に。**インデント (depth) / 折りたたみ三角 / collapse 挙動は維持** | **gui_01 #085** |

- 残すもの: インデント (`depth * indent_px`)、disclosure ▶/▼、collapse、
  `track_color_strip` (ユーザー指定トラック色ストライプ) は別概念なので不変。
- 消すもの: グループ判定だけで付く背景 tint (mixer 青 / arrangement グループ行色)。

## gui_01 要望

`docs/gui_01_conversation.md` #085 で arrangement widget のグループ行背景着色を
外す要望を提出 (この plan を関連仕様として参照)。daw_01 側の mixer 修正は
gui_01 landing を待たず独立に実施可能。

## 受け入れ基準

- mixer のグループ strip 背景が通常 strip と同じ neutral 色。
- arrangement のグループトラック行背景が通常トラックと同色。
- インデント・折りたたみ三角・階層構造は引き続き視認でき、グループ関係が
  構造から分かる (= 情報は失わない)。

## 非範囲

- 階層構造そのもの (インデント / disclosure / collapse) の撤廃 (grill で
  「色ハイライトだけ消す・構造手掛かりは残す」を選択)。
- Video-kind トラック背景や selected 背景など group 以外の背景処理は不変。
