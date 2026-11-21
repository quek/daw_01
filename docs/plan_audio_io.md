# r.md #156 — オーディオ入出力 (ASIO 対応 + オーディオ入力の録音)

この文書はデバイス層の正本。設定画面は `daw_gui/src/view/settings*`、録音は録音側の module doc が持つ。

## 0. 決定事項 (2026-11-21 にユーザーが確定)

| # | 決定 |
|---|---|
| 1 | 範囲は出力 + オーディオ入力の録音。オーディオ設定画面も作る |
| 2 | ASIO は SDK・bindgen・LLVM・C++ ビルド・cpal の ASIO host を使わず、Rust からドライバーの COM interface を直接呼ぶ。Windows 標準 (WASAPI) は cpal |
| 3 | 設定 window は「左にカテゴリ (オーディオ / VOICEVOX / テーマ)、右にページ」。移動・リサイズ可、外クリックで閉じない、背後を操作できる、位置と開閉を app_config に永続 |
| 4 | オーディオのページ: ドライバーの種類 (Windows 標準 / ASIO)、デバイス (Windows 標準は入力と出力を別々に、ASIO は 1 つ)、使う入力 ch・出力 ch、サンプルレート、バッファサイズ、ASIO コントロールパネル、レイテンシ表示、先読み |
| 5 | 設定は選んだ瞬間に切り替える (「適用」ボタン無し。再生中なら止まる) |
| 6 | トラックごとに「入力」を 1 つ: MIDI (既定) / オーディオ入力 (モノ / ステレオ)。R を点けて録音するとその区間にクリップができ、WAV は `samples/` に入る |
| 7 | 「入力」はトラックインスペクタ上部のドロップダウン (編集面は 1 か所) |
| 8 | モニターは オフ / 自動 / オン の 3 択。自動 (既定) = R が点いている間だけ聞こえる |
| 9 | `daw_audio/src/asio/sys.rs` だけ `GPL-3.0-only` (`REUSE.toml` に理由)。「ASIO」の商標の帰属は `NOTICE` |

## 1. 全体の形

```
daw_gui  ── AudioDeviceConfig ─▶ daw_audio (handshake: Hello → Ack → OpenDevice → DeviceInfo → Session)
                                   └ device::control (制御スレッド。デバイスに触るのはここだけ)
                                       └ Backend: system (cpal) / asio_device / synthetic
                                           └ コールバック ─▶ device::callback::run ─▶ EngineCallback
                                                              (OutputStage・InputTap・MonitorFeed・時計)
```

- 型は `common/src/audio_device.rs` (望み = `AudioDeviceConfig`、実際 = `AudioDeviceInfo`、一覧 = `AudioDeviceList`)。
  GUI の state は `AppData::audio_device`、操作は `AppEvent::AudioDevice`。
- **engine のレートと入力 ch の範囲は、daw_audio のプロセスの間は変えない。** だから切り替えは 1 本:
  設定を保存して **daw_audio を起こし直す** (`AppData::reopen_audio_device` → `restart_audio_child`)。古い方が
  デバイスを手放して終わるのを有界に待ってから次を起こす (同じデバイスを二重に開かない)。
  FL Studio ASIO / Realtek ASIO で実測 0.2〜0.3 秒。その間 GUI は止まる。
- レートが変わったら、plugin host に `PluginCommand::SetSessionSampleRate` を送り、全 plugin を新しいレートで
  activate し直す (plugin host は起こし直さない = plugin の状態は残る)。

## 2. 描画の入口は 1 つ (`device::callback`)

どの backend も、自分の形式の buffer を「入力 = planar f32」「出力 = stereo interleave f32」に直して
`callback::run` を呼ぶだけ。描画 (先読みを含む)・録音の取り出し・`mute_output`・MMCSS・`MAX_FRAMES` を超える
コールバックの分割は `EngineCallback` に 1 回だけ書いてある。

- `EngineCallback` は stream の外 (`CallbackSlot`) に置く。stream を組み直しても、描き手・描き溜め・録音の
  取り出し口・時計がそのまま続く。backend は `try_lock` で借り、置かれていない間は無音を出す。
- 出力は常に stereo。デバイスのどの組へ置くか (`output_first_channel`) は backend の仕事で、他の ch は無音。
- 録音の取り出し (`InputTap::capture`) は描画の後に呼ぶ。入力の無いデバイスでも毎回呼ぶ (時刻だけ進む)。

## 3. 時計と入力の渡し方 (`device::input` / `device::monitor`)

- 時計は 1 つ: **コールバックに届いたフレームの通し番号** (`device_frame`)。stream が止まっている間は進まない。
- 描画には「このブロックの先頭がデバイスへ渡る時点の通し番号」(`DeviceIo::out_frame`) を渡す。先読みしていても
  同じ物差しになる (描き溜めに積んだ量 + 描き溜めを通らずに出した量)。
- コールバックは入力を ring に積む。描き手は **通し番号で** 取り出す (`MonitorInput::take`): 欲しい位置より古い分は
  捨て、まだ届いていない分は無音で埋める。捨てた / 埋めたフレームは metrics に数える
  (`input_dropped_frames` / `input_filled_frames`)。同じ時計のデバイス (ASIO / 合成) の定常運転では 0 のまま。
  **先読みを切り替えた直後は 2 周期ぶん 1 回だけ出る** (描く位置が 2 周期ずれるため)。
