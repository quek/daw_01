<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Plan: A7 — plugin ロード race condition の同期化

## Context

A3 smoke test (2026-05-05) で発見した A2 由来の race condition:
- Open でプロジェクトをロードした直後、 ユーザーが Play を押すと一部 track が silent
- 数秒後 / ループ後に該当 track が鳴り始める

### 原因
A2 で旧 `tracks.mutate` (audio thread を stop/restart して plugin lifecycle と同期) を撤去し、 plugin lifecycle を非同期 IPC に切り替えた:

```
GUI                   plugin_host         daw_audio
 │ SetSlotPlugin   →   │                    │
 │                     │ load + activate +  │
 │                     │ start_processing   │
 │                     │ (重い、 数秒)      │
 │ ← SlotPluginLoaded ─┤                    │
 │ OpenPluginShmem ─────────────────────→   │
 │                                          │ pump_commands で
 │                                          │ plugin_refs に register
```

ユーザーが Play を押すタイミング次第で、 audio engine の `plugin_refs` がまだ未登録の track が出る → `process_track_owned` で `slot_to_plugin_id.get((track_idx, Instrument))` が `None` → instrument dispatch を skip → silent。

## アプローチ

GUI 側で **pending plugin loads** を tracking、 Play を **queue** する。

### 設計
1. `AppData` に追加:
   - `pending_plugin_loads: HashSet<(u32, PluginSlot)>` (track, slot ごとに pending)
   - `pending_play: bool` (Play を queue 中フラグ)
2. `SetSlotPlugin` 送信時に `pending_plugin_loads.insert((track, slot))`
3. `AppEvent::SlotPluginLoadedFromChild` handler で `pending_plugin_loads.remove((track, slot))`
4. `play()` で `!pending_plugin_loads.is_empty()` なら:
   - `pending_play = true` を立てる
   - `status_message = "プラグイン読み込み中... (N 個残)"`
   - 通常の Play は発火しない
5. `SlotPluginLoadedFromChild` handler で `pending_plugin_loads.is_empty() && pending_play` なら:
   - `pending_play = false`
   - 通常の `play()` を発火 (queue 解放)

### スコープ判断
- 対応: Open / Add Track / SetSlotPlugin 直後の race (主目的)
- 非対応: track 削除 / plugin 切り替え時の race は別 PR (現実的に困らないなら放置)
- UI: status_message での進捗表示のみ (Play ボタンを disable する追加修飾は別 PR)

## ファイル変更

### 主な変更
- `daw_gui/src/app.rs`:
  - `AppData::pending_plugin_loads`, `pending_play` フィールド追加 + `AppData::new` 初期化
  - `SetSlotPlugin` を `send_plugin` で送る箇所 (2 箇所、 grep "MainToChild::SetSlotPlugin") に `pending_plugin_loads.insert(...)` を併設
  - `play()` (L1219-) を pending 判定 + queue 化
  - `on_plugin_loaded_from_child` (L1856-) に pending 解放 + queue Play flush

### 検証
- 静的: `cargo build / clippy / test` clean
- 動的 smoke test (commit 前必須):
  1. Open で複数 plugin track を持つプロジェクトをロード → 即 Play → 全 track が **同時に** 鳴り始める (silent track が無い)
  2. status_message に「プラグイン読み込み中...」 が一瞬表示される
  3. Add Track + plugin ロード後、 即 Play → 該当 track も最初の buffer から鳴る
  4. 既存挙動 (1 plugin / 0 plugin) のリグレッション無し

## 想定外への対応

- plugin ロードが失敗したら `SlotPluginLoaded` が来ない → pending が永久に残る → Play 永久 queue。 失敗通知 (ChildToMain::SlotPluginLoadFailed か timeout) を別 PR で対応
- Play 押下 → pending → ユーザーが Play 取り消し → 取り消し処理は別 PR (`pending_play = false`)
