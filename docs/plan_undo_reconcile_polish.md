# Undo/Redo plugin sync polish 計画

ステータス: **着手中** (2026-05-14)。 [`plan.html`](plan.html) §「Undo/Redo plugin
sync の残リスク」 で挙げた残 3 件 (B / D / E) を片付ける小規模 polish。 機能
追加なし、 既存挙動の堅牢化と test 整備のみ。

## 背景

A8 完了で「load 失敗 → pending stuck」 (旧 A) は解消、 4dc982c で
reconcile が slot 粒度に拡張済 (旧 C)。 残るのは以下:

- **B**: 連続 deferred edit の race。 `pending_state_request.is_some()` 即時
  fallback path で 2 番目以降の deferred edit が state 同期なしに実行され、
  Undo で knob 値が復元されないケース (`delete_track` / `action_ungroup_tracks`
  / `remove_slot` 3 箇所)
- **D**: 4dc982c (slot-level diff) の integration test なし。 reconcile の
  「host extra / song extra / plugin_id_str mismatch」 3 ケースを cover する
  test 不在
- **E**: 多段 Undo パフォーマンス未検証。 `after_undo_redo` で reconcile が
  毎 step 走る (機能正しさは OK、 連続 load/unload の cost 未測定)

## PR 計画

### PR-1: Risk B — pending_state_request を queue 化

**動機**: 現状の `pending_state_request: Option<PendingStateRequest>` は
1 つしか保持できず、 in-flight 中に来た新規 request は state 同期なしで
即時実行する。 plugin 削除が絡む編集を高速で 2 回続けると、 2 回目の
edit に対する Undo snapshot には plugin の最新 knob 値が乗っていないので、
Undo しても knob が復元されない。

**スコープ**:
- `AppData::pending_state_request: Option<PendingStateRequest>` を
  `pending_state_queue: VecDeque<PendingStateRequest>` に変更
- 「in-flight」 判定は `!pending_state_queue.is_empty()` (= queue.front()
  に対応する `RequestAllStates` が既に発行済)
- 新規 helper `enqueue_state_request(req)`:
  - queue が空なら push + `send_plugin(RequestAllStates)`
  - 非空なら push のみ (= 先行 request の応答後に処理される)
- `on_all_states_from_child`:
  - `apply_plugin_states` は従来どおり最新化
  - `pop_front` で完了 request を取得 → 実行
  - 完了後に queue.front() が残っていれば 改めて `RequestAllStates` を発行
- 既存 3 dispatcher (`delete_track` / `action_ungroup_tracks` / `remove_slot`)
  と `begin_save` から `pending_state_request.is_some()` 即時 fallback を
  撤去 (= 必ず queue 経由)
  - 例外: `song_has_plugin()` が false の場合は従来どおり即時実行
    (= state を取りに行く相手が居ない)

**受け入れ基準**:
- 2 連続 delete_track / remove_slot / ungroup_tracks の Undo で knob 値が
  両方とも復元される
- Save 中に delete_track / remove_slot が来ても順序保持
- 既存 single edit + Undo の挙動は変化なし
- `cargo build/clippy/test --workspace` clean

**規模**: ~80-150 行 (純構造変更、 機能追加なし)

---

### PR-2: Risk D — slot-level diff の unit test

**動機**: 4dc982c の reconcile slot-level diff は smoke test (実機 +
`spec/sidechain.daw`) でしか動作確認していない。 regression を防ぐため
pure function に切り出して unit test を追加する。

**スコープ**:
- `reconcile_plugins_with_song` の Phase B (slot diff) ロジックを純粋関数に
  抽出:
  ```rust
  enum ReconcileAction {
      RemoveSlot { track_id: u32, slot: PluginSlot },
      LoadSlot { track_id: u32, slot: PluginSlot, plugin_id_str: String },
  }
  fn compute_slot_reconcile(
      song: &Song,
      loaded_slots: &HashMap<(u32, PluginSlot), LoadedSlotInfo>,
  ) -> Vec<ReconcileAction>
  ```
- 既存 `reconcile_plugins_with_song` は `compute_slot_reconcile` を呼んで
  各 action を IPC に dispatch する形に refactor (= ロジック差分なし)
