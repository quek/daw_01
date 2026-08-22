<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Routing Graph + Group / PDC / Sidechain / Parallel Output 実装計画

## Context

ユーザー要望: Reaper / Ableton Live のようなグループトラック・サイドチェイン・PDC（プラグイン遅延補償）・**プラグインのパラアウト（マルチ出力）** の実装。

**現状（2026-05-05）:**
- `daw_audio/src/tracks.rs::AudioRouting` は flat `Vec<TrackRouting>`、`engine.rs::reduce_master` (L747-764) が全トラックを `master_l/r` に直接加算
- `daw_plugin_host/src/clap_plugin.rs::query_port_channel_count` (L799-828) は **port 0 のみ** 取得、`is_main` フラグを無視、aux/sidechain port 完全未対応
- CLAP_EXT_LATENCY 関連コード一切なし（host vtable も plugin.get() も 0%）
- `common/src/model.rs::Track` (L141-176) にルーティング情報（`parent_group_id` / `send` / `sidechain_source`）なし

4 機能はいずれも「Track / Plugin 間の信号フロー」を必要とする：
- グループ → 子→親の DAG
- PDC → 各 sink への到達経路ごとの累積 latency 計算
- **サイドチェイン → 任意 track の出力を別 track のプラグインの追加 input port (is_main=false) にタップ**
- **パラアウト → プラグインの追加 output port (is_main=false) を別 track / bus へルーティング**

サイドチェインとパラアウトは **「is_main=false の追加 audio port」のホスト側ルーティング** という同根の問題で、CLAP `audio_ports` 拡張の input/output 全走査と ProcessData の aux buffer slot で統一対処できる。

これらを **共通基盤 = signal graph + topological schedule** の上に統一実装する（CLAUDE.md「DRY / SSOT」「ベストプラクティス」「大胆に破壊して作り直す」精神）。

**決定済み仕様:**

| 項目 | 採用 | 根拠 |
|---|---|---|
| PDC モード | **自動全補償 (Live 風)** | ユーザー確認 (2026-05-05)。全 sink (Master + sidechain dest) のパスを常時均等化 |
| グループ signal flow | **Live 風**: children post-fader → group input → group fx → group fader → upstream | 現代的標準。Live / Cubase / Studio One 一致 |
| グループのネスト | **無制限** | Live 11+ / Reaper / Cubase 同様、技術的にも特に難しくない |
| シリアライズ | **v4 → v5 自動 migrate** | `#[serde(default)]` で既存 .daw ファイル読込維持、開発中データ保護 |
| VST3 サポート | **本計画スコープ外** (M2 で別途) | DESIGN.md M2 マイルストーン |
| **パラアウトのルーティング先** | **任意トラック / グループ / Master**（送り元 plugin と同じ track 内で次プラグインへも可） | Reaper の "Multichannel routing" / Live の "Multi-out plugin → audio track" 同等 |
| **パラアウト本数の上限** | **MAX_AUX_OUT = 4** (= 2-out drum sampler 1 系統 + spare) | 一般的なドラムサンプラー (Battery / Geist) で 4 stereo out まで足りる。後で拡張可 |
| 段階リリース | **4 PR** | smoke test を各段階で維持、回帰検出可 |

## 実装状況: Send / Return (2026-05-30 実装・実機確認済み)

PR4 の **send/return 部分**を実装。group / sidechain / PDC は既存基盤を流用。

- model: `Track::sends: Vec<Send>` / `Send { dest_track_id, gain, mode:
  SendMode{PostFader, PreFader}, enabled }`。リターンは派生 (incoming send を
  持つ track、新 TrackKind 無し)。v16→v17 forward-migrate、ensure_ids で send
  dest を remap。
- graph (`compile.rs`): 各 send を宛先バスの入力に `NodeOp::MixSend` で追加。
  emission 順序を parent+sidechain+send 依存の post-order に一般化、cycle 検出に
  send エッジ、PDC `compute_path_latency` に send fan-in。
- engine (`engine.rs`): `MixSend` を post-dispatch で **live send gain**
  (`SendGain` 自動化 / ノブ drag を recompile 無し per-sample) で加算。pre-fader
  タップは strip 前に `TrackScratch.pre_fader_l/r` へ snapshot。
- IPC: realtime `SetSendGain` / `SetSendEnabled` (構造変更は LoadSong 再コンパ
  イル)。
