# plan_master_fx — master トラックに fx を挿せるようにする

作成: 2026-05-31 / status: daw_01 側実装済み (gui_01 #061 landing 待ち)

## 実装進捗 (2026-05-31)

- [x] gui_01 #061 (master 行を選択可能に) — **landing 済み (gui_01 Phase 90)、status `[Resolved]`**。
      daw_01 側は SelectTrack handler が既に master を受理するため追加コード不要、rebuild のみ
- [x] Model: `Song.master_fx_chain` + `fx_chain_by_track_id(_mut)` accessor + ensure_ids
      sidechain remap (`common/src/model.rs`)
- [x] Audio engine: `process_master_fx_chain` を master mix 後・metronome 前に挿入
      (`daw_audio/src/engine.rs`)。 plugin host / slot_to_plugin_id は `(track,slot)` 汎用
      keying なので master も追加コードなしで通る
- [x] daw_gui: reconcile (`compute_slot_reconcile_actions` / `restore_plugin_from_song`)、
      state 書き戻し (`apply_plugin_states` / `song_has_plugin`)、load 反映
      (`on_slot_plugin_loaded`)、plugin install (`select_plugin_from_db`)、Inspector
      (`inspector_chain` / `selected_track_label` / `+FX` button)、chain ops
      (`reorder_inspector_chain` / `remove_slot(_inner)` / `toggle_slot_gui` /
      `open_slot_gui`) を master 対応
- [x] unit test: master fx reconcile + serde round-trip / forward-migration (`app.rs`)
- [x] build/clippy/test all green (`cargo build --workspace` / `clippy -D warnings` / `test`)
- [x] 実機 boot smoke: `cargo run -p daw_gui` で 3 プロセス handshake / audio session ready /
      plugin worker 起動までクリーン起動を確認 (exit 0)
- [x] master fx sidechain 対応: `compile_schedule` が master Mix の後に `SidechainTap`
      (dst_track=MASTER_TRACK_ID / Fx(i)) を emit。 `execute_schedule_post_dispatch` が
      source scratch を master fx plugin の `pd.buffer_aux_in` に staging → 直後の
      `process_master_fx_chain` が読む (`pd.prepare()` は events のみ reset、 aux_in は保持)。
      UI: `sidechain_entries` / `set_sidechain_source` を master 対応 (Inspector に sidechain
      section + source dropdown が出る)。 unit test `master_fx_sidechain_emits_tap_after_master_mix`
- [ ] **ユーザー click-through 確認待ち**: arrangement の Master 行を選択 → Inspector の「+ FX」で
      reverb / compressor 等を挿す → 再生して master バス全体に効果が乗るか + sidechain dropdown で
      source track を選んで ducking が効くか + 保存→再起動で復元されるか目視

---

## (以下、当初設計)


## 1. 背景と問題

現状 master トラックには fx を挿せない。UI 制限ではなくデータモデル由来:

- master は通常トラックと違い `Song.tracks[]` に**存在しない**。`MASTER_TRACK_ID = u32::MAX`
  という sentinel で表現され (`common/src/model.rs:2041`)、automation のみ `Song.song_lanes`
  に、mixer strip は `synthesize_master_track()` で都度合成される。
- `Song` / `Track` に **master 用 `fx_chain` フィールドが無い** → master の plugin chain を
  保存する場所が無い。
- 選択は `AppData.selected_track_ids` → `cursor_track_index()` (`app.rs:1303`) が
  `song.tracks` から index を引くので master ID は常に `None`。Track Inspector の「+ FX」も
  plugin install (`app.rs:12622` 付近) も全部 `cursor_track_index()` 経由なので master では no-op。

## 2. 設計方針 — 既存「master は Song 側」パターンの正統な拡張

### 2.1 二つの選択肢と決定

| 案 | 内容 | 評価 |
|----|------|------|
| **A. master を Song 側に置く (採用)** | `Song.master_fx_chain: Vec<PluginInstance>` を追加。`song_lanes` (automation) と同じく Song 直下に master データを置き、sentinel 分岐アクセサで通す。 | daw_01 が既に automation で確立済みのパターン (`automation_lane_by_key_mut`, `model.rs:382`)。Single Source of Truth に一致。 |
| B. master を `tracks[0]` 化 | sing_like_coding 流。master を通常 `Track` として `tracks` に入れる。 | daw_01 の既存 `song_lanes` / synthetic master row / mixer strip 設計を全て解体して作り直すことになる。既存の綺麗な分離を破壊する。理想ではない。 |

**採用: 案 A。** 根拠 = daw_01 には既に「master の automation は Track ではなく `Song.song_lanes`
に置き、`track_id == MASTER_TRACK_ID` で分岐する unified accessor で読み書きする」前例がある
(`model.rs:378-404`)。fx_chain も同じ形にするのが内部整合的で SSoT に沿う。

参照: sing_like_coding は master = `tracks[0]`、`track.modules` を全トラック共通の chain として
持ち、master mix 後に `tracks[0].modules` を直列 process する (`singer.rs:490-500`)。
処理の **順序** (全トラック mix → master chain process → master volume → output) はこれを踏襲する。
Ardour/REAPER も master は通常トラックリストと別管理の特別 bus であり、案 A と整合。

### 2.2 PluginSlot は拡張しない

master は audio fx chain のみ持つ (instrument / midi fx は master に意味がない)。
audio engine は plugin を `(track_id, PluginSlot)` で keying している (`engine.rs:189` 付近の
`slot_to_plugin_id: HashMap<(u32, PluginSlot), u32>`)。よって
**`(MASTER_TRACK_ID, PluginSlot::Fx(i))` をそのまま master fx slot のキーに使える**。
`PluginSlot` に `Master` variant を足す必要は無い (足すと二重表現になり SSoT 違反)。

## 3. レイヤ別の変更

### 3.1 Model — `common/src/model.rs`

1. `Song` に field 追加 (`tracks` の近く、`song_lanes` と同じ「master は Song 側」グループ):
   ```rust
   /// master bus の audio fx chain。 通常 track の `Track.fx_chain` (model.rs:965)
   /// と同 schema。 master は instrument / midi_fx を持たない (audio fx のみ)。
   /// audio engine は全 track mix 後・metronome 前に直列 process する。
   /// 旧 file は `#[serde(default)]` で空 Vec に forward-migrate。
   #[serde(default, skip_serializing_if = "Vec::is_empty")]
   pub master_fx_chain: Vec<PluginInstance>,
   ```
2. unified accessor を追加 (automation の `automation_lane_by_key_mut` と同 idiom):
   ```rust
   pub fn fx_chain_by_track_id(&self, track_id: u32) -> Option<&[PluginInstance]>
   pub fn fx_chain_by_track_id_mut(&mut self, track_id: u32) -> Option<&mut Vec<PluginInstance>>
   ```
   `track_id == MASTER_TRACK_ID` → `master_fx_chain`、それ以外 → `track_by_id(...).fx_chain`。
3. plugin id remap (`model.rs:471/481` の `remap_chain`) と GC / `ensure_ids` 系で
   `master_fx_chain` も track fx_chain と同じく走査対象に含める。漏らすと sidechain id / state
   復元がずれる。

### 3.2 Protocol — `common/src/protocol.rs`

- 型追加なし。既存 `MainToChild::SetSlotPlugin { track, slot, ... }` (`protocol.rs:452`) を
  `track = MASTER_TRACK_ID`, `slot = Fx(i)` で送る。`MoveSlot` / `ClearChain` / `RemoveSlot`
  系も同じく `track = MASTER_TRACK_ID` で master chain に効くよう、受け側 (audio engine /
  plugin host) の track 解決を `fx_chain_by_track_id` 経由に統一。
- ⚠️ bincode derive 型 (`Song`) に field 追加 → **`cargo build --workspace` 必須**。
  daw_gui だけ rebuild すると daw_audio.exe が古い protocol のまま LoadSong decode 失敗
  (memory: workspace-build-for-protocol-changes)。

### 3.3 Audio engine — `daw_audio/src/engine.rs`

- master mix 完了直後・metronome 直前 (`engine.rs:879-883` 付近、`render_metronome()` の手前) に
  master fx chain を直列 process。`process_track_owned()` の fx ループ (`engine.rs:1320-1357`) と
  同じ buffer io パターン:
  ```text
  for i in 0..master_fx_chain.len():
      key = (MASTER_TRACK_ID, PluginSlot::Fx(i))
      pd.buffer_in[0/1] <- master_l/r
      ws.dispatch(plugin_id)              // worker pool 経由 (track fx と同じ)
      master_l/r <- pd.buffer_out[0/1]
  ```
- master fx は全 mix 後の最終直列段なので並列化メリットは無い。graph (`compile.rs`) に
  `NodeOp` を足さず process_buffer 内 inline で良い (track fx と同じ `ws.dispatch` は使う)。
  ただし param automation (master の song_lanes 上 fx param automation) を将来入れるなら
  `fill_track_param_ramps` 相当を master 用に用意する余地を残す。
- RT 制約厳守: buffer は再生前確保・使い回し。master fx 用 scratch / ProcessData も起動時確保。

### 3.4 選択 — daw_gui + gui_01

UX = ユーザーが arrangement の master 行 (または mixer の master strip) を選択 → Inspector に
master の fx chain が出て「+ FX」で挿せる。

1. **gui_01 (要望提出が先)**: arrangement widget の master 行は現在 header click を選択として
   emit しない描画分岐になっている (`gui_01 .../arrangement.rs:7535` 付近、normal header 要素を
   skip)。master 行クリックで `SelectTrack { next: [MASTER_TRACK_ID] }` を emit するよう gui_01
   側の対応が必要。→ `docs/gui_01_conversation.md` に**最終形態 + `関連仕様: docs/plan_master_fx.md`**
   付きで要望を出す (memory: gui_01_conversation / scope_review / link_plan_ref)。interim workaround
   は作らない (memory: request_before_interim)。
2. **daw_01 選択 state**: master を選択可能にする。`cursor_track_index() -> Option<usize>` は
   master を表現できないので、選択解決を **track_id ベース**に寄せる。Inspector / plugin install が
   `selected_track_ids.last()` の **id** を直接見て、`MASTER_TRACK_ID` 分岐するようにする。
   (新規 enum `CursorTarget { Track(usize), Master }` を introduce するか、既存箇所を id 直接判定に
   置換するかは実装時に最小重複で決定。SSoT 優先。)
3. **Track Inspector** (`track_inspector.rs:127`): 現状 `app.song.tracks.get(idx)` 前提。
   選択が `MASTER_TRACK_ID` のとき master 専用 inspector を出す:
   - 表示するのは **FX chain セクションのみ** (instrument / midi fx / clip 系は master に無い)。
   - chain の add/remove/move は `Song.master_fx_chain` を対象に。
   - 「+ FX」ボタンは `OpenPluginPickerFor(PickerTarget::Fx)` を master 文脈で発火。
4. **plugin install** (`app.rs:12622` `select_plugin_from_db`): 冒頭の
   `cursor_track_index()` 解決を master 対応に。master 選択時:
   ```text
   track_id = MASTER_TRACK_ID
   dest_slot = Fx(master_fx_chain.len())
   master_fx_chain.push(PluginInstance{...})
   send SetSlotPlugin { track: MASTER_TRACK_ID, slot: Fx(next), ... }
   ```
   `send_destination_candidates` 等 master を含めるべき箇所も見直す (現状 `song.tracks` のみ走査)。

### 3.5 Mixer strip (任意, 同時にやると一貫)

master strip (`mixer_strips.rs:200-219`) からも fx chain にアクセスできると Ableton/REAPER 風。
最低限は arrangement 行選択 → Inspector 経路で足りる。mixer strip からの選択同期は別途。

## 4. 永続化 / migration

- `master_fx_chain` は `#[serde(default)]` で旧 project file は空 Vec に forward-migrate。
  file version bump は不要 (serde default で吸収)。
- save 前 GC / id 再採番に master_fx_chain を含める (§3.1-3)。

## 5. 実装順序 (別セッションで着手)

1. gui_01 へ「master 行を選択可能に」要望提出 (`docs/gui_01_conversation.md`)。先に出す。
2. Model: `master_fx_chain` + unified accessor + remap/GC 追加 → `cargo build --workspace`。
3. Audio engine: master fx 直列 process を mix 後・metronome 前に挿入 → `cargo build -p daw_audio`。
4. daw_gui: 選択解決の master 対応 + Inspector master 分岐 + plugin install master 分岐。
5. gui_01 landing 後に master 行選択を wire (diagnostic 自動検知, memory: gui_01_auto_resume)。
6. `cargo run -p daw_gui` で実機 smoke: master 行選択 → +FX で reverb 等挿入 → 再生して効果確認。

## 6. 検証チェックリスト

- [ ] 旧 project file が `master_fx_chain` 無しで load できる (forward-migrate)。
- [ ] master に挿した fx が保存 → 再起動で復元される (state 含む)。
- [ ] master fx が全トラック mix 後に効く (track fx と二重に効かない)。
- [ ] RT 制約: master fx process 経路でヒープ確保 / lock / I/O が無い。
- [ ] daw_audio.exe を rebuild 済み (protocol field 追加後)。

## 参照

- daw_01 既存 master-on-Song 前例: `common/src/model.rs:378-404` (automation lane unified accessor),
  `:2041` (MASTER_TRACK_ID), `:182` (song_lanes)。
- track fx process パターン: `daw_audio/src/engine.rs:1320-1357`、master mix: `:1715-1739`、
  挿入点: `:879-883`。
- plugin install / 選択: `daw_gui/src/app.rs:1303` (cursor_track_index), `:12622`
  (select_plugin_from_db)、`track_inspector.rs:127`。
- sing_like_coding (master = tracks[0], chain serial process): `singer.rs:433-528`。
