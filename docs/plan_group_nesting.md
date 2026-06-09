# plan_group_nesting — グループのグループ化で入れ子を保持する

FIXME #13。「複数のグループトラックをグループ化すると元のグループが解除されて
しまいました」。

## 現状 (2026-06-09)

「グループ化」(Ctrl+G) は `selected_track_ids` をそのまま `GroupSelectedTracks` に渡し
([root.rs:299-305](F:/dev/daw_01/daw_gui/src/view/root.rs))、`action_group_selected_tracks`
([app.rs:9169-9246](F:/dev/daw_01/daw_gui/src/app.rs)) が選択トラック全部 (`child_ids`) の
`parent_group_id` を新規グループへ付け替える
([app.rs:9228-9232](F:/dev/daw_01/daw_gui/src/app.rs))。

トラックヘッダの Shift 範囲選択 / Ctrl 多重選択（gui_01 #016）で**グループとその子トラックを
一緒に選ぶ**と、`child_ids` にグループ自身と子孫が両方入る。結果、子孫まで新グループ直下へ
付け替えられ、内側グループの階層が**平坦化**する（元のグループが空になり解除されたように
見える）。

データモデルは多段ネストを支えている: `Track.parent_group_id`
([common/src/model.rs](F:/dev/daw_01/common/src/model.rs)) の祖先 walk
（`track_visually_silenced` / `ancestor_soloed` 等）は hop-cap 付きで任意段数を扱える。
バグはグループ化**操作**側にある。

## 確定仕様 (grill-me 2026-06-09)

**入れ子を保持する（任意段数）**。既存グループを複数選んでグループ化すると、各グループは
中身を保ったまま「1 つの塊」として新しい親グループに収まる（フォルダの入れ子）。

実装は **selection-root rule**: 選択集合のうち**最上位のトラックだけ**（= 自分の
`parent_group_id` が選択集合に含まれていないトラック）を新グループへ付け替える。子孫
トラックは元の親に残す。これにより:
- 単独トラック選択 → 従来どおり新グループの子になる。
- グループ選択（範囲選択で子孫も含む）→ グループ自身だけが付け替わり、その子孫はグループに
  付いたまま 1 段深くなる。

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | group 化 | `action_group_selected_tracks` で repoint 対象を `child_ids` 全部から **selection root のみ**（`parent_group_id` が選択集合に無いトラック）へ絞る。子孫は元の親に残す | **daw_01**（本 plan） |

- `common_parent`（選択が同一親を共有するなら継承）の既存ロジック
  ([app.rs:9197-9212](F:/dev/daw_01/daw_gui/src/app.rs)) は **root 集合**に対して評価する。
- 挿入位置・選択カーソル更新・`sync_song_to_plugin_host` は不変。
- cycle 安全性: root だけを repoint するので、選択内の親子関係は保たれ循環を作らない。
- 実装時に `AppEvent::GroupSelectedTracks` の doc-comment（[app.rs:2793-2794](F:/dev/daw_01/daw_gui/src/app.rs)、
  旧「全選択を平坦に nest」挙動を記述）も selection-root rule に合わせて更新する。

## 受け入れ基準

- 中身を持つグループを 2 つ選んでグループ化 → 新しい親グループの中に 2 つのグループが
  **それぞれの子トラックを保ったまま**並ぶ（平坦化しない）。
- グループ + 単独トラックの混在選択 → 新グループ直下にそのグループ（中身保持）と単独
  トラックが並ぶ。
- 既存の「単独トラックだけのグループ化」は従来どおり。

## 非範囲

- ネスト段数の上限導入（任意段数を許可）。
- グループ解除（ungroup）側の挙動（`action_ungroup_tracks` は不変）。
- グループの視覚表現（インデント / 折りたたみ三角）— 既存のまま。
