# VOICEVOX 合成の builtin instrument plugin 化 計画

ステータス: **着手中** (2026-05-08、 PR-V1 完了)。 `plan_audio_followup.md`
後回し 2「VOICEVOX → ClipContent::Audio 統合」 を「ゼロから作るなら」
の観点で再設計したもの。

## タイミングの不変条件 (r.md #39、これに触れる変更は先にここを読む)

**合成バッファの index N = 曲の sample 位置 N。これが唯一の契約。**

- **読み出し側 (`VoicevoxAudioHalf::process`) は補正オフセットを一切持たない**:
  `base = song_pos_beats * samples_per_beat` のみ。ここに「lead-in を足す」等の
  経路別補正を戻さない (旧実装は歌用 lead-in を talk にも一律に適用しており、
  talk が話速依存で −53〜+96ms ずれていた)。
- **各ソースの先頭無音は配置側 (synth thread) が自分の既知量として吸収する**:
  - 歌 = `sing_place_samples(base_beat, bpm, sr)` — query が入れた
    `REST_FRAMES` の leading rest ぶん手前に置く。
  - talk = `talk_place_samples(start_beat, speed, bpm, sr)` — `prePhonemeLength`
    を 0 に注入してあるので実質「クリップ位置 = 発話開始」。
  - 配置位置は **符号付き**。曲頭付近では負になり、その部分 (無音) は mix が捨てる。
    位置をずらして回避しない。(r.md #75 で `mix_placed_groups` は
    `voicevox_render::MixBuffer` の差分 mix に置き換わったが、**この契約は不変**。)
- **query の note 位置は絶対 frame**: `build_sing_query` は「基準ノートからの拍
  オフセットを frame へ丸めた絶対値」を先に確定してから frame_length 列に落とす。
  長さの下限 1 frame を確保するために **後続の位置を押し出さない**
  (重なりは前ノートの切り詰めで解決、潰れたら落とす)。残差は VOICEVOX の
  93.75fps 分解能そのもの (±5.33ms、バイアス 0) で、これ以上は原理的に詰められない。
- **口パクも同じ契約**: `build_mouth_events` の anchor は「phoneme 列 frame 0 が
  来る beat」= wav 先頭が来る beat。`docs/plan_pakupaku.md` §6 参照。
- **キャッシュ**: wav の中身を決める定義 (engine へ注入するパラメータ等) を変えたら
  `voicevox_cache.rs::CACHE_SCHEMA_VERSION` を必ず +1 する。query 文字列や
  `TalkParams` に現れない変更は key に自然には反映されず、旧 wav を掴んで
  「直したのに変わらない」になる。同じ理由で、**口パクの配置ルール** を変えたら
  `common::lipsync::PLACEMENT_GEN` を +1 する (保存済み口パク clip は入力が同じままなので
  fingerprint では再生成されない。`Clip::lipsync_gen` の世代照合が唯一のトリガ)。

## master 出力の遅延 (`Schedule::master_latency_samples`) の消費者

`master_buffer[P]` に載っているのは曲位置 `P - master_latency_samples` の音。この値は
**master `Mix` の src の最大 path latency ＋ `Song::master_reported_latency_samples`**
(= master fx chain 自身の報告 latency)。master は `Track` を持たないので、後者は
`Song` 直下に置き `Song::reported_latency_mut` の sentinel 分岐で書き込む
(`fx_chain_by_track_id_mut` と同 idiom)。

この 1 つの値を **2 箇所** が引く。片方だけ直すと非対称になるので必ず両方見ること:

1. **metronome click** (`engine.rs`) — click は `render_master_buffer` の後に重ねる
   (master fx を通らない) ので、参照位置からこの値を引いて音と揃える。count-in の
   click も同じだけ引く (引かないと count-in → 本再生の境目で 1 拍だけ間隔が伸びる)。
2. **WAV 書き出し** (`export.rs::shift_window_for_master_latency`) — 書き出し窓と
   走査終端をこの値だけ後ろへずらす。ずらさないと wav 全体が後ろへずれ、
   stem を元位置に貼り戻すと二重にずれる。

