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
  → **r.md #65 で更に更新**: `OpenSlotGuiEmbedded` は
  `{ device_id, title, geometry: Option<EditorWindowGeometry> }`、
  `SlotGuiOpened { width, height }` は `SlotGuiGeometry { device_id, geometry }` に置き換え。
  下の「§窓契約」を参照。

### daw_plugin_host (新規 `editor_window.rs` + main.rs 配線)
- `editor_window.rs`: B 所有のトップレベル窓 (plugin_embed.rs の B 版)。
  window class 登録、WNDPROC は WM_CLOSE で close フラグを立てて hide (DefWindowProc しない)、
  `EditorWindow { hwnd, state }`、`set_client_size` / `take_close_request` / `destroy` (DestroyWindow)。
  GWLP_USERDATA に (close_requested AtomicBool) を leak で貼り Drop/destroy で回収。
- plugin_main_loop:
  - `editor_windows: HashMap<(track,slot), EditorWindow>` を持つ (v29 で
    `InstanceRecord.editor` へ統合)。
  - `gui_resize` チャネル (`mpsc<(track,slot,w,h)>`) を持ち、`on_request_resize` callback が送る
    → **r.md #65 で「同期が正、channel は fallback」へ反転** (§窓契約 3)。
  - `open_gui` の順序も r.md #65 で変更 (§窓契約 1)。
  - 毎 outer iteration (cmd drain 後 / GetMessageW 前): close poll (editor の close フラグを見て
    plugin.gui_destroy → editor.destroy → map remove → SlotGuiClosed 送信) と
    geometry poll (§窓契約 5) を実行。
  - `CloseSlotGui` handler: plugin.gui_destroy + editor.destroy + map remove + SlotGuiClosed。
  - RemoveSlotPlugin / RemoveTrack / SetSlotPlugin(置換) / MoveSlot(再キー) / Shutdown:
    対象 (track,slot) の editor_window を destroy (idempotent: map にあれば)。
- vst3_host.rs `Vst3PlugFrame::resizeView`: 既存どおり `on_request_resize` callback を呼ぶ
  (callback の宛先が IPC → gui_resize チャネルに変わるだけ)。
  → **r.md #65 で撤回**。同期処理が正 (§窓契約 3)。
- CLAP は `LoadedPlugin::gui_set_parent_hwnd` 経由で B の窓 hwnd を受けるので追加変更不要。

### daw_gui (app.rs / main.rs / view)
- `plugin_host_windows: HashMap<.., PluginHostWindow>` → `open_plugin_guis: HashSet<(track,slot)>`
  (開いているかの追跡のみ)。
- `open_slot_gui`: 窓生成を廃止。set に挿入 + `OpenSlotGuiEmbedded { track, slot, title }` 送信。
- `on_gui_opened`: no-op (B が自分の窓を sizing する)。
  → **r.md #65 で `on_gui_geometry` に置換** (窓の位置 / サイズを記録して永続化する。§窓契約 5)。
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

---

# §窓契約 (r.md #65) — エディタコンテナ窓が満たすべき Win32 / VST3 / CLAP の契約

2026-08-22 追加。FIXME #31 で窓の**所有者**は正した (= plugin-host が持つ) が、**窓としての
契約**は未実装のままだった。ここがその正本で、`daw_plugin_host/src/editor_window.rs` の
module doc がこの節を指す。

## 0. 症状と根本原因 (実測)

r.md #65 の申告は 2 つ:

- **A. Renoise Redux のエディタ窓がアクティブにならない** (タイトルバーが一瞬アクティブ色に
  なってすぐ戻る / 毎回ではない)
- **B. リサイズが正常にできない**

`daw_plugin_host --editor-selftest "<path>.vst3"` (= daw_gui を起動せずエディタ窓だけを開いて
窓メッセージを採る使い捨てモード) と `EnumChildWindows` の外部プローブで、**A と B が同じ
根本原因**であることを確認した:

1. `Vst3PlugFrame::resizeView` が窓も直さず `onSize` も呼ばずに `kResultOk` を返していた
   (実処理は channel 経由で plugin-main の次周回)。これは `iplugview.h` の
   *"Afterwards, **in the same callstack**, the host has to call IPlugView::onSize ()"* 違反。
