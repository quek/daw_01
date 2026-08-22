<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# 終了シーケンス (r.md #61)

対象: 「Alt-F4 で閉じたときもプラグインのアンロードなど行っていますか。」

答え = **行っていなかった**。本書はその修正の設計正本。

---

## 1. 修正前に何が起きていたか

### 1.1 終了経路は 1 本しかなく、そのどれもが後始末をしていなかった

`WindowEvent::CloseRequested` が唯一の終了トリガで、winit 0.30.13 の win32
backend は `WM_CLOSE` をここに畳む。つまり **✕ / Alt+F4 / システムメニュー /
タスクバー右クリックは完全に同じ経路**で、Alt+F4 固有の抜けは存在しなかった
(抜けは全経路共通)。File メニューにも「終了」は無く、終了ショートカットも無かった。

正常終了でやっていたことの全量:

| やっていた | やっていなかった |
|---|---|
| 未保存確認ダイアログ | 子プロセスへの終了通知 (protocol に variant 自体が無い) |
| `window_state.json` 保存 | transport の停止 |
| recovery ファイル削除 | プラグインの `deactivate` / `destroy` |
| | プラグインエディタ窓の破棄 |
| | CPAL stream の停止 (デバイス解放) |

### 1.2 実際に子を殺していたのは Job Object

`run_runner` から戻った後の `drop(bootstrap)` で `Arc<JobHandle>` の最後の 1 本が
落ち、`CloseHandle` が `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` を発火して
**全子プロセスが `TerminateProcess`** される。強制 kill が例外経路ではなく
**正規経路**になっていた。

しかも「最後の 1 本」は `Bootstrap.job` / `ChildSupervisor.job` /
`Win32JobDispatcher` (→ `AppData.voicevox.voicevox_job`) の 3 owner に散っており、
「いつ子が死ぬか」が `Arc` の refcount という暗黙知に依存していた。

`daw_plugin_host` 側には **正しい graceful shutdown コードが存在した**
(pipe EOF → `plugin_thread.shutdown()`) が、親が一切待たないので kill が先に来る。

実測ログ (`%LOCALAPPDATA%\daw_01\logs` 全期間):

| ログ行 | 回数 |
|---|---|
| `daw_plugin_host shutting down` | 612 |
| `plugin-main thread exiting` | 11 |
| `daw_plugin_host exiting` | **1** |

決定的証拠は 2026-08-16 11:27:44 の MTurboComp のケース: 同一ミリ秒の
`plugin worker exiting` 31 行は flush されているのに `VST3 plugin destroyed` だけが
無い (= 非同期 appender のバッファ落ちでは説明できない)。
`"plugin pipe loop ended"` → `"daw_gui exiting"` は **2.116ms**。プラグイン 0 個の
セッションですら 91% がレースに負けていた = kill window はサブミリ秒。

### 1.3 graceful path 自体も CLAP spec 違反だった

仮に kill が間に合わなくても、`PluginHost::shutdown` は `pool.shutdown()` →
各 plugin に `gui_destroy()` → `instances.clear()` しかせず、
**`stop_processing` / `deactivate` を一度も呼ばなかった**。
`ClapPlugin::drop` は ARA session がある場合しか `deactivate` しないので、
通常の CLAP プラグインは **active のまま `clap_plugin.destroy`** に入っていた。

> CLAP `plugin.h`: `destroy` は `[main-thread & !active]`、
> "Free the plugin and its resources. It is required to deactivate the plugin
> prior to this call."
> <https://raw.githubusercontent.com/free-audio/clap/main/include/clap/plugin.h>

VST3 は `Vst3Plugin::drop` が `view.removed()` → `setProcessing(0)` →
`setActive(0)` → `terminate()` を防御的に実行するので、spec 違反は **CLAP 限定**
だった (プロセスが kill される以上どちらも走らないという点は同じ)。

### 1.4 daw_audio の欠陥は別物

