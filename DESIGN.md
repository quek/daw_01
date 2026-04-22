# 設計書

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。Clip ベースのタイムラインに
トラッカー UI で入力し、CLAP/VST3 プラグインに対応する。

## アーキテクチャ

### プロセス構成

3 つの独立した実行ファイル（別プロセス）で構成する。

| プロセス | 役割 |
|---|---|
| **daw_gui** | UI 表示・編集操作。Vizia ベース |
| **daw_audio** | オーディオ出力・シーケンサー・ミキサー。CPAL (WASAPI) |
| **daw_plugin_host** | CLAP/VST3 プラグインのロード・実行 |

```
daw_gui <──IPC──> daw_audio <──IPC──> daw_plugin_host
  │                  │                     │
  │ 制御メッセージ    │ オーディオバッファ    │ プラグイン process()
  │ (named pipe)     │ (shared memory)     │ (shared memory)
```

### IPC

**Data plane (realtime):**
- Shared memory + lock-free ring buffer + OS セマフォ
- Audio Engine ⟷ Plugin Host 間のオーディオバッファ受け渡し
- Windows: `CreateFileMapping` + `CreateSemaphore`
- Linux (将来): `shm_open` + `sem_open`
- リアルタイムスレッドでは絶対にアロケート・ロック・システムコールをしない

**Control plane (non-realtime):**
- Named pipes + bincode 直列化
- GUI → Audio Engine: 再生/停止、Song 状態更新、Clip 変更通知
- GUI → Plugin Host: プラグインロード/アンロード、パラメータ変更、GUI 表示
- 制御プレーンと data プレーンは別チャネルに分離

候補 crate: `shared_memory`, `rtrb`, `miow`, `bincode`

### 対応 OS

- Windows (primary)
- Linux (将来。OS 依存は薄い抽象化レイヤに閉じ込める)
- macOS はスコープ外

## 技術スタック

| コンポーネント | 技術 |
|---|---|
| 言語 | Rust (Edition 2024) |
| GUI | Vizia (v0.3+, winit backend) |
| オーディオ I/O | cpal (WASAPI、排他モード優先) |
| IPC | shared_memory + rtrb + miow (named pipes) |
| CLAP ホスト | clap-sys + libloading |
| VST3 (M2) | vst3-sys (候補) |
| VOICEVOX 通信 | reqwest (async HTTP) |
| シリアライズ | serde + serde_json / bincode |
| Async runtime | tokio |
| MIDI (将来) | midir / midly / wmidi |

## データモデル

### 階層構造

```
Song → Track → Clip → Row
```

Pattern / Song Order 方式は不採用。Clip ベースのタイムラインを採用する。

**選定理由:**
歌もの（VOICEVOX）ではアウフタクト（弱起）が自然に発生し、フレーズが小節境界を
跨ぐ。Pattern の固定長区画とは根本的に相性が悪い。Clip ベースならフレーズ単位で
自由配置でき、VOICEVOX の合成単位（1 フレーズ → 1 WAV）とも自然にマップする。

### 構造定義

```rust
struct Song {
    bpm: f32,
    time_sig: (u8, u8),
    tracks: Vec<Track>,
    length_beats: f64,
}

struct Track {
    name: String,
    source: InstrumentSource,
    fx_chain: Vec<PluginInstance>,
    volume: f32,
    pan: f32,
    clips: Vec<Clip>,
}

enum InstrumentSource {
    Vocal { speaker_id: u32, style_name: String },
    Clap { path: PathBuf },
    Vst3 { path: PathBuf },
    BuiltinSynth,
}

struct Clip {
    name: String,
    start_beat: f64,    // 小数・負値 OK（アウフタクト対応）
    length_beats: f64,
    rows_per_beat: u16, // グリッド解像度（4 = 16分音符）
    rows: Vec<Row>,
}

struct Row {
    note: Option<Note>,
    volume: Option<u8>,
    fx: Option<(u8, u8)>,   // (command, value)
    lyric: Option<String>,  // 1 モーラ。Vocal トラックのみ使用
}
```

### 設計判断

- **INS 列なし**: 音源はトラックに紐付く（モダン DAW 流）
- **Lyric 列**: Vocal トラックのみ UI 表示。データ上は全 Row に存在
- **Clip 配置**: `start_beat` は f64、負値可（アウフタクト対応）
- **将来拡張**: `Vec<Row>` → `Vec<Event { time_beat, ... }>` でピアノロール化

## VOICEVOX 統合

### 方式

HTTP API。DAW が VOICEVOX Engine をサブプロセスとして自動起動し、
`http://localhost:50021` を叩く。事前合成 + キャッシュ方式。

### API フロー

**歌唱 (Sing):**
1. Clip の Row データから JSON を構築
   ```json
   {"notes": [{"id": "note1", "key": 60, "frame_length": 93, "lyric": "こ"}, ...]}
   ```
2. `POST /sing_frame_audio_query?speaker=<query_speaker_id>` → audio query JSON
3. `outputSamplingRate` を 48000 に書き換え
4. `POST /frame_synthesis?speaker=<singer_id>` → WAV バイナリ

**トーク (Talk):**
1. `POST /audio_query?speaker=<id>&text=<url_encoded>` → audio query JSON
2. `POST /synthesis?speaker=<id>` → WAV バイナリ

### 定数

| 項目 | 値 |
|---|---|
| Engine URL | `http://localhost:50021` |
| フレームレート | 93.75 Hz (24000 / 256) |
| REST_FRAMES | 10 (前後パディング) |
| QUERY_SPEAKER | 6000 (波音リツ、クエリ生成用固定) |
| OUTPUT_SAMPLE_RATE | 48000 |

