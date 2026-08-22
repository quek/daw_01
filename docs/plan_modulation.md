# plan_modulation.md — 統合 sidechain + エンベロープフォロワー・モジュレーション

> FIXME #54（キック追従の映像効果）の基盤。ただし**映像専用ではなく音声 param も同じ仕組みで
> 変調できる汎用基盤**として設計する。既存 sidechain を別系統として温存せず、本系統へ吸収する
> （ユーザー指示・SSoT・大胆に作り直す）。
>
> 関連: [plan_video_fx.md](plan_video_fx.md)（消費側の映像効果）、[plan_linear_chain.md](plan_linear_chain.md)
> （device chain）、[plan_routing_graph.md](plan_routing_graph.md)（NodeOp / schedule）。

## 0. 原則（SSoT）

参照 DAW 調査の結論: **Reaper** だけが「1 つのルーティング概念」で *プラグインの音声キー入力* と
*パラメータ単位のエンベロープフォロワー* の両方を賄う。これを daw_01 の Source→Consumer 語彙で採る。

「音をどこから取るか」の唯一の真実 = **`AudioTap { source_track, tap_point }`**。

```
   AudioTap (route, SSoT)            消費者は 2 つだけ。どちらも AudioTap の "使い方" であって
   { source_track, tap_point }       新しいルートではない:
        │
        ├─▶ Consumer A: プラグイン aux 入力 (= 旧 sidechain を吸収)
        │     raw 音声 → pd.buffer_aux_in[port]  (CLAP aux / VST3 aux bus)
        │
        └─▶ Consumer B: EnvelopeFollower (新)
              { tap, band_filter?, attack/release, gain, mode }
              → 正規化スカラー 1 本/buffer → N 個の param へ depth+極性 で配送
```

旧 `PluginInstance.sidechain_sources: Vec<Option<u32>>`（model.rs:2383）は、
`tap_point = PostFader` 固定（`SidechainTap` は常に `BufRef::TrackScratch` を読む, compile.rs:437）・
消費者 = 「プラグイン aux port N」固定 の**退化した AudioTap リスト**。本系統で `tap_point` と
消費者選択を明示化する。

## 1. データモデル（`common/src/model.rs`）

```rust
// 3 段タップ（Q4）。Pre-FX は新規スナップショット点が要る（§6）。
#[derive(Clone, Copy, PartialEq, Eq, /* serde + bincode */)]
pub enum TapPoint { PreFx, PostFx, PostFader }

#[derive(Clone, PartialEq, Eq, /* serde + bincode */)]
pub struct AudioTap {
    pub source_track: u32,   // Track::id
    pub tap_point: TapPoint, // PostFx=PreFaderScratch, PostFader=TrackScratch, PreFx=新snapshot
}

// 共有モジュレーション源（Q2: 1 source → 多 params）。Song 直下に並べる
// （song_lanes / next_song_lane_id と同じ流儀）。これが route の唯一の所有者。
#[derive(Clone, PartialEq, /* serde + bincode */)]
pub struct ModSource {
    pub id: u32,             // 安定 ID。ensure_ids 管理（sentinel 0）
    pub tap: AudioTap,
    pub follower: FollowerConfig,  // 解析パラメータは consumer 側に置く
}

#[derive(Clone, Copy, PartialEq, /* serde + bincode */)]
pub struct FollowerConfig {
    pub mode: FollowerMode,        // Peak | Rms
    pub rectify: bool,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub gain: f32,                 // 検出前ゲイン
    pub band_filter: Option<BandFilter>, // { hp_hz, lp_hz } キック抽出用（Q3）
}

// 消費者 A: プラグイン aux 入力。旧 sidechain_sources を置換。
//   PluginInstance.sidechain_sources: Vec<Option<u32>>  →
//   PluginInstance.aux_inputs:        Vec<Option<AuxInputRoute>>
#[derive(Clone, PartialEq, Eq, /* serde + bincode */)]
pub struct AuxInputRoute { pub tap: AudioTap }   // 生音声を pd.buffer_aux_in[port] へ

// 消費者 B: param への変調 edge（Q5: 加算スタック＋極性）。
#[derive(Clone, Copy, PartialEq, /* serde + bincode */)]
pub struct ModRouting {
    pub source_id: u32,            // → Song.mod_sources[*].id
    pub depth: f32,                // target の *正規化* 領域での量（0..=1）
    pub polarity: Polarity,        // Unipolar | Bipolar
}
```

