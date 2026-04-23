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

### IPC — 制御プレーン (named pipe + bincode)

`common/src/{protocol,wire,pipe,client}.rs`。

プロトコル:
```rust
pub enum ChildToMain {
    Hello { kind: ChildKind, pid: u32 },
}
pub enum MainToChild {
    Ack,
    Play,
    Stop,
    Session(AudioSession),
    LoadSong(Song),
}
pub struct AudioSession {
    shmem_id: String, request_sem_id: String, ready_sem_id: String,
    sample_rate: u32, max_frames: u32, channels: u16,
}
```

フロー:
1. daw_gui が `\\.\pipe\daw_01_{pid}_{kind}` を server 作成
2. 子プロセスを spawn（JobObject で紐付け）し、pipe 名を第 1 引数で渡す
3. 子が `ClientOptions::new().open(...)` で connect → Hello 送信
4. daw_gui が Ack 返信
5. daw_gui が Session メッセージを送信（shmem / セマフォ名 / sample_rate / max_frames）
6. 以降は `Play` / `Stop` / `LoadSong` を送る
7. ウィンドウ close → pipe drop → 子が EOF 検知して正常終了

フレーミング: 4 byte little-endian 長プレフィクス + bincode body（16 MB 上限で DoS 防御）。

### IPC — データプレーン (shared memory + Win32 セマフォ)

`common/src/{audio_bridge,win_sem}.rs`。

```rust
#[repr(C)]
pub struct AudioBridge {
    frames_requested: AtomicU32,  // daw_audio が書き込む
    _pad: u32,
    samples: [f32; 2048],          // 1024 frames × 2ch interleaved
}
```

固定値: `SAMPLE_RATE=48000`, `MAX_FRAMES=1024`, `CHANNELS=2`。

同期: 2 つの名前付きセマフォを使った往復。
- `request_sem`: daw_audio → daw_plugin_host、「N フレーム埋めろ」
- `ready_sem`:  daw_plugin_host → daw_audio、「書き終えた」

daw_audio の CPAL コールバックで 1 往復（RT スレッドが wait するが、plugin_host の process 時間が短ければ OK）。

### 対応 OS

- Windows (primary)
- Linux (将来。OS 依存は薄い抽象化レイヤに閉じ込める)
- macOS はスコープ外

### 対応 OS

- Windows (primary)
- Linux (将来。OS 依存は薄い抽象化レイヤに閉じ込める)
- macOS はスコープ外

### プロセス寿命管理

- daw_gui が `windows` crate の `CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` で Job Object を作成
- 子プロセスを spawn 直後に `AssignProcessToJobObject` で紐付ける
- daw_gui が正常終了・panic・強制終了のいずれでも、OS が handle を閉じた時点で子プロセスが kill される（ゾンビ防止）

## 技術スタック

| コンポーネント | 技術 | 実装状況 |
|---|---|---|
| 言語 | Rust (Edition 2024) | ✅ |
| GUI | Vizia 0.3.0 (winit + Skia) | ✅ Lens ベース、HackGen Console NF |
| オーディオ I/O | cpal 0.15 (WASAPI 共有モード、F32) | ✅ |
| Control plane IPC | tokio named pipe + bincode 2 | ✅ |
| Data plane IPC | shared_memory 0.12 + windows crate (名前付きセマフォ) | ✅ |
| CLAP ホスト | clap-sys 0.5 + libloading 0.8 | ✅ |
| VST3 (M2) | vst3-sys (候補) | 未着手 |
| VOICEVOX 通信 | reqwest (async HTTP) | 未着手 |
| JSON シリアライズ | serde + serde_json (プロジェクトファイル) | ✅ |
| バイナリシリアライズ | bincode 2 (IPC) | ✅ |
| Async runtime | tokio (sync / rt-multi-thread / net / macros / io-util / process) | ✅ |
| 文字幅計算 | unicode-width 0.2 (CJK lyric セル padding) | ✅ |
| Lock-free 共有 | arc-swap 1 (Song を audio thread へ wait-free 配布) | ✅ |
| MIDI (将来) | midir / midly / wmidi | 未着手 |
| ファイルダイアログ | rfd 0.15 | ✅ |

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

## CLAP 統合

`daw_plugin_host/src/{scan,clap_host,plugin}.rs`。

### スキャン
- 起動時に `%COMMONPROGRAMFILES%\CLAP` (e.g. `C:\Program Files\Common Files\CLAP`) 直下の `.clap` を列挙
- 先頭の「`features` に `"instrument"` を含む」プラグインを自動選択
- 無ければ最初にロードできるものに fallback
- 開発中は `DAW_CLAP_PATH` 環境変数で特定の `.clap` を指定できる

### ライフサイクル
CLAP 仕様のスレッド規約（`@[main-thread]` / `@[audio-thread]`）に従う:

```
[main-thread]
Library::new(.clap) → clap_entry → entry.init(path)
  → factory.get_plugin_descriptor × n (ログ)
  → factory.create_plugin(host, id) → plugin.init
  → plugin.get_extension(CLAP_EXT_AUDIO_PORTS / CLAP_EXT_NOTE_PORTS)
  → plugin.activate(48000, 64, 1024)
[audio-thread] (専用 std::thread)
  → plugin.start_processing → process loop → plugin.stop_processing
[main-thread]
  → plugin.deactivate → plugin.destroy → entry.deinit → Library drop
```

