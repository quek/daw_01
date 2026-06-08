8. クリップ色をトラックに揃えるやクリップを右クリックしての色でクリック色が変わらない。
9. カラーピッカでドラッグするしその下のクリップをドラッグしてしまう。

## 計画 (grill-me 2026-06-08 確定)

| # | 確定仕様 / 真因 | 修正箇所 | 参照 |
|---|---|---|---|
| 8 | (a) 共有 clip で widget が hue を `color` に優先 → gui_01 #086 で fill を `clip.color` 一本に。(b) `SetClipColor` が 1 clip しか塗らない → handler で **content_id 共有先へ伝播** (= 共有クリップ着色は全 member へ)。「トラックに揃える」は **track-scoped のまま** (他 track の共有 clip は不変)。色は per-clip `Clip.color` 維持 (content 移設は確定動作 2 と衝突し却下) | daw_01(app handler) + gui_01 #086 | `docs/plan_track_clip_color.md` 追加要件、conversation `#086` |
| 9 | `color_picker` が非 capturing modal で、ドラッグ press を背景 arrangement が先取りし下の clip を drag。widget を #065 の真モーダル (capture_input=true) で開く | gui_01 #087 | `docs/plan_track_clip_color.md` 追加要件、conversation `#087` |
