<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_progress_streaming — ロードのストリーミング化 + 保存/再スキャンのノンブロック進捗

FIXME #24「プロジェクトロード中などプログレスバーを表示」。grill-me（2026-06-10）で理想 =
「**長い処理で GUI を固めない**」を据え、ロード=ストリーミング / 保存=snapshot 背景 /
再スキャン=背景、と詰めた（VOICEVOX 合成は対象外）。

## 現状 (2026-06-10)

- **load** `action_open_path`（[app.rs:5637-5706](F:/dev/daw_01/daw_gui/src/app.rs)）:
  `project::load`（[project.rs:52-93](F:/dev/daw_01/common/src/project.rs)）→ `self.song` を丸ごと差し替え。
  `decode_audio_sources_into_cache`（[app.rs:5499-5558](F:/dev/daw_01/daw_gui/src/app.rs)）/
  `decode_image_sources_into_cache`（[:5567-5635](F:/dev/daw_01/daw_gui/src/app.rs)）を
  **GUI スレッドで同期実行 → UI が固まる**。plugin 復元は async IPC
  （`restore_plugin_from_song` :5652）。daw_audio は `LoadSong` IPC で自前に schedule compile
  （[daw_audio/src/main.rs:141-162](F:/dev/daw_01/daw_audio/src/main.rs)）。
- **save** `begin_save`（[app.rs:6341](F:/dev/daw_01/daw_gui/src/app.rs)）:
  plugin あれば `RequestAllStates` → states 到着で `apply_plugin_states`
  （[:6273-6322](F:/dev/daw_01/daw_gui/src/app.rs)、track id + slot index で対応・tolerant）
  → `save_after_states` → `save_to` → `project::save(&self.song)`
  （[:6397](F:/dev/daw_01/daw_gui/src/app.rs)、**生 song を完了時に直列化**）。
  全工程が GUI スレッド単一なので race は無いが、states 待ちの間に **plugin を並べ替え**ると
  state を別スロットに誤適用しうる窓がある（普通の編集は安全）。
- **rescan** `begin_rescan`（[app.rs:14227-14266](F:/dev/daw_01/daw_gui/src/app.rs)）:
  既に background thread、`is_rescanning` フラグのみで進捗 UI 無し。
- 既存進捗 UI: `export_overlay.rs`（`export_progress: Option<(u64,u64)>`、background thread から
  `AppEvent::ExportProgress` を `EventLoopProxy` で送る）。modal widget は gui_01
  `crates/ui/src/widgets/modal.rs`。

## 確定仕様 (grill-me 2026-06-10) — 見える挙動

- **全画面ブロックは無し**（理想: 固めない）。
- **ロード = ストリーミング**:
  - 構造（トラック / クリップ / ノート）は**即操作可**（スクロール・編集）。
  - 波形・画像・プラグインは**背景で充填**。**全体バー（隅/上部）＋未準備クリップは淡色プレースホルダ**、
    未準備トラックに印。
  - **再生は音声準備完了までグレー ＋「音声を準備中」**。
- **保存 = ノンブロック**:
  - 押した瞬間に song を **snapshot（clone）** → 背景書き出し → 隅にインジケータ。
  - 待機中の編集は**今回の保存に入らず次の保存へ**。snapshot に plugin states を適用して直列化する
    ので、**plugin 並べ替えの誤適用窓が消える**。
  - saved baseline = snapshot、`is_dirty = (live != snapshot)`。
- **再スキャン = ノンブロック**（既に背景）: 隅 or picker に進捗。
- 進捗は全て **determinate**（件数で算出可能: audio N + image M + plugin P / 検出 plugin 数）。

## 実装メモ

- **asset デコードを background thread 化**（`std::thread` + `EventLoopProxy`。background 規約に従い
  `tokio::time::sleep` 不可、`std::thread::sleep`）。`AppEvent::LoadProgress { done, total }` +
  per-source 完了通知でキャッシュ充填 → 該当クリップの波形 / 画像を順次描画。
  - **playback gating**: 音声準備（daw_audio の `LoadSong` 反映 + 全 audio source デコード）完了まで
    Play を無効化。完了通知で解除。
  - 構造差し替え（`self.song` swap、選択クリア、トラックメタ再構築）は従来どおり即時。重い decode のみ
    background へ逃がす。
- **save snapshot-at-invoke**: `begin_save` で `self.song.clone()` を snapshot 化し、
  `PendingStateRequest::Save { path, snapshot }` に持たせる。`apply_plugin_states` を snapshot に適用、
  `save_to(snapshot)`。書き出し成功で baseline = snapshot、`is_dirty` 再計算。
