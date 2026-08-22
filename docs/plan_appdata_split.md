<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_appdata_split.md — `AppData` God Object の段階的解体

## 動機

`daw_gui/src/app.rs` (≈7,600 行) の `AppData` 単一 struct に GUI 全状態が
集中している。

- フィールド 60+、`AppEvent` バリアント 200+、`handle_event` の巨大 match
- session ごとの reflection log でも常時 read ホットスポット (37 read / session)
- selection 系の `unwrap_or(0)` / `unwrap_or_default()` が多用され、
  「未選択」と「index 0」が区別できない箇所が散在
  (例: peak meter `get(i).copied().unwrap_or((0.0, 0.0))`)
- `view/*.rs` が `AppData` の具象を直接触るので、テスト・差し替えが困難

## 目標構造

```text
AppData {
    tracks: TracksState,       // Song / 編集 / clipboard / undo
    view: ViewState,           // viewport / cursor / selection / modal
    workers: WorkerState,      // autosave / playhead / midi / VOICEVOX synth
    plugin_host: PluginHostState, // 子プロセス bridge / chain reconcile
    transport: TransportState, // play / loop / metronome
}
```

各 sub-state は独自の `handle_event(&mut self, ev: SubEvent)` を持ち、
`AppData::handle_event` は dispatcher として top-level enum を子 enum に
分配するだけ (≈ 50–100 行)。

## 段階移行 (4 ステップ)

```text
Step 1  PluginHostState を切り出し       ← 最も独立性が高い
Step 2  WorkerState (autosave/playhead/midi 系) を切り出し
Step 3  ViewState (viewport / cursor / selection / modal) を切り出し
Step 4  TracksState (Song + undo + clipboard) を切り出し
```

各ステップは別 PR。順序は依存方向 (PluginHost は Tracks に依存しない →
最初に切る) に従う。

## 各ステップの定義

### Step 1: PluginHostState

- `plugin_host_windows: HashMap<PluginKey, PluginHostWindow>`
- `plugin_chain_reconciler` 関連
- `pending_state_request` / `DeferredEdit`
- `AllStatesReceived` / `SlotPluginLoaded*` 系イベント
- 既存の AppData 上 `pub fn` は `pub fn plugin_host_mut(&mut self) -> &mut PluginHostState` で expose

### Step 2: WorkerState

- `autosave_*` フィールド
- `last_playhead_samples` / `peak_l` / `peak_r` / `track_peaks`
- `MidiInputOpened` / `Tick` / `TrackPeaksTick` / `AutosaveTick` の handler

### Step 3: ViewState

- `viewport`, `cursor_track`, `cursor_row`, `selection`, `modal_*`
- selection 系 `Option` を `Option` のまま流す。 `unwrap_or(0)` 削除

### Step 4: TracksState

- `Song`, `clipboard`, `undo_stack`, `redo_stack`
- `push_undo_snapshot` / `apply_edit` / `import_audio`

## リスク / 制約

- view 関数は AppData を直接参照しているため、Step ごとに view 側も
  `&TracksState` 等に書き換える必要がある
- dispatcher は AppData::handle_event 1 箇所なので、ここで子 event の
  ルーティングが正しく走るか丁寧に test
- gui_01 の `Edit<M>` クロージャは `&mut AppData` を受け取る前提。
  Edit 経由の更新は現状維持で AppData 越しに sub-state を触る

## 完了基準

- `app.rs` 行数 ≤ 1500 (現状 ≈ 7,600)
- `AppEvent` enum を top-level + 4 sub-enum に分解
- `unwrap_or(0)` / `unwrap_or_default()` を selection / view 系から消す
  (= 「未選択」 と 「index 0」 を分離)
- view 各 widget が `&TracksState` / `&ViewState` のような具体 struct を
  受け取るシグネチャに改修
- 既存の clippy / build / smoke test 全 pass

## 関連

- [docs/plan_song_ssot.md](plan_song_ssot.md) — Song を 3 プロセス重複から
  canonical + cache に変える計画。 Step 4 (TracksState) と同時または直後に進める
