# plan: VOICEVOX シンガー/スタイルを「クリップ単位」で選ぶ (FIXME #36)

## 目的

VOICEVOX のシンガー (キャラ) + スタイルを **Clip 単位** で選べるようにする。
現状は `InstrumentSource::Vocal { speaker_id, style_name }` という **トラック単位** の
1 声だけで、トラック上の全 vocal clip が同じ声で合成される。これを破棄し、
各 Clip が自分の確定声を焼き込んで持つ形に作り直す。

声の正体は VOICEVOX の `/frame_synthesis?speaker=<歌唱 style id>`。一覧は
`/singers` が返す歌唱 style 群 (「中国うさぎ へろへろ」等を含む)。`/sing_frame_audio_query`
のクエリ段の speaker は `QUERY_SPEAKER = 6000` 固定のまま (現状維持)。`/speakers` (トーク)
は使わない。声選択はレンダ段に渡す style id を差し替えるだけで、ハミング/歌詞処理とは直交。

---

## 0. 現状調査の結論 (実装前に把握すべき code reality)

調査で判明した、設計の前提に直接効く事実:

1. **合成の生きた経路は (b) builtin だけ。(a) `synthesize_song` は dead code。**
   - `common/src/voicevox.rs::synthesize_song` (:105) は定義されているが、`daw_gui` /
     `daw_audio` のどこからも呼ばれていない (grep 0 hit)。実際の vocal 合成は
     `daw_gui/src/app.rs::sync_vocal_metadata` (:7663) → `MainToChild::SetBuiltinPluginNoteMetadata`
     → `daw_plugin_host` の builtin VOICEVOX plugin が担う。
   - 設計が「2 経路に通す」と書く (a)(b) のうち、**(a) は今は死んでいる**。本 plan では
     (a) も per-clip 声を読むよう作り直す (将来の offline render / WAV export で復活する
     可能性があり、声 SSoT を 1 つに保つため) が、**実機で効くのは (b)**。検証は (b) 中心。

2. **builtin は「トラック全 clip を 1 本に連結 → 1 speaker で 1 回 synth」している。**
   - `sync_vocal_metadata` (:7697-7719) は track の全 clip の notes を `note_id =
     entries.len()` の通し番号で 1 つの `Vec<NoteMetadata>` に flatten し、`bpm` +
     `entries` を 1 plugin に送る (clip 境界も声の区別も無い)。
   - builtin 側 (`daw_plugin_host/src/builtin/voicevox.rs`) は `SynthJob { bpm, speaker_id,
     notes }` (:69) を 1 本作り、`synthesize_notes_for_builtin` (:550) で **1 本の WAV** を
     合成。`process()` (:412-451) は note_on の `note_id` で `note_offsets` を引いて単一
     WAV 内を cursor jump して再生する (= rolling 単一 voice)。
   - **→ per-clip 声は、この「track 単一 speaker・単一 WAV」前提を壊して clip 単位の
     synth job / WAV / voice に作り直すのが本丸**。データモデルだけ直しても音は変わらない。

3. **`fetch_singers` / `SingersLoaded` は定義済みだが一度も発火していない死に配線。**
   - `common/src/voicevox.rs::fetch_singers` (:44) は実装済み。`AppEvent::SingersLoaded`
     も定義 (`app.rs:3795`) + handler (`app.rs:5514`) もある。が、`fetch_singers()` を
     呼ぶ場所が grep 0 hit。`app.singers` は常に空のまま = 現 Track Inspector の
     dropdown は `(VOICEVOX engine 未起動)` placeholder しか出ない。これを活かす。

4. **クリップ選択の SSoT は `AppData::selected_clip: Option<ClipKey>` (`app.rs:1002`)。**
   - 解決は `selected_clip_ref()` (:10856) → index `ClipRef`。既存の per-clip inspector
     (Audio/Image/Text) は全て `track_inspector.rs` 内で `selected_clip_ref()` を介して
     描画している (例: `inspector_audio_event_summary()` :2097)。**新規パネルの新設は不要、
     既存 `track_inspector.rs` の per-clip セクション群に 1 つ足す**。

---

## 1. データモデル変更 (`common/src/model.rs`)

### 1.1 Clip に声フィールドを 3 つ追加

`Clip` (:1821) に以下を追加する。bincode (`Encode`/`Decode` は struct derive 済) +
serde 両対応、`#[serde(default)]` で旧プロジェクト互換。

