<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: plugin editor を plugin-host プロセス所有のトップレベル窓へ移す (FIXME #31)

## 背景 / 根本原因 (一次情報で検証済み, 確度: 高)

Scaler 2 は JUCE 製。JUCE のカスケード(2段目)サブメニューは `PopupMenu::Options::forSubmenu()`
で生成され、その際 `targetComponent = nullptr` になる (`juce_PopupMenu.cpp:2180`)。各メニュー窓は
~20Hz の `checkButtonState` で `doesAnyJuceCompHaveFocus()` が false なら ~10ms 以内に自分を閉じる。
判定は:

```
doesAnyJuceCompHaveFocus()
 → isForegroundOrEmbeddedProcess(componentAttachedTo)
     = Process::isForegroundProcess()                       // bare check
       || isEmbeddedInForegroundProcess(componentAttachedTo) // JUCE #401 の救済
```

- **1段目メニュー**: `componentAttachedTo` = 実エディタ → `isEmbeddedInForegroundProcess` が
  `GetAncestor(editorHwnd, GA_ROOTOWNER)` をプロセス境界越しに辿り daw_gui のコンテナ HWND
  (= foreground window) に到達、PID 一致 → 通る (JUCE #401 fix)。→ 1段目は開く。
- **2段目 (カスケード)**: `componentAttachedTo == nullptr` → `isEmbeddedInForegroundProcess(nullptr)`
  は即 false (null ガード, `juce_Windowing_windows.cpp:5566`) → `Process::isForegroundProcess()`
  = `getProcess(GetForegroundWindow()) == GetCurrentProcessId()` のみに落ちる。
  daw_01 では foreground window は **プロセスA (daw_gui)**、メニューを動かすのは
  **プロセスB (daw_plugin_host)** → PID 不一致 → false → サブメニューが出た瞬間に dismiss。

これが「1段目は出るがカスケードだけ開かない」症状の正体 (JUCE 自身のソースに内在する非対称性)。

棄却した対案:
- `AttachThreadInput` ❌ focus/active/capture は共有するが foreground window の所有プロセスは
  変えられない (`GetForegroundWindow()` はシステム全体で 1 つ)。サブメニューが読む唯一の状態を
  動かせない。IME/focus 退行と deadlock リスクのみ。
- DPI/座標ずれ ❌、メッセージポンプ枯渇 ❌ (同じ 20Hz タイマーが 1段目を維持している事実が反証)。
- プラグイン側 LookAndFeel 修正 ❌ Scaler のソースは触れない。

参照: JUCE #401, free-audio/clap discussions#331 (baconpaul: トップレベル pop-out 窓方式 =
Bespoke/Logic/Live)。

## 修正方針 (理想アーキテクチャ)

サブメニューのゲートが読むのは「foreground window をどのプロセスが所有しているか」だけ。よって
**プラグインエディタのトップレベル窓を plugin-host プロセス (B) 側が plugin-main スレッド上で所有**
するよう作り直す。これで `GA_ROOTOWNER` がプロセスB に解決する。

**鍵となる自然な挙動**: メニューを開くにはユーザーがエディタ内をクリックする必要があり、その
クリックはエディタのルート窓 (B 所有) をアクティブ化 → **プロセスB が foreground process になる**
→ B 内で `Process::isForegroundProcess()` が true → 1段目もカスケードも成立する。よって
`SetForegroundWindow` / `AllowSetForegroundWindow` は「開いた瞬間に前面へ出す」ための polish で
あって、メニュー修正の必須要件ではない。

副次効果: クロスプロセス親子に伴うキーボードフォーカスの脆弱性も解消。

## 現状 (修正前)

- daw_gui (A): `PluginHostWindow` (WS_OVERLAPPEDWINDOW トップレベル) を作り HWND を IPC で B へ。
  `plugin_host_windows: HashMap<(track,slot), PluginHostWindow>` で所有。✕ は WNDPROC が flag →
  tick が poll → `CloseSlotGui` IPC。resize は `SlotGuiRequestResize`(B→A) → A が窓を resize →
  `ResizeSlotGui`(A→B) → onSize。
- daw_plugin_host (B): plugin-main スレッドで `IPlugView::attached(host_hwnd, HWND)` /
  CLAP `gui.set_parent(host_hwnd)`。プラグインの editor は A のコンテナの WS_CHILD。

## 変更内容

### common/protocol.rs
- `MainToChild::OpenSlotGuiEmbedded { track, slot, host_hwnd }` → `{ track, slot, title: String }`。
  `host_hwnd` 廃止 (もうクロスプロセス parent にしない)。`title` で窓キャプション。
- resize は B 内で完結させるため `ChildToMain::SlotGuiRequestResize` と
  `MainToChild::ResizeSlotGui` を **削除** (round-trip 廃止)。各 match site を更新。

### daw_plugin_host (新規 `editor_window.rs` + main.rs 配線)
- `editor_window.rs`: B 所有のトップレベル窓 (plugin_embed.rs の B 版)。
  window class 登録、WNDPROC は WM_CLOSE で close フラグを立てて hide (DefWindowProc しない)、
  `EditorWindow { hwnd, state }`、`set_client_size` / `take_close_request` / `destroy` (DestroyWindow)。
  GWLP_USERDATA に (close_requested AtomicBool) を leak で貼り Drop/destroy で回収。
- plugin_main_loop:
  - `editor_windows: HashMap<(track,slot), EditorWindow>` を持つ。
  - `gui_resize` チャネル (`mpsc<(track,slot,w,h)>`) を持ち、`on_request_resize` callback が送る。
  - `open_gui`: gui_create_embedded → gui_get_size → **EditorWindow::create(size, title)** →
    gui_set_parent_hwnd(editor.hwnd) → gui_show → editor.set_client_size(size) →
    SetForegroundWindow(editor.hwnd)。editor_windows に挿入。SlotGuiOpened を返す。
  - 毎 outer iteration (cmd drain 後 / GetMessageW 前): close poll (editor の close フラグを見て
    plugin.gui_destroy → editor.destroy → map remove → SlotGuiClosed 送信) と resize drain
    (editor.set_client_size + plugin.gui_set_size = onSize) を実行。
  - `CloseSlotGui` handler: plugin.gui_destroy + editor.destroy + map remove + SlotGuiClosed。
  - RemoveSlotPlugin / RemoveTrack / SetSlotPlugin(置換) / MoveSlot(再キー) / Shutdown:
    対象 (track,slot) の editor_window を destroy (idempotent: map にあれば)。
- vst3_host.rs `Vst3PlugFrame::resizeView`: 既存どおり `on_request_resize` callback を呼ぶ
  (callback の宛先が IPC → gui_resize チャネルに変わるだけ)。
- CLAP は `LoadedPlugin::gui_set_parent_hwnd` 経由で B の窓 hwnd を受けるので追加変更不要。

### daw_gui (app.rs / main.rs / view)
- `plugin_host_windows: HashMap<.., PluginHostWindow>` → `open_plugin_guis: HashSet<(track,slot)>`
  (開いているかの追跡のみ)。
- `open_slot_gui`: 窓生成を廃止。set に挿入 + `OpenSlotGuiEmbedded { track, slot, title }` 送信。
- `on_gui_opened`: no-op (B が自分の窓を sizing する)。
- `on_gui_request_resize` / `AppEvent::GuiRequestResizeFromChild` / main.rs の
  `SlotGuiRequestResize` 経路: **削除** (B 内完結)。
- tick の `take_close_request` poll: **削除** (close は B 発、`SlotGuiClosed` イベントで届く)。
- `GuiClosedFromChild` handler: `open_plugin_guis` から remove。
- `cleanup_slot_gui`: 窓 drop → `CloseSlotGui` IPC + set から remove。`shift_slot_gui_keys` は
  set 上で同様に再キー。
- `toggle_slot_gui`: `contains` で open 判定。
- reconcile / remove_track の `plugin_host_windows.retain` ×2 → `open_plugin_guis.retain`
  (窓破棄は B 側 RemoveTrack/RemoveSlotPlugin が行う)。
- `view/plugin_embed.rs` と `view/mod.rs` の `pub mod plugin_embed;`: **削除**。
- 子プロセス spawn 時の PID を保持して `AllowSetForegroundWindow(child_pid)` を open 直前に呼ぶ
  (polish; 必須でない。"started by foreground process" 条件でも SetForegroundWindow は通る)。
  → 初版では省略可。窓が前面に出ない場合のみ追加。

## 検証 (実機, Scaler 2 必須)
1. `cargo build --workspace` (protocol 変更 → 子バイナリ再生成必須)。
2. daw_gui を起動 (二重起動チェック)。track に Scaler 2 (VST3) を instrument で挿入、editor を開く。
3. editor が plugin-host プロセス所有のトップレベル窓として現れることを確認。
4. Scaler 2 の **カスケード(2段)メニュー** をホバー (~150ms, 親項目から外さずに)。
   PASS = サブメニューが開き、カーソルが親/サブ上にある間は開いたまま、子項目をクリックできる。
   FAIL = 出ない / 1フレームで消える。5回以上繰り返す。
5. **キーボードフォーカス退行チェック** (アクティブ化を変えたので必須): editor を閉じ daw_gui
   本体へ戻り、Space=transport / Ctrl+S=save / テキスト欄入力 + IME / hjkl tracker 移動 を確認。
   editor を閉じたら本体が正常に前面へ戻ること。
6. 既存 plugin の退行確認: VCV Rack / MeldaProduction で editor 開閉・リサイズが従来通り。

## 注意 (FFI / window)
- editor 窓は B の plugin-main スレッド (GetMessageW を回すスレッド) で **必ず**作る。別スレッドで
  作るとメッセージが孤立する。
- teardown 順: plugin.gui_destroy (view.removed() = プラグインが子窓を detach) → editor.destroy
  (DestroyWindow 親)。逆順だと子が attached のまま親破棄。
- editor 窓を A の本体窓の owner にしては **いけない** (GA_ROOTOWNER が A に解決して再発)。
  owner なしの独立トップレベルにする。
- 既存 GetMessageW ポンプ / WM_COMMAND_WAKE drain は残す (JUCE async menu が依存)。
- builtin / voicevox は `gui_is_embed_supported()==false` で open_gui が早期 return、窓を作らない。
