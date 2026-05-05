# daw_01 master plan

このファイルは daw_01 の中期作業計画。`/clear` 後でもこの 1 枚を読めば「次に何をやるか」が分かるように維持する。

## 運用ルール

- このファイル = master plan。日々の作業順を決める起点。
- 大きい個別タスクは `docs/plan_<feature>.md` に切り出し、本ファイルからリンク。
- 各 Phase が完了したら本ファイル末尾の「進捗ログ」に commit hash + 1 行サマリを追記。
- gui_01 への要望は `docs/gui_01_conversation.md` にエントリ追加 → 返信受領 → 対応完了で `_archive_NNN.md` へ。
- スコープが変わったらこのファイルを書き換える。古い計画を残して別 phase を増やすより、上書きで always-current を保つ。

## 大方針

**Phase 1-5 (UI を gui_01 widget で再構築) は完了**。次は **DESIGN.md の M1 (= 「VOICEVOX 歌唱 + Clip ベース DAW」) を実用ラインに乗せる** ことが最大目標。

これまでに達成済みの土台:
- 3 プロセス分離 + IPC (named pipe + shared memory)
- CLAP plugin scan / load / activate / process / GUI embed
- gui_01 widget による全 view 構築 (`push_rect` / `push_text` / `push_lines` 0 件)
- arrangement / piano_roll widget (M13 Phase 55 で小節番号 ruler + time_sig 対応 grid)
- master fader / peak meter / loop / hover-cursor / track rename / chain reorder

不足は M1 残 6 項目 (Phase 6 で潰す)。

## Phase 6: M1 完成

DESIGN.md M1 残項目を 6 タスクに分解。`docs/plan_<feature>.md` への切り出しは着手時に判断 (本ファイルでは概要のみ)。

### 着手順マップ

```
A2 (完了 ✓) ─→ A3 WAV export (完了 ✓) ─→ A7 plugin load 同期 (完了 ✓) ─┬─→ A1 VOICEVOX ─→ M1 完成
                                                                         │
A6 tempo/timesig (完了 ✓)
A4 autosave     (← 次タスク、独立、軽量)
A5 lyric UI     (gui_01 #015、 A1 の前提)
```

### 既知の残 bug

1. **プラグインセレクターの ✕ ボタンが効かない**: gui_01 widget レベル。 `docs/gui_01_conversation.md` に投げる候補。

