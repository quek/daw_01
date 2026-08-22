# A1: VOICEVOX 統合

## Context

`docs/plan.html` Phase 6 の A1。 M1 (= 「VOICEVOX 歌唱 + Clip ベース DAW」) の本丸。

調査の結果、 統合は当初想定より遥かに進んでいることが判明:

- **`common/src/voicevox.rs` (615 行)** で歌唱 / トーク両パイプラインが完成
  - `synthesize_song(song, query_speaker, singer_id) -> Vec<SynthResult>`
  - `/sing_frame_audio_query` → `outputSamplingRate=48000` patch → `/frame_synthesis` → `decode_wav_to_f32`
  - JSON build (`build_sing_query`) + lyric escape + gap rest 充填、 全部実装済
- **`daw_gui/src/app.rs::begin_vocal_synth`** で background thread 合成 + `AppEvent::SynthesizeVocal`/`VocalSynthCompleted` 連携
- **transport の「Synth (V)」 ボタン**から起動可能
- **`MainToChild::SetVocalAudio { samples: Vec<f32> }`** で daw_audio に送信、 `engine.rs:286 / :631` で hot-swap + 再生 mix-in

## 残タスク

### Phase A — VOICEVOX engine 自動起動 [優先度 1]

**現状**: ユーザーが手動で VOICEVOX を起動しておく必要がある。 起動していなければ Synth ボタンで「VOICEVOX が応答しません」 エラーになる。

**やること**:
1. 設定ファイル `%LOCALAPPDATA%/daw_01/voicevox_engine_path.txt` から exe パスを読む (PATH 環境変数優先 → 設定ファイル → ユーザーに dialog で選択させる の順)
2. `/version` で起動確認 → 起動済ならスキップ、 未起動なら subprocess 起動
3. **Windows Job Object** で daw_01 終了時に VOICEVOX も kill (sing_like_coding と同じ手法、 plugin_host 起動でも使っているはず — 確認)
4. 起動後 `/version` を 30 回 × 2 秒間隔でポーリング → タイムアウトは status_message に出してユーザーに通知

**主な変更**:
- 新規 `common/src/voicevox_engine.rs` or `daw_gui/src/voicevox_engine.rs`
- daw_gui 起動時 (`main.rs::run`) に背景 thread で起動
- 失敗時は status bar に表示、 リトライボタン (or status bar クリックで再試行)

### Phase B — Speaker / Singer 選択 UI [優先度 2]

**現状**: `common::voicevox::DEFAULT_SINGER_ID` (= 3061 中国うさぎノーマル) ハードコード。 トラックごとに speaker を変えられない。

**やること**:
1. 起動時に `/singers` を fetch (background thread) → `Vec<SingerInfo { name, styles: Vec<{ id, name }> }>` を AppData に保持
2. `InstrumentSource::Vocal` schema に `singer_id: u32` フィールド追加 (なければ)
3. `daw_gui/src/view/track_inspector.rs` で Vocal track 選択時に singer dropdown を出す
4. `synthesize_song` の引数を「track ごとの singer_id」 を受けるように変更 or per-track 呼び出し

**主な変更**:
- `common/src/model.rs::InstrumentSource::Vocal` に `singer_id: u32` 追加 (default = 3061)
- `daw_gui/src/app.rs` に `singers: Vec<...>` + 起動時 fetch
- `daw_gui/src/view/track_inspector.rs` で dropdown 表示
- `common/src/voicevox.rs::synthesize_song` の signature 拡張 (or per-track build)

### Phase C — WAV cache [優先度 3]

**現状**: Synth ボタン押下のたびに全 Vocal track 全 clip を再合成。 1 音 4 秒 × 10 音で 40 秒、 速度的に厳しい。

**やること**:
1. `clip_id + content_hash (notes + singer_id) → Vec<f32>` を AppData (or daw_audio 側) で持つ
2. cache hit なら HTTP call をスキップして即 SetVocalAudio
3. ファイル永続化: `%LOCALAPPDATA%/daw_01/voicevox_cache/<hash>.wav` (起動時に load)
4. cache size 上限 (例: 500MB) で LRU eviction

**主な変更**:
- 新規 `common/src/voicevox_cache.rs` (in-memory + file-system 両層)
- `synthesize_song` の前後で cache check / save

### Phase D — 歌詞分割の拗音結合 [優先度 4]

**現状**: `common/src/voicevox.rs` 内では 1 char = 1 モーラ。 "きゃ" は 2 モーラとして合成 → 不自然。

**やること**:
1. `common/src/voicevox.rs` (or 新規 `common/src/morae.rs`) に `split_into_morae(text: &str) -> Vec<String>` 追加
   - 小書き仮名 (ぁぃぅぇぉ ゃゅょ っ ァィゥェォ ャュョ ッ) を直前と結合
2. `build_sing_query` でこの helper を使う or note の lyric 自体を pre-split する設計判断
3. 既に gui_01 #017 でも同じ split が必要 → **モーラ分割は common に寄せる** (gui_01 と daw_01 で重複しない)

**主な変更**:
- `common/src/voicevox.rs` に `split_into_morae` 追加 + unit test
- `build_sing_query` で use

## 進め方

Phase A → B → C → D の順。 A は独立、 B は schema 変更が絡むので Vocal source 編集と一緒に。 C は B 後に着手 (cache key に singer_id が要る)。 D は最後 (現状の 1char = 1 モーラでも音は出る、 quality 改善)。

各 Phase ごとに commit。 ユーザー smoke test を要する Phase は A (engine 起動) と B (speaker UI 操作)。 C/D は unit test 中心。

## 受け入れ基準 (M1 の A1 完了条件)

- 新規 Vocal track 作成 → speaker 選択 → clip 内 note に歌詞入力 (gui_01 #017 が必要) → Synth ボタン → 再生で歌う
- 同 clip の 2 度目の Synth はキャッシュヒット (合成 1 回のみ)
- Engine 起動失敗時はエラー表示 + 他 track は再生継続
- Vocal track の `volume` / `pan` / `muted` / `solo` が反映 (A2 完了済)

## スコープ外

- `/audio_query` + `/synthesis` (トーク機能) は M2
- 拗音以外の高度なモーラ判定 (連音 / 促音の特殊処理) は M2
- Engine の version mismatch detection
- Singer switching の実時間プレビュー
