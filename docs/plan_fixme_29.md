# FIXME #29 実装プラン — トラック = 1 本の可視プラグイン列（port + 位置で役割決定）

ユーザー要望: 「Scaler 2 は MIDI エフェクトにも楽器にもなる。Scaler 2 → Analog Lab V の順で刺したい」
= dual-role プラグイン（note 出力 かつ audio 出力）を音源の前に置いて「後段の音源を鳴らす生成器」
として使いたい。

## grilled 設計（確定）

- 1 トラック = 1 本の可視プラグイン列。内部の「note 生成器列 → 音源 1 個 → audio-fx 列」へ写像。
- 役割 = 持っているポート（note in/out, audio in/out）+ 並びの中の位置。カテゴリで固定しない。
- dual-role（note-out かつ audio-out）は、音源の前なら生成器（自身の audio は破棄）、音源位置なら音源。
- スキャン時に全ポート構成を記録。VST3/CLAP とも probe（instance 生成 → port query）で確定。
  「生成器になれる = note 出力を持つ」。
- ドラッグ並べ替えはポート的に有効な位置のみ受け付け、無効な drop は元に戻す。
- 既定挿入: 音源が埋まっていれば dual-role は生成器として音源前へ、純粋音源は既存音源を置換。
- 1 トラック 1 音源は維持。

## 一次情報による重要な訂正

- **CLAP scan は instantiate しない**（`plugin_db.rs:286` コメント）。descriptor.features に port 有無は
  無いので、CLAP も VST3 と対称に **probe が必須**（Step 4）。daw_plugin_host に既存の note-ports /
  audio-ports query idiom（`clap_plugin.rs`）を再利用。
- **engine の MidiFx ループ（`engine.rs:1176-1252`）は `pd.buffer_out` を読まない** → 生成器位置の
  dual-role は audio が破棄される = 設計と一致。**engine 変更不要**（Step 9 は検証のみ）。
  audio を track に足す / `Generator` slot 新設は spec 違反 or 不要な protocol 破壊なので不採用。
- **PluginSlot enum（`protocol.rs`）は不変**。「生成器」= MidiFx slot に置いた note 出力プラグイン、
  という写像で既存 IPC / engine routing をそのまま使う → IPC protocol migration 不要。

## SSoT / DRY

- capability の唯一の真実は `PluginEntry` の 3 bool（`has_note_input/has_note_output/has_audio_output`）。
- 役割導出 `PluginCapability::from_ports(has_note_out, has_audio_out)` を picker 挿入と reorder drop
  検証で共有。`PluginCategory::from_features` は picker 行の表示タグ（楽器/FX/MIDI）専用に降格。

## Step（依存順）

| Step | 領域 | 内容 | スキーマ変更 |
|---|---|---|---|
| 1 | scan | `PluginEntry` に 3 bool（`#[serde(default)]`） | JSON cache（serde default 互換） |
| 2 | scan | builtin/VST3/CLAP の scan-time 暫定値を埋める | — |
| 3 | scan | VST3 probe を `PortConfig`（3 bool）返しに拡張 | — |
| 4 | scan | CLAP probe 新設（`--probe-clap`、note-ports/audio-ports ext query） | — |
| 5 | scan→DB | rescan flow を「probe → 3 bool 更新」に統一（VST3+CLAP 両方 probe） | — |
| 6 | picker | port ベース挿入（`PluginCapability::from_ports` + dual-role 既定規則） | — |
| 7 | 永続化 | cache に `port_probe_version`、不一致なら起動時 1 回 rescan | JSON cache version field |
| 8 | reorder UI | port ベース drop 検証 + section 跨ぎ write-back（無効は元に戻す） | — |
| 9 | engine | 検証のみ（変更なし） | — |
| 10 | 検証 | workspace build/test/clippy + 子バイナリ rebuild + 実機 smoke | — |

- 全 Step **daw_01 単独**（gui_01 要望なし）。
- `PluginEntry`/`PluginDatabase` は IPC bincode 型ではなく JSON cache 専用なので bincode derive 連鎖は
  無いが、`common` 変更後は規律どおり `cargo build --workspace` を 1 回通す。
- probe subprocess は有界化（既存 8s timeout 踏襲）。
