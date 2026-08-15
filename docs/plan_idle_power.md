# r.md #49 — アイドル時の省電力 (停止中 + 非アクティブでオーディオと描画を止める)

## 0. 決定事項 (grill-me で確定)

| # | 論点 | 決定 |
|---|---|---|
| 1 | 「アクティブでない」の定義 | **別アプリに切り替えたとき**。daw_01 の窓 (メイン / 動画プレビュー / プラグインエディタ) をどれも触っていない状態。プラグインのつまみを回している間はアクティブ |
| 2 | 裏に回したときまだ音が鳴っている場合 | **音が消えるまで待ってから止める**。自走プラグイン (VCV Rack 等) がある曲では park に入らない。ブツッと切れる音は絶対に出さない |
| 3 | 非アクティブでも画面が見えている場合 | **完全に止まってよい** |
| 4 | 重い処理 (VOICEVOX 合成 / 書き出し / プラグイン検索) 中 | **進捗は動き続ける** = 省電力に入らない |
| 5 | 省電力に入るまでの時間 | **画面は即座、音は 5 秒後** |
| 6 | ON/OFF 設定 | **作らず常に ON** |
| 7 | アクティブなまま停止しているとき | **こちらも直す**。「画面が変わるときだけ描く」へ |

## 1. 現状 (調査で確定した事実)

### 1.1 描画は既に `ControlFlow::Wait`。それでも 30fps 出ている理由

`daw_gui/src/view/runner.rs:47` で `ControlFlow::Wait`。にもかかわらずアイドルで約 30fps
回っているのは **2 段の連鎖**による:

1. `daw_gui/src/main.rs:379-437` の playhead poller が、再生状態・フォーカス・最小化を
   **一切見ずに 33ms 周期で 4 イベント/tick** を送る (`Tick` / `ModScalarsTick` /
   `TrackPeaksTick` / `MetricsTick` = 約 121 events/sec)
2. `runner.rs:940-941` の `user_event` が **イベント種別を問わず無条件に `request_redraw()`**

`render_frame` の戻り値は既に `is_playing || voicevox_animating` で節電を意図している
(`runner.rs:1300-1301`) が、上の無条件 redraw に潰されている。

### 1.2 停止中もプラグインを毎バッファ叩いている

`daw_audio/src/main.rs:1501` で `stream.play()` するだけで、**`pause()` の呼び出しが
コードベースに 1 件も存在しない**。停止中も毎バッファ `render_master_buffer`
(`engine.rs:1028`) が走り、`pd.playing = 0` を書いた上で worker を `SetEvent` で起こし、
plugin_host へクロスプロセス往復して `process()` を呼び続ける。

`playing` で gate されているのは sequencer のノート収集 / audio clip レンダ / メトロノーム /
playhead 前進の 4 つだけ。プラグイン dispatch・master fx・per-sample の volume/pan ランプ・
`block_peaks_stereo` の全走査は **停止中も走る**。

plugin_host 側の worker は `WaitForSingleObject(INFINITE)` (`common/src/plugin_ref.rs:161`,
`daw_plugin_host/src/process_server.rs:534`) でブロックしているので、**audio callback さえ
止めれば plugin host も連鎖して idle になる**。

### 1.3 park の前例が既にある

export freewheel 中は callback が `live_parked` を立てて即 return し、export thread が
それを待ってから plugin を駆動する (`engine.rs:844-862`)。**「止めて後で再開する」構造は
既に存在する**ので、#49 はその一般化。

### 1.4 アクティブ判定の材料は未配線

- `WindowEvent::Focused(bool)` は受けているが `InputAccumulator` に渡すだけ (`runner.rs:856-858`)。
  `InputAccumulator` は `Focus(false)` で合成 release を積むだけで真偽値を保持しない
- **アクティブ状態を保持する変数はコードベースに存在しない**
- プラグインエディタ窓は **daw_plugin_host が所有する owner 無し top-level**
  (`daw_plugin_host/src/editor_window.rs`)。daw_gui を owner にするのは設計上禁止
  (`GetAncestor(GA_ROOTOWNER)` が daw_gui に解決すると JUCE の cascade サブメニューが
  `isForegroundProcess()` 判定で即 dismiss される)。
  **帰結: プラグイン GUI 操作中、daw_gui は非フォーカスどころか foreground プロセスですらない。
  daw_gui 内の情報だけでは原理的に判定できない**