- **rescan 進捗**: `scan_system`（[plugin_db.rs](F:/dev/daw_01/common/src/plugin_db.rs)）に
  discovered / processed 件数の callback を足し、`EventLoopProxy` で `AppEvent::RescanProgress`。
  VST3 note-effect の out-of-process probe（[plan_unified_plugin_picker.md](plan_unified_plugin_picker.md)）も
  この rescan 経路に乗る（probe ぶん scan が伸びるので進捗表示の意義が増す）。
- **進捗 UI**: `export_overlay.rs` の bar 描画を流用しつつ、ロード/保存/再スキャンは blocking modal では
  **なく非ブロック overlay（隅）** にする。

## 実装状況 (2026-06-10)

- **ロード = ストリーミング**: landed（`begin_asset_decode` → background decode → `AssetDecodeTick` →
  `on_asset_decode_tick` で逐次 cache 排出 + 再生 gate）+ 非ブロック進捗 overlay（`view/load_overlay.rs`）。
- **保存インジケータ**: landed（非同期保存中 = `is_async_save_pending` の間 load_overlay が「保存中…」を
  表示、非ブロック）。
- **保存の snapshot-at-invoke = landed（2026-06-10, co-temporal snapshot 設計）**: `begin_save` は
  plugin 有りなら `PendingStateRequest::Save { path, snapshot: Option<Box<Song>> }` を enqueue（snapshot は
  None で積む）。`dispatch_front_state_request` が **この save の `RequestAllStates` を送るその瞬間** に
  live を clone して snapshot を充填する。これで snapshot の plugin slot 配置と、その応答 state の配置が
  **同時刻サンプリング (co-temporal)** になり、FIFO IPC により待機中の slot 削除 / 並べ替えが保存ファイルへ
  位置 index 誤適用される窓が消える。state 応答で snapshot に state を適用 → `finish_save` で snapshot を
  直列化。`finish_save` は **migrate を snapshot と live の両方** に適用（ファイルは move なので両者追従が
  必要）し、**serialize 成功時のみ** `file_path` 確定 + saved baseline = snapshot + `recompute_dirty`
  （live が乖離していれば dirty 維持 = 待機中編集は次の save へ）。「保存して終了」 は `finish_save`（save
  成否が分かる場所）で判定し、待機中編集で dirty なら同 path へ再保存して意図を維持、clean なら quit。
  - **設計の経緯**: 当初「migration + file_path を invoke 前倒し + invoke で snapshot 凍結」を試したが、
    多角レビューで (a) Save が slot 削除 Deferred の後方に積まれると invoke 時 snapshot に削除後 state が
    誤適用される **critical silent corruption**、(b) file_path 前倒しで serialize 失敗時 recovery 退行、を
    検出。co-temporal snapshot（凍結を invoke ではなく state 収集開始の瞬間に遅らせる）で (a) を、migration を
    `finish_save` に戻し file_path 成功時のみ確定で (b) を解消。回帰 test:
    `daw_gui/tests/pending_state_queue.rs::{save_behind_deferred_remove_snapshots_post_removal_layout,
    save_with_idle_queue_freezes_snapshot_at_invoke, save_and_quit_*}`。
  - **serialize 失敗時の atomicity（別コミットで解消済み）**: 以前は migration がファイルを物理移動した
    **後**に serialize していたため、書き出し失敗（ディスク満杯/権限等）で live が `ProjectRelative` +
    `file_path=None` となり autosave/recovery でオーディオが解決不能になる欠陥が f400ee2 時点から存在した。
    migration を **plan（パス書換のみ・I/O なし、`import_audio::plan_unsaved_*_migration`）** と
    **commit（実ファイル移動、`commit_migration`）** に分割し、`finish_save` は snapshot を plan→serialize→
    （**成功時のみ**）commit + live migrate の順にする atomic 設計に変更。serialize 失敗時はファイルが一切
    動かず import_cache に無傷で残り、live は `Absolute(cache)` のまま recovery が健全に働く。unit test:
    `import_audio::tests::{plan_rewrites_paths_without_moving_files_then_commit_moves, commit_dedups_when_destination_exists}`。
- **再スキャン進捗**: VST3 probe（[plan_unified_plugin_picker.md](plan_unified_plugin_picker.md) Phase B）で
  scan を改造する際に同時に入れる（scan が重くなるので進捗の意義が増す）。

関連: [plan_export_modal.md](plan_export_modal.md)（既存進捗 modal）、
[plan_a4_autosave_recovery.md](plan_a4_autosave_recovery.md)（save baseline / autosave）、
[plan_unified_plugin_picker.md](plan_unified_plugin_picker.md)（VST3 probe で rescan が重くなる件）。