なお click の **拍グリッド** は `metronome::ClickGrid::Song(&TempoMap)` (tempo automation
積分済み) で求める。瞬間 bpm × 絶対 sample の等間隔グリッドにすると、テンポ変更以降の
click が clip / note (`playhead_beats` 基準) と別グリッドに載る。

## 進捗

- ✅ **PR-V1**: builtin plugin インフラ完了
  - `PluginFormat::Builtin` variant 追加 (`common/src/plugin_format.rs`)
  - `common::plugin_db::BUILTIN_ID_SILENCE` + `builtin_descriptors()`
    新設、 `scan_system` で append (= picker UI が常に builtin を表示)
  - `daw_plugin_host::builtin` module 新設、 `Silence` (= 無音 instrument
    の reference impl) + `load_builtin(uri)` dispatcher 実装
  - `plugin_instance::load_plugin` に Builtin 分岐 (= 既存 LoadedPlugin
    trait をそのまま実装、 audio thread の codepath は CLAP / VST3 と
    完全に同じ)
  - unit test 5 件追加 (load 成功 / unknown / non-URI / process 無音 /
    state roundtrip)
- ✅ **PR-V2.1**: VOICEVOX builtin plugin skeleton 完了
  - `VoicevoxBuiltin` struct + `VoicevoxState { speaker_id, style_name }`
    (`daw_plugin_host/src/builtin/voicevox.rs`)
  - `BUILTIN_ID_VOICEVOX` を `builtin_descriptors()` に追加 → picker UI
    から VOICEVOX を選択可能に
  - bincode で state encode/decode の skeleton (= state_load は PR-V2.5
    で `&mut self` 化 + full restore 実装予定、 現状は parse 妥当性確認
    + warn ログのみ)
  - unit test 5 件追加 (default state / bincode roundtrip / id-format /
    silent process / state_save bytes)
- ✅ **PR-V2.2**: 歌詞 IPC 経路完成
  - `common::plugin_metadata::NoteMetadata { note_id, lyric }` 新設
  - `MainToChild::SetBuiltinPluginNoteMetadata { plugin_id, entries }`
    IPC variant 追加 (`common::protocol`)
  - `LoadedPlugin::set_note_metadata(&mut self, &[NoteMetadata])` を
    default no-op で trait 追加 (= CLAP / VST3 plugin は影響なし、 trait
    の format-neutrality 維持)
  - `VoicevoxBuiltin::set_note_metadata` で内部 `HashMap<u32, String>` を
    完全置換 (= 「現在の clip 内 notes 全部」 を毎回 flush する設計)
  - `daw_plugin_host` main: `MainToChild` → `PluginCommand::
    SetBuiltinPluginNoteMetadata` に転送、 plugin-main thread で
    `plugin_lookup` を walk して `(track, slot)` を逆引き → `set_note
    _metadata` 呼び出し
  - `daw_audio` main: `SetBuiltinPluginNoteMetadata` を ignore arm に
    追加 (= 役割は plugin_host のみ)
  - unit test 1 件追加 (set_note_metadata で lyrics buffer が完全置換、
    空 flush で全消去)
