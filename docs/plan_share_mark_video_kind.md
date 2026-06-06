# plan: 共有クリップマークを Video-kind トラックの clip にも表示 (gui_01 arrangement widget)

## 背景 / 症状

共有コピー (linked clip = 同一 `content_id` を参照、refcount >= 2) の clip には、アレンジビューで
**share マーク** (リンクグリフ `⇌` + hue 由来のアクセント fill/border) が描かれる。これが
**Text クリップ (および Image クリップ) で表示されない**。

ユーザー報告: 「Text クリップに共有コピーマークが表示されていません」。

## 原因 (gui_01 内で確定済)

- daw_01 は `share_group_color` を **clip 種別に依らず** refcount>=2 で `Some(hue)` に設定している
  (`daw_gui/src/view/arrangement_view.rs:256-266`)。`refcount_by_content` は全 clip を数えるので
  (同 :130-138)、共有 Text クリップでも `share_group_color = Some(hue)` になる。daw_01 側は正しい。
- linked copy 生成 (`clone_clips_linked`, `daw_gui/src/app.rs:12227-`) は **content_id を流用**する
  (clip 種別に依らず共有)。よって共有 Text クリップは実際に refcount>=2 になる。
- 問題は gui_01 の `draw_clip` (`crates/ui/src/widgets/arrangement.rs:2900-2917`):
  ```rust
  if matches!(track_kind, TrackKind::Video) {
      draw_video_clip(hctx, r, clip, style, lanes, selected);
      return;   // ← share_group_color を見ずに return
  }
  ```
  `draw_video_clip` (同 :2845-2898) は **`share_group_color` を完全に無視**する
  (コメント :2843「share_group_color / audio_edit overlay は video clip では描画しない」)。
- daw_01 は **video / image / text clip を持つ track を `TrackKind::Video`** として widget に渡す
  (`arrangement_view.rs:202-213`、row 背景 / thumbnail で視認性を上げるため)。よって Text クリップは
  必ず Video-kind track 上にあり、`draw_video_clip` 経由 → share マークが描かれない。
- **daw_01 側ではクリーンに直せない**: daw_01 が制御できるのは per-track の `kind` のみ。Text/Image を
  持つ track を Audio-kind に落とせば share マークは出るが、video header / row styling を失う
  (mixed track では video clip もある)。per-clip の描画分岐は widget の責務。

## 最終的に欲しい完成形

**`share_group_color = Some(hue)` の clip は、所属トラックの `TrackKind` に依らず share マークを描く。**

具体的には `draw_video_clip` (= Video-kind track の全 clip 経路) でも share クリップを識別可能にする:

1. **thumbnail を持たない clip (Text / PiP image でサムネ未生成 / 非 video visual clip)**:
   audio 経路 (`draw_clip`) と**同じ full 扱い** — hue 由来の fill + border + 名前左のリンクグリフ `⇌`。
   (現状 audio clip と全く同じ見た目で良い。)
2. **thumbnail を持つ clip (実 video)**: thumbnail が clip 面を占有するので full fill は不可。
   **hue 由来の border アクセント + リンクグリフ `⇌`** を描いて共有を識別可能にする (thumbnail は隠さない)。
3. selection は従来どおり最優先 (selected の時は selection 色、リンクグリフは #022 どおり selected でも描く)。
4. リンクグリフぶん名前を右にずらすのも audio 経路と同様 (`draw_clip` の `has_link` 分岐と同じ)。
5. active group 強調 (`in_active_group` の glow/border) も Video-kind clip に同様に適用できるのが理想
   (audio 経路と対称)。

= 「共有マークは content 共有の意味であって track kind と直交する。video 経路でも honor する」。

## 実装の当たり (gui_01 セッション)

- `draw_video_clip` (arrangement.rs:2845) に `share_group_color` 分岐を追加するか、
  `draw_clip` の share-group fill/border/glyph ロジックを共通 helper に括り出して video 経路からも呼ぶ。
- thumbnail 有無で full-fill か border-accent かを切替。リンクグリフ描画は共通化。
- `clip_text_color_for` の auto-contrast は share fill に合わせる (audio 経路と同様)。

## daw_01 側の前提 (変更しない)

- daw_01 は今後も video/image/text clip を持つ track を `TrackKind::Video` で渡す
  (header / row styling のため)。`share_group_color` は clip 種別に依らず refcount>=2 で渡す。
- = daw_01 は無修正。widget が Video-kind clip でも `share_group_color` を honor すれば解決する。

## 確認事項

- 実 video clip (thumbnail 有) の share 表示は border + glyph で十分か、hue の薄い wash を重ねるか
  (thumbnail 視認性とのバランス) は gui_01 の意匠判断に委ねる。
