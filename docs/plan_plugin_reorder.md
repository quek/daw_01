<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: プラグイン D&D 並び替えの音追従（別セッション対応）

FIXME #31 セッション (2026-06-10) の調査メモ。実装は別セッションで行う。

## 現状

- **UI はある**: インスペクタ「Chain」セクションは gui_01 の `reorderable_list` で
  ドラッグ並び替え対応。`daw_gui/src/view/track_inspector.rs:1865` 付近で wire され、
  `AppEvent::ReorderInspectorChain(order)` を発火。
- **ハンドラ**: `daw_gui/src/app.rs::reorder_inspector_chain` (~13720)。
  生成器(MidiFx)/音源(Instrument)/audio FX のセクションをまたいだ移動を、各プラグインの
  port 能力 (`PluginCapability::allows_slot_kind`) で検証してから song を組み替え、
  最後に `sync_song_to_plugin_host()` を呼ぶ。

## 「UI 上で並びが変わらない」の正体（要再現確認）

- `reorder_inspector_chain` は**ポート的に無効な drop を early-return で棄却**する
  (= UI は元の並びへ戻る)。
- テスト時のチェーンが `[Scaler(生成器), Analog Lab(音源)]` だと、両者の入れ替えは
  「音源の後ろに note 生成器」になり**正しく棄却**される → UI が変わらないように見える。
- → まず **FX を 2 つ以上挿した状態で FX↔FX の並び替え**が UI 反映されるか確認する。
  それでも変わらないなら `reorderable_list` widget のドラッグ検出 or 統合不具合を疑い、
  `reorder_inspector_chain` 冒頭に `tracing::info!(?order)` を仕込んで発火を確認する
  (debug-gui skill の手順)。

## 音追従の欠落（本丸）

`sync_song_to_plugin_host()` は **daw_audio へ `LoadSong` を送るだけ**:
- plugin_host のチェーン順は `MoveSlot` を送っていないので**変わらない**。
- daw_audio の `slot_to_plugin_id`（slot→plugin_id）も**貼り替えない**。

→ 仮に song/UI が並び替わっても、各 slot の plugin_id マップが旧のままなので
**実処理順（音）は追従しない**（既知バグ、memory `project_plugin_slot_rekey`）。

## 実装方針（FIXME #31 で揃った基盤を使う）

並び替え時に、移動した各プラグインについて:
1. plugin_host へ `MoveSlot{from, to}`（または降格で追加した
   `DemoteInstrumentToGenerator` と同系の re-key コマンド）を送り、plugin_host の
   チェーン順・`plugin_lookup`・`editor_windows`（エディタ窓追従、FIXME #31 で実装済み）・
   registry entry slot を貼り替える。**cross-section (Instrument↔MidiFx↔Fx) は
   `move_plugin` が未対応**なので、汎用の slot 移動に拡張するか、降格と同じく専用経路にする。
2. daw_audio を再マップする: 移動後に該当プラグインの `SlotPluginLoaded` を新スロットで
   **再送**（FIXME #31 の降格と同じ手法 → `OpenPluginShmem` 再発行）。daw_audio 側の
   `OpenPluginShmem` ハンドラは「別スロットにまだマップ済みの plugin_id を plugin_refs
   から落とさない」保護を **FIXME #31 で導入済み**（`daw_audio/src/engine.rs` の
   OpenPluginShmem 周辺）なので、移動した音源の取りこぼしは起きない。
3. 最後に `LoadSong` でスケジュール再構築（既存どおり）。

## 関連コード

- `daw_gui/src/view/track_inspector.rs:1865` — reorderable_list の wire
- `daw_gui/src/app.rs::reorder_inspector_chain` — 検証 + song 組み替え
- `daw_gui/src/app.rs::sync_song_to_plugin_host` — 現状 LoadSong のみ
- `daw_plugin_host/src/main.rs` — `MoveSlot` / `DemoteInstrumentToGenerator` ハンドラ（再 key の手本）
- `daw_audio/src/engine.rs` — `OpenPluginShmem` ハンドラ（slot→pid マップ、FIXME #31 で stale 保護済み）

## 実装済み (FIXME #32, 2026-06-11)

ライブ移動方式で 3 プロセス貫通の再キーを実装。新コマンド
`MainToChild::ReorderChain { track, moves: Vec<(PluginSlot/*old*/, PluginSlot/*new*/)> }`
を **両 child に送信**:

- **daw_gui** `reorder_inspector_chain` → `apply_chain_reorder`: 旧→新 slot の
  完全 permutation `moves` を組み、`loaded_slots`/`open_plugin_guis`/`plugin_params`
  と **automation lane の `PluginParam{slot}`**（`remap_lane_slots`）を再キー。track / master
  両分岐対応。
- **daw_plugin_host** `PluginCommand::ReorderChain`: live `Box<dyn LoadedPlugin>` を
  heap address 保持のまま permute（音切れなし・エディタ窓追従）、`plugin_lookup` /
  `loaded_*_for_slot` / `editor_windows` / registry entry slot を再キー。
- **daw_audio** `AudioCommand::ReorderChain`: `slot_to_plugin_id` を 1 回の ArcSwap store
  で atomic 再キー（`plugin_refs` 不変＝transient drop なし）。処理順は後続 `LoadSong` の
  schedule 再構築で追従。

検証: `daw_gui/tests/group_track_lifecycle.rs` に 3 つの reorder test
（3 プロセス再キー / automation 追従 / 不整合チェーンの no-op）。adversarial review
（16-agent workflow）で 5 findings、うち correctness 3 件を修正。

### 修正した review findings

- **#3 automation lane が追従しない**: `PluginParam{slot}` は slot 番地かつ永続化される。
  reorder で旧 slot のまま残ると別 plugin を automation が駆動し保存もされる
  → `remap_lane_slots` で旧→新へ remap。
- **#1/#4 host validation skip と audio/gui apply の分岐**: load 失敗の phantom が song に
  残ると host の live chain が song と不一致 → host は ReorderChain を skip、audio/gui は
  適用で恒久分岐。`reorder_inspector_chain` 冒頭で **song チェーン == loaded_slots** を
  gate（不一致なら snap back + status_message）。host 側 validation は IPC trust-boundary の
  belt-and-suspenders として残置。

### 未対応 (別 commit の follow-up)

- **#2 RT スレッドでの heap 確保**: `AudioCommand::ReorderChain` は `pump_commands`
  経由で **CPAL callback (RT) スレッド** 上で HashMap clone + `Arc::new` する。これは
  既存の `OpenPluginShmem` / `ClosePluginShmem` と同一パターン（既存の architectural debt）。
  理想は slot_to_plugin_id mutation 全体を RT callback 外（IPC/専用スレッド）へ移し ArcSwap
  publish のみ RT で行う設計。reorder 固有ではないので別途。
- **#5 reorder を跨ぐ Undo が live-move でなく reload になる**: reorder は undo snapshot を
  積まない（現状維持）。reorder 前の編集を 1 回 Undo で跨ぐと、`compute_slot_reconcile_actions`
  が permutation を認識せず Remove+Load で再 instantiate（エディタ窓再生成・非永続 state 喪失の
  可能性）。理想は reconcile を permutation-aware にするか reorder を undoable 化して Undo 時に
  live `ReorderChain` を発行する。reconcile ロジック（well-tested）に触れるので別途。
