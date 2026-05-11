# Automation 機能 実装計画

ステータス: **仕様策定中** (実装着手前)。

ユーザー要望 (2026-05-09):
- FL Studio / Bitwig 流の **clip 方式** (linked / independent コピー可)
- Reason / Reaper 流の **track header での default 値設定**
- 一般 DAW 標準のオートメーション機能 (Read / Touch / Latch / Write、curve 種別、lane の bypass / visible / delete)

## 1. 全体方針

Bitwig 寄りに揃える。理由:

- 既存の **Clip + ContentId 共有モデル** ([docs/plan_clip_share_clone.md](plan_clip_share_clone.md))
  と整合する。`ClipContent` enum に `Automation` variant を追加するだけで linked / independent
  が無料で手に入る
- 既存の Track 階層 (track + parent_group_id) と整合する。**automation lane は track の
  「子レーン」として展開**、track の上下移動・group 化に追従
- VOICEVOX 統合の歌唱パラメータ (将来 expression / breathiness 等) も同じ仕組に乗る

参照点:
- Bitwig manual ch.13 "Automation": track ごとの per-parameter lane、lane に automation
  clip を配置、clip 外は lane の "stable value" (= header の knob)
- FL Studio manual "Automation Clips": automation clip は専用 channel として playlist に
  置く。daw_01 では「clip を track 配下の lane に置く」 Bitwig モデルを採用 (track 概念を
  壊さない)
- Reaper manual "Track envelopes": envelope lane を track 直下に展開、ON/OFF/Read/Write/Touch/Latch
  toggle、Bypass / Visible / Lock の 3 軸。default 値は track 本体の volume/pan slider
- Reason manual "Combinator automation": knob は track header に常駐、automation を上書き
  したいときは右クリック → Edit Automation
- CLAP `clap/include/clap/ext/params.h` (free-audio): `clap_param_info` の id /
  default_value / min_value / max_value、`CLAP_PARAM_IS_AUTOMATABLE` フラグ
- CLAP `clap/include/clap/events.h`: `clap_event_param_value` (param_id + value + sample
  offset) を sample-accurate に流せる
- VST3 SDK `IParameterChanges` + `IEditController::getParameterInfo`: normalized 0..1 で
  ParamID 単位に sample-accurate point list

## 2. 機能スコープ

### 2.1 M2 Phase に組み込むもの

