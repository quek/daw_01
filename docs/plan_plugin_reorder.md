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
