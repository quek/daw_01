# plan: VOICEVOX トーク(読み上げ)機能 + 字幕デバイス化

`/grill-me` (2026-06-20) で設計確定。M2 送りにしていた `/audio_query` → `/synthesis`
(トーク) を一級機能として実装する。あわせて、トークが Text clip を流用する帰結として
**テキストオーバーレイ表示を「字幕デバイス」でゲートする**破壊的再設計を行う。

---

## 0. 確定した設計判断 (grill-me)

| # | 分岐 | 確定 |
|---|------|------|
| 1 | トラック構成 | 歌唱と**同じ VOICEVOX トラックに混在**。clip の content 種別で自動判別 |
| 2 | セリフの入力元 | **`ClipContent::Text` をそのまま流用**(字幕=読み上げ原稿を 1 文字列に統一) |
| 3 | 表示 vs 読み上げ | **デバイスゲート式**。挿さっているデバイスで振る舞いが決まる |
| 4 | デバイス分類 | **2 つの別 builtin**: ①字幕(表示)デバイス ②VOICEVOX(読み上げ)デバイス。直交 |
| 5 | タイミングと長さ | **完全独立**。字幕表示時間は手動、読み上げは行頭で発火・自然な長さ。自動一致なし |
| 6 | 声の単位 | **per-clip**。`Clip::speaker_id` を talk style として流用(歌唱と SSoT 統一) |
| 7 | 声の調整 | **全体スケールのみ(clip 単位)**: 話速/音高/抑揚/音量。アクセント編集なし |
| 8 | 口パク | **対応**。talk の phoneme を既存 MouthMap→ImageEvent 機構へ流用 |
| 9 | 誤読修正 | **辞書/読み欄なし**。難読語は表記そのものをかな等で書き分ける |
| 10 | 字幕デバイス移行 | **既存は自動移行**(Text 持ちトラックへ auto-insert)・**新規は明示挿入** |

### 統一モデル (clip の振る舞いはトラックのデバイスで決まる)

| トラックのデバイス | Text clip | MIDI clip |
|---|---|---|
| VOICEVOX のみ | **読み上げ**(画面に出ない) | 歌う(既存) |
| 字幕のみ | **画面表示**(従来の overlay) | — |
| VOICEVOX + 字幕 | **表示 + 読み上げ**(字幕=ナレーション) | 歌う |
| どちらも無し | 不活性 | 不活性 |

`is_voicevox_vocal()`(= device 実在が SSoT)と同思想を表示にも適用。FIXME #54 の
「映像効果を `Track.devices` に統合」の延長線上。

---

## 1. データモデル変更 (`common/src/model.rs`)

### 1.1 Clip に talk プロソディスケールを追加

声は既存 `Clip::speaker_id` を流用(Text clip では talk style id、MIDI clip では sing
style id と解釈。content 種別で分岐)。プロソディは clip 単位なので Clip に持たせる:

```rust
/// (talk) VOICEVOX 読み上げの全体スケール。Text clip が VOICEVOX デバイス付き
/// トラックに居るときだけ意味を持つ。値は VOICEVOX AudioQuery の同名フィールドに
/// そのまま渡す(speed/pitch/intonation/volume + 前後無音)。`None` = 既定値。
#[serde(default, skip_serializing_if = "Option::is_none")]
pub talk: Option<TalkParams>,
```

```rust
#[derive(... Serialize, Deserialize, Encode, Decode)]
pub struct TalkParams {
    pub speed_scale: f32,      // 既定 1.0
    pub pitch_scale: f32,      // 既定 0.0
    pub intonation_scale: f32, // 既定 1.0
    pub volume_scale: f32,     // 既定 1.0
    // 任意: pre/post_phoneme_length (秒)。MVP は AudioQuery 既定のまま
}
```

`Option` で旧プロジェクト互換 + 「全部既定」clip は serialize されない。`bincode`
derive 追加 → **protocol 型なので §7 で workspace build**。

> 注: TextEvent には talk 用フィールドを足さない(声/スケールは per-clip = Clip 側)。
> TextEvent は表示属性のみのまま。複数 TextEvent は clip の 1 声・1 スケールで読む。

### 1.2 字幕デバイスの builtin id

`builtin.video.*` 系の marker device として追加(`common/src/plugin_db.rs` /
`common/src/video_fx.rs`):

```rust
pub const SUBTITLE_ID: &str = "builtin.video.subtitle";
```

- shader 効果(`VideoFxDef`)ではなく**表示ゲート marker**。pixel pipeline は走らせず、
  `text_compose.rs` が「このトラックに `SUBTITLE_ID` が在るか」で表示を gate する。
