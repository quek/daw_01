# plan_select_all — Ctrl+A コンテキスト全選択 + 選択 SSoT の安定 ID 化

「アレンジメントで Ctrl+A で全クリップ選択」。grill-me（2026-06-09）で文脈対応・解除・
段階拡大・選択 SSoT の安定 ID 化まで詰めた。

## 現状 (2026-06-09)

- 選択 SSoT は **index ベース**。`selected_clip: Option<ClipRef>`（anchor=末尾）/
  `selected_clips: Vec<ClipRef>`（複数選択）
  ([app.rs:904-905](F:/dev/daw_01/daw_gui/src/app.rs))。
  `ClipRef { track: u32, clip: u32 }` は **Vec のインデックス**
  ([app.rs:392-396](F:/dev/daw_01/daw_gui/src/app.rs))。並べ替え / Undo で別クリップを指す。
- 既に `reorder_tracks` は選択を一旦 id ペアに変換→並べ替え後 index 逆引き、という**手動
  ラウンドトリップ**で延命 ([app.rs:6779-6831](F:/dev/daw_01/daw_gui/src/app.rs))。
  `swap_tracks` は `selected_clip` のみ再マップし **`selected_clips` を取りこぼすバグ**
  ([app.rs:6742-6752](F:/dev/daw_01/daw_gui/src/app.rs))。`after_undo_redo` は範囲外 index を
  filter で落とす ([app.rs:2194-2218](F:/dev/daw_01/daw_gui/src/app.rs))。
- ウィジェット境界は既に**安定 ID**。gui_01 の `ClipKey { track: u32, clip: u32 }`
  （= track_id, clip_id, serde/bincode 無し、[gui_01 arrangement.rs:239-245](F:/dev/gui_01/crates/ui/src/widgets/arrangement.rs)）で
  やり取りし、`clip_key_to_ref` で index に解決
  ([arrangement_view.rs:2187-2191](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs))。
- `Ctrl+A` は **どこにも未割り当て**。gui_01 default binding に `select_all=Ctrl+A` はあるが
  ([gui_01 shortcut.rs:226](F:/dev/gui_01/crates/ui/src/shortcut.rs))、daw_01 の
  `dispatch_shortcuts` ([root.rs:265-656](F:/dev/daw_01/daw_gui/src/view/root.rs)) は
  `take_shortcut("select_all")` を一切呼んでいない。
- `ClearSelection`（[app.rs:3329](F:/dev/daw_01/daw_gui/src/app.rs) 定義 /
  [app.rs:4598-4600](F:/dev/daw_01/daw_gui/src/app.rs) 処理）は**定義・処理あるが未発火の死蔵**。
  Escape は rename 取消→audio editor 閉じ→help 閉じのみ
  ([root.rs:637-655](F:/dev/daw_01/daw_gui/src/view/root.rs))、選択解除に未割り当て。
- 文脈判定: 既存 D キー / グリッド系は `is_pianoroll_active = bottom_panel==1 && pointer_in_bottom`
  ＝**マウス位置**で分岐 ([root.rs:451-455](F:/dev/daw_01/daw_gui/src/view/root.rs))。
  一方 Delete は**選択セットの非空**で分岐（audio event > automation point > note >
  automation clip > clip、[root.rs:407-443](F:/dev/daw_01/daw_gui/src/view/root.rs)）。

## 確定仕様 (grill-me 2026-06-09) — 見える挙動

Ctrl+A =「マウスが乗っている編集面の中身を全部選ぶ」。選択前なので Delete の「非空セット
判定」は使えず、**マウス位置で対象を判定**する。

| 文脈（ポインタ位置） | 1 回目 | 2 回目以降 |
|---|---|---|
| クリップ領域（アレンジメント） | 曲全体・全トラックの全クリップ | 冪等（何もしない） |
| オートメーションレーン（アレンジ内インライン） | そのレーンの全ポイント（レーン内全クリップの全点） | **段階拡大**: 曲全体の全クリップ → 以降冪等 |
| ピアノロール（下部パネル） | 編集中クリップの全ノート | 冪等 |
| オーディオエディタ（下部パネル） | 編集中クリップの全イベント | 冪等 |
| 文字入力フォーカス中 | その文字を全選択（widget が処理、ショートカットは奪わない） | — |