優先順序の根拠:
- **A2 完了**: track-parallel スレッドプール + MMCSS / thread_check / assert_no_alloc 稼働
- **A3 完了**: freewheel offline render + CLAP render ext で WAV export 復旧 (5 PR + smoke fix、 plan_a3_wav_export.md 参照)
- **A7 完了**: plugin ロード race condition の同期化 (plan_a7_plugin_load_sync.md 参照)
- **A6 完了**: transport に BPM / time_sig 編集 UI を追加 (plan_a6_tempo_timesig.md 参照)
- **A4 が次** (独立で軽量、 autosave + 起動時復元)
- **A5** は gui_01 改修先行 (#015)。reply 待ちの間 A1 の Engine / HTTP 周りを進める
- **A1** は A5 完了後に本格実装

### A2: 責務分担正常化 + track-parallel スレッドプール化 [完了]

詳細は [docs/plan_a2_audio_engine.md](plan_a2_audio_engine.md)。

**完了内容 (PR1-7 + cleanup PR、 build / clippy / test clean、 ユーザー smoke test OK)**:

1. **責務分担の正常化** — DESIGN.md の規定どおり `daw_audio = シーケンサー / mixer / オーディオ出力`、`daw_plugin_host = プラグインのロード / process` に整理
2. **新しい IPC layer** — per-plugin `ProcessData` shmem (16 KB / plugin)、per-worker `WorkerBridge` shmem + Win32 named auto-reset events
3. **track-parallel スレッドプール (両プロセス N worker、 1:1 ペア)** — `audio_worker::AudioWorkerPool` が work-stealing で track を fan-out、`process_server::WorkerPool` が各 plugin を wake/done event で dispatch
4. **plugin_id registry** — plugin_host が SetSlotPlugin で発行 + shmem create + `Arc<ArcSwap<Vec<Option<PluginEntry>>>>` で publish、daw_audio は `slot_to_plugin_id` map で引く
5. **TIME_CRITICAL priority + MMCSS** — 両プロセスの worker が `SetThreadPriority(TIME_CRITICAL)` + `AvSetMmThreadCharacteristicsW("Pro Audio")` (Drop で revert)
6. **CLAP `thread_check` ext** — host 側で `clap_host_thread_check` を提供、TLS フラグで is_audio_thread を判定
7. **assert_no_alloc** — `[features] rt-assert` 追加、`cargo test --features rt-assert` で RT 違反検出可能
8. **旧 audio thread コード完全撤去** — `run_audio`, `collect_events_for_buffer`, `Tracks::params/vocal`, `TrackAudioParams (旧)`, `VocalAudio (旧)`, `PerTrackState`, `start_audio_legacy`, `AudioThread`, `TrackRouting`, `AudioRouting` を全削除 (~1500 行減)、tests を `daw_audio/src/sequencer.rs` に移植

**未完: WAV export** — A2 残タスクとしてあったが、daw_audio 側 LocalState 共有設計が大規模変更のため A3 で本格実装に切り出し。本フェーズでは plugin_host の旧 export_wav_offline + GUI Export メニューを削除のみ (機能は一時停止状態で A3 まで持ち越し)。

### A6: tempo / time_sig 変更 UI [完了]

詳細は [docs/plan_a6_tempo_timesig.md](plan_a6_tempo_timesig.md)。

**完了内容 (build / clippy / test --features rt-assert all clean、 ユーザー smoke test OK)**:

- transport bar に BPM `text_input` (1.0..=400.0 clamp、 commit で audio engine に LoadSong 再送)
- 同じく time_sig numerator `text_input` (1..=32 clamp) と denominator `dropdown` (2/4/8/16)
- AppData に `bpm_edit_text` / `time_sig_num_edit_text` 編集 buffer、 `AppEvent::BpmEditChanged` / `CommitBpmEdit` / `TimeSigNumEditChanged` / `CommitTimeSigNumEdit` / `SetSongTimeSigDenominator` を追加
- commit 系 3 種を `is_undoable` に登録 → handle_event 冒頭の `push_undo_snapshot` で自動 undo 対応
- `resync_song_edit_texts` helper で song 差し替え時 (after_undo_redo / action_new / action_open_path) に表示 buffer を現値に書き戻す
- 再生テンポは sequencer の `samples_per_beat = sample_rate * 60 / song.bpm` (sequencer.rs:71) が ArcSwap publish された Song を毎フレーム参照 → 変更が次フレームから即時反映

**スコープ外 (M2 範囲)**: CLAP/VST3 plugin への transport / tempo 通知 (`clap_event_transport_t`)、 tempo automation、 拍子 automation、 bar 番号 offset。

### A4: autosave + 起動時復元 [優先度 3 — 独立 / 軽量]

**現状**: 手動保存 (`Ctrl+S`) のみ。

**やること**:
1. background thread で 60 秒ごとに `<file_path>.autosave.daw` (file_path == None なら `%APPDATA%/daw_01/recovery/<uuid>.autosave.daw`) に保存
2. dirty flag (modified since last save) を AppData に追加 → dirty かつ 60 秒経過で書く
3. 起動時に recovery ディレクトリを scan + 既存 file の autosave 兄弟を検出 → modal で「復元しますか?」プロンプト
4. 正常終了時に autosave ファイルを削除

**主な変更ファイル**:
- 新規 `daw_gui/src/autosave.rs`
- [daw_gui/src/main.rs](daw_gui/src/main.rs) (起動時 detection)
- [daw_gui/src/app.rs](daw_gui/src/app.rs) (`AppEvent::Autosave*`、`dirty: bool`)

**受け入れ基準**:
- 60 秒後に autosave ファイルが作成される
- アプリを kill して再起動 → 復元プロンプトが出る
- 「復元」で直近の autosave からロードできる
- 「破棄」で autosave ファイルを削除

### A5: piano_roll note 歌詞編集 UI [優先度 4 — gui_01 #015]

**現状**: `Note { lyric: Option<String> }` schema あり、piano_roll widget の表示も M9 Phase 44c で済 (歌詞文字を note 上にレイアウト)。**入力 UI が無い**。

**やること**:
1. **gui_01 conversation #015 起こす**: piano_roll widget 内で note を選択 → F2 / Enter / dbl-click で text_input overlay 起動 → Enter commit / Esc cancel / Tab で次 note へ。編集中は drag/resize/wheel zoom を抑制 (modal-ish)
2. gui_01 reply 受領後、daw_01 側で `NotesEditRequest::SetLyric { id, text }` ハンドリング、`AppEvent::SetNoteLyric { clip_ref, note_id, text }` 追加
3. IME 入力対応 (CJK モーラ単位、既存 `text_input_at` の IME 機構に乗る)

**主な変更ファイル**:
- `docs/gui_01_conversation.md` #015 起こし
- gui_01 reply 後: [daw_gui/src/view/piano_roll_view.rs](daw_gui/src/view/piano_roll_view.rs)、[daw_gui/src/app.rs](daw_gui/src/app.rs)

**受け入れ基準**:
- piano_roll で note を選択 → F2 / Enter で inline 編集
- Tab で次の note (start_beat 順) へ移動
- 入力した歌詞が JSON プロジェクトファイルに保存される
- IME 入力で CJK が正しく入る (commit 時のみ反映、preedit は表示)

### A1: VOICEVOX 統合 [優先度 5 — 本丸]

**現状**: `daw_audio/voicevox.rs` プレースホルダ ([common/src/model.rs](common/src/model.rs) の `InstrumentSource::Vocal` schema は完備)。

**やること** (REAPER VOICEVOX スクリプト = `%APPDATA%\REAPER\Scripts\yoshino\voicevox\` を参照実装に):
1. **HTTP client** (`reqwest` async): `/sing_frame_audio_query` / `/frame_synthesis` / `/audio_query` / `/synthesis` / `/speakers`
2. **Engine 自動起動**: 設定 (`%APPDATA%/daw_01/voicevox_engine_path`) から実行ファイルパスを読み、subprocess + Job Object で寿命管理 (DESIGN.md の手法を流用)
3. **歌詞分割**: 小書きかな (ぁぃぅぇぉゃゅょっ) は直前と結合して 1 モーラ化
4. **Sing pipeline**:
   - Clip notes → `{"notes":[{"id","key","frame_length","lyric"}, ...]}` JSON
   - `POST /sing_frame_audio_query?speaker=6000` (波音リツ固定) → audio_query
   - `outputSamplingRate=48000` 書き換え
   - `POST /frame_synthesis?speaker={singer_id}` → WAV bytes
5. **WAV cache**: `clip_id + content_hash → Vec<f32>` を `daw_audio` 側で保持。同 clip 同内容は再合成しない
6. **Vocal track の audio mix**: audio thread は cache から該当時刻の f32 を読んで master へ mix。track の plugin chain (`fx_chain`) は通すが `instrument` は使わない (Vocal は WAV 直挿し)
7. **Track Inspector の Vocal source UI**: speaker / style 選択 dropdown ([speakers] API レスポンス + cache)
8. **失敗時の resilience**: Engine 起動失敗 / HTTP 失敗時は track を mute 扱い、エラーメッセージ表示、他 track は影響受けず再生続行

**主な変更ファイル**:
- [daw_audio/src/voicevox.rs](daw_audio/src/voicevox.rs) (現状プレースホルダ → 実装)
- 新規 `daw_audio/src/voicevox_cache.rs` (clip_id → Vec<f32>)
- 新規 `daw_audio/src/voicevox_engine.rs` (subprocess 起動 + ヘルスチェック)
- [daw_audio/src/engine.rs](daw_audio/src/engine.rs) (Vocal track の mix-in)
- [daw_gui/src/view/track_inspector.rs](daw_gui/src/view/track_inspector.rs) (Vocal source 編集)
- [common/src/protocol.rs](common/src/protocol.rs) (Vocal track 内容変更を audio に伝達するメッセージが必要なら追加)

**受け入れ基準**:
- 新規 Vocal track 作成 → speaker 選択 → clip 内 note に歌詞入力 → 再生で歌う
- 同 clip の 2 度目の再生はキャッシュヒット (合成 1 回のみ)
- Engine 起動失敗時はエラー表示 + 他 track は再生継続
- Vocal track の `volume` / `pan` / `muted` / `solo` が反映 (A2 完了が前提)

着手時 `docs/plan_a1_voicevox.md` を切り出し必須 (大規模)。

### A3: WAV 書き出し / mixdown (freewheel offline render) [完了]

詳細は [docs/plan_a3_wav_export.md](plan_a3_wav_export.md)。

**完了内容 (PR1-5、 build / clippy / test --features rt-assert all clean)**:

- engine resource 共有化: `LocalState` の `worker_bridge` / `worker_syncs` / `plugin_refs` / `slot_to_plugin_id` / `vocal_store` / `worker_pool` を `Arc<EngineShared>` に移管 (ArcSwap で wait-free 共有)
- CLAP `clap_plugin_render` ext: `LoadedPlugin::set_render_mode(mode)` を追加、 ClapPlugin で `clap_plugin_render::set` 呼び出し
- protocol: `MainToChild::ExportWav { path }` / `SetRenderMode(RenderMode)` 追加、 `ChildToMain::ExportWavComplete { error }` 復活
- daw_audio: `daw_audio/src/export.rs` で freewheel offline render を実装。 同 AudioWorkerPool で track-parallel + plugin handshake を駆動、 hound::WavWriter で出力。 `export_running` フラグで CPAL callback を silence
- IPC: audio pipe を tokio::io::split で双方向化、 export 完了通知を ChildToMain::ExportWavComplete で送信
- GUI: File → Export WAV / Ctrl+E / status bar 表示復活、 SetRenderMode(Offline/Realtime) を export bookend で plugin host に送る

**残課題 (M1 完成後の検討、 必要なら別 plan)**:

- 進捗 modal (cancel button、 export thread を中断する mechanism)
- Selection / Loop range のみの export
- VST3 の render mode サポート (現状 no-op、 IComponent::setIoMode へのマップは M2)
- has_hard_realtime_requirement() を返す plugin (hardware proxy 等) のサポートは M2 で realtime export 検討

---

### A3 (旧版): WAV 書き出し / mixdown [優先度 6 — 完了済、 以下は元計画]

**現状**: A2 で plugin_host 側の旧 export_wav_offline を削除し、GUI Export メニューも一旦無効化済。daw_audio 側に新規実装する。

**設計方針** (Ardour [Export Dialog manual](https://manual.ardour.org/exporting/export-dialog/) と CLAP [render ext spec](https://github.com/free-audio/clap/blob/main/include/clap/ext/render.h) を一次情報根拠):
- **freewheel mode 採用**: export 中は通常再生を停止、CPAL callback は無音、export thread が同じ AudioWorkerPool / plugin shmem を使って CPU 限界速度で render
- **plugin instance は複製しない**: Ardour / REAPER とも標準は同 instance 流用、複製案はメモリ 2 倍化で非現実的
- **CLAP `clap_plugin_render` ext**: export 開始前に全 plugin に `set(CLAP_RENDER_OFFLINE)`、完了で `CLAP_RENDER_REALTIME`

**やること**:
1. **engine の resource 共有化**: `LocalState` の `plugin_refs` / `slot_to_plugin_id` / `vocal_store` / `worker_syncs` / `worker_pool` を `SharedState` に Arc 化して移管 (CPAL closure と export thread で共有可能に)
2. **新規 `daw_audio/src/export.rs`**: offline render loop + WAV writer。export thread が `process_track_owned` を `Song.length_beats` 分ループ呼び出し、 `hound::WavWriter` で出力
3. CPAL callback は `shared.export_running` 中、`process_buffer` を skip し無音出力
4. `MainToChild::ExportWav { path }` を再導入 (or `MainToAudio::ExportWav`)、daw_audio で受信
5. `ChildToMain::ExportWavComplete { error }` を audio から送信
6. `LoadedPlugin` trait に `set_render_mode(mode)` 追加、ClapPlugin で `clap_plugin_render` ext を取得 / set 呼び出し
7. GUI File menu / `Ctrl+E` shortcut / `action_export_wav` 復活
8. UI: export 中は status bar に「Export 中」表示、Play ボタンを無効化

**主な変更ファイル**:
- 新規 `daw_audio/src/export.rs`
- [daw_audio/src/engine.rs](daw_audio/src/engine.rs) (resource 共有化、export hook)
- [daw_audio/src/main.rs](daw_audio/src/main.rs) (ExportWav handler + writer thread)
- [daw_plugin_host/src/plugin_instance.rs](daw_plugin_host/src/plugin_instance.rs) (`set_render_mode`)
- [daw_plugin_host/src/clap_plugin.rs](daw_plugin_host/src/clap_plugin.rs) (`clap_plugin_render` 実装)
- [common/src/protocol.rs](common/src/protocol.rs) (ExportWav / ExportWavComplete 再追加)
- [daw_gui/src/app.rs](daw_gui/src/app.rs) (action_export_wav 復活)
- [daw_gui/src/view/root.rs](daw_gui/src/view/root.rs) (File menu / shortcut)

**受け入れ基準**:
- File → Export WAV → パス選択 → 再生中は自動停止 → freewheel offline render → WAV ファイルが書き出される
- 書き出した WAV を別アプリで再生 → DAW 内の再生と聴感上一致
- export 中は CPAL は無音、 完了後通常再生に復帰可能
- VOICEVOX cache を併用 (再合成しない、A1 完成後)

着手時 `docs/plan_a3_wav_export.md` を切り出し推奨 (engine の resource 共有化が大規模変更のため)。

## Phase 7 以降 (M2 へのつながり、現時点では着手しない)

メモリと DESIGN.md より、M2 候補:
- VST3 対応
- オートメーション
- send / return バス、グループ track
- オーディオ録音 + オーディオクリップ
- MIDI 録音 / export
- メトロノーム / count-in
- アンドゥ / リドゥ統合 (gui_01 `HistoryStack` の wire-up 確認)
- Linux 対応

各々に着手するときに本ファイルへ Phase 7 以降を追加する。

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
| 2026-05-04 | da0bdf5 | Phase 5 | dead_code 清掃 (ClipBox/NoteBox/TrackHeader 構造体 + 派生メソッド削除、AppEvent に `#[allow(dead_code)]`) + clippy fix 4 件 |
| 2026-05-04 | b12adff | Phase 4 | track rename / chain reorder / reorder IPC 取り込み |
| 2026-05-04 | d832627 | Phase 4 仕上げ | gui_01 #013 (M11 Phase 52) `text_input_at_focused` 取り込み、track Rename → 即タイプ可能 |
| 2026-05-04 | b9a3fe2 | Phase 0 | gui_01 #013 を Resolved 化、archive へ移動 |
| 2026-05-04 | ce08095 | bug fix | `UiHost::with_window` で cursor 配線、hover/drag のカーソル形状が変化しない問題を解消 |
| 2026-05-04 | 22ffa56 | Phase 0 | gui_01 #014 [Open] ルーラー & time_sig 対応 grid 要望 |
| 2026-05-04 | a2117cb | chore  | .gitignore に /.claude/scheduled_tasks.lock を追加 |
| 2026-05-04 | af44c46 | M13 取り込み | gui_01 #014 (M13 Phase 55) を取り込み — アレンジビュー小節番号 ruler + ピアノロール ruler 領域 + time_sig 対応 grid |
| 2026-05-04 | 12be490 | Phase 0 | gui_01 #014 を Resolved 化、archive へ移動 |
| 2026-05-04 | 8954bed | A2 PR1-7 | 責務分担正常化 + track-parallel スレッドプール化 (build/clippy clean): plan_a2_audio_engine.md 参照 |
| 2026-05-05 | d7a7575 | A2 完了 | 残 cleanup + audio quality: 旧 audio thread コード全削除 (~1500 行)、tests を daw_audio/src/sequencer.rs に移植、MMCSS / CLAP thread_check ext / assert_no_alloc 追加。 CLAUDE.md に「まず調べる」 ルール追記。 WAV export は A3 で本格実装に切り出し |
| 2026-05-05 | 82f7e54 | A3 PR1 | LocalState から EngineShared を抽出 (Arc + ArcSwap で wait-free 共有)、 機能変化なし |
| 2026-05-05 | c10141d | A3 PR2 | LoadedPlugin::set_render_mode trait method 追加、 ClapPlugin で clap_plugin_render::set 呼び出し、 Vst3Plugin は no-op |
| 2026-05-05 | 4ace2a5 | A3 PR3 | protocol 拡張: MainToChild::ExportWav / SetRenderMode + ChildToMain::ExportWavComplete 復活、 plugin_host で全 plugin に render mode broadcast |
| 2026-05-05 | 9607660 | A3 PR4 | daw_audio/src/export.rs 新規: freewheel offline render + WavWriter。 export_running フラグで CPAL callback を silence、 同 worker pool / plugin shmem を export thread が独占駆動 |
| 2026-05-05 | e8b3d6e | A3 完了 | GUI で File→Export WAV / Ctrl+E / status bar 復活。 audio pipe を read/write 双方向化し、 export 完了通知を ChildToMain::ExportWavComplete で daw_gui へ。 SetRenderMode(Offline/Realtime) bookend で plugin に CLAP render hint |
| 2026-05-05 | 469acd7 | A3 smoke fix | smoke test で発見した 3 件 (Play/Stop/SetLoop の plugin_host 重複送信、 Ctrl+E shortcut 漏れ、 メーター peak の publish 漏れ) を修正 |
| 2026-05-05 | 02fe061 | A7 完了 | plugin ロード race の同期化: AppData::pending_plugin_loads + track_pending_load helper で SetSlotPlugin 送信時に再生中なら自動 Stop、 全 SlotPluginLoaded 受信完了で自動 Play 再開 |
| 2026-05-05 | (this commit) | A6 完了 | transport bar に BPM / time_sig 編集 UI: text_input + dropdown、 commit で song 更新 + LoadSong 再送 + Undo/Redo 対応。 numpad Enter 不対応は gui_01 #016 で対応依頼 |
