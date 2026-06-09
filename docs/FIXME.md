理想とベストプラクティスを追求する。
そのためは実装コストは無視して大胆に破壊して作り直す。

## gui_01 landing 待ち (重大バグ、 daw_01 回避策なし)

- menu_bar の cascade (sub_menu) item click 後に **cascade sub-popup が閉じず孤立** し、
  画面左上 anchor `(0,72,360,192)` の入力を遮断する。File > Open Recent / Recently Saved から
  プロジェクトを開くと、 その後アレンジ上部 ~1/3 のトラックを double-click で rename できなく
  なる (実機 `20260512.daw`)。真因は `menu.rs:~580` が top menu popup のみ close し cascade を
  残すこと。gui_01 へ修正 request 提出済 (`docs/plan_menu_cascade_close.md` /
  `gui_01_conversation.md`)。daw_01 から cascade popup を close できないため回避策なし。
  当面の手動回避: トラックヘッダを広げ、 名前の右側 (anchor 外) を double-click。
