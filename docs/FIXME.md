理想とベストプラクティスを追求する。
そのためは実装コストは無視して大胆に破壊して作り直す。

## gui_01 landing 待ち (daw_01 側は配線・提出済、 parked)

- #16 アレンジ track header 幅の drag リサイズ — gui_01 へ `ArrangementEditRequest::SetHeaderW`
  の request 提出済 (`docs/gui_01_conversation.md` / `docs/plan_arrange_header_width.md`)。
  daw_01 consumer (`AppData.arrange_header_w` + `SetArrangeHeaderW` handler + `TRACK_HEADER_W`
  定数撤去) は配線済。landing 後 `make_edit` に 1 arm 足すだけ。
- #18 group track 名 double-click rename — 深ネスト group で名前 hit 矩形が潰れて rename が
  始まらない件、 gui_01 へ修正 request 提出済 (`docs/plan_track_rename_dblclick.md`)。
  保険の **F2 で track rename** は daw_01 側で対応済。
- #20 piano roll 鍵盤オクターブラベル (C5 / root) の可読性 — label 色が key fill ではなく
  dark keyboard_bg 想定で調色されており warm cream 背景に潰れる。 Fold は白鍵/黒鍵跨ぎで
  単一色不可。 gui_01 へ WCAG auto-contrast 適用 request 提出済
  (`docs/plan_pianoroll_label_contrast.md`)。 daw_01 は static override 撤去・default に戻し済。