daw_audio は kill されておらず、毎回 `main` の末尾 (`daw_audio exiting`) まで
到達していた。欠陥は **`Park = Arc<Mutex<ParkDriver>>` を notify thread
(脱出条件の無い `loop`) が永久保持する Arc リーク**で、そのため
`cpal::Stream::drop` (= WASAPI デバイスの解放) が **原理的に走らなかった**。
`recv_loop` を break するだけでは直らない。

---

## 2. 理想形

**すべての終了経路が 1 つのシーケンスに合流し、子プロセスは自分で正しく死ぬ。**
Job Object は crash / hang / 期限超過の **backstop** に格下げする (撤去はしない
— `std::mem::forget` されていた VOICEVOX engine の最終的な保険でもある)。

```
✕ / Alt+F4 / システムメニュー / タスクバー   ─┐
File > 終了                                  ─┤
Ctrl+Q                                       ─┼─→ AppData::request_quit(QuitRequest)
--smoke-test の判定完了 (終了コード付き)     ─┤        │
Windows のサインアウト / シャットダウン      ─┘        │ 未保存なら確認モーダル
                                                       ↓
                                              AppData::begin_shutdown
                                                       │
   ┌───────────────────────────────────────────────────┴────────────┐
   │ 1. CancelExport      走行中の freewheel を止める               │
   │ 2. Stop              process() を回している最中に壊さない      │
   │ 3. PluginCommand::Shutdown   全 device を teardown → pool 停止 │
   │ 4. AudioCommand::Shutdown    stream pause+drop → exit          │
   │ 5. VOICEVOX engine kill (我々が spawn したものだけ)            │
   │ 6. 開いている picker / help を畳む                             │
   └────────────────────────────────────────────────────────────────┘
                                                       ↓
                          Draining: 子の exit を try_wait で観測 (最大 5s)
                                    UI は「終了処理中…」だけ
                                    **以後 event は全部落とす** (§2.4)
                                                       ↓
                          Finished → recovery ファイル削除 (§2.5)
                                   → event_loop.exit() → Bootstrap::shutdown()
                                                       ↓
                          残っている子を kill → JobHandle::close() (backstop)
```

### 2.1 完了判定はプロセスの exit そのもの

返信 event は使わない。「返事を書けた」と「DLL を unload し終えた」は別の事実で、
欲しい保証は後者だけであり、それを外から確かめられるのは exit しかない。
`tokio::process::Child::try_wait` は同期・非ブロッキング・runtime context 不要
なので GUI メインスレッドから毎フレーム叩ける。

待ちは `DRAIN_TIMEOUT = 5s` で有界 (アーキテクチャ不変条件 4)。超えたら
どの子が残ったかを warn に出して backstop に委ねる。**無限待ちにしない** —
応答しないプラグインの `deactivate` / `FreeLibrary` でアプリの終了が固まっては
いけない。

### 2.2 protocol: 新 variant は「合成」であることを明記する

`PluginCommand::UnloadAllPlugins` は既に「daw_gui の帳簿に依存せず、plugin_host
自身の `instances` を列挙して `teardown_device` に流す」正しい実装を持っている。
`PluginCommand::Shutdown` は **それを置き換えるのではなく合成**する:

```
Shutdown = 全 device の unload (UnloadAllPlugins と同じ teardown_device 経路)
         + worker pool 停止
         + プロセス終了
```

unload の実装は 1 本 (`PluginHost::shutdown` → `teardown_device` →
`teardown_plugin`) しか無い。「全部畳め」のコマンドが 2 つ並ぶが、実装は
共有されているので不変条件 3 (「相手が無視する variant / 重複表現」) に
抵触しない。

`AudioCommand::Shutdown` も同様に「recv_loop を抜けて stream を畳んで exit」だけ。

### 2.3 `Draining` は「アプリが動き続ける窓」なので、event を全部落とす

旧実装は `should_quit` を立てた **同じフレーム**で `exit()` まで走り切っていたので、
「終了を決めた後」という時間が存在しなかった。子の teardown を待つようにすると
0〜5 秒のあいだイベントループが回り続け、そこに副作用が入り込む:

- 「終了処理中…」の下に残った picker のクリックが通る (畳ませた plugin host へ
  `SetSlotPlugin` が飛ぶ)
- 30 秒周期の `AutosaveTick` が recovery ファイルを書き直す

`AppData::handle_event` の冒頭で **`Draining` 中は全 event を捨てる**。
export gate (positive-default + block-list) と違って **allow-list ではなく全遮断**
にできるのは、終了が必ず `DRAIN_TIMEOUT` で終端するから — 「落としすぎて永久ロック」
という export gate の失敗モードが原理的に存在しない。完了判定 (`poll_shutdown`) は
event ではなく `try_wait` で回っている。

### 2.4 recovery ファイルを消すのは「本当に終わる瞬間」

`Draining` の開始時に消すと、teardown に数秒かかる間に OS へ強制終了された場合
「まだ終われていないのに復旧候補だけ消えている」状態になる。`Finished` へ遷移する
`finish_shutdown` で消す (期限超過の backstop 経路もここを通る)。

### 2.5 待ちを二重に張らない

`Bootstrap::shutdown(already_drained)` は、シーケンスが既に `DRAIN_TIMEOUT` ぶん
待ち切っている場合は待ち直さない。待ち直すと応答しないプラグインを抱えた終了が
合計 10 秒になり、しかも後半 5 秒は `event_loop.exit()` の後なので、winit の
`Window::drop` が `PostMessageW` でしか窓を壊せず**メインウィンドウが凍ったまま
画面に残る** (Windows に「応答なし」扱いされる)。待ち直すのは **シーケンスを
通らずに来た経路** (event loop のエラー / `--script` の early return) だけ。

### 2.6 respawn 抑止は必須

子が自力 exit すると pipe は **子側から先に閉じる**ので、daw_gui の reader task が
EOF を拾って crash とまったく同じ `ChildDisconnected` を合成し、
`handle_child_disconnected` が respawn する (= 終了しようとしているのに子が
生き返る)。`tokio::select!` は writer / reader のどちらが先に完了するか非決定
なので、「tx を drop して writer を先に終わらせる」だけでは塞げない。

ガードは 3 段:

1. **発生源**: `ChildSupervisor.shutting_down: Arc<AtomicBool>` を pipe loop と
   共有し、立っていたら `ChildDisconnected` を合成しない。
2. **最終防波堤**: `AppData::handle_child_disconnected` の入口。呼び出し口は 3 つ
   (`AudioEvent::ChildDisconnected` / `PluginEvent::ChildDisconnected` /
   `WorkerPoolStalled` からの合成) あるので、呼び出し側に撒くと必ず漏れる。
3. **respawn の直前でもう一度**。入口のガードは「この関数の実行中に phase は
   変わらない」を前提にしていたが、関数の途中の `abort_state_roundtrip` が
   §3.4 の「終了意図を聞き直す」経路を通って **同期的に** `begin_shutdown` まで
   走り切ることがある。見逃すと、終了シーケンスの直後に新しい plugin host を
   spawn して全プラグインをロードし直し、5 秒後にそれを強制 kill する — 本件で
   消したはずの症状がそのまま再現する。

---

## 3. 同件 (同じ root cause class)

### 3.1 respawn 経路の無条件 `start_kill`

`ChildDisconnected` は「子の死」だけでなく **writer task の死** (`write_msg` 失敗
= 16MB 超 encode 等) でも合成される。後者では子は生きていて active な plugin を
抱えているのに、旧 `spawn_and_handshake_one` は無条件に `start_kill()` していた。
「強制 kill が正規経路になっている」がそのまま当てはまる 2 つ目の箇所。