- `Song` に `pub mod_sources: Vec<ModSource>` + `next_mod_source_id: u32` を追加（**唯一の store**）。
- 変調 edge は **既存の `AutomationLane`（model.rs:3574）に同居**させる。lane は `target` で
  param を 1:1 に指す既存 SSoT なので、新しい addressing state を増やさない:

```rust
pub struct AutomationLane {
    pub id: u32,
    pub target: AutomationTarget,   // 既存（PluginParam / ImageBuiltin / GroupTransform / ...）
    pub default_value: f64,         // base（curve が無いときの値、knob と two-way）
    pub clips: Vec<AutomationClip>,
    pub mod_routings: Vec<ModRouting>,   // ★追加: 0..N の follower がこの target を変調
    // ...
}
```

curve が無く変調だけの target も、base（`default_value`）+ `mod_routings` を持つ lane が要る
（= 既存の「default はあるが curve は無い knob」の表現そのまま）。**追加 addressing state ゼロ**。

## 2. 合成式（base ⊕ automation ⊕ modulation）

`song_beat` における target の **実効正規化値**:

```
norm_base = plain_to_norm(target, lane_value_at(lane, beat))     // base ⊕ automation curve
mod_sum   = Σ over lane.mod_routings:
              s = follower_scalar(source_id)                     // 0..=1
              match polarity { Unipolar => depth*s, Bipolar => depth*(2*s - 1) }
norm_eff  = clamp(norm_base + mod_sum, 0.0, 1.0)
plain_eff = norm_to_plain(target, norm_eff)
```

Bitwig の「automation が base、modulator が符号付き量を上乗せ、最後に clamp」と同一。正規化領域で
加算するので depth の意味が異種 target 間で一貫する（volume 0..2 / rotation -π..π / image x 0..1 は
既存 `plain_to_norm`/`norm_to_plain` で正規化済み, automation.rs:36-115）。

- **要修正（critique #5）**: `AutomationTarget::PluginParam` の `plain_to_norm`/`norm_to_plain` は
  今 identity placeholder（automation.rs の「Phase 2 で置換する」）。depth を正しくするには
  **プラグイン実 min/max を使う正規化**へ置換する。`PluginParamList`（protocol）で受け取る param info
  に min/max を載せ、target ごとの range を引けるようにする。
- 実装 SSoT: `common/src/automation.rs` に `effective_norm(lane, beat, &mod_scalars) -> f64` を新設し、
  音声側・GUI 側の両 consumer がこの 1 関数を呼ぶ。

## 3. フォロワー計算（audio engine・サンプル精度）

`SidechainTap` と同じ **post-dispatch sequential walk** `execute_schedule_post_dispatch`
（engine.rs:1551-1740）内で走らせる。single-thread かつ source scratch は dispatch 済みで確定。

新 NodeOp（`schedule.rs`）:

```rust
EnvelopeFollow { src: BufRef, source_id: u32 }   // src は tap_point に応じた scratch
```

検出器（RT-safe・**alloc/lock なし**・scratch slice 上で in-place）:

```
for n in 0..frames:
    x = detector_input(src_l[n], src_r[n])         // Peak: max|L|,|R|  / Rms: sqrt(½(L²+R²))
    if band_filter: x = biquad.process(x)          // per-source 1-pole HP+LP, 状態は follower ring
    x *= gain
    t = if rectify { x.abs() } else { x }
    coeff = if t > env { atk } else { rel }         // 係数は recompile 時に precompute（§10）
    env += coeff * (t - env)
    env_hist[n] = env                               // per-sample 履歴（音声 param 用）
```

- **per-source 状態（env, biquad）はエンジン所有の固定リング**（`MAX_MOD_SOURCES`、init 時確保、
  `source_id→slot` の dense map は `slot_to_plugin_id` と同型）。per-buffer alloc なし。PDC の
  `DelayLine` 状態と同じく buffer 跨ぎで保持。
- **要修正（critique #2）**: スカラーは **project-global**。per-plugin の `ProcessData` shmem には
  置かない（複製 + 毎 buffer N 回 memcpy になる）。エンジン所有リングに置く。

## 4. スカラー配送（2 粒度）

### 4.1 音声 param（サンプル精度・Q6）

