# plan_fixme_56_modulators.md — LFO / Random / MSEG / Steps モジュレーター (FIXME #56)

> 「Bitwig みたいに LFO, ランダム, Mセグなどでの変調」(FIXME #56)。既存のエンベロープ
> フォロワー基盤 ([plan_modulation.md](plan_modulation.md) / [plan_modulation_routing_redesign.md](plan_modulation_routing_redesign.md))
> を**作り直さず再利用**し、`ModSource` に **generator 種別** を追加する差分。
> **フェーズ分けせず最終形まで一気に完走する** (CLAUDE.md)。
>
> スコープ確定 (2026-06-14, user): **LFO / Random / MSEG / Steps の 4 種**。

## 0. 原則と核心アーキテクチャ

参照 DAW 一次情報 (workflow `w7eqm8141`):
- **Bitwig** 公式: LFO (shape/phase/polarity/sync/run-mode)、Random (discrete/slewed)、
  Steps (最大 64 step・direction)、Curves/Segments (MSEG, play mode One-shot/Loop/Ping-Pong)。
- **Vital** ソース: LFO phase = `frac(freq × transport秒)` の**純関数 → オフライン完全再現**。
  MSEG = `LineGenerator` = `Vec<{x,y, power(曲率)}>` を 2048 LUT 化し Catmull-Rom。
  **Vital の Random は mt19937 をグローバルカウンタ seed で非再現** → daw_01 は seed 保存必須。
- **Ableton/Reaper**: free Hz ⇔ tempo-sync division トグルが標準。タイムライン文脈に per-note
  retrigger は無く、Sync LFO は本質的に「transport 位相連動の free-run」(Ableton は Sync 時に
  retrigger ボタンを無効化)。

**核心 SSoT**: generator (LFO/Random/MSEG/Steps) は **`song_beat` の純粋関数**。audio 入力にも
engine ring にも依存しない (follower との決定的な違い)。よって:
- follower → engine 所有 ring が `env` を算出 (既存、不変)。
- generator → audio / GUI / export の各 consumer が `song_beat` から**直接** `eval` を呼ぶ。
  状態レス・alloc/lock なし・全経路で同一関数 → **drift ゼロ・bounce 完全再現**。
- 既存の `mod_scalars` プレーン / `source_scalar` slot indexing / `apply_modulation` 合成式 /
  `ModRouting` / depth リング / gui_01 widget は **一切変更不要**。generator は `scalar(source_id)`
  の供給元が 1 つ増えるだけ。

## 1. データモデル (`common/src/model.rs`)

現状 `ModSource { id, owner_track_id, tap: AudioTap, follower: FollowerConfig }`
(= envelope follower 専用) を **kind enum に一般化**:

```rust
pub struct ModSource {
    pub id: u32,
    pub owner_track_id: u32,
    pub color: ModColor,          // Bitwig 流 source 色 (routing_redesign §6, depth リング用)
    pub kind: ModSourceKind,
}

pub enum ModSourceKind {
    EnvelopeFollower { tap: AudioTap, follower: FollowerConfig },  // 既存をそのまま内包
    Lfo(LfoConfig),
    Random(RandomConfig),
    Mseg(MsegConfig),
    Steps(StepsConfig),
}

// 全 generator 共通の rate (SSoT)。
pub enum ModRate {
    Free { hz: f32 },                       // transport 非同期の絶対周波数 (ただし評価は song 秒で決定論)
    Sync { numerator: u32, denominator: u32 },  // 音価。period_beats = 4.0 * num/den
}                                           // 1/4=(1,4)→1beat, 1bar=(1,1)→4beat, 1/8T=(1,12), 付点1/4=(3,8)

// タイムライン文脈の retrigger (壁時計基準は決定論を壊すので採らない)。
pub enum RetriggerMode {
    FreeRun,                    // phase = f(song_beat) 連続。既定 (Bitwig Sync 相当)
    FromBeat { anchor_beat: f64 },  // phase = f(song_beat - anchor)。MSEG OneShot 等
}

pub enum LfoShape { Sine, Triangle, SawUp, SawDown, Square, Pulse { width: f32 } }
pub struct LfoConfig { pub shape: LfoShape, pub rate: ModRate, pub phase: f32, pub retrigger: RetriggerMode }

pub enum RandomMode { Smooth, SampleHold }  // Smooth=step 間補間, SampleHold=階段
pub struct RandomConfig { pub rate: ModRate, pub mode: RandomMode, pub seed: u64, pub retrigger: RetriggerMode }

pub struct MsegPoint { pub time: f32, pub value: f32, pub curve: f32 }  // time/value 0..=1, curve -1..=1 tension
pub enum MsegPlayMode { OneShot, Loop, PingPong }
pub struct MsegConfig { pub points: Vec<MsegPoint>, pub rate: ModRate, pub play_mode: MsegPlayMode, pub retrigger: RetriggerMode }

pub enum StepsDirection { Forward, Backward, PingPong }
pub struct StepsConfig { pub values: Vec<f32>, pub rate: ModRate, pub direction: StepsDirection, pub slew: f32, pub retrigger: RetriggerMode }
```

- すべて `Serialize/Deserialize + bincode Encode/Decode` (IPC/save 経由)。`Vec` を持つ
  `MsegConfig`/`StepsConfig` のため `ModSource`/`ModSourceKind` は `Clone` だが非 `Copy`
  (= 既存の `ModRouting` と同じ扱い)。
- **migration**: 変調機能は未リリース (`owner_track_id 0 = legacy, 実データ無し`) なので、
  旧 `ModSource{tap,follower}` → `kind: EnvelopeFollower{tap,follower}` への serde 変換は
  実プロジェクトには不要。`#[serde(default)]` と最小 migrate のみ。回帰テスト更新。
- `ensure_ids` は既存の `mod_sources[*].tap.source_track` remap を
  `EnvelopeFollower` arm 限定に変更 (generator には tap が無い)。id 採番は不変。
- `ModColor` は source 作成時に palette から循環割当 (`next_mod_source_id` と同流儀)。

## 2. 評価 SSoT (`common/src/modulators.rs` 新設) — 純粋関数・RT-safe

```rust
/// generator の出力スカラー (unipolar 0..=1)。極性は ModRouting.polarity が後段で担う。
/// follower 以外の全種別をここで評価。song_beat / song_secs は transport の関数 → 決定論。
pub fn generator_scalar(kind: &ModSourceKind, song_beat: f64, song_secs: f64) -> Option<f32> {
    match kind {
        ModSourceKind::EnvelopeFollower { .. } => None,   // ring 由来。ここでは扱わない
        ModSourceKind::Lfo(c)    => Some(lfo_eval(c.shape, phase(&c.rate, song_beat, song_secs, c.phase, &c.retrigger))),
        ModSourceKind::Random(c) => Some(random_eval(c, song_beat, song_secs)),
        ModSourceKind::Mseg(c)   => Some(mseg_eval(c, song_beat, song_secs)),
        ModSourceKind::Steps(c)  => Some(steps_eval(c, song_beat, song_secs)),
    }
}
```

- `phase(rate, beat, secs, phase0, retrig)`:
  - `Sync{num,den}` → `period_beats = 4*num/den`; `local = retrig.map(beat)`; `frac(local/period_beats + phase0)`
  - `Free{hz}` → `local_secs = retrig.map_secs(secs)`; `frac(local_secs*hz + phase0)`
  - `FromBeat{anchor}` は `beat-anchor` (secs は beat→secs 換算) を local に。
- `lfo_eval`: `Sine=0.5-0.5cos(2πp)`, `Triangle=1-|2p-1|`, `SawUp=p`, `SawDown=1-p`,
  `Square=p<0.5?1:0`, `Pulse{w}=p<w?1:0`。
- `random_eval`: `step = floor(phase_total)`; `a = splitmix64(seed ^ step) → [0,1)`;
  `SampleHold → a`; `Smooth → lerp(a, splitmix64(seed^(step+1)), frac(phase_total))`。**決定論**。
- `mseg_eval`: `play_mode` で phase を fold (Loop=そのまま, PingPong=三角化, OneShot=clamp 0..1)
  → points を時刻昇順で bracket → `curve` で歪ませた補間 (`apply_tension`)。
- `steps_eval`: `n = values.len()`; `idx = direction.map(floor(phase*n), n)`; `slew==0 → values[idx]`,
  else 隣接 step と補間。

`Cargo.toml`: 新 module は `common` 内。splitmix64 は内製 (依存追加なし)。

## 3. エンジン統合 (`daw_audio`)

producer seam = engine の slot publish (`engine.rs:852-856` でdispatch 前に `follower_slots[*].env`
を `mod_scalars_snapshot` へ、`:999` で `AudioBridge::mod_scalars` へ publish)。

- `cached_schedule.follower_slots` を **全 ModSource 1 slot** に一般化 (or 並列の generator slot 表)。
  follower slot は `env`、generator slot は `generator_scalar(kind, buffer_beat, buffer_secs)`。
- `EnvelopeFollow` NodeOp は follower のみ emit (compile.rs)。generator は NodeOp 不要 (状態レス・
  publish 時評価)。buffer 先頭 beat/secs で block-rate 評価 (既存 follower の `env[frames-1]` と同粒度)。
- これで音声 param 変調 (`fill_pd_param_events`/`fill_track_param_ramps`) も generator を自動取り込み。

## 4. GUI / export での直接評価 (SSoT・状態レス)

generator は状態レスなので、GUI / export は ring を待たず **`song_beat` から直接** `generator_scalar`
を呼んで `mod_scalars` プレーンの当該 slot を埋める (follower slot のみ ring/poll 由来):
- GUI: `ModScalarsTick` で follower 値を受けた後、generator slot を playhead beat で上書き
  (or compose 直前に全 generator を評価)。停止中も playhead beat で評価 → 静止プレビューで値が見える。
- export (`render_video.rs::build_frame_scene`): `playhead_beat = frame/framerate` から直接評価。
  **generator は env sidecar 不要** (follower のみ sidecar)。→ プレビューと bit 一致。

## 5. UI (`daw_gui/src/view/track_inspector.rs` + `app.rs`)

source rack を種別対応に拡張 (既存 follower row は据え置き):
- `+ Src` を**種別ピッカー** (Follower / LFO / Random / MSEG / Steps) に。`.take(2)` 制限を撤廃。
- 種別ごとの inspector (既存 `scrubable_number`/dropdown idiom 流用, `feedback_reuse_inspector_idiom`):
  - **LFO**: shape dropdown / rate (Free Hz ⇔ Sync division トグル + picker) / phase scrub / retrigger。
  - **Random**: rate / mode (Smooth/S&H) / seed (+ re-roll ボタン)。
  - **MSEG**: breakpoint エディタ (下記) / rate / play_mode / retrigger。
  - **Steps**: step bar 編集 (下記) / step 数 +- / direction / slew / rate。
- **MSEG/Steps エディタ** = gui_01 `heavy()` escape hatch (ピアノロール/automation curve と同方式):
  `hctx.cached(...)` 内で `push_lines`(曲線/bar) + `push_rect`(ハンドル) + `push_text`(目盛)。
  入力: 空白ダブルクリック=点追加 (`AppData::last_click` 400ms/5px 判定)、ハンドルドラッグ=
  time/value、segment 中央縦ドラッグ=curve(tension)、右クリック/Delete=削除 (両端固定)。
  Edit は `hctx.push_edit(Edit::mutate(|app| app.handle_event(AppEvent::MsegMovePoint{..})))`。
- ライブ位相カーソル: `scalar(source_id)` を playhead beat から算出してエディタに重畳描画。
- 新イベント: `AddModSource{kind}` / `SetModSourceKind` / `SetLfoShape` / `SetModRate` /
  `SetLfoPhase` / `SetRandomSeed`(re-roll) / `SetRandomMode` / `MsegAddPoint` / `MsegMovePoint` /
  `MsegSetCurve` / `MsegRemovePoint` / `SetMsegPlayMode` / `StepsSetValue` / `SetStepsCount` /
  `SetStepsDirection` / `SetModRetrigger`。すべて `sync_song_to_plugin_host()` 経由 (generator は
  schedule recompile 不要だが song 保存・プレビュー反映のため既存 sync 経路に乗せる)。
- **per-param depth リング割当 UI** は既存 follower と共通の pending gui_01 widget 待ち
  (`routing_redesign §6`)。#56 の新規 UI 自体はブロックされない。

## 6. テスト (TDD・決定論が核心)

- `modulators.rs` 単体: 各 `*_eval` をパラメタライズドで具体値検証。LFO 各 shape の既知点、
  MSEG bracket/tension、Steps direction、Random の **seed 再現性** (`eval(beat)==eval(beat)` かつ
  seed 違いで別値) と Smooth 補間。phase の Sync/Free/FromBeat。
- model: `ModSource` の bincode round-trip、`ensure_ids` の follower-only tap remap、
  generator kind の save/load。
- **3 経路一致**: 同 `song_beat` で audio/GUI/export が同一 scalar を返すこと (drift ゼロ)。
- protocol/model 変更 → `cargo build --workspace` (`feedback_workspace_build_for_protocol_changes`)。

## 7. Touch points

| 層 | ファイル | 変更 |
|---|---|---|
| Model | `common/src/model.rs` | `ModSource` 一般化, `ModSourceKind`, 各 Config/enum, `ModColor`, `ensure_ids`, migration, 回帰テスト |
| Eval SSoT | `common/src/modulators.rs` (新) | `generator_scalar` + `lfo/random/mseg/steps_eval` + `phase` + splitmix64 + テスト |
| Engine | `daw_audio/src/engine.rs`, `graph/compile.rs`, `graph/schedule.rs` | slot publish を generator 対応に一般化, follower のみ `EnvelopeFollow` emit |
| GUI eval | `daw_gui` compose/poll | generator slot を playhead beat で直接評価 |
| Export | `daw_gui/src/render_video.rs` | generator を frame beat で直接評価 (sidecar 不要) |
| UI | `daw_gui/src/view/track_inspector.rs`, `app.rs` | 種別ピッカー + 種別別 inspector + MSEG/Steps エディタ + events |
| Protocol | `common/src/protocol.rs` 等 | 新 AppEvent / song 同期 |

## 8. 実装順序 (一括・フェーズ分けなし)

1. **Eval + テスト** (`modulators.rs`): 純粋関数を TDD で。決定論テスト先行。
2. **Model**: `ModSource` 一般化 + ensure_ids + migration + round-trip テスト。`cargo build --workspace`。
3. **Engine**: slot publish 一般化 (follower=env / generator=eval)。音声 param 変調に自動波及。
4. **GUI/export 直接評価**: compose/poll/render_video に generator 評価を配線。
5. **UI**: 種別ピッカー + 種別別 inspector + MSEG/Steps エディタ + events。
6. `/review` → 全テスト + clippy + smoke + 実機検証 (user sign-off) → commit → release build。
