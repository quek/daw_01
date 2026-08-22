<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Plan: A8 — Plugin load 失敗通知 (`SlotPluginLoadFailed`)

## Context

A7 ([plan_a7_plugin_load_sync.md](plan_a7_plugin_load_sync.md)) の末尾で「失敗通知
(`ChildToMain::SlotPluginLoadFailed` か timeout) を別 PR で対応」 と明示した
残課題。 直近の Undo/Redo reconcile (4dc982c) で `SetSlotPlugin` が
reconcile 経由でも飛ぶようになり、 失敗が起きると pending stuck になる
頻度が上がるため優先度が上がった。

### 症状

- 失敗時、 daw_plugin_host は `tracing::error!` + `continue` のみで daw_gui
  に何も通知しない
- daw_gui の `pending_plugin_loads: HashSet<(track, slot)>` から該当 entry が
  永久に消えない
- 結果: `pending_play` が解放されず Play queue 永久 → 再生不能
- `play()` の status_message が「プラグイン読み込み中... (残 N)」 のまま固まる

### 失敗箇所 (daw_plugin_host/src/main.rs)

| 場所 | 失敗 | 既知 trigger |
|---|---|---|
| L505 `load_plugin(...)` Err | `.clap` / `.vst3` 自体の load 失敗 | ABI 不一致 / file not found / entry init returns false |
| L641-647 `ProcessDataHandle::create(&shmem_id)` Err | shmem 作成失敗 | ENOMEM (極稀) |

L641 の方は **新 plugin が既に line 553 で `install_plugin` 済** で
`tracks.chains` に live。 ただし `plugin_lookup` / `loaded_id_for_slot`
未登録なので **orphan** (RemoveSlotPlugin で消せない、 でも process は動く)
状態になる。 修正で必ず teardown する必要がある。

## 設計

### IPC 拡張 (`common/src/protocol.rs`)

`ChildToMain` に variant 追加 (新規追加なので backward compatible):

```rust
SlotPluginLoadFailed {
    track: u32,
    slot: PluginSlot,
    plugin_id: String,   // SetSlotPlugin で渡された stable id
    reason: String,      // tracing::error! と同等の message
},
```

### plugin_host 側

`PluginEvent::PluginLoadFailed { ... }` を追加 + `From<PluginEvent>` 実装で
`ChildToMain::SlotPluginLoadFailed` に対応。

L505 (`load_plugin` Err) — 旧 plugin 状態は touch していないので emit のみ:

```rust
let plugin = match load_plugin(format, &path, &plugin_id, callbacks) {
    Ok(p) => p,
    Err(e) => {
        tracing::error!(error = ?e, ?format, path = %path.display(), "load failed");
        let _ = evt_tx.send(PluginEvent::PluginLoadFailed {
            track,
            slot,
            plugin_id: plugin_id.clone(),
            reason: format!("{e}"),
        });
        continue;
    }
};
```

L641 (`ProcessDataHandle::create` Err) — 既に install 済の新 plugin を
**detach + quiesce + teardown** で掃除してから emit:

```rust
Err(e) => {
    tracing::error!(error = ?e, new_plugin_id, "failed to create ProcessData shmem");
    // orphan cleanup: 新 plugin は line 553 で install 済。 same dance
    // as SetSlotPlugin の旧 plugin teardown 経路 (registry None publish
    // は new_plugin_id 未 publish なので不要) で安全に teardown する。
    let mut detached: Option<Box<dyn LoadedPlugin>> = None;
    tracks.mutate(|t| {
        if let Some(chain) = t.chains.get_mut(&track) {
            detached = detach_plugin(chain, slot);
        }
    });
    if let Some(pool) = worker_pool.as_ref() { pool.quiesce(); }
    if let Some(p) = detached { teardown_plugin(p); }
    let _ = evt_tx.send(PluginEvent::PluginLoadFailed {
        track,
        slot,
        plugin_id: plugin_id.clone(),
        reason: format!("shmem create failed: {e}"),
    });
}
```

### daw_gui 側 (`daw_gui/src/app.rs`)

`AppEvent::SlotPluginLoadFailedFromChild { track, slot, plugin_id, reason }` を
追加 + handler:

```rust
fn on_plugin_load_failed_from_child(
    &mut self,
    track: u32,
    slot: PluginSlot,
    plugin_id: String,
    reason: String,
) {
    tracing::error!(track, ?slot, %plugin_id, %reason, "plugin load failed");
    // (1) pending 解放: A7 と対称的に、 失敗 = ロード round-trip 完了 と同じ扱い
    self.pending_plugin_loads.remove(&(track, slot));
    // (2) Song の該当 slot は touch しない (= 旧 plugin が動いていれば
    //     継続、 reconcile 由来で旧無し → slot 空のまま)
    // (3) ユーザー通知
    self.status_message = format!(
        "プラグイン読み込み失敗: {plugin_id} ({reason})"
    );
    // (4) pending_play 解放: A7 と同じロジック (pending_plugin_loads が
    //     空になり、 かつ Play 待ちだったら flush)
    if self.pending_plugin_loads.is_empty() && self.pending_play {
        self.pending_play = false;
        self.send_audio(MainToChild::Play);
        self.is_playing = true;
    }
}
```

### dispatch サイト

`ChildToMain::SlotPluginLoadFailed` を AppEvent に変換する箇所:
- [daw_gui/src/main.rs:167](../daw_gui/src/main.rs:167) — 通常 GUI 起動時の receiver loop
- [daw_gui/src/script.rs:161](../daw_gui/src/script.rs:161) — JS scripting (headless integration test 用)
- [daw_gui/src/script.rs:446](../daw_gui/src/script.rs:446) — script の wait helper

## 試験

### 静的
- `cargo build --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`

### 単体テスト (`daw_gui/tests/plugin_load_failure.rs` 新設)

`group_track_lifecycle.rs` 同様、 fake plugin_host channel + AppData を
組んで:

1. `track_pending_load(0, Instrument)` → `pending_plugin_loads` に entry
2. Play 押下 → `pending_play = true`
3. `AppEvent::SlotPluginLoadFailedFromChild { ... }` を inject
4. `pending_plugin_loads` から entry が消えていることを assert
5. `pending_play` が false に戻り、 `is_playing` が true になり、
   `send_audio(MainToChild::Play)` が記録されていることを assert
6. `status_message` に「プラグイン読み込み失敗:」 が含まれることを assert

### 実機 smoke

1. `DAW_CLAP_PATH=C:\nonexistent\foo.clap cargo run -p daw_gui` のように
   存在しない plugin path を指定 → 起動時のロードで失敗 → status bar に
   エラー表示
2. または scripted: 正常 plugin で song を組んだあと、 Inspector の
   plugin picker で **わざと壊した .clap (4 byte に切り詰めた dummy file)**
   を選択 → 失敗通知 → Play は通る (pending 解放確認)
3. Undo/Redo 経由の reconcile で失敗するケース: plugin DB に登録した plugin
   を **DAW 起動中に file を削除** → Undo で reconcile が SetSlotPlugin →
   load_plugin Err → status bar 表示 + Play 復活

## ファイル変更

| ファイル | 変更 |
|---|---|
| `common/src/protocol.rs` | `ChildToMain::SlotPluginLoadFailed` 追加 |
| `daw_plugin_host/src/main.rs` | `PluginEvent::PluginLoadFailed` 追加 + `From` impl + L505 / L641 の error path 拡張 (orphan cleanup 含む) |
| `daw_gui/src/app.rs` | `AppEvent::SlotPluginLoadFailedFromChild` 追加 + handler `on_plugin_load_failed_from_child` + `handle_event` 分岐 |
| `daw_gui/src/main.rs` | ChildToMain → AppEvent dispatch 追加 |
| `daw_gui/src/script.rs` | 同上 (2 箇所) |
| `daw_gui/tests/plugin_load_failure.rs` (新規) | unit test |

## スコープ外 (将来課題)

- **timeout 機構**: 失敗通知が来ない場合の最終安全網。 現実装は失敗時に
  必ず Err パスを通って通知が emit されるので冗長。 plugin_host 自体が
  panic / freeze した場合は IPC が close するため別 layer (子プロセス
  death detection) が必要 → 別 PR
- **失敗 plugin の suspended badge**: 失敗 slot を Inspector で × アイコン
  + retry ボタン表示 → UX 改善 PR
- **reason の i18n**: `format!("{e}")` の英語 message をそのまま表示。
  L10n は M2 全体 i18n と一緒
- **plugin DB scan で orphan plugin entry を抑止**: ユーザーが file を消し
  たケースで scan 側で early reject → 別 PR
