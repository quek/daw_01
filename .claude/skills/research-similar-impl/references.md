# 調査対象プロジェクト

| プロジェクト | 言語 | 特徴 | クローン先 / パス | URL |
|---|---|---|---|---|
| clap | C (ヘッダ) | **CLAP 仕様そのもの**。拡張ヘッダ (`ext/*.h`) でセマンティクスを確認する。**最優先** | /tmp/clap | https://github.com/free-audio/clap |
| clap-host | C++ | CLAP ホストのリファレンス実装。ライフサイクル・スレッド設計の模範 | /tmp/clap-host | https://github.com/free-audio/clap-host |
| clack | Rust | Rust 製 CLAP ホスト/プラグインライブラリ。安全な Rust ラッパの参考 | /tmp/clack | https://github.com/prokopyl/clack |
| nih-plug | Rust | Rust 製プラグインフレームワーク (CLAP/VST3)。FFI とイベント変換の設計 | /tmp/nih-plug | https://github.com/robbert-vdh/nih-plug |
| clap-validator | Rust | CLAP プラグインを検証するホスト。ホスト側契約の確認に有用 | /tmp/clap-validator | https://github.com/free-audio/clap-validator |
| Meadowlark | Rust | Rust 製 DAW、RT オーディオと UI の参考 | /tmp/meadowlark | https://github.com/MeadowlarkDAW/Meadowlark |
| vst3_pluginterfaces | C++ (ヘッダ) | **VST3 のインターフェース定義そのもの** (`gui/iplugview.h` 等)。VST3 の呼び出し契約はここで確認する | /tmp/vst3_pluginterfaces | https://github.com/steinbergmedia/vst3_pluginterfaces |
| vst3_public_sdk | C++ | VST3 SDK の実装とサンプル。`samples/vst-hosting/editorhost` がホスト実装の手本 | /tmp/vst3_public_sdk | https://github.com/steinbergmedia/vst3_public_sdk |
| **gui_01 (daw-ui)** | Rust | **本プロジェクトで採用した自作 GUI ライブラリ。daw_01 の path 依存先** | ui/ | (ローカル) |

全プロジェクトを調査する必要はない。機能に最も関連するものを優先する。

## ⚠️ crates.io 版と GitHub main で API が違う場合

`/tmp/<crate>` にクローンされるのは **GitHub main**（未リリース版）。daw_01 が実際に
使うのは `Cargo.lock` で solver が選んだ crates.io 版。両者で API が違うなら、
**crates.io 側を基準に実装する**。

既知の乖離・バージョン依存:
- `windows` crate の `HANDLE` は 0.56 では `isize`、0.58 以降は `*mut c_void` (daw_01 の直接依存は 0.62。
  Cargo.lock には midir / sysinfo / trash 経由の 0.56 も居るので、どちらの版の型かを確かめる)
- `tokio` の `net::windows::named_pipe` は 1.x 前提 (daw_01 の IPC pipe が使う)
- `bincode` 2.x は 1.x とは別 API（`Encode`/`Decode` derive）。IPC 型に
  `#[derive(bincode::Encode, bincode::Decode)]` が必要
- `wgpu` 29: `InstanceDescriptor` は `Default` を持たない (`InstanceDescriptor::new_without_display_handle()` で
  作ってからフィールドを書き換える)。`request_adapter` / `request_device` は Future を返し、daw-ui は
  `pollster::block_on` で待つ (`ui/crates/renderer/src/device.rs`)
- `winit` 0.30: `ApplicationHandler` trait + `EventLoop::run_app` (closure を渡す `EventLoop::run` は `#[deprecated]`)。
  Window 生成は `resumed` で `ActiveEventLoop::create_window(attrs)` (`EventLoop::create_window` も `#[deprecated]`)

Agent に調査を依頼するときは「crates.io の `<crate> = \"X.Y.Z\"` 基準で」と明記。

# 自プロジェクト（前作・併存）

| プロジェクト | パス | 参考ポイント |
|---|---|---|
| sing_like_coding | 作者ローカルの別リポジトリ | IPC (shmem.rs, protocol.rs), CLAP ホスト (clap_manager.rs), オーディオエンジン (singer.rs), コマンドパターン (command/), データモデル (model/) |
| gui_01 サンプル | `ui/crates/examples/` | mixer / automation / embedded_host / sample_editor ほか — daw-ui の使い方の参照 |
| DAW 固有 widget | `daw_gui/src/widgets/` | arrangement / piano_roll (daw-ui core には置かない。CLAUDE.md 不変条件 8) |