```rust
pub struct Clip {
    // ... 既存 ...
    /// FIXME #36: この clip の歌唱声 = VOICEVOX /frame_synthesis の speaker
    /// (= 歌唱 style id)。clip 単位で独立・焼き込み (前の clip の声を後で
    /// 変えても後続に波及しない)。`/singers` の style id。
    #[serde(default = "default_clip_speaker_id")]
    pub speaker_id: u32,
    /// 表示用 (キャラ名)。speaker_id 解決の都度引かなくても inspector が
    /// 取得中に出せるよう焼き込む。
    #[serde(default)]
    pub singer_name: String,
    /// 表示用 (スタイル名)。同上。
    #[serde(default)]
    pub style_name: String,
}
```

- **`speaker_id` の serde default は `0` ではなく `DEFAULT_SINGER_ID` 由来にする** —
  旧プロジェクトは clip に speaker_id 欄を持たないので、欠落時は serde default が効く。
  ただし旧プロジェクトには「トラックの声」があるので、それを優先して焼き込みたい
  (§4 migration)。そのため default fn は `DEFAULT_SINGER_ID` を返すだけにし、
  「トラック声で上書きする」のは migration 側で行う (default は最終フォールバック)。

  ```rust
  fn default_clip_speaker_id() -> u32 { crate::voicevox::DEFAULT_SINGER_ID }
  ```

  `singer_name` / `style_name` は単純な `#[serde(default)]` (空文字列)。migration / 新規
  生成で埋める。空のままなら inspector は「取得中…」表示にフォールバックできる (§5)。

- **`Clip::default()` (:1822 は `#[derive(Default)]`) を手書き impl に変える** か、または
  derive を維持して `speaker_id` default を 0 にし、clip 生成側で必ず解決値を入れる方針に
  揃える。**ここでは derive Default を捨てて手書き Default を置き、`speaker_id =
  DEFAULT_SINGER_ID` / `singer_name = "中国うさぎ"` / `style_name = "ノーマル"` を埋める**
  (Single Source of Truth: 「未設定 clip の声」をモデルの 1 箇所で定義)。
  - 注意: `Clip` は現在 `#[derive(... Default ...)]` (:1821)。手書き Default に変えると、
    既存の `..Clip::default()` / `Clip { ..Default::default() }` 利用箇所はそのまま動く。
    grep で `Clip::default()` / `Clip {` の利用箇所を確認し、声 3 フィールドを明示指定して
    いない生成箇所が手書き Default を経由することを確認する。

### 1.2 アプリ既定声の定数化

`common/src/voicevox.rs`:
- `DEFAULT_SINGER_ID = 3061` (:81) は既存。**コメントは「中国うさぎ ノーマル」と「春日部
  つむぎ ノーマル相当」で食い違っている** (voicevox.rs:80 と builtin/voicevox.rs:57)。
  設計は「中国うさぎ ノーマル」。**3061 が実際に中国うさぎ ノーマルかは engine の
  `/singers` 実レスポンスで確認**し、ズレていれば値を正す (notes 参照)。
- 既定の表示名 (`singer_name` / `style_name`) も同 module に定数で置く:
  ```rust
  pub const DEFAULT_SINGER_NAME: &str = "中国うさぎ";
  pub const DEFAULT_STYLE_NAME: &str = "ノーマル";
  ```
  Clip 手書き Default / 新規 clip 解決 / migration フォールバックが全部これを参照する。

### 1.3 `InstrumentSource::Vocal` をユニットマーカーに作り替え

`InstrumentSource` (:1560):

```rust
pub enum InstrumentSource {
    #[default]
    None,
    /// FIXME #36: 「このトラックは VOICEVOX builtin で鳴らす」印。
    /// 声は per-clip (`Clip::speaker_id`) が SSoT。トラックは声を持たない。
    Vocal,
    Vst3 { path: PathBuf },
    BuiltinSynth,
}
```

