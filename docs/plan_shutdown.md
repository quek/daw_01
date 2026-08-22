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
   │ 6. recovery ファイル削除                                       │
   └────────────────────────────────────────────────────────────────┘
                                                       ↓
                          Draining: 子の exit を try_wait で観測 (最大 5s)
                                    UI は「終了処理中…」だけ
                                                       ↓
                          Finished → event_loop.exit() → Bootstrap::shutdown()
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

### 2.3 respawn 抑止は必須

子が自力 exit すると pipe は **子側から先に閉じる**ので、daw_gui の reader task が
EOF を拾って crash とまったく同じ `ChildDisconnected` を合成し、
`handle_child_disconnected` が respawn する (= 終了しようとしているのに子が
生き返る)。`tokio::select!` は writer / reader のどちらが先に完了するか非決定
なので、「tx を drop して writer を先に終わらせる」だけでは塞げない。

ガードは 2 段:

1. **発生源**: `ChildSupervisor.shutting_down: Arc<AtomicBool>` を pipe loop と
   共有し、立っていたら `ChildDisconnected` を合成しない。
2. **最終防波堤**: `AppData::handle_child_disconnected` の入口。呼び出し口は 3 つ
   (`AudioEvent::ChildDisconnected` / `PluginEvent::ChildDisconnected` /
   `WorkerPoolStalled` からの合成) あるので、呼び出し側に撒くと必ず漏れる。

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

修正: `VoicevoxState.spawned_engine: Arc<Mutex<Option<Child>>>` に保持し、
終了シーケンスが `kill()` + `wait()` する。**ユーザーが自分で立ち上げていた
engine は `spawned_engine` が `None` のままなので触らない**
(`ensure_voicevox_engine` は `is_running()` が false のときしか spawn しない)。
engine は状態を持たない HTTP サーバで graceful shutdown のエンドポイントを
持たないため、`kill` が正しい終わり方 (プラグインのような「畳む手順」が無い)。

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
- `GWLP_WNDPROC` は winit が触らない (WNDPROC はクラス登録時に固定、
  `set_window_long` は `GWL_USERDATA` / `GWL_STYLE` のみ) ので競合しない。
- comctl32 の `SetWindowSubclass` でも良いが、`Win32_UI_Shell` feature
  (57k 行) を丸ごと有効化することになるので採らない。

### 4.2 WNDPROC から `AppData` は触れない

`WM_QUERYENDSESSION` は winit の pump の `DispatchMessageW` から **同期に**
呼ばれる。つまり `ApplicationHandler::window_event` のスタックの内側で発火し、
その時点で `RunnerState.app` は上位フレームに `&mut` で借用されている。
よって判断は event loop 側に投げ、WNDPROC が持つのは最小限の材料だけ
(HWND / `EventLoopProxy` / 窓 / `AppDirs` / 未保存かの `AtomicBool` ミラー)。
ミラーの更新は `refresh_activity` と同じ場所 (毎フレーム)。

### 4.3 応答

Microsoft のガイダンス (Vista 以降) は **「`WM_QUERYENDSESSION` でダイアログを
出してはいけない」** (OS の猶予は既定 5 秒)。時間が要るアプリは
`ShutdownBlockReasonCreate` で理由を登録して `FALSE` を返し、片付いたら自分で
終了する。シャットダウン画面にはその理由が表示され、ユーザーは「キャンセル」で
戻って未保存確認に答えられる。

よって **常に**理由を登録して `FALSE` を返し、通常の終了シーケンスを
`AppEvent::Quit` で起こすだけにする。理由文は未保存かで出し分ける
(「未保存の変更があります」/「終了処理中です」)。

- `WM_ENDSESSION(wParam != 0)` … 本当に終わる。まだ書けていない window geometry
  だけ同期で残す (recovery ファイルの削除は終了シーケンスが済ませている)。
- `WM_ENDSESSION(wParam == 0)` … 取り消された。ブロック理由を消して通常運転へ。

**この WNDPROC の中で子プロセスを畳む実装は書かない**。書くと `AppData` を
触れないぶん別実装になり、「終わり方」が 2 つに割れる。

参照:
- <https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-queryendsession>
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
2. `daw_audio.*` に `audio stream paused for shutdown` →
   `audio notify thread joined` → `daw_audio exiting` が出ること。
3. `tasklist` で daw_audio / daw_plugin_host / VOICEVOX engine が残っていないこと。
4. `shutdown /s /t 60` → シャットダウン画面に理由が出ること → `shutdown /a` で中止。
5. 未保存の状態で ✕ → 確認モーダル → 「保存して終了」/「保存せず終了」/
   「キャンセル」の 3 分岐。