- plugin picker に出すための descriptor は `plugin_db::builtin_descriptors()` に追加。
  パラメータ無し(将来テキスト既定値を持たせる余地は残す)。
- `Track` に `pub fn has_subtitle_device(&self) -> bool`(`is_voicevox_vocal` と同 idiom)。

---

## 2. 字幕(表示)デバイスゲート化 (`daw_gui/src/text_compose.rs`)

現状 `active_text_sources_at` は全トラックを走査し、`ClipContent::Text` を無条件で
overlay 化している(`text_compose.rs:85`、device 非依存)。これを **`SUBTITLE_ID` を
持つトラックの Text clip だけ**に絞る:

```rust
for track in song.tracks.iter().rev() {
    if song.track_visually_silenced(track.id) { continue; }
    if !track.has_subtitle_device() { continue; }   // ★ 追加: 表示ゲート
    ...
}
```

- export 経路 (`render_video.rs:572` 同関数を使用) も同 gate で一貫。
- 既存テストの fixture は字幕デバイスを挿した track を組むよう更新(§7)。

---

## 3. VOICEVOX デバイスの talk 拡張 (本丸)

歌唱は「MIDI clip の notes を per-clip 声で 1 本に合成 → note_on で cursor jump」。
talk はこれと**並走**する経路を builtin に足す。RT path(`process()`)は不変方針を踏襲。

### 3.1 talk 合成 API (`common/src/voicevox.rs`)

- 既存 `synthesize_talk(client, text, speaker_id)`(audio_query → synthesis)を、
  **AudioQuery にスケールを適用**してから synthesis する形に拡張:
  - `audio_query` のレスポンス JSON に `speedScale`/`pitchScale`/`intonationScale`/
    `volumeScale` を patch(`outputSamplingRate` patch と同じ要領)してから `/synthesis`。
- **talk 用 phoneme 抽出**を新設(口パク用、§5):
  ```rust
  pub fn query_talk_phonemes(text: &str, speaker_id: u32, scales: &TalkParams)
      -> Result<Vec<Phoneme>>
  ```
  audio_query の `accent_phrases[].moras[]`(consonant/vowel + `*_length` 秒)と
  `pause_mora` を、既存 `Phoneme { phoneme, frame_length }`(`FRAME_RATE` 換算)へ
  変換。これで歌唱用 `build_mouth_events` をそのまま再利用できる。
- builtin 向けラッパ `synthesize_talk_for_builtin(text, speaker_id, scales)
  -> BuiltinSynthOutput`(mono PCM + sample_rate + 1 entry の note_offset)を追加。

### 3.2 flush 経路 (`daw_gui/src/app.rs::sync_vocal_metadata`)

現状は VOICEVOX トラックの MIDI clip の notes を `NoteMetadata` に flatten して
`SetBuiltinPluginNoteMetadata` で送る。これに **Text clip の TextEvent を talk
エントリとして混ぜる**。`NoteMetadata` を拡張するのではなく、**talk 用の別メタデータ**
を protocol に足すのが clean:

```rust
// common/src/plugin_metadata.rs
pub struct TalkMetadata {
    pub event_id: u32,         // track 内通し番号(note_id 空間と衝突しない採番)
    pub start_beat: f64,       // song-absolute(clip.start + event_start_in_clip)
    pub text: String,
    pub speaker_id: u32,       // talk style(clip.speaker_id)
    pub speed_scale: f32, pub pitch_scale: f32,
    pub intonation_scale: f32, pub volume_scale: f32,
}
```

- 新 protocol `MainToChild::SetBuiltinPluginTalkMetadata { track, entries: Vec<TalkMetadata> }`
  (または既存 metadata msg を `{ notes, talk }` の 2 リストに拡張)。
- `sync_vocal_metadata` は同じ VOICEVOX トラックについて、MIDI clip → note 群、
  Text clip → talk 群、の両方を集めて flush。

### 3.3 builtin 側 (`daw_plugin_host/src/builtin/voicevox.rs`)

- `set_talk_metadata(bpm, entries)` を追加 → synth thread の `SynthJob` に talk グループ
  を載せる。synth thread は talk エントリごとに `synthesize_talk_for_builtin` を呼び、
  歌唱と同じく **song-absolute サンプル位置へ配置した 1 本の buffer** に mix。
  `note_offsets` 相当(`event_id → buffer offset`)も累積。
- `process()` は不変。note_on(歌唱)と talk-trigger(§3.4)はどちらも
  `note_offsets` を引いて cursor jump する**同一機構**で扱える(id 空間が分離していれば
  衝突しない)。
- coalesce / retry / shutdown / generation 機構はそのまま流用。