2. Redux は自分の内部 "Editor" ビューを開くときコンテナより大きい領域 (880x162 → 1105x687)
   を要求する。ホストが同期で応えないので、**自分の view 窓をコンテナから切り離して
   `WS_CHILD` → `WS_POPUP` の owned top-level に作り替える**
   (実測: style `0x54000000` → `0x90000000`、`GetParent` はコンテナ = owner のまま、
   `EnumChildWindows` からは消える)。
3. その結果コンテナは**空の枠**になり、タイトルバーをクリックすると Windows は
   「owner をアクティブ化 → 直後に owned popup をアクティブ化」を行う。実測ログでは
   `WM_NCACTIVATE(TRUE)` の **0.25ms 後**に `WM_NCACTIVATE(FALSE)` が来る = 症状 A の
   「一瞬濃くなってすぐ薄く戻る」そのもの。Redux の Editor を開いていない間は
   `WM_NCACTIVATE(TRUE)` の後に何も来ない (= 症状が出ない) ので「毎回ではない」も一致する。

さらに独立した欠陥として:

4. **ユーザー起点リサイズの経路が丸ごと無かった** (`WM_SIZING` / `WM_SIZE` の実装が無く
   `DefWindowProc` 直行)。枠だけ伸びて中身が追従しない = 症状 B の literal な説明。
5. **`canResize()==false` を無視して `WS_OVERLAPPEDWINDOW` (= `WS_THICKFRAME` +
   `WS_MAXIMIZEBOX`) を出していた**。Redux は false を返すので、本来この窓は固定枠。
6. **フォーカスを子窓へ渡していなかった** (`SetFocus` の呼び出しがプロセス全体で 0 件)。
   `DefWindowProc` は `WM_ACTIVATE` でフォーカスを *アクティブ化された窓自身* に置くので、
   コンテナがフォーカスを持ったまま = 打鍵が `editor_window.rs` の relay に積まれ
   `main.rs::handle_editor_key` が転送対象外を捨てる = **キーが 1 つも通らない**。
7. **`gui_set_size` がプラグイン起点 resize にまで `checkSizeConstraint` を掛けていた**。
   実ログで `IPlugView::onSize -> 0x1` の WARN が全 VST3 に出ていた原因
   (`kResultFalse` を `ensure!` で hard error 扱いしていた)。
8. **窓の位置 / サイズが永続化されていなかった** (`on_gui_opened` が空実装、座標は
   `(120,120)` 決め打ち)。

## 1. 窓スタイルと open シーケンス

    style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN | WS_CLIPSIBLINGS
          + (resizable ? WS_THICKFRAME | WS_MAXIMIZEBOX : 0)
    class = { style: CS_DBLCLKS, hCursor: IDC_ARROW, hbrBackground: NULL }
            + WM_ERASEBKGND -> TRUE / WM_PAINT -> BeginPaint+EndPaint

VST3 SDK editorhost (`samples/vst-hosting/editorhost/source/platform/win32/window.cpp`
L79-131) と clap-wrapper standalone が独立に同じ結論。`CS_HREDRAW|CS_VREDRAW` は**付けない**
(リサイズのたび親の client 全域が無効化され、子の上でちらつく)。

open シーケンス (`PluginHost::open_gui`) — **窓は隠したまま作り、最後に 1 回だけ見せる**:

    gui_create_embedded()            (VST3: createView + setFrame / CLAP: gui.create)
      -> gui_sizer().can_resize()    pre-attach の可否
      -> gui_get_size()              初期サイズ
      -> EditorWindow::create(..., resizable, saved_position)   <- WS_VISIBLE 無し
      -> editor.attach_sizer(...)    ★ attach より前 (attached の中から resizeView が来る)
      -> gui_set_scale(dpi)
      -> gui_set_parent_hwnd(hwnd)   (VST3: attached / CLAP: set_parent)
      -> pump_pending_messages()
      -> gui_show()
      -> can_resize() 再 query -> 変わっていれば set_resizable()   ★ attach 後の値が正
      -> set_client_size(saved か plugin の値)
      -> show_and_focus()            <- SWP_SHOWWINDOW + SetForegroundWindow を **ここ 1 回だけ**