- ✅ **PR-V2.4**: per-note voice + note_id 経路完成
  - `common::process_data::Event` に `note_id: u32` field 追加 (= 旧
    `_pad2` 領域を昇格)、 `push_note_on` / `push_note_off` の signature
    に `note_id` 引数追加
  - `daw_plugin_host::plugin_instance::NoteTransition::On/Off` に
    `note_id` field 追加 (`daw_audio::sequencer::NoteTransition` 側にも
    並行追加)。 全 caller を `note_id, ..` で更新
  - `daw_audio::sequencer::collect_events_for_buffer`: track 内全 clip の
    notes を flatten した「通し index」 を `note_id` として振る (= daw_gui
    `sync_vocal_metadata` と同じ番号体系で flush されるので builtin
    plugin は note_id ↔ 歌詞 / 合成 wav frame offset を正しく引ける)
    — **r.md #75 で `common::plugin_metadata::sing_note_id(clip.id, note.id)` へ
    置換済 (歴史的経緯)**。通し index は「クリップ先頭に 1 音足すと以降の全 note_id が
    ずれる」欠陥があり、両側 (daw_gui / daw_audio) が独立に数え直していた。
    現在は同じ関数を両側が呼ぶ安定 id (アーキ不変条件 1)。
    合成そのものの分割 (フレーズ / 塊) の設計正本は
    [`plan_rmd_75_voicevox_phrase.md`](plan_rmd_75_voicevox_phrase.md)。
  - `daw_plugin_host::clap_plugin`: `clap_event_note.note_id` field を
    encode/decode で host ↔ plugin 間に伝搬 (`-1` = "未指定" sentinel
    対応)
  - `daw_plugin_host::vst3_plugin`: VST3 NoteOnEvent / NoteOffEvent の
    `noteId` field に同様に伝搬
  - `daw_plugin_host::builtin::voicevox::VoicevoxBuiltin`:
    `active_voice: Option<Voice>` を `active_voices: Vec<Voice>` に
    拡張、 process() で note_on event 受信時に
    `synth_result.note_offsets[note_id]` から wav 開始 frame を取得して
    voice 起動。 同 note_id 連続 trigger は voice 上書き (後勝ち)。
    note_off は無視 (= VOICEVOX 出力は envelope を内包、 wav 終端で自然
    停止)
- ✅ **PR-V3 (後段)**: 旧 project file の vocal track migration
  - `AppData::migrate_legacy_vocal_tracks(&mut Song)`: `track.source =
    Vocal` で `track.instrument` が `None` の track を `PluginInstance
    { format: Builtin, plugin_id: BUILTIN_ID_VOICEVOX, ... }` に書き換え
  - `action_open_path` (project load) と `action_new` (Song::default)
    の両方で migration を実行 → `restore_plugin_from_song` 経由で
    `SetSlotPlugin` が plugin host に飛ぶ
  - 結果、 旧 project を開いた瞬間 vocal track が builtin VOICEVOX に
    切り替わり、 通常の instrument plugin path で再生される
- ✅ **PR-V4**: 旧 vocal codepath 全削除 (= net negative diff)
  - `daw_audio::engine::process_track_owned` の vocal block 削除
    (= 約 50 行)、 `track.instrument.is_none()` gate も含めて。 vocal
    track は通常の Instrument 段階で builtin VOICEVOX が処理する
  - `EngineShared::generated_audio_store` field 削除
  - `DispatchShared::generated_audio_ptr` 削除
  - `process_track_owned` / `dispatch_and_wait` / `compile_audio_schedule`
    の `generated_audio_store` 引数を全廃 (= caller chain 全更新)
  - `MainToChild::SetGeneratedAudio` IPC variant 削除 +
    `daw_audio` main の handler 削除
  - `AudioSourcePath::Generated` 経路は warn ログ + skip (= 旧 project
    の generated source は無音再生、 builtin plugin が代替で synth)
  - `AppEvent::SynthesizeVocal` / `VocalSynthCompleted` /
    `AppData::synth_result` field / `begin_vocal_synth` /
    `finish_vocal_synth` 全削除
  - View 側: `transport.rs` の「Synth (V)」 ボタン削除、 `view::root::
    daw.synthesize_vocal` shortcut 無効化
  - `daw_gui::script::daw_set_generated_audio` を no-op (= warn ログ
    のみ)。 既存 test scripts (`pdc_mcenter.js` /
    `pdc_mcompressor_sidechain.js`) は `setGeneratedAudio` で click
    signal を inject していたので、 関連 integration test 2 件
    (`pdc_real_mcenter_aligns_master_output` /
    `sidechain_real_mcompressor_pipeline_does_not_crash`) を `#[ignore]`
    化。 別 PR で「audio clip + ImportAudio 経由の test 用 inject path」
    を整備する想定