### audio thread のループ

```
request_sem.wait_timeout_ms(100)
  ↓
(note_state が変わっていたら) NoteTransition を TimedNoteEvent に変換
  ↓
Song の Track 0 / Clip 0 の rows を walk し、[playhead, playhead+frames) の
各 row から TimedNoteEvent を生成（モノフォニック再トリガー: 新 NoteOn は
active_notes を先に off → 新 on）
  ↓
plugin.process(frames, &events) → planar f32 出力
  ↓
先頭 2 ch を interleaved に変換して AudioBridge.samples に書き込み
  ↓
ready_sem.release()
```

### RT 安全性
- `pending_events: Vec<clap_event_note>` は capacity 64 で activate 時に事前確保、push のみ
- `output_buffers: Vec<Vec<f32>>` と `output_ptrs: Vec<*mut f32>` も activate 時確保
- 入力イベント用 vtable は process 毎にスタック上で構築、ctx は `&self.pending_events` を指す
- 出力イベント vtable は `const` static（現状は try_push で捨てる）

### Song 再生
- `Play` 時に daw_gui が `LoadSong(Song)` と `Play` を送る
- plugin_host の recv_loop が `ArcSwapOption<Song>` に swap、audio thread は `load` で wait-free に取得
- `samples_per_row = sample_rate * 60 / bpm / rows_per_beat` で行 → サンプル変換
- clip 終端 (`start_beat + length_beats`) を越えたら自動停止 + 残留ノートを off

## GUI (Vizia)

### レイアウト (daw_gui/src/main.rs)

```
┌─ VStack ────────────────────────────┐
│ Menu (File / Track)                 │
│ Transport (Play/Stop, pos, BPM)     │
│ HStack                              │
│  ├ TrackInspector (220px)           │
│  └ ArrangementView (stretch)        │
│ StatusBar (file path)               │
└─────────────────────────────────────┘
```

### 状態 (daw_gui/src/app.rs の AppData)

`#[derive(Lens)]` で以下を Lens 化:
- `song: Song`
- `file_path: Option<PathBuf>`
- `cursor_row: u32`, `cursor_track: u32`
- `tracker_text: String` — 描画用の事前レンダリング結果
- `last_note: Note` — 空セル transpose のテンプレート
- `is_playing: bool`

`#[lens(ignore)]`:
- `audio_tx: Option<UnboundedSender<MainToChild>>`
- `plugin_tx: Option<UnboundedSender<MainToChild>>`

`Song` は Vizia の `Data` trait を実装していないため `Binding` には使えない。
state 変更時に `refresh_tracker_text()` で文字列化し、それを Lens 経由で Label に渡す方式。

### キーバインド (sing_like_coding 準拠)

| キー | 動作 |
|---|---|
| h / j / k / l | カーソル track±1 / row±1 |
| Space | Play / Stop トグル |
| Ctrl+Space | Play from cursor（将来） |
| Ctrl+J / Ctrl+K | transpose -1 / +1 セミトーン（空セルや Off は last_note をそのまま配置） |
| Ctrl+H / Ctrl+L | transpose -12 / +12 |
| N | NoteOff を配置 |
| Delete | セル clear |
| Ctrl+N / O / S / Shift+S | New / Open / Save / Save As |

## マイルストーン

### M1: 使える DAW

- [x] 3 プロセス起動 + IPC ハンドシェイク
- [x] Job Object によるプロセス寿命管理
- [x] Named pipe + bincode の制御プレーン (Play / Stop / Session / LoadSong)
- [x] Shared memory + セマフォのデータプレーン
- [x] Audio Engine: CPAL (WASAPI) 経由でオーディオ出力
- [x] GUI: Vizia で 4 パネルレイアウト + メニュー
- [x] Arrangement トラッカーグリッド描画（HackGen フォント、CJK 幅合わせ）
- [x] hjkl ナビゲーション + ノート編集
- [x] CLAP: Plugin Host で instrument プラグイン scan / load / activate / process
- [x] Song データ駆動の再生（Track 0 / Clip 0、モノフォニック）
- [x] プロジェクト保存・読込 (`.daw` JSON アトミック書き込み)
- [x] プラグイン選択 GUI (Track Inspector, ホットスワップ対応)
- [x] ループ再生 (`P` キー / Transport ボタン、clip 範囲をシームレス wrap)
- [ ] オートセーブ + 起動時復元プロンプト
- [ ] VOICEVOX: 歌詞入力 → sing 合成 → WAV キャッシュ → 再生
- [ ] WAV 書き出し
- [ ] velocity / lyric / FX 列の編集

### M2 以降

- [ ] VST3 対応
- [ ] ピアノロール
- [ ] ミキサー GUI
- [ ] オートメーション
- [ ] MIDI 入出力
- [ ] Linux 対応
- [ ] プラグイン GUI 埋め込み

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