- OS 標準で入力と出力が別デバイスのときは時計が別なので、番号ではなく溜まり具合で合わせる
  (`take_unsynced`: 足りなければ無音、溜まり過ぎたら古い方を捨てる)。
- 入力のフレーム D は、出力のフレーム `D − 入力レイテンシ − 出力レイテンシ` と同時に鳴っていた音。
  レイテンシは `AudioDeviceInfo` と同じ値を `DeviceIo` で RT にも渡す。

## 4. 制御スレッド (`device::control`)

ASIO のドライバーは作ったスレッドに縛られる (STA の COM object)。**開く・止める・起こす・開き直す・コントロール
パネル・閉じる** は全部この 1 本で行い、他のスレッドからは channel + event で頼む (待ちは有界)。

- STA + 隠し窓 + message pump。100 ms ごとに、アイドル park の要求 (`SharedState::park_requested`) への追従と、
  デバイス側の変化の確認 (`Backend::poll`) をする。stream の停止 / 再開を出すのがここだけなので、止める側と
  起こす側が入れ違わない。ASIO の park は `stop` / `start`。
- **ドライバー起点の変更**: ASIO のコールバック (`kAsioResetRequest` / `sampleRateDidChange`) は数えるだけ。
  制御スレッドが 300 ms 落ち着くのを待って、同じ設定で開き直す。開いたものが今の session (種類・レート・入力 ch) と
  合えばそのまま続け、`DeviceInfo` を送る。合わなければ閉じて `DeviceRestartRequired` を送り、daw_gui が
  起こし直す。10 秒に 3 回を超える要求は無視する。`kAsioLatenciesChanged` はレイテンシを読み直すだけ。
- OS 標準はデバイスが抜かれる / 形式が変わると stream が止まるので、`DeviceRestartRequired` を送る。
- **コントロールパネルの `sysRef` は制御スレッドの隠し窓** (daw_gui の本体窓ではない)。パネルの窓を作るのは
  ドライバーで、owner と TOOLWINDOW の契約 (`plan_plugin_editor_topwindow.md`) を守らせられない。modal な
  パネルは親を無効化するので、daw_audio が落ちると本体窓が無効のまま残る。前面に出すために、daw_gui が
  送る直前に `AllowSetForegroundWindow` し、daw_audio が隠し窓を前面にしてから `controlPanel()` を呼ぶ。
  modal なパネルが開いている間、制御スレッドはその中に居る (park の追従と終了はパネルを閉じるまで待つ。
  終了は daw_gui が 3 秒で見切る)。

## 5. 開けないとき

`device::open` は必ず何かを返す: 設定どおり → OS 標準の既定の出力 (入力なし) → 音の出ない合成デバイス。
戻した理由は `AudioDeviceInfo::fallback_reason` に入れ、保存された設定は変えない (機材をつなぎ直せば元に戻る)。
OS 標準で入力だけ開けないとき (入力デバイスが無い / 入力と出力のレートが違う) は、出力は設定どおりに開き、
理由だけ入れる。

## 6. backend ごとの事実

| | OS 標準 (cpal / WASAPI 共有モード) | ASIO | 合成 |
|---|---|---|---|
| レート・バッファ | 選べない (OS の設定に従う。候補は空) | ドライバーの候補から。バッファの候補が 1 つならコントロールパネルで変える | 44.1 / 48 / 96 kHz、64〜2048 |
| sample 形式 | cpal の全整数・浮動小数 | Int16 / Int24 (3 byte 詰め) / Int32 / Float32 / Float64 の little-endian。他は開くときに断る | f32 |
| 入力 | 別デバイス・別 stream。**出力とレートが違えば開かない** (レート変換はしない) | 同じドライバー・同じコールバック | `ramp` (ch と通し番号で決まる波形) / `loopback` (出力を申告どおり遅らせて返す) |
| レイテンシ | 観測値からの推定 (動き出してから `DeviceInfo` で更新) | ドライバーの申告 (バッファぶんを含む) | `ramp` は 0、`loopback` は 入力 64 + 出力 128 + バッファ |
| 対象 | — | 64 bit Windows だけ | 検査用 (設定画面に出さない) |

- 1 回のコールバックが `MAX_FRAMES` (1024) を超えるデバイスは、割って描く。先読みは 2×`MAX_FRAMES` までの周期なら
  飢えない。それより長い周期では先読みの描き溜めが 1 回ぶんに足りず、飢えやすい (先読みを切る)。
- 合成の `reopen` はドライバー起点の開き直しを通す検査用 (動き出して 1 秒後に 1 回求める)。

## 7. 確かめ方

- `daw_gui/tests/scripts/audio_device_switch.js` (`cargo test -p daw_gui --features script --test audio_device_smoke`):
  OS 標準 → 合成 → 開き直し → 存在しないデバイス → 既定。`--arg "asio=<名前>|<名前>"` でその ASIO ドライバーにも
  切り替える (使用中の実機を指さないこと。ドライバーは排他のことが多い)。
- script の口: `daw.setAudioDevice` / `audioDevices` / `audioDeviceInfo` / `audioInputStats`
  (`daw_gui/src/script/audio_device.rs`)。窓なしの script は隔離した設定で動き、保存された実機は開かない。
- `daw_audio --asio-probe [名前]`: ASIO の層だけの実測 (列挙 / 申告値 / 無音 1 秒)。