### 3.4 再生トリガ (`daw_audio/src/sequencer.rs`)

歌唱は MIDI note の On を buffer 窓内で emit している。talk は note が無いので、
**TextEvent の song-absolute start に合成 note_on(`event_id` を note_id として)を
emit** する。`collect_*` の vocal 経路に「Text clip の各 TextEvent start で
`NoteTransition::On { note_id: event_id, key: 0, velocity: 1.0 }` を出す」分岐を足す。
note_off は不要(builtin は wav 終端で自動 drain、talk も同様)。

> id 空間の分離: note_id(歌唱)= clip 跨ぎの note 通し番号、event_id(talk)= 別オフセット
> 帯(例: `TALK_ID_BASE` から)で採番し、builtin の単一 `note_offsets` map で共存させる。
> `sync_vocal_metadata`(flush)と sequencer(trigger)で同じ採番規則を共有するのが要。

### 3.5 ライブ再合成

歌唱と同じく、テキスト/声/スケール編集 → `sync_vocal_metadata` 再 flush →
builtin の coalesced background synth が走る(bounce 不要)。VOICEVOX engine は
既存 `ensure_voicevox_engine` の lazy spawn を流用。

---

## 4. UI (`daw_gui/src/view/track_inspector.rs`)

選択中 clip が **`ClipContent::Text` かつ トラックが VOICEVOX デバイス付き** のとき、
インスペクタに「読み上げ(Talk)」セクションを描く(既存 per-clip section の隣):

- **話者(talk style)ドロップダウン**: `/speakers`(talk)を fetch して 2 段
  (キャラ → スタイル)。既存の歌唱 singer picker(`/singers`)と同 idiom。選択は
  `clip.speaker_id`(talk style id)へ焼き込み。`AppEvent::SetClipVoice` を talk 文脈で流用
  or 新 `SetClipTalkVoice`。
- **4 スケール**: 話速/音高/抑揚/音量を `scrubable_number`(既存 idiom、
  [[feedback_reuse_inspector_idiom]])で。`clip.talk` を編集 → 再 flush。
- 字幕(表示)の有無は別途デバイスチェーンに字幕デバイスが在るかで決まる。
  Text clip 選択時、トラックに字幕デバイスが無ければ「字幕デバイス未挿入=非表示」
  ヘルパ + ワンクリック追加ボタンを出す(Q10)。

`/speakers` fetch は `fetch_singers` と同様の background thread + `AppEvent` で
`app.talk_speakers` に保持。engine ready 後に 1 回 + 再取得ボタン。

---

## 5. 口パク (`common/src/lipsync.rs` + `daw_gui` 側生成)

歌唱は vocal clip の notes+phonemes から target 口トラックへ `auto_lipsync`
ImageEvent を生成している(`Track::lipsync_target_track` / `mouth_map`)。talk も
**同じ target / mouth_map** を使い、`query_talk_phonemes`(§3.1)の phoneme 列から
`build_mouth_events` で口 ImageEvent を生成する。

- 生成タイミング: 歌唱と同じ口パク再生成経路に Text clip を追加(`auto_lipsync`
  clip は再生成で全削除→再構築)。