前作 / gui_01 サンプルに類似実装があれば最初に確認する。gui_01 サンプルは daw-ui の今の API に追従している
(breaking change は全 example を同じ commit で直す) ので、daw-ui の使い方の参照元として最も信頼できる。
ただし前作はプロト品質なので、構造 (プロセス分割・
IPC の形・イベントの流れ) の目安にとどめ、実装はベストプラクティスを調べて採る
(`feedback_prioritize_best_practices`。implement skill の手順 2 と同じ)。

# API リファレンス・ガイド

| ドキュメント | URL |
|---|---|
| CLAP 公式 | https://github.com/free-audio/clap |
| CLAP ホスト実装ガイド | https://github.com/free-audio/clap/blob/main/include/clap/plugin.h |
| vst3 crate (Rust bindings、Cargo.lock は 0.3.0) | https://github.com/coupler-rs/vst3-rs |
| cpal | https://docs.rs/cpal |
| winit | https://docs.rs/winit/0.30 |
| wgpu | https://docs.rs/wgpu/29 |
| gui_01 (daw-ui) | `ui/crates/{platform,renderer,ui}/src/` を直接 Read |
| windows crate (Rust) | https://microsoft.github.io/windows-docs-rs/ |
| Win32 API | https://learn.microsoft.com/en-us/windows/win32/api/ |
| VOICEVOX Engine API | http://localhost:50021/docs (起動後の Swagger UI) |
| MIDI (midir / midly) | https://docs.rs/midir / https://docs.rs/midly |

# 機能と API の対応例

| 機能 | 主な API / インターフェース |
|---|---|
| プラグインスキャン | `clap_plugin_factory::get_plugin_descriptor` / `create_plugin` |
| 初期化・破棄 | `clap_plugin::init`, `activate`, `start_processing`, `stop_processing`, `deactivate`, `destroy` |
| 音声処理 | `clap_plugin::process`, `clap_process`, `clap_audio_buffer` |
| パラメータ | `clap_plugin_params` (`count`, `get_info`, `get_value`, `text_to_value`, `value_to_text`, `flush`) |
| オートメーション | `clap_event_param_value`, `clap_event_param_mod` (input events) |
| MIDI I/O | `clap_event_note`, `clap_event_midi`, `clap_event_midi_sysex` |
| プラグイン GUI | `clap_plugin_gui` (`create`, `set_parent`, `set_size`, `show`, `hide`, `destroy`) — spec は `/tmp/clap/include/clap/ext/gui.h` の先頭コメントに「初期化順序」が図解されている。`clap_host_gui` (`request_resize`, `closed`) も忘れずに実装 |
| スレッドチェック | `clap_host_thread_check` (main thread / audio thread の判定) |
| ウィンドウ埋め込み | `SetParent`, `SetWindowLongPtrW(GWL_STYLE)`, `raw-window-handle` |
| 低レイテンシ I/O | `cpal::Stream`, WASAPI exclusive mode |
| MIDI 入力 / SMF 読み書き | `midir::MidiInput` (`daw_gui/src/midi.rs`)、`midly` (`daw_gui/src/midi_import.rs` / `midi_export.rs`) |
| gui_01 ボタン / フェーダー / ノブ | `Ui::button_at(id, label, rect, on_click)`, `fader_at`, `knob_at`, `text_input_at`, `checkbox_at` |
| gui_01 カスタム描画 | `Ui::heavy(id, |hctx| { hctx.cached(viewport_key, |hctx| { hctx.push_rect/text/lines(...) }) })` |
| gui_01 レイアウト | `LayoutPass::new() → leaf / flex / compute_at → rect(node)` (taffy flexbox ラッパー) |
| gui_01 入力 | `Ui::pointer() → PointerFrame { pos, primary_just_pressed/released, scroll_delta, modifiers }` |
| background → UI スレッド通知 (daw_gui) | `EventLoopProxy<AppEvent>::send_event(event)` (winit 0.30 user event。`AppEvent` は `daw_gui/src/event.rs`) |
| VOICEVOX 歌唱 | `/sing_frame_audio_query`, `/frame_synthesis` |
| VOICEVOX トーク | `/audio_query`, `/synthesis` |
| VOICEVOX キャラクター | `/singers`, `/speakers` |

# 実装で特に注意するポイント

- CLAP の **main thread / audio thread** の区別（各関数のスレッド要件を `plugin.h` で確認）
- `clap_process` のイベント配列は時刻順にソートされている必要がある
- オーディオバッファは CLAP 側が所有する場合と、ホスト側が貸し出す場合があるため `flags` を確認
- サブプロセスでプラグインを動かす場合、共有メモリのレイアウトとシグナリング順を厳密に設計する
- gui_01 の `HeavyCtx::cached` 内の描画は viewport_key 一致時にスキップ。動的 overlay (cursor / 選択範囲) は cached の **外側** で `push_*`
- VOICEVOX の歌唱クエリでは `key` は MIDI ノート番号（60 = C4）、`frame_length` はフレーム数（93.75Hz 基準）