- 旧 `Vocal { speaker_id: u32, style_name: String }` → `Vocal` (unit)。
- **serde 互換**: 旧プロジェクトは `Vocal { speaker_id, style_name }` を JSON object
  として書いている。unit variant に変えると単純 derive では deserialize が壊れる
  (`Vocal` を string として期待 → object で失敗)。**migration の責務 (§4) で
  「旧フィールド値を clip へ吸い上げてから unit 化」する**ため、serde 側は旧 object 形を
  受けられる必要がある。対処:
  1. `InstrumentSource` の deserialize 時に旧 `Vocal { speaker_id, style_name }` を
     許容する **過渡的 serde 表現** を用意する。最小実装は、`Vocal` を
     `#[serde(alias)]` では無理 (構造が違う) なので、**deserialize-only の捨て field を
     持つ中間 enum** ではなく、**`Vocal` variant を一旦
     `Vocal { #[serde(default)] speaker_id: u32, #[serde(default)] style_name: String }`
     の形で残し、in-memory では値を無視する**方式にする…のは「ユニットマーカー化」の
     設計に反する。
  2. **理想形: `InstrumentSource` の `Deserialize` を手実装**し、`Vocal` が unit でも
     object (旧 `{speaker_id, style_name}`) でも受理して `InstrumentSource::Vocal` (unit)
     に落とす。object 内の値は **Song レベルの migration が別ルートで拾う** (§4)。
     - ここで問題: 手実装 deserialize だと object 内の旧値を捨ててしまい migration が
       読めなくなる。→ **migration は `InstrumentSource` ではなく「生 JSON / 旧 struct」
       から拾う必要がある**。
  3. **採用案 (理想): 2 段階にする。**
     - `model.rs` には `InstrumentSource::Vocal` (unit) を最終形として置く。
     - `common/src/project.rs` の load 経路 (`load()`) で、**deserialize する前段の生
       JSON (`serde_json::Value`) を見て**、各 track の `source` が旧
       `{"Vocal": {"speaker_id": N, "style_name": "..."}}` 形なら、その track の全 clip
       JSON に `speaker_id` / `style_name` を注入してから本 deserialize に渡す。これで
       「旧トラック声 → clip へ焼き込み」が serde の前で完結し、`InstrumentSource` 手実装
       deserialize は不要 (unit で素直に読める)。
     - **ただし `.daw` が JSON か bincode かで分岐**: `project::load` の現実装フォーマットを
       読んで、JSON 経由なら上記の Value 前処理、bincode 経由なら別途 (§4 で記述)。

  - **実装注意**: §4 でフォーマット依存の移行戦略を確定させる。serde 前処理が最も堅い。

### 1.4 影響を受ける既存 match の修正

`InstrumentSource::Vocal { .. }` を参照している箇所を全列挙して unit 化に追従:
- `daw_gui/src/app.rs:6379` / `:7666` / `:7672` (`matches!(.., Vocal { .. })` →
  `matches!(.., Vocal)`)
- `daw_gui/src/app.rs:13917-13923` (`set_track_speaker` の分配代入) → §3 で関数ごと削除
- `daw_gui/src/app.rs:15295-15298` (builtin 挿入時 `source = Vocal { speaker_id, style_name }`)
  → `source = InstrumentSource::Vocal` (§3.2)
- `daw_gui/src/view/track_inspector.rs:1501` / `:1650` → §5 で per-track dropdown 削除に伴い修正
- `common/src/voicevox.rs:114-125` (`synthesize_song` の `let Vocal { speaker_id }`) → §6 (a)
- `common/src/project.rs:153` (テスト fixture `Vocal { speaker_id: 3, style_name }`) →
  `Vocal` に変更 + テスト clip に声フィールド付与
- `common/src/model.rs:3570` (テスト fixture) → 同上

---

## 2. cache キーに per-clip 声を含める (`common/src/voicevox_cache.rs`)

`VoiceVoxCache::key_for_notes(notes, singer_id)` (:90) は既に `singer_id` を hash に
混ぜている (:108)。**caller が clip の `speaker_id` を渡すよう徹底すれば既存 API で足りる**。

- builtin 側 (§3) と (a) `synthesize_song` (§6) の両 caller が、「トラック共通の
  default」ではなく「その clip の `speaker_id`」を渡すよう変更する。
- key 自体の構造変更は不要 (singer_id が既に key の一部)。test (:138 `key_differs_when_
  singer_changes`) はそのまま通る。

---

## 3. builtin VOICEVOX を per-clip 声で作り直す (本丸)

現状「track 全 clip を 1 speaker / 1 WAV」を「**clip ごとに speaker / WAV を分け、note は
所属 clip の WAV を鳴らす**」へ作り直す。

### 3.1 flush 経路: `sync_vocal_metadata` を clip 単位に拡張 (`daw_gui/src/app.rs:7663`)

現状 (:7697-7719) は全 clip を 1 つの `entries: Vec<NoteMetadata>` に flatten。これを
**clip 境界と clip 声を保ったまま** plugin へ渡す形に変える。

- `NoteMetadata` (`common/src/plugin_metadata.rs:37`) に **clip 識別と声を足す**:
  ```rust
  pub struct NoteMetadata {
      pub note_id: u32,
      pub start_beat: f64,
      pub duration_beats: f64,
      pub pitch: u8,
      pub velocity: u8,
      pub lyric: String,
      /// FIXME #36: この note が属する clip の安定 id (track 内)。builtin が
      /// clip 単位で synth job を分けるための grouping key。
      #[serde(default)]
      pub clip_id: u32,
      /// FIXME #36: この clip の歌唱 speaker (= /frame_synthesis style id)。
      #[serde(default)]
      pub speaker_id: u32,
  }
  ```
  - bincode `Encode`/`Decode` + serde derive は struct に付いている (:34-36) ので
    フィールド追加で自動追従。**protocol 型なので §8 の `cargo build --workspace` 必須**。