- GUI: mixer strip の Sends セクション (level knob + pre/post + mute + remove)、
  リターンを右帯に表示、Add Return、`track_picker`、SendGain 自動化ジェスチャ。
  group 兼 return は通常帯に残す (階層保持)。

### ソロ × センドの規範挙動 (実機検証で確定)

- **明示 mute** はトラックの dry / send / sidechain を全て止める (`track_l/r` を
  zero 化)。
- **ソロ除外** (`any_solo && !solo`) は `track_l/r` を **zero 化しない** — master
  / group mix は `effective_mute` フラグで dry を除外するが、信号は send /
  sidechain 用に保持する。
- **リターンは solo-safe**: `has_soloed_contributor` が send 元エッジも辿り、
  ソロしたトラックの send 先リターンを生かす (ソロ中もセンドが鳴る)。
- **send は solo を尊重**: ソロ中、send が流れるのは「送り元が solo-audible」
  または「宛先リターンが明示 Solo (audition)」のときだけ。あるトラックを Solo
  しても、同じリターンへ送る別トラックの send は漏れない。リターンを Solo すると
  そのリターンへの全 send を audition できる。
- sidechain は send と非対称: solo-excluded source の信号は SidechainTap で常に
  タップされる (ソロ中も ducking 継続 = ミックスでの聞こえ方を維持)。

commit: model/protocol/graph/engine/gui 一括 `35b2129`、solo-safe `2344a1a`、
return audition `a2854d0`、solo leak 修正 `7631178`。

## アーキテクチャ

### Signal Graph（共通基盤）

新設 `daw_audio/src/graph/`：

```rust
pub struct Schedule {
    pub nodes: Vec<NodeOp>,          // RT は for op in nodes { dispatch(op) }
    pub delay_lines: Vec<DelayLine>, // PDC 補償用 ring buffer pool
    pub port_buffers: PortBufferPool,// node 出力 buffer 共有プール
}

pub enum NodeOp {
    ProcessTrack { track_id: u32, scratch_idx: u32 },
    ApplyDelay { line_idx: u32, frames: u32 },
    MixToBus { srcs: SmallVec<[(BufRef, f32); 8]>, dst: u32 },  // (BufRef, gain)
    ProcessBusFx { track_id: u32, scratch_idx: u32 },
    SidechainTap { src: BufRef, dst_plugin: PluginRef, aux_in_port: u8 },
}

// Buffer 参照は「track scratch」「plugin の特定 output port」を統一表現
pub enum BufRef {
    NodeMain(u32),                              // node の主出力 (track scratch)
    PluginAuxOut { plugin: PluginRef, port: u8 }, // パラアウト用、is_main=false の output port
}
```

`Arc<Schedule>` を `ArcSwap` でホットスワップ。GUI 編集（group 追加 / send 変更 / latency 通知）→ `compile_schedule(&Song, &PluginLatencies) -> Result<Schedule, GraphError>` → swap。RT スレッドは毎バッファ最初に `arc_swap.load()` の 1 ポインタ読みのみ。

### compile_schedule のステップ
1. **トポロジカルソート** (Kahn): track 親子 + send + sidechain edge を DAG として依存解決
2. **サイクル検出**: DAG でなければ `GraphError::Cycle` → GUI で赤枠表示
3. **PDC 累積計算**: 各 node `latency_in = max(parent_path_latency)`、各 sink で `compensation = max_path_latency - this_path_latency` を `ApplyDelay` として node 入力境界に挿入（Ardour 流、`libs/ardour/latent.cc` 参照）
4. **DelayLine プール**: edit 前後の差分のみ確保、RT スレッドへ allocation push しない

### Track データモデル (`common/src/model.rs`)

```rust
pub enum TrackKind {
    Audio,
    Group,  // children を fx_chain で処理してまとめて upstream へ
}

pub struct Track {
    // 既存維持: id, name, instrument, midi_fx_chain, fx_chain,
    //          volume, pan, muted, solo, source, clips, next_clip_id
    #[serde(default)] pub kind: TrackKind,            // PR1
    #[serde(default)] pub parent_group_id: Option<u32>, // PR1
    #[serde(default)] pub sends: Vec<Send>,            // PR4
    #[serde(default)] pub reported_latency_samples: u32, // PR3, plugin host 由来
}

pub struct Send {
    pub dest_track_id: u32,
    pub gain: f32,
    pub mode: SendMode,  // Pre | Post
    pub enabled: bool,
}

pub struct PluginInstance {
    // 既存維持
    #[serde(default)] pub sidechain_sources: Vec<Option<u32>>,  // PR4: aux input port_idx → source track_id
    #[serde(default)] pub output_routes: Vec<Option<u32>>,       // PR4: aux output port_idx → dest track_id (パラアウト)
}
```

