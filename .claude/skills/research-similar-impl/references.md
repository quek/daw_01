# 調査対象プロジェクト

| プロジェクト | 言語 | 特徴 | クローン先 | URL |
|---|---|---|---|---|
| clap | C (ヘッダ) | **CLAP 仕様そのもの**。拡張ヘッダ (`ext/*.h`) でセマンティクスを確認する。**最優先** | /tmp/clap | https://github.com/free-audio/clap |
| clap-host | C++ | CLAP ホストのリファレンス実装。ライフサイクル・スレッド設計の模範 | /tmp/clap-host | https://github.com/free-audio/clap-host |
| clack | Rust | Rust 製 CLAP ホスト/プラグインライブラリ。安全な Rust ラッパの参考 | /tmp/clack | https://github.com/prokopyl/clack |
| nih-plug | Rust | Rust 製プラグインフレームワーク (CLAP/VST3)。FFI とイベント変換の設計 | /tmp/nih-plug | https://github.com/robbert-vdh/nih-plug |
| clap-validator | Rust | CLAP プラグインを検証するホスト。ホスト側契約の確認に有用 | /tmp/clap-validator | https://github.com/free-audio/clap-validator |
| Meadowlark | Rust | Rust 製 DAW、RT オーディオと UI の参考 | /tmp/meadowlark | https://github.com/MeadowlarkDAW/Meadowlark |
| Vizia | Rust | GUI フレームワーク。カスタムビュー、Lens、IME の参考 | /tmp/vizia | https://github.com/vizia/vizia |

全プロジェクトを調査する必要はない。機能に最も関連するものを優先する。

## ⚠️ crates.io 版と GitHub main で API が違う場合

`/tmp/<crate>` にクローンされるのは **GitHub main**（未リリース版）。daw_01 が実際に
使うのは `Cargo.lock` で solver が選んだ crates.io 版。両者で API が違うなら、
**crates.io 側を基準に実装する**。

既知の乖離:
- **Vizia 0.3.0 (crates.io) = Lens ベース**（`#[derive(Lens)]`, `Binding`, `Lens::map`、`Data` trait 必要）
  **Vizia main = Signal ベースに移行中**（`Signal::new`, `ReadSignal`, `WriteSignal`）
  → Song 型を Lens で bind するときに `Data` 未実装でコンパイル不能になる。解決策は
  `AppData.tracker_text: String` のような派生文字列を保持して Lens 化する

- **Vizia 0.3.0 の API 細部差異** (main とも違うので公式 docs を信じすぎない):
  - `Slider::on_changing` ではなく `on_change`
  - `Alignment::Bottom` は存在しない。`BottomCenter` / `BottomLeft` / `BottomRight`
  - `List` のデフォルト CSS は `list-item { height: 30px }`。per-row フラット表示したい場合は
    `cx.add_stylesheet("list.foo list-item { height: 17px; }")` で上書き
  - `cx.spawn(|proxy| ...)` は std::thread を使うので内部で tokio の時間系 API は使えない

- `windows` crate は 0.58 → 0.61 で `HANDLE` 型が `isize` → `*mut c_void` に変更

- `bincode` 2.x は 1.x とは別 API（`Encode`/`Decode` derive）。IPC 型に
  `#[derive(bincode::Encode, bincode::Decode)]` が必要

Agent に調査を依頼するときは「crates.io の `<crate> = \"X.Y.Z\"` 基準で」と明記。

# 自プロジェクト（前作）

| プロジェクト | パス | 参考ポイント |
|---|---|---|
| sing_like_coding | `F:\dev\sing_like_coding` | IPC (shmem.rs, protocol.rs), CLAP ホスト (clap_manager.rs), オーディオエンジン (singer.rs), コマンドパターン (command/), データモデル (model/) |

前作に類似実装がある場合、**最も信頼性の高い参照元**として最初に確認する。

# API リファレンス・ガイド

| ドキュメント | URL |
|---|---|
| CLAP 公式 | https://github.com/free-audio/clap |
| CLAP ホスト実装ガイド | https://github.com/free-audio/clap/blob/main/include/clap/plugin.h |
| cpal | https://docs.rs/cpal |
| Vizia ドキュメント | https://docs.vizia.dev / https://docs.rs/vizia |
| Vizia examples | https://github.com/vizia/vizia/tree/main/examples |
| windows crate (Rust) | https://microsoft.github.io/windows-docs-rs/ |
| Win32 API | https://learn.microsoft.com/en-us/windows/win32/api/ |
| VOICEVOX Engine API | http://localhost:50021/docs (起動後の Swagger UI) |
| MIDI (wmidi / midly) | https://docs.rs/wmidi / https://docs.rs/midly |

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
| MIDI 入出力 | `midir::MidiInput` / `MidiOutput`, `wmidi::MidiMessage` |
| Vizia カスタムビュー | `View` trait, `Canvas`, `cx.draw()`, キーボードイベント |
| Vizia テーマ | CSS スタイリング、`cx.add_stylesheet()` |
| VOICEVOX 歌唱 | `/sing_frame_audio_query`, `/frame_synthesis` |
| VOICEVOX トーク | `/audio_query`, `/synthesis` |
| VOICEVOX キャラクター | `/singers`, `/speakers` |

# 実装で特に注意するポイント

- CLAP の **main thread / audio thread** の区別（各関数のスレッド要件を `plugin.h` で確認）
- `clap_process` のイベント配列は時刻順にソートされている必要がある
- オーディオバッファは CLAP 側が所有する場合と、ホスト側が貸し出す場合があるため `flags` を確認
- サブプロセスでプラグインを動かす場合、共有メモリのレイアウトとシグナリング順を厳密に設計する
- Vizia の `View` trait 実装でイベントハンドリングの順序（`event` → `draw` サイクル）
- VOICEVOX の歌唱クエリでは `key` は MIDI ノート番号（60 = C4）、`frame_length` はフレーム数（93.75Hz 基準）
