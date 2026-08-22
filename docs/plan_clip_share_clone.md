<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Clip 共有コピー / 独立コピー 仕様

REAPER pooled MIDI 寄りの「共有コピー (linked clip)」 を導入し、 既存の「独立コピー」 と並行して
扱えるようにする。 共有 clip 群は notes を Song-level 別 store で 1 実体として持ち、
ピアノロール上での編集が同 source を持つ全 clip に即時反映される。

ステータス: **仕様策定中** (操作仕様確定、 実装着手前)。

## 1. 操作仕様

| 操作 | 動作 |
|---|---|
| drag | move (現状) |
| **Ctrl+drag** | **共有コピー** — 元 clip を残し、 drop 位置に source 共有のコピーを配置 |
| **Ctrl+Shift+drag** | **独立コピー** — 元 clip を残し、 drop 位置に notes を deep clone した独立コピーを配置 |
| Alt+drag | move + snap 一時無効 (現状維持) |
| **D** | 選択中 clip の末尾直後 (start+length) に共有コピーを生成 |
| **Alt+D** | 選択中 clip の末尾直後に独立コピーを生成 |

### 1.1 modifier 判定タイミング

drag **release 時** の modifier で確定する (REAPER / Ableton / Cubase / Bitwig 流)。 drag 中は
modifier 状態を毎フレーム監視し、 cursor 形状と ghost preview を切り替える:

- move: 既存の半透明 rect (現状)
- linked clone (Ctrl): rect + `⇌` 風 link アイコン overlay
- independent clone (Ctrl+Shift): rect + `+` アイコン overlay

### 1.2 D / Alt+D の詳細

- **位置**: 元 clip の末尾直後 (`start_beat + length_beats`)、 同サイズ (length_beats 同一)
- **連打**: 直前に生成したコピーが次の「元」 になり、 後ろにどんどん並ぶ (REAPER `Ctrl+D` / Ableton
  `Ctrl+D` 流)。 内部的には「コピー後は新 clip が選択状態」 でこれを実現
- **複数選択中**: 選択中の各 clip それぞれの末尾直後に並列生成 (全選択の最後尾ではなく)
- **生成後の選択**: 新しいコピー clip 群だけが選択状態になる
- **発火条件**: arrangement の focus 状況に関わらず発火、 ただし text_input フォーカス中は除外
  (既存 shortcut dispatcher の `focused_id.is_some()` 判定に乗る)

### 1.3 視覚区別

- **通常 clip** (`refcount == 1`): 既存の青系 clip 色のまま
- **共有 clip** (`refcount >= 2`):
  - 共有グループごとに**アクセント色**: caller は `content_id` ベースの hash で hue を `[0.0, 1.0)`
    に正規化して widget へ渡す (`ArrangementClip.share_group_color: Option<f32>`)。 widget 側が
    `ArrangementStyle.share_group_saturation` / `share_group_fill_lightness` /
    `share_group_border_lightness` / `share_group_alpha` (theme 単位で tunable) で HSL→RGB
    変換して描画
  - clip 名の**左に link アイコン**: widget 内蔵で描画 (`ArrangementStyle.share_group_link_glyph`、
    default `⇌` U+21CC)。 **selected/非 selected どちらでも常に描画** (selection 色に
    上書きされない、 識別マーカーなので消えてはいけない)
  - 同じ source を共有する clip 群は**同じアクセント色**、 別の共有グループとは色が違う
  - selected 状態の rect fill / border は `clip_selected_fill / _border` で上書き OK
    (selection の visibility 優先) — link glyph だけ独立して描画する

piano_roll 上では特別な区別はなし (編集中はどっちでも同じ操作)。

### 1.4 Make Unique (右クリックメニュー、 実装済み)

arrangement 上で clip を右クリック → **「Make Unique (独立化)」** で共有 clip → 独立 clip に変換:

- `refcount >= 2` の場合: source content を deep clone → 新 ContentId 採番 → `clip.content_id`
  を新 ID に書き換え。 当該 clip だけが独立化、 元の共有グループは残る (refcount が 1 減る)
- `refcount == 1` の場合: no-op (status_message で「すでに独立 clip」 と通知)