すべての PR-V (V1 / V2.1〜V2.5 / V3 前後段 / V4) が完了。 VOICEVOX
専用 codepath は audio engine / IPC / GUI から完全に消え、 業界標準どおり
「instrument plugin として MIDI 経由で駆動」 の世界観に到達。

- ✅ **PR-V3 (前段)**: vocal track の builtin VOICEVOX auto-load + 歌詞
  flush 経路 (= 既存 vocal block と並列で動かせる、 二重再生は
  `track.instrument.is_some()` の audio engine 分岐で自動回避)
  - `action_add_vocal_track`: track 作成と同時に builtin VOICEVOX を
    `SetSlotPlugin` で load 要求、 track.instrument に
    `PluginInstance { format: Builtin, plugin_id: BUILTIN_ID_VOICEVOX
    , state: None }` を pre-fill (= 後続 `SlotPluginLoadedFromChild`
    handler が format = Clap default で上書きするのを防ぐ)
  - `AppData::sync_vocal_metadata()` 新設: 全 vocal track の clip notes
    を `NoteMetadata` 配列に変換 → `SetBuiltinPluginNoteMetadata` で
    plugin host に送信。 plugin_id 未確定の track はスキップ
  - 呼び出し hook: `sync_song_to_plugin_host` 末尾 + `on_plugin_loaded
    _from_child` (slot == Instrument) (= load 完了通知の直後に flush)
  - 既存 project (= track.source = Vocal だが instrument = None) は
    legacy vocal block で動作、 影響なし
  - daw_audio 側変更なし: `process_track_owned` の vocal block は
    `song_track.instrument.is_none()` で gate 済 (= builtin load 後は
    自動 skip、 二重再生回避)
- ✅ **PR-V2.5**: state save / restore 完全対応 (PR-V2.3 と並行で着手)
  - `LoadedPlugin::state_load(&mut self, ...)` に trait method の signature
    変更。 ClapPlugin / Vst3Plugin / Silence / VoicevoxBuiltin すべての
    impl を `&mut self` に追従、 inherent fn (`ClapPlugin::state_load`)
    も合わせて mutable 化
  - `VoicevoxBuiltin::state_load` で `self.state` を decode 結果で実際に
    更新 (= speaker_id / style_name が project file から復元される)。
    合成 cache は state に含めず、 restore 直後は cache miss = 無音、
    project load 完了後の `set_note_metadata` flush で synth が走り cache
    が温まる
  - `daw_plugin_host` main: `let mut plugin = ...` に変更 (= mutable
    receiver の load_plugin 後 `plugin.state_load(&bytes)` 呼び出しに
    対応)
- ✅ **PR-V2.3**: HTTP synth + cache + process MVP 完成
  - `NoteMetadata` を `(note_id, start_beat, duration_beats, pitch,
    velocity, lyric)` に拡張、 IPC + trait method の signature に `bpm`
    引数を追加 (= note の frame offset 計算に必要)
  - `common::voicevox::synthesize_notes_for_builtin(notes, bpm,
    speaker_id) -> BuiltinSynthOutput { samples, sample_rate,
    note_offsets }` 新設、 既存 `synthesize_sing_clip` を流用しつつ
    `note_id → 合成 wav 内 frame offset` の対応表を返す。 `BuiltinNoteSpec`
    も SDK 境界として `model::Note` から独立
  - `VoicevoxBuiltin` に背景 synth thread (`voicevox-builtin-synth`) を
    spawn、 `set_note_metadata` で job 投入 (= 連続 flush は coalesce
    で最後の 1 件のみ synth)。 結果は `Arc<RwLock<Option<SynthResult>>>`
    で audio thread と共有
  - `process()` MVP: note_on event 受信 → synth_result から voice 生成
    → mix。 1 voice 単位で wav 全体を流す (= per-note voice / global
    transport sync は PR-V2.4)
  - `Drop` 実装で synth thread を join、 deactivate でも同様
  - unit test 2 件追加 (synth_result 無しで note_on 来ても無音、
    synth_result 仕込みで voice が wav を drain する); 既存 test 含めて
    合計 15 件 pass

