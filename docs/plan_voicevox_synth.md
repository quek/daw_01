# VOICEVOX 合成の builtin instrument plugin 化 計画

ステータス: **着手中** (2026-05-08、 PR-V1 完了)。 `plan_audio_followup.md`
後回し 2「VOICEVOX → ClipContent::Audio 統合」 を「ゼロから作るなら」
の観点で再設計したもの。

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