- **解除**: Escape を新設（死蔵 `ClearSelection` を生かす）＋ 既存の「何もないところを
  クリック」も維持。
- **表示は飛ばさない**: 全選択時に `fit_piano_roll_to_clip`（末尾クリップへズーム /
  [app.rs:9971-10007](F:/dev/daw_01/daw_gui/src/app.rs)）と `select_track`（カーソルトラック
  移動 / [app.rs:6845-6853](F:/dev/daw_01/daw_gui/src/app.rs)）を**抑止**。anchor=末尾は
  inspector 表示用に維持。
- 段階拡大は**オートメーションレーンのみ**（アレンジ内インラインで「点⊂レーン⊂アレンジ」の
  入れ子が自然）。下部パネル（ノート/イベント）は冪等のまま。

## 内部設計（ユーザー確認不要 / SSoT・DRY）

### 1. 選択 SSoT を安定 ID 化 — `common::ClipKey` 新設

`ClipRef`（index）を選択経路から排除し、**安定 ID** で保持する。

- **`common::ClipKey { track: u32, clip: u32 }`（= track_id, clip_id）を新設**。derive は
  既存の `AutomationClipKey`（[common/model.rs:2979-2987](F:/dev/daw_01/common/src/model.rs)）に
  揃え `Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode`。
  - gui_01 にも同名 `ClipKey` はあるが serde/bincode 無しの**汎用 widget 型**。これを daw_01 の
    ドメイン SSoT に流用すると serialize 可否に縛られ、gui_01 に serde 依存を強いる。common 側に
    持つのが既存流儀（gui_01::AutomationLaneKey と common::AutomationLaneKey が**別々に**存在し
    境界変換している）と一致し、serde/bincode 安全。
- `selected_clip: Option<ClipKey>` / `selected_clips: Vec<ClipKey>` に置換。
- 配列アクセスは利用時に解決。既存ヘルパを使う:
  `track_index_by_id` ([common/model.rs:719](F:/dev/daw_01/common/src/model.rs)) /
  `clip_index_by_id` ([:1617](F:/dev/daw_01/common/src/model.rs)) /
  `clip_by_id` ([:1621](F:/dev/daw_01/common/src/model.rs)) / `clip_by_id_mut`。
  AppData に薄いヘルパ `clip_at(ClipKey)->Option<&Clip>` / `clip_at_mut` /
  `clip_indices(ClipKey)->Option<(usize,usize)>` を足し、各 consumer の inline index 取得を畳む。
- **得られる副産物（要件外の挙動は変えない範囲で自然消滅するもの）**:
  - `reorder_tracks` の手動 id ラウンドトリップが不要に（選択が自動で並べ替え追従）。
  - `swap_tracks` の `selected_clips` 取りこぼしバグが消える（再マップ自体が不要）。
  - `after_undo_redo` の範囲外 index フィルタが「`clip_by_id` で解決できなければ落とす」に簡素化。
- 境界変換 `clip_key_to_ref` は **index 解決 → `common::ClipKey`⇔`gui_01::ClipKey` の field
  コピー**に変わる（[arrangement_view.rs:350-360, 2187-2191](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs)
  の双方向）。`SelectClips` 受信
  ([arrangement_view.rs:1100-1106](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs)) は
  index 逆引きが消える。
- **スコープはクリップのみ**。automation point の不安定性
  （`AutomationPointKey.point_idx` は frame 内のみ有効、[common/model.rs:3000-3010](F:/dev/daw_01/common/src/model.rs)；
  daw_gui の `AutomationPointKeyRef` も同様 [app.rs:413-418](F:/dev/daw_01/daw_gui/src/app.rs)）は
  別課題として本 plan では触れない。

### 2. Ctrl+A の振り分けと段階拡大

`dispatch_shortcuts` で `take_shortcut("select_all")` を取り、ポインタ位置で分岐:

1. `pointer_in_bottom`（[root.rs:451-455](F:/dev/daw_01/daw_gui/src/view/root.rs)）:
   - `audio_editor_clip.is_some()` → 全イベント選択（**依存 B**）。
   - else（piano roll, `bottom_panel==1`） → 編集中クリップの全ノート選択
     （`selected_notes = 全 note id`、`SetNoteSelection` 経由 [app.rs:4642](F:/dev/daw_01/daw_gui/src/app.rs)）。