- `sync_vocal_metadata` の clip ループ (:7698-7719) で、各 note の `NoteMetadata` に
  `clip_id = clip.id` と `speaker_id = clip.speaker_id` を載せる。`note_id` は引き続き
  track 内の通し番号で良い (builtin 側で clip_id でグルーピングするので衝突しない)。
- `bpm` は track 一律で送る (現状維持)。

### 3.2 builtin 挿入時のトラックマーキング (`app.rs:15294-15299`)

`is_voicevox` で builtin VOICEVOX を挿したとき:
```rust
if is_voicevox {
    track.source = InstrumentSource::Vocal; // unit 化
}
```
声は clip 側 (§1.1 の Clip default / §7 の新規 clip 解決) が持つので、ここで声は設定しない。

### 3.3 builtin plugin の per-clip synth 化 (`daw_plugin_host/src/builtin/voicevox.rs`)

`VoicevoxState { speaker_id, style_name }` (:41) の **per-track 声を廃止**し、声は
flush で来る `NoteMetadata.speaker_id` (per-clip) を読む。

変更:
- **`VoicevoxState` から声を外す**。state_save / state_load (:508-525) の対象を最小化。
  - 設計「per-track VoicevoxState の声も廃し、per-clip の声を読む」に従う。
  - `VoicevoxState` が空になるなら state ext 自体を no-op (`state_save -> Ok(None)`) に
    してよい。ただし `bincode` 後方互換: 旧 project file は builtin state に
    `{speaker_id, style_name}` を bincode で埋めている。`state_load` (:515) は旧 bytes を
    **受けても無視して捨てる** (decode して破棄 or ignore) よう寛容にする。
    decode 失敗で `Err` を返すと plugin restore が壊れるので、`state_load` は
    「decode できなくても Ok」にする (空 state へ migrate)。
- **`SynthJob` を clip 単位にする** (:69):
  ```rust
  struct SynthJob {
      bpm: f32,
      clips: Vec<ClipSynthSpec>, // 1 entry = 1 clip
  }
  struct ClipSynthSpec {
      clip_id: u32,
      speaker_id: u32,
      notes: Vec<BuiltinNoteSpec>,
  }
  ```
- **`set_note_metadata` (:527)** で、`entries` を `clip_id` でグルーピングして
  `Vec<ClipSynthSpec>` を組む。各 spec の `speaker_id` は entry の `speaker_id`
  (clip 内は一定)。
- **synth thread (:171-322)** は、job 受信で **clip ごとに `synthesize_notes_for_builtin`
  を呼ぶ** (clip ごとに speaker_id が違うので別 WAV)。結果は **clip_id → WAV** の
  map に格納:
  ```rust
  struct SynthResult {
      clips: Arc<HashMap<u32 /*clip_id*/, ClipSynthOutput>>,
  }
  struct ClipSynthOutput {
      samples: Arc<Vec<f32>>,
      sample_rate: u32,
      note_offsets: Arc<HashMap<u32 /*note_id*/, u64>>,
  }
  ```
  `synthesize_notes_for_builtin` (`common/src/voicevox.rs:550`) は単一 clip 用なので
  そのまま per-clip に呼べる (signature 変更不要)。
  - 注意: clip ごとに HTTP 1 往復 (`sing_frame_audio_query` + `frame_synthesis`)。
    coalesce slot (:185) は引き続き「最新 job だけ処理」。失敗 retry (:280-288) も
    job 単位で維持。
- **`process()` (:412-451)** の voice 起動を per-clip WAV 参照にする:
  - note_on の `note_id` から「どの clip の WAV を、どの offset で鳴らすか」を引く必要が
    ある。`note_id` は track 内通し番号なので、**`note_id → (clip_id, offset)` の逆引き
    table** を synth 結果と一緒に持つ:
    ```rust
    struct SynthResult {
        clips: Arc<HashMap<u32, ClipSynthOutput>>,
        note_to_clip: Arc<HashMap<u32 /*note_id*/, u32 /*clip_id*/>>,
    }
    ```
  - `Voice` (:94) に `samples: Arc<Vec<f32>>` を持たせる構造は維持できる (clip ごとの
    `samples` Arc を clone して cursor を `note_offsets[note_id]` 起点にする)。
  - rolling 単一 voice (:449-450 の `clear()` + `push`) は維持してよい (同時に 1 clip の
    歌声のみ。clip が時間的に重ならない前提で十分。重なる場合の多 voice は別 polish)。

### 3.4 protocol / host main の追従

- `NoteMetadata` field 追加で `SetBuiltinPluginNoteMetadata` (`common/src/protocol.rs:323`)
  の payload が変わる。protocol struct 自体は `entries: Vec<NoteMetadata>` のままなので
  signature は不変、**ただし bincode wire format は変わる**ので §8 で全 crate rebuild。
