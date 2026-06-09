理想とベストプラクティスを追求する。
そのためは実装コストは無視して大胆に破壊して作り直す。

## open 項目なし

#16-#23 および follow-up (最上段 rename freeze / Open Recent cascade orphan) は全て解決。
最後の gui_01 依存だった menu_bar cascade orphan は gui_01 Phase 120 (#095) で修正 landing
(`close_orphaned_cascades` 再帰 close、 daw_01 無修正)。実機での最終一括確認待ち。
