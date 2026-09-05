# 時間範囲選択への統一 / クリップ非オーバーラップ / 範囲駆動 Glue

選択を「オブジェクトの集合」から **「時間区間 × レーン集合」1 本**へ統一する。あわせて
「1 トラックの 1 時点にクリップは 1 つ」を `Track` の不変条件として保証し、`J` (結合) を
範囲駆動へ移す。

一次情報は Ableton Live 12 Reference Manual (`https://www.ableton.com/en/live-manual/12/<slug>/`)。
本文中の §番号はそこを指す。**実機観測で裏を取った項目はその旨を明記する** — マニュアルに
記述が無い / 記述と実機が食い違う箇所が複数あった。

---

## 1. 理想

> **選択 = 「時間区間 × レーン集合」1 本。オブジェクト (クリップ / ノート / automation 点 /
> automation クリップ / audio event) は、そこに交差するものとして導出される。**

「レーン」を面ごとに読み替えるだけで、全面が同じ仕組みになる。

| 面 | 「レーン」= | 見た目 |
|---|---|---|
| アレンジャー | トラック行 / オートメーションレーン行 | 横方向の帯 |
| ピアノロール | **鍵盤の行** (Live の "key track"、§10.5.2) | 時間 × 鍵盤の矩形 |
| オーディオエディタ | 波形の行 | 時間帯 |

これが Live の Arrangement のモデルである根拠 (実機観測):

- **離れた 2 クリップを Ctrl+クリックすると、間のクリップも選択される。**
  → Arrangement の選択は単一の連続した時間範囲 × トラック集合が 1 本あるだけで、
  「クリップ選択」はその特殊形 (範囲がクリップの占有区間に一致した状態)。
- §6.9 が「クリップをクリック」と「ドラッグで timespan」を並記しているのは、
  同じ 1 つの選択の 2 通りの作り方だから。
- §20.1 が "time selections … every track **on which a clip is selected**" と言い換えられるのも同じ理由。
- 決定的に — **Arrangement には Enter トグルが無く、ノートエディタにだけある** (§10.5.2)。
  アレンジャーは選択が 1 本しか無いから切り替える必要が無い。

**daw_01 は Live と違い、ピアノロールもこの 1 本に統一する。** Live のノートエディタは
投げ縄 (2 次元) + 全鍵の時間範囲という 2 本立てだが、面ごとに操作感が変わって混乱するので採らない。
ピアノロールの投げ縄は「レーン = 鍵盤行」と読み替えた**同じ範囲選択**になる。

**引き換えに非連続の選択は捨てる。** アレンジャーでは Live どおり (間のクリップも入る)。
ピアノロールでは Live より弱くなる (Live は Shift+クリックで非連続ノートを拾える) が、
一貫性を優先する。

### 1.1 編集操作の二分

| 種類 | 対象 | 挙動 | 例 |
|---|---|---|---|
| **範囲操作** | 時間区間 | 範囲の境界で**分割**し、範囲部分だけに適用 | Delete / Cut / Copy / `J` / ミュート (`Q`) / フェード |
| **属性操作** | 交差するオブジェクト全体 | オブジェクトそのものに適用 | 改名 (`F2`) / 色 / インスペクタ各項目 |