- `daw_plugin_host/src/main.rs:1356` / `:1871` の forward は `entries` を素通しなので
  変更不要 (NoteMetadata の中身が増えるだけ)。
- `daw_audio/src/main.rs:502` は ignore arm なので変更不要。

---

## 4. 旧プロジェクト migration

旧 `.daw`:
- track が `InstrumentSource::Vocal { speaker_id, style_name }` を持つ
- clip は声フィールドを持たない

移行ゴール: **そのトラックの全 clip に、旧トラック声 (`speaker_id` / `style_name`、
キャラ名は style から逆引き or 空) を焼き込む**。トラック側 `source` は `Vocal` (unit) に。

### 4.1 フォーマット確認 (確認済み: JSON)

`common/src/project.rs` は **JSON** (`save`: `serde_json::to_string_pretty` :31 /
`load`: `serde_json::from_str` :56)。`ProjectFile` に `version` field があり
`MIN_LOADABLE_VERSION = 2` / `CURRENT_VERSION` で版管理済み (:16, :58)。
→ 採用ルート確定: load 内で `serde_json::Value` を一度読み、旧トラック声を全 clip
JSON へ注入してから本 deserialize する (bincode 分岐は不要)。version gate にも乗せる。

- **JSON 経路の場合 (推奨ルート)**:
  `load()` 内で本 deserialize の **前段** に `serde_json::Value` を一度読み、各 track の
  `source` が旧 `{"Vocal": {"speaker_id": N, "style_name": "S"}}` 形なら:
  1. その track の各 clip JSON object に `"speaker_id": N`, `"style_name": "S"`,
     `"singer_name": ""` を (未設定時のみ) 注入。
  2. track の `source` を `"Vocal"` (unit) に書き換え。
  3. 書き換えた Value を本 deserialize に渡す。
  - これで §1.3 の serde 互換問題も同時に解決する (本 deserialize は unit `Vocal` だけ
    見ればよい)。`singer_name` はキャラ名が旧データに無いので空。inspector が
    「取得中…」フォールバック (§5) で救う。`/singers` 取得後に style id から
    `singer_name`/`style_name` を再解決して焼き直すのは §7.3。

- **bincode 経路の場合**:
  bincode は variant index で enum を encode するので、`Vocal { speaker_id, style_name }`
  → `Vocal` (unit) への変更で **index ずれ / 構造ずれ** が起きる (旧 bytes が
  `Vocal` の後に `u32 + String` を期待)。理想形は「旧 enum 定義を残した deserialize-only
  の legacy struct」で旧 bytes を読み、新形へ変換する `From`。`Track` に
  `#[serde(rename, skip_serializing)]` の legacy slot を足す idiom (Track の
  `legacy_instrument` 等 :1419-1424 と同型) を `source` にも適用できる。
  - **どちらの経路でも、声の焼き込みは 1 箇所 (project::load 直後 or normalize) に集約**し、
    `Song::normalize_after_load` (`model.rs:524`) 経由に乗せるのが SSoT 的に綺麗。

### 4.2 migration hook の置き場所

`Song::normalize_after_load` (:524) は load の正規化ハブ
(`ensure_clip_contents` → `ensure_ids` 等を順に呼ぶ)。**ここに
`migrate_vocal_voice_to_clips()` を 1 つ足す**:
- 各 track が `Vocal` で、clip の `speaker_id == 0` (= serde default も効かなかった
  欠落) または「旧トラック声がまだ焼かれていない」状態なら、トラック声 (§4.1 で
  Value 注入済みなら不要、bincode 経路ならここで) を全 clip に焼く。
- idempotent (既に焼かれている clip は触らない)。
- 注意: 既存の `daw_gui/src/app.rs::migrate_legacy_vocal_tracks` (:6367) は
  「旧 vocal track に builtin VOICEVOX device が無ければ足す」別物 (device 補完)。
  声の migration はそれとは独立。`migrate_legacy_vocal_tracks` は §1.4 の
  `matches!(.., Vocal { .. })` → `matches!(.., Vocal)` 追従だけ行い、機能はそのまま。

---

## 5. UI: クリップ選択時のインスペクタに 2 段ドロップダウン (`daw_gui/src/view/track_inspector.rs`)

### 5.1 per-track 声 dropdown を撤去

`track_inspector.rs:1499-1560` の「Vocal source 編集 (Vocal track のときのみ)」
セクションを **削除**。`speaker_id` を track から読む前提なので unit 化で壊れる。

### 5.2 per-clip 声セクションを新設 (既存 per-clip section の隣)

