# VOICEVOX 合成の builtin instrument plugin 化 計画

ステータス: **設計合意待ち** (2026-05-08)。 `plan_audio_followup.md`
後回し 2「VOICEVOX → ClipContent::Audio 統合」 を「ゼロから作るなら」
の観点で再設計したもの。 着手は別セッションで段階実装。

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

### PR-V2: VOICEVOX builtin plugin 実装

**スコープ**:

- `daw_plugin_host::builtin::voicevox` module 新設
- 入力: MIDI note events + 並列歌詞バッファ (= 歌詞 string 列、 note 順)
- 出力: stereo audio (engine_sr)
- 内部状態:
  - speaker_id / style_name (= plugin parameter)
  - HTTP client (= reqwest async, tokio runtime はホスト側持ち)
  - 合成済 audio cache (`note_id → AudioBuffer`、 LRU / per-note 永続)
  - 起動時に bulk synth (= clip の note 列を一括で `singing_query` →
    `singing_synthesis`)、 cache に格納
- process() は cache から該当 note の audio を時間軸に合わせて mix

**設計判断**:

- HTTP は plugin 内部で **tokio runtime の handle を host から借りる**
  (= host が `cap_host_audio_thread_handle` 的な拡張を builtin plugin に
  渡す)。 audio thread からは block しないため、 cache miss 時は dummy
  silence を返して synth thread で背景 fetch
- 合成 cache は **plugin state に serialize** (= save 時に WAV-PCM とし
  て embedded)。 project file size は数 MB / 数十秒 vocal なので 許容

**規模**: ~800-1200 行 (HTTP 経路 + cache + process() 統合)。

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