### 1.5 `transport.is_playing` は「停止中」の判定に使えない

`is_playing = true` の代入は `handler/transport.rs:54` の 1 箇所のみ。`start_recording`
(`handler/midi.rs:450-485`) は `AudioCommand::Play` を送るが `is_playing` を立てない。
→ **Rec 単独の録音中は `is_playing == false`**。これを park 条件に使うと録音中に音が切れる。

## 2. 設計

### 2.1 責務の分割 (SSoT)

```
daw_gui         : 「アプリの窓がアクティブか」の SSoT。事実だけを engine に渡す
daw_plugin_host : 「自プロセスの窓がアクティブか」の SSoT (WM_ACTIVATEAPP)
daw_audio       : 「今 park してよいか」の SSoT (再生 / count-in / 書き出し / 無音)
```

daw_gui は park の可否を判断**しない**。`is_playing` の GUI 側不整合 (§1.5) を構造的に
回避でき、engine 側の条件 (`export_running` / `preroll` / 無音) は engine が唯一の権威になる。

```
アプリがアクティブ = daw_gui のいずれかの窓が focus
                  ∨ daw_plugin_host のいずれかの窓が active
```

### 2.2 オーディオ: 無音が 5 秒続いたら CPAL stream を pause

`cpal::Stream` は wasapi backend で `Send + Sync`
(`cpal-0.17.1/src/host/wasapi/stream.rs:40,49`)。`pause()` は `IAudioClient::Stop()` を
呼び (同 `stream.rs:227-236`)、cpal の run thread は
`WaitForMultipleObjectsEx(INFINITE)` (同 :253-262) でブロックする = **スピンせず完全に寝る**。

**park 条件** (CPAL callback 内で atomic のみで評価 = RT 安全):

```
!app_active ∧ !playing ∧ preroll == 0 ∧ !export_running ∧ 出力が無音
```

これが **連続して 5 秒**成立したら `park_requested` を立てる。1 つでも崩れたらカウンタを
0 に戻す。「無音」を条件に含めることで決定 2 (残響 / 自走プラグインは止めない) が
**追加のロジック無しで**満たされる — 残響が鳴っている間はカウンタが進まず、消えてから
5 秒で park する。自走プラグインは永久に無音にならないので park しない。

park の実行は callback の外 (notify thread、100ms 周期) が行う。callback 内から stream は
触れない (cpal のコマンドキュー経由なので、callback から呼ぶとデッドロックしうる)。

**park は「命令」ではなく「要求 → 追従」で組む**。`park_requested` を*望ましい状態*として
置き、notify thread が stream をそこへ寄せる reconciler として動く:

- 要求を立てるのは callback (アイドルの検出者)
- 要求を取り下げるのは callback (アイドルが崩れた) と receive loop (コマンドが来た)
- stream の実操作は reconciler + receive loop の即時 wake。どちらも **Mutex の中で
  要求を読んでから**操作する

「pause しろ / play しろ」を直接命令する形にすると、要求を立てた直後に `Play` が来たとき
**取り下げが早期 return で素通りして、再生中の stream を pause する** (= 音が出ない) race に
なる。実際に最初の実装がこれを踏んでいた。

同じ理由で、連続アイドルサンプル数の更新は `load` → 加算 → `store` ではなく
**`fetch_add`** で行う。receive loop 側の 0 リセットを取りこぼすと、起こした直後に古い
カウントが復活して即座に park し直す。

**park 直前に meters を 0 で publish する** (master peak / track peak / dsp load)。
publish が止まった後に古い値が残って GUI が「動いているメーター」を凍結表示するのを防ぐ。
mod scalars は**ゼロにしない** — メーターではなくパラメータ値なので、最後の値のまま
凍結するのが正しい。

**resume**: recv_loop が `SetAppActive(false)` **以外**のコマンドを処理したら無条件に
`stream.play()`。「Play のとき / preview のとき / export のとき…」と列挙すると
不変条件 1 の言う「補償コード」になるので列挙しない。resume 後は callback が park 条件を
再評価し、まだ idle ならまた 5 秒かけて park する (無害)。