2. ポインタがアレンジ領域:
   - `resp.hovered_automation_lane`（**依存 A**）が `Some(lane)` → 段階拡大:
     - そのレーンの全ポイントが**まだ全選択でない** → レーン内全ポイントを選択。
     - 既に全選択済み → 曲全体の全クリップを選択。
   - `None`（クリップ領域） → 全クリップ選択。
- **段階判定**: Ctrl+A 時に対象セットを算出し、現在の選択が既に対象と一致していれば次段階へ。
- 全クリップ選択は新 `AppEvent::SelectAllClips` を足し、`set_clip_selection` を**副作用抑止版**
  で呼ぶ（下記 3）。

### 3. set_clip_selection の副作用抑止

現 `set_clip_selection` ([app.rs:9909-9921](F:/dev/daw_01/daw_gui/src/app.rs)) は
`fit_piano_roll_to_clip` と `select_track` を必ず呼ぶ。全選択では**呼ばない**。
引数で挙動を選べるようにする（例 `set_clip_selection_with(targets, focus: bool)`、
通常クリックは `focus=true`、全選択は `focus=false`）。`selected_notes.clear()` /
`step_cursor_beat=0` は全選択でも妥当なので維持。anchor=末尾も維持。

### 4. Escape で解除（既存挙動を壊さない順序）

Escape cascade ([root.rs:637-655](F:/dev/daw_01/daw_gui/src/view/root.rs)) に挿入:
rename track 取消 → rename clip 取消 → **audio editor 閉じ（既存・先に）** →
**選択解除（新規: 非空なら clips/notes/automation points/automation clips を clear）** →
help 閉じ。audio editor の Escape=閉じは不変。クリップ/ノート文脈では従来 help 閉じに落ちて
いただけなので解除を割り込ませても既存挙動を壊さない。死蔵 `ClearSelection` /
`ClearNoteSelection`（[app.rs:4598-4623](F:/dev/daw_01/daw_gui/src/app.rs)）を活用。

## 依存（grill-me で「揃うまで待つ」を選択 → 先に揃える）

### A. gui_01: `hovered_automation_lane` を ArrangementResponse に公開（要望 #090）

オートメーション文脈の振り分けには「ポインタが今どの automation lane の上か」が必要。
`ArrangementResponse` は `hovered_track` / `hovered_clip` / `hovered_zone` を公開するが
([gui_01 arrangement.rs:839-887](F:/dev/gui_01/crates/ui/src/widgets/arrangement.rs))
`hovered_automation_lane` が無い。`automation_lane_key_at_y()` は `pub fn` だが
`tops`（毎フレーム算出 y）/ `style` / レイアウト寸法という widget 内部データを要し
([gui_01 arrangement.rs:4097-4132](F:/dev/gui_01/crates/ui/src/widgets/arrangement.rs))、
daw_01 からは供給不可（再現は SSoT 違反）。→ widget が既存関数で埋めて応答に積む
`hovered_automation_lane: Option<AutomationLaneKey>` を `docs/gui_01_conversation.md #090` で要望。

### B. オーディオエディタのマルチイベント選択

