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
A2 multi-track ─┬─→ A1 VOICEVOX ─┐
                │                 ├─→ M1 完成
                └─→ A3 WAV export ┘
A6 tempo/timesig (独立、早期で OK)
A4 autosave     (独立、早期で OK)
A5 lyric UI     (gui_01 #015 要望 → 取り込み、A1 の前提)
```

優先順序の根拠:
- **A2 が基盤**。現状 `Track 0 / Clip 0 / モノフォニック` で固定 ([daw_plugin_host/src/audio.rs](daw_plugin_host/src/audio.rs) の audio thread)。これを解かないと A1 (VOICEVOX) も A3 (mixdown) も「単一 track のみ」になる。
- **A6 / A4** は独立で軽量。A2 の合間に挟める。
- **A5** は gui_01 改修先行 (#015)。reply 待ちの間 A1 の Engine / HTTP 周りを進める。
- **A1** は A2 + A5 完了後に本格実装。
- **A3** は A2 完了後 (multi-track mix を offline render に流用)。

### A2: multi-track / polyphonic 再生 [優先度 1 — 基盤]

**現状の制約**: `daw_plugin_host` の audio thread が `song.tracks[0].clips[0]` 固定で、`active_notes` を 1 つだけ持ち単音再トリガ ([daw_plugin_host/src/audio.rs](daw_plugin_host/src/audio.rs))。

**やること**:
1. **全 track loop**: audio thread で `song.tracks.iter()` を walk、各 track が個別の plugin instance を持つ
2. **Polyphonic note state**: `active_notes` を `Vec<NoteState>` 化、同時刻の複数 NoteOn を保持
3. **Per-track mix**: track 単位の f32 buffer を確保 (RT スレッドでの allocation 禁止 — `activate` 時に事前確保)、master へ sum
4. **Volume / pan / mute / solo を mix に反映** (現状 schema にあるが audio に無接続)
5. **Plugin host のスレッド分離**: track 単位で `Plugin` を持つので main-thread でのライフサイクルを track ごとに直列化

**主な変更ファイル**:
- [daw_plugin_host/src/audio.rs](daw_plugin_host/src/audio.rs) (audio thread の全面書換)
- [daw_plugin_host/src/host.rs](daw_plugin_host/src/host.rs) (plugin instance map: `track_id → Plugin`)
- [daw_audio/src/engine.rs](daw_audio/src/engine.rs) (master mix が plugin_host から受ける format 変更を吸収)

**受け入れ基準**:
- 2 トラック以上に別 instrument を載せて同時再生で音が混ざる
- 1 clip 内に同時刻 NoteOn を 2 つ以上配置 → 両方鳴る
- track の `muted` / `solo` トグルが即座に反映
- track の `volume` / `pan` 変更で master 出力が変わる
- `cargo clippy --workspace -- -D warnings` clean

着手時 `docs/plan_a2_multi_track.md` を切り出し検討。

### A6: tempo / time_sig 変更 UI [優先度 2 — 独立 / 軽量]

**現状**: `Song { bpm, time_sig }` schema 完備、#014 で ruler/grid も連動済み。**変更 UI が無い**。

**やること**:
1. transport に BPM number_input (1.0..400.0) を追加
2. transport に time_sig 用 (numerator: 1..32, denominator: 2/4/8/16 dropdown) を追加
3. `AppEvent::SetSongBpm(f32)` / `SetSongTimeSig(u8, u8)` 追加
4. `handle_event` で `song.bpm` / `song.time_sig` を更新 + plugin_host に `LoadSong` を送り直す (既存パスに乗る)
5. undoable history 対象とする

**主な変更ファイル**:
- [daw_gui/src/view/transport.rs](daw_gui/src/view/transport.rs)
- [daw_gui/src/app.rs](daw_gui/src/app.rs)

**受け入れ基準**:
- transport から BPM 変更 → ruler/grid + 再生テンポが追従
- time_sig 変更 → bar 線位置が変化
- Undo/Redo で変更を巻き戻せる

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

### A3: WAV 書き出し / mixdown [優先度 6]

**現状**: export 機能なし。

**やること**:
1. File menu (or transport の Export ボタン) に "Export WAV..." を追加 (`rfd::FileDialog` でパス選択)
2. offline render mode を `daw_audio` に新設: audio thread とは別の dedicated thread で sequencer + plugin process を CPAL コールバック非依存で走らせる
3. 出力長は `Song.length_beats` をデフォルト、UI で Loop range / Selection も選択可
4. `hound` crate で WAV 書き出し (16/24/32 bit, 48000 Hz fixed)
5. 進捗 modal (cancel button、IPC で `ChildToMain::ExportProgress` を流す)
6. VOICEVOX cache を併用 (再合成しない)

**主な変更ファイル**:
- 新規 `daw_audio/src/render.rs` (offline render loop)
- 新規 `daw_gui/src/export.rs` (dialog + progress modal)
- [common/src/protocol.rs](common/src/protocol.rs) (`MainToChild::ExportWav { path, length_beats, bit_depth }` / `ChildToMain::ExportProgress`)

**受け入れ基準**:
- File → Export WAV → パス選択 → WAV ファイルが書き出される
- 書き出した WAV を別アプリで再生 → DAW 内の再生と聴感上一致
- export 中に cancel ボタンで中断できる
- export 中も DAW 自体は操作可能 (offline render は別スレッド)

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