- unit test 3 件:
  - **case A** (host extra): host に Fx(0)+Fx(1)、 song は Fx(0) のみ →
    `RemoveSlot { Fx(1) }` 1 件
  - **case B** (song extra): host に Fx(0)、 song に Fx(0)+Fx(1) →
    `LoadSlot { Fx(1) }` 1 件
  - **case C** (plugin_id_str mismatch): host に Fx(0)=PluginA、 song に
    Fx(0)=PluginB → `LoadSlot { Fx(0), PluginB }` 1 件 (plugin_host の
    dedup logic で実 IPC は再 load になる)

**受け入れ基準**:
- 3 cases pass
- `reconcile_plugins_with_song` 実行時の IPC 順序は変化なし (= 既存実機
  smoke は green を維持)

**規模**: ~120-180 行 (extract + 3 test cases)

---

### PR-3: Risk E — 多段 Undo perf 測定 + 必要なら最適化

**動機**: `after_undo_redo` で `reconcile_plugins_with_song` が毎 step 走る。
plugin 数 × Undo step 数で IPC + plugin load/unload cost が線形にかかる
可能性。 まず測定してから最適化の要否を判断する (KISS)。

**スコープ**:
- `after_undo_redo` の `reconcile_plugins_with_song` 呼び出し直前 / 直後に
  `tracing::info!(target: "undo_perf", elapsed = ?t, ...)` を仕込み、
  `Instant::now()` の差分を出す (= debug 用の常時ログ、 release でも
  軽量)
- 手動測定手順 (本 plan に記録):
  1. `spec/sidechain.daw` を開く (= plugin 多数)
  2. 連続 10 step の Undo (例: knob 編集を 10 回した状態から Ctrl+Z 10 回)
  3. tracing 出力から各 step の reconcile 所要時間を集計
- 結果:
  - <50ms / step なら OK → 最適化なし。 plan に記録のみ
  - >50ms / step なら次の最適化を検討:
    - 直前 Song と現 Song の plugin chain (= MidiFx/Instrument/Fx
      の plugin_id_str 列) を比較し、 一致なら reconcile skip
    - reconcile 結果を coalesce (= 連続 Undo で同一 slot が何度も
      Remove/Set されないようにする)

**受け入れ基準**:
- 測定結果が plan に追記される
- 最適化不要なら plan に「N ms / step、 許容範囲」 と記録して closure
- 最適化必要なら追加 PR で対応

**規模**: ~30 行 (測定 hook) + 必要なら最適化 ~100 行

---

## 進行状況

- ✅ **PR-1 (Risk B)**: `pending_state_request: Option<_>` → `pending_state_queue:
  VecDeque<_>` に置換。 `enqueue_state_request` helper 追加、 4 dispatcher
  (`begin_save` / `delete_track` / `action_ungroup_tracks` / `remove_slot`)
  から「 in-flight 中の即時 fallback」 を撤去 → queue に積んで先行 request の
  応答後に処理。 `on_all_states_from_child` で `pop_front` + 次が残っていれば
  再度 `RequestAllStates` を発行。 test `tests/pending_state_queue.rs` で
  2 連続 RemoveSlot のシリアライズを検証。
- ✅ **PR-2 (Risk D)**: `reconcile_plugins_with_song` Phase B を純粋関数
  `compute_slot_reconcile_actions(&Song, &HashMap) -> Vec<SlotReconcileAction>`
  に抽出。 `SlotReconcileAction::{RemoveSlot, LoadSlot}` enum で IPC 抽象化。
  test `tests/reconcile_slot_diff.rs` で 5 ケース (host extra / song extra /
  plugin_id_str mismatch / 完全一致 no-op / initial_state 伝搬) を unit test。
- ✅ **PR-3 (Risk E)**: `after_undo_redo` の `reconcile_plugins_with_song`
  呼び出し前後で `Instant::now()` を取り、 `daw_gui::app::undo_perf` target
  に elapsed_us を `tracing::info!`。 実運用で多段 Undo が遅いと感じた際に
  `RUST_LOG=daw_gui::app::undo_perf=info` で測定可能。 PR-2 の純粋関数 test
  `matching_slot_produces_no_action` で「plugin chain 不変の Undo は IPC ゼロ」
  を保証 (= 最大の cost 源を slot-level diff が遮断)。 機能正しさは smoke
  済、 cost が問題化したら追加 PR で coalesce 検討。

すべての PR (PR-1 / PR-2 / PR-3) 完了。 `cargo build / clippy / test --workspace`
clean を確認 (147+39+22+... 全テスト pass)。