音声 param は `pd.events_in` の離散 param event で届く（automation.rs:fill_pd_param_events、
現状 frame0 の 1 値のみ, line 115-117）。サンプル精度にするには:

- **要修正（critique #3）**: エンジンが 1 buffer 内に **複数の time-stamped param event** を出す経路を
  新設する。実装は「N サンプルおき（control-rate, 例 32 サンプル）に `env_hist[n]` を読み、
  §2 合成して `push_param(n, param_id, value)`」。CLAP は `clap_event_param_value.time` を、
  VST3 は `IParamValueQueue` の sample offset を既に持つ → プラグイン側はそのまま消費できる。
- **要修正（critique #4）**: §2 の合成を `daw_audio/src/automation.rs` の
  `fill_pd_param_events`（PluginParam）と `fill_track_param_ramps`（volume/pan、per-sample ramp）
  **両方に配線**する。これを怠ると「キックでコンプ閾値を変調」等の音声 param 変調が一切効かない。

### 4.2 映像/GUI param（block 粒度）

映像は frame rate（~30-60fps）なので block 値で十分・安価。既存の `AudioBridge` atomic streaming
（playhead / peaks と同じ）を流用:

```rust
// audio_bridge.rs, track_peaks の隣:
pub mod_scalars: [AtomicU32; MAX_MOD_SOURCES],   // f32::to_bits, 各 source の env[frames-1]
```

エンジンが毎 buffer `env[frames-1]` を Release store → GUI poller（main.rs:468-500, 30Hz）が
peaks と同 tick で Acquire load → `AppEvent::ModScalarsTick(Vec<f32>)`（`TrackPeaksTick` と同型）。
`AppData` に `mod_scalars: Vec<f32>`（`ModSource.id`→index の安定 map）をキャッシュ。新 IPC channel・
lock・alloc なし。`resolve_image_fields`（image_compose.rs:208-241）/ `group_active_transform`
（group_compose.rs:47-79）/ 映像効果パスが `lane_value_at` の後に §2 合成を適用。

## 5. グラフ emit（compile）

`emit_sidechain_taps`（compile.rs:412-444）を **`emit_taps_and_followers`** に置換:

1. `wanted_taps: Set<(source_track_idx, TapPoint)>` を **全 `AuxInputRoute.tap` と、live な
   `ModRouting` が参照する全 `ModSource.tap`** から構築（dedup。同じキックを「コンプキー＋
   フォロワー」で叩いても source scratch 解決は 1 回）。
2. aux port に繋がる各 `AuxInputRoute` で従来どおり `NodeOp::SidechainTap` を emit。ただし `src` を
   `tap_point` で解決（`PostFader→TrackScratch`, `PostFx→PreFaderScratch`, `PreFx→新snapshot`）。
   現状の always-post-fader は `PostFader` arm に一致 → migrate 後も byte 同一。
3. 参照される各 `ModSource` で `NodeOp::EnvelopeFollow { src, source_id }` を source track 処理後に emit。
4. dangling（source track 削除）は両 consumer とも従来どおり寛容に skip（compile.rs:432-434）。

- **要修正（critique #1・blocker）**: sidechain は **2 か所**で emit される。
  `emit_sidechain_taps`（per-track）と、**`master_fx_chain` のインライン loop（compile.rs:273-297）**。
  後者も `emit_taps_and_followers` 化しないと、`sidechain_sources` 削除で**コンパイル不能**になり
  マスターバスの keying が壊れる。両 site を統合する。
- PDC: `compute_path_latency`（compile.rs:476-569）は既に sidechain source latency を dest の
  input latency に畳む。follower edge も同形なので、follower の `src` track の path_latency を
  変調先 param が載る track の input latency に含める（compile.rs:514-563 の input-source 集合へ
  `ModRouting` source tap を追加）。cycle 検出（compile.rs:113-192）も自動的に follower edge を被覆。

## 6. 3 段タップ（Q4）の buffer 対応

| TapPoint | 既存 BufRef | 備考 |
|---|---|---|
| PostFader | `TrackScratch(i)` | 既存。旧 sidechain の既定 |
| PostFx（fader 前） | `PreFaderScratch(i)` | 既存。pre-fader snapshot guard が要る |
| PreFx（素の音） | **新規 snapshot** | device chain 適用前の per-track buffer を新設 |