park 中は `live_parked = true` を立てる (dispatch していないのは事実)。これが無いと
`export.rs:184-191` の 2 秒待ちを毎回踏む。

### 2.3 描画: 「画面が変わるときだけ描く」

**(a) 描かない条件は「停止中 ∧ 非アクティブ ∧ 進捗表示なし」。**
アクティブに戻った瞬間に 1 回描く。

- 「重い処理中」(`export_stage` / `pending_video_export` / `is_rescanning` /
  `voicevox_animating`) はアクティブ扱い (決定 4)
- **再生 / 録音中は非アクティブでも描き続ける**。r.md #49 の条件は「**再生停止中かつ**
  アクティブでない」であって、アクティブ判定だけで止めてよいとは書かれていない。
  裏で再生しながら別ウィンドウで作業するのは普通の使い方
  (実機検証で「再生中なのに 27 秒間 1 フレームも描かれない」状態を作ってしまった)

**(a-2) daw-ui の自動 redraw も同じゲートで束ねる。**
`UiHost::frame` は末尾で自動的に `request_redraw` を呼び、その発火条件には
**widget 発の継続アニメ要求** (`Ui::request_redraw` — レベルメーターの減衰 / peak hold)
が含まれる。widget は自分が見えているかを知らないので、非アクティブでも要求を出し続け、
daw_gui 側のゲートを**迂回して**描画が回り続ける (実機検証で、停止 + 非アクティブでも
メーターが落ち切るまで 8 秒間 60fps だった)。

daw-ui に `UiHost::set_redraw_suppressed(bool)` を足して、「今この画面は誰も見ていない」
という上位の判断を唯一の口にする。widget 側は変更しない — 見えているかを知らないのが
正しく、判断を上位へ集約するのが筋。ドメイン知識を持たない汎用スイッチなので
不変条件 8 に抵触しない。判定は **`ui.frame` の前**に行う (末尾で判定すると、その frame の
自動 redraw に間に合わない)。

**(b) tick 系イベントは「見た目が変わったときだけ」redraw する。**
`user_event` は従来どおり**既定で redraw する** (安全側) が、5 つの tick variant
(`Tick` / `ModScalarsTick` / `TrackPeaksTick` / `MetricsTick` / `SystemMetricsTick`)
だけは `handle_event` の前後で**画面に出る値**を比較し、変化したときだけ redraw する。

denylist は tick 5 種のみなので、他の ~500 の variant に「立て忘れ = 画面が固まる」
リスクを持ち込まない。`handle_event` を `bool` 返しに変える案 (30 ファイル以上に波及) は
採らない。

比較は **表示解像度で量子化**する。ステータスバーは `DSP {:>3.0}%` / `CPU {:>3.0}%` /
`{:>2.0}fps` (`view/status_bar.rs:85-108`) なので整数パーセントまで。生の f32 で比較すると
DSP load の EMA が毎 tick 変わって結局 30fps になる。

**(c) redraw を要求していない入力イベントを直す。**
`runner.rs:686-705` の `_ => {}` に落ちている `Focused` / `PointerEntered` / `PointerLeft` /
`FileHovered` / `FileHoverCancelled` / `FileDropped` / `ScaleFactorChanged` は
**現在 33ms Tick に救われているだけの既存バグ**。Tick を間引くと即座に体感バグ
(Alt+Tab でドラッグが貼り付く、ドロップ対象ハイライトが出ない/消えない) になるので
先に潰す。

**(d) 動画プレビュー窓の自走ループを止める。**
`render_frame` が毎フレーム `preview.render()` した上で `preview.window.request_redraw()`
する (`runner.rs:1177-1191`) → その redraw が `handle_preview_window_event` で
**もう一度** `preview.render()` する (`runner.rs:1329-1343`) = **1 main frame につき
preview を 2 回描いている**。main のフレームに従属させ、自走 redraw を撤去する。

**(e) FPS 表示を「1 フレームごとの dt の EMA」から「実測本数 ÷ 経過時間」へ。**
描画がイベント駆動になると dt の分布が二極化する (連続描画中は 16ms、アイドル明けは数秒)
ため、per-frame dt を EMA に入れる方式は**どちらに転んでも壊れる**:

- そのまま入れる → アイドル明けの 1 サンプルで表示が 0 付近まで落ちる
- 長い dt を捨てる → **短い側だけが残って EMA が上に暴走する** (実測 3.9fps のとき
  126fps と表示された。最初この対処を入れて実機検証で発覚)

本数 ÷ 経過時間ならどちらの regime でも「秒間何枚描いたか」をそのまま表す。アイドル中に
低い値が出るのは嘘ではなく、省電力が効いている証拠そのもの。加えて、**省電力に入る
最後の 1 フレームでは 0 に畳んでから描く** — 止まった画面に直前の 60 が焼き付くと
「省電力中なのに 60fps 出ている」と読めてしまうため。

**(f) リソースモニターの表示レートを 4Hz に落とす。**
ステータスバーの読み値は整数パーセント表示なのに、DSP load は毎 tick 揺れる。
30Hz で流すと **停止中でも「DSP% が変わった」だけで全画面を 30fps 描き直す**ことになり、
(b) の変化検出が意味を失う。表示に必要なのは数 Hz なので tick 自体を間引く
(REAPER が `Meter update frequency` を別設定に持っているのと同じ理由)。
`take_dsp_load_peak` は swap-reset なので、間引いても peak は 8 tick 分の worst-case を
正しく拾う (むしろ精度が上がる)。

**(g) 描画しないフレームでも子プロセス同期は流す。**
`flush_song_sync` は `render_frame` の末尾にしか無い。描画を止めると、
**裏で MIDI コントローラを触った編集が engine に届かない** (`MidiControlChange` は
フォーカスと無関係に届く)。再描画しない経路で明示的に流す。epoch 差分ゲートなので
編集が無ければ no-op。

**(h) 自動テストはアクティブ扱いに固定する。**
`--smoke-test` は preview 窓を `PrintWindow` で pixel capture する。窓がフォーカスを
得るかは実行環境次第なので、省電力で描画が止まると「真っ黒 = 視覚回帰」と誤検出しうる。
検証対象は描画結果なのでこの経路だけ判定を固定する (`ActivityState::force_active`)。

### 2.4 その他のアイドルコスト

- `daw_plugin_host` の `run_param_drain` は**空振りでも 2ms sleep = 500Hz で起き続けている**
  (`process_server.rs:301-303`)。空振りが続いたら sleep を 2ms → 32ms へ伸ばす backoff にする。
  RT 側から `SetEvent` を打つ案は RT 制約 (システムコール最小化) と衝突するので採らない
- sysinfo poller は 1Hz で `ProcessesToUpdate::All` = **全プロセス列挙**
  (`main.rs:442-475`)。非アクティブ中は poll しない
- playhead poller は省電力中 33ms → 250ms。**完全には止めない** — `on_tick` に同居する
  3 つの watchdog (panic 遅延 reinit / 書き出し 60s / plugin state round-trip) が死ぬため。
  1 秒まで伸ばさないのは復帰時の応答性 (寝ているスレッドは間隔ぶん起きない)。
  ただし**裏で再生 / 録音が続いている間は 33ms を維持する** (`needs_fast_ticks`) —
  曲末の自動停止判定・オートメーション録音の 1/64 拍間引き・再生追従スクロールが
  `on_tick` に同居しているので、粗くすると停止位置がずれ録音カーブが階段になる

## 3. 変更点

### common
- `AudioCommand::SetAppActive(bool)` — daw_gui → daw_audio
- `PluginEvent::HostWindowsActive(bool)` — daw_plugin_host → daw_gui
- protocol fingerprint が変わるので **`make build` で 3 exe すべて再ビルド必須**
  (`common/build.rs`。不一致は respawn せず `FingerprintMismatch` で fail する)

### daw_plugin_host
- `editor_window.rs`: `WM_ACTIVATEAPP` を `editor_wnd_proc` で拾い、プロセス全体の
  `AtomicBool` に反映 (WM_ACTIVATEAPP はスレッド単位なので per-window ではなく
  プロセス単位の状態が正しい表現)
- `main.rs`: plugin-main の pump が変化を poll して `HostWindowsActive` を emit。
  最後のエディタを閉じたら false を強制
- `process_server.rs`: param drain の backoff