| 機能 | Phase |
|---|---|
| データモデル (Lane / Clip / Content / Target) + v8 migrate | 1 |
| Track header の **default knob** + lane 追加 / 削除 | 1 |
| Track 内蔵 parameter (volume / pan / mute / send) の lane 再生 | 1 |
| arrangement 上の **automation lane 行** + 折り畳み | 1 (gui_01 #028 依頼後) |
| automation clip の linked / independent コピー (D / Alt+D / Ctrl+drag / Ctrl+Shift+drag) | 1 |
| automation point の add / move / delete + curve type (Hold / Linear / Bezier) | 1 |
| Plugin parameter 列挙 (CLAP_EXT_PARAMS / VST3 `IEditController`) + IPC | 2 |
| **`A` キー shortcut**: 最後に触った param に対し選択中 track へ lane を追加 | 1 (内蔵 param) / 2 (plugin param) |
| Last-touched param トラッキング (knob touch / plugin GUI gesture) | 1+ |
| Read mode (再生時に curve → CLAP_EVENT_PARAM_VALUE / VST3 ParameterChanges) | 2 |
| Bezier 詳細 (tension / asymmetric)、Exponential / Logarithmic curve | 3 |
| 点の選択 / 矩形選択 / コピー / ペースト / quantize | 3 |
| Make Unique / Share シェア対応 | 3 |
| Recording mode (Touch / Latch / Write) | 4 |
| Tempo / Time signature automation (Song level / Master lane) + CLAP_EVENT_TRANSPORT 連携 | 5 |

### 2.2 スコープ外 (将来)

- Modulator / LFO / step sequencer ベースの parameter 駆動 (Bitwig "Modulators")
- Macros (複数 param を 1 knob で操る)
- MIDI Learn / MIDI CC マッピング
- Trim mode (curve に offset/scale を被せる Reaper "Trim/Read" モード)

## 3. データモデル

### 3.1 AutomationTarget

「何を automate するか」 を識別する型。3 種類:

```rust
// common/src/model.rs

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum AutomationTarget {
    /// Track 内蔵パラメータ (volume / pan / mute / send n)。track_id は
    /// AutomationLane を所有する Track の id (= self) を冗長保持しない。
    /// Lane が track に紐付く時点で自明。
    TrackBuiltin(TrackBuiltinParam),

    /// プラグインパラメータ。slot で plugin instance を identify、
    /// param_id は CLAP `clap_id` / VST3 `ParamID` (どちらも u32)。
    PluginParam {
        slot: PluginSlot,
        param_id: u32,
    },

    /// Song レベル (Master lane でのみ使う)
    SongTempo,
    SongTimeSigNumerator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum TrackBuiltinParam {
    Volume,
    Pan,
    Mute,
    SendGain { send_idx: u8 },   // 将来 Send 実装時
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum PluginSlot {
    Instrument,
    MidiFx { idx: u8 },
    Fx { idx: u8 },
}
```

`PluginSlot` は `Track` 内のプラグイン位置を addressing するための型。
[common/src/model.rs:418-479](../common/src/model.rs:418) の `Track::midi_fx_chain` /
`instrument` / `fx_chain` の 3 区画と 1:1 対応。`(Track::id, PluginSlot)` のペアで
song-global に plugin instance を一意に指定できる。

`param_id` の意味は format ごと:
- CLAP: `clap_param_info::id` (`clap_id` = `u32`)。spec で「stable parameter identifier,
  it must never change」 ([clap/ext/params.h:139](https://github.com/free-audio/clap/blob/main/include/clap/ext/params.h#L139))
- VST3: `Steinberg::Vst::ParamID` = `uint32`。`ParameterInfo::id`

VST3 / CLAP どちらも `u32` なので `param_id: u32` で吸収する。format の区別は
`Track.fx_chain[idx].format` で取れる。

### 3.2 AutomationContent (新 ClipContent variant)

```rust
// common/src/model.rs (既存 ClipContent 拡張)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(untagged)]
pub enum ClipContent {
    Midi(MidiContent),
    Audio(AudioContent),
    Automation(AutomationContent),    // NEW
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AutomationContent {
    /// time_beat 昇順でソート済 (insert 時に維持)。
    /// clip-local beat (= clip.start_beat 相対)。
    pub points: Vec<AutomationPoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AutomationPoint {
    /// clip-local beat 位置。0.0 が clip 先頭、`Clip.length_beats` が末尾。
    pub time_beat: f64,
    /// Target の plain value (volume なら 0.0..2.0、CLAP plugin param なら
    /// `clap_param_info::min_value..max_value` で正規化前)。
    /// daw_01 内部は plain で持ち、plugin に流す直前で format ごとに変換。
    pub value: f64,
    /// 直前の point からこの point までの **線分の補間方法**。
    /// 最初の point の curve は意味を持たない (clip 先頭から線形に立ち上がる)。
    pub curve: AutomationCurve,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum AutomationCurve {
    /// 直前 point の値を維持してこの point で step jump (= 階段状)。
    Hold,
    /// 直前 point からこの point へ直線。
    Linear,
    /// 2D cubic Bezier。tension は -1.0..=1.0。 SSoT は
    /// `common/src/automation.rs::apply_curve` の `eval_bezier`:
    ///   - 制御点 x は固定 (`c1x = 1/3`, `c2x = 2/3`)
    ///   - 制御点 y は対角線と end-hold の lerp:
    ///       tension >= 0: c1y = lerp(diag1, a, tension), c2y = lerp(diag2, b, tension)
    ///       tension <  0: c1y = lerp(diag1, b, |tension|), c2y = lerp(diag2, a, |tension|)
    ///   - x(t) は Bernstein 基底で打ち消し合って `x(t) = t` に縮退、
    ///     時間軸 u から Bezier parameter t は `t = u` で即決定 (Newton 不要)
    ///   - tension=0 で 4 制御点が対角線上 → 直線 (= Linear と一致)
    ///   - tension=+1.0 で S 字 (両端緩い)
    ///   - tension=-1.0 で inverse S 字 (overshoot 系)
    Bezier { tension: f32 },
    /// 指数。bend は -1.0..=1.0、0.0 で linear、正で前半遅く後半速い、負で逆。
    /// `value = a + (b - a) * u^(2^bend)`。
    Exponential { bend: f32 },
}
```

`#[serde(untagged)]` の判定は disjoint field set:
- `MidiContent.notes` (Vec<Note>)
- `AudioContent.events` (Vec<AudioEvent>)
- `AutomationContent.points` (Vec<AutomationPoint>)

3 つ全て field 名が異なるのでタグなし dispatch が成立する
([common/src/model.rs:630-635](../common/src/model.rs:630) の既存
`#[serde(untagged)]` 使用法と整合)。

### 3.3 AutomationLane (Track 内)

```rust
// common/src/model.rs

pub struct AutomationLane {
    /// Track 内 stable id。Track::ensure_lane_ids で採番。0 は sentinel。
    pub id: u32,
    /// 何を automate するか。
    pub target: AutomationTarget,
    /// Lane 範囲外 / disabled / clip 無し領域で使う「定数値」。
    /// Bitwig "stable value" / Reason "knob 直値" / Reaper "main fader value"。
    /// Header の knob と双方向同期: knob を回すと default_value が更新、
    /// default_value を編集すると knob 表示が更新。
    pub default_value: f64,
    /// false のとき lane は無視され、target は default_value で動く
    /// (Bitwig "Disable Automation"、Reaper "Bypass envelope")。
    pub enabled: bool,
    /// Arrangement 上で lane 行を表示するか。false なら inspector のみ。
    pub visible: bool,
    /// Lane 行の高さ (px)。Bitwig 流に lane ごとに調整可能。default 60。
    pub height_px: u16,
    /// このレーンの automation clip 群。
    pub clips: Vec<AutomationClip>,
    /// Per-lane stable id allocator for `AutomationClip`。
    pub next_clip_id: u32,
}

pub struct AutomationClip {
    pub id: u32,
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,
    /// Song.clip_contents 参照。MIDI / Audio clip と同じ store を共用。
    /// content variant が ClipContent::Automation でない場合は audio
    /// thread / GUI 側で警告ログ + 無視。
    pub content_id: ContentId,
}
```

### 3.4 Track 拡張

```rust
pub struct Track {
    // 既存フィールド全て維持

    /// このトラックに乗る automation lane 群。表示順は Vec の順序
    /// (drag で並び替え可能)。空なら lane 表示なし。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation_lanes: Vec<AutomationLane>,
    /// Per-track stable id allocator for AutomationLane.
    #[serde(default)]
    pub next_lane_id: u32,
}
```

### 3.5 Song 側ヘルパ

`Song::clip_content_refcount` / `gc_clip_contents` / `clip_notes` 等の既存ヘルパは
そのまま `AutomationContent` も対象にする (= refcount は MIDI / Audio / Automation
全 variant で合算)。新規ヘルパ:

```rust
impl Song {
    /// Lane / clip / point を辿って、automation curve の現在値を返す。
    /// `lane.enabled = false` または clip 範囲外なら lane.default_value。
    /// engine / GUI 双方から呼ぶ。
    pub fn automation_value_at(&self, track_id: u32, lane_id: u32, beat: f64) -> f64;

    /// AutomationContent を解決 (clip_contents lookup)。Audio / Midi は None。
    pub fn automation_content(&self, content_id: ContentId) -> Option<&AutomationContent>;
}

impl ClipContent {
    pub fn automation_points(&self) -> Option<&[AutomationPoint]> { ... }
    pub fn automation_points_mut(&mut self) -> Option<&mut Vec<AutomationPoint>> { ... }
}

impl Track {
    pub fn alloc_lane_id(&mut self) -> u32;
    pub fn ensure_lane_ids(&mut self);
    pub fn lane_by_id(&self, lane_id: u32) -> Option<&AutomationLane>;
    pub fn lane_by_id_mut(&mut self, lane_id: u32) -> Option<&mut AutomationLane>;
}
```

### 3.6 マイグレーション

- `CURRENT_VERSION` **7 → 8** にバンプ
- v7 file 読込時: `Track.automation_lanes` 不在 → `#[serde(default)]` で空 Vec
- `ClipContent::Automation` variant を v7 file が持つことはない (untagged dispatch
  で衝突しない)
- bincode IPC: enum の variant 追加は新 variant index を末尾に置く。protocol
  端は新 variant を encode しないのでバージョン非互換は起きない (新 build 同士のみ)

## 4. Linked / Independent コピー

既存の plan_clip_share_clone.md (§3.4, §3.5) の semantics をそのまま流用する:

| 操作 | 結果 |
|---|---|
| drag | move (現状) |
| Ctrl+drag | linked: source 共有のコピー、`content_id` 同一 |
| Ctrl+Shift+drag | independent: deep clone + 新 `content_id` |
| D | 末尾直後に linked コピー |
| Alt+D | 末尾直後に independent コピー |
| 右クリック → Make Unique | `refcount >= 2` なら deep clone + 新 `content_id` |

automation clip も MIDI clip と同じ store (`Song.clip_contents`) に乗るので、
**MIDI clip と automation clip の混在を防ぐ責務は GUI 側**:
- clip の context_id 解決時に `ClipContent::Automation` variant でなければ「不正な
  reference」 として lane では空表示
- D / Alt+D / Ctrl+drag を MIDI track の clip → automation lane へ drop することは
  禁止 (gui_01 widget で source/dest lane の type ガードを実装、§11 の依頼参照)

## 5. デフォルト値 / Header knob

### 5.1 オーバーライドモード (Bitwig)

- `lane.enabled = true` かつ clip がカバーする beat 範囲: clip の curve 値
- それ以外 (clip ギャップ、clip 範囲外、`enabled = false`): `lane.default_value`

clip の **末尾を越えた瞬間に default に戻る** (Bitwig "stable value")。Reaper の
「最後の値を保持」 (Hold-after-end) は `AutomationCurve::Hold` を最終 point に
設定すれば近似できるが、デフォルトは Bitwig 流。

### 5.2 Header knob の双方向同期

- track header の inspector に **lane ごとに knob** を 1 つ表示
- knob = `lane.default_value` (= 内部値、target の plain value)
- automation 範囲内 (clip カバー中) では knob は **automation の現在値を ghost 表示**
  (薄色 + read-only)、ユーザーが触ったら「automation 範囲を抜ける」(= clip を mute / hide
  / Bypass) ではなく **lane を一時 Bypass + knob 操作** という Bitwig flow:
  - knob touch + drag → 一時的に `enabled = false` 相当の動作 (clip は残る、再生時は
    default_value)。release で `enabled` を復元
- 詳細仕様は Phase 4 (recording mode) と統合決定 (Touch mode との関係)

### 5.3 Lane 追加時の default_value 初期化

- `A` キー (§7.3) → `last_touched_param` の lane を追加 (clip は **空**、§5.5 で
  user が dblclick で作る)
- `default_value` の初期値:
  - `TrackBuiltinParam::Volume` → 現在の `track.volume`
  - `TrackBuiltinParam::Pan` → 現在の `track.pan`
  - `PluginParam` → daw_plugin_host から `plugin_params.get_value(param_id)` を IPC で
    取得 (= 現在の plugin 内部値、§7.5 の PluginParamList)

### 5.5 Automation clip の作成方法

MIDI clip の作成と同じ idiom を採用:

| 操作 | 動作 |
|---|---|
| lane body 内 clip rect 内で dblclick | `AddAutomationPoint` (point 追加) |
| lane body 内 **clip rect 外** (= 空き / clip ギャップ) で dblclick | `CreateAutomationClip` (新規 clip 作成) |
| 既存 clip drag (Move / Linked / Independent / Resize / Delete) | §11.4 の通り |

`CreateAutomationClip` は gui_01 #029 で要望済 (現行 widget は lane 空き dblclick で
no-op)。reply 受領後に AppEvent / handler を追加。

clip 作成時の default 仕様 (daw_01 handler):

- `start_beat`: dblclick 位置 (widget が snap 適用済値を渡す)
- `length_beats`: 既定 4 beats (= MIDI clip の既定と同じ感覚)
- `content_id`: 新規採番、`ClipContent::Automation(AutomationContent::default())` を
  insert
- `name`: lane の display name + " curve" (例: "Volume curve")

### 5.4 Lane 跨ぎ drag (target 不一致) のポリシー

automation clip の `points` は **normalized 0..1 の curve** なので、target が違う lane
に drop しても curve の shape はそのまま意味を持つ (Bitwig と同じ)。すべての操作で
target 不一致でも accept、demote / reject は行わない:

| 操作 | target 一致 | target 不一致 |
|---|---|---|
| `MoveAutomationClips` | accept | accept |
| `CloneAutomationClipsLinked` (Ctrl+drag) | accept | **accept** (linked のまま、curve を target を跨いで共有) |
| `CloneAutomationClipsIndependent` (Ctrl+Shift+drag) | accept (deep clone) | accept (deep clone、新 ContentId) |

linked で「Volume curve と Pan curve が同じ point list を共有する」 状態は、ユーザーが
意図的に「同じ shape を別 param に揃えたい」 時に有用 (例: filter cutoff と reverb send
を同じ swell shape で動かす)。意図しない reflinked は Make Unique で随時独立化できる
ので、初期 drop で reject する設計コストの方が高い。

実装場所: 特別な処理は不要。`daw_gui/src/app.rs` の各 `*AutomationClips` AppEvent
ハンドラはどちらも target を見ず、純粋に clip を移動 / コピーするだけ。

## 6. Recording Mode (Phase 4)

| Mode | 動作 |
|---|---|
| Read (default) | curve を読み出して plugin / built-in に流すのみ |
| Touch | knob 操作中だけ point 生成 / 上書き、release で curve に戻る |
| Latch | 1 度触れたら停止まで上書き (= 触り続けと同等) |
| Write | 再生中ずっと既存 curve を knob 値で上書き (= overdub) |

実装方針 (Phase 4 で詳細設計):

- transport bar に 4 way toggle
- `daw_gui` で knob touch を AppEvent::ParamGestureBegin / End にし、対応 lane を見つけて
  recording_state を立てる
- 再生中、recording 中の lane は audio thread の curve sample 結果を捨て、knob 値 →
  AutomationPoint::time_beat = `playhead_beat` で生成 (一定間隔、例 1/64 beat)
- 停止 + 再生終了で recording 終了、隣接 point は thinning (Reaper / Live と同じ間引き
  アルゴリズム、tolerance ε 内の中間点削除)

CLAP の `CLAP_EVENT_PARAM_GESTURE_BEGIN/END`
([clap/include/clap/events.h:205-210](https://github.com/free-audio/clap/blob/main/include/clap/events.h#L205))
は plugin GUI 側からの knob touch を受け取るので、plugin GUI 経由の recording も将来対応。

## 7. UI

### 7.1 Arrangement の lane 行

```
TRK1 ▶ │[こんにちは    ]    │  ┌[━さようなら────]
       │            (★ V volume     )           │   ← lane 行 (展開時)
       │                ●─╮               ╭──   │
       │                  ╰────●──────────╯     │
       │                  [Auto Clip 1   ]      │
       │            (★ P pan        )           │   ← もう 1 lane
       │                ────────●─────          │
TRK2   │[━━━━━━━ Bass Loop ━━━━━━━━━━━━━━━━]
```

- Track header の `▶ / ▼` toggle で lane 群の表示・折り畳み
- Lane 1 行は `(★ I name)` のラベル + curve 描画域
- ★ = enabled toggle、I = lane 種別アイコン (V=Volume, P=Pan, F=plugin filter cutoff
  などの省略名)、名前は target に応じて display 文字列
- curve 内で point drag、Shift+click で curve type 切替メニュー、Alt+click で 1 point
  insert、Ctrl+click で point 削除

### 7.2 Track Inspector

```
TRK1 Vocal
  Source: VOICEVOX  Speaker: ずんだもん
  ─────────────────────────────
  Automation:                          last touched: ⌗ Cutoff (Serum)
    [V] Volume      [○──── 0.85 ]  👁 ▣  ✕
    [P] Pan         [────●── -0.10]  👁 ▣  ✕
    [F] Cutoff (Serum)  [○─────  3200Hz]  👁 ▣  ✕
                                          [press A to add]
  ─────────────────────────────
  FX: [EQ] > [Reverb]
```

- 各行: アイコン + 名前 + knob (= default_value) + 👁 (visible toggle) + ▣ (enabled
  toggle、Bypass) + ✕ (delete)
- 末尾の `[press A to add]` ヒントは `last_touched_param` がセットされていて
  かつ対象 track にまだ lane が無いときのみ表示。`last_touched_param` の display 名は
  Automation セクション右上の `last touched: ⌗ ...` に常時表示
- knob 直接編集 = `default_value` 更新 (automation 無効区間で即反映)

### 7.3 `A` キー shortcut: Last-touched param からの lane 追加

Parameter Picker 方式は不採用。Bitwig / Live の "last touched parameter" 流に
**`A` キー 1 打**で `AppData.last_touched_param` の lane を `selected_track` に追加する。

```rust
// daw_gui/src/app.rs
pub struct AppData {
    // 既存
    pub last_touched_param: Option<TouchedParam>,
}

pub struct TouchedParam {
    /// Param が乗る track。selected_track と一致しなくても、A キー時の lane は
    /// 「last_touched_param が属する track」 に追加する (Bitwig 流。 selected_track
    /// に追加すると別 track の plugin への automation を作れず、UX が崩れる)。
    pub track_id: u32,
    pub target: AutomationTarget,
    /// inspector の hint や error message で使う display 名 ("Cutoff (Serum)" 等)。
    pub display_name: String,
    /// セット時刻。stale 判定 (例: track / plugin が削除されたあとの自動 clear) 用。
    pub touched_at: Instant,
}
```

#### Last-touched の更新トリガー

以下の操作でいずれも `last_touched_param` を上書きする:

| 操作 | target |
|---|---|
| inspector の volume knob を drag | `(track_id, TrackBuiltin(Volume))` |
| inspector の pan knob を drag | `(track_id, TrackBuiltin(Pan))` |
| inspector の send gain knob を drag (Phase 5+) | `(track_id, TrackBuiltin(SendGain{...}))` |
| inspector の lane default knob を drag | その lane の `target` |
| plugin GUI 内の knob を drag (CLAP gesture event 経由) | `(track_id, PluginParam{slot, param_id})` |
| plugin GUI 内の knob を drag (VST3 `beginEdit`/`performEdit`) | 同上 |
| arrangement の lane 上で point を drag | その lane の `target` |

#### `A` キーの挙動

- text input フォーカス中・modal open 中は dispatch しない (既存 shortcut dispatcher
  の `focused_id.is_some()` 判定に従う)
- `last_touched_param` が `None` → status_message で「No parameter touched yet —
  drag any knob first」 と表示、何もしない
- `last_touched_param` の `track_id` の track が削除済 → clear して上記と同じ表示
- 当該 track に **既に同 target の lane が存在する** → 既存 lane を `visible = true` に
  + arrangement で lane 行までスクロール + 一瞬ハイライト (Bitwig 流。lane が見つかる
  UX が直感的)
- それ以外: `AppEvent::AddAutomationLane { track_id, target, display_name }` 発火、
  `default_value` は §5.3 の通り現在値で初期化、生成した lane は `visible = true`、
  arrangement に展開 (`automation_lanes_collapsed = false`)、新 lane の行までスクロール

#### gesture event の経路

CLAP `CLAP_EVENT_PARAM_GESTURE_BEGIN` ([clap/include/clap/events.h:205-210](https://github.com/free-audio/clap/blob/main/include/clap/events.h#L205))
は plugin が host に送る output event で、plugin GUI 内 knob の touch を表す。

```
plugin GUI (任意 thread) → plugin が clap_output_events.try_push でキューに積む
  → host (audio thread) が clap_plugin.process の out_events から拾う
  → ChildToMain::PluginParamTouched { track_id, slot, param_id, display_name }
  → daw_gui の AppEvent::PluginParamTouched
  → AppData.last_touched_param 更新
```

`display_name` は param 列挙時に取得済の `PluginParamInfo::name` を host が補完。
VST3 は `IComponentHandler::beginEdit(paramID)` 経路を `IComponentHandler` impl で
受けて同じパスに乗せる。

CLAP gesture が来ない plugin (古い / 実装漏れ) のフォールバック: plugin GUI 内
knob 操作は検出できないので、**inspector に lane を 1 度作っておけば**その lane の
default knob 操作で last_touched が更新される (ループが閉じる)。これは Live と同じ
仕様。

#### 補助 shortcut (将来検討)

- `Shift+A`: lane 追加せずに `last_touched` 表示だけトグル (= 既存 lane の visible
  on/off)
- `Ctrl+A`: 当該 track の **全 lane** を visible toggle (Reaper "Show all envelopes")

両方 Phase 3+。M2 では `A` のみ。

### 7.4 Transport Bar

- automation **mode toggle** (Phase 4): `Read` / `Touch` / `Latch` / `Write` の 4 way
  segmented button
- automation **bypass all** toggle (Phase 1 で導入可): 全 lane の `enabled` を一時的に
  上書き (Reaper "Bypass FX/Send"、状態は session のみ、保存しない)

### 7.5 Plugin parameter の前提: param 列挙 IPC

automation lane を Plugin parameter に対して張るためには、daw_gui が **plugin の
param 一覧** を持つ必要がある。

新 IPC メッセージ (Phase 2 で導入):

```rust
// common/src/protocol.rs
pub enum ChildToMain {
    // 既存
    PluginParamList {
        track_id: u32,
        slot: PluginSlot,
        params: Vec<PluginParamInfo>,
    },
    PluginParamValueChanged {  // plugin GUI で knob 動かしたとき
        track_id: u32,
        slot: PluginSlot,
        param_id: u32,
        value: f64,
    },
}

pub struct PluginParamInfo {
    pub id: u32,
    pub name: String,         // CLAP_NAME_SIZE 切捨
    pub module_path: String,  // CLAP "module" / VST3 "unitId path"
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    pub flags: PluginParamFlags,
}

bitflags! {
    pub struct PluginParamFlags: u32 {
        const STEPPED      = 1 << 0;
        const PERIODIC     = 1 << 1;
        const READONLY     = 1 << 2;
        const HIDDEN       = 1 << 3;
        const AUTOMATABLE  = 1 << 4;
        const MODULATABLE  = 1 << 5;
    }
}
```

- daw_plugin_host が `plugin.activate` 直後に param 列挙を 1 回流す
- `clap_plugin_params.changed` (rescan) を host が受けたら再度流す
- daw_gui の `AppData.plugin_params: HashMap<(u32, PluginSlot), Vec<PluginParamInfo>>`
  にキャッシュ

CLAP で `CLAP_PARAM_IS_AUTOMATABLE` が立っていない param は Picker で hidden + 警告
ログ。`CLAP_PARAM_IS_HIDDEN` も同様。`CLAP_PARAM_IS_STEPPED` は curve UI で integer
snap を有効化。

## 8. 再生エンジン

### 8.1 Sample-accurate event 生成

`daw_audio/src/automation.rs` を新規作成。`sequencer.rs::collect_events_for_buffer`
と並列に呼ばれ、buffer ごとに lane の events を生成する。

```rust
// daw_audio/src/automation.rs

pub struct TimedParamEvent {
    pub time: u32,                // sample offset within buffer
    pub target: AutomationTarget,
    pub value: f64,               // plain value (target の単位)
}

pub fn collect_automation_for_buffer(
    song: Option<&Song>,
    track_idx: u32,
    sample_rate: u32,
    bpm: f32,
    playhead: u64,
    frames: u32,
    out: &mut Vec<TimedParamEvent>,
);
```

ロジック:
1. 各 lane を走査
2. lane が `enabled=false` → 1 frame 目で `default_value` 1 発のみ push (前 buffer の
   curve 残響を default に戻す)
3. lane が enabled、各 clip を走査:
   - `[playhead, playhead+frames)` と clip の重なり区間を計算
   - 重なり区間内の各 point について、point の sample offset を計算 → push
   - 区間先頭が point と一致しない場合は、curve 評価値を frame 0 に push (= 補間の
     初期値)
4. clip ギャップに入る瞬間は `default_value` を 1 発 push
5. point 数の上限 / event 間引き: `MAX_POINTS_PER_BUFFER = 256` を超えたら curve を
   `frames / 64` 段階で sample (Live と同等)

`AutomationCurve` の評価:
- `Hold` → step jump (point 直前まで前値維持、point で即新値)
- `Linear` → 直前 point からこの point に向かって直線
- `Bezier { tension }` → cubic Bezier flatten。`tension=0` で Catmull-Rom (gui_01
  `automation_curve` 既定と同じ式 [crates/ui/src/widgets/automation.rs:91-145](../../gui_01/crates/ui/src/widgets/automation.rs:91))
- `Exponential { bend }` → `value = a + (b-a) * t.powf(2^bend)`

### 8.2 Built-in volume / pan の ramp 適用

現状 [daw_audio/src/engine.rs:930-966](../daw_audio/src/engine.rs:930) は track.volume
/ track.pan を buffer 全体で定数として適用している。これを **frame 単位 ramp** に変える:

```rust
// daw_audio/src/mixer.rs

pub struct TrackScratch {
    // 既存
    pub volume_per_sample: [f32; MAX_FRAMES],   // 新
    pub pan_per_sample: [f32; MAX_FRAMES],      // 新
}
```

`engine.rs::process_track_owned` (L530-741) の volume / pan 適用ループで、

```rust
for i in 0..frames {
    out_l[i] = sample_l * scratch.volume_per_sample[i] * gain_l_per_sample[i];
    out_r[i] = sample_r * scratch.volume_per_sample[i] * gain_r_per_sample[i];
}
```

の形に変更。`TimedParamEvent` を frame 0..frames に展開して `volume_per_sample` を
linear interpolate で埋める prelude を追加。

RT 安全性: `volume_per_sample` 等は activate 時に固定確保、buffer ごとに上書きのみ。
新規 Vec 確保なし。

### 8.3 Plugin parameter の event 流し方

`daw_plugin_host/src/clap_plugin.rs` の process input event 生成 (L645-678) に
`clap_event_param_value` を追加:

```rust
// 現状: in_events に clap_event_note のみ push
// 拡張: TimedParamEvent (target=PluginParam) も push

let ev = clap_event_param_value {
    header: clap_event_header {
        size: size_of::<clap_event_param_value>() as u32,
        time: timed_event.time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_: CLAP_EVENT_PARAM_VALUE,
        flags: 0,
    },
    param_id,
    cookie: ptr::null_mut(),  // host は cookie キャッシュ未対応で OK
    note_id: -1,              // wildcard
    port_index: -1,
    channel: -1,
    key: -1,
    value: timed_event.value,
};
self.pending_events.push(...);
```

`pending_events` の capacity を `64 → 256` に増やす (note + param 合算)。activate 時
事前確保のまま。

VST3 側 (`vst3_plugin.rs`) は `IParameterChanges::addParameterData` 経由。Plain →
Normalized 変換は `ParameterInfo::min/max` で割る。

### 8.4 lane が指す PluginSlot の解決

audio thread が song.tracks[track_idx].fx_chain[idx] にどう辿り着くかは既に
sequencer / plugin host の既存パスで解決済 (slot index は SlotPath で渡している)。
`AutomationTarget::PluginParam.slot` から `SlotPath` への変換ヘルパを追加する。

### 8.5 IPC / Song 同期

`MainToChild::LoadSong(Song)` は automation_lanes 込みで送られる (Song 内に既に乗る)。
Curve 編集ごとに LoadSong 再送する既存パスで自動同期。

将来 (Phase 4) recording mode では、point 生成は **audio thread が ChildToMain で
daw_gui に push** する形 (= playhead 位置 + value だけ送り、daw_gui 側で song に
AutomationPoint を insert)。Audio thread から song を直接書き換えない (RT 安全性 +
SSOT 遵守)。

## 9. RT 安全性チェックリスト

- `automation_lanes` の参照は ArcSwap<Song> 経由で wait-free 取得 (既存パス)
- buffer ごとの `Vec<TimedParamEvent>` は **per-track scratch にプリアロケート**
  (capacity = MAX_LANES * MAX_POINTS_PER_BUFFER = 16 * 256 = 4096)
- `volume_per_sample` 等の per-frame 配列は activate 時固定確保
- `format!()` / `String` / `Box::new` 一切なし (event push のみ)
- Curve 評価は浮動小数のみ、メモリ確保なし
- lane 配列の長さ変動 (lane 追加 / 削除) は Song 再 swap で吸収。Audio thread 中の
  `song.tracks[..].automation_lanes.iter()` は新 song でしか変わらない

## 10. 段階リリース計画

### Phase 1: 骨格 + Track 内蔵 param のみ Read 再生

- [x] `common/src/model.rs`: AutomationTarget / TrackBuiltinParam /
      AutomationContent / AutomationPoint / AutomationCurve / AutomationLane /
      AutomationClip / Track.automation_lanes 追加 (PluginSlot は既存
      `common::protocol::PluginSlot` を流用 + Serialize/Deserialize 追加)
- [x] v7 → v8 migrate + test (`v7_track_loads_with_empty_automation_lanes`)
- [x] `common/src/automation.rs` 新規: curve 評価関数 (Hold / Linear / Bezier /
      Exponential) + `lane_value_at`
- [x] `daw_audio/src/automation.rs` 新規: `fill_track_param_ramps`
- [x] `daw_audio/src/mixer.rs::TrackScratch` に volume_per_sample / pan_per_sample 追加
- [x] `daw_audio/src/engine.rs::process_track_owned` + `run_group_fx_chain`: ramp
      適用ループに変更
- [x] gui_01 #028 起こす + reply 受領 (2026-05-09、 §11 に確定 API)
- [ ] `common/src/model.rs` に **AutomationLaneKey / AutomationClipKey /
      AutomationPointKey** 追加 (gui_01 #028 §11.2 と 1:1)
- [ ] gui_01 Phase 63n-1 commit 受領後に AppData / arrangement view を新 schema へ
      migrate (空 lane 描画まで)
- [ ] `daw_gui/src/view/arrangement_view.rs`: lane を ArrangementAutomationLane に変換、
      EditRequest を AppEvent に変換 (Phase 63n-2 / -3 commit 後)
- [ ] `daw_gui/src/view/track_inspector.rs`: lane list + default knob + last_touched
      ヒント表示 ("press A to add")
- [ ] `daw_gui/src/app.rs`: `last_touched_param: Option<TouchedParam>` 追加
- [ ] AppEvent (gui_01 §11.3 の EditRequest と 1:1 対応):
      `ToggleTrackAutomationCollapsed` / `AddAutomationLane` (= last-touched 経由) /
      `DeleteLane` / `SetLaneDefault { prev, next }` / `SetLaneEnabled` /
      `SetLaneVisible` /
      `AddAutomationPoint` / `MoveAutomationPoints` / `DeleteAutomationPoints` /
      `SetAutomationCurveType { prev, next }` /
      `MoveAutomationClips` / `CloneAutomationClipsLinked` /
      `CloneAutomationClipsIndependent` / `ResizeAutomationClips` /
      `DeleteAutomationClips` /
      `TouchParam` / `AddAutomationFromLastTouched`
      (`SetLaneHeight` は本要望対象外、`MakeAutomationClipUnique` は既存
      `MakeClipUnique` と同 idiom で別途検討)
- [ ] `A` キー shortcut wire (text input / modal open 時は除外)
- [ ] inspector の volume / pan / send / lane default knob drag で `TouchParam` 発火
- [ ] is_undoable 登録 (knob 連続編集は drag end でまとめて 1 step)
- [ ] smoke test: Volume lane 1 本で 0.0 → 1.0 → 0.0 sweep、出音が ramp する
- [ ] smoke test: track の volume knob を回す → `A` キー → Volume lane が出来て
      default = 直前の volume 値で初期化される

### Phase 2: Plugin parameter 連携

- [ ] `daw_plugin_host/src/clap_plugin.rs`: CLAP_EXT_PARAMS 列挙 (count / get_info /
      get_value)
- [ ] `daw_plugin_host/src/vst3_plugin.rs`: IEditController::getParameterCount /
      getParameterInfo
- [ ] `common/src/protocol.rs`: `ChildToMain::PluginParamList` /
      `PluginParamValueChanged` / `PluginParamTouched`
- [ ] `daw_gui/src/app.rs::AppData.plugin_params`: HashMap<(u32, PluginSlot),
      Vec<PluginParamInfo>>
- [ ] CLAP gesture event (`CLAP_EVENT_PARAM_GESTURE_BEGIN`) を audio thread の
      out_events から拾い、`PluginParamTouched` IPC で daw_gui に通知 → AppData の
      `last_touched_param` を plugin param で更新
- [ ] VST3 `IComponentHandler::beginEdit` 経路で同じ通知
- [ ] `daw_audio/src/automation.rs`: PluginParam 用の TimedParamEvent 生成
- [ ] `daw_plugin_host/src/clap_plugin.rs::process` input event 拡張 (param_value 流す)
- [ ] `daw_plugin_host/src/vst3_plugin.rs::process` IParameterChanges 流す
- [ ] smoke test: CLAP synth (Surge / Vital など `audio-ports`+`params` 両対応の
      無料プラグインで gesture も実装されているもの) を load → plugin GUI で cutoff
      knob を回す → `A` キー → Cutoff lane が出来て default = 直前値、再生時に
      cutoff が curve 通りに動く + plugin GUI の knob も同期して動く

### Phase 3: Curve / 編集機能拡張 ✅ **完了** (2026-05-11)

gui_01 #033 (Phase 63n-7 / -8 / -9) と daw_01 wire 全て land。 全項目達成。

- [x] AutomationCurve: Bezier tension / Exponential bend 評価関数 (Phase 1 で
      実装済、 `common/src/automation.rs::apply_curve`)
- [x] Curve type popup を **4 択化** (`["Hold", "Linear", "Bezier", "Exponential"]`、
      `daw_gui/src/view/arrangement_view.rs::automation_point_rects` ループ)。
      popup で Exponential を選んだ point は `AutomationCurve::Exponential { bend: 0.0 }`
      として model に書き込まれ、 audio thread は exponential 評価で再生する。
      widget の描画は #033 完了まで Bezier { 0.0 } fallback 表示
- [x] `AppData.selected_automation_points: Vec<AutomationPointKeyRef>` 追加 +
      `AppEvent::SelectAutomationPoints { prev, next }` (widget は #033 で発火)
- [x] Point copy / paste — `copy_selected_automation_points_as_json` /
      `paste_automation_points_from_json` (Note copy/paste と同 idiom、 normalized
      0..=1 で JSON 化 → paste 先 target に応じて plain 復元、 lane 跨ぎ可)
- [x] Quantize point time_beat to grid —
      `AppEvent::QuantizeSelectedAutomationPoints(div)` + handler。 sort 維持で
      selection も `(snapped_time, value)` で再 lookup して新 idx に更新
- [x] shortcut: Ctrl+C / Ctrl+V / Delete を automation point 選択優先に拡張
      (`daw_gui/src/view/root.rs::dispatch_shortcuts`)
- [x] gui_01 #033 Phase 63n-7 reply 受領 (2026-05-11):
      curve 4 種描画 + `ArrangementCurveKind::Exponential { bend }` variant
      追加 + Bezier 描画式を daw_01 SSoT (新 S 字 cubic) と完全同期
- [x] Phase 63n-7 wire (本セッション):
      (a) `model_curve_to_widget` / `widget_curve_to_model` を 4 種完全変換に
          (Exponential fallback 撤廃)
      (b) popup 選択時 default を `Bezier { tension: 0.5 }` /
          `Exponential { bend: 0.5 }` に変更 (0.0 は新式で Linear 等価、
          選択後 visually 形状変化を保証)
- [x] gui_01 #033 Phase 63n-8 reply 受領 (2026-05-11): lasso 矩形選択 +
      multi-select point drag + selection visual feedback +
      `SelectAutomationPoints` EditRequest + widget API 第 8 引数
      `selected_automation_points: &[AutomationPointKey]` + Response
      field `automation_lasso_active: bool`
- [x] Phase 63n-8 wire (本セッション):
      (a) `arrangement_view.rs::draw` で `selected_automation_points` を
          widget 型 (`AutomationPointKey { clip, point_idx }`) に変換して
          widget 第 8 引数として渡す
      (b) `arrangement_view.rs::make_edit` に `SelectAutomationPoints { prev,
          next }` arm を追加、 widget key → `AutomationPointKeyRef` 変換して
          `AppEvent::SelectAutomationPoints` dispatch
      (c) lasso → copy / paste / delete / quantize batch が即動作
          (selection 配線 + shortcut 優先順は #033 第 1 reply で先行配線済)
- [x] gui_01 #033 Phase 63n-9 reply 受領 (2026-05-11): Bezier/Exponential
      curve 中央 handle drag (lane 高さ連動 30 px = full range / Alt × 0.2
      微調整) + live preview + `SetAutomationCurveParam { point, kind:
      SetAutomationCurveParamKind, prev_value, next_value }` EditRequest
- [x] Phase 63n-9 wire (本セッション):
      (a) `AppEvent::SetAutomationCurveBezierTension` /
          `SetAutomationCurveExponentialBend` 2 variant 追加 (既存
          `SetLaneEnabled` / `SetLaneVisible` 等の per-field 別 variant idiom)
      (b) `set_automation_curve_bezier_tension` / `set_automation_curve_exponential_bend`
          handler 追加 (`matches!` で current curve type と一致するときのみ更新、
          race 防止、 defensive で `clamp(-1.0, 1.0)`)
      (c) `arrangement_view.rs::make_edit` に `SetAutomationCurveParam` arm
          追加、 kind で 2 AppEvent に分岐 dispatch
      (d) `is_undoable` に 2 AppEvent 登録
- [x] **#033 完結** (Phase 63n-7 / -8 / -9 all wired)。 Phase 3 全項目達成。
- [ ] smoke test (gui_01 #033 完了後): lasso で複数 point 選択 → Move /
      Delete / Copy / Paste / Quantize / curve type change が batch で動作する

### Phase 4: Recording (Touch / Latch / Write)

Phase 3 (#033) 完結後 (2026-05-11) より着手。 Step 単位で実装 → smoke test →
commit を回し、 各 Step landing 後に user 目視確認を挟む。

#### Step A: 足場 — RecordingMode enum + transport 4 way toggle ✅ (2026-05-11)

- [x] `common::model::RecordingMode { Read, Touch, Latch, Write }` 定義
      (`Default = Read`、 `Serialize / Deserialize / Encode / Decode` 付き、
      session-only で Song には埋め込まない方針)
- [x] `AppData.recording_mode: RecordingMode` field 追加、 起動時 `Read`
- [x] `AppEvent::SetRecordingMode(RecordingMode)` + handler 追加
      (`is_undoable` に登録せず、 session-only / Undo 対象外)
- [x] `daw_gui/src/view/transport.rs`: Loop button の右に `toggle_button_at`
      × 4 (Read / Touch / Latch / Write) を配置、 active 1 個だけ on_color
      (橙) + hint band で recording 状態を強調
- [ ] **smoke test (Step A)**: `cargo run -p daw_gui` で起動 → transport bar に
      4 way toggle が出る → 各 button click で active 1 個が切り替わる →
      Read 起動デフォルト → 再生 / 停止 / loop 等の既存挙動に regression なし

#### Step B: ParamGesture wire (mixer knobs + last_touched_param 連携) ✅ (2026-05-11)

- [x] `AppEvent::ParamGestureBegin { track_id, target, display_name }` /
      `ParamGestureEnd { track_id, target }` 追加 (`is_undoable` 未登録 =
      session-only)
- [x] `AppData.active_param_gestures: HashSet<(u32, AutomationTarget)>`
      field 追加。 起動時は空、 ParamGestureBegin / End で更新。
      Step C で audio thread がこの set を読んで該当 lane の curve eval を
      bypass する予定
- [x] `daw_gui/src/view/mixer_strips.rs`: per-track strip の volume fader
      (`fader_at`) と pan knob (`knob_at`) の `dragging` 状態と
      `app.active_param_gestures` 内 membership を diff して、 edge
      transition (= drag 開始 / drag 終了) で `ParamGestureBegin` /
      `ParamGestureEnd` を push。 master strip は automation target を
      持たないので skip。 helper: `push_param_gesture_edges`
- [x] `ParamGestureBegin` handler は `last_touched_param` も同時に更新
      (= 既存 `TouchParam` の subsume idiom、 gesture begin の瞬間が touch)
- [ ] inspector lane default knob の wire (lane.default_value も automation
      target を持つので gesture 対象。 Step B follow-up で `arrangement_view.rs`
      / `track_inspector.rs` の lane knob にも `push_param_gesture_edges`
      を仕込む)
- [ ] CLAP plugin GUI の `CLAP_EVENT_PARAM_GESTURE_END` IPC 追加
      (Phase 2c で BEGIN のみ送信中、 END も同 idiom で plugin host →
      daw_gui へ。 Step B follow-up)
- [ ] **smoke test (Step B)**: `cargo run -p daw_gui` で起動 → mixer strip の
      volume fader / pan knob を drag → tracing log 等で
      `ParamGestureBegin / End` の発火を確認 (Step C で audio 影響が出るまで
      visual には反映なし、 invisible wire の確認のみ)

#### Step C: Audio thread recording sampling + IPC

- [ ] `ChildToMain::AutomationPointRecord { track_id, lane_id, time_beat, value }`
      IPC variant 追加 (audio thread → daw_gui)
- [ ] daw_audio: `recording_mode != Read` かつ playback 中で、 lane の target が
      `active_param_gestures` に含まれる場合、 該当 lane の curve eval を bypass
      し、 GUI から流れてきた knob 値を `playhead_beat` 起点 (1/64 beat 刻み)
      で point として書き戻す
- [ ] daw_gui: `AutomationPointRecord` 受信 → 該当 lane の playhead 位置に
      `AutomationPoint` を insert (`SetLaneDefault` と同 idiom、 ただし
      session 中の連続 insert なので Undo は recording stop で 1 step)
- [ ] CLAP plugin GUI の out param value (Phase 2c の `PluginParamValueChangedFromChild`)
      を上記の point insert source にも使う

#### Step D: thinning algorithm

- [ ] Live / Reaper 流の tolerance ε 削減: 直前 point からの y 変化が ε 内で
      かつ x 距離が 1/64 beat 未満なら間引き
- [ ] recording stop (Touch=knob release / Latch=Stop / Write=Stop) で発火、
      1 step Undo で全 inserted point を覆う

### Phase 5: Tempo / TimeSig / Transport event

- [ ] AutomationTarget::SongTempo / SongTimeSigNumerator
- [ ] Master lane (Song level) の表示 (transport 上 or 専用行)
- [ ] CLAP_EVENT_TRANSPORT 実装 (現状 0%)、tempo を毎 buffer 通知

各 Phase で:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo run -p daw_gui    # 手動 smoke
```

を全通過。

## 11. gui_01 #028 確定 API (reply 受領: 2026-05-09)

[docs/gui_01_conversation.md](gui_01_conversation.md) #028 の reply で確定した API。
要望 (初稿) からの主な diff:

| 項目 | 初稿 | 確定 |
|---|---|---|
| EditRequest 命名 | 既存 `MoveClips` を流用 | **別 variant 新設** (`MoveAutomationClips` 等)、別 key 型導入 |
| key 型 | `(track_id, lane_id, clip_id)` 三つ組フラット | **`AutomationLaneKey` / `AutomationClipKey` / `AutomationPointKey`** で構造化 |
| `SetLaneDefault` | `{ value_norm }` のみ | `{ prev, next }` (Undo 構築容易) |
| `SetLaneHeight` | あり | **削除** (lane 高さ drag は本要望対象外、別 phase) |
| default knob 描画 | 円形 knob | **horizontal slider 帯** (M10 track volume slider と同 design) |
| curve type popup | widget 内蔵 | widget は `Response.automation_curve_popup_request: Option<(AutomationPointKey, Rect)>` を返し、daw_01 が `context_menu_for` で開く |
| automation_curve widget 流用 | 検討 | **不採用** (arrangement widget 内蔵で curve 描画 + hit-test) |
| `#[non_exhaustive]` | 検討 | **不採用** (1 commit で全 caller 一括 migration) |

### 11.1 確定 schema (gui_01 側)

```rust
// gui_01: crates/ui/src/widgets/arrangement.rs
pub struct ArrangementTrack {
    // 既存全フィールド維持
    pub automation_lanes_collapsed: bool,
    pub automation_lanes: Vec<ArrangementAutomationLane>,
}

pub struct ArrangementAutomationLane {
    pub id: u32,
    pub label: Arc<str>,
    pub icon_glyph: char,
    pub color: Color,
    pub enabled: bool,
    pub visible: bool,
    pub height_px: u16,            // widget は read-only、caller が値を変えると次フレーム反映
    pub default_value_norm: f32,   // 0.0..=1.0、widget 側 sanity clamp
    pub clips: Vec<ArrangementAutomationClip>,
}

pub struct ArrangementAutomationClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    pub points: Vec<ArrangementAutomationPoint>,
    pub share_group_color: Option<f32>,    // hue 0..1、既存 audio clip と同 helper
}

pub struct ArrangementAutomationPoint {
    pub time_beat: f64,            // clip-local
    pub value_norm: f32,            // 0.0..=1.0
    pub curve: ArrangementCurveKind,
}

pub enum ArrangementCurveKind {
    Hold,
    Linear,
    Bezier { tension: f32 },        // -1.0..=1.0、0.0 で Catmull-Rom
}
```

### 11.2 確定 key 型

`MoveAutomationPoints` などで `(track, lane, clip, point)` を 3〜4 個の `u32` で
書き散らかすのを避けるため、構造化 key を gui_01 側が公開する。daw_01 側でも
`common/src/model.rs` に同形の型を定義し、`AppEvent` でそのまま流す:

```rust
// gui_01 側 (公開型) と daw_01 側 (common/src/model.rs) で 1:1 対応
pub struct AutomationLaneKey { pub track: u32, pub lane: u32 }
pub struct AutomationClipKey { pub track: u32, pub lane: u32, pub clip: u32 }
pub struct AutomationPointKey { pub clip: AutomationClipKey, pub point_idx: u32 }
```

注意: `point_idx` は **同フレーム内のみ valid**。point の add / delete で再採番される
ので、drag session を跨ぐ場合は session 内で `prev_index` を別途保持する。

### 11.3 確定 ArrangementEditRequest

```rust
pub enum ArrangementEditRequest {
    // 既存 (省略)

    ToggleTrackAutomationCollapsed { track: u32 },
    SetLaneEnabled  { lane: AutomationLaneKey, enabled: bool },
    SetLaneVisible  { lane: AutomationLaneKey, visible: bool },
    SetLaneDefault  { lane: AutomationLaneKey, prev: f32, next: f32 },
    DeleteLane(AutomationLaneKey),

    AddAutomationPoint {
        clip: AutomationClipKey,
        time_beat: f64,
        value_norm: f32,
    },
    MoveAutomationPoints(Vec<MoveAutomationPointDelta>),
    DeleteAutomationPoints(Vec<AutomationPointKey>),
    SetAutomationCurveType {
        point: AutomationPointKey,
        prev: ArrangementCurveKind,
        next: ArrangementCurveKind,
    },

    MoveAutomationClips(Vec<MoveAutomationClipDelta>),
    CloneAutomationClipsLinked(Vec<MoveAutomationClipDelta>),
    CloneAutomationClipsIndependent(Vec<MoveAutomationClipDelta>),
    ResizeAutomationClips(Vec<ResizeAutomationClipDelta>),
    DeleteAutomationClips(Vec<AutomationClipKey>),
}

pub struct MoveAutomationPointDelta {
    pub point: AutomationPointKey,
    pub prev_time_beat: f64,
    pub prev_value_norm: f32,
    pub next_time_beat: f64,
    pub next_value_norm: f32,
}

pub struct MoveAutomationClipDelta {
    pub from: AutomationClipKey,
    pub to_lane: AutomationLaneKey,    // lane 跨ぎ可能 (target 不一致でも OK、§5.4)
    pub prev_start_beat: f64,
    pub next_start_beat: f64,
}

pub struct ResizeAutomationClipDelta {
    pub key: AutomationClipKey,
    pub prev_start: f64, pub prev_len: f64,
    pub next_start: f64, pub next_len: f64,
}
```

### 11.4 操作 binding (確定)

| 操作 | EditRequest |
|---|---|
| track 行右端 ▶/▼ click | `ToggleTrackAutomationCollapsed` |
| lane 内 空き領域 click | `AddAutomationPoint` |
| point hover + drag | release 時 `MoveAutomationPoints(deltas)` |
| Alt+click on point | `DeleteAutomationPoints(vec![point_key])` |
| Right-click on point | `Response.automation_curve_popup_request` で daw_01 へ通知 → `context_menu_for(rect, ["Hold", "Linear", "Bezier"], ...)` → 選択 → `SetAutomationCurveType` |
| lane 内 clip drag | release 時 `MoveAutomationClips` / Ctrl で `CloneAutomationClipsLinked` / Ctrl+Shift で `CloneAutomationClipsIndependent` |
| lane header `★` click | `SetLaneEnabled` |
| lane header `👁` click | `SetLaneVisible` |
| lane header `✕` click | `DeleteLane` |
| lane header default slider drag | release 時 `SetLaneDefault { prev, next }` |
| Shift+drag (rect select on points) | Phase 後送り |

### 11.5 gui_01 Phase 分割

gui_01 から提案された 3 phase 分割:

- **Phase 63n-1**: schema 追加 + lane 行 collapsible 描画 + `ToggleTrackAutomationCollapsed`
  のみ発火 (hit-test は基本のみ)。daw_01 は v7 → v8 model migration を完了し、空 lane
  list が render されることを確認できる
- **Phase 63n-2**: point の add / move / delete / curve type popup + lane header の
  default slider / enabled / visible / delete。daw_01 はこの phase で `A` キー bind +
  last-touched param 経由の lane 追加が動かせる
- **Phase 63n-3**: automation clip drag (Move / CloneLinked / CloneIndependent / Resize
  / Delete)

各 phase は独立 commit + visual check 後 daw_01 に reply 形式で進捗共有。phase 跨ぎで
schema の field 削除 / 改名は無い (= 追加のみ)。

### 11.6 daw_01 側 follow-up 決定 (gui_01 reply の 3 件)

1. **lane 跨ぎ target 不一致**: §5.4 の通り **全 accept** (`MoveAutomationClips` /
   `CloneAutomationClipsLinked` / `Independent` 全て)。reject / demote ロジックは
   入れない (Bitwig 流)
2. **Curve type popup**: gui_01 の `Response.automation_curve_popup_request` を
   `arrangement_view.rs::make_edit` で受け、`context_menu_for(rect, &["Hold", "Linear",
   "Bezier"], ...)` を表示 → 選択を `AppEvent::SetAutomationCurveType` に変換 (既存
   "Make Unique" の context_menu 受け idiom と同パターン)
3. **share_group_color**: 既存の audio/MIDI clip 用 hue 算出 (`content_id` の hash →
   `[0.0, 1.0)`) を `arrangement_view.rs` 内 helper でそのまま automation clip にも
   適用

### #029 [将来要望] automation_curve widget の curve 種別対応

現行 [crates/ui/src/widgets/automation.rs](../../gui_01/crates/ui/src/widgets/automation.rs)
は Catmull-Rom 固定。`AutomationCurveStyle` を per-segment で切り替えられるように:

```rust
pub fn automation_curve<F>(
    &mut self,
    id: impl Hash,
    rect: Rect,
    points: &[(f32, f32, AutomationCurveKind)],   // 各 point の incoming curve 種別
    ...
)
```

優先度低 (Phase 3+ で必要時)。

## 12. リスク / 未解決事項

1. **plugin param 列挙のタイミング**: plugin load 直後に列挙して IPC で送るが、
   activate より先か後かで挙動が変わる (CLAP spec: `params.count/get_info` は
   main-thread)。activate 後 + audio thread 開始前に列挙する流れに統一
2. **VST3 の paramID と CLAP の clap_id**: 共に u32 だが、format ごとの値域 / 意味は
   別物。AutomationTarget::PluginParam に `format` フィールドを冗長保持するかは
   `track.fx_chain[idx].format` 経由で十分なので保持しない
3. **lane の display name の locale**: CLAP は ASCII (CLAP_NAME_SIZE=256)、VST3 は
   UTF-16 で来る。daw_gui 側で String → unicode-width で truncate
4. **共有 clip + lane 間の content_id 衝突**: Song.clip_contents は MIDI / Audio /
   Automation を全て同じ HashMap に置く。ContentId の uniqueness は維持されるが、
   MIDI clip が Automation lane 上に置かれる error case を GUI で防ぐ必要 (gui_01
   #028 で **`AutomationClipKey` を独立 key 型化**したことで、widget 側の hit-test /
   selection が MIDI clip と完全に分離される。型違反は compile error で防げる)
5. **Recording mode と RT 安全性**: knob touch 通知は IPC レイテンシ (data plane では
   なく control plane 経由) があるので、Phase 4 設計時に「knob 操作 → 何 ms 以内に
   point 生成されるか」を実測してから recording の許容差を決める
6. **CLAP `request_flush`**: 再生中でない (process が走っていない) 状態で knob を
   動かす → host の `request_flush` で plugin に param 送る必要。Phase 2 で実装、
   audio thread が idle な間は main thread から `flush()` を呼ぶ
7. **VOICEVOX 歌唱パラメータ (将来)**: speaker volume / pitch shift / formant 等を
   automation 対象にしたい場合、AutomationTarget に新 variant `VocalParam` を追加。
   M2 スコープ外
8. **gui_01 Phase 63n-1/2/3 commit 待ち**: schema → point edit → clip drag の 3 phase
   が gui_01 側で順次 land する。daw_01 は 63n-1 commit hash 確認後に AppData の lane
   migration を進め、 63n-2 の commit で UI を point 編集 / lane header 操作に対応、
   63n-3 commit で clip drag を wire する

## 13. 進捗

- [x] Phase 1 model + audio engine 完了 (data 型 / curve evaluator /
      `fill_track_param_ramps` / TrackScratch ramp / process_track_owned 適用)
- [x] gui_01 #028 起こす
- [x] gui_01 #028 reply 受領 (2026-05-09)
- [x] common/src/model.rs に `AutomationLaneKey` / `AutomationClipKey` /
      `AutomationPointKey` 追加
- [x] gui_01 Phase 63n-1 commit 受領 (`a4a06f2`) → AppData
      (`expanded_automation_tracks`) / arrangement_view (lane mapper +
      `ToggleTrackAutomationCollapsed`) migration
- [x] gui_01 Phase 63n-2 commit 受領 (`addadae` + `31d8b46`) → 8 EditRequest
      arm + AppEvent + handler 8 件 (lane header / point edit / curve type
      popup) + `automation_point_rects` ループの `context_menu_for` 接続 +
      `common::automation` に `plain_to_norm` / `norm_to_plain` SSoT 化
- [x] gui_01 Phase 63n-3 commit 受領 (`58bfd75`) → 6 EditRequest arm +
      AppEvent + handler 6 件 (Move / CloneLinked / CloneIndependent /
      Resize / Delete / Select) + `MakeAutomationClipUnique` AppEvent +
      `automation_clip_rects` ループの `context_menu_for` (Make Unique /
      Delete) + `selected_automation_clips` widget 連携
- [x] gui_01 Phase 63n-4 commit 受領 (`d9fdbc1` + `e932874`、 #029
      [Resolved]) → `CreateAutomationClip` arm + AppEvent + handler
      (空 lane で空き dblclick → clip 作成、 MIDI と同 idiom)
- [x] gui_01 Phase 63n-5 commit 受領 (#030 in-flight) →
      `SetLaneHeight` arm + AppEvent + handler (Alt+drag or 下端 splitter
      で lane 高さ変更、 widget 側で min/max clamp 済)
- [x] context_menu_for popup 衝突 fix (point 上の右クリック frame では
      `automation_clip_rects` ループを skip。 これがないと point の
      "Linear" (idx=1) click と clip popup の "Delete" (idx=1) が同時
      発火し clip が消失する bug)
- [x] visual override: `share_group_fill_lightness 0.30` /
      `share_group_border_lightness 0.55` + `clip_selected_fill` を
      暗めの blue-grey + `clip_selected_border` 白系で文字 contrast 確保
- [x] `A` キー shortcut wire ([daw_gui/src/view/shortcuts.rs](../daw_gui/src/view/shortcuts.rs) +
      [root.rs](../daw_gui/src/view/root.rs) で `daw.add_automation_from_last_touched`
      → `AppEvent::AddAutomationFromLastTouched`)
- [x] inspector の volume / pan knob drag で `last_touched_param` を自動更新
      (`set_track_volume` / `set_track_pan` ハンドラ末尾、 lane default knob
      drag (`set_lane_default`) も同パスで更新)
- [x] is_undoable 登録 (lane / point / clip 構造変化系 11 AppEvent を Undo step 化)
- [ ] gui_01 #029 reply 受領 → `CreateAutomationClip` arm + AppEvent +
      handler (clip 自動作成: §5.5)
- [ ] smoke test (実機): `cargo run -p daw_gui` で起動し
      (1) volume knob → A → Volume lane 出現 / default = 直前値で初期化
      (2) lane disclosure +/- で展開・折り畳み
      (3) lane body 空き領域 dblclick で clip 作成 (#029 reply 後)
      (4) clip 内 dblclick で point 追加
      (5) point drag で位置更新 (sort 維持)
      (6) point Alt+click で削除
      (7) point 右クリックで curve type popup
      (8) clip drag (Move / Ctrl=Linked / Ctrl+Shift=Independent / lane 跨ぎ)
      (9) clip 左右 edge drag で resize
      (10) clip 短 click で selection
      (11) clip 右クリック → Make Unique / Delete
      (12) Volume sweep 再生で出音が ramp する
- [x] Phase 3 daw_01 側完了 (2026-05-11):
      curve popup 4 択化 (Exponential 追加) /
      `selected_automation_points` AppData field /
      `AppEvent::SelectAutomationPoints { prev, next }` +
      `AppEvent::QuantizeSelectedAutomationPoints(div)` + handler /
      copy / paste 実装 (Note 同 idiom、 norm 0..=1 で JSON 化) /
      shortcut: Ctrl+C / Ctrl+V / Delete を automation point 選択優先に拡張 /
      is_undoable に `QuantizeSelectedAutomationPoints` 追加
- [ ] gui_01 #033 (2026-05-11 起票): widget 側の curve 4 種描画 +
      tension/bend handle + lasso 矩形選択 + selected point visual feedback。
      reply 受領後に `model_curve_to_widget` Exponential fallback 削除 +
      `SetAutomationCurveParam` 対応 AppEvent + handler を追加

## 14. 主要ファイル変更点

| 層 | ファイル | Phase |
|---|---|---|
| Model | [common/src/model.rs](../common/src/model.rs) | P1 (lane / clip / content / target / slot, v8 migrate) |
| Model | `common/src/automation.rs` (新規) | P1 (curve 評価関数) |
| Audio | `daw_audio/src/automation.rs` (新規) | P1 (collect_automation_for_buffer) |
| Audio | [daw_audio/src/engine.rs](../daw_audio/src/engine.rs) | P1 (volume/pan ramp、process_track_owned) |
| Audio | [daw_audio/src/mixer.rs](../daw_audio/src/mixer.rs) | P1 (TrackScratch.{volume,pan}_per_sample) |
| Audio | [daw_audio/src/sequencer.rs](../daw_audio/src/sequencer.rs) | P1 (call site 整理) |
| Plugin | [daw_plugin_host/src/clap_plugin.rs](../daw_plugin_host/src/clap_plugin.rs) | P2 (CLAP_EXT_PARAMS 列挙、process input event 拡張、pending_events capacity 64→256) |
| Plugin | [daw_plugin_host/src/vst3_plugin.rs](../daw_plugin_host/src/vst3_plugin.rs) | P2 (IEditController::getParameterInfo / IParameterChanges) |
| Protocol | [common/src/protocol.rs](../common/src/protocol.rs) | P2 (PluginParamList / PluginParamValueChanged) |
| GUI | [daw_gui/src/view/arrangement_view.rs](../daw_gui/src/view/arrangement_view.rs) | P1 (lane 描画変換、EditRequest→AppEvent) |
| GUI | [daw_gui/src/view/track_inspector.rs](../daw_gui/src/view/track_inspector.rs) | P1 (lane list + default knob + last_touched ヒント) |
| GUI | [daw_gui/src/view/transport.rs](../daw_gui/src/view/transport.rs) | P4 (mode toggle、bypass) |
| GUI | [daw_gui/src/app.rs](../daw_gui/src/app.rs) | P1+ (新 AppEvent 群、`last_touched_param`) |
| GUI | [daw_gui/src/view/shortcuts.rs](../daw_gui/src/view/shortcuts.rs) | P1 (`A` キー bind) |