シリアライズ: `Song::CURRENT_VERSION = 4 → 5`、`#[serde(default)]` で v4 ファイルも読込可（kind=Audio, parent=None, sends 空）。bincode IPC は新フィールドを末尾追加。

### Audio Engine 改修 (`daw_audio/src/`)

**削除・置換:**
- `tracks.rs::AudioRouting` (flat Vec) → `Schedule` で代替
- `engine.rs::reduce_master` L747-764 → schedule の Master sink ノードに統合
- `engine.rs::process_buffer` L320-514 の `dispatch_and_wait` 呼び出し → `schedule.execute(&workers, &mut node_scratch_pool)` に置換

**保持・分解:**
- `process_track_owned` L530-741 を 3 ハンドラに分解（既存ロジックを温存しつつ NodeOp ハンドラ化）：
  - `op_collect_midi(track, scratch)` — MIDI bus assemble（既存 MIDI FX / Instrument 経路）
  - `op_run_plugin_chain(slot_kind, scratch, workers)` — MIDI FX / Instrument / Audio FX を共通化
  - `op_apply_strip(scratch, params)` — volume/pan/solo/mute（最終段または bus 入力前）

**Scratch buffer:**
- `mixer.rs::TrackScratch` → `NodeScratch` に汎化。`Vec<NodeScratch>` を node ID で index する `NodeScratchPool`
- 容量: MAX_TRACKS=32 + MAX_GROUPS=8 + MASTER 1 = 41、activate 時に固定確保

**RT 安全性:**
- Schedule は immutable + ArcSwap で wait-free
- DelayLine は固定 ring buffer、RT で write/read のみ
- `port_buffers` は activate 時確保した固定 `Vec<Vec<f32>>`、frame 毎に上書き

### Plugin Host 拡張 (`daw_plugin_host/`)

**CLAP_EXT_AUDIO_PORTS 全走査** (`clap_plugin.rs` L799-828 を拡張):
```rust
struct PortLayout {
    main_in: Option<PortInfo>,        // is_main=true な input port (instrument は無いことも)
    main_out: PortInfo,               // is_main=true な output port
    aux_inputs: Vec<PortInfo>,        // is_main=false な input port = サイドチェイン候補
    aux_outputs: Vec<PortInfo>,       // is_main=false な output port = パラアウト候補
}
fn query_ports(&self) -> PortLayout {
    // input 全走査
    let count_in = count_fn(plugin, true);
    for i in 0..count_in {
        let info = get(plugin, i, true);
        if info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0 { main_in = Some(info) }
        else { aux_inputs.push(info) }
    }
    // output 全走査（同じパターン、is_input=false）
    // -> パラアウトはここで aux_outputs に集まる
}
```

サイドチェインとパラアウトは **同じ全走査ループ** で対処（is_input フラグだけ差し替え）。

**CLAP_EXT_LATENCY 実装** (`clap_host.rs` L104-122 + 新規 `clap_latency.rs`):
```rust
const CLAP_HOST_LATENCY_VTABLE: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};
extern "C" fn host_latency_changed(host: *const clap_host) {
    // host_data から ChildToMain::PluginLatencyChanged をポスト
    // → daw_gui の handle_event で compile_schedule を再実行 → ArcSwap
}

fn query_latency(&self) -> u32 {
    let ext = plugin.get_extension(CLAP_EXT_LATENCY) as *const clap_plugin_latency;
    if ext.is_null() { return 0; }
    unsafe { (*ext).get(plugin) }
}
```

CLAP spec (`clap/ext/latency.h`): latency 変化時は `deactivate → get → activate` 必須。`changed` 内では即座に再 activate せず main-thread に flag を立てて次の音切れ目で実施。

**ProcessData 拡張** (`common/src/process_data.rs`):
```rust
pub const MAX_AUX_IN: usize = 2;      // sidechain 1 + spare
pub const MAX_AUX_OUT: usize = 4;     // ドラムサンプラー 4 stereo パラアウト
pub struct ProcessData {
    // 既存 frames / events_in/out / buffer_in/out (main port) 維持
    pub buffer_aux_in: [[[f32; MAX_FRAMES]; 2]; MAX_AUX_IN],    // ch=2 固定、サイドチェイン入力
    pub buffer_aux_out: [[[f32; MAX_FRAMES]; 2]; MAX_AUX_OUT],  // ch=2 固定、パラアウト出力
    pub aux_in_active: [u8; MAX_AUX_IN],                         // 1 = この aux in に信号あり
    pub aux_out_active: [u8; MAX_AUX_OUT],                       // 1 = この aux out が routes 設定済
    pub latency_samples: AtomicU32,                               // plugin → host 通知
}
```