**UI wire**: gui_01 #020 (M14 Phase 63f、 commit `0d194d4`) で `ArrangementResponse.clip_rects:
Vec<(ClipKey, Rect)>` が追加されたので、 daw_01 側で `track_header_rects` と同パターンで
`context_menu_for(clip_rect, &["Make Unique"], ...)` を重ねている
([`daw_gui/src/view/arrangement_view.rs`](daw_gui/src/view/arrangement_view.rs))。 すべての
clip に同形の menu を出す (refcount==1 で項目を省くと UX が分かりにくい、 click したときに
status_message で通知)。

shortcut は割り当てない (多用する操作ではない、 必要になったら別途検討)。

### 1.5 共有化 UI (独立 clip → 共有) は実装しない

独立 clip 群の notes は別物なので、 共有化するには「片方の notes を捨てる」 confirmation modal
が必要 → 複雑な割に需要が薄い。 共有 clip が欲しい場合は最初から D / Ctrl+drag で作る運用。

## 2. データモデル

### 2.1 ClipContent 別 store

```rust
// common/src/model.rs

pub type ContentId = u32;

/// 共有可能な clip 内容。 notes は source 内 0 beat 起点で配置される。
/// length は持たない (各 clip 側の length_beats が表示範囲を決める)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ClipContent {
    pub notes: Vec<Note>,
}

pub struct Song {
    // 既存フィールド ...

    /// Content store. Clip.content_id が key として参照する。
    /// refcount=0 になった entry は autosave 時に GC される。
    #[serde(default)]
    pub clip_contents: HashMap<ContentId, ClipContent>,

    /// 次の ContentId 採番値。 ensure_ids で 0 sentinel を再採番するときも進める。
    #[serde(default)]
    pub next_content_id: ContentId,
}
```

### 2.2 Clip 変更

```rust
pub struct Clip {
    pub id: u32,
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,

    /// Song.clip_contents 内の content への参照。 0 は "未採番" sentinel。
    /// ensure_ids で再採番される。
    #[serde(default)]
    pub content_id: ContentId,

    // 旧 notes フィールドは削除 (migration で content store に移管、 §6 参照)。
}
```

### 2.3 自動独立化のセマンティクス

仕様確定 (Q4): refcount=1 になった共有 clip は **視覚的に通常 clip 表示に戻る**。
ただし**内部実装は常に別 store + ContentId 参照** (refcount に応じて inline ⇄ 別 store
を切替えると edge case が増えるため)。

つまり:
- データ表現は「Clip → ContentId → ClipContent」 の単一形で一貫
- アクセント色 / link アイコンの表示判定だけ `refcount >= 2` で切替
- ユーザー視点では「もう共有されていない clip = 通常表示」 で意図と一致

### 2.4 ContentId と Clip.id の独立性

- `Clip.id` は track 内 stable id (既存)、 `Track::ensure_clip_ids` で採番
- `ContentId` は **Song-global** な stable id、 `Song::ensure_clip_contents` で採番
- 異なる track の clip でも同じ ContentId を共有可能 (= track をまたいだ共有コピーが可能)

## 3. ライフサイクル

### 3.1 新規 clip 作成 (CreateClip / dbl-click)

- 新 ContentId を採番
- 空の `ClipContent { notes: vec![] }` を `clip_contents` に insert
- `clip.content_id` を採番した ID で初期化

### 3.2 D (共有コピー shortcut)

- 選択中 clip の `content_id` をそのまま新 clip にコピー
- 新 clip:
  - `start_beat = 元.start_beat + 元.length_beats`
  - `length_beats = 元.length_beats`
  - `id` は track の `next_clip_id` で採番
  - `content_id` は元と同一
- 選択を新 clip 1 個に置き換え

### 3.3 Alt+D (独立コピー shortcut)

- 選択中 clip の content (`clip_contents[元.content_id]`) を deep clone
- 新 ContentId を採番、 clone を `clip_contents` に insert
- 新 clip 作成、 `content_id` は採番した新 ID
- 位置は §3.2 と同様、 選択も同様

### 3.4 Ctrl+drag (共有コピー drop)

- drop 位置 (snap 済み) に新 clip 群を生成、 `content_id` は元 clip と同一
- 元 clip 群はそのまま、 移動しない
- 複数選択時は相対位置を維持してまとめて drop
- **short-click demote**: drag 距離 4px 未満で release した場合、 widget は CloneClipsLinked
  ではなく現状通り **selection toggle** に demote (Ctrl+click は selection toggle、 Ctrl+drag
  (>=4px) は clone — Ableton / Bitwig 流。 gui_01 reply 確定)
- **Alt との直交性**: Ctrl+Alt+drag は CloneClipsLinked + snap 一時無効 (Alt は引き続き snap
  modifier、 Ctrl 系と独立)

### 3.5 Ctrl+Shift+drag (独立コピー drop)

- §3.4 と同様、 ただし content は deep clone + 新 ContentId 採番
- **short-click demote**: 4px 未満は selection 範囲展開 (`Shift+click` の現状動作と一致、 違和感なし)
- **Alt との直交性**: Ctrl+Shift+Alt+drag は CloneClipsIndependent + snap 一時無効

### 3.6 Make Unique (右クリック)

- §1.4 参照

### 3.7 Delete clip

- clip を `track.clips` から remove
- 削除後の `clip.content_id` の refcount を計算、 0 なら `clip_contents` から remove
- ただし Undo 履歴 (Song snapshot) には残るので、 Undo で content ごと復元可能

### 3.8 Undo / Redo

- 既存の Song snapshot ベース Undo (Ctrl+Z) で自然に対応
- 共有 clip の note 編集 → Song snapshot に clip_contents の差分が乗る → Undo 1 step で復元
- D / Alt+D / Make Unique / Ctrl+drag / Ctrl+Shift+drag は `is_undoable` に登録、 1 操作 1 step

## 4. piano_roll 編集の伝播

- piano_roll は `AppData.selected_clip` (ClipRef) → `clip.content_id` → `clip_contents[id].notes`
  を取得して描画
- 既存の `AppEvent::AddNote / DeleteNotes / SetNoteVelocities / SetNoteLyric` は内部で
  `content_id` を targeting に変更 (clip_id ではなく)
- 同 content_id を持つ他の clip は arrangement で次フレーム再描画 (notes プレビューが更新)

## 5. VOICEVOX cache

**変更なし**。 現状 [common/src/voicevox_cache.rs:63](common/src/voicevox_cache.rs:63) の
`key_for_clip` は notes (sorted) + singer_id ベースの hash で、 既に clip_id を含まない設計。
共有 clip でも独立コピーでも、 内容が同じなら同 cache key で hit する。 むしろ独立コピーでも
1 回合成で済む点で最適。

将来 ContentId ベース (1 段目) + notes hash (2 段目 fallback) に拡張する余地はあるが、 M2 で
判断。

## 6. autosave / .daw 形式 (migration)

- `CURRENT_VERSION` **5 → 6** にバンプ (実装時に確認、 routing graph で 5 まで進んでいた)
- **旧 file 読み込み時の migration** (実装済み):
  - `Clip.notes: Vec<Note>` を「legacy deserialize-only」 として残す
    (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`)。 v6 形式では空に
    なるので serialize されない、 v5 形式では deserialize で受け取れる
  - `Song::ensure_clip_contents()`: 各 clip の `notes` が non-empty なら、
    新 `ContentId` を採番 → `clip_contents` に insert → `clip.notes` を `take()` で空に。
    `content_id == 0` (sentinel) の clip にも新 id を割り当てる
  - `project::load` のパス末尾で `ensure_clip_contents()` を呼ぶ