- talk phoneme は秒単位長 → `FRAME_RATE` 換算で歌唱 phoneme と同じ frame_length へ。
- 先頭/末尾の pause も含めて配置。**先頭無音は `prePhonemeLength = 0`
  (`common::voicevox::TALK_PRE_PHONEME_LENGTH`) を合成時に注入して消す**ので、
  `build_mouth_events` に渡す anchor は素の TextEvent 開始位置になる (r.md #39)。
  engine 既定 0.1s のまま残すと先頭無音が `speedScale` で割られ、話速 0.5 で
  +96ms / 1.5 で −43ms と発話位置が動いてしまう。`parse_talk_phonemes` も応答の
  `prePhonemeLength` ではなく同じ定数を見る (音声と口の SSoT)。

---

## 6. 移行 (`common/src/project.rs` load 前処理)

既存テキストオーバーレイは「全トラック常時表示」。デバイスゲート化で消えないよう、
**load 時に `ClipContent::Text` を 1 つ以上持つ全トラックへ `SUBTITLE_ID` device を
auto-insert**(既に在れば no-op)。

- `migrate_vocal_source_to_clips`(FIXME #36)と同じ JSON Value 前処理層、または
  `Song::normalize_after_load` に hook。idempotent。
- 新規 Text clip(`AddTextClipAt`)は **auto-insert しない**(ゲートを意味あるものに保つ。
  Q10)。表示したいユーザーは字幕デバイスを明示挿入 or §4 のヘルパで 1 クリック追加。
- 回帰テスト: 旧 .daw(字幕デバイス無し + Text clip)→ load 後に字幕デバイスが付き
  表示が保たれる。

---

## 7. ビルド & 検証

- **protocol 変更**(`Clip.talk` / `TalkParams` / `TalkMetadata` / 新 `MainToChild`)は
  bincode wire format を変えるので **`cargo build --workspace` 必須**
  ([[feedback_workspace_build_for_protocol_changes]])。`daw_audio.exe` /
  `daw_plugin_host.exe` も再生成。
- `cargo clippy --workspace -- -D warnings` / `cargo test --workspace`。
- 非自明ロジックのみ unit test([[feedback_no_tests_for_simple_cases]]):
  - `query_talk_phonemes`(audio_query JSON → Phoneme 換算、固定 JSON fixture)。
  - AudioQuery へのスケール patch。
  - 移行(旧 .daw → 字幕デバイス auto-insert)。
  - text_compose の device gate(字幕デバイス有/無で表示が出る/出ない)。
- **実機検証(最後に 1 度、VOICEVOX engine 起動下)**:
  1. トラックに VOICEVOX デバイス → Text clip にセリフ → 再生で**喋る**(画面に字幕は出ない)。
  2. さらに字幕デバイスを挿す → 同じテキストが**字幕表示 + 読み上げ**。
  3. 4 スケール変更 → 声が変わる。複数 TextEvent が各行頭で順に喋る。
  4. mouth_map 設定済みトラック → 読み上げに合わせて**立ち絵の口が動く**。
  5. 旧プロジェクト読込 → 既存テキストが従来どおり表示される(字幕デバイス auto-insert)。
- `cargo build --workspace --release`(commit 後 hook + 自分でも green 確認)。

---

## 8. リスク / 注意

- **混在トラックの id 空間**(§3.4 が最難所): 1 トラックに歌唱 MIDI clip と talk Text
  clip が同居するとき、note_id(歌唱)と event_id(talk)を builtin の単一
  `note_offsets` で衝突なく共存させ、flush(daw_gui)と trigger(daw_audio sequencer)で
  採番を完全一致させる。ここがずれると「鳴らない / 別 clip を鳴らす」。
- **`Clip::speaker_id` の二重解釈**: MIDI clip では sing style、Text clip では talk style。
  inspector の picker と合成経路が content 種別で正しく分岐すること。
- **字幕デバイスは非オーディオ device**: `Track.devices` に audio を流さない marker が
  混じる。audio graph compile / device chain UI が `builtin.video.*` を既に無害に
  扱えているか確認(transform device の前例あり)。
- talk は TextEvent ごとに `audio_query` + `synthesis` の HTTP 2 往復。多数行は
  coalesced background synth + cache(notes ベースの既存 cache を text+scale ベースへ拡張)で
  緩和。engine 起動前は retry pending(歌唱と同挙動)。

---

## 9. 実装順序

1. **model**: `TalkParams` + `Clip.talk`、`SUBTITLE_ID` + descriptor + `Track::has_subtitle_device`。`cargo build --workspace`。
2. **表示ゲート + 移行**: `text_compose` の device gate、旧 .daw auto-insert 移行 + 回帰テスト。
3. **talk 合成**: `synthesize_talk` スケール対応 + `query_talk_phonemes` + builtin ラッパ。unit test。
4. **flush / trigger / builtin**: `TalkMetadata` + protocol、`sync_vocal_metadata` 拡張、sequencer trigger、builtin talk synth。`cargo build --workspace`。
5. **UI**: inspector talk セクション(/speakers picker + 4 スケール + 字幕デバイス追加ヘルパ)、`/speakers` fetch 配線。
6. **口パク**: talk phoneme → `build_mouth_events` 再生成経路。
7. **検証**: §7 の test + 実機 + release build。

---

## 実装結果メモ (2026-06-20、全 phase landed・clippy `-D warnings` / 全 test green / release build green)

計画どおり全 7 phase 実装完了。主な確定 / 逸脱:

- **id 空間 (§8 最難所)**: running counter ではなく **`talk_event_id(clip_id, event_index)` を
  high band (`1<<28`) に決定論的導出** (`common/src/plugin_metadata.rs`)。flush (daw_gui) と
  trigger (daw_audio sequencer) が同式で計算するので skip 計数の同期が不要 = リスクを構造的に解消。
  sing の note_id と衝突しない (r.md #75 以降は `sing_note_id(clip_id, note.id)` =
  値域 `[0, 1<<28)`。high band と定義上ちょうど接する。それ以前は「小さい通し index」だった)。
- **builtin の RT path は完全不変**: `process()` は note_id / event_id を区別せず `note_offsets` を
  引くだけ。synth thread で talk WAV を `start_beat*spb` に配置し `note_offsets[event_id]=placement`
  を足すだけ (sing と同じ `placed` バッファ機構に相乗り)。
- **字幕デバイス** = `builtin.video.subtitle` marker (shader 無し)。`has_video_input/output:true` で
  audio engine / plugin host から skip、FX executor も `def_by_id` 未ヒットで素通り。`text_compose` が
  `has_subtitle_device()` で表示を gate。
- **移行は version-gate** (`project.rs`、v25→v26): 旧 .daw の Text 持ちトラックへ字幕デバイスを
  auto-insert、新規 (v26) は対象外で「喋るが映さない」を温存。回帰テスト 3 本 + 表示 smoke test で確認。
- **声/プロソディは per-clip**: `Clip::speaker_id` を talk style として流用 (`/speakers`)、4 scale は
  `Clip::talk: Option<TalkParams>`。inspector は Text clip + VOICEVOX device 時に talk セクション
  (2 段 picker + scrubable 4 つ + 字幕デバイス追加ヘルパ)。scale は scrub 系なので非 undoable。
- **誤読**: 辞書/読み欄なし (Q9)。テキスト = 表示 = 読み上げの 1 文字列を貫く。
- **口パク**: talk の `query_talk_phonemes` (audio_query → Phoneme 換算、speed で長さ補正) を既存
  `build_mouth_events` に相乗り。anchor は「phoneme 列 frame 0 = wav 先頭」の共通契約
  (r.md #39) に従い `event 開始 − pre-silence` を渡す (pre-silence は現行 0)。
  1 Text clip = 1 発話 (先頭の非空 TextEvent) として扱う (多 event/clip の口パクは別途)。
- **再合成トリガ配線**: text 本文編集 (`set_clip_text_event_content`) / 声変更 (`set_clip_voice`) /
  scale 変更 (`set_clip_talk_param`) いずれも `sync_vocal_metadata` 再 flush + `mark_lipsync_dirty`。

### 自動検証 (完了)
- `cargo clippy --workspace -- -D warnings` clean / `cargo test --workspace` 全 pass
  (新規: talk_event_id / TalkMetadata roundtrip / apply_talk_params / parse_talk_phonemes ×2 /
  字幕デバイス移行 ×3) / `cargo build --release` (3 exe) clean。
- **表示ゲート回帰**: `daw_gui --smoke-test-text` EXIT=0 (pixel capture で text 描画を確認 =
  device-gating が既存表示を壊していない)。

### 実 VOICEVOX engine 統合検証 (done、`C:\Program Files\VOICEVOX\vv-engine\run.exe` を起動して)
`#[ignore]` 統合テスト (`cargo test -p common -p daw_plugin_host -- --ignored`):
- `synthesize_talk_for_builtin` → **非無音音声** (rms>0.001) / `query_talk_phonemes` → 母音+前後 pau /
  `fetch_speakers` → talk 話者 / builtin `set_note_metadata(talk)`→synth→非無音 synth_result+event_id offset。

### full DAW pipeline end-to-end 検証 (done、headless)
- `target/talk_fixture.daw` (VOICEVOX device + Text clip、`project::gen_talk_fixture` で生成) を
  `daw_gui --script target/talk_export.js` で load → synth 待ち → `exportWav`。
- 出力 WAV を解析: **peak=0.358 / rms=0.021、per-second peak `[0.22,0.21,0.36,0.03,0,0,…]`** =
  talk が頭 ~3.5s で鳴り後続無音 = **flush→builtin 合成→sequencer trigger→freewheel export が
  end-to-end で動作**。耳なしで「実際に喋る」を確定。
- **この検証中に実バグ発見・修正**: script harness (`script.rs::handle_incoming`) が `SlotPluginLoaded`
  で OpenPluginShmem 転送のみで `app.handle_event(SlotPluginLoadedFromChild)` を呼ばず、`loaded_slots`
  未充填 + `sync_vocal_metadata` 再 flush 欠落 → headless VOICEVOX export が無音だった。GUI runner 忠実に
  app へ dispatch するよう修正 (sing export 検証にも効く改善)。daw_gui 全 test (140+) green を確認。

### 残り = 人の主観のみ (機能は上記で証明済み)
- 声の自然さ / 意図どおりの声か (= 耳)。口パク立ち絵の「動き」の見栄え (= 目、立ち絵 setup 要)。
- inspector talk UI の操作感。機能的な「喋る/表示/口が出力される」は検証済み。