ABI 破壊は **PR3 で latency_samples 追加 + PR4 で aux_in/aux_out 一括追加** を 2 段に分ける（PR3 単独で test 可能性を維持）。

**process() 拡張** (`clap_plugin.rs` L517-548):
- `clap_audio_buffer` 配列を input 側 `[main_in, aux_in_1, aux_in_2]` (最大 3)、output 側 `[main_out, aux_out_1..aux_out_4]` (最大 5) で生成
- aux_in_active=0 の入力 port は `data32: nullptr` で渡す（CLAP spec の no-signal）
- aux_out 側は常に有効バッファを渡す（ホスト側で aux_out_active を見て routing 判断）

### GUI 改修 (`daw_gui/src/view/`)

**mixer_strips.rs** (PR2):
- 階層インデント表示（Live 流）、group track strip は背景色を変えて区別
- 折り畳みボタン (`AppData::collapsed_groups: HashSet<u32>`)

**arrangement_view.rs** (PR2):
- parent track 行の下に子をぶら下げて indent 表示
- drag-and-drop で `parent_group_id` 変更
- right-click menu: "Group selected tracks" / "Move to Group..."

**新規 view/track_picker.rs** (PR4):
- `plugin_picker.rs` パターン流用、modal + list_view で track list 表示
- 用途: サイドチェイン source 選択 + パラアウト destination 選択 を共通化
- Inspector のプラグイン行に
  - "SC[port]: [None / TrackName]" ボタン (aux_inputs ごと)
  - "Out[port]: [Default(=main) / TrackName]" ボタン (aux_outputs ごと)
- `AppEvent::SetSidechainSource { plugin_slot, in_port_idx, source_track_id }`
- `AppEvent::SetPluginOutputRoute { plugin_slot, out_port_idx, dest_track_id }`

**Inspector の latency 表示** (PR3):
- track header に `+128 spl (2.7ms)` 形式
- master の cumulative latency を transport bar 横に表示

## 段階リリース計画

### PR1: Signal graph 骨格（model + graph mod のみ、engine は触らない）
- `Track::kind`, `parent_group_id`, `reported_latency_samples` フィールド追加 (default Audio / None / 0)
- Song version 4 → 5 + serde default migrate（v4 .daw 自動 forward-compat）
- 新設 `daw_audio/src/graph/{mod.rs, schedule.rs, compile.rs, delay_line.rs, port_buffer.rs}`
  - `compile_schedule` は flat (Audio track → Master) のみ。group は無視
  - graph mod は dead_code allow で engine からは呼ばない
- 既存挙動完全保持。**model migrate test + graph 単体テスト**だけで完結
- 単独 PR で「engine を flat 用に置換」する意義が薄いため、engine 切替は PR2 に統合

### PR2: engine の schedule 駆動化 + グループトラック

> 詳細仕様 (UI / RT / model / 制約 / テスト計画) は [`plan_group_track.md`](plan_group_track.md) に分離。
- `compile_schedule` に group 階層対応：children → group node → upstream（Kahn ソート + サイクル検出）
- `Schedule::execute` を実装、`engine.rs::process_buffer` を schedule 駆動に置換
- 既存 `reduce_master` 削除、Master = `Mix { dst: Master }` ノードに統合
- `op_run_plugin_chain` の audio-only 経路を group fx に流用（既存 `process_track_owned` の audio FX ループを汎化）
- `AudioWorkerPool` を schedule の `ProcessTrack` / `ProcessGroupFx` ノード dispatch に改修（または既存 dispatch_and_wait を薄くラップ）
- `TrackScratch` を `NodeScratch` に汎化、`Vec<NodeScratch>` プール
- GUI: mixer 階層、arrangement 折り畳み、新規グループ作成、drag-and-drop
- **smoke test 1 (engine 切替)**: 既存 demo `.daw` 再生で master 音が PR1 前と sample-exact 一致（`hound` で WAV diff、graph 経由でも flat 動作が一致）
- **smoke test 2 (group)**: 2 track をグループ化 → group fx に reverb → 子トラックが両方 reverb 経由、group volume で子が一括変動

