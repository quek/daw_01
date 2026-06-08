1. フェーダーとメーターのスケール合わせの続き
2. 2つの Text トラックがあると、両方とも同じテキストが描画される
3. MIDI エディタのルーラが5barから始まるクリップでも常に1barからになっています。
4. 普通のDAWみたいに再生時間を表示したい。
5. グループトラックのハイライト表示をなくして他のトラックと同じように表示してください。
6. 20260512.daw 9bar以降の口以外の画像と動画が表示されない。
7. ミキサーのグループトラックをアレンジメントのように子を折り畳めるようにしてください。

## 計画 (grill-me 2026-06-08 確定)

| # | 確定仕様 / 真因 | 修正箇所 | 参照 |
|---|---|---|---|
| 1 | フェーダ/メータを単一 widget に統一（gui_01 #083 完了 → wire） | daw_01 | `docs/plan_channel_fader_meter.md` |
| 2 | gui_01 effect text が `renderers[0]` 共有で last-prepare-wins。pool 化で修正 | gui_01 | conversation `#084`、`docs/plan_text_overlay.md` |
| 3 | piano roll を song 絶対座標に統一（view 入口 +clip.start_beat / 出口 −） | daw_01 | `docs/plan_pianoroll_song_absolute.md` |
| 4 | トランスポートに bar.beat + 分:秒.ms 併記（SSoT=playhead_beat） | daw_01 | `docs/plan_transport_time.md` |
| 5 | グループの色ハイライトのみ撤去（構造手掛かりは残す） | daw_01(mixer) + gui_01(arrangement) | `docs/plan_group_highlight_remove.md`、conversation `#085` |
| 6 | 単一画像/動画は clip 長=表示長を不変条件に。resize 同期 + load 修復 | daw_01 | `docs/plan_image_clip_length_ssot.md` |
| 7 | ミキサーの group strip を折り畳み可能に（arrangement と `collapsed_groups` 共有・SSoT 1つ） | daw_01 | `docs/plan_mixer_group_collapse.md` |
