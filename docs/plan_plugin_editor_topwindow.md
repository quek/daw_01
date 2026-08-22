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
- ~~editor 窓を A の本体窓の owner にしては **いけない** (GA_ROOTOWNER が A に解決して再発)。
  owner なしの独立トップレベルにする。~~
  **2026-08-22 撤回。** JUCE のメニューが使う述語は
  `Process::isForegroundProcess() || isEmbeddedInForegroundProcess(c)` で、
  `GA_ROOTOWNER` を見るのは `||` の**右側 = 救済パス**。判定を緩める方向にしか効かないので、
  `GA_ROOTOWNER` が A に解決することが dismiss を引き起こすことは原理的に無い。
  #31 を踏んだ真因は「A が窓を**作って**いた」= 窓が A のプロセスに属し
  `Process::isForegroundProcess()` (前面窓の **プロセス ID** 比較) が false になったこと。
  **editor 窓は B が作り、owner は A の本体窓**にする (r.md #65。REAPER と同じ構成)。
  詳細は CLAUDE.md「プラグインエディタ窓と Win32」節。
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
2. Redux は自分の内部 "Editor" ビューを開くとき、**自分の view 窓の style を
   `WS_CHILD` → `WS_POPUP` に書き換える** (実測: `0x54000000` → `0x80000000`、以後
   `0x90000000` = +`WS_VISIBLE`)。**ただし `SetParent` は呼ばない。**
   結果、`GetParent` / `GA_PARENT` / `GA_ROOT` / `GA_ROOTOWNER` / `GW_OWNER` がすべて
   コンテナを指したまま = **style は popup、階層は依然として子**という中間状態になる
   (`EnumChildWindows` と `IsChild` からは消えるので「逃げた」ように見えるが、逃げていない)。

   > **注意 (2026-08-22 に訂正)**: 以前この項は「ホストが `resizeView` に同期で応えないので
   > Redux が痺れを切らして popup 化した」と書いていた。**実ログで反証済み**。
   > style 反転 `07:57:09.904725` → `resizeView` `07:57:10.015380` で、**反転が 111ms 先行**する。
   > attach から反転までの間に `resizeView` は 1 本も来ていない。反転は Editor ボタンを
   > 押した時点の Redux の無条件動作であって、ホストの応答に対する fallback ではない。
   > 1. の修正は仕様準拠として正しいが、本症状の根治ではない。

3. その結果コンテナは**空の枠**になり、タイトルバーをクリックすると Windows は
   「owner をアクティブ化 → 直後に owned popup をアクティブ化」を行う。実測ログでは
   `WM_NCACTIVATE(TRUE)` の **0.25ms 後**に `WM_NCACTIVATE(FALSE)` が来る = 症状 A の
   「一瞬濃くなってすぐ薄く戻る」そのもの。Redux の Editor を開いていない間は
   `WM_NCACTIVATE(TRUE)` の後に何も来ない (= 症状が出ない) ので「毎回ではない」も一致する。
3b. **残っている症状の本体**は 3 とは別。中間状態の view は `SetForegroundWindow` /
   `GetActiveWindow` の判定を通ってしまう (実測: `fg` も `active` も view) が、Z 順は
   *"A child window is grouped with its parent in z-order."* に従って親リンク = コンテナの
   グループに束縛される。よって **view が前面になってもコンテナごとデスクトップ上に
   浮上しない** (実測: `container_above=Chrome_WidgetWin_1 "YouTube - Vivaldi"`)。
   view はコンテナのクライアント領域にぴったり収まったまま (view `(128,151 1274x639)` /
   container `(120,120 1290x678)`) なので、**見た目の位置関係は壊れておらず、
   失われているのは「コンテナごと前に出ること」だけ**。

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

    style = WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN | WS_CLIPSIBLINGS
          + (resizable      ? WS_THICKFRAME | WS_MAXIMIZEBOX : 0)
          + (owner が取れた ? 0 : WS_MINIMIZEBOX)         <- 1-1 を必ず読むこと
    ex    = owner が取れた ? WS_EX_TOOLWINDOW : 0         <- 1-1 を必ず読むこと
    owner = daw_gui の本体窓 (CreateWindowExW の hWndParent で **作成時に**決める)
    class = { style: CS_DBLCLKS, hCursor: IDC_ARROW, hbrBackground: NULL }
            + WM_ERASEBKGND -> TRUE / WM_PAINT -> BeginPaint+EndPaint

### 1-1. owner と `WS_EX_TOOLWINDOW` は **1 つの判断から導く** (r.md #65)

実装は `editor_window::OwnerBinding` (`OwnedBy(HWND)` / `Standalone`) の 1 型に閉じている。
**この 2 つを別々の条件で分岐させてはいけない。**

|              | owner あり | owner なし |
|--------------|-----------|-----------|
| TOOLWINDOW あり | REAPER と同じ。◎ | **唯一の悪化パターン。作ってはいけない** |
| TOOLWINDOW なし | 安全 (Alt+Tab に残る) | 現状維持。安全 |

**`WS_MINIMIZEBOX` も同じ判断から導く。** tool window は taskbar にも Alt+Tab にも出ないので、
**最小化すると復元する手掛かりが無くなる** — 上の「作ってはいけない」欄と同じ種類の
取り戻せない窓で、しかも **owner を導入したこと自体が新設する経路**。よって
`OwnerBinding::OwnedBy` では `WS_MINIMIZEBOX` を落とす。REAPER の FX 窓も実測で
`WS_MINIMIZEBOX` を持たない (style `0x94cd0044`: `WS_MAXIMIZEBOX` はあるが `WS_MINIMIZEBOX` は無い)。
最大化は ✕ / 復元ボタンで必ず戻せるので残す。
「エディタだけ引っ込める」操作は
*"An owned window is hidden when its owner is minimized."* が肩代わりする
(daw_gui 本体を最小化すれば一緒に隠れる)。

- `WS_EX_TOOLWINDOW` … *"A tool window does not appear in the taskbar or in the dialog that
  appears when the user presses ALT+TAB."* (extended-window-styles)
- owner … *"An owned window is always above its owner in the z-order."* /
  *"The system automatically destroys an owned window when its owner is destroyed."* /
  *"An owned window is hidden when its owner is minimized."* (window-features)

owner が無いまま TOOLWINDOW だけ付けると「他窓の下へ潜れるのに一覧から選べない」=
**取り戻せない窓**になる。逆に owner だけなら潜れないうえ Alt+Tab にも残るので単独で安全。
よって分岐点は「owner が取れたか」だけ。回帰テストは
`owner_binding_never_yields_toolwindow_without_an_owner`。

**owner は `CreateWindowExW` の `hWndParent` で決める。`SetWindowLongPtr(GWLP_HWNDPARENT)` で
後から付け替えない。** Microsoft Learn が
*"After creating an owned window, an application cannot transfer ownership of the window to
another window."* (window-features) と
*"GWLP_HWNDPARENT ... Sets a new owner for a top-level window."* (SetWindowLongPtr) で
正面から矛盾しているため。作成時に渡す形なら前者が明示的に許している唯一の作り方になる。

owner の HWND は `PluginCommand::OpenSlotGuiEmbedded.owner_main_window`
(`common::protocol::PlatformWindowHandle`) で daw_gui から渡る。**preview 窓ではなく本体窓**。
`None` / 既に破棄済みなら `Standalone` に落ちる (= TOOLWINDOW も付かない)。

`WS_EX_TOOLWINDOW` はキャプションを低くするので、**`AdjustWindowRectExForDpi` にも同じ ex を
渡すこと**。渡さないと client 領域が要求サイズからずれる。

**副作用として新設される経路**: daw_gui が異常終了すると Windows がコンテナを道連れに破棄する。
✕ は押されないので `take_close_request` では拾えない。`poll_editor_close_requests` が
`is_window_alive()` で検出し、**通常と同じ close flow** (`gui_destroy` → drop →
`SlotGuiClosed`) に合流させる (`removed()` を飛ばすと `IPlugView` が attach 済みで残る)。

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

`PluginEvent::SlotGuiGeometry { device_id, geometry }` を open 直後 / **rect が変化したとき** /
close 直前に送る (ドラッグ**中**は poll が回らないので、確定後に 1 回だけ届く)。

### 5.1 rect の変化を漏れなく捕捉する不変条件

`geometry_dirty` を立てるのは **`WM_MOVE` と `WM_SIZE` の 2 箇所だけ**。漏れが無いことは
ジェスチャを数え上げるのではなく Win32 の構造から出る:

- 窓の rect が変わる唯一の入口は `WM_WINDOWPOSCHANGED` で、その既定処理
  (`DefWindowProc`) が **位置が変われば `WM_MOVE`、サイズが変われば `WM_SIZE`** を送る。
- このコンテナ窓は `WM_WINDOWPOSCHANGED` を**自前で処理していない** (トレースだけして
  `DefWindowProcW` に落とす) ので、その既定処理は必ず走る。

よって「rect が変わった ⟹ `WM_MOVE` か `WM_SIZE` の少なくとも一方が届く」。個別に挙げると:

| 経路 | 通るメッセージ |
|---|---|
| キャプションのドラッグ (移動) | `WM_MOVE` |
| サイズ枠のドラッグ | `WM_SIZE` (+ 上/左辺なら `WM_MOVE`) |
| 最大化 / 復元 / Aero Snap | `WM_SIZE` (+ 位置が変われば `WM_MOVE`) |
| 枠の縦最大化 (上下端ダブルクリック) | `WM_SIZE` のみ ← **旧実装が取りこぼしていた形** |
| プラグイン起点 resize (`resizeView` / `request_resize`) | `SetWindowPos` → `WM_SIZE` |
| `set_resizable` のスタイル貼り替え後の外形再構築 | `SetWindowPos` → `WM_SIZE` |
| 最小化からの復元 | `WM_SIZE` + `WM_MOVE` |
| DPI 変更 (awareness を上げた後の `WM_DPICHANGED`) | `SetWindowPos` → `WM_MOVE` + `WM_SIZE` |

逆に **`WM_EXITSIZEMOVE` では立てない**。ドラッグで動いたなら上の 2 つが既に立てているし、
動いていないなら送るものが無い。ジェスチャ単位のトリガを足すと「どのジェスチャを拾い忘れたか」を
数え続けることになる。同じ理由で `plugin_requested_resize` の末尾でも立てない。

意図的に捨てているものが 2 つ:
- **最小化中** (`(-32000,-32000)` / `0×0`) は `persistable_geometry()` が弾く。
- `CreateWindowExW` の内側で来る `WM_SIZE` / `WM_MOVE` は `GWLP_USERDATA` 未設定で
  `shared_of` が `None` を返す (open 時は `open_gui` が明示的に 1 回送り、その際
  `take_geometry_change()` で dirty を食べて二重送信を避ける)。

**プロセス終了時 (`teardown_device`) では emit しない**。上の 3 点で全ての変化が既に届いて
おり、終了時の emit は 4 つ目の重複になるうえ、daw_gui は `Shutdown` を送った後 drain に
入っているので**届く保証が無い** (「たまに効く」経路を足すことになる)。
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
