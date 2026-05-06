# グループトラック仕様

## Context

PR2 で実装するグループトラック (Reaper folder / Ableton Live Group 互換) の詳細仕様。
[`plan_routing_graph.md`](plan_routing_graph.md) の PR2 マイルストーンを実装するための、UI / RT / model の挙動を一義的に定義する。

ベース: [Ableton Live 12 Reference Manual — Mixing](https://www.ableton.com/en/manual/mixing/) + 本 DAW の制約 (gui_01 immediate-mode、3 プロセス分離)。

---

## 1. データモデル — kind を分けない

### 採用する型
```rust
pub struct Track {
    // 既存フィールド維持: id, name, instrument, midi_fx_chain, fx_chain,
    //                    volume, pan, muted, solo, source, clips, next_clip_id
    pub parent_group_id: Option<u32>,
    pub reported_latency_samples: u32,
}
```

**`TrackKind` enum は廃止。** 「子トラックを 1 つ以上持つ」 = group track という運用。

### 「group か否か」の判定
- 任意の track `t` が **group** ⇔ 他のいずれかの track の `parent_group_id == Some(t.id)`
- 子の有無で動的に決まる。最後の子を取り除いた瞬間に、その track は普通の audio track に戻る
- グラフコンパイラ (`compile_schedule`) も子の存在で判定する

### この設計の根拠
- Single Source of Truth: 「親子関係」だけが routing の真実、`kind` は冗長な複製
- 状態整合性のリスク回避: `kind = Group` なのに子がいない、`kind = Audio` なのに子がいる、といった不整合を表現できない
- group / audio の区別は派生情報 — `app.is_group(track_id)` のような method として毎フレーム計算

### group track が持つ意味
group track は **通常 track と完全に同じ機能を持つ** (Reaper のフォルダトラック相当)。group になっても `clips` / `instrument` / `midi_fx_chain` / `source` / `fx_chain` / `volume` / `pan` / `muted` / `solo` の挙動は普通と変わらない。group 化は親子関係を作るだけで、track 自身の信号処理は何も削らない。

RT 処理順 (compile_schedule で生成される NodeOp の論理的順序):
1. group 自身の `clips` → `midi_fx_chain` → `instrument` を実行し、自身の audio output を scratch に書く (= `ProcessTrackPreFx`)
2. 子の post-fader output を scratch に **加算**
3. group の `fx_chain` (audio FX) を scratch に適用
4. group の strip (volume / pan / mute / solo) を適用 + peak meter 更新
5. 親 (master or 上位 group) の Mix に参加

通常 track (子を持たない) は ②をスキップ — それ以外は同じ。よって「group」は信号処理側では「②の加算ステップが間に挟まる track」というだけ。

### parent_group_id
- `None` → master 直下
- `Some(id)` → 親の `Track::id`
- 親は track として存在する必要がある (`compile_schedule` が `DanglingReference(id)`)
- サイクル禁止 (`compile_schedule` が `Cycle`)

### ネスト深さ
- **無制限**

---

## 2. 多重選択 — 最初から実装

### `AppData::selected_track_ids: Vec<u32>`
- 既存の `selected_track: u32` は **削除**。後方互換は取らない (CLAUDE.md「大胆に破壊して作り直す」)
- 「カーソル位置の単一 track」が必要な箇所は `selected_track_ids.last()` を直接読む
- 並びは選択操作の順序を保つ (末尾 = 最後にクリックされたもの = カーソル相当)
- 0 要素もあり得る (track 全削除直後など)。空のときは select 系コマンドは no-op

### track header クリックハンドラ
| 修飾子 | 動作 |
|---|---|
| なし | 単独選択。`selected_track_ids = vec![clicked_id]` |
| Shift | 範囲選択。最後の anchor と clicked_id の間の連続 track 全部を選択 |
| Ctrl | toggle。clicked_id が含まれていれば外す、無ければ追加 |

範囲選択の anchor は **直前のなし-クリック** で確定したもの。Shift 連打の途中で変わらない (Live 互換)。

### 既存コードの追従
- `selected_track: u32` を参照している全箇所を grep し、用途別に置換:
  - 「現在のカーソル」 → `selected_track_ids.last().copied()` (Option)
  - 「ある track が選択中か」 → `selected_track_ids.contains(&id)`
- 既存 `AppEvent::SelectTrack(u32)` は `SelectTrack { track_id: u32, modifier: SelectModifier }` (`Single` / `RangeFromAnchor` / `Toggle`) に再設計
- view ハイライト (mixer / arrangement) は `selected_track_ids.contains(&id)` で判定

---

## 3. ショートカットキー

| キー | アクション |
|---|---|
| `Ctrl+G` | グループ化 (`daw.group_tracks`) |
| `Alt+G` | グループ化解除 (`daw.ungroup_tracks`) |
| `G` | 既存どおり grid snap toggle (`daw.toggle_snap`) |

`Shift+G` は使わない (取り消し)。

---

## 4. グループ化 (Ctrl+G)

### 発火条件
- `selected_track_ids` が 1 つ以上
- 0 なら no-op + log

### 挿入位置
**現状の実装は「`song.tracks` の末尾に append」 — 仕様乖離**。

理想は「選択トラックのうち、index が最も小さいものの直前」 (= 一番上の選択 track の上、Live 互換) だが、daw_plugin_host 側の `Tracks::chains: HashMap<track_idx, TrackChain>` が **track index ベース** で、daw_gui 側で `Vec::insert(idx, group)` するとプラグインホストの chains が同期されず子の plugin chain が無音になる (LoadSong は daw_audio のみが受信、daw_plugin_host は受信しない: `daw_plugin_host/src/main.rs` grep で 0 件)。

修正経路の候補（別 PR で対応）:
1. daw_plugin_host の `chains` を `HashMap<(track_id, slot), ...>` (track id ベース) に改修
2. または `MainToChild::InsertTrack { at: u32, id: u32 }` 専用 IPC を新設して daw_plugin_host 側の chains を shift up

PR2 は機能優先で末尾 append にとどめる。視覚位置は別 PR で追従する。

### 共通親の引き継ぎ
- 「全選択 track の `parent_group_id` が同一」のとき、新 group も同じ親を引き継ぐ
- 異なる場合は `parent_group_id = None` (top-level)
- ⇒ ある group の内側で track を複数選択して Ctrl+G すると、新 group はその既存 group の中に作られる (グループの中でグループ化、Live 互換)

### 子の repointing
- 選択 track 全員の `parent_group_id` を新 group の id に書き換え
- これにより新 group が親、選択 track 群が子になる

### 命名
- `Group N` (N = `song.tracks.len() + 1`)
- 後で rename 可

### 入力前提
- `track_ids` には**重複は来ない** (`selected_track_ids` 自体が重複しないように管理する)
- 存在しない id は事前に validation 不要 (`selected_track_ids` 同期は他で担保)

---

## 5. アングループ (Alt+G)

### 発火条件
- `selected_track_ids` のうち少なくとも 1 つが「子を持つ track」(= group)

### 挙動
各選択 group について:
- 子の `parent_group_id` を group の親 (`group.parent_group_id`) に書き換える (= 1 階層上に持ち上げる)
- group track 自体を `song.tracks` から remove
- group の `fx_chain` は失われる (Live 仕様)

複数 group が選択されているときは、深いものから順に処理 (子 → 親) してインデックスを安定化させる。

---

## 6. 削除 (DeleteTrack)

### group の削除
- 削除する track が **group (子を持つ)** のとき、子も再帰削除 (Live 仕様)
- 子孫 track の clip / fx / instrument 全部が失われる
- Undo で復元可能

### 通常 track の削除
- 既存どおり 1 track だけ消す
- 直前/直後の track の `parent_group_id` には影響なし

---

## 7. ルーティング (RT 側)

### NodeOp 列の構造 (Reaper folder 流)

通常 track の処理を 2 段に分け、間に「子の合計の加算」と「group 自身の audio fx + strip」を差し挟む。

```
NodeOp 列の例 (group_idx に child_a, child_b がぶら下がっているケース):

ProcessTrackPreFx { child_a }    // child_a の clips/midi_fx/instrument → scratch[a]
ProcessTrackFx    { child_a }    // child_a の audio fx_chain → scratch[a]
ProcessTrackStrip { child_a }    // child_a の strip → scratch[a] (post-fader)
[child_b も同様]

ProcessTrackPreFx { group_idx }  // group 自身の clips/midi_fx/instrument → scratch[group]
MixAdd { srcs: [scratch[a], scratch[b]], dst: scratch[group] }  // 子を group の scratch に加算
ProcessTrackFx    { group_idx }  // group の audio fx_chain → scratch[group]
ProcessTrackStrip { group_idx }  // group の strip → scratch[group] (post-fader)

Mix { srcs: [..., scratch[group], ...], dst: Master }
```

通常 track (子なし) は `MixAdd` のステップが省略されるだけ。`ProcessTrack*` 系の 3 段は全 track 共通。

### NodeOp 種別の整理 (PR2 で再設計)
旧 `ProcessTrack` (full chain) を 3 つに分割:
- `ProcessTrackPreFx { track_idx }` — sequencer / midi_fx / instrument / vocal sample → scratch
- `ProcessTrackFx { track_idx }` — audio fx_chain を scratch に適用
- `ProcessTrackStrip { track_idx }` — volume / pan / mute / solo + peak meter

旧 `ProcessGroupFx` は不要 (`ProcessTrackFx` + `ProcessTrackStrip` で代替)。
新規 `MixAdd { srcs, dst }` (既存 scratch を保ったまま srcs を加算) を `Mix` の派生として用意 — 既存 `Mix` (zero-clear してから加算) と区別する。

### Solo / Mute
- group の solo / mute は通常 track と同じく `any_solo` / `effective_mute` 計算に参加
- **子が solo されている group の effective_mute** は false 扱い (Live 互換): 子が solo なら group も透過させて master に届ける
  - 実装: `effective_mute = muted || (any_solo && !solo && !has_soloed_descendant)`
  - `has_soloed_descendant(track_id)` は子孫を BFS で走査
- Mute / Volume は multiplicative (子の volume × group の volume)

---

## 8. UI

### 8.1 Mixer strip
- group strip は青系背景 (`COLOR_GROUP_BG`)
- 階層インデント: strip の左端を `depth * INDENT_PX` 分インデント (arrangement view と同じ規則)
- M / S / Pan / Volume / peak meter は通常と同じ widget
- selected_track_ids にこの strip の id が含まれていればハイライト

### 8.2 Track Inspector
- 「Parent」ドロップダウン (top-level + 子を持つ既存 track のみ候補) — group も普通の track も差別なく親候補になりうる (任意の track が group になり得る)
- group track でも "+Inst" / "+MIDI" / "+FX" は **全部表示** (Reaper folder 流、§1 参照)
- chain list は `midi_fx_chain` / `instrument` / `fx_chain` を全部出す (group / 通常 track 区別なし)
- 単独選択 (selected_track_ids.len() == 1) のときだけ Inspector を出す。複数選択中は「N tracks selected」と表示し、操作は disable

### 8.3 Arrangement view (PR2 で実装)

#### 階層インデント
- 子トラック行の左端を `depth * INDENT_PX` 分インデント
- 行の高さは変えず、track header の x 座標だけずらす

#### 折り畳みボタン
- group 行の左に `▼` (展開中) / `▶` (折り畳み中) アイコン
- クリックで `AppData::collapsed_groups: HashSet<u32>` を toggle
- 折り畳み中は子トラックの row を hide (高さ 0、または skip 描画)
- 折り畳み中の group の peak meter は子の合算を継続表示

#### group 行の背景色
- mixer strip と同じ青系で塗る (group であることが arrangement view からも分かる)

### 8.4 Drag-and-drop による reparent (PR2 で実装)
- track header を **別 track の上**に drop:
  - drop 先が group なら、その group の子になる (drop ターゲットの最後の子の下)
  - drop 先が通常 track なら、drop 先と同じ親の隣に並ぶ (= 親変更しない、reorder のみ)
- track header を **空白領域 / master 行**に drop:
  - top-level に持ち上がる (`parent_group_id = None`)
- 視覚フィードバック:
  - drop 候補位置に水平の青いインジケータ (既存 reorderable_list と同じパターン)
  - 階層ネスト先には少しインデントしたインジケータ
- 多重選択 drag: 複数 selected_track_ids をまとめて移動

### 8.5 Transport bar
- "+Group" 専用ボタンは置かない
- "+Vocal" / "+Inst Track" は維持

---

## 9. 整合性チェック

### compile_schedule (実装済)
- DanglingReference: 親 id が存在しない
- Cycle: parent_group_id を辿るとループ
- 親 track の `kind` チェックは廃止 (kind 自体がない)。 **任意の track が group になり得る** ので、親が「子を持つ」かどうかは問わず、id 存在のみ検証

### action_set_track_parent (実装済、修正必要)
- 自分自身を親に → no-op + warn
- 親 id が存在しない → no-op + warn
- 新親 chain に自分が含まれる (cycle) → no-op + warn
- (kind チェックは削除)

### action_group_selected_tracks (実装済、修正必要)
- 空選択 → no-op + log
- 重複 de-dup ロジックは削除 (来ない前提)

---

## 10. PR2 のスコープ

### 必須 (実装済)
- TrackKind 廃止、Track::kind フィールド削除
- 多重選択 (`selected_track_ids: Vec<u32>` + Shift / Ctrl クリック)
- Ctrl+G グループ化 (**末尾 append**、§4 参照: 一番上の直前は別 PR)
- Alt+G アングループ (`RemoveTrack` IPC で plugin_host の chains 同期)
- G は元の grid snap に戻す
- delete_track で group の子を再帰削除
- 共通親継承 (選択 track 全員が同じ parent_group_id を持つ場合)
- arrangement view の階層インデント + 折り畳み (gui_01 #016 で widget 内蔵)
- drag-and-drop reparent (gui_01 #016 で widget 内蔵 + caller 側 SetTrackParent 3 段再構築)
- mixer strip の青色背景
- Inspector の Parent dropdown
- 子 solo 時の group effective_mute 透過 (`has_soloed_descendant`)
- engine 切替 (PR1 で実装済)

### 別 PR で対応
- **group 化時の挿入位置を「一番上の選択 track の直前」 に変更** — daw_plugin_host の `chains` を track index ベースから track id ベースに改修するか、 `InsertTrack { at, id }` 専用 IPC を新設。 PR2 では末尾 append で機能優先。 §4 参照
- mixer strip の階層インデント (gui_01 widget 側拡張、 daw_01 自前描画)
- track header の Shift / Ctrl クリックの **個別キー** detection (現在 widget 内 anchor で動作中、 user 検証済)
- PR3 PDC / PR4 sidechain + parallel out

---

## 11. テスト計画

### unit test
- `compile_schedule_with_group_hierarchy_emits_two_phase_mix` (実装済 — 仕様変更追従要)
- `nested_groups_emit_inner_group_before_outer` (実装済)
- `parent_cycle_is_rejected` (実装済)
- `parent_pointing_to_unknown_track_is_rejected` (実装済)
- 新規:
  - `is_group_returns_true_when_track_has_children`
  - `is_group_returns_false_after_last_child_is_removed`
  - `action_group_selected_tracks_inserts_above_top_selected`
  - `action_group_selected_tracks_inherits_common_parent`
  - `action_ungroup_promotes_children_to_grandparent`
  - `delete_group_track_removes_subtree_recursively`
  - `range_select_with_shift_click`
  - `toggle_select_with_ctrl_click`

### smoke test
1. 起動 → "+Vocal" 2 回で Track 1 / 2 を作成
2. arrangement の Track 1 ヘッダーをクリック → Track 2 ヘッダーを Shift+クリック → 両方ハイライト
3. **Ctrl+G** → Track 1 の直前に "Group 3" 出現、Track 1 / 2 が子に
4. arrangement の Group 3 行の `▼` をクリック → 子 row が折り畳まれる
5. group 3 行を別の group の上に drag → 親が変わる
6. Group 3 を選択して **Alt+G** → group が消え、Track 1 / 2 が top-level に戻る
7. (再 group 化して) Group 3 を delete → Track 1 / 2 もまとめて消える

---

## 12. 実装ステータス (PR2 完了時点)

| 機能 | 状態 |
|---|---|
| RT 側 schedule 駆動化 | ✅ |
| compile_schedule の group 階層対応 (子の有無で判定) | ✅ |
| ProcessGroupFx + run_group_fx_chain | ✅ |
| mix_into_track_scratch / mix_into_master | ✅ |
| process_track_owned の group early-return (子の有無で判定) | ✅ |
| Mixer strip の青色背景 | ✅ |
| Inspector の Parent dropdown | ✅ |
| TrackKind enum 削除 | ✅ |
| 多重選択 (`selected_track_ids: Vec<u32>`) | ✅ |
| `collapsed_groups: HashSet<u32>` 新設 | ✅ |
| Ctrl+G ショートカット (`daw.group_tracks`) | ✅ |
| Alt+G ショートカット (`daw.ungroup_tracks`) | ✅ |
| G を grid snap に戻す | ✅ |
| `action_group_selected_tracks` (de-dup なし、共通親継承、 末尾 append) | ✅ |
| `action_ungroup_tracks` (深さ降順処理、 RemoveTrack IPC) | ✅ |
| arrangement widget での 階層インデント / 折り畳み (▼/▶) / group 行背景色 / drag&drop reparent / track header の Shift / Ctrl クリック multi-select | ✅ (gui_01 #016 で widget 内蔵) |
| `SetTrackParent { tracks, parent, anchor_after }` の 3 段再構築 | ✅ |
| `ToggleGroupCollapsed(u32)` ハンドラ | ✅ |
| `delete_track` で group 削除時の subtree 再帰削除 | ✅ |
| 子 solo 時の group effective_mute 透過 (`has_soloed_descendant`) | ✅ |
| group 選択時の Inspector ボタン表示分岐 撤回 (Reaper folder 流で group も全機能保持) | ✅ |
| smoke test (group 化 → 子の音継続、 ungroup → 音継続、 ungroup フリーズ無し) | ✅ (2026-05-06 ユーザー検証 済) |
| **group 化時の挿入位置「一番上の選択 track の直前」 → 末尾 append** | ⚠️ **別 PR** (§4 参照: daw_plugin_host を track id ベースに改修要) |
| Mixer strip の階層インデント | ⚠️ 別 PR (gui_01 widget 側拡張要 or 自前描画拡張) |