### PR3: PDC
- CLAP_EXT_LATENCY 実装（host vtable + plugin.get()）
- `Track::reported_latency_samples` を IPC 経由更新（`ChildToMain::PluginLatencyChanged`）
- `compile_schedule` に PDC accum 計算 + DelayLine 挿入
- ProcessData に `latency_samples: AtomicU32` 追加
- GUI: track header に latency 表示、master cumulative latency 表示
- **smoke test**: 高 latency CLAP（linear-phase EQ）と通常 CLAP を 2 track で同時再生 → master で位相揃い確認（同位相反転 mix で 0 になる）

### PR4: サイドチェイン + パラアウト（マルチポート routing 統合）
- ProcessData に `buffer_aux_in[MAX_AUX_IN]` + `buffer_aux_out[MAX_AUX_OUT]` + active flag 追加（ABI break 2 回目）
- CLAP audio_ports 全走査（input + output 両方）、is_main 区別、aux 分類
- `PluginInstance::sidechain_sources` + `output_routes` 追加
- 新規 `view/track_picker.rs`（サイドチェイン source / パラアウト destination 共通の track 選択 modal）
- `compile_schedule` 拡張:
  - sidechain edge: source track → dest plugin の aux_in
  - パラアウト edge: source plugin の aux_out → dest track / group / master 入力
  - PDC とインタラクト: 両方とも path latency 計算に組み込み
- `BufRef::PluginAuxOut` を schedule の `MixToBus.srcs` で参照可能にする
- **smoke test 1 (sidechain)**: kick → bass の sidechain compressor で ducking 動作
- **smoke test 2 (パラアウト)**: ドラムサンプラー（kick/snare/hat の 3 stereo パラアウト出力）→ 各 aux_out を別の track に流して個別 EQ + reverb send

