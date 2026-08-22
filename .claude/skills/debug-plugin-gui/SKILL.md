---
name: debug-plugin-gui
description: |
  CLAP / VST3 プラグインの埋め込み GUI が期待どおり開かない・前面に出ない・閉じない・
  リサイズしない・カスケード(2段)メニューが開かない等の症状を、ホスト構成 (FIXME #31 で
  エディタ窓を plugin-host プロセス所有へ移行) と IPC / スレッド / Win32 foreground に
  照らして切り分ける手順。
  「プラグイン GUI が表示されない」「エディタが裏に出る」「サブメニューが開かない」
  「✕ で閉じても状態が残る」「show が false を返す」「VST3 エディタが極小/出ない」等のとき発動。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(./target/debug/*)
---

<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# プラグイン GUI デバッグワークフロー

daw_plugin_host のプラグイン GUI が動かないとき、どの層で止まっているかを切り分ける。

## アーキテクチャ (FIXME #31 以降 — 重要)

**エディタのトップレベル窓は daw_plugin_host (= プラグインをロードするプロセス) が
plugin-main スレッド上で所有する** (`daw_plugin_host/src/editor_window.rs`)。daw_gui は
窓を持たず、開閉状態を `open_plugin_guis: HashSet<(track,slot)>` で追跡するだけ。

```
daw_gui (UI 側)
  ↓ open_slot_gui: AllowSetForegroundWindow(ASFW_ANY)  ← 前面化許可を付与
  ↓ MainToChild::OpenSlotGuiEmbedded { track, slot, title } ──▶
                                          daw_plugin_host (tokio recv → PluginCommand)
                                          ↓                     plugin-main std::thread
                                          ↓                     ├ gui_create_embedded
                                          ↓                     ├ gui_get_size (pre-attach)
                                          ↓                     ├ EditorWindow::create  ← 窓は B 所有
                                          ↓                     ├ gui_set_parent_hwnd(editor.hwnd)
                                          ↓                     ├ pump_pending_messages
                                          ↓                     ├ gui_show
                                          ↓                     ├ gui_get_size (post-attach 再取得)
                                          ↓                     ├ editor.set_client_size
                                          ↓                     └ editor.set_foreground
                                          ↓ ChildToMain::SlotGuiOpened { w, h } ◀──
daw_gui: on_gui_opened は no-op (窓は B が sizing 済み)
```

- **✕ クローズ**: B の WNDPROC が `WM_CLOSE` で close フラグ → ループが poll →
  `close_slot_gui` (plugin.gui_destroy → DestroyWindow) → `SlotGuiClosed` を B が送る。
  **daw_gui は close を poll しない**（旧 30Hz tick poll は撤去）。
- **リサイズ**: plugin の `request_resize` / `resizeView` は B 内の `gui_resize` チャネルに
  入り、ループが editor 窓 resize + onSize。**IPC 往復なし**。
- **削除/降格でエディタ追従**: `editor_windows` は close/resize は `(track,slot)` keyed だが、
  **削除は安定 `plugin_id` で照合**（slot ずれ耐性）。降格は `DemoteInstrumentToGenerator`
  でライブ移動し editor 窓を同 HWND のまま維持。詳細は memory `project-plugin-slot-rekey`。

## 典型的な症状と切り分け

### ① ボタンを押しても何も起きない
- daw_gui が送信したか: `sending to plugin_host msg=OpenSlotGuiEmbedded { ... }`
- B が受信したか: `received OpenSlotGuiEmbedded`。出ない → IPC 切れ (`pipe_loop` のエラー)
- plugin-main が `PluginCommand` を drain しているか。`PostThreadMessageW` の `thread_id` が 0 でないか

### ② エディタが裏 (メイン窓の後ろ) に出る / 前面に来ない
- **原因**: B の `SetForegroundWindow` が Win32 のフォーカス奪取防止で拒否される (B は foreground
  でない)。**修正**: daw_gui が open 直前に `AllowSetForegroundWindow(ASFW_ANY)` を呼ぶ
  (`app.rs::open_slot_gui`)。B は started-by-foreground-process なので許可されれば前面化が通る。
- メニューを開くだけなら不要 (エディタをクリックした時点で B が foreground になる)。前面化は polish。

### ③ VST3 エディタが極小 / まったく出ない (例: Analog Lab)
- **原因**: 一部 VST3 は `attached` **前**の `getSize` で 0×0 (or placeholder) を返し、本当のサイズは
  `attached` **後**にしか分からない。attach 前サイズで窓を作ると ~1px = 不可視。
- **修正** (`open_gui`): 初期サイズは 0/空なら 800×600 に既定化して attach、attach+show **後に
  `gui_get_size` を再取得**して `set_client_size`。ログ `VST3 getSize after attached w=.. h=..` を確認。

### ④ カスケード (2段) メニュー / サブメニューが開かない (JUCE 製: Scaler 2 等)
- **原因 (一次情報, JUCE #401)**: JUCE はメニュー窓の存続を毎 ~20Hz の `checkButtonState` で
  `doesAnyJuceCompHaveFocus()` により判定し、false なら ~10ms で自分を dismiss する。これは
  `isForegroundOrEmbeddedProcess(componentAttachedTo)` をゲートにする。
  - **1段目**メニューは `componentAttachedTo` = 実エディタ → `isEmbeddedInForegroundProcess` が
    `GetAncestor(hwnd, GA_ROOTOWNER)` を辿り foreground 窓の所有プロセスと一致すれば通る (#401 fix)。
  - **サブメニュー**は `Options::forSubmenu()` で `targetComponent = nullptr` → 上記 escape hatch が
    使えず `Process::isForegroundProcess()` = `getProcess(GetForegroundWindow()) == GetCurrentProcessId()`
    のみに落ちる。**エディタ窓を別プロセス (daw_gui) が所有していると plugin-host プロセス内で
    常に false → サブメニューだけ即 dismiss**。
- **修正**: エディタのトップレベル窓を **plugin-host プロセスが所有** (FIXME #31 で実施)。クリックで
  B が foreground になり成立。`AttachThreadInput` では直らない (foreground 窓の所有プロセスは
  変わらない)。詳細根拠は `docs/plan_plugin_editor_topwindow.md`。

### ⑤ `gui.show returned false`
1. CLAP spec の呼び出し順序を守っているか (create → set_scale → get_size → set_parent → show)。
   初回 open で `set_size` を呼んでいないか (VCV Rack 等が拒否)
2. `set_parent` と `show` の間にメッセージポンプ (`pump_pending_messages`) が入っているか
3. false でも GUI が実際に表示される例 (VCV Rack)。`Ok(false)` で警告ログに留め destroy しないのが正

### ⑥ ✕ で閉じても状態が残る / 次のクリックが効かない
- B の WNDPROC `WM_CLOSE` を `DefWindowProcW` に流すと Windows が `DestroyWindow` して RAII Drop と
  二重解放。必ず `ShowWindow(SW_HIDE)` + close フラグ + `LRESULT(0)` で傍受 (editor_window.rs)。
- WNDPROC → Rust state は `GWLP_USERDATA` に `Arc::into_raw`、Drop で `Arc::from_raw` 回収。
- close は B のループが close フラグを poll → `close_slot_gui` で `gui_destroy` → 窓 Drop →
  `SlotGuiClosed` 送信 → daw_gui の `on_gui_closed` が `open_plugin_guis` から除去。

### ⑦ プラグイン削除でエディタが閉じない / 別のエディタが閉じる
- `editor_windows` は `(track,slot)` keyed だが、エディタを開いたまま slot がずれる (下位 Fx 削除 /
  降格) と key が陳腐化。**削除は安定 `plugin_id` で照合**して破棄 (`destroy_editor_windows_where`)。
- daw_gui は削除前に `cleanup_slot_gui` (CloseSlotGui) を送ってから RemoveSlotPlugin (順序重要 —
  逆だとチェーンずれ後に隣のプラグイン GUI を誤破棄)。詳細 memory `project-plugin-slot-rekey`。

### ⑧ リサイズが反映されない / 一部しか見えない
- コンテナの *client* 領域が plugin の希望サイズと合っているか。`SetWindowPos` は **outer rect**
  (`AdjustWindowRectEx(WS_OVERLAPPEDWINDOW, false, 0)` で client→outer 変換)。
- 動的リサイズは B 内完結: `request_resize`/`resizeView` → `gui_resize` チャネル → ループが
  `editor.set_client_size` + `gui_set_size` (onSize)。daw_gui を経由しない。

### ⑨ プラグインが panic / 不正終了 / ハング
- すべての CLAP/VST3 呼び出しが **plugin-main std::thread 上で直列化**されているか (tokio に混ぜない)。
- GUI 呼び出し中に同時に process() が走るとプラグイン実装のバグで落ちる例あり。疑わしければ
  GUI 操作時は playback を stop。
- `clap_host_thread_check` を実装すればプラグイン側から main-thread 検証ができる。

## デバッグ用ログ追加のコツ
- IPC 送受信を全てログ化: `sending to plugin_host msg=...` / `received <Command>` / `<ChildToMain>`。
- プラグイン本体の stderr ログも見る (VCV Rack の `guiSetParent()`/`guiDestroy()` 等)。
- foreground 周りを疑うときは B 側で `GetWindowThreadProcessId(GetForegroundWindow())` vs
  `GetCurrentProcessId()` をログして「どのプロセスが foreground か」を確認。

## 参考ファイル
- `daw_plugin_host/src/editor_window.rs` — B 所有のエディタ窓 (FIXME #31)
- `daw_plugin_host/src/main.rs` — plugin-main thread / open_gui / close_slot_gui / Demote / 各 re-key
- `daw_plugin_host/src/clap_plugin.rs` / `vst3_plugin.rs` — gui_* メソッド群
- `daw_plugin_host/src/clap_host.rs` — `clap_host_gui` 実装
- `daw_gui/src/app.rs` — open_slot_gui / on_gui_opened / on_gui_closed / open_plugin_guis
- `docs/plan_plugin_editor_topwindow.md` — FIXME #31 の根本原因と設計
- CLAP 仕様: `/tmp/clap/include/clap/ext/gui.h` 先頭コメント / JUCE #401 (サブメニュー foreground 判定)
