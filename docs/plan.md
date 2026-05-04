# daw_01 master plan

このファイルは daw_01 の中期作業計画。`/clear` 後でもこの 1 枚を読めば「次に何をやるか」が分かるように維持する。

## 運用ルール

- このファイル = master plan。日々の作業順を決める起点。
- 大きい個別タスクは `docs/plan_<feature>.md` に切り出し、本ファイルからリンク。
- 各 Phase が完了したら本ファイル末尾の「進捗ログ」に commit hash + 1 行サマリを追記。
- gui_01 への要望は `docs/gui_01_conversation.md` にエントリ追加 → 返信受領 → 対応完了で `_archive_NNN.md` へ。
- スコープが変わったらこのファイルを書き換える。古い計画を残して別 phase を増やすより、上書きで always-current を保つ。

## 大方針

**「daw_01 の UI 描画は全て gui_01 widget で構築する」** を目標にする。

- 自前 `push_rect` / `push_text` / `push_lines` は最終的にゼロ。
- gui_01 に widget が無い領域は **gui_01 側で widget 化を要望**、daw_01 内で自前実装を温存しない。
- 機能拡張 (drag/resize/marquee 等) は gui_01 widget 化が完了してから widget API 上で組む。

理由: piano_roll widget 化 (commit 52394b5) で 493 → 320 LOC、cache 統合・Shift+drag 等の挙動共通化のメリットを実証済み。同じパターンを arrangement / plugin_picker にも適用したい。

## gui_01 側ロードマップ (#005-#010 全て受領済 / Phase 45a-45g merge 済)

詳細 API は `docs/gui_01_conversation.md` 参照。実装順:

| gui_01 phase | 内容 | daw_01 側の取り込み作業 | status |
|---|---|---|---|
| **45a** | `Ui::panel` + `Ui::panel_with_border` (#008) | 12 箇所の背景塗りを置換 | merged (gui_01) |
| **45b** | `Ui::toggle_button_at` (#009) | mixer の M/S を置換 | merged (gui_01) |
| **45c** | `PianoRollView` に `velocity_lane_h` + `playhead_beat` 追加 (breaking、#006) | PianoRollView 構築箇所更新、`draw_velocity_lane` / `draw_playhead` を削除 | **取り込み完了 (8aebba3)** |
| **45d** | `Ui::modal` + `Ui::list_view` (#007) | plugin_picker.rs を rewrite (171 → ~80 LOC) | merged (gui_01) |
| **45e** | `Ui::arrangement` widget (#005、4 sub-phase A-D) | arrangement_view.rs を rewrite (614 → ~150 LOC) | merged (gui_01) |
| **45g** | `scroll_area` thumb drag 修正 (#010) | path 依存先で自動反映、追加コードなし | **対応完了 (再ビルドで解消)** |

## Phase 1: ローカル置換 (gui_01 merge 不要)

既存 gui_01 widget だけで完結する作業。各タスクは個別 commit。

### 1-1. `bottom_panel.rs` を `tab_view_with_state` 化 [done c46df37]

98 → 49 LOC、自前 `push_rect` x2 + 定数 4 個削除。

### 1-2. `plugin_picker.rs` のリストを `scroll_area` 化 [done f280274]

`max_rows` 手動 truncation を廃止、scroll で全件表示。
注意: 行背景の `heavy + cached(i)` は scroll で stale rect を replay するので `push_rect` 直呼びにする。最終的に 45d (`Ui::list_view`) で消える tech debt。

### 1-3. `mixer_strips.rs` / `track_inspector.rs` の scroll 対応 [done]

- mixer: 横方向 scroll_area、master strip は scroll 外に右端固定。
- inspector: 縦方向 scroll_area、+Inst/+FX/+MIDI ボタンは scroll 外に下端固定。
- 注意: scrollbar drag 不能のバグを発見、gui_01 #010 で報告。wheel/keyboard scroll は動作する。

## Phase 2: arrangement widget 用の schema 移行 (Phase 1 と並列)

gui_01 #005 回答で確定: arrangement widget は **clip_id / track_id を index ではなく安定 ID で受ける** 設計。daw_01 側の schema を先に整えると 45e merge 直後すぐ widget に乗せられる。

詳細は `docs/plan_arrangement_widget_rewrite.md` を起こして展開予定だが、最低限の作業:

1. `Clip` schema に `id: u32` フィールド追加
2. `Track` に `next_clip_id: u32` 採番ロジックを追加 (clip 作成時に bump)
3. `Track` に `id: u32` フィールド追加
4. `Song` に `next_track_id: u32` 採番ロジックを追加
5. `ClipRef.clip` の意味を index → clip_id に切替 (型は `u32` のまま意味だけ変える)
6. クリップ参照箇所 (handle_event 内の `selected_clip` lookup 等) を index 検索 → id 検索に書き換え

bincode encode/decode と Song の新規作成・読み込み・autosave への影響を検証。

着手前に `docs/plan_arrangement_widget_rewrite.md` を作成し、この移行を 1 段階目として詳細計画する。

## Phase 3: gui_01 widget 取り込み (45a-45e merge トリガ)

gui_01 phase が merge されたら順に取り込む。

### 3a. `Ui::panel` 取り込み (45a 後)

12 箇所の `ui.heavy(... |hctx| hctx.cached(... hctx.push_rect(...)))` を `ui.panel(id, rect, fill, radius)` に置換。border 付き 1 件 (file drop hover) は `panel_with_border`。

※ 12 箇所のうち plugin_picker (45d 化で消える) と arrangement clip 矩形 (45e 化で消える) を除けば、実質置換対象は 9 箇所。

### 3b. `Ui::toggle_button_at` 取り込み (45b 後)

`mixer_strips.rs:164-222` の M/S を置換。`button_at + 自前 hint band push_rect` の 2 段構えが 1 呼び出しに。
※ `arrangement_view.rs` 側の M/S は 3e (arrangement widget) で消えるので置換対象は mixer のみ。

### 3c. piano_roll velocity / playhead 削除 (45c 後)

`PianoRollView` 構築箇所 (`piano_roll_view.rs:76-83`) に `velocity_lane_h: 60.0` / `playhead_beat: app.playhead_beat.map(|b| b as f64)` を追加。
`draw_velocity_lane` / `draw_playhead` 関数 (90 LOC) を削除。view ファイルが 320 → ~230 LOC。

### 3d. `plugin_picker.rs` を modal + list_view で rewrite (45d 後)

171 → ~80 LOC を目標に全面書き換え。conversation #007 の rewrite サンプル (line 565-577) をベース。

### 3e. arrangement_view.rs を `Ui::arrangement` で rewrite (45e 後)

Phase 2 で schema 移行済が前提。`docs/plan_arrangement_widget_rewrite.md` で詳細展開。
gui_01 側 sub-phase に対応:
- 45e-A merge → `draw_canvas` 相当を置換 (-210 LOC 見込み)
- 45e-B merge → drag move / resize / rect select の自前実装削除
- 45e-C merge → loop band drag を widget callback に
- 45e-D merge → `draw_track_headers` を置換 (-200 LOC 見込み)、context_menu と rename UI は daw_01 側で `track_header_rects` を使って外側に書く

最終的に arrangement_view.rs 614 → ~150 LOC。

## Phase 4: arrangement-driven 機能拡張 (Phase 3e 後)

widget 内蔵で動くもの (3e で同時に有効化):
- clip drag move / resize
- marquee (rect) select
- loop band drag (現状は ruler ドラッグでの範囲設定のみ、commit 74246c8)
- track rename UI (BeginRenameTrack Edit を受けて daw_01 側で text_input に切替)

daw_01 側で別途組むもの:
- track_inspector chain reorder。45d 時点で `Ui::list_view` には drag-reorder 無し (gui_01 #007 回答)。将来 `Ui::reorderable_list` が来たら乗る、来ない間は drag-handle button + Edit でも可。

## Phase 5: 仕上げ

- `daw_gui/src/view/` 全体で `push_rect` / `push_text` / `push_lines` が **0 件** であることを grep で確認。
  - **2026-05-04 時点**: `track_inspector.rs:77` の chain row 背景 1 件のみ残存。
    scroll_area 内で row_y が変動するため heavy+cached が使えず ui.push_rect 直呼び。
    将来 `Ui::reorderable_list` (gui_01) が来れば消える tech debt。
- `cargo build --workspace` warning **0 件** (Phase 5 で清掃済)。
- `cargo clippy --workspace -- -D warnings` **クリーン** (Phase 5 で 4 件修正)。
- `cargo run -p daw_gui` で全画面の操作確認 (transport / arrangement / piano_roll / mixer / inspector / plugin_picker / modal)。

## 並行で進める無関係タスク (このプランの外)

- VST3 サポート (M2)
- VOICEVOX 合成パイプラインの最適化
- Audio Engine 側のチューニング

必要になったら別途 plan ファイルを起こす。

## 進捗ログ

| 日付 | commit | Phase | 内容 |
|---|---|---|---|
| 2026-05-03 | (plan.md 初版) | - | master plan 作成 |
| 2026-05-03 | 2625255 | Phase 0 | 要望 #005-#009 を gui_01_conversation.md に投稿 |
| 2026-05-03 | c46df37 | Phase 1-1 | bottom_panel を `tab_view_with_state` 化 (98 → 49 LOC) |
| 2026-05-03 | f280274 | Phase 1-2 | plugin_picker のリストを `scroll_area` 化 (truncation 廃止) |
| 2026-05-03 | (gui_01) | Phase 0 | gui_01 から #005-#009 全件 [Replied] 受領、API 確定 |
| 2026-05-03 | 2d6d70e | Phase 1-3 | mixer/inspector を scroll_area 化、scrollbar drag bug を gui_01 #010 で報告 |
| 2026-05-04 | (gui_01) | Phase 0 | gui_01 から #006 (Phase 45c API) と #010 (Phase 45g 修正済) の返信受領 |
| 2026-05-04 | 8aebba3 | Phase 3c | piano_roll の velocity / playhead を gui_01 widget 内蔵に移譲 (#006 解決、320 → ~210 LOC) |
| 2026-05-04 | ad26af5 | Phase 0 | gui_01 #006 / #010 を Resolved 化、archive_001.md へ移動 |
| 2026-05-04 | 51ff532 | Phase 3a | 背景塗りを Ui::panel / panel_with_border に置換 (10 箇所、heavy+cached boilerplate を 1 行化) |
| 2026-05-04 | bbda445 | Phase 3b | mixer の M/S を Ui::toggle_button_at に置換 (button + 自前 hint band の 2 段構えを 1 呼び出しに) |
| 2026-05-04 | 59c93ec | Phase 3d | plugin_picker.rs を Ui::modal + Ui::list_view で rewrite (167 → 145 LOC) |
| 2026-05-04 | 73e3504 | Phase 2  | Track / Clip に stable id を追加 (next_*_id 採番、ensure_ids、CURRENT_VERSION 2→3) |
| 2026-05-04 | eda8954 | Phase 3e | arrangement_view を Ui::arrangement widget で rewrite (614 → 322 LOC、id ↔ index 変換層) |
| 2026-05-04 | b7b9def | M10 取り込み | gui_01 #010 (Phase 46-48 + 47b/c) build 追従 + #011 (UX 非対称 2 件) を gui_01 に相談 |
| 2026-05-04 | (gui_01) | -            | gui_01 が #011 に Phase 49 (volume live update) + Phase 50 (reorder optimistic preview) で対応、daw_01 側は make_edit が fn 自由関数なので追従不要 |
