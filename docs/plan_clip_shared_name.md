# Clip 名の共有 + 共有グループの識別/一括選択 仕様

`docs/plan_clip_share_clone.md` で導入した「共有コピー (linked clip)」の続き。
共有クリップ群で **名前も共有** し (片方を rename → 全部 rename)、 さらに
**どのクリップが同じ共有グループか** を一括選択 / 連動ハイライトで判別できるようにする。

ステータス: **実装着手** (2026-06-01)。

## 0. 背景 (なぜやるか)

現状 `content_id` で notes は 1 実体共有されるが、 `Clip.name` は per-clip。
共有コピーを作ると生成時に名前は複製されるが、 以後 rename しても他に伝播しない
→ 「同じ素材なのに名前がバラバラ」 になる。 また共有グループの視覚区別は
`refcount>=2` のアクセント色 (`content_id` hue) + link glyph `⇌` だけで、
「どれとどれが同じか」 を能動的に確認する手段 (一括選択 / 連動強調) が無い。

ユーザー要望 (2026-06-01):
1. 共有コピーしたクリップは **名前も共有** にしたい (rename 連動)。
2. 共有クリップを **一括選択するなどで、 どれが同じ共有か分かる** ようにしたい。
   → 採用 UX: **一括選択 (daw_01 即実装) + 連動ハイライト (gui_01 要望)**。
   グループ番号ラベルは今回スコープ外。

## 1. 名前の共有 (SSoT)

### 1.1 データモデル

`Clip.name` は `content_id` 単位で共有されるべき値 → notes と同じく content store に
正規化する。 `clip_contents` は payload enum を保つ設計なので、 名前は **兄弟 map** に
置く (`audio_sources` 等と同 idiom、 同 `ContentId` キー)。

```rust
// common/src/model.rs  Song
/// 共有クリップ名。 `Clip.content_id` をキーに、 同 content を参照する全 clip が
/// 共有する表示名。 `clip_contents` と lifecycle を共にする (GC / migration 連動)。
#[serde(default)]
pub clip_content_names: HashMap<ContentId, String>,
```

`Clip.name` / `AutomationClip.name` は **legacy deserialize-only** に降格 (= `Clip.notes`
を v5→v6 で content store に移管したのと同一 idiom):

```rust
/// **Legacy field**: v19 までは per-clip 名の owner。 v20+ は
/// `Song.clip_content_names[content_id]` が SSoT。 load 時に
/// `ensure_clip_contents` が map へ drain して空にする。 空なら serialize されない。
#[serde(default, skip_serializing_if = "String::is_empty")]
pub name: String,
```

### 1.2 helper (lifecycle を原子化)

名前と content の desync を防ぐため、 採番/複製は helper に集約する:

- `Song::content_name(&self, id: ContentId) -> &str` — map lookup、 未登録は `""`。
- `Song::set_content_name(&mut self, id: ContentId, name: String)` — rename の書き込み口。
- `Song::alloc_content(&mut self, content: ClipContent, name: String) -> ContentId`
  — 新規生成: id 採番 + `clip_contents` insert + `clip_content_names` insert を 1 回で。
- `Song::fork_content(&mut self, src: ContentId) -> ContentId` — 独立コピー:
  content を deep clone + 名前を複製して新 id を返す。

既存の `alloc_content_id()` は内部用に残す (helper が利用)。

### 1.3 migration (v19 → v20)

- `CURRENT_VERSION` **19 → 20** にバンプ。
- `Song::ensure_clip_contents()` 末尾を拡張: content_id 確定後、 各 clip の legacy
  `name` が非空なら `clip_content_names.entry(cid).or_insert(name)` で backfill し
  (同 content を複数 clip が共有する v19 file では **最初に見た非空名** を採用)、
  `clip.name` を `String::take()` で空に。 automation clip も同様。
- `Song::gc_clip_contents()`: `clip_contents` の retain と同じ live 集合で
  `clip_content_names.retain(|id,_| live.contains(id))` も実施。

### 1.4 書き換え箇所

| 箇所 | 変更 |
|---|---|
| `app.rs::commit_rename_clip` (6759) | `clip.name = new_name` → `self.song.set_content_name(content_id, new_name)` |
| `app.rs::begin_rename_clip` (6725) | `clip.name.clone()` → `self.song.content_name(content_id).to_string()` |
| `arrangement_view.rs` clip 表示 (162) | `c.name` fallback → `app.song.content_name(c.content_id)` (text_clip_label override は不変) |
| `arrangement_view.rs` automation 表示 (1644) | `c.name` → `song.content_name(c.content_id)` |
| 新規生成サイト (create_clip / import audio·video·image·text / split / bounce 等 約25箇所) | `alloc_content_id()`+`clip_contents.insert()` → `alloc_content(content, name)`、 Clip は `name: String::new()` |
| 独立コピー (`duplicate_clip_unique` / `clone_clips_independent` / `make_clip_unique` / automation 版) | content clone+alloc を `fork_content(src)` に、 Clip は `name: String::new()` |
| 共有コピー (`duplicate_clip_shared` / `clone_clips_linked` / automation 版) | 名前複製を削除、 Clip は `name: String::new()` (content_id 経由で自動共有) |