既存 Audio/Image/Text の per-clip section (`track_inspector.rs:261` / `:563` / `:1146`、
いずれも `app.selected_clip` 起点) と同じ idiom で、**選択 clip が vocal track 上の
MIDI clip のとき**だけ「Clip Voice」セクションを描く。

- 表示条件 (新規 `AppData` read helper、`app.rs` に追加):
  ```rust
  /// 選択中 clip が vocal track 上 (= source==Vocal) かを判定し、その clip の
  /// 焼き込み声 (speaker_id / singer_name / style_name) を返す。inspector 専用 read。
  pub fn inspector_clip_voice(&self) -> Option<ClipVoiceSummary> { ... }
  ```
  `selected_clip_ref()` → track が `InstrumentSource::Vocal` かつ content が `Midi` の
  ときに `Some`。
- **2 段ドロップダウン**:
  1. 上段「キャラ ▼」: `app.singers` の `name` 一覧。選択中は clip の `singer_name`
     (取得済みなら index 一致、無ければ「取得中…」付き先頭)。
  2. 下段「スタイル ▼」: 上段で選んだキャラの `styles` 一覧。選択は clip の
     `style_name` / `speaker_id`。
  - 既存 dropdown の widget は `ui.dropdown(id, rect, &label_refs, selected_idx)`
     (track_inspector.rs:1540 / :1624 で実証済み)。同 API を 2 つ並べる。
- **未取得中 (`app.singers.is_empty()`)**:
  保存済みの声名 (`clip.singer_name` / `clip.style_name`、空なら既定名) を**読み取り
  ラベルで表示**し、横に「取得中…」を出す。dropdown は disabled でも、ラベルとして
  焼き込み済み声名を見せる (情報を失わない)。
- **「再取得」ボタンを 1 つ**: `app.singers.is_empty()` か否かに関わらず常設し、押下で
  `AppEvent::RefetchSingers` (§7.2) を emit。

### 5.3 選択時の AppEvent

dropdown 確定で:
```rust
ui.push_edit(Edit::mutate(move |app| {
    app.handle_event(AppEvent::SetClipVoice {
        clip: clip_key,            // ClipKey (stable id)
        speaker_id, singer_name, style_name,
    });
}));
```
- **`SetClipVoice` は新規 AppEvent** (§7.1)。track index ベースではなく **stable
  `ClipKey`** で渡す (selected_clip と同じ idiom、reorder 安全)。

### 5.4 旧 `SetTrackSpeaker` / `set_track_speaker` の撤去

- `AppEvent::SetTrackSpeaker` (`app.rs:5561` 周辺 + 定義) と handler、
  `set_track_speaker` (`app.rs:13913-13930`) を **削除**。per-clip 化で不要。
- 口パク binding section (track_inspector.rs:1646-) は `matches!(.., Vocal)` に
  追従するだけで機能維持。

---

## 6. (a) `synthesize_song` を per-clip 声で読み直す (`common/src/voicevox.rs:105`)

現状は track の `Vocal { speaker_id }` を読み (:114-125)、全 clip に同 `singer_id` を
使う (:140 / :165 / :196)。これを **clip ごとに `clip.speaker_id` を使う**よう作り直す:
- track ループ (:114) は `InstrumentSource::Vocal` (unit) を「vocal track か」の
  判定だけに使う (`let Vocal { speaker_id } = ...` の分配を `matches!(.., Vocal)` に)。
- clip ループ (:127) 内で `singer_id = clip.speaker_id` を使う。cache key (:139-140) /
  sing (:165) / talk (:196) 全てこの per-clip id に差し替え。
- signature の `default_singer_id` (:107) は **fallback としてのみ残す** (clip の
  speaker_id が 0 のとき) か、clip default が常に解決済みなら引数を削除。
  - **dead code なので現時点で実害は無いが**、声 SSoT を per-clip に統一する意味で
    今のうちに直す。caller が居ないので呼び出し側修正は不要 (テストがあれば追従)。

---

## 7. 一覧取得配線を活かす (`fetch_singers` / `SingersLoaded` / 再取得)

### 7.1 新規 AppEvent

`app.rs` の `AppEvent` enum に追加:
```rust
SetClipVoice { clip: common::model::ClipKey, speaker_id: u32, singer_name: String, style_name: String },
RefetchSingers,
```
`SingersLoaded` (:3795) は既存を流用。

- `SetClipVoice` handler: `clip_at`/`track_by_id_mut`+`clip_by_id_mut` で対象 clip を
  引き、`speaker_id`/`singer_name`/`style_name` を焼き込む → `sync_vocal_metadata()` を
  呼んで builtin を再 flush (= 声変更が再合成に反映)。undo snapshot を 1 step。
- `RefetchSingers` handler: §7.2 の fetch を再発火。

### 7.2 engine ready で初回 fetch、ボタンで再 fetch