修正: `retire_child` — pipe が閉じた子に `RETIRE_GRACE = 3s` の猶予を与え
(その間に子は EOF を見て自分で畳む)、期限を過ぎたものだけ kill する。
GUI スレッドは待たない (detached task)。猶予中は新旧の子が一瞬共存するが、
worker pool の named event は世代 bump 済み・shmem 名も incarnation 込みなので
名前空間は衝突しない。

### 3.2 `std::process::exit` によるバイパス

`smoke_test.rs` の 7 箇所が `drop(bootstrap)` を全バイパスしていた
(tracing の `WorkerGuard` flush も飛ぶ)。smoke test は常用の検証経路なので、
ここが終了シーケンスを通らないと「終了時にプラグインを解放できているか」を
smoke で守れない。

修正: 判定結果を `AppEvent::Quit(QuitRequest::automated(code))` に載せて通常の
シーケンスへ入れ、終了コードは `run()` → `run_gui()` → `main()` の
`ExitCode` で返す。watchdog (30s) は終了シーケンスまで含めて覆う backstop に
役割を変える。event loop が既に死んでいる場合だけ従来どおり即 exit する。

なお `main.rs` の `std::process::exit` は `--help` の 1 箇所だけで、これは
bootstrap 前なので問題無い。

### 3.3 VOICEVOX engine 子プロセス

`ensure_voicevox_engine` は `job.assign_std(&child)` の直後に
`std::mem::forget(child)` して handle ごと捨てており、停止手段が Job Object の
`CloseHandle` しか無かった (= 終了シーケンスが engine の停止を所有できない)。

修正: `VoicevoxState.spawned_engine: Arc<Mutex<VoicevoxEngineSlot>>` に保持し、
終了シーケンスが `kill()` + `wait()` する。**ユーザーが自分で立ち上げていた
engine は `child` が `None` のままなので触らない**
(`ensure_voicevox_engine` は `is_running()` が false のときしか spawn しない)。
engine は状態を持たない HTTP サーバで graceful shutdown のエンドポイントを
持たないため、`kill` が正しい終わり方 (プラグインのような「畳む手順」が無い)。

**spawn と停止のレースも塞ぐ**。`is_running()` は localhost:50021 への HTTP GET
(タイムアウト 1 秒) なので、launcher スレッドが待っている間に終了シーケンスが
`JobHandle::close()` まで走り切ることがある。その後に spawn が成功すると
**Job にも入らず kill もされない engine** がポート 50021 と GPU メモリを掴んだまま
残り、次回起動では `is_running()` が true になるので **二度と回収されない**。
`VoicevoxEngineSlot { child, shutting_down }` として、停止側が先に旗を立て、
launcher は spawn 後にそれを見て自分で kill する。`assign_std` が失敗した場合も
握り潰さず kill する (Job に入れられないなら backstop が効かない)。

### 3.4 「保存して終了」の途中で子が落ちたとき終了意図が消えていた

`abort_state_roundtrip` が `guard_after_save` / `guard_pending_action` を
**無条件に捨てて**いたので、warn ログ 1 行だけ残して終了意図が消え、
ユーザーからは「✕ が効かなかった」ようにしか見えなかった。

修正: 破棄系 (New / Open) は従来どおり実行しない (保存が成立していない状態で
project を差し替えると未保存変更を失う) が、**終了は song を触らない**ので、
queue が空になった最新状態でガードをやり直す。これは正常系
(`on_all_states_from_child` 末尾) とまったく同じ扱い。

---

## 4. Windows のセッション終了 (サインアウト / シャットダウン)

winit 0.30.13 は `WM_QUERYENDSESSION` / `WM_ENDSESSION` を **一切扱わない**
ので、OS がセッションを終わらせるとき `CloseRequested` が発火せず、
**未保存確認すら出ないまま殺されていた**。

### 4.1 subclass の方式

`SetWindowLongPtrW(GWLP_WNDPROC)` + `CallWindowProcW`。

- `GWLP_USERDATA` は使えない — winit が自分の `WindowData` ポインタを入れて
  全メッセージ処理に使っており (`WM_NCDESTROY` で 0 に戻す)、奪うと winit が壊れる。
  `daw_plugin_host::editor_window` の idiom (`GWLP_USERDATA` に `Arc` を leak) は
  **自分で `RegisterClassExW` した窓専用**。