**activation を 1 回にまとめるのが要点**。旧実装は `create` の中で即 `ShowWindow(SW_SHOW)` して
おり、(a) 中身の無い枠が一瞬見え、(b) その表示で得た activation を attach 中にプラグインが作る
一時 top-level に奪われ、(c) 最後の `SetForegroundWindow` は
`AllowSetForegroundWindow` の**ワンショット許可**を使い切った後なので拒否され得た。
MSDN: *"The process specified by dwProcessId ... **loses the ability to set the foreground
window the next time that either the user generates input**, unless the input is directed at
that process"*。`SetForegroundWindow` の戻り値は必ずログする (false = OS の foreground 制限)。

`set_resizable` の後処理は MS doc が名指しした
`SetWindowPos(SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_FRAMECHANGED)` + client サイズの作り直し。

## 2. ホスト起点リサイズ (ユーザーのドラッグ)

    WM_SIZING  -> drag rect (window/screen) から枠厚を引いて client に落とす
                  -> checkSizeConstraint / adjust_size で矯正
                  -> CLAP get_resize_hints の軸固定 + アスペクト比
                  -> wParam (WMSZ_*) を見て **掴んでいない辺を固定**して書き戻す -> return TRUE
    [OS が窓をリサイズ]
    WM_SIZE    -> client サイズを onSize / set_size で通知
                  ただし **プラグインの現在サイズと違うときだけ** (feedback loop 防止)

矯正は `WM_SIZING` だけ、通知は `WM_SIZE` だけ。VST3 dev portal の "Initiated from Host" 図と
`clap/ext/gui.h` L41-45 が一致している。

- `checkSizeConstraint` の**戻り値では分岐しない**。ヘッダは戻り値を規定しておらず
  (規定は「不可なら rect を許容サイズへ直す」だけ)、dev portal は実プラグインが
  `kResultTrue (always)` を返すと注記している。JUCE と同じく**呼び出し前後の rect 比較**で
  採用可否を決める。editorhost の `!= kResultTrue` 条件をそのまま写すと丸めが常に捨てられる。
- CLAP `adjust_size` は逆に *"Returns true if the plugin could adjust the given size."* と
  戻り値が明示されているので、そちらは戻り値で分岐してよい。
- `WM_GETMINMAXINFO` は**実装しない** (editorhost も扱わない)。最小サイズを問い合わせる API が
  どちらのフォーマットにも無く、`constrain(1,1)` は仕様に無い推測になる。`WM_SIZING` が
  ドラッグの下限を担うので機能的な穴は無い。

## 3. プラグイン起点リサイズ (`resizeView` / `request_resize`)

`editor_window::plugin_requested_resize(hwnd, w, h)` が **同じコールスタックで**:

    再入中なら false (VST3 は kResultFalse)
    現在サイズと同じなら true (onSize も呼ばない)
    窓をリサイズ -> 同期の WM_SIZE -> onSize / set_size
    それでも合わなければ保険で直接 notify

**`checkSizeConstraint` / `adjust_size` は掛けない** — ヘッダにも dev portal の図にも
editorhost / JUCE / clap-host / clap-wrapper の実装にも、プラグイン起点フローでの矯正は
登場しない (矯正はホスト起点ドラッグ専用)。掛けると要求値と `onSize` 引数がずれる。

窓が無い / 別スレッドからの `request_resize` (CLAP は `[thread-safe]`) は
`HostCallbacks::on_request_resize` → `HostNotify::Resize` の**非同期フォールバック**へ落とし、
plugin-main の周回で **同じ `plugin_requested_resize` へ合流**させる (実装は 1 本 = SSoT)。

窓の HWND は `HostCallbacks::editor_hwnd: Arc<AtomicU64>` で公開する。書き込むのは
`gui_set_parent_hwnd` / `gui_destroy` の 1 対だけ (= 「今どの窓に attach しているか」がそのまま値)。

## 4. フォーカス転送と `EditorSizer` の所有

- `WM_ACTIVATE` で子へ `SetFocus` (Raymond Chen 2014-05-21 の正準パターン)。非アクティブ化で
  「どの子がフォーカスを持っていたか」を覚え、再アクティブ化で戻す。`GetFocus() != target` の
  ガードで子が親へ返し送りする場合の往復を 1 回で断つ。**`WM_KILLFOCUS` の中では触らない**
  (MSDN が明示的に禁止: *"do not make any function calls that display or activate a window"*)。