### 1.5 セマンティクス

- 共有コピー: 同 `content_id` → 名前は自動共有。 片方 rename → 全部即連動。
- 独立コピー / Make Unique: 新 `content_id` → fork 時点の名前を複製、 以後独立。
- refcount==1 の通常 clip も `content_id` を持つので同一形で扱う (特例なし)。

## 2. 共有グループの一括選択 (daw_01 のみ)

- `AppEvent::SelectLinkedClips(ClipRef)` 追加 + `is_undoable` 対象外 (選択は履歴に乗せない、
  既存 SelectClip と同様)。
- `app.rs::select_linked_clips(target)`: target の `content_id` を取り、 全 track の
  main clips を走査して同 `content_id` の `ClipRef` を集め `set_clip_selection(linked)`。
  (automation clip は別 selection set + 別 content_id 空間なので main clips のみ。
  refcount==1 でも自身 1 個の選択 = 無害、 ガード不要。)
- 右クリックメニュー (`arrangement_view.rs:437`): "Make Unique" の直後に
  **「共有を一括選択」** を挿入、 以降の idx を +1。
- shortcut: `shortcuts.rs` + `root.rs` に 1 つ (例 `Shift+L`)、 rename focus 中は除外
  (`app.clip_rename.is_none()` ガード)。

## 3. 連動ハイライト (gui_01 要望 → landing 後に daw_01 wire)

選択 or hover 中の共有クリップと同グループの他クリップを自動強調する。
**gui_01 widget 拡張が必要** なので、 interim を作らず要望を先に提出
(`docs/gui_01_conversation.md` に新エントリ、 §4 参照)。

### 3.1 gui_01 へ要望する最終形

- `ArrangementClip` に per-clip flag 追加 (例 `in_active_group: bool`)。
- `ArrangementStyle` に強調描画の tunable 追加 (share-group hue ベースの ring / glow:
  `share_group_active_border_lightness` / `share_group_active_border_w` / `_glow_alpha` 等)。
- `in_active_group == true` の clip は、 選択 (黄塗り) とは別レイヤで hue リング/グローを描画。
  選択中 clip は従来通り黄塗り優先 (グループ識別は非選択メンバーの強調で担保)。

### 3.2 daw_01 側ロジック (landing 後)

- 毎フレーム `active_groups: HashSet<ContentId>` を計算
  = `selected_clips` の content_id ∪ 前フレーム `ArrangementResponse.hovered_clip` の content_id、
  ただし `refcount>=2` のみ。
- 各 `ArrangementClip` 構築時 `in_active_group = refcount>=2 && active_groups.contains(content_id)`。

## 4. gui_01 要望文面 (投稿予定)

`docs/gui_01_conversation.md` に `## #0xx [Open] 2026-06-01 [要望]` で起こす。
`関連仕様: docs/plan_clip_shared_name.md §3` を必須記載。 最終形 (v1/v2 分割なし) で記述。

## 5. IPC / audio

- `clip_content_names` は `Song` トップレベル field → `MainToChild::LoadSong` でそのまま伝播、
  bincode `Encode/Decode` は `HashMap<u32,String>` で自動。 audio engine は名前を読まない。

## 6. 進捗

- [x] `common/src/model.rs`: `clip_content_names` field + helpers (`content_name` /
      `set_content_name` / `alloc_content` / `fork_content`) + `gc` / `ensure` 拡張
- [x] `CURRENT_VERSION` 19 → 20 + v19→v20 migration test
      (`v19_clip_names_drain_into_shared_map_and_rename_is_group_wide`) + vocal roundtrip test 修正
- [x] `daw_gui/src/app.rs`: rename 経路 (`commit/begin_rename_clip`) +
      生成 (`create_clip` / 録音 / import audio·video·image·text / split / glue / bounce×2) +
      コピー (`duplicate_clip_shared/unique` / `clone_clips_linked/independent` / `make_clip_unique`)
      を `content_name` / `alloc_content` / `fork_content` / `set_content_name` 経由に
- [x] `daw_gui/src/view/arrangement_view.rs` + `audio_editor.rs`: 表示を `content_name` 経由に
- [x] `SelectLinkedClips` event + handler (`select_linked_clips`) + 右クリック「共有を一括選択」
      + shortcut `Shift+L` (shortcuts.rs / root.rs、 rename focus 中除外)
- [x] gui_01 #068 [要望] 連動ハイライト 投稿 (`docs/gui_01_conversation.md`)
- [x] `cargo build --workspace` / `clippy --workspace -D warnings` / `test --workspace --lib` clean
- [x] gui_01 #068 landing (Phase 96) → daw_01 wire: `ArrangementClip.in_active_group` +
      `AppData.arrange_hover_content` (前フレーム hover 保持) + active group 計算
      (選択 ∪ hover content_id、 refcount>=2)。 `cargo build` / `clippy` clean
- [ ] 実機 smoke (共有 rename 連動 / 一括選択 / 連動ハイライト / 独立コピー後の名前独立) — ユーザー目視待ち
