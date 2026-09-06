# 時間範囲操作 — Live §6.11 "…Time" コマンド群

範囲選択 (`docs/plan_range_selection.md`) の **時間そのもの** を全トラック縦断で動かす
5 コマンド。plan_range_selection §8 で「今回は入れない」としていたものを入れる。

一次情報: Ableton Live 12 Reference Manual §6.11 "Using the …Time Commands"
(<https://www.ableton.com/en/live-manual/12/arrangement-view/>)。要点:
「全トラックに効く」「Paste Time / Insert Silence は insert marker に入る」
「範囲内の拍子マーカーも一緒に動く」。

## 1. 確定した仕様 (2026-09-06)

| コマンド | キー (Live と同じ) | 挙動 |
|---|---|---|
| 時間をカット (Cut Time) | `Ctrl+Shift+X` | 範囲の時間ごとの写しを clipboard へ載せてから、範囲の時間を全トラックから取り除いて詰める |
| 時間を貼り付け (Paste Time) | `Ctrl+Shift+V` | clipboard の写しを **範囲選択の先頭** に時間ごと差し込む (以降を押し出す)。貼った時間が新しい範囲選択になる |
| 時間を複製 (Duplicate Time) | `Ctrl+Shift+D` | 範囲を直後に時間ごと複製する。範囲選択は複製先へ移る (連打で繰り返せる) |
| 時間を削除 (Delete Time) | `Ctrl+Shift+Delete` | 範囲の時間を全トラックから取り除いて詰める。範囲選択は消える |
| 無音を挿入 (Insert Silence) | `Ctrl+I` | **範囲の先頭に範囲の長さぶん** の空き時間を差し込む (ダイアログ無し)。範囲選択はそのまま (= 差し込んだ空き時間) |

- **スコープは全トラック** (Live §6.11)。範囲選択のレーン集合は見ない — 時間区間だけを見る。
  clip / automation clip (トラック + master の song lane) / scale 変化点 / セクション帯 /
  `length_beats` / 再生ループ範囲 (`edit_song_rippling`) が一緒に動く。
- **差し込み位置は範囲選択の先頭** (daw_01 は insert marker を持たない — plan_range_selection
  §2.3。範囲が無ければ status に理由を出して何もしない)。
- **Cut Time が載せる clipboard** は `ClipboardPayload::Time(TimeRangeCopy)`: 範囲の長さ +
  全トラックの clip / automation clip (トラック id / レーン id で宛先を引く) + content の写し +
  scale 変化点 + 範囲に完全に入る帯。素の `Ctrl+V` に載っていても Paste Time として貼る。
  同じプロジェクトなら content を共有 (linked)、無ければ写しから作る。宛先が無い中身は落とす。
- **セクション帯** (Studio One 流): 削除範囲に完全に入る帯は消え、またぐ帯は重なりぶん縮む。
  挿入点をまたぐ帯は挿入ぶん伸びる。複製 / 貼り付けで運ぶ帯は範囲に完全に入っていたものだけ。
- **境界をまたぐクリップ**は境界で窓を割る (`split_clips_at`、content は触らない)。
  挿入点をまたぐクリップも割れて右半分が押し出される (Live と同じ)。

## 2. 実装

- `common/src/model/time_ops.rs` — `Song::copy_time_range` / `delete_time_range` /
  `insert_time` / `paste_time_range` / `duplicate_time_range` と `TimeRangeCopy`。
  Arranger の `delete_section_range` / `duplicate_section` もここを通る (時間を動かす規則は 1 本)。
- `daw_gui/src/handler/range_ops.rs` — `cut_time` / `delete_time` / `duplicate_time` /
  `insert_silence` / `paste_time` (`edit_song_rippling` でループ範囲も追従)。
- 入口: `view/shortcuts.rs` (5 本) / `view/root.rs` / `view/clipboard_ops.rs::paste_time` /
  Edit メニュー (`view/menu_bar.rs`)。Paste Time は `Ui::read_clipboard_text` (on-demand 読み) を使う —
  paste 先読みは "paste" ショートカットにしか付かないため daw-ui に足した。
- テスト: `common/src/model/tests.rs` の `delete_time_range_*` / `insert_time_*` /
  `duplicate_time_range_*` / `paste_time_range_*`。