- `GWLP_WNDPROC` は winit が触らない (WNDPROC はクラス登録時に固定し、
  `set_window_long` は `GWL_USERDATA` / `GWL_STYLE` のみ) ので競合しない。
- comctl32 の `SetWindowSubclass` でも良いが、`Win32_UI_Shell` feature
  (57k 行) を丸ごと有効化することになるので採らない。
- 失敗判定は `SetLastError(0)` してから呼んで `GetLastError()` を見る
  (`SetWindowLongPtrW` の 0 は「直前の値が 0 だった」かもしれないので、
  戻り値だけでは失敗と断定できない)。

### 4.2 WNDPROC から `AppData` は触れない

`WM_QUERYENDSESSION` は winit の pump の `DispatchMessageW` から **同期に**
呼ばれる。つまり `ApplicationHandler::window_event` のスタックの内側で発火し、
その時点で `RunnerState.app` は上位フレームに `&mut` で借用されている。
よってここから `AppData` には**原理的に触れない**。WNDPROC が持つのは
HWND / `EventLoopProxy` / 未保存かの `AtomicBool` ミラーだけ。

### 4.3 応答 — MSDN の指定どおり

**`WM_QUERYENDSESSION` は即答する。0 (FALSE) は未保存のときだけ返す。**

> Applications should respect the user's intentions and return **TRUE**. …
> Each application should return TRUE or FALSE immediately upon receiving this
> message, and **defer any cleanup operations until it receives the WM_ENDSESSION
> message**.

0 を返すのは重い応答である:

> **If any application returns zero, the session is not ended. The system stops
> sending WM_QUERYENDSESSION messages as soon as one application returns zero.**

つまり FALSE はセッション終了を取り消すだけでなく、**まだ聞かれていない他の
アプリに WM_QUERYENDSESSION が届かなくなる** — 隣で開いている未保存の Word が
保存確認を出す機会を奪う。加えて Vista 以降のガイダンスは
"**Applications should not block shutdown.**" と明示している。

したがって:

| 状態 | 応答 |
|---|---|
| 未保存の変更あり | `ShutdownBlockReasonCreate` 済みの状態で **FALSE**。同時に `AppEvent::Quit` を投げて確認モーダルを出す (ユーザーはシャットダウン画面の「キャンセル」で戻って答えられる) |
| clean | **TRUE**。ユーザーの意図を尊重し、後始末は `WM_ENDSESSION` で |

**`WM_ENDSESSION` でも WNDPROC の中で子プロセスを畳む実装は書かない**。書けば
`AppData` を触れないぶん別実装になり、「終わり方」が 2 つに割れる。
`AppEvent::Quit` を投げて **通常の終了シーケンス** に任せ、即 return する。
その後もイベントループは回り続けるのでシーケンスが完走してプロセスが終わり、
`window_state.json` の保存も通常どおり `Runner::exiting` が担う。Windows は
アプリの exit を待つ (待ちきれなければブロッカー画面を出す = 事実がそのまま
表示されるだけ)。

### 4.4 ブロック理由の登録は WNDPROC の中ではない

> Applications should call this function **as they begin an operation that
> cannot be interrupted**, such as burning a CD or DVD.

daw_01 にとってのそれは「未保存の変更を抱えている」なので、dirty ミラーの更新
(`session_end::set_dirty`、runner が毎フレーム呼ぶ) がそのまま
`ShutdownBlockReasonCreate` / `Destroy` の維持になる。WNDPROC の中で登録するのは
仕様の使い方ではない (あちらは即答すべき場所)。

参照:
- <https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-queryendsession>
- <https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-endsession>
- <https://learn.microsoft.com/en-us/windows/win32/shutdown/shutdown-changes-for-windows-vista>
- <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-shutdownblockreasoncreate>

---

