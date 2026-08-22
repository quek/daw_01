# plan: group track 名 double-click rename の信頼性 (FIXME #18)

## 症状 / 最終形態

トラック名をダブルクリックすると inline rename が始まる仕様だが、**group track
(子を持つ track) では rename が始まらない場合がある** (特定プロジェクト
`20260513.20260512.daw` の Group22 で再現)。

最終形態: **どの track でも** (通常 track / 浅い group / 深くネストした group いずれも)、
track header の名前部分を double-click すると確実に inline rename が始まる。

## 原因 (一次情報)

`crates/ui/src/widgets/arrangement.rs` で group track のとき名前の hit 矩形
`name_rect_visible` を disclosure (▶/▼) 分だけ削り、さらに `.max(2.0)` で下限クランプ
している:

```rust
// arrangement.rs (track header 描画ループ内)
let name_rect_visible = if is_group {
    Rect {
        x: disclosure_rect.x + disclosure_rect.w,
        y: name_rect.y,
        w: (name_rect.w - disclosure_rect.w).max(2.0),  // ← 深いネストで 2px に潰れる
        h: name_rect.h,
    }
} else {
    name_rect
};
...
if self.take_double_click_in_rect(name_rect_visible).is_some() {
    self.push_edit(make_edit(ArrangementEditRequest::BeginRenameTrack(track_id)));
}
```

深くネストした group は `header_x = rect.x + depth * indent_px` で名前開始 x が右に寄り、
M/S/R ボタンが右を占有するため `name_rect.w - disclosure_rect.w` が極小 (〜2px) になり、
double-click が当たる面積がほぼ消える。結果 rename が始まらない。

## gui_01 への依頼

「group track でも double-click rename が確実に始まる」end state を満たす実装を希望
(内部手法は gui_01 判断で可)。候補:

- **A. name hit 矩形を潰さない**: disclosure を名前 hit 領域から除外しても最低幅を
  実用値 (例: track 名 font 数文字分) 確保する。深ネスト時は #16 (header 幅可変) /
  #17 (indent 半減) と併せて名前領域を確保。
- **B. double-click を近接判定にフォールバック**: `take_double_click_in_rect` で rect
  内厳密判定に加え、直近 click との距離 (既存 threshold) が満たされれば rect 判定を
  緩める。
- **C. disclosure グリフを名前 click 領域に含めない代わりに、名前帯全体 (disclosure
  含む) を double-click rename の対象にする** (disclosure の single-click 折り畳みとは
  別ジェスチャなので両立可能)。

いずれでも、通常 track の現行挙動は不変であること。

## daw_01 側 (実装済)

double-click が効かない場面の保険として **F2 キーで track rename を開始** できるよう
daw_01 側で対応済 (`daw.rename_clip` shortcut を文脈分岐: clip 選択中は clip rename、
clip 未選択時は `cursor_track_index` の track を rename)。double-click 修正後も F2 は併存。

## 検証 (landing 後)

- `20260513.20260512.daw` の Group22 を含む各深さの group で double-click rename 起動。
- 通常 track / 浅い group の double-click rename が回帰していない。
- disclosure ▶/▼ の single-click 折り畳みが double-click rename と競合しない。

## Follow-up #2 (2026-06-09、 gui_01 #092 landing 後も残存)

#092 (深ネスト group の hit-zone 拡張) landing 後も、 `20260512.daw` で **master row 直下の
最初の実 track (`visible_tracks[1]`) だけ double-click rename が効かない**。daw_01 の `make_edit`
受信トレース (`BeginRenameTrack`) で再現確認:

- `visible_tracks[2]` (Inst 25、 group 27 の子) を double-click → **emit される** (rename 正常)。
- `visible_tracks[1]` (group 27、 master 直下) を double-click → **emit されない**。

→ widget 側 `visible_tracks[1]` の rename double-click hit-test が成立していない (daw_01 受信側は
正常、 F2 でも rename できる)。 gui_01 へ #092 follow-up として再提出済
(`docs/gui_01_conversation.md`)。 master row 隣接の `visible_tracks[1]` 特有の問題で、
先行する `take_double_click_in_rect(lanes)` の消費 / header loop の空振りが候補。

### daw_01 側で別途修正したもの (本件とは別の確定バグ)
rename 状態を index で持っていた `track_rename_idx` を **安定 ID `track_rename_id`** に変更
(commit 39fc0b0)。reorder/delete で rename が別 track にすり替わり「最上段に rename 枠が居座って
フリーズ」 する SSoT 違反を修正。これは daw_01 単独で解決済。double-click が emit されない
本件 (gui_01) とは別問題。