- **保存時** (実装済み): `CURRENT_VERSION = 6`、 `clip.notes` は空なので serialize されず、
  `clip_contents` map が Song トップレベルに乗る
- **GC** (実装済み): `project::save` 直前に `Song::gc_clip_contents()` で refcount=0 の
  content を `clip_contents` から remove (in-memory は keep、 disk file だけきれいに)

## 7. IPC 影響

- `MainToChild::LoadSong(Song)` は Song 全体を送るので、 `clip_contents` map もそのまま乗る
- 共有 clip の note 編集は `MainToChild::LoadSong` 再送 (現状の song-level 同期方式) で audio
  / plugin host へ伝播、 既存パスに変更なし
- bincode `Encode / Decode` を `ClipContent` に追加 (Song / Clip の derive 拡張)
- `daw_audio::vocal::VocalAudio` 等の audio パスは clip 単位の合成結果 cache を使うので、 変更なし
  (clip → content_id → notes lookup を engine 側で行う形に)

## 8. 実装ファイル変更点 (案)

### common/

- [common/src/model.rs](common/src/model.rs)
  - `ContentId` / `ClipContent` 型追加
  - `Clip` から `notes: Vec<Note>` 削除、 `content_id: ContentId` 追加、 legacy_notes 互換 field
  - `Song` に `clip_contents` / `next_content_id` 追加
  - `Song::ensure_clip_contents()` / `gc_clip_contents()` / `clip_content_refcount()` helper
  - `CURRENT_VERSION` 4 にバンプ + migration test

- [common/src/voicevox_cache.rs](common/src/voicevox_cache.rs) — **変更なし**

### daw_gui/