各 PR で:
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo run -p daw_gui    # 手動 smoke test
```
を全て通過させる。

## 主要な変更ファイル

| 層 | ファイル | PR |
|---|---|---|
| Model | `common/src/model.rs` | PR1 (kind/parent), PR4 (sends, sidechain_sources, output_routes) |
| Schema | `common/src/process_data.rs` | PR3 (latency), PR4 (aux_in + aux_out buffers) |
| IPC | `common/src/protocol.rs` | PR3 (PluginLatencyChanged), PR4 (SetSidechainSource + SetPluginOutputRoute) |
| Audio core | `daw_audio/src/graph/{*.rs}` (新設) | PR1, PR2/3/4 で拡張 |
| Audio engine | `daw_audio/src/engine.rs` | PR1 (process_buffer 置換) |
| Audio mixer | `daw_audio/src/mixer.rs` | PR1 (NodeScratchPool 化) |
| Audio routing | `daw_audio/src/tracks.rs` | PR1 (削除 or schedule 内へ移動) |
| Plugin host | `daw_plugin_host/src/clap_plugin.rs` | PR3 (latency), PR4 (audio_ports 全走査) |
| Plugin host | `daw_plugin_host/src/clap_host.rs` | PR3 (host_latency vtable) |
| Plugin host | `daw_plugin_host/src/clap_latency.rs` (新設) | PR3 |
| GUI mixer | `daw_gui/src/view/mixer_strips.rs` | PR2 (階層) |
| GUI arrange | `daw_gui/src/view/arrangement_view.rs` | PR2 (折り畳み) |
| GUI inspector | `daw_gui/src/view/track_inspector.rs` | PR3 (latency 表示), PR4 (SC + Out ルーティング行) |
| GUI track picker | `daw_gui/src/view/track_picker.rs` (新設) | PR4 (sidechain source + パラアウト dest 共通) |

## 既存資産の再利用

| 既存 | 再利用先 |
|---|---|
| `daw_audio/src/mixer.rs::TrackScratch` (L1-55) | `NodeScratch` に汎化、ring buffer 統合で DelayLine と兼用候補 |
| `daw_audio/src/engine.rs::process_track_owned` (L530-741) | 3 関数に分解 (`op_collect_midi` / `op_run_plugin_chain` / `op_apply_strip`)、各 NodeOp ハンドラに転用 |
| `daw_plugin_host/src/process_server.rs::run_worker` (L129-260) | そのまま使用、複数 plugin dispatch 既に対応済 |
| `daw_plugin_host/src/clap_plugin.rs::query_port_channel_count` (L799-828) | `query_ports()` に拡張 |
| `daw_gui/src/view/plugin_picker.rs` パターン | `track_picker.rs` に流用（`PickerTarget::SidechainSource` + `PickerTarget::PluginOutputRoute` 追加） |
| `common/src/audio_bridge.rs` per-track peak 配列 | そのまま使用、書き手を engine から schedule に変更 |
| `daw_plugin_host/src/clap_host.rs::get_extension` (L104-122) | latency vtable 分岐を追加 |

## 検証方法

各 PR ごとに以下を実施:

**PR1**: 既存 demo `.daw` を読込・再生、master 出力を録音 → PR1 前と sample-exact 一致を `hound` で WAV diff。`cargo test -p common -- model::tests::v4_to_v5_migrate` 追加

**PR2**:
1. 2 track 作成、`Group selected tracks` 実行 → 親 group track 出現
2. group fx に reverb 追加 → 子トラックの音が両方 reverb 経由
3. group volume を 0 → 子の音が消える
4. group 折り畳みで子の row 非表示、展開で復元
5. drag-and-drop で子を別 group へ移動

**PR3**:
1. 同じ source 信号を 2 track 同時再生 (track1: linear-phase EQ で latency=512、track2: 通常 EQ)
2. track2 を反転 (gain=-1) → master mix が 0 になる（位相完全一致 = PDC 動作）
3. PDC OFF（PR4 含む将来切替時）→ phase が崩れる
4. transport bar の cumulative latency 表示が `512 spl / 10.7ms` 等の正値

**PR4 (サイドチェイン)**:
1. track1 = kick、track2 = bass、track2 fx_chain に sidechain compressor 追加
2. compressor の sidechain source picker → track1 を選択
3. 再生で kick の hit 時に bass が ducking
4. picker で source 解除 → ducking 停止

**PR4 (パラアウト)**:
1. マルチアウトプットなドラムサンプラー（CLAP `audio-ports` で is_main=true な 1 stereo + is_main=false な複数 stereo を expose するもの。検証用に Geist2 / Battery / 自作 test plugin）を track1 にロード
2. Inspector でプラグインの aux_outputs が UI に列挙される（"Out 1: kick", "Out 2: snare" 等のラベルが取れれば表示）
3. aux_output port 1 → track2、port 2 → track3 にルーティング
4. 再生で track2/track3 にそれぞれの音が分離されて流れることを確認、各 track で個別に EQ / reverb send 可能
5. ルーティング解除 → main_out のみに mix される動作に戻る

## リスクと対策

1. **PR1 の diff 規模**: 機能追加ゼロでも 1500+ 行になる見込み。`reduce_master` 等価動作の単体テストを充実させて回帰防止。WAV diff も CI に組み込み候補
2. **DelayLine の RT 安全性**: 固定容量 ring buffer で activate 時確保、capacity exceed は `tracing::warn!` のみ（現実的に exceed しない数値で確保）
3. **CLAP latency 変化のスレッド境界**: `changed` コールバックは任意スレッドから呼ばれる可能性 → main-thread post で受け流す（CLAP spec 準拠）
4. **graph 再コンパイル中の音切れ**: ArcSwap で wait-free swap、新旧 schedule で port buffer 配置が異なる場合は activate 直後の 1 buffer に dropout 可能性。GUI 編集時のみ発生で許容
5. **保存ファイル互換**: v4 → v5 migrate を unit test (`migrate_v4_to_v5`) でカバー、CI に含める
6. **MAX_AUX_IN=2 / MAX_AUX_OUT=4 制約**: 一般的な sidechain compressor / vocoder の入力は 1 つで足りる。ドラムサンプラーのパラアウトは 4 stereo (= 8 ch) で大半カバー。Geist2 等の 16+ パラアウトを持つプラグインは将来拡張（バイナリ互換破壊だが ProcessData の定数 bump で対応）
7. **パラアウトのレイアウト変更通知**: CLAP `clap_plugin_audio_ports.changed` (rescan 通知) を受けたら graph 再コンパイル必要。ホスト側で `host_audio_ports` vtable の `is_rescan_flag_supported` + `rescan` を実装、plugin → host 通知を IPC 経由で伝搬
8. **パラアウトと PDC の合成**: パラアウト側の累積 latency も sink ごとに計算する必要。compile_schedule の path latency 計算で plugin の各 aux_out を独立 source とみなして DAG をたどる（実装は単に edge を追加するだけで、Kahn ソートで自然に処理される）