## 動機 — なぜ専用 codepath を捨てるか

現状 daw_01 は VOICEVOX 合成のために **専用 codepath** を持つ:

- `common::model::InstrumentSource::Vocal { speaker_id, style_name }` で
  track 種別を分岐
- `daw_audio::engine::process_track_owned` 内に **vocal block** (VOICEVOX
  合成済 audio を `EngineShared::generated_audio_store` から拾って再生)
  と **plugin block** (CLAP / VST3 process()) が並列
- `MainToChild::SetGeneratedAudio` / `EngineShared::generated_audio_store`
  という汎用化はされているが、 VOICEVOX 用のループ閉路が `daw_gui` 側に
  あって complexity が高い

これは**業界標準と乖離**している:

- VOCALOID 5 / 6 / Synthesizer V Studio / CeVIO AI / NEUTRINO は **すべて
  VST3 instrument plugin として配布** されている
- DAW (Cubase / Studio One / FL Studio / REAPER 等) は何も特別扱いしない:
  ただの「instrument track + plugin slot」
- 入力 = MIDI ノート + 歌詞 (= note metadata、 VOCALOID 規格では NoteOn の
  user data 領域に phoneme を載せる)
- 出力 = stereo audio (plugin の output bus)

DAW 側に「歌唱合成専用 codepath」 を持つのは **アンチパターン**。 ゼロ
から作るなら、 VOICEVOX も同じ instrument plugin の枠で扱う。

## 理想設計 — VOICEVOX を内蔵 instrument plugin として扱う

### model 変更

- `InstrumentSource::Vocal { speaker_id, style_name }` を **廃止**
- すべて `InstrumentSource::Vst3 { path }` 相当の plugin slot に統一
- VOICEVOX 用には「**daw_01 内蔵 instrument plugin**」 を 1 つ実装し、
  ユーザーが instrument plugin slot に load することで vocal track 化
  (= track.kind 分岐は不要、 普通の instrument track)
- VOICEVOX plugin の選択肢 (speaker / style) は plugin 自身の parameter
  として持つ (= UI は plugin GUI、 自動化も plugin parameter automation)

### audio engine 変更

- `process_track_owned` の vocal block を **削除**
- すべての track が同 codepath: MIDI clip → instrument plugin process()
  → audio out → fx_chain → master
- `EngineShared::generated_audio_store` は廃止、 もしくは builtin plugin
  内部の audio cache に置き換え (Bounce In Place の export pipeline と
  共有可能なら shared)

### IPC 変更

- `MainToChild::SetGeneratedAudio` を廃止 (= VOICEVOX 経路は plugin が
  自分で HTTP を呼ぶので、 daw_gui → daw_audio へ buffer を流す経路は
  不要)
- `MainToChild::SynthesizeVocal` 等の VOICEVOX 制御 IPC も廃止
- 代わりに plugin parameter / state save で speaker_id / style_name /
  歌詞 cache を保持

### 歌詞をどう note に乗せるか

CLAP / VST3 の MIDI events に「歌詞 string」 を直接載せる規格は無いので、
以下のどれかで対応する:

- **(α)** plugin 側に「歌詞テキスト + note 配列」 の parallel buffer を
  持たせ、 `clap_note_event.event_id` を index として歌詞を引く。 plugin
  state に歌詞バッファを serialize
- **(β)** CLAP の `NoteExpression` (= per-note parameter) で phoneme を
  整数 ID に encode し、 phoneme dictionary は plugin 内蔵
- **(γ)** plugin の sysex-相当 event (CLAP `EVENT_PARAM_VALUE` の特殊
  ID) で「note_id → 歌詞」 を flush し、 plugin が context 化する

最初は **(α)** が最小実装: VOICEVOX plugin は CLAP plugin だが daw_01
内蔵なので、 `daw_plugin_host` が plugin の plain mutable struct に直接
歌詞バッファを差し込める拡張 API を持てば、 規格逸脱なし。 外部 plugin
化したくなったら CLAP NoteExpression で標準化を検討 (= Phase 5+)。