- [daw_gui/src/app.rs](daw_gui/src/app.rs)
  - `AppEvent::DuplicateClipShared { source: ClipRef }` 追加 (D shortcut → handler)
  - `AppEvent::DuplicateClipUnique { source: ClipRef }` 追加 (Alt+D shortcut → handler)
  - `AppEvent::CloneClipsLinked(Vec<(ClipRef, f64)>)` 追加 (Ctrl+drag drop)
  - `AppEvent::CloneClipsIndependent(Vec<(ClipRef, f64)>)` 追加 (Ctrl+Shift+drag drop)
  - `AppEvent::MakeClipUnique(ClipRef)` 追加 (右クリックメニュー)
  - 既存 note 編集 event の handler 内部で `content_id` 経由 lookup に切替
  - `is_undoable` にすべて登録

- [daw_gui/src/view/runner.rs](daw_gui/src/view/runner.rs) (or shortcuts.rs)
  - D / Alt+D の shortcut handler 追加 (text_input フォーカス中は除外)

- [daw_gui/src/view/arrangement_view.rs](daw_gui/src/view/arrangement_view.rs)
  - `ArrangementClip` を構築する際に `share_group_color: Option<f32>` を計算 (refcount>=2 の
    content_id を持つ clip だけ `Some(hue)`、 hue は `content_id` を `u64` hash → `[0.0, 1.0)`
    正規化)
  - `make_edit` で `ArrangementEditRequest::CloneClipsLinked(deltas) / CloneClipsIndependent(deltas)`
    を新 AppEvent に変換
  - `ArrangementStyle::default()` で start (gui_01 Phase 63e で 11 field 追加: clone ghost
    fill/border × 2、 badge size/color、 share_group S/L_fill/L_border/alpha、 link_glyph。
    default 値で開始、 必要に応じて theme tweak)
  - 右クリックメニューに「Make Unique」 を追加 (refcount>=2 の clip 上でのみ enable)

- [daw_gui/src/view/piano_roll_view.rs](daw_gui/src/view/piano_roll_view.rs)
  - notes lookup を `clip.notes` から `app.song.clip_contents[clip.content_id].notes` に変更
  - 編集 event は AppEvent (内部で content_id 経由) なので view 側のロジック変更最小

### daw_audio/

- vocal track 関連 (engine.rs / vocal.rs)
  - clip → notes 取得を `song.clip_contents[clip.content_id]` 経由に変更
  - cache key は §5 の通り変更なし

### daw_plugin_host/

- 変更なし (Song を受け取って Clip を読む箇所のみ、 notes lookup を content_id 経由に)

## 9. gui_01 #019 (要望) 文面

[docs/gui_01_conversation.md](gui_01_conversation.md) に新規エントリで投稿:

```
## #019 [Open] 2026-05-07 [要望] arrangement clip の共有コピー / drag-modifier-aware EditRequest

daw_01 で clip の共有コピー (linked clip) と独立コピー (unlinked clip) を実装する
([daw_01:docs/plan_clip_share_clone.md](daw_01:docs/plan_clip_share_clone.md))。 これに
伴い `Ui::arrangement` widget に以下を追加してほしい。

### 1. 新 EditRequest 2 種

既存の `MoveClips(deltas)` に加えて:
- `CloneClipsLinked(deltas)` — Ctrl+drag で release されたとき発行
- `CloneClipsIndependent(deltas)` — Ctrl+Shift+drag で release されたとき発行

deltas の型は MoveClips と同形 (`Vec<MoveDelta { from: ClipKey, next_start_beat: f64 }>`)。
daw_01 側で source 共有 / fork を分岐する。

### 2. drag 中の modifier-aware ghost

modifier 判定は drag **release 時**。 ただし drag 中は modifier 状態を毎フレーム監視して
ghost preview と cursor を切り替える:
- move: 既存の半透明 rect (現状)
- linked clone (Ctrl): rect + `⇌` 風 link アイコン overlay
- independent clone (Ctrl+Shift): rect + `+` アイコン overlay

snap との衝突回避のため Ctrl+Alt は使わない (Alt は現状の「snap 一時無効」 のまま)。

### 3. ArrangementClip に share_group_color フィールド追加

```rust
pub struct ArrangementClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    pub color: Option<Color>,
    pub share_group_color: Option<f32>,  // 新フィールド (HSL hue 0.0..1.0)
}
```

`Some(hue)` のとき、 widget は通常の clip 色を `hue` ベースのアクセント色に置き換え、
clip 名の左に小さな link アイコン (`⇌`) を描画する。 `None` のとき現状通り。
daw_01 が refcount>=2 の clip にだけ Some(hue) を設定する (hue は content_id ベースの
hash で一意化)。

### 4. 受け入れ基準

- arrangement で Ctrl+drag → release → CloneClipsLinked が daw_01 側で受け取れる
- 同じく Ctrl+Shift+drag → CloneClipsIndependent
- drag 中に Ctrl を押し続けた状態と Ctrl+Shift の状態で ghost overlay が変わる
- daw_01 が share_group_color = Some(0.5) を渡した clip だけ枠色とアイコンが変わる
```