- **初回**: `ensure_voicevox_engine` (`app.rs:15316`) の spawn thread (engine 起動)
  完了後、もしくは「vocal track が存在する」状態が初めて発生した時に、
  **`fetch_singers()` を background thread で 1 回呼ぶ**。
  - engine 起動は非同期 (`spawn_engine` :15337)。`common::voicevox_engine::wait_until_ready()`
    (`voicevox_engine.rs:192`、`/version` polling・60s timeout) が ready 判定に使える。
  - 配線: `ensure_voicevox_engine` の spawn thread の末尾で `wait_until_ready()` →
    成功なら `fetch_singers()` → `proxy.send_event(AppEvent::SingersLoaded(singers))`。
    既に running の早期 return パス (:15323) でも、fetch 未実施なら fetch する分岐を足す。
  - `fetch_singers` は blocking + 5s timeout (:44-48)。background thread なので OK。
  - **1 回だけ**にするフラグ (`singers_fetch_attempted: bool` を `AppData` に追加)、
    `RefetchSingers` でリセットして再 fetch 可能に。
- **再取得ボタン** (§5.2): `RefetchSingers` → フラグリセット → `fetch_singers` background
  spawn → `SingersLoaded`。

### 7.3 `SingersLoaded` 受信時に焼き込み声名を再解決 (任意 polish)

`SingersLoaded` handler (:5514-5532) は `singers` 投入 + `vocal_speaker_entries/labels`
再構築 (:5523-5531)。**per-clip 化で `vocal_speaker_entries/labels` は per-track dropdown
専用だったので、用途が無くなれば撤去**。inspector の 2 段 dropdown は `app.singers` を
直接読む (キャラ→style の階層が必要なので flat な labels より生 singers が適切)。
- polish: 旧プロジェクト migration で `singer_name` が空の clip について、`singers` 取得
  後に `speaker_id` から `(singer_name, style_name)` を逆引きして焼き直す
  (`SingersLoaded` 内で全 vocal clip を走査)。これで保存済みプロジェクトの声名が
  engine 起動後に正しく表示される。

---

## 8. ビルド & 検証

- **protocol 変更 (`NoteMetadata` に field 追加 / `Clip` に field 追加 / `InstrumentSource`
  変更) は bincode wire format を変えるので、`cargo build --workspace` 必須**。
  `daw_audio.exe` / `daw_plugin_host.exe` も再生成しないと、古い protocol で decode 失敗 →
  audio/plugin engine が落ちて「音が出ない / 再生が止まる」誤認症状になる
  (memory: protocol 変更は workspace build)。
- `cargo clippy --workspace -- -D warnings`。
- `cargo test --workspace` — `voicevox_cache` の singer key test (:138)、
  `project.rs` の roundtrip test (:107 / :122)、`model.rs` の `ensure_*` test 群が
  Clip 声フィールド追加・`InstrumentSource::Vocal` unit 化に追従して green か確認。
  旧形式 fixture を読む migration test を 1 つ足す (旧 track 声 → clip 焼き込みの回帰)。
- **実機検証 (最後に 1 度)**:
  1. builtin VOICEVOX を vocal track に挿す → clip を 2 つ作り、片方を別キャラ
     (例: 中国うさぎ → 別シンガー) に inspector で変更 → 再生で 2 clip が別声で歌う。
  2. 旧プロジェクト (per-track 声を持つ `.daw`) を開く → 全 clip がそのトラックの旧声で
     再生 (波及無し)、inspector で焼き込み声名が見える。
  3. engine 未起動で起動 → inspector は焼き込み声名 + 「取得中…」、engine ready 後に
     dropdown がキャラ/style で埋まる。「再取得」で再取得できる。
- `cargo build --workspace --release` (commit 後の git hook + 自分でも green 確認)。

---

## 9. 実装順序 (phase)

1. **Phase 1 — model**: `Clip` に声 3 field + 手書き Default、`DEFAULT_SINGER_NAME/STYLE_NAME`
   定数、`InstrumentSource::Vocal` unit 化、影響 match の追従、テスト fixture 修正。
   ここで `cargo build --workspace` を通す (serde 互換は §1.3/§4 の方式確定が前提)。
2. **Phase 2 — migration**: `project::load` のフォーマット確認 → 旧トラック声 → clip 焼き込み
   (`normalize_after_load` に hook)。旧形式 fixture の migration test。
3. **Phase 3 — flush / builtin**: `NoteMetadata` に `clip_id`/`speaker_id` 追加、
   `sync_vocal_metadata` を per-clip 化、builtin の `SynthJob`/`SynthResult`/synth thread/
   `process()` を per-clip WAV へ作り直し、`VoicevoxState` 声廃止 + state_load 寛容化。
   `cargo build --workspace`。
