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

- 自前 `push_rect` / `push_text` / `push_lines` は最終的にゼロを目指す。
- gui_01 に widget が無い領域は **gui_01 側で widget 化する要望を投げる**。daw_01 内で自前実装を温存しない。
- 機能拡張 (drag/resize/marquee 等) は gui_01 widget 化が完了してから、widget API 上で組む。

理由: piano_roll widget 化 (commit 52394b5) で 493 → 320 LOC、cache 統合・Shift+drag 等の挙動共通化のメリットを実証済み。同じパターンを arrangement / plugin_picker にも適用したい。

## 現状インベントリ (2026-05-03 時点)

`daw_gui/src/view/` に残っている自前描画は **31 箇所**。3 カテゴリに分類:

### A. gui_01 既存 widget で置換可 (daw_01 だけで対応)

| 箇所 | 置換先 widget |
|---|---|
| `bottom_panel.rs:24,44` タブストリップ手書き | `Ui::tab_view_with_state` (#004 で推奨パターン受領済) |
| `mixer_strips.rs:188,207` mute/solo hint 帯 | `Ui::checkbox_at` で M/S トグルを表現 |
| `mixer_strips.rs:76` / `track_inspector.rs:67` truncate | `Ui::scroll_area` を活用 |
| `plugin_picker.rs:105-156` リスト truncate | `Ui::scroll_area` を活用 |

### B. gui_01 に widget が無い → 要望が必要

| 領域 | 影響 LOC | 提案 widget |
|---|---|---|
| `arrangement_view.rs` 全体 (canvas/ruler/headers/clips/loop band/playhead) | ~600 | `Ui::arrangement` widget |
| `piano_roll_view.rs:229-320` velocity lane + playhead | ~90 | `piano_roll` widget に `velocity_lane` + `playhead_beat` オプション拡張 |
| `plugin_picker.rs` 全体 (overlay + panel + list) | ~170 | `Ui::modal` + `Ui::list_view` (仮想スクロール対応) |
| 各 view の背景塗り (12 箇所) | 軽微 | `Ui::panel(rect, fill, radius)` helper |

### C. 機能未実装 (widget が来てから手をつける)

- arrangement: drag move / resize / marquee select / track rename UI / loop band drag
- track_inspector: chain reorder (drag で並べ替え)
- mixer / inspector / plugin_picker の scroll 動作確認

## Phase 0: gui_01 への要望投稿 (immediate / blocking)

`docs/gui_01_conversation.md` に以下のエントリを追加。番号は既存 archive (#001-#004) の続きで #005 から。

### #005 [要望] `Ui::arrangement` widget の新設

- piano_roll widget と同等粒度の "1 枚で完結する" widget。Track / Clip / loop / playhead / ruler / track headers を構造で渡し、Edit を返す。
- 入力: drag move / resize / marquee select / wheel zoom & scroll / clip dbl-click / track header クリック。
- daw_01 側 reference: `daw_gui/src/view/arrangement_view.rs` 全文を提示し「これを置き換えたい」と明示。
- API イメージ案を 1 つ提示 (`piano_roll` の API と並びを揃える)。

### #006 [要望] `piano_roll` widget の velocity lane + playhead 内蔵オプション

- 現状 daw_01 側で `draw_velocity_lane` (piano_roll_view.rs:229) と `draw_playhead` (:280) を自前で持っている。
- 要望: `PianoRollStyle` (or `PianoRollView`) に `velocity_lane_h: f32` (0 で disabled) と `playhead_beat: Option<f64>` を追加し、widget 内で描画してほしい。
- 利点: piano_roll widget 1 呼び出しで完結、view 側ロジックが空白 dbl-click + wheel handler のみに縮退。

### #007 [要望] `Ui::modal` + `Ui::list_view` widget

- plugin_picker.rs を完全 widget 化したい。
- `modal(id, screen, panel_size, |panel_rect, ui| { ... })` で半透明オーバーレイ + 中央 panel + Esc / 外側クリックで close を一括化。
- `list_view(id, rect, &items, &selected, |row_ui, item, idx| { ... })` で行描画 + 仮想スクロール + キーボード上下移動 + 選択ハイライト。
- daw_01 側に「Open File」「Save As」等の dialog も増えるので、modal は今後の Save / Open / Export ダイアログでも再利用予定。

### #008 [質問] `Ui::panel(rect, fill, radius)` helper を入れる意義

- 各 view の背景塗りで `ui.heavy(... |hctx| hctx.cached(... hctx.push_rect(...)))` を 12 箇所書いている。
- 単純 1 段の filled rect なら `ui.panel(rect, fill, radius)` 1 行で済む helper があると "raw push_rect ゼロ" を達成しやすい。
- gui_01 側のポリシー (薄い helper を増やす vs heavy で抽象化させる) を確認したい。要らなければ daw_01 側 helper でも可。

### #009 [要望] `Ui::checkbox_at` を mute/solo の表現に使う

- 現状 `button_at("M") + 色帯 push_rect` でトグルの ON/OFF を表現している。
- gui_01 の `checkbox_at` を使えば 1 呼び出しで済むが、ラベル"M"/"S"の見た目を維持したい。
- 質問: `CheckboxStyle` に `label_override: Option<&str>` 等で「□ の代わりに任意ラベル + 背景色変化」を許容できるか。難しければ daw_01 で button + heavy push_rect を続ける。

**Phase 0 完了条件**: 上記 5 件を `gui_01_conversation.md` に投稿し、gui_01 から `[Replied]` を受領。

## Phase 1: ローカル置換 (Phase 0 と並行)

gui_01 回答を待つ間、既存 widget で完結するものを片付ける。各タスクは個別 commit。

### 1-1. `bottom_panel.rs` を `tab_view_with_state` 化

- archive #004 の sample を参考にタブ index `&mut u8 → &mut usize` 変換で書き換え。
- mixer / piano_roll の中身 closure はそのまま維持。
- 期待: ファイルが ~98 LOC → ~60 LOC、自前 `push_rect` x2 削除。

### 1-2. `plugin_picker.rs` のリストを `scroll_area` 化 (modal 化は #007 待ち)

- まず scroll_area で行 truncate を解消 (現状 `max_rows` で打ち切り、line 105-156)。
- modal 化 (#007 受理後) で全面書き換え予定。先に scroll_area だけでも導入。

### 1-3. `mixer_strips.rs` / `track_inspector.rs` の scroll 対応

- mixer: 横方向 scroll_area (master strip は scroll 外に固定したい)。
- inspector: 縦方向 scroll_area (chain entry が画面に入りきらないとき)。

### 1-4. `mixer_strips.rs` mute/solo を `checkbox_at` 化

- #009 の回答次第。受理 → checkbox_at 化、保留 → 現状維持。

## Phase 2: arrangement widget 大改造 (#005 受理後)

`docs/plan_arrangement_widget_rewrite.md` に詳細計画を起こす (piano_roll の `plan_piano_roll_widget_rewrite.md` と同じ構成)。

- gui_01 側で widget が完成したら、daw_01 の `arrangement_view.rs` 614 LOC → ~150 LOC へ縮約。
- 自前 `handle_canvas_input` (line 516-614) も widget 内蔵入力で代替、daw_01 側は SelectClip / SelectBottomPanel(1) に変換するだけ。
- track_headers (line 315-512) も widget 内に移管 → mute/solo button が widget 内 callback。

## Phase 3: 機能拡張 (Phase 2 後)

arrangement widget 内蔵で実装される想定 (gui_01 側で組む):

- clip drag move / resize
- marquee (rect) select
- track rename inline (header の名前を text_input に切替)
- loop band drag (現状は ruler ドラッグで範囲設定だけ可、コミット 74246c8)

daw_01 側で組むもの:
- track_inspector chain reorder (drag で MIDI FX / Inst / FX 内の並び替え)。`Ui::list_view` (#007) が drag-reorder をサポートするなら そちらに乗る。

## Phase 4: 仕上げ

- `daw_gui/src/view/` 全体で `push_rect` / `push_text` / `push_lines` が **0 件** であることを grep で確認。
- ない場合、残った箇所の事情を本ファイルに記録 (gui_01 側で widget 化が困難な特殊用途等)。
- `cargo build --workspace` warning 0 / `cargo clippy -- -D warnings` クリーン。
- `cargo run -p daw_gui` で全画面の操作確認 (transport / arrangement / piano_roll / mixer / inspector / plugin_picker / dialog)。

## 並行で進める無関係タスク (このプランの外)

- VST3 サポート (M2)
- VOICEVOX 合成パイプラインの最適化
- Audio Engine 側のチューニング

これらは widget 化と独立、必要になったら別途 plan ファイルを起こす。

## 進捗ログ

| 日付 | commit | Phase | 内容 |
|---|---|---|---|
| 2026-05-03 | (本ファイル作成) | - | plan.md 初版 |
