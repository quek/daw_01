---
name: debug-plugin-gui
description: |
  CLAP プラグインの embedded GUI が期待どおり開かない・閉じない・リサイズしない等の症状を、
  CLAP 仕様と IPC / スレッド構成に照らして切り分ける手順。
  「プラグイン GUI が表示されない」「ボタンが押せない」「✕ で閉じても状態が残る」「show が
  false を返す」等のとき発動。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(./target/debug/*)
---

# CLAP プラグイン GUI デバッグワークフロー

daw_plugin_host のプラグイン GUI が動かないとき、どの層で止まっているかを切り分ける。

## 主な層

```
daw_gui (UI 側)
  ↓ AppEvent::TogglePluginGui
  ↓ PluginHostWindow::create() → u64 HWND
  ↓ MainToChild::OpenGuiEmbedded { host_hwnd } ──▶
                                                  daw_plugin_host (tokio recv)
                                                  ↓ PluginCommand::OpenGuiEmbedded
                                                  ↓ (mpsc + PostThreadMessage で起こす)
                                                  ↓                     plugin-main std::thread
                                                  ↓                     ├ gui_create_embedded
                                                  ↓                     ├ gui_set_scale(1.0)
                                                  ↓                     ├ gui_get_size
                                                  ↓                     ├ gui_set_parent_hwnd
                                                  ↓                     ├ pump_pending_messages
                                                  ↓                     └ gui_show
                                                  ↓ ChildToMain::GuiOpened { width, height } ◀──
                                                  ↓ (incoming_rx → AppEvent::GuiOpenedFromChild)
daw_gui: container.set_client_size(w, h)
```

## 典型的な症状と切り分け

### ① ボタンを押しても何も起きない

- daw_gui 側が `OpenGuiEmbedded` を送っているか? → `tracing::info!` を `toggle_plugin_gui` 冒頭に
- `sending to plugin_host msg=OpenGuiEmbedded { host_hwnd: ... }` がログに出るか
- 出ている場合: daw_plugin_host の `received OpenGuiEmbedded` が出ているか
  - 出ていない → IPC 切れ。`pipe_loop` のエラーログを見る
- plugin-main スレッドが `PluginCommand` を受けているか
  - `tracing::debug!` を plugin_main_loop の drain ループに一時的に挿入
- `plugin_host` の `received OpenGuiEmbedded` は見えるが open_gui が呼ばれない
  → `PostThreadMessageW` が届かない可能性。`thread_id` が 0 になっていないか確認

### ② `gui.show returned false`

1. CLAP spec の呼び出し順序を守っているか確認（create → set_scale → can_resize → get_size →
   set_parent → show）。特に `set_size` を初回 open で呼んでいないか(呼ぶと VCV Rack 等が拒否)
2. `set_parent` と `show` の間にメッセージポンプ(`PeekMessage` で 1 周)が入っているか
3. それでも false を返す場合: CLAP spec 的には失敗を意味するが、実際には GUI が表示されて
   いる例 (VCV Rack) がある。`Ok(false)` を返して警告ログに留めるのが best practice
4. 本当に破綻している場合: プラグインが host 側に要求する追加拡張があるかもしれない
   (`clap_host_log`, `clap_host_thread_check` など)。Host の `get_extension` に拡張を足すと
   プラグインの診断ログが読めることがある

### ③ ✕ ボタンを押しても `is_gui_open` が残る / 次のクリックが効かない

- WNDPROC の `WM_CLOSE` が `DefWindowProcW` に流れると Windows がデフォルトで `DestroyWindow`
  してしまい、Rust 側の RAII Drop と二重解放になる。必ず `ShowWindow(SW_HIDE)` + `LRESULT(0)`
  で傍受する
- WNDPROC から Rust state に触るのが難しい → `GWLP_USERDATA` に `Arc::into_raw` で貼り付け、
  Drop で `Arc::from_raw` で回収する close-request フラグパターンが定石
- 30Hz tick で `take_close_request()` を読み、`is_gui_open && flag` なら `CloseGui` IPC を送る
- 併せて plugin_host の `CloseGui` ハンドラは **無条件で** `ChildToMain::GuiClosed` を返すこと。
  `gui_open` 内部フラグが false のときにも ack を返さないと、daw_gui の UI が永続「open」状態に

### ④ リサイズが反映されない / 画面の一部しか見えない

- コンテナの *client* 領域が plugin の希望サイズと合っているか
- `SetWindowPos` に渡すのは **outer rect**（タイトルバー・境界線込み）。client size から
  `AdjustWindowRectEx(WS_OVERLAPPEDWINDOW, false, 0)` で変換する
- 動的リサイズの場合:
  - plugin → host: `clap_host_gui.request_resize` コールバックが来る → 送信端 (`Arc<dyn Fn>`) を経由して
    `PluginEvent::GuiRequestResize` → IPC → AppEvent → container リサイズ → `MainToChild::ResizeGui`
    → plugin.gui_set_size で plugin 側に確定通知

### ⑤ プラグインが panic / 不正終了 / ハング

- すべての CLAP 呼び出しが **plugin-main std::thread 上で直列化** されているか
  - tokio multi-thread ランタイムに混ぜるとスレッド ID がブレてプラグインが検出するケースあり
- `clap_host_thread_check` 拡張を実装すれば、プラグインの側から「main-thread で呼ばれているか」を
  検証できるようにしてくれる。疑わしい場合は実装
- audio thread と main thread が **同じ Plugin ポインタ** を見るのは spec 上 OK だが、
  GUI 呼び出し中に同時に process() が走っているとプラグイン実装のバグで落ちる例あり。
  疑わしければ GUI 呼び出し時は playback を stop してから触る

## デバッグ用ログ追加のコツ

- `tracing::info!` で IPC 送受信をすべてログ化すると、どこで止まっているか一目で分かる
  - daw_gui → plugin_host: `sending to plugin_host msg=...`
  - plugin_host 受信: `received <CommandName>`
  - plugin_host → daw_gui: `received <ChildToMain 変種>`
- プラグイン本体が stderr に出すログも見る（CLAP プラグインは printf / fprintf 経由で
  デバッグ情報を吐く実装が多い。VCV Rack の `guiSetParent()` / `guiDestroy()` など)
- Windows メッセージ経由の問題を疑うときは `WM_*` 名を含めてログを出すとすぐ分かる

## 参考ファイル

- `F:\dev\daw_01\daw_plugin_host\src\main.rs` — plugin-main thread / open_gui / close_gui
- `F:\dev\daw_01\daw_plugin_host\src\plugin.rs` — gui_* メソッド群
- `F:\dev\daw_01\daw_plugin_host\src\clap_host.rs` — `clap_host_gui` 実装
- `F:\dev\daw_01\daw_gui\src\view\plugin_embed.rs` — PluginHostWindow
- `F:\dev\daw_01\daw_gui\src\app.rs` — AppEvent / AppData の gui 関連ハンドラ
- CLAP 仕様: `/tmp/clap/include/clap/ext/gui.h` の先頭コメントに embedded GUI の正典手順
