理想とベストプラクティスを追求する。
そのためは実装コストは無視して大胆に破壊して作り直す。

## gui_01 landing 待ち (daw_01 側は対応済、 parked)

- #18 follow-up: master row 直下の最初の実 track (`visible_tracks[1]`) だけ double-click
  rename が効かない (`20260512.daw` で再現)。daw_01 トレースで「widget が `BeginRenameTrack`
  を emit していない」ことを確認済 = gui_01 の hit-test 残存バグ。#092 follow-up として再提出
  (`docs/plan_track_rename_dblclick.md` / `gui_01_conversation.md`)。
  daw_01 側は無修正で受ける。当面の回避は **F2 rename**（対象 track を選択して F2）。
  ※ 別途、 rename 状態の index→安定 ID 化 (`track_rename_id`) は daw_01 単独で修正済
  (reorder/delete で rename がすり替わりフリーズする件、 commit 39fc0b0)。