- `EditorSizer` (`plugin_instance.rs`) は WNDPROC がプラグインを同期で叩くための口。実装は
  **borrowed な FFI ポインタしか持たない**。view / plugin instance の所有は `LoadedPlugin` 側
  1 箇所のまま (ComPtr を二重 AddRef して WNDPROC にも持たせると `removed()` と競合して UAF)。
  `gui_destroy` は **先頭で** `alive` を落とす契約で、以後 `is_alive()` が false になり
  nested dispatch の再入も塞ぐ。
- `gui_destroy` は editorhost と同じく **`setFrame(nullptr)` → `removed()`** の順。

## 5. ジオメトリの永続化

`PluginEvent::SlotGuiGeometry { device_id, geometry }` を open 直後 / `WM_EXITSIZEMOVE` /
`WM_MOVE` / プラグイン起点 resize 完了 / close 直前に送る (ドラッグ**中**は送らない)。
daw_gui は `ui_prefs.plugin_editor_windows` に貯め、`ViewState.plugin_editor_windows` として
プロジェクトに保存する。**`Song` ではなく `ViewState`** = 窓の位置は「見方の都合」なので
動かしても `*` は付かない (memory `project_dirty_flag_rule`)。key は安定 id
(`PluginInstance.id`)、save 時に現存 device の分だけへ GC。

復元は `OpenSlotGuiEmbedded { geometry }`。**位置は常に**復元し (画面外なら既定位置へ)、
**サイズは resizable のときだけ** (CLAP `gui.h` の手順 9 と同じ規約 — 固定サイズ GUI に
前回のサイズを押し付けない)。

## 6. modal move/size ループ

キャプション / サイズ枠のドラッグ中は `DefWindowProc` の内側の modal ループに入り、
plugin-main の `GetMessageW` ループが止まる (MSDN WM_ENTERSIZEMOVE:
*"The operation is complete when DefWindowProc returns."*)。`WM_ENTERSIZEMOVE` で
`SetTimer(hwnd, .., 16ms)` し、`WM_TIMER` から `crate::pump_host_during_modal_loop()` を呼ぶ
(窓タイマにするのが必須 — スレッドタイマは `DispatchMessageW` が WNDPROC を呼ばない)。

**回すのは再入して安全な処理だけ** (`PluginHost::pump_modal` = ARA notify + activation 報告)。
command / notify の drain はここでやらない: `CloseSlotGui` / `HostNotify::Closed` は
`DestroyWindow` に至り、いま WNDPROC が走っているその窓を modal ループの内側から壊してしまう。
ドラッグが終われば外側 loop の先頭で即 drain されるので、遅れるだけで落ちるものは無い。

借用は thread-local の `MODAL_PUMP_HOST` (= `DispatchMessageW` の直前に立てて直後に戻す) と
`IN_MODAL_PUMP` の再入ガード。`pump_pending_messages()` (= `open_gui` が `&mut self` を握った
まま呼ぶ) の側では立てないので、そこから WM_TIMER が来ても null で弾かれる。

## 7. 診断

`RUST_LOG=info,editor_win=trace` で WNDPROC が activation / focus / sizing 系メッセージを
**相手 HWND の PID / TID / クラス名 / タイトル / style 付き**で出す。無効時は `tracing::enabled!`
で早期 return するので HWND 問い合わせも走らない。

`daw_plugin_host --editor-selftest "<path>.vst3" [plugin_id] [seconds]` は daw_gui を
**一切起動せず**にエディタ窓だけを開いて上記トレースを採る使い捨てモード
(`--ara-selftest` と同じ方式)。daw_gui が絡むかどうかの二分に使う。

## 8. 残課題 (別項目)

- **DPI**: daw_plugin_host はマニフェストも `SetProcessDpiAwarenessContext` も持たず
  **DPI unaware**。`window_dpi_scale` は常に 1.0 を返し、`gui_set_scale` の分岐が実質無効。
  per-monitor v2 + `WM_DPICHANGED` / `WM_GETDPISCALEDSIZE` (editorhost `window.cpp` L185-221 が
  正準) まで一式が要るので #65 とは分ける。client⇄window 変換は既に
  `AdjustWindowRectExForDpi` に一本化済みなので、awareness を上げても壊れない。
- **プラグインが view を owned popup へ逃がす挙動そのもの** (Redux の内部 Editor)。同期
  resize で必要性は消えるはずだが、プラグイン側の実装次第で残る可能性がある。残る場合、
  コンテナが空になったことを検出して窓を隠す / 追従する等の対処が要るかを実機で見てから決める。