- PreFx は新スナップショット点。track の音源（clip/notes）を device chain に通す **前** の
  per-track buffer を確保し、`ProcessTrack` 開始時にコピー。`MAX_TRACKS` 分の固定領域。
- PostFx/PreFx を使うタップがあるとき、pre-fader snapshot guard（engine.rs:1385-1394）の述語を
  「PreFader send を持つ」→「PreFader send **または** Pre/PostFx タップ/フォロワーの source」に拡張。

## 7. 書き出しの決定論（Q7）

`render_video.rs`（書き出し）は audio engine 非稼働で `playhead_beat = frame_index/framerate`
（render_video.rs:280-281）から算出 → live の `AudioBridge` スカラーを読めない。プレビューと一致
させるには follower を **song 状態から再構築可能** にする。

**採用（Q7=1）: 音声を先に render → env を焼き込み → frame ごとにサンプル。**

- 音声書き出し（`export.rs` freewheel）は **同一 `Schedule` + 同一 `execute_schedule_post_dispatch`
  （`EnvelopeFollow` 含む）** を再利用。ここで各 `ModSource` の env 履歴を **per-source envelope
  sidecar**（beat キー、例 1kHz downsample の `Vec<f32>`）として WAV と並べて書き出す。
- `render_video.rs::build_frame_scene` が `playhead_beat` で sidecar をサンプルし、live と **同一の
  §2 合成**に渡す。follower 実装は **RT preview / 音声書き出し / オフライン bake の 3 経路で 1 つ**
  （`follower_step`）に統一 → drift なし。
- **要修正（critique #6）**: 「bit 完全一致」は誇張しない。live は `env[frames-1]`（block 1 点）を
  GUI 側 smoother で、export は 1kHz bake をサンプル → サンプリング経路が違う。**両者の
  サンプリングを揃える**（export も block 末尾値を frame 時刻でサンプル、もしくは live も
  per-frame に sidecar 相当を計算）ことで知覚上一致させる。export.rs→render_video の env sidecar
  受け渡し plumbing（現状 `audio_wav_path` のみ）を新設する。

## 8. 移行（旧プロジェクト・挙動不変）

「キックでコンプを鳴らす」= 旧 `bass.devices[d].sidechain_sources=[Some(kick_id)]` →
新 `bass.devices[d].aux_inputs[0]=Some(AuxInputRoute{tap:{kick_id, PostFader}})`。
compile は同一 `SidechainTap{src:TrackScratch(kick_idx), ...}` を emit → エンジン handler・PDC・
master-FX 順序・dangling-skip すべて不変 = **挙動完全不変**。

| 旧 | 新 | 注意 |
|---|---|---|
| `sidechain_sources: Vec<Option<u32>>` | `aux_inputs: Vec<Option<AuxInputRoute>>` | **要修正（critique #7）**: `#[serde(alias)]` は型を `u32→AuxInputRoute` に持ち上げられない。**deserialize 専用の legacy field + 明示 migrate**（`legacy_*` パターン model.rs:3379 と同型）で `Some(id)→Some(AuxInputRoute{tap:{id, PostFader}})` に lift |
| (なし) | `Song.mod_sources`, `next_mod_source_id` | `#[serde(default)]` → 旧ファイルは空・next=1 |
| (なし) | `AutomationLane.mod_routings` | `#[serde(default)]` → 旧 lane は空 = 変調なし |

- `ensure_ids`（model.rs:1284-1308）を拡張: 既存の `sidechain_sources` remap を
  `aux_inputs[*].tap.source_track` に置換（master_fx_chain loop も）。`mod_sources[*].tap.source_track`
  remap loop と `mod_source.id` sentinel 割当 loop を追加。`mod_routings[*].source_id` は
  **mod-source id**（track id でない）なので track 番号変更には不変、ただし source 削除後の dangling は
  スカラー 0 扱い。
- 回帰テスト `ensure_ids_remaps_sidechain_sources_and_parent_group_id`（model.rs:4243-4259）を
  `aux_inputs` 版に更新 + `mod_sources` の track remap / id 割当テストを追加。

## 9. UI（既存 sidechain dropdown を置換）

`track_inspector.rs:1858-2017` の per-plugin「sidechain source」dropdown（port 0 のみ）を
**統合モジュレーションラック**に置換:

- **Sources パネル**（project 単位）: `ModSource` の作成/一覧。source track + Pre/PostFx/PostFader
  タップ + follower（attack/release/band/gain/mode）。