## 10. 進捗

- [x] gui_01 #019 起こす
- [x] gui_01 reply 受領 (M14 Phase 63e、 commit `46ea71b` で main merge 済)
- [x] `common/src/model.rs` データモデル変更 (`ContentId` / `ClipContent` / `Song.clip_contents`
      / `Clip.content_id`) + helpers (`alloc_content_id` / `ensure_clip_contents` /
      `clip_content_refcount` / `gc_clip_contents` / `clip_notes` / `notes_in_clip_mut`)
- [x] `CURRENT_VERSION` 5 → 6 + v5 → v6 migration test (`load_v5_migrates_clip_notes_to_clip_contents`)
- [x] `common/src/voicevox.rs` / `voicevox_cache.rs` を `&[Note]` 引数 + content_id 経由
      lookup に refactor (cache key は notes hash で変更なし、 共有 / 独立で同 key hit)
- [x] `common/src/project.rs`: load 末尾で `ensure_clip_contents`、 save 前に `gc_clip_contents`
- [x] `daw_audio/src/sequencer.rs`: `for note in &clip.notes` → `clip_contents` 経由
- [x] `daw_gui/src/app.rs`: 14 箇所の `clip.notes` アクセスを helper 経由に書き換え
- [x] `daw_gui/src/app.rs`: 新 AppEvent 5 個 (`DuplicateClipShared / DuplicateClipUnique /
      CloneClipsLinked / CloneClipsIndependent / MakeClipUnique`) + handler 実装 +
      `is_undoable` 登録
- [x] `daw_gui/src/view/arrangement_view.rs`: `share_group_color` 計算 (refcount>=2 →
      golden-ratio hue) + 新 `EditRequest` arm 2 種 (CloneClipsLinked / CloneClipsIndependent)
- [x] `daw_gui/src/view/piano_roll_view.rs`: `build_widget_notes` を `Song.clip_notes(clip)`
      経由に
- [x] `daw_gui/src/view/shortcuts.rs` + `root.rs`: D / Alt+D shortcut wire (text_input
      フォーカス中は gui_01 が自動で除外)
- [x] `cargo build / clippy / test workspace clean` (model 系 96 test pass、 PDC integration
      test 1 件のみ unrelated な timeout — §11 で別途調査)
- [x] **arrangement 右クリック「Make Unique」** — gui_01 #020 (Phase 63f / commit `0d194d4`)
      で `ArrangementResponse.clip_rects` が追加され、 daw_01 側で `context_menu_for` を
      重ねて wire 完了 (§1.4)
- [ ] 実機 smoke test (Ctrl+drag / Ctrl+Shift+drag / D / Alt+D / 右クリック Make Unique の
      full path) — ユーザー目視確認待ち
- [ ] VOICEVOX 合成 smoke test (共有 / 独立コピー両方で cache hit するか実機確認)

## 11. 既知の課題

- **PDC integration test** (`pdc_real_mcenter_aligns_master_output`): `cargo test --workspace`
  で 60 秒 timeout で fail。 daw_audio / daw_plugin_host 側に `clip.notes` 直接アクセスは
  残っていないので、 本仕様変更による regression ではないと推測。 `daw_gui --script` 経由
  で MCenter VST3 を実 load + WAV export する重い test、 環境依存 (VST3 plugin 存在 / DLL
  load / pump_until timeout) が原因の可能性が高い。 別 phase で調査。
- ~~**arrangement clip 右クリックメニュー**: gui_01 #020 で `ArrangementResponse.clip_rects`
  追加を要望、 受領後に Make Unique を context menu として wire。~~ → 解決 (Phase 63f /
  commit `0d194d4` で `clip_rects` 取り込み済、 daw_01 側 wire 完了 §1.4)
- **Undo の reorder 順**: D 連打で大量の clip を生成 → Ctrl+Z で 1 step ずつ巻き戻る挙動は
  `is_undoable` で対応済だが、 1 連打 = 1 Undo step (= 連打 5 回で 5 step) になる。
  ユーザー期待によっては「連打 sequence を 1 step」 にまとめる仕様も検討余地あり (gui_01
  #018 の velocity drag の「1 batch = 1 Undo step」 と類似の議論)。