### daw_audio
- `SharedState`: `app_active` / `idle_silent_frames` / `park_requested`
- CPAL callback: park 条件の評価 + park 直前の meter ゼロ publish
- notify thread: `park_requested` を見て `stream.pause()`
- recv_loop: `SetAppActive` の反映 + それ以外のコマンドで `stream.play()`

### daw_gui
- `state/`: アクティブ状態 (`main_focused` / `preview_focused` / `plugin_host_active`)
- `view/runner.rs`: focus 配線 / redraw gate / tick 変化検出 / preview 自走停止 / fps EMA
- `main.rs`: sysinfo poller の gate

## 4. テスト

自動テストを書くのは **純粋ロジック**のみ (`feedback_no_tests_for_simple_cases`):

- park 条件の状態機械 (無音カウンタの積算とリセット、5 秒閾値)
- tick の「見た目が変わったか」比較 (量子化の境界: 3.4% と 3.6% は別、3.4% と 3.49% は同じ)
- アクティブ合成 (`main ∨ preview ∨ plugin_host`)
- protocol の bincode roundtrip (新 variant、既存慣例に従う)

フォーカス遷移・実際の消費電力は自動テストで拾えないので実機検証 (§5)。

## 5. アクティブ判定が固着する経路 (塞いだもの)

`plugin_host_active` / `preview_focused` が true のまま取り残されると、**二度と省電力に
入らない**。報告者が消える経路を全部塞ぐ:

| 経路 | 対処 |
|---|---|
| エディタ窓を閉じた (窓が消えるので `WM_ACTIVATEAPP` が来ない) | `close_slot_gui` / plugin unload で、残りエディタが 0 なら非アクティブを強制 |
| daw_plugin_host が crash した | `PluginEvent::ChildDisconnected` で `plugin_host_active = false` |
| 動画プレビュー窓を閉じた | `sync_preview_window` の teardown で `preview_focused = false` |

## 6. 検証

### 自動 (完了)

- `make test` / `make clippy` / `make arch-lint` — green
- `cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4` — **PASS**
  (unique_colors=10500 / black 9%)。preview の二重描画を外しても描画は正常
- park の状態機械 (無音カウンタの積算・リセット・外部リセットの取りこぼし) は
  `daw_audio/src/engine.rs` の `idle_park_tests`
- アクティブ合成 (`main ∨ preview ∨ plugin_host`) と進捗中の扱いは
  `daw_gui/src/state/activity.rs` の tests

### 実機ログ (完了)

```
02:42:59〜02:43:09  is_playing=true  main_focused=false  keep=true   → 60fps 継続
02:42:42〜02:42:55  is_playing=false main_focused=false  keep=false  → 10.3 → 2.5 → 停止
02:42:45  audio stream park state changed parked=true    ← 非アクティブ 5 秒後
02:42:56  audio stream park state changed parked=false   ← 復帰
```

**変更前のベースライン** (release ビルド / 停止中 / 非アクティブ / 空プロジェクト):
daw_gui 30.5% of one core、daw_plugin_host 1.3%、daw_audio 0.1%
(daw_audio が低いのは track 0 で worker が早期 return するため。プラグインを積むと上がる)。

### 実機検証で発覚した実装ミス 2 件 (修正済)

どちらも**自動テストでは絶対に拾えない**種類 (フォーカス遷移とフレーム駆動):

1. 再生中でも非アクティブなら描画を止めていた → §2.3(a)
2. daw-ui の自動 redraw がゲートを迂回していた → §2.3(a-2)

### 目視 sign-off (ユーザー)

1. 停止 + アクティブのまま放置 → daw_gui の CPU が下がること
2. 別アプリに切り替え → 画面が即止まり、5 秒後に daw_audio の CPU が 0 になること
3. 戻る → 即座に描画とオーディオが復帰し、Play が普通に鳴ること
4. リバーブを掛けた音を鳴らして即座に裏へ回る → **残響がブツッと切れないこと**
5. VCV Rack 等の自走プラグインを挿して裏へ回る → 鳴り続けること
6. プラグインエディタを開いて触っている間 → 画面もオーディオも止まらないこと
7. VOICEVOX 合成中 / 書き出し中に裏へ回る → 進捗が回り続けること
8. Alt+Tab でドラッグ / ファイルドロップ / hover が壊れていないこと (§2.3(c) の回帰確認)