## 5. 見せ方

`Draining` の間は「終了処理中…」モーダルを出し、下の UI 操作を全部塞ぐ。
書き出し進捗 (`export_overlay`) と同じ idiom (`close_on_outside_click` /
`close_on_escape` を倒した true modal) だが、**キャンセルできない** — 子は
もう畳み始めていて、途中で「やめた」に戻す方法が無い。

進捗は determinate に作れない (プラグインの `deactivate` に進捗の概念が無い)
ので、**あと何秒で諦めるか** を出す。正確に出せる唯一の量。

無表示で固まって見えるのが最悪 — 実ログに「応答が無いときユーザーが ✕ を
4 回押して強制終了した」記録がある。

---

## 6. 実装地図

| ファイル | 役割 |
|---|---|
| `common/src/protocol.rs` | `AudioCommand::Shutdown` / `PluginCommand::Shutdown` |
| `daw_gui/src/shutdown.rs` | `QuitRequest` / `ShutdownState` (状態機械の SSoT) |
| `daw_gui/src/handler/shutdown.rs` | `request_quit` / `begin_shutdown` / `poll_shutdown` / respawn 抑止 / VOICEVOX 停止 |
| `daw_gui/src/session_end.rs` | `WM_QUERYENDSESSION` / `WM_ENDSESSION` の subclass |
| `daw_gui/src/view/shutdown_overlay.rs` | 「終了処理中…」 |
| `daw_gui/src/view/runner.rs` | `drive_shutdown` (終了に関する唯一の口)、`WaitUntil` 駆動 |
| `daw_gui/src/bootstrap.rs` | `poll_live_children` / `wait_for_children_exit` / `kill_remaining` / `retire_child` / `Bootstrap::shutdown` |
| `daw_gui/src/job.rs` | `JobHandle::close()` (backstop の明示化) |
| `daw_plugin_host/src/main.rs` | `PluginHost::shutdown` を `teardown_device` ループへ、`pipe_loop` の `Shutdown` 分岐 |
| `daw_audio/src/main.rs` | `NotifyThread` (停止フラグ + join)、`ParkDriver::stop_for_shutdown`、`recv_loop` の `Shutdown` 分岐 |

回帰テスト: `daw_gui/tests/app_state/shutdown_sequence.rs`
(送るコマンドと順序 / 未保存ガード / 自動実行の終了コード / 終了中の respawn 抑止)。

---

## 7. 実機で確かめること

`make build` で 3 exe を同時に再生成すること (protocol fingerprint が変わるので、
片方だけ古いと handshake で `FingerprintMismatch` になる)。

1. CLAP / VST3 を数枚 (できれば VCV Rack のような重量級と、同一 `.clap` の
   複数 instance) ロードした状態で ✕ / Alt+F4 / File > 終了 / Ctrl+Q の 4 経路。
   `%LOCALAPPDATA%\daw_01\logs\daw_plugin_host.*` に
   `shutdown: tearing down all devices` → `plugin destroyed` /
   `VST3 plugin destroyed` が device 数ぶん → `plugin-main thread exiting` →
   `daw_plugin_host exiting` が出ること。
2. `daw_audio.*` に `requested audio stream pause for shutdown` →
   `audio notify thread joined` → **`audio stream released`** → `daw_audio exiting`
   が出ること。デバイスが実際に解放された証拠は `audio stream released` の方
   (`pause` はコマンドをキューに積むだけで、実際の `IAudioClient::Stop` は
   cpal の run thread が後から実行する)。
3. `tasklist` で daw_audio / daw_plugin_host / VOICEVOX engine が残っていないこと。
4. `shutdown /s /t 60` → **未保存のとき**だけシャットダウン画面に
   「未保存の変更があります」が出て止まること (保存済みならそのまま進むこと) →
   `shutdown /a` で中止。
5. 未保存の状態で ✕ → 確認モーダル → 「保存して終了」/「保存せず終了」/
   「キャンセル」の 3 分岐。