## 段階ロードマップ

各 PR は build / clippy / test --features rt-assert clean を確認 + commit。

### PR-V1: builtin plugin インフラ (= plugin_host 拡張)

**スコープ**:

- `daw_plugin_host` crate に `BuiltinPlugin` trait + registry 実装
  (CLAP factory に「daw_01 builtin」 という偽 path を予約、 同 path で
  factory_load が呼ばれたら builtin descriptor を返す形)
- builtin plugin は `Box<dyn BuiltinPlugin>` として持ち、 process() は
  `clap_plugin.process` と同 signature で呼べる
- `PluginFormat::Builtin` という新 variant を `common::plugin_format` に
  追加、 protocol で routing
- builtin plugin は state save / restore 対応 (= plugin parameter は
  serde で project file に乗る)

**規模**: ~600-800 行。 VOICEVOX 合成は **PR-V2** で別途実装。

### PR-V2: VOICEVOX builtin plugin 実装 (= sub-PR 分割)

合計規模 ~800-1200 行 + 新 host API 設計が必要なため、 5 つの sub-PR に
分割して 1 セッション完結のリスクを下げる。

#### PR-V2.1: skeleton (✅ 完了)

- `daw_plugin_host::builtin::voicevox` module 新設、 `VoicevoxBuiltin`
  struct を `LoadedPlugin` 実装 (= 現状 process() は無音を返すだけ)
- `VoicevoxState { speaker_id, style_name }` struct + bincode encode /
  decode で project file persistence の skeleton
- `BUILTIN_ID_VOICEVOX = "builtin://daw_01.voicevox"` を `common::
  plugin_db` に追加、 `builtin_descriptors()` で picker UI に露出
- `daw_plugin_host` crate に bincode dependency 追加
- unit test 5 件追加 (default state / bincode roundtrip / id-format /
  silent process / state_save bytes)

`state_load` は `LoadedPlugin` trait が `&self` 受け取りなので、
parse の妥当性確認だけで self.state は default のまま (= 既存 user の
speaker / style 選択は復元されない、 PR-V2.5 で trait 拡張 + full
restore に置換予定、 影響範囲は 2 fields のみで user 再選択で復旧可能)。

#### PR-V2.2: 歌詞付き MIDI events を builtin plugin に渡す host API

- `LoadedPlugin` trait か builtin 専用 sidecar (= `BuiltinPluginCtx`) に
  「note_id → 歌詞 string」 map を渡す method を追加
- daw_audio 側: vocal track の clip notes から歌詞 + note_id を抽出して
  plugin host に送る経路 (= 既存 `process_track_owned` の vocal block
  からのコード移行)
- daw_gui 側: notes 編集時に「歌詞変更」 を builtin plugin に flush
  する経路 (= edit → IPC → plugin の歌詞バッファ更新 → bulk re-synth)

**規模**: ~200-400 行。

#### PR-V2.3: HTTP synthesis + cache 統合

- `common::voicevox::synthesize_song` 経由の bulk synth を `VoicevoxBuiltin`
  内部から呼ぶ経路
- `common::voicevox_cache` の per-note cache を builtin plugin の
  internal cache として再利用 (= LRU / disk persistence)
- HTTP client は **plugin 内部で tokio runtime の handle を host から
  借りる** (= audio thread block 回避、 cache miss は silence + 背景
  fetch)

**規模**: ~300-400 行。

#### PR-V2.4: process() で cache 引き → mix integration

- `process()` 内で cache から該当 note の audio を取り出し、 buffer の
  時間軸に合わせて output_l / output_r に書き込む
- pitch / velocity / 音量 envelope は VOICEVOX 側合成済なので host は
  単純に WAV を流すだけ
- cache miss 時の placeholder (= 無音) と「synth 中」 の状態通知

**規模**: ~150-250 行。

#### PR-V2.5: state save / restore 完全対応 + trait API 拡張

