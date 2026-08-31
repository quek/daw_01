# r.md #87 — クリップランチャー / セッションビュー

Bitwig Studio のハイブリッドレイアウト (アレンジの**左**にランチャー、トラック＝行 / シーン＝列) を
daw_01 に入れる。**「もう 1 本のタイムライン」ではなく、行ごとに時間軸の供給元を切り替える機構**として作る。

一次情報:

- Ableton Live 12 マニュアル [Session View](https://www.ableton.com/en/live-manual/12/session-view/) /
  [Launching Clips](https://www.ableton.com/en/live-manual/12/launching-clips/)
  — スロット / シーン / ローンチモード 4 種 / ローンチ量子化 / Legato / フォローアクション 10 種 +
  確率 A,B + Linked-Unlinked + 時間 + 倍率 / 空スロット = 停止 / セッション → アレンジ記録。
- Bitwig ユーザーガイド [The Clip Launcher](https://www.bitwig.com/userguide/latest/the_clip_launcher/) /
  [Triggering Launcher Clips](https://www.bitwig.com/userguide/latest/triggering_launcher_clips_0/)
  — 各トラックの **Stop Clips** ボタン / **最後のスロットの右**に置く
  **Switch Playback to Arranger** ボタン / それぞれのグローバル版 / 空スロットは
  「アーム中なら録音● / 非アームなら停止■」/ シーンは ▶ + 名前 + 色ストライプ /
  ランチャーが主導権を取るのは **トラック単位**。
- Studio One Pro 7 Launcher ([Sound on Sound](https://www.soundonsound.com/techniques/studio-one-clip-launcher))
  — 「タイムラインと横に並ぶ」配置とトラック＝行の裏取り。

## 0. grill で確定した設計判断

| #    | 決定                                                                                                                                                               |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Q1   | ランチャーは**アレンジの左**。Bitwig のハイブリッドレイアウト                                                                                                      |
| Q2   | **トラック＝行 / シーン＝列**。トラックヘッダは 1 本を共有し、行・行高・折りたたみ・縦スクロールはアレンジと完全一致                                               |
| Q3   | シーンは**独立** (`Song.scenes`)。Arranger セクション (`Song.sections`) とは無関係 — 列を足しても曲の長さもクリップ位置も動かない                                 |
| Q4   | セルを置けるのは**全部の行** — 通常トラック (MIDI / オーディオ / 映像 / 画像 / 字幕) と、展開したオートメーションレーンの行                                       |
| Q5   | 既定で表示。**ヘッダとレーンの間の境界をドラッグ**して幅を変える。左端＝アレンジのみ / 中間＝両方 / 右端＝ランチャーのみ                                           |
| Q5-b | `Tab` で 3 レイアウトを巡回 (両方 → ランチャーのみ → アレンジのみ)。「両方」の比率は覚えている。View メニューにも 3 項目                                    |
| Q6   | ランチャー主導の行は、**アレンジ側のクリップを減光**する (+ Switch Playback to Arranger ボタン点灯)                                                                |
| Q7   | セルごとのローンチ設定は**左インスペクタの「ローンチ」セクション**                                                                                                 |
| Q8   | ランチャー演奏のアレンジへの記録は**既存の Rec ボタン 1 つ**が担う (MIDI 入力録音と同時に走る)                                                                     |
| Q9   | **書き出しはランチャーの状態を反映する** (今聴こえている通りに書き出す)。押した瞬間に鳴っているセルを「書き出し範囲の先頭で一斉に撃った」扱いで freewheel 描画する |
| Q10  | ランチャーの状態 (主導権 + 鳴っているセル) は **`.daw` に保存**し、変更で `*` が立つ (= 曲の一部)                                                                  |
| Q11  | 空セルは**停止**。オートメーションレーン行も同じで、セルの無いシーンを撃つとレーン既定値へ戻る                                                                     |
| Q13  | Follow Action の Next / Previous / First / Last / Any / Other が指す範囲は **「同じ行の中で空セルに区切られた連続した塊」** (Live と同じ定義)                      |

Bitwig のスクリーンショットから導出して**質問せずに決めたもの**:

- トランスポートは常に走る。停止 → 再生で**同じセルが鳴り直す** (ランチャーの状態は停止で消えない)
- 主導権は**行単位**でランチャーが奪う。奪う契機は「その行のセルを撃つ」「その行の Stop Clips を押す」「その行を含むシーンを撃つ」
- ローンチ量子化の基準は曲の小節グリッド (`TempoMap` 由来)
- マスター行もランチャーの 1 行として並ぶ (daw_01 はマスター行がアレンジの最上段 = Reaper 流。ランチャーもそれに従う)
- セル本体クリック = 選択 / ▶ クリック = 発火 / 空セルのダブルクリック = 空クリップ作成 / クリップ有りのダブルクリック = ピアノロール・オーディオエディタを開く
- ドラッグの修飾キーは既存のクリップ規約そのまま (素で移動 / `Ctrl` でリンクコピー / `Ctrl+Shift` で独立コピー、`docs/plan_clip_share_clone.md`)
- シーン列の幅は可変 (見出しの境界をドラッグ、全列共通)。あふれた列は横スクロール
- グループトラックの行は**まとめセル** — daw_01 のグループトラックは自分のクリップを鳴らさない
  (`process_track_owned` が `track_has_children` で pass 1 を抜ける) ので、行の意味が衝突しない

## 1. モデル (Song v35)

### 1.1 新しい型 (`common/src/model/session.rs` を新設)

```rust
/// ランチャーの 1 列。Song 内で安定 id。Arranger セクションとは無関係。
pub struct Scene {
    pub id: u32,                      // Song::alloc_scene_id、0 は sentinel
    pub name: String,                 // 空 = 表示側が "Scene N" を自動生成
    pub color: Option<[f32; 3]>,      // None = パレット既定
    pub follow: FollowAction,         // シーンのフォローアクション (Live 12 相当)
}

/// トラック行のセル 1 つ。`clip.id` は `Track.next_clip_id` の id 空間を共有するので
/// `ClipKey { track_id, clip_id }` がアレンジのクリップと同じ形で通る。
pub struct SessionClip {
    pub scene_id: u32,
    pub clip: Clip,                   // start_beat は常に 0 (窓は content_offset_beats + length_beats)
    pub launch: LaunchSettings,
}

/// オートメーションレーン行のセル 1 つ。
pub struct SessionAutomationClip {
    pub scene_id: u32,
    pub clip: AutomationClip,
    pub launch: LaunchSettings,
}

pub struct LaunchSettings {
    pub quantize: LaunchQuantize,     // Global 既定
    pub mode: LaunchMode,             // Trigger 既定
    pub looping: bool,                // true 既定 (false = ワンショット)
    pub legato: bool,                 // false 既定
    pub follow: FollowAction,
}

pub enum LaunchQuantize { Global, Off, Bars(u8), Note { div: u16, triplet: bool } }
pub enum LaunchMode { Trigger, Gate, Toggle, Repeat }

/// Live 12 相当。`a` / `b` の 2 つを確率で選ぶ。
pub struct FollowAction {
    pub enabled: bool,
    pub a: FollowActionKind,
    pub b: FollowActionKind,
    pub chance_a: u8,                 // 0..=100 (b は 100 - a)
    pub linked: bool,                 // true = クリップ終端 / 倍率で発火
    pub time_beats: f64,              // linked == false のときの発火間隔
    pub multiplier: u8,               // linked == true のときのループ回数
}

pub enum FollowActionKind {
    NoAction, Stop, PlayAgain, Previous, Next, First, Last, Any, Other,
    Jump { scene_id: u32 },
}

/// 行の主導権。行 = トラック行 or オートメーションレーン行。
pub enum RowPlayback {
    Arranger,                                    // 既定
    Launcher { clip_id: u32 },                   // 鳴っているセル (列は clip 側が持つ)
    LauncherStopped,                             // ランチャーが握ったまま無音
}
```

### 1.2 既存型への追加

```rust
// Song
pub scenes: Vec<Scene>,                     // #[serde(default)] 空 Vec で forward-migrate
// IdAllocators
pub next_scene_id: u32,

// Track
pub session_clips: Vec<SessionClip>,
pub launcher: RowPlayback,                  // トラック行の主導権 (保存対象)

// AutomationLane
pub session_clips: Vec<SessionAutomationClip>,
pub launcher: RowPlayback,                  // レーン行の主導権 (保存対象)
```

- `clip_contents` / `clip_content_names` は**そのまま共有**する。アレンジのクリップとセルが同じ
  `content_id` を持てる = リンククリップがランチャーを跨いで成立する。`gc_clip_contents` /
  `clip_content_refcount` / `audio_source_refcount` などの参照数え上げに
  **`session_clips` を必ず足す** (落とすと保存時に中身が GC で消える)。
- `Song::ensure_ids` / `ensure_clip_contents` / `sanitize_ranges` / `normalize_after_load` は
  session 側も同じ規則で通す。`ensure_scene_ids` を追加。
- **空 Scene は遅延生成**。旧 `.daw` は `scenes` 空で読み込まれ、グリッドは「実シーン + 表示幅ぶんの
  空きプレースホルダ列」を描く。プレースホルダにクリップを置いた瞬間にそこまでの `Scene` を実体化する。
  → **開いただけで `*` が立たない** (r.md #9 / `feedback_derived_load_collapse_idempotency`)。
- `common/build.rs` の `WIRE_SOURCES` に `src/model/session.rs` を追加 (不変条件 7)。

### 1.3 dirty 規約

`.daw` に保存し `*` を立てる (曲の中身が変わる = 出てくる音が変わる、Q9 / Q10):
`scenes` / `session_clips` / `launch` / `follow` / `Track.launcher` / `AutomationLane.launcher`。

`ViewState` (聴き方の都合、`*` を立てない): ランチャーパネル幅 / シーン列幅 / 横スクロール位置 /
レイアウト巡回の状態 / 「両方」レイアウトの比率。

### 1.4 保存する状態と、しない状態 (フォローアクションの扱い)

`Track.launcher` / `AutomationLane.launcher` ([`RowPlayback`]) が保存するのは
**「ユーザーが最後に撃った状態」** = 再生を始める起点であって、走行中の現在位置ではない。

| いつ | Song を書き換えるか | dirty |
|---|---|---|
| ユーザーがセル / シーンを撃つ・行を止める・アレンジへ返す | **書き換える** | 立てる |
| フォローアクションで次のセルへ移る | **書き換えない** (engine の走行状態のみ) | 立てない |

**フォローアクションの遷移先を保存してはいけない** — 保存すると「何秒鳴らしてから書き出したか」で
出力が変わり、Q9 で守ると決めた再現性 (同じプロジェクト → 同じファイル) が壊れる。走行位置を
保存しないので、書き出しは常に「範囲の先頭で `Track.launcher` を一斉に撃った」状態から始まり、
そこからフォローアクションが決定的に進む。停止 → 再生も同じ起点に戻る。

走行中の現在位置 (いま鳴っているセル / 予約済みのセル / セル内の進捗) は
**`common::audio_bridge` の atomic で GUI へ publish する**だけで、`Song` には入れない
(表示専用。plugin latency を `Song` に載せない r.md #9 と同じ理由)。

## 2. 再生エンジン (daw_audio)

### 2.1 中核 — 行ごとの時間軸

既存の `collect_events_for_buffer(song, track_idx, sr, playhead_beats, bpm, frames, out, active)` と
`render_audio_events(renderer, track_idx, l, r, playhead_beats, ...)` は **すでに `playhead_beats` を
引数で受ける = トラックごとに別の値を渡せる**。ここが設計の梃子で、per-track の時間軸は
**シグネチャ変更なしで**入る。

行 `r` の実効拍を毎 buffer こう解く:

```
Arranger      : effective_beat = playhead_beats、イベント源 = track.clips / arrangement schedule
Launcher(cell): effective_beat = launch_beat + ((playhead_beats - launch_beat) mod loop_len)
                イベント源 = その 1 セル
LauncherStopped: 無音 (オートメーションはレーン既定値)
```

- **ループ境界を跨ぐ buffer は 2 分割**して各セグメントを別々の実効拍で描く (確保なし、
  `render_audio_events` は出力スライスを、`collect_events_for_buffer` は `time` オフセットを分ける)。
- ワンショット (`looping == false`) はセル終端で停止 (＋フォローアクション)。
- `RowTimeSource` (Copy、確保なし) を **音声スレッドが dispatch 前に事前確保済み `Vec` へ埋め**、
  `DispatchShared` にポインタで配って worker が読む (`playhead_beats_bits` と同じ流儀)。

### 2.2 ローンチのキュー

- GUI からの発火は `AudioCommand::LaunchCell / LaunchScene / StopRow / StopAllRows /
  SwitchRowToArranger / SwitchAllToArranger / SetGlobalLaunchQuantize` で届く。
- エンジンは **量子化境界まで待つキュー**を持つ (行ごとに最大 1 件の予約)。境界は `TempoMap` で
  求める (`metronome` の `ClickGrid::Song` と同じグリッド = click / note とズレない)。
- `Legato` は前のセルの位相を引き継ぐ (= `launch_beat` を「前セルの位相が保たれる値」に解く)。
- `LaunchMode` は GUI 側の押下/離しを `LaunchCell { pressed: bool }` で運び、エンジンが 4 モードを解釈。

### 2.3 フォローアクション

- セル終端 (linked、`multiplier` 回ループ後) / `time_beats` (unlinked) で次の行動を決める。
- `chance_a : (100 - chance_a)` で `a` / `b` を抽選。`Any` / `Other` の乱数と合わせて
  **`f(seed, 発火拍)` の純ハッシュ** — 既存モジュレータの決定論規約と同じ (書き出し再現性の前提)。
- **グループ = 同じ行の中で空セルに区切られた連続した塊** (Q13、Live と同じ定義)。
  `Next` / `Previous` / `First` / `Last` / `Any` / `Other` はこの塊の中で解決し、`Next` は末尾で先頭へ巡回する。
  → 空セルを 1 つ置くだけで巡回範囲を区切れる (追加 UI が要らない)。
- シーンのフォローアクションはクリップより優先 (Live 12 の規則)。走行中のクリップの
  フォローアクションは、シーンのそれが発火するまでは動き続ける。
- Follow Action が有効なセル / シーンは **▶ ボタンを縞模様**にして一目で分かるようにする (Live と同じ)。
- 発火はグローバル量子化を迂回するが、**セル自身の `quantize` には従う** (Live の規則)。

### 2.4 RT 安全性

- 新規の確保・ロック・I/O をゼロにする。キュー・時間軸テーブル・グループ集約は全て事前確保。
- セルの探索は `Song` snapshot 上の線形走査 (トラック内セル数は小さい)。`HashMap` は使わない。

### 2.5 書き出し (Q9)

`export.rs` の freewheel を、**書き出し範囲の先頭で「今の `RowPlayback`」を一斉に撃った状態**から
開始する。以降はフォローアクションが決定的に進むので、同じプロジェクト → 同じファイル。
`render_master_buffer` は live と共有したまま (不変条件 6)。

## 3. GUI

### 3.1 レイアウト — `arrangement` widget に帯を 1 本足す

`frame.rs` の矩形分割を `header_pane | ruler / arranger 帯 / lanes` から
`header_pane | 停止列 | launcher 帯 | 返す列 | ruler / arranger 帯 / lanes` へ拡張する。
`tops` (行の縦位置) は既に header と lanes で共有されているので、**行ズレは構造的に起きない**。

```
┌──────────┬─┬───────────────────────┬─┬───────────────────────────┐
│          │▣│ ▶Scene1 ▶Scene2 …     │⇥│ ルーラー / Arranger 帯     │
├──────────┼─┼───────────────────────┼─┼───────────────────────────┤
│ Kick ●SM │■│  ▢   ▶Kick   ▢        │⇥│    ▭Kick#1   ▭Kick#2      │
│ Bass ●SM │■│  ●   ▶Bass  ▶Bass     │⇥│  ▭Bass    ▭Bass           │
└──────────┴─┴───────────────────────┴─┴───────────────────────────┘
  ヘッダ   停止   セル格子 (シーン=列)  返す        アレンジのレーン
```

実装は `daw_gui/src/widgets/arrangement/launcher/` に分割して置く
(`layout.rs` / `draw.rs` / `press.rs` / `drag.rs`)。既存 4 ファイルが 1,000 行 budget に近いので
**新規コードは既存ファイルへ足さない**。

### 3.2 描画

- セル: トラック / クリップ色の面 + ▶ + クリップ名 + 中身のミニ表示 (行が低いときは名前だけ)。
  再生中は**進捗**を重ねる。可変背景の上の標識なので、暗いチップ + 明色記号で
  コントラストを保証する (`feedback_ui_indicator_contrast_on_variable_bg`)。
- 空セル: アーム中なら録音● / 非アームなら停止■。
- グループ行: 子のクリップ色の縞 + シーン名。押すとそのシーンの子セルを一斉発火。
- シーン見出し: ▶ + 名前 (未入力は "Scene N") + 色ストライプ。ドラッグで並べ替え。
- ランチャー主導の行は**アレンジ側のクリップを減光** (Q6)。既存のミュートクリップ減光と同じ語彙。

### 3.3 選択と編集

- セルは `ClipKey { track_id, clip_id }` で指せるので、**選択 SSoT / undo / クリップ色 /
  Mute / 名前 / リンク表示が追加実装ゼロで通る**。
- **ピアノロール / オーディオエディタは「ゼロ」ではなかった** (2026-08-29 実測)。編集面は
  daw_gui 独自の `ClipRef { track: index, clip: index }` (= `track.clips` への添字) で
  クリップを指しており、セルは `Track.session_clips` に居るので**そもそも住所が無かった**。
  住所を安定 id 1 本へ統合して解消する:
  - `daw_gui::ClipRef` を廃止し `common::model::ClipKey` に統合 (widget 層にあった同名の
    mirror 型も畳む = アーキ不変条件 8)。
  - `Track::clip_by_id` / `remove_clip_by_id`、`Song::clip_by_key(_mut)` /
    `notes_in_clip_mut(key)` が `clips` と `session_clips` の**両方**を引く。
  - id ↔ index 変換 (`clip_ref_of` / `clip_key_of`) は生存確認 1 本 (`live_clip_key`) へ。
    index が要るのは行レイアウトとクリップボードの相対トラックだけで、そこは
    `Song::track_index_of` を明示的に通す。
- Del / Cut / Copy / Paste は `feedback_selection_action_last_wins` に従い、直近に触った面で決まる。

### 3.4 インスペクタ「ローンチ」セクション (Q7)

選択中のセルの `quantize` / `mode` / `looping` / `legato` / フォローアクション一式を
既存 idiom (`scrubable_number` / `dropdown` / `toggle_button`) で出す。複数選択は一括変更。

### 3.4-b セルの右クリックメニュー / 長さ

- セル (クリップ有り): 「ピアノロール / エディタで開く」「色...」「独立化」「削除」。
  アレンジのクリップにある操作を帯からも同じように出す。
- 空セル: 「空のクリップを作る」(プレースホルダ列なら `ensure_scene_at` で実体化)。
- セルは格子の中の固定サイズで**掴む端が無い**ので、長さ (= ループ長) は
  インスペクタ「ローンチ」の数値欄が唯一の口。新規セルは拍子から 1 小節ぶん。
- **オーディオをセルへ落としたら小節にフィットさせる** — いちばん近い小節数へ丸め、
  その長さへ time-stretch する (`source_*_frames` は動かさないので中身は全部鳴る)。
  アレンジのレーンへ落としたときは実長のまま。

### 3.5 トランスポート / メニュー / ショートカット

- トランスポートに**グローバルローンチ量子化**の dropdown (既定 = 1 小節)。
  「アレンジに戻す (全行)」は**ランチャー帯の「返す列」上端だけ**に置く — バーにも
  同義ボタンを出すと行側と重複し、しかも記号が Follow の「ページめくり」(`⇥`) と
  ぶつかっていた。
- View メニュー: 「両方 / ランチャーのみ / アレンジのみ」の 3 項目。`Tab` で巡回。
  daw-ui core の default binding にある `tab_next` / `tab_prev` (Tab focus traversal) は、
  `Ui::focusable` の登録が daw_gui / daw-ui widget に 1 つも無く**実挙動が無かった**ので
  `SHORTCUTS` から宣言ごと落とし、`Tab` を巡回に充てた (Ableton と同じ操作感)。
- キーボード: 矢印でセル選択移動、`Enter` で発火、`Delete` で削除、`Ctrl+D` で複製。
- MIDI: `MidiBinding` の入力を **CC 固定からノートも受ける形** (`MidiBindInput`) へ広げ、
  `BindingTarget` に `LaunchCell` / `LaunchScene` / `StopLauncherRow` /
  `StopAllLauncherRows` / `SwitchRowToArranger` / `SwitchAllToArranger` を追加する。
  **表は `Song.midi_bindings` 1 本**に保つ (パラメータ用と別表にすると「この CC を何に
  割り当てたか」を 2 か所探すことになり、同じ入力を 2 つの意味に bind できてしまう)。
  セルは `clip.id` ではなく `(track_id, scene_id)` で指す — パッドの物理位置は
  「このトラックのこの列」に対応するので、セルを差し替えても同じパッドが新しいセルを撃つ。

### 3.6 映像 / 画像 / 字幕プレビュー

`image_compose` / `text_compose` / `video_playback` は既に `playhead_beat` を引数で受けて
`track.clips` を走査するので、**行ごとの実効拍とイベント源**を渡す形に揃える (エンジンと同じ解き方)。
動画書き出し (`render_video.rs`) も同じ解決器を通す。

### 3.7 セルで歌ったときの口パク

アレンジでは「歌唱トラックの歌 → 立ち絵トラックに **1 本の `auto_lipsync` Image クリップ**」。
セルは song 絶対位置を持たない (`SessionClip::clip` の `start_beat` は常に 0) ので、同じものを
**列ごとの口パクセル**として作る。撃つ契機はシーン発火 — 歌のセルと同じ列の口パクセルが
一緒に撃たれるので、`RowTimeline::track_scan` が解く位相の上で口と歌が揃う。

| 入れ物 | 何を覆うか | 長さ |
|---|---|---|
| アレンジ (従来) | `tachie_body_range` (song 絶対) ∪ 開き口の広がり | 範囲の幅 |
| 列 `scene_id` のセル (新) | 位相 `[0, L)` | その列の**歌のセルの最大長**。歌のセルが無い列は立ち絵 body セルの長さ (閉じ口だけ) |

歌のセルがある列で長さを立ち絵セルへ伸ばさないのは、**口は歌と同じ周期で回らないと 2 周目から
ズレる**から (立ち絵 16 拍 / 歌 4 拍なら、口も 4 拍で回って 4 回とも歌う)。

`quantize` / `looping` / `legato` は歌のセル (最優先 = いちばん上のソーストラック) から**写す** —
写さないと最初の 1 発で発火位置がズレる。**フォローアクションは写さない**: 行が違えば空セルの
位置 (Q13 のグループ) も違うので `Next` の行き先が食い違い、`Any` / `Other` は行ごとに独立に
抽選される。写すと「別の歌詞の口が動く」= 動かないより悪い。**残課題**は、口の行を歌の行へ
従属させて (行の主導権をコピーではなく参照にして) フォローアクションまで自動で追わせること。

#### 平坦化タイムライン (`common::lipsync::LipsyncLayout`)

phoneme query は背景スレッドで走り、複数のソーストラックの結果を **優先度つきで 1 本にマージ**
してから入れ物へ配る (上のトラックが勝つ、`docs/plan_voicevox_talk.md`)。マージは 1 本の拍軸の
上でしか定義できないので、アレンジと各列を**重ならない帯**へ並べた合成座標を 1 本用意し、
マージ後に帯で切り分けて入れ物へ戻す。これは口パク生成の内部座標で、音にも `.daw` にも出ない。

- セルの帯は原点が「撃った瞬間 = 位相 0」。発注時に `content_offset_beats` を引くので、
  窓の外の note が隣の列の帯へはみ出さない。
- 発注時と適用時で**同じ表**が引けることが前提。`mark_lipsync_dirty` が Song 編集のたびに世代を
  上げ、`LipsyncGenerated` は世代一致のときだけ適用されるので、飛行中に Song が変わった結果は
  そもそも捨てられる (§1.4 の「走行位置は保存しない」と同じ、決定論の担保)。
- 入力 fingerprint と snap 収集は `LipsyncLayout::placements` / `lipsync_source_of` の
  **同じ 1 本**を通す。片方だけ対象がずれると「セルの歌詞を直したのに口パクが再生成されない」が
  静かに起きる。

#### 触ってはいけない側

- `common::lipsync::tachie_body_range` は **song 絶対拍**。セルの `start_beat` は常に 0 なので
  混ぜると範囲が曲頭へ引きずられ、立ち絵が映っていない曲頭に閉じ口が敷かれる
  (`Track::all_clips` の契約どおり「時間軸そのものを扱う側」)。列版は長さ 1 つを返す別関数。
- 口パクセルは**自動生成物**なので、その列に手で置いたセルがあれば何もしない。列にセルは 1 つ
  しか置けないため、アレンジの `place_clip` (重なりを削り取る) と違って上書き = まるごと消滅になる。
- 作り直しでも `clip.id` は使い回す。変えると `RowPlayback::Launcher` の指す先が消えたことになり、
  `normalize_session` が行を停止に落として**鳴っている最中に口が消える**。
- 出力先がグループ行 (セルを持てない行、`row_accepts_cells`) なら生成しない (置いても永久に
  鳴らない)。取り残された生成物の掃除だけ行う。

## 4. テスト

- **モデル**: v33 `.daw` の forward-migrate / save-load 往復 / `ensure_ids` の冪等性 /
  refcount と GC がセルを数えること / 「開いただけで `*` が立たない」回帰。
- **エンジン (純粋ロジック)**: 量子化境界の解決 / ループ跨ぎ buffer の 2 分割 /
  フォローアクション 10 種の遷移表 / 乱数の決定論 (同じ seed と拍 → 同じ結果) /
  空セル発火が停止になること。
- **統合**: `AppData::handle_event` 経由でセル発火 → `RowPlayback` の遷移、
  シーン発火で空セル行が止まること、Rec 中の発火がアレンジのクリップとして落ちること。
- **書き出し**: 同じランチャー状態から 2 回書き出して byte 一致。
- 自明な算術の写経テストは書かない (`feedback_no_tests_for_simple_cases`)。

## 5. 並列作業の分担と統合順

| 束                       | 範囲                                                                                   | 主に触るファイル                                                                     |
|--------------------------|----------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------|
| **A. モデル**            | §1 全部 + migration + refcount/GC + テスト                                            | `common/src/model*`, `common/src/project.rs`, `common/build.rs`                      |
| **B. エンジン**          | §2 全部 (時間軸 / キュー / フォローアクション / 書き出し)                             | `daw_audio/src/**`, `common/src/protocol.rs`                                         |
| **C. ランチャー widget** | §3.1-3.3                                                                              | `daw_gui/src/widgets/arrangement/**`                                                 |
| **D. GUI 配線**          | §3.4-3.5 (インスペクタ / トランスポート / メニュー / ショートカット / MIDI / handler) | `daw_gui/src/view/**`, `daw_gui/src/handler/**`, `daw_gui/src/state/**`              |
| **E. 映像系**            | §3.6                                                                                  | `daw_gui/src/{image,text,group}_compose.rs`, `video_playback*.rs`, `render_video.rs` |

**統合順**: **A を先に main へ入れる** (B〜E 全部の前提)。その後 **B / C / D / E を並列**、
統合は `protocol.rs` を触る B → C → D → E の順 (C と D は `view` と `widgets` で分かれるが、
`arrangement_view.rs` が両方の接点なので C を先に入れる)。

**衝突の実測**:

| ファイル                 | A    | B            | C                                         | D          | E    |
|--------------------------|------|--------------|-------------------------------------------|------------|------|
| `common/src/model*`      | 全面 | —           | —                                        | —         | —   |
| `common/src/protocol.rs` | —   | variant 追加 | —                                        | 送信側のみ | —   |
| `daw_audio/**`           | —   | 全面         | —                                        | —         | —   |
| `widgets/arrangement/**` | —   | —           | 全面 (新 `launcher/` + `frame.rs` の分割) | —         | —   |
| `view/**` `handler/**`   | —   | —           | `arrangement_view.rs` のみ                | 全面       | —   |
| `*_compose.rs` `video_*` | —   | —           | —                                        | —         | 全面 |

## 6. 各 worktree の作業前チェック

1. `make fetch-ffmpeg` (worktree には `third_party/` が無い)
2. `cargo build -p daw_audio -p daw_plugin_host` (子プロセスの exe が要る)
3. `make arch-lint` を 1 回走らせて出発点を記録する

## 7. 全 worktree 共通の禁止事項

- **`make test` を走らせない** (daw_gui 本体を起動して再生を壊す)。`make test-nolaunch` か
  `cargo test -p <crate> --test <name>`。
- **daw_gui を起動しない**。実機確認が要るならユーザーへ事前に断る。
- `r.md` を編集しない。commit はユーザーの sign-off を得てから。
