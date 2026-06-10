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
- **保存の snapshot-at-invoke は分離（未実装）**: `save_to` の `migrate_unsaved_audio_sources_into`
  / `migrate_unsaved_bounce_sources_into` が **live `self.song` のファイル移動 + path 書換**を伴うため、
  snapshot 側だけ書き換えると live project が移動後ファイルを見失う。正しくは「migrate を invoke 時に
  `self.song` へ適用 → snapshot → 完了時に plugin state を snapshot へ適用して serialize」という再配列が
  要る。**save はユーザーの作業データに直結する critical path** なので、コスト判断ではなく**データ安全性
  判断**として、慎重にレビューする独立変更に分離する。現状の save は既に非ブロック+単一スレッドで安全。
- **再スキャン進捗**: VST3 probe（[plan_unified_plugin_picker.md](plan_unified_plugin_picker.md) Phase B）で
  scan を改造する際に同時に入れる（scan が重くなるので進捗の意義が増す）。

関連: [plan_export_modal.md](plan_export_modal.md)（既存進捗 modal）、
[plan_a4_autosave_recovery.md](plan_a4_autosave_recovery.md)（save baseline / autosave）、
[plan_unified_plugin_picker.md](plan_unified_plugin_picker.md)（VST3 probe で rescan が重くなる件）。