`audio_editor_selected_event: Option<usize>` は**単一選択のみ**
([app.rs:936](F:/dev/daw_01/daw_gui/src/app.rs)、描画 [audio_editor.rs:450](F:/dev/daw_01/daw_gui/src/view/audio_editor.rs) で
`idx == selected_idx`）。「全イベント選択」には先にマルチ選択を新設する必要がある:
- state を `Vec<usize>` / `HashSet<usize>` 化（[app.rs:936](F:/dev/daw_01/daw_gui/src/app.rs)）。
- 描画 `contains(&idx)` 化（[audio_editor.rs:450,476-504](F:/dev/daw_01/daw_gui/src/view/audio_editor.rs)）。
- hit-test に Ctrl/Shift クリック（[audio_editor.rs:557,583,610,634,646](F:/dev/daw_01/daw_gui/src/view/audio_editor.rs)）。
- delete / next-prev nav / `SelectAudioEditorEvent`（[root.rs:418-432,620-628](F:/dev/daw_01/daw_gui/src/view/root.rs)、
  [app.rs:5091](F:/dev/daw_01/daw_gui/src/app.rs)）を複数対応。
- ※ イベント index も追加削除で renumber する不安定 ID。マルチ選択化と同時に安定化方針を検討。

## 実装順

1. **要望 #090 提出**（本 plan 参照）。gui_01 landing 待ち。
2. **依存 B**（audio editor マルチ選択）を daw_01 側で構築。
3. **依存 A landing 後**、Ctrl+A を一括実装: `common::ClipKey` 化（1）→ 振り分け+段階拡大（2）
   → 副作用抑止（3）→ Escape 解除（4）。安定 ID 化を最初に入れてから上物を載せる。
4. 検証（下記受け入れ基準）→ commit → release build green 確認。

## 受け入れ基準

- クリップ領域で Ctrl+A → 全トラックの全クリップが選択。2 回目は不変。
- オートメーションレーン上で Ctrl+A → そのレーンの全ポイント。続けて Ctrl+A → 全クリップ。
- ピアノロール上で Ctrl+A → 編集中クリップの全ノート。オーディオエディタ上で Ctrl+A →
  全イベント。各 2 回目は不変。
- 文字入力中の Ctrl+A は文字全選択（ショートホールが奪われない）。
- Escape / 空欄クリックで選択解除。Escape の rename 取消・audio editor 閉じ・help 閉じは不変。
- 全選択でピアノロールのズーム / スクロール / カーソルトラックが**動かない**。
- 全クリップ選択後にトラック並べ替え / Undo しても選択が**ずれない**（安定 ID 化の確認）。
- `cargo test --workspace` 全 pass、`cargo clippy --workspace -- -D warnings` clean、release build green。

## 非範囲

- automation point / audio event の **ID 安定化**（不安定 index のまま。別 plan）。
- ノート / イベントの**段階拡大**（下部パネルは冪等）。
- 選択した「トラック自体」の全選択（本 plan はクリップ/ノート/イベント/点のみ）。
- 範囲限定の全選択（可視範囲のみ等）。曲全体固定。

## 実装状況 (2026-06-09 完了)

全フェーズ実装・green（`cargo build --workspace` / `cargo clippy -p daw_gui -- -D warnings` /
`cargo test -p daw_gui`）。各フェーズ敵対的レビュー済（依存B: 2 件修正 / Ctrl+A: 0 件 / 安定ID化: 1 件修正）。
**未 commit・実機の最終確認は未実施**。

- ✅ 依存A: gui_01 #090 `hovered_automation_lane` landing → consume 済（[Resolved]）。
- ✅ 依存B: audio editor マルチイベント選択（`audio_editor_selected_events: Vec<usize>`、矩形 lasso /
  Shift+click トグル / 全ハイライト / 一括 delete / `audio_event_target_indices` 経由の一括編集 / anchor 表示）。
  multi-move は今回スコープ外。
- ✅ Ctrl+A 本体: クリップ領域=全クリップ（冪等・view 非ジャンプ）/ ピアノロール=全ノート /
  オーディオエディタ=全イベント / オートメーションレーン=全ポイント→2 回目で全クリップ（段階拡大）/
  Escape 解除（rename・audio editor 閉じ・help は非破壊）。
- ✅ 選択 SSoT 安定 ID 化: **`common::ClipKey { track_id, clip_id }`**（plan 当初案の `{track, clip}` から
  改名 — index ベース `ClipRef { track, clip }` との取り違えを compile error 化する安全策）。
  `selected_clip`/`selected_clips` を `ClipKey` 保持にし、resolver（`clip_ref_of` / `clip_key_of` /
  `selected_clip_ref` / `selected_clip_refs` / `clip_at`）で index 解決。書き込み interface は ClipRef 受けの
  まま内部変換。**副産物**: swap_tracks の selected_clips 取りこぼしバグ解消、reorder_tracks の手動 id
  ラウンドトリップ撤去、after_undo_redo / delete_track 系を `clip_at` ベースに簡素化。

### 残課題
- 実機検証（Ctrl+A 全文脈 / 並べ替え・undo で選択がずれないこと / audio マルチ選択）。
- multi-move（audio event の複数同時移動）、automation point / audio event の ID 安定化（別 plan）。
- `clip_ref_of` は O(tracks) 線形探索（id→index）。songs <200 track 想定で許容、必要なら track_id→index
  キャッシュ化（レビュー nit）。