- **プラグイン aux 入力**: aux port ごとに「Audio From」dropdown（`{source_track, tap_point}`）。
  = 旧 sidechain UI に Pre/Post トグルが付いたもの。
- **param 単位の変調**: 任意の knob（track strip / plugin param / image/text/group/映像効果 field）に
  「+ modulation」で `ModRouting{source_id, depth, polarity}` を追加。depth は Bitwig 風リングで表示。
  既存 `scrubable_number`/inspector idiom を流用（`feedback_reuse_inspector_idiom`）。
- `AppEvent::SetSidechainSource`（app.rs:3704）を `SetAuxInputRoute{...}` +
  `{Add,Set,Remove}ModRouting{...}` 等に置換。すべて `sync_song_to_plugin_host()` 経由で schedule 再compile
  （既存 sidechain event と同経路, app.rs:14953-14977）。

## 10. RT-safety

- `FollowerConfig` は Copy・model 側（Song）に置く。biquad/env **状態**はエンジン所有リング。
- 係数（atk/rel/biquad）は **schedule recompile 時のみ** precompute。`attack_ms`/`band` 変更は
  `SetSidechainSource` 同様 `sync_song_to_plugin_host` で recompile をトリガ → callback 内で
  f32→係数算出も alloc も起きない（critique #8）。

## 11. Touch points

| 層 | ファイル | 変更 |
|---|---|---|
| Model | `common/src/model.rs` | `TapPoint`/`AudioTap`/`ModSource`/`FollowerConfig`/`BandFilter`/`AuxInputRoute`/`ModRouting`/`Polarity`/`FollowerMode` 追加; `Song.mod_sources`+`next_mod_source_id`; `sidechain_sources`→`aux_inputs`; `AutomationLane.mod_routings`; ensure_ids 拡張 + legacy migrate |
| Eval SSoT | `common/src/automation.rs` | `effective_norm(lane, beat, &mod_scalars)`; PluginParam の実 min/max 正規化 |
| Schedule | `daw_audio/src/graph/schedule.rs` | `NodeOp::EnvelopeFollow`; `SidechainTap.src` を tap_point 解決 |
| Compile | `daw_audio/src/graph/compile.rs` | `emit_sidechain_taps`→`emit_taps_and_followers`（**per-track + master_fx 両 site**）; follower を latency/cycle 集合へ |
| Engine RT | `daw_audio/src/engine.rs` | `EnvelopeFollow` handler + per-source follower ring; `bridge.mod_scalars` 書き込み; 音声 param の per-sample event 発行; PreFx snapshot |
| Bridge | `common/src/audio_bridge.rs` | `mod_scalars` atomic plane（`MAX_MOD_SOURCES`） |
| Export | `daw_audio/src/export.rs` + `daw_gui/src/render_video.rs` | env sidecar の bake + frame サンプル |
| GUI poll | `daw_gui/src/main.rs` | `mod_scalars` 読取 → `ModScalarsTick` |
| GUI compose | `image_compose.rs`/`group_compose.rs`/映像効果パス | `lane_value_at` 後に §2 合成 |
| Protocol | `common/src/protocol.rs` | aux-input route / mod 操作 msg; `PluginParamList` に min/max |
| UI | `track_inspector.rs` + `app.rs` | モジュレーションラック + aux-input route + events |

## 12. Phasing

1. **Model + migration**: 型追加・`sidechain_sources`→`aux_inputs` lift・ensure_ids・回帰テスト
   （旧プロジェクトが routing 不変でロードできること）。`cargo build --workspace`
   （`feedback_workspace_build_for_protocol_changes`）。
2. **Compile 統合**: 両 sidechain emit site を `emit_taps_and_followers` に統合（挙動不変を test で確認）。
3. **Follower エンジン**: `EnvelopeFollow` + ring + `AudioBridge.mod_scalars` + 30Hz GUI 配送。
4. **合成 SSoT**: `effective_norm` を GUI compose に配線（映像 param 変調が動く）。
5. **音声 param 変調**: per-sample event 経路 + `fill_pd_param_events`/`fill_track_param_ramps` 配線。
6. **3 段タップ**: PreFx snapshot + UI Pre/Post トグル。
7. **書き出し**: env sidecar bake + render_video サンプル + プレビュー一致検証。
8. **UI**: モジュレーションラック（sources / aux-input / per-param depth）。