- `LoadedPlugin::state_load(&mut self, ...)` への trait API 変更
  (= CLAP / VST3 / Builtin すべての backend を更新)
- `VoicevoxBuiltin::state_load` で speaker / style + cache を full
  restore (= bincode embed)
- migration helper: 旧 state 形式 (= speaker_id / style_name のみ) も
  読める後方互換性 (= bincode optional field)

**規模**: ~150-250 行。

### PR-V3: Vocal track の builtin plugin 切替

**スコープ**:

- 既存 project file を読むときの migration: `InstrumentSource::Vocal {
  speaker_id, style_name }` を読んだら、 builtin VOICEVOX plugin slot に
  自動変換 (= speaker_id / style_name を plugin parameter として復元)
- `daw_gui` の「Add Vocal Track」 メニューを「Add Instrument Track →
  builtin VOICEVOX を auto-load」 に変更 or 別途「Add VOICEVOX Track」
  shortcut を残す
- track_inspector の Vocal 関連 UI を plugin GUI 経由に統合

**規模**: ~400-600 行 + project file migration。

### PR-V4: 専用 vocal block 削除

**スコープ**:

- `daw_audio::engine::process_track_owned` の vocal block 削除
- `MainToChild::SetGeneratedAudio` / `MainToChild::SynthesizeVocal` 等の
  VOICEVOX 用 IPC variant 削除
- `EngineShared::generated_audio_store` を削除 (= もう使ってない)
- `daw_gui::AppEvent::SynthesizeVocal` / `VocalSynthCompleted` 等の
  VOICEVOX UI ループ削除 (= plugin 内部完結)

**規模**: ~300-500 行 (= 削除中心、 net negative diff)。

### PR-V5 (任意): VOICEVOX plugin の外部 VST3 化

**スコープ**:

- builtin plugin として実装した VOICEVOX を **本物の VST3 plugin** として
  ビルドできるように crate 構造を整える (= cdylib + VST3 SDK)
- 他 DAW でも VOICEVOX が使えるようになる (= 公開 OSS 化候補)
- 歌詞は CLAP NoteExpression / VST3 NoteExpression で標準化
- 規模超大型なので任意 (= daw_01 内で動けばまずは十分)

## 後方互換性

- 既存の `daw_01` project file (= `InstrumentSource::Vocal { ... }` を
  保存済) は PR-V3 の migration で自動変換 (= 1 度開いて save すれば
  builtin plugin slot を持つ新形式に書き換わる)
- migration 失敗時は status_message + skip (= track は instrument
  source なし状態で再生される、 ユーザーが手動で plugin load 可能)

## なぜ今すぐ着手しないか

- **規模超大型**: 全 PR で ~2000-3500 行 + IPC + plugin host 拡張
- **builtin plugin インフラ自体が複雑**: CLAP / VST3 host から見て
  「内蔵 plugin」 を扱う仕掛けは前作 sing_like_coding にもないので新規
  設計
- **代替案 (= 現状 vocal block) は動いている**: 機能復旧優先 (= memory
  「機能復旧 > 新機能」) のため、 既存機能を壊して着手する強い動機がない
- **Phase 3+ の Stretch / Slice 等の audio 高度機能と並行進められる**:
  vocal codepath 整理よりも、 audio editor 完成度向上のほうが UX
  影響が大きい

ただし「将来の正解はこれ」 が明確になったので、 中間妥協の (a) (b) (c)
(d) はもう検討不要 (= 雑な統合で技術負債を増やすだけ)。

## 着手判断のトリガ

以下のいずれかが起きたら本 plan に着手:

1. ユーザーが「VOICEVOX 統合に着手して」 と明示的に指示
2. Phase 3+ で Stretch / Slice 着手前の負債整理として優先度が上がる
3. VOICEVOX plugin 化を開発者コミュニティ向けに公開する話が出る
4. 既存 vocal block が deadlock / 性能問題で機能不全になる
   (= memory「機能復旧 > 新機能」 で優先度が逆転)