Live の §6.9 が同じ二分を持つ ("Pressing the 0 key **deactivates a selection of material**, even if it
contains multiple clips" / "It is possible to **reverse a selection of audio material**, even if it
contains multiple audio clips")。

---

## 2. 型と、消えるもの

```rust
/// 選択の SSoT。session-only (保存しない)。
pub struct TimeSelection {
    /// song-absolute 拍。start < end を常に満たす (幅ゼロは None で表現)。
    pub start_beat: f64,
    pub end_beat: f64,
    /// 掛かっているレーン。面ごとに種類が違う。
    pub lanes: Vec<LaneRef>,
}

pub enum LaneRef {
    Track(u32),                        // アレンジャーのトラック行 (Track::id)
    Automation(AutomationLaneKey),     // オートメーションレーン行
    KeyTrack { clip: ClipKey, pitch: u8 },  // ピアノロールの鍵盤行
    AudioLane(ClipKey),                // オーディオエディタの波形行
}
```

### 2.1 消える実体

| 消えるもの | 場所 | 置き換え |
|---|---|---|
| `selected_clips` / `selected_clip` | `state/selection.rs:48-50` | 範囲からの導出 |
| `selected_notes` | `:51` | 同上 |
| `selected_automation_points` | `:45` | 同上 |
| `selected_automation_clips` | `:32` | 同上 |
| `audio_editor_selected_events` | `:59` | 同上 |
| アンカー 9 本 (`clip_anchor` / `note_anchor` / `automation_point_anchor` / `automation_clip_anchor` / `audio_editor_anchor` …) | `:69-93` | 範囲のアンカー 1 本 |
| `EditSurface` の `Clips` / `Notes` / `AutomationPoints` / `AutomationClips` / `AudioEvents` | `app_types.rs:846` | `TimeRange` 1 面 |
| 矩形選択 (marquee) 実装 | `release.rs:792-991` | 範囲 |
| 投げ縄実装 | `press_lanes.rs:435`, `release.rs:406-510` | 範囲 |
| ピアノロールの `rect_select` | `piano_roll/run.rs:560` | 範囲 |
| 範囲ブロック選択 (`select_modifier.rs` の `range_block`) | 面ごとに 5 経路 | 範囲の伸縮 1 経路 |
| `Shift+L` (リンククリップ選択) | `selection_view.rs:936` | **撤去** (非連続選択が作れないため。§10) |

### 2.2 残るもの

- **ランチャー (セッション) のセル選択** — 時間軸を持たないので範囲では表せない。
  `selected_launcher_cells: Vec<ClipKey>` として独立のオブジェクト選択のまま残る。
- `selected_track_ids` / `selected_section_ids` / `selected_scene_ids` / `selected_device_ids` —
  いずれも時間軸を持たない。
- `EditSurface` は **8 面 → 5 面** (`TimeRange` / `LauncherCells` / `Tracks` / `Sections` / `Devices`)。
  last-wins arbiter (`selection_view.rs:42 edit_surface`) は残るが大幅に単純になる。
  `TimeRange` の中では「どの面か」を範囲の `lanes` が既に持っているので、面の裁定が要らない。

### 2.3 幅ゼロと永続化

- **幅ゼロの範囲 (insert marker) は持たない。** Live は insert marker を再生開始位置に使うが
  (§6.3)、daw_01 は再生ヘッドをルーラー専用のまま据え置き、**範囲は再生位置に一切関与しない**。
  範囲が幅ゼロになる場面では `None` にする。
- **永続化しない** (session-only)。`Esc` でクリア。
  旧 `selected_clips` は ViewState に保存されていたが、その保存項目は落とす。

---

## 3. 範囲の作り方

### 3.1 マウス

| 場所 | 操作 | 結果 |
|---|---|---|
| トラックレーンの空き | 左ドラッグ | 範囲 |
| クリップの**本体** (ヘッダ以外) | 左ドラッグ | 範囲 (§6.9 "Clicking and dragging in the clip's waveform or MIDI display allows you to select time within the clip") |
| クリップの**ヘッダ** | クリック | そのクリップの占有区間 × そのトラック |
| クリップの**ヘッダ** | 左ドラッグ | クリップ移動 (**素材を動かせるのはここだけ**。範囲の内側でもヘッダ以外は範囲の引き直し) |
| クリップの**ヘッダ / 端の resize ハンドル** | **Alt+左ドラッグ** | 範囲 (Live の "Select Time Within Clip"。マニュアルは Shift+Alt と書くが**実機は Alt 単独**)。端も含めるのは、短いクリップでは端ハンドルが幅の大半を占め、範囲を引くのにズームが必須になるのを避けるため |
| オートメーションレーンの空き | 左ドラッグ | 範囲 |
| オートメーションクリップの**名前帯** | 左ドラッグ | クリップ移動 (r.md #109: トラック行のクリップと同じ規約。点 / 線の当たりはこれより先に効く) |
| オートメーションクリップの**本体** | 左ドラッグ | 範囲 |
| オートメーションクリップの**名前帯 / 端** | **Alt+左ドラッグ** | 範囲 (点の上は削除、線の上は曲げるが先勝) |
| ピアノロール | 左ドラッグ | 時間 × 鍵盤行の範囲 (**x はグリッドにスナップ**、アレンジャーと同じ帯で描く。旧・矩形選択は撤去) |
| ピアノロールのノート | クリック | そのノートの区間 × その鍵盤行 |
| Ctrl+クリック | — | 範囲を**外接まで拡張**する (間のオブジェクトも入る) |
| Shift+クリック | — | アンカーから範囲を伸縮 |

**Alt の二重の意味**: ジェスチャの種類は**押した瞬間の Alt** で決まり、**ドラッグ中の Alt は
スナップの on/off** (離す = 有効 / 押す = 無効)。`last_alt` が continuation フレームで更新される
既存構造 (`drag.rs:76`) をそのまま使う。

**行が低くてヘッダだけになったら、クリップ上は常に移動。** Live も §6.9 で
"To access the time within a clip for editing, **unfold its track**" と明記している。
低い行のまま範囲を引きたいときはヘッダ上の Alt+ドラッグを使う。

### 3.2 キーボード

| キー | 動作 |
|---|---|
| `Shift`+←→ | 範囲の端を伸縮 |
| `Shift`+↑↓ | 範囲をレーン方向に伸縮 |
| ←→ | **範囲内の素材**をグリッド 1 つ分ナッジ (§6.9 "You can nudge a selection of material using the left and right arrow keys") |
| `Alt`+←→ | 同上、スナップ無効 |
| `Ctrl+A` | その面の全体 (アレンジャーなら全トラック × 曲の先頭〜最後のクリップの終わり) |
| `Esc` | 範囲をクリア |

ピアノロールでは ←→ が「範囲内のノートをナッジ」、↑↓ が「移調」になる (現行の 12 本の割り当てを
そのまま範囲駆動へ移す)。

### 3.3 右ドラッグ

context menu 専用に戻す (現状は marquee にも使われている、`release.rs:943`)。

---

## 4. 描画

- 範囲は**半透明の明色帯**で塗り、**左右端に縦線**を引く。**クリップ / ノートの上に重ねる** —
  部分的に覆っているとき、どこからどこまでが範囲かが見える。
- 塗る範囲は**レーンごと**にその行の高さいっぱい。縦線もその行の中だけ (ルーラーまで貫通させない)。
- 色は既存のテーマ機構 (`Palette` / `DawColors`、`themes/*.json`) に 1 色追加。
  値は linear (render target が sRGB のため、画面色から書くなら `srgb()` を通す)。
- **クリップのヘッダ帯は不可視のまま**。ヘッダと本体の区別は**マウスカーソルの形**で示す
  (`widgets/arrangement/cursor.rs`)。
- クリップ色 / トラック色 / 波形の上に重なるので、**明るいクリップと暗いクリップの両方で
  コントラストを目視確認**する。

---

## 5. オートメーション追従設定

- グローバル設定「オートメーションをクリップに追従」。**アレンジャー上部の Snap toolbar に
  トグルボタン**として常時表示 (`view/arrangement_view.rs:1190 draw_snap_toolbar`、既存の
  トグルスタイルを流用)。アプリ全体で永続、**既定 ON**。
- **効くのは編集だけ。** 範囲のハイライトは常に「ドラッグが実際に横切った行」。
- ON のとき、トラック行に掛かった範囲への Delete / Cut / Copy / 移動 / Duplicate / `J` が、
  **閉じているレーンも含めて**そのトラックの automation に同じ範囲で適用される。OFF なら触らない。
- オートメーションレーン行を直接ドラッグしたときは、設定に関係なくその automation だけが対象。
- 開いているレーンには連動ハイライトを出して、何が付いてくるかを予告する。

---

## 6. クリップの非オーバーラップ

### 6.1 不変条件

> **同一トラックの `Track.clips` は、時間的に重ならない。**

現状はこの契約がコードにもドキュメントにも一切無く、移動 / リサイズ / 貼り付け / 複製 / 録音 /
インポート / 新規作成の**全経路**がガード無しで重なりを作れる。再生側は解決規則がレイヤごとに
バラバラ (audio = 加算 / MIDI = 独立 emit / video = emit 順で後勝ち / automation = 先勝ち)。

Live のマニュアルにこのルールの明文は無い。あるのは "A track can only play one clip at a time"
(§3.7) と、同時に鳴らしたければ take lane を使えという設計方針 (§21) だけ。
したがって**これは daw_01 が自分で書く不変条件**である。

### 6.2 解決規則 — 上書き (overwrite)

**新しく置かれた方が勝つ。**

| 状況 | 既存クリップ B の結果 |
|---|---|
| A が B を完全に覆う | B は消える |
| A が B の端に食い込む | B はその分だけ縮む (trim) |
| A が B の真ん中に落ちる | B は 2 つに分割され、A の下だけが消える |

Undo で完全に復元できる (`edit_song()` が full-song snapshot を積むため)。

### 6.3 適用経路 (全 8 経路、例外なし)

移動 / リサイズ (端ドラッグ) / 貼り付け / 複製 (`D` / `Alt+D` 連打) / 録音 / インポート /
ダブルクリック新規作成 / ランチャーからアレンジャーへのドロップ。

現状 `Clip` の 13 フィールド構築が 14 箇所に手写しされ、`Track` にクリップ追加 API が無い。
不変条件を守らせる**単一の口 `Track::place_clip(clip) -> ClipKey`** を新設し、全経路をそこへ通す
(import 経路が `place_imported_clip` に集約されている先例と同じ)。

### 6.4 既存プロジェクトの移行

**開いた瞬間に上書き規則で解決する。** `Track.clips` の配列順で後ろにあるもの
(= 描画で前面に来ているもの) が勝つ。中身が変わるので **`*` (未保存) が立つ**。

「開いた直後だけ不変条件が破れている」状態を許さない — 許すと全コードが「重なっている
かもしれない」前提を持ち続け、不変条件の意味が消える。

### 6.5 Auto-Crossfade の作り直し (機能復旧)

現状の Auto-Crossfade (`handler/audio_editor.rs:557`) は「**重なっている** audio クリップの
ペア」(`next_start < prev_end`) を探すので、非オーバーラップ化で空振りする。

**隣接ペアを対象に、真のクロスフェードとして作り直す。** 境界を挟んで、左クリップは末尾 N 拍で
フェードアウトしつつ**クリップの外の素材まで読み進め**、右クリップは先頭 N 拍でフェードインしつつ
**クリップの手前の素材から読み始める**。クリップの占有範囲は重ならないまま、鳴らす範囲だけが
境界を跨ぐ。実装上は `audio_clip_renderer.rs:718-724` の clip gate をフェード長ぶん緩める。

**Live の「隣接クリップに自動で 4ms クロスフェードが付く」(§6.8) は入れない。**
フェードはユーザーが明示的に付けたときだけ。Auto-Crossfade は明示的に呼ぶコマンドのまま。

---

## 7. `J` (結合) の範囲駆動

### 7.1 結果

- **結果クリップ = 範囲そのもの**。`start_beat` = 範囲の先頭、`length_beats` = 範囲長。
- 範囲が中身より広ければ、**前後の空白は content 内の「何も無い区間」として自然に表現される**
  (`content_offset_beats` を負にする必要は無い)。
- **範囲からはみ出た部分は、範囲の境界で分割されて元のクリップとして残る。**
  例: クリップ A (0〜16) に範囲 4〜12 で `J` → `0〜4` / `4〜12` (新しい結合クリップ) / `12〜16`
  の 3 クリップ。Live の `Ctrl+E` "Split Clip at Selection" (§6.12) と同じ切り出し。
- **トラックごとに 1 クリップ** (§6.13 / §7.5)。範囲内にクリップが 1 つも無いトラックには
  何も作らない (§7.5 "creates a new sample for every audio track in the selection **that contained
  at least one clip**")。
- **「2 個未満は拒否」は撤廃** — 範囲があればクリップ 1 個でも意味が定まる (crop + 空白付与)。

Live の Consolidate は audio を新規サンプルへ書き出して **normalize** する非中立操作
(§38.3.7)。daw_01 は event を crop する非破壊方式を維持し、ここは Live に合わせない。
なお「選択範囲が素材より広い / 狭いときの結果長」は Live のマニュアルに定義が無く、
**daw_01 独自の明示的決定**である。

### 7.2 対応する種別

| 種別 | 現状 | 変更後 |
|---|---|---|
| MIDI / Audio / Video / Image | 対応 | 範囲駆動へ |
| **Text (字幕・タイトル)** | **未実装** (`glue.rs:179-182` で「混在」扱いで拒否) | **実装する** |
| Automation | no-op | 追従設定 ON なら範囲内の automation クリップを 1 つに結合。レーン行を直接選んだ場合は設定に関係なく対象 |

### 7.3 既存バグの修正

`had_mixed_kind` がトラックループの外で宣言され、一度立つとリセットされない (`glue.rs:117`)。
あるトラックで混在が起きると以降のトラックが全部 skip され、**しかも先に結合済みのトラックの
編集は残ったまま**「Glue できません」で終わる。トラックごとに閉じる。

### 7.4 ピアノロールの `j`

Live はビューごとに `Ctrl+J` の意味を変える — アレンジャー = Consolidate、
**ノートエディタ = Join Notes** (§10.5.8.3)。daw_01 も同じにする。
ピアノロールで `j` → 範囲内の**同じピッチのノート**を 1 本に結合する。

---

## 8. 各コマンドの挙動

| コマンド | 範囲に対して |
|---|---|
| `Delete` | 範囲の境界で分割し、範囲部分だけ削除。時間は詰めない。**ノート / audio event も同じ**(部分的に掛かったものは分割されて範囲部分が消える) |
| `Ctrl+X` / `Ctrl+C` | 範囲の形のまま (前後の空白込みで) クリップボードへ。Cut は範囲部分を削除 |
| `Ctrl+V` | **マウスカーソルの位置** (ホバー中のトラック + 拍) を先頭にして貼る (現状維持、`clipboard_ops.rs:196-201`)。貼り先の既存クリップは上書き規則で削られる |
| `J` | §7 |
| `Q` (ミュート) | 範囲の境界で分割し、範囲部分だけをミュート (§6.9 "Pressing the 0 key deactivates a selection of material") |
| フェード | 範囲部分に適用 |
| `R` (Loop) | 範囲の区間をそのまま Loop 範囲に (空き領域に引いた範囲でも効く) |
| `Z` (ズーム) | 範囲の区間へズーム |
| `F2` / 色 / インスペクタ | **範囲と交差するオブジェクト全体**が対象 (範囲操作ではない) |

Live の "…Time" コマンド群 (Cut/Copy/Paste/Duplicate/Delete Time / Insert Silence、§6.11) は
**今回は入れない**。曲の尺を変える編集は Arranger セクションの機能で行う。

---

## 9. 撤去する機能

一貫性のために失うものを明示しておく。

| 機能 | 理由 |
|---|---|
| **非連続の選択** (Ctrl+クリックで離れたものだけを拾う) | 選択が単一の区間 × レーン集合になるため。アレンジャーは Live どおり (間も入る)、ピアノロールは Live より弱くなる |
| **矩形選択 (marquee)** / **投げ縄** / ピアノロールの `rect_select` | 範囲が引き継ぐ |
| **`Shift+L` (リンククリップ選択)** | リンク先は散らばっており、単一の区間で表せない |
| **automation 点の値方向での絞り込み** | 投げ縄の撤去に伴う |
| 右ドラッグでの矩形選択 | context menu 専用に戻す |

---

## 10. 付随して直すもの (調査で判明した既存の不整合)

範囲操作は「境界での分割」を多用するので、分割の実装が 1 本でないと破綻する。

1. **split が 2 実装ある。**
   `handler/clips.rs:818 split_clip_at_beat` は両側を新 ContentId に fork し、**Text / Automation
   非対応** (`:973`, `:1073` で `false` を返し、しかも「カーソルが clip 範囲外」という**実態と違う**
   status を出す)。`common/src/model.rs:1334 Song::split_clips_at` は左を窓の切り詰めだけで済ませ、
   右のみ fork し、**全 variant 対応**。後者が窓モデル (`Clip = content の窓`) に忠実なので、
   **こちらへ 1 本化**する。
2. **Text clip の split 未実装** — 範囲操作に必須なので実装する。
3. `clips.rs:1082` の `next_content_id.saturating_sub(1)` による front content id 逆算ハックを廃す。
4. **MIDI sequencer の早切れ** — `active_notes: Vec<u8>` (`sequencer.rs:57`) が pitch を refcount せず
   `swap_remove` (`:234`) するため、重なった clip が同ピッチを鳴らすと早切れ / stuck の余地がある。
   非オーバーラップ化で実質解消するが、契約としてコメントに明示する。
5. `Track.clips` の doc に順序・非重なりの契約を書く (現状コメントが 1 行も無い)。
6. ノートの分割 API が無い (ピアノロールは `resolve_note_overlaps` しか持たない)。
   範囲 Delete / `Q` で必要になるので追加する。

---

## 11. アーキテクチャ不変条件との整合

| 不変条件 | 影響 |
|---|---|
| 1 安定 id addressing | `LaneRef` は `Track::id` / `AutomationLaneKey` / `ClipKey` の安定 id。positional index を使わない。`audio_editor_selected_events` が `Vec<usize>` (index) だったのが id ベースへ移る**副次的な改善**になる |
| 2 wire は blob-less | 範囲は GUI 内で完結。IPC を渡らない |
| 3 宛先は型で表現 | 新しい IPC message は不要 |
| 4 RT スレッド | 範囲は RT パスに出ない。Auto-Crossfade の gate 緩和は再生前に焼き込む (`RenderedEvent.gate_*`) ので RT で確保しない |
| 5 `edit_song()` チョークポイント | 範囲操作はすべて `edit_song()` 経由。`Track::place_clip` も closure の中で呼ぶ |
| 6 live と export は同じ render 関数 | クロスフェードの gate 緩和は `audio_clip_renderer` 1 箇所 |
| 7 fingerprint handshake | wire 型を新ファイルへ切り出さないので `WIRE_SOURCES` の追加は不要 |
| 8 daw-ui core はドメイン知識を持たない | `TimeSelection` は `common::model` に置き、widget は `daw_gui/src/widgets/` で直結。mirror 型を作らない |
| 9 サイズ budget | `mod.rs` (2388 物理行) / `draw.rs` (2118) / `geometry.rs` (1841) / `release.rs` (1356) は既に大きい。範囲のヒットテスト・描画・コミットは**新ファイルへ分ける**。着手前に `python scripts/loc_budget.py --report` で実コード行を確認する |

---

## 12. テスト

高いレイヤーから。自明な算術をテストへ写すだけのテストは書かない。

**モデル層 (`common`)**
- 上書き規則: 完全被覆で削除 / 端の食い込みで trim / 中央で 2 分割 — パラメタライズド 1 本
- load 時の重なり解決が**冪等** (2 回通しても同じ結果、`*` は 1 回目だけ)
- `Track::place_clip` を通した全経路で不変条件が保たれる

**コマンド層 (`daw_gui`)**
- 範囲 Delete: A(0-8) B(8-16) に範囲 4-12 → `0-4` と `12-16` が残る
- 範囲 `J`: A(0-16) に範囲 4-12 → 3 クリップになり、中央が `start=4 / length=8`
- 範囲 `J`: 中身が 6-10 しか無い範囲 4-12 → `start=4 / length=8` で content の 2-6 拍に素材
- 範囲 `Q`: 範囲部分だけ `muted=true` の 3 クリップになる
- 導出: クリップヘッダをクリック → 範囲がそのクリップの区間になり、交差クリップがそれ 1 つ
- 導出: Ctrl+クリックで離れた A と C → 範囲が外接区間になり、間の B も交差クリップに入る
- ピアノロール: 範囲 Delete で部分的に掛かったノートが分割される
- Text clip の split / glue が通る

**GUI / 見た目**
- ヘッダとボディの当たり判定、カーソル形状、範囲の描画は自動テストで拾えないので実機で目視。
  明るいクリップ色と暗いクリップ色の両方で範囲帯のコントラストを確認する。

---

## 13. 実装メモ (landing 時点の実際)

計画からの差分と、意図的に残した点。

### 13.1 範囲が「どの面」かはレーンの種類が持つ

`AppData::time_selection_surface()` が、範囲の `lanes` から面を決める:

| レーン | 面 |
|---|---|
| `KeyTrack` (鍵盤行) | `EditSurface::Notes` |
| `AudioLane` (波形行) | `EditSurface::AudioEvents` |
| `Track` / `Automation` | `EditSurface::TimeRange` |

面ごとのタイブレーク (旧 last-wins の 5 面) はこれで不要になり、`EditSurface` に
残るのは `TimeRange` / `Notes` / `AudioEvents` / `LauncherCells` / `Tracks` /
`Sections` / `Devices`。 矢印キー・Delete・Copy の振り分けは従来の面別 dispatch を
そのまま使える。

### 13.2 ノートの範囲は**そのクリップのトラック行も持つ**

`LaneRef::KeyTrack` だけだと、
(a) アレンジャー側でそのクリップが選択表示にならない、
(b) `shown_pianoroll_clips()` がトラック行から解決できない、
の 2 つが起きる。 ノート選択の範囲には `LaneRef::Track(clip.track_id)` も入れる。
面の判定は「`KeyTrack` があれば `Notes`」なので、Delete はノートに効く。

### 13.3 ノート選択の解除は**クリップの範囲へ落とす**

範囲を捨てるとピアノロールに出ていたクリップまで消え、空白クリック 1 回で
エディタが真っ白になる。 `set_note_selection(&[])` は範囲を捨てずに
「表示していたクリップの区間 × そのトラック行」へ落とす。

### 13.4 選択は編集の**前**に捕まえる

選択は範囲からの導出なので、ノートを動かした**後**に読むと「動く前の範囲」で
解決してしまい、移動・移調のたびに選択が外れる。 `edit_clip_notes` は編集前に
`selected_note_ids()` を捕まえ、remap してから `set_note_selection` する。

### 13.5 範囲化の適用範囲

アレンジャー / ピアノロール / オーディオエディタの選択はすべて範囲からの導出になった
(`selected_clip_refs` / `selected_note_ids` / `selected_audio_event_indices`)。
残るオブジェクト選択は**時間軸を持たない面**だけ — ランチャーのセル / トラック /
セクション / 列 / device。

**オートメーション追従**が効くのは「移動」「複製 (`D` / `Alt+D`)」「範囲 Delete」。
コピー / 貼り付けはまだ (クリップボードの payload に automation を積んでいない)。

**サイズ budget の分割**は行っていない (ユーザー判断)。 新規の超過は
`scripts/arch_lint_baseline.txt` に理由付きで記録した。

### 13.6 実機フィードバックで直したもの (2026-08-30)

- **範囲の内側ドラッグで素材を動かす**のは入れない。 Live と同じく、素材を動かせるのは
  **クリップのヘッダ**を掴んだときだけで、それ以外はどこでも範囲の引き直し。
- **ピアノロールのドラッグはグリッドにスナップした範囲**にする (旧・矩形選択の cyan 矩形は撤去)。
  修飾キーの UNION / XOR も廃止 — ドラッグは常に引き直しで、足すのは Ctrl・Shift+**クリック**
  (アレンジャーと同じ規約)。
- **アレンジャーで範囲を引いたら、ピアノロールはその「範囲」を映す** ([`fit_piano_roll_to_range`])。
  掛かったクリップ全体を映すと、範囲を引いた意図と表示が食い違う。

### 13.7 実機フィードバック 3 巡目 (2026-08-30)

**「範囲ではなくクリップ全体が移動してしまう」** — クリップヘッダのドラッグが
`SetClipPositions` (クリップ単位) のままだった。 **「クリップを動かす」操作を廃止**し、
[`AppData::move_time_range`] 1 本に統合した (矢印キーのナッジと同じ口)。

- press 時に「動かす範囲」を確定する (`ClipDragSession.move_range`)。 いまの範囲が
  掴んだクリップに掛かっていればその範囲、掛かっていなければそのクリップの占有区間。
- ドラッグ ghost の anchor は **範囲で切った断片**。 ゴーストの content 原点も
  断片の先頭ぶん進めるので、「見えていたもの」がそのまま確定する。
- Ctrl / Ctrl+Shift は [`AppData::copy_time_range`] (リンク / 独立コピー)。 元は
  1 拍も割らず、範囲からはみ出した部分は**窓を詰めて**コピーする。
- automation の追従は**同じトラックへ動かすときだけ**。 行き先が別トラックなら、
  そこのレーンは別のデバイス / パラメータなので対応付けようがない。
- 追従 (トラック行) と明示レーンで**同じレーンを二度ずらさない** (旧ナッジは
  トラック行とその automation レーン行を同時に選ぶと 2 倍動く bug を持っていた)。

**「ピアノロールで D がノートだけを複製する / 裏拍のパターンを複製できない」** —
送る量がノートの**外接 span** だった。 頭と尻に空白のある裏拍パターンでは 1 回ごとに
詰まってグリッドから外れる。 **範囲の長さ**で送るようにし、範囲そのものも同じだけ
後ろへ動かす (D 連打で後方連鎖)。 行き先が窓の外へ出るならクリップの窓を伸ばす
(伸ばせるのは隣のクリップの手前まで — 非重なり不変条件)。 面の判定も
「ノートが選ばれている」から **`time_selection_surface() == Notes`** へ変えた
(範囲に音が 1 つも無いときに「クリップ全体を複製」へ落ちない)。

### 13.8 実機フィードバック 4 巡目 (2026-08-30)

**「ピアノロールで undo するとピアノロールが閉じる」** — `after_undo_redo` が
`clear_note_selection()` / `set_audio_event_selection(&[])` を呼んでおり、範囲ごと
消えていた。 これは選択が **positional な note index** だった時代の名残 (「undo で
index がずれる」)。 いまの選択は範囲 1 本で、時間は song 絶対拍・レーンは安定 id
なので undo でずれようがない。 消えたクリップ / トラックを指す行は
`prune_selection_lanes()` が落とすので、その 1 本で足りる。

同じ根の呼び出しを全部たどって外した (`feedback_sibling_occurrence_check`) — 結果、
`clear_note_selection` の呼び出しは **`AppEvent::ClearNoteSelection` 1 本だけ**になった。
`select_new_clips(..)` / `set_single_clip_selection(..)` の直後の呼び出しは、範囲を
張り直した直後に「ノート範囲なら捨てる」を当てる無意味な口だった。 ノートの結合
(`join_selected_notes`) も同様。

**「D で複製した範囲に元から居たクリップが、次の D で一緒に複製される」** — アレンジャーの
`D` がまだクリップ集合ベース (`DuplicateClipsShared/Unique` → 外接 span) だった。
**範囲の複製 1 本** ([`AppData::copy_time_range`]) に統合した。

- 送る量は**範囲の長さ**。 行き先は上書き規則 (`place_clip`) で削られるので、複製後の
  範囲には**複製したものしか居ない** — 次の `D` が巻き込まない。
- ピアノロールの `D` も同じ規約にした。 複製が乗る区間に元から居たノートは
  (同じ鍵盤行なら) 消す。 クリップと同じ「行き先は上書き」。
- 撤去: `AppEvent::DuplicateClipsShared` / `DuplicateClipsUnique`、
  `duplicate_clips_shared` / `_unique`、`duplicate_one_clip_shared_at` / `_unique_at`、
  `clip_block_span`、`duplicate_track_automation_for`。 automation の複製は
  `copy_one_lane` 1 本 (移動側の `shift_one_lane` と対)。

### 13.9 レビューで潰したもの (2026-08-30)

- **`sel.track_ids()` は広すぎた。** オートメーションレーン行しか掛かっていない
  トラックまで含むので、そのトラックの**クリップ**まで動かして / 複製してしまう。
  クリップ側は [`TimeSelection::track_row_ids`] (= `LaneRef::Track` だけ) を使う。
  ドラッグの起動判定も `has_track` から `has_lane(Track(..))` へ。
- **レーン行だけの範囲でも automation は動く。** `move_time_range` / `copy_time_range`
  は `track_map` が空でも打ち切らない (旧ナッジがレーン行だけの範囲を扱えていた挙動を保つ)。
- **1 操作 = 1 undo step。** ピアノロールの `D` はクリップごとの `edit_song` + 窓伸ばしで
  何段も積むので `begin_gesture` / `end_gesture` で畳む。 アレンジャーの `D` も
  クリップと automation の 2 回ぶんを畳む。

### 13.10 実機フィードバック 5 巡目 (2026-08-30)

**「Z で、選択した MIDI クリップではないオートメーションクリップにズームされる」** —
オートメーションクリップの選択 (`selected_automation_clips`) だけが範囲と**別勘定**で
残っていた。 クリップを選び直しても automation 側の選択が生き残り、`edit_surface` が
`AutomationClips` を返して `Z` がそちらへ飛ぶ。 選択が範囲 1 本という前提が、この面だけ
成立していなかった。

- **automation クリップを選んだら範囲もそこへ張り直す**
  ([`AppData::select_automation_clip_range`])。 クリップ選択 (`apply_clip_range`) と同じ規約
  なので、「選んで見えているもの」と「Z / Delete / Copy が効くもの」が一致する。
- **範囲を張り直したら、範囲の外に残った automation クリップ / 点の選択は落とす**
  (`set_time_selection` → `prune_automation_selection`)。 点は幅ゼロで端に乗るのが普通
  なので `contains_beat` (両端を含む) で判定する — `intersects` だとクリップ先頭の点が
  毎回こぼれる。

### 13.11 「Z が別のクリップにズームする」 の真因 (2026-08-30)

推測で 2 度直して外した。 3 度目に `Z` の判断材料を 1 行ログに出して実データを取ったら、
**ズーム対象の解決は正しかった** — `automation=false` / `last_face=TimeRange` /
`span` = 選んだクリップの区間。 ズレていたのは横位置ではなく **行の高さ**だった。

真因: `zoom_arrange_horizontal` (1 段目 = 横ズーム) が
`ui_prefs.automation_lane_row_overrides` を **map ごと** clear していた。 あの map には
2 人の書き手が居る —

| 書き手 | 意図 | 捨ててよいか |
|---|---|---|
| `Z` の縦ズーム (2 段目) | 1 レーンを viewport 高いっぱいに**広げる** | 次の fresh な `Z` で捨てる |
| fit (`X` / 全体フィット) | 全行を viewport に収めるため**縮める** | 捨ててはいけない |

所有者を持たない共有 map だったので、1 段目が fit の行高まで巻き添えにし、model の
`height_px` へ戻る = 「1 回目の `Z` でオートメーションレーンだけ急に高くなる」。

- 所有者を `ui_ephemeral.zoom_lane_fill` (= `Z` が広げた 1 行) に持たせ、1 段目が捨てるのは
  **その 1 行だけ**にした。 fit と `X` は自分の分を自分で張り直すので後始末は要らない。
- 診断ログ (`Z: zoom to selection`) は残す。 画面から逆算できない類の切り分けは、
  この 1 行が無いと推測合戦になる。

**あわせて直したもの** (症状としては同じ「選んだものと違うものに効く」):

- `edit_surface` の優先順位が「ポインタが乗っている面」→「最後に選んだ面」だった。
  オートメーションレーン行にマウスが乗っているだけで、後から選び直したクリップを
  差し置いて automation 面が勝つ。 順序を逆にし、ポインタ位置は**タイブレーク**に
  降格した ([[feedback_selection_action_last_wins]] の規約そのもの)。