### 歌詞分割

小書きかな（ぁぃぅぇぉゃゅょっ等）は直前の文字と結合して 1 モーラ化。

### 合成タイミング

Clip 内容変更時に VOICEVOX Engine で合成 → WAV をキャッシュ（Clip ID + content hash）
→ 再生時は Audio Engine がキャッシュ済み WAV を読み出す。

## UI 設計

### ビュー構成

1. **Arrangement View** — タイムライン × トラック。Clip を配置・移動・リサイズ
2. **Clip Editor (Tracker View)** — 選択した Clip の中身をトラッカー UI で編集
3. **Track Inspector** — トラックの音源・FX チェイン設定

### キー操作

カーソル移動: `h` (左) `j` (下) `k` (上) `l` (右)

### Arrangement View

```
       1   2   3   4   5   6   7   8   9
TRK1 ▶ │[こんにちは    ]    │  ┌[━さようなら────]
TRK2   │[━━━━━━━ Bass Loop ━━━━━━━━━━━━━━━━]
TRK3   │[Drum A  ][Drum A  ][Drum B  ]
```

### Clip Editor (Tracker View)

```
TRK1 Clip "こんにちは"   Start:bar1.0  Len:4bar
 R# │NOT VOL FX  LYR
 ───┼────────────────
 00 │C-4 40  --- こ
 01 │--- --  --- -
 02 │D-4 40  --- ん
 03 │--- --  --- -
 04 │E-4 3E  --- に
 05 │--- --  --- -
>06 │D-4 40  --- ち       ← カーソル行
 07 │--- --  --- -
 08 │C-4 40  A08 は
 09 │--- --  --- ー       ← 母音延長
```

### Track Inspector

```
TRK1 ▶ Vocal
  Source: VOICEVOX  Speaker: ずんだもん  Style: あまあま
  FX: [EQ] > [Reverb]
TRK2   Bass
  Source: CLAP  Serum.clap
  FX: [Compressor]
```

## ワークスペース構成

```
daw_01/
├── Cargo.toml              # workspace root
├── CLAUDE.md
├── DESIGN.md
├── Makefile
├── common/                 # 共有型・IPC プロトコル・shared memory
│   └── src/
│       ├── lib.rs
│       ├── protocol.rs     # メッセージ enum (bincode 直列化)
│       ├── shmem.rs        # shared memory 抽象化
│       ├── audio_buffer.rs # 固定サイズオーディオバッファ
│       ├── model.rs        # Song / Track / Clip / Row
│       └── event.rs        # Note / CC / Automation イベント
├── daw_gui/                # GUI プロセス (Vizia)
│   └── src/
│       ├── main.rs
│       ├── app.rs          # アプリケーション状態
│       ├── command/        # ユーザーアクション
│       ├── communicator.rs # Audio/Plugin プロセスとの通信
│       └── view/           # Vizia ビュー (Arrangement, ClipEditor, Inspector)
├── daw_audio/              # Audio Engine プロセス
│   └── src/
│       ├── main.rs
│       ├── engine.rs       # オーディオコールバック・シーケンサー
│       ├── mixer.rs        # ミキシング
│       ├── voicevox.rs     # VOICEVOX HTTP クライアント + WAV キャッシュ
│       └── communicator.rs # GUI/Plugin プロセスとの通信
├── daw_plugin_host/        # Plugin Host プロセス
│   └── src/
│       ├── main.rs
│       ├── host.rs         # CLAP/VST3 ホスト実装
│       ├── clap_manager.rs # CLAP スキャン・ロード
│       ├── plugin.rs       # プラグインインスタンス管理
│       └── communicator.rs # Audio プロセスとの通信
```

## マイルストーン

### M1: 使える DAW

- [ ] 3 プロセス起動 + shared memory IPC 通信
- [ ] Audio Engine: CPAL (WASAPI) 経由でオーディオ出力
- [ ] GUI: Vizia で Arrangement View + Clip Editor 表示
- [ ] Clip Editor: トラッカー行編集、hjkl 移動、ノート入力
- [ ] VOICEVOX: 歌詞入力 → sing 合成 → WAV キャッシュ → 再生
- [ ] CLAP: Plugin Host 経由で CLAP プラグインをロード、オーディオ通過
- [ ] プロジェクト保存・読込
- [ ] WAV 書き出し

### M2 以降

- [ ] VST3 対応
- [ ] ピアノロール
- [ ] ミキサー GUI
- [ ] オートメーション
- [ ] MIDI 入出力
- [ ] Linux 対応

## 参照

| プロジェクト | 参考ポイント |
|---|---|
| sing_like_coding (自作前作) | IPC, CLAP ホスト, オーディオエンジン, コマンドパターン |
| REAPER VOICEVOX スクリプト (自作) | VOICEVOX API フロー, 歌詞分割, 自動起動 |
| [Renoise](https://www.renoise.com/) | トラッカー UI, オートメーション |
| [clap-host (free-audio)](https://github.com/free-audio/clap-host) | CLAP ホストリファレンス (C++) |
| [clack](https://github.com/prokopyl/clack) | Rust 製 CLAP ライブラリ |
| [Meadowlark](https://github.com/MeadowlarkDAW/Meadowlark) | Rust 製 DAW, RT オーディオ |
| [nih-plug](https://github.com/robbert-vdh/nih-plug) | Rust 製プラグインフレームワーク |