4. **Phase 4 — UI**: per-track dropdown 撤去、per-clip 2 段 dropdown + 再取得ボタン、
   `SetClipVoice`/`RefetchSingers` event、`set_track_speaker`/`SetTrackSpeaker` 撤去。
5. **Phase 5 — fetch 配線**: `ensure_voicevox_engine` で ready 後に `fetch_singers` →
   `SingersLoaded`、再取得、`SingersLoaded` で焼き込み声名 再解決 (polish)。
6. **Phase 6 — (a) 経路**: `synthesize_song` を per-clip 声で読み直す。
7. **Phase 7 — 検証**: §8 の test + 実機 + release build。

---

## 実装結果メモ (2026-06-11、 全 phase landed・workspace clippy `-D warnings` / 全 test green)

計画からの主な確定 / 逸脱:

- **Clip の声は derive Default を維持** (手書き Default にしない)。`speaker_id: u32` ＋
  `singer_name` / `style_name`、 `speaker_id == 0` = 未設定 → 合成時に `DEFAULT_SINGER_ID`
  へフォールバック (既存 idiom 踏襲)。`#[serde(default, skip_serializing_if = "is_zero_u32")]`。
- **migration は `project::load` の JSON Value 前処理** (`migrate_vocal_source_to_clips`) で
  確定 (format は JSON と確認)。`normalize_after_load` hook は不要だった。回帰テスト 2 本追加。
- **builtin は `process()` を一切触らず**、 synth thread で「**clip ごとに合成 → mono WAV を
  連結 → note_offsets を累積シフト → 単一 `SynthResult`**」にした (RT path 不変 = 最も安全)。
  note_id は track 内通し番号なので global note_offsets で衝突しない。`NoteMetadata` に
  `clip_id`/`speaker_id` 追加、 `SynthJob` を `Vec<ClipSynthSpec>` 化。
- **`VoicevoxState` は除去せず vestigial 化** (synth が読まなくしただけ)。state_save/load は
  bincode 後方互換のため不変。
- **inspector の声名は表示時に解決** (`clip.singer_name` が空なら `singers` から speaker_id
  逆引き、 無理なら既定名)。`SingersLoaded` での model 焼き直しはしない (spurious dirty 回避)。
- **声引き継ぎ**: duplicate (D) / clone (Ctrl・Ctrl+Shift drag) / split = 実装済み。
  **paste / glue は未対応** (paste はクリップボード構造体への声追加が必要、 glue は「どの
  source の声を採用するか」のポリシー判断が必要 — 別タスク)。

### 未検証 (実機 runtime が必要、 unit/smoke では不可)
- **可聴**: 1 トラックに別キャラの 2 clip → 別声で歌う。 旧プロジェクト読込 → 全 clip が旧
  トラック声で鳴る + inspector に焼き込み声名。 engine 未起動→起動で dropdown 自動 populate、
  「再取得」動作。

### #38 / #37 連動
- **#38**: gui_01 #100 (interval_beats モデル) が landing。 daw_01 は
  `snap::subgrid_interval_beats` + `piano_roll_view.rs` 1 行 wire 済み。 縦グリッドの視覚確認は
  runtime が必要。
- **#37**: gui_01 #099 landing 待ち。 daw_01 側変更なし。

## 2026-08-28 追記 (r.md #75): 「声の解決」は per-clip のまま、「合成の単位」はフレーズ

本書の「clip 単位で声を分ける」は**そのまま生きている** — `Clip.speaker_id` が声の SSoT で、
`sync_vocal_metadata` が per-note の `NoteMetadata.speaker_id` に焼き込む。

変わったのは **合成の単位**。旧実装は「解決済み speaker でグルーピングして、その声の全 note を
1 query + 1 synth」だったが、r.md #75 で

- **フレーズ** (= 隙間ゼロで続く note の極大列、同一 speaker。**クリップ境界では切らない**)
  = `/frame_synthesis` の単位
- **塊** (= 連続する複数フレーズ、既定 60 秒) = `/sing_frame_audio_query` の単位

へ分割した。フレーズ分割は声でも切るので、per-clip 声の解決結果はそのまま尊重される。
`§2 cache キーに per-clip 声を含める` も生きている (`key_for_sing_phrase` が `singer_id` を
混ぜる)。ただしキャッシュ module は `common/src/voicevox_cache.rs` へ移設され、鍵は
**フレーズ単体 query + singer + 塊の長さ**になった。設計正本は
`docs/plan_rmd_75_voicevox_phrase.md`。

なお `NoteMetadata.clip_id` の用途も変わった: 旧「builtin のグルーピング key」→
「`sing_note_id` の導出元 + 合成進捗のクリップ帰属 (`VocalSynthProgress::pending_clips`)」。
