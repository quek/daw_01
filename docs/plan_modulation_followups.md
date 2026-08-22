<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_modulation_followups.md — モジュレーション残作業 (Pre-FX タップ + プラグイン param 表示精度)

> [plan_modulation.md](plan_modulation.md) / [plan_modulation_routing_redesign.md](plan_modulation_routing_redesign.md)
> の積み残し 2 件。**#56 (LFO/Random/MSEG ソース) は別セッション/worktree で進行中なので本 plan の対象外**。
> worktree `F:\dev\daw_01_mod_followups` (branch `feature/modulation-followups`) で最終形まで一括実装する。

## 1. Pre-FX タップ (3 段タップの完成)

plan_modulation.md §6 の 3 段タップ `PreFx | PostFx | PostFader` のうち **PreFx (素の音 =
device chain 適用前)** だけ未実装。現状 `compile.rs::tap_bufref` が `PreFx → PreFaderScratch`
(= PostFx) にフォールバックし (compile.rs:424-433)、UI も PostFader / PostFx しか出していない。

### 1.1 SSoT — 捕捉点

`process_track_owned` の信号の流れ (engine.rs):
1. `track_l/r` を clear → audio clip を加算 (1291-1303)
2. input-delay (sidechain alignment) (1306-1312)
3. **device chain loop** (1314-1413) … ここで FX が `track_l/r` を上書き/加算
4. pre-fader snapshot → `pre_fader_l/r` (= PostFx, 1421-1430)
5. mixer strip (fader/pan) → `track_l/r` が PostFader に

→ **PreFx 捕捉点 = 3 と 4 の間でなく、2 と 3 の間** (= 最初の FX device が input として受け取る信号)。
group track は `run_group_fx_chain` で children mix 後・group device chain 前に同様に捕捉。

### 1.2 変更

| 層 | ファイル | 変更 |
|---|---|---|
| Scratch | `daw_audio/src/mixer.rs` | `TrackScratch` に `pre_fx_l/r: Vec<f32>` (MAX_FRAMES, `new()` で 1 回確保) |
| BufRef | `daw_audio/src/graph/schedule.rs` | `BufRef::PreFxScratch(u32)` 追加 (`PreFaderScratch` と並列) |
| Compile | `daw_audio/src/graph/compile.rs` | `tap_bufref`: `PreFx => PreFxScratch(src_idx)` (フォールバック削除、doc 更新) |
| Engine | `daw_audio/src/engine.rs` | ① `process_track_owned`: device loop 前に pre_fx snapshot (`track_needs_prefx_snapshot` guard) ② SidechainTap handler (1732) / EnvelopeFollow handler (1807) に `PreFxScratch` arm ③ `run_group_fx_chain` も同捕捉 ④ 新 `track_needs_prefx_snapshot` 述語 ⑤ 既存 `track_needs_prefader_snapshot` の述語を `!PostFader` → `PostFx` のみに絞る (PreFx は別 buffer を読むので pre_fader を立てない) |
| Event | `daw_gui/src/app.rs` | `SetModSourceTapPoint { post_fader: bool }` → `{ tap_point: TapPoint }` (bool では 3 値を表せない)。handler / 呼び出し側を更新 |
| UI | `daw_gui/src/view/track_inspector.rs` | mod source の tap 選択に **Pre-FX** を 3 つ目の選択肢として追加 |

- RT 安全: snapshot は guard 付き memcpy のみ (pre_fader と同型)。`pre_fx_l/r` は確保済み、RT realloc なし。
- 挙動不変: PreFx を誰も使わなければ guard が false で memcpy skip → 既存 byte 同一。
- aux_inputs (plugin sidechain) の PreFx タップも `track_needs_prefx_snapshot` がカバー (mod source と aux 両方を走査)。

## 2. プラグイン param の表示精度 (critique #5)

`automation.rs::plain_to_norm`/`norm_to_plain` の `PluginParam` 枝は identity placeholder
(automation.rs:48/107, doc に「Phase 2 で plugin_params lookup に置換」)。plugin の実 min/max を
使わないので、min/max が 0..1 でない param (例 20..20000 Hz) では:
- modulation overlay の base_norm が端に飽和 (`inspector_mod_data` app.rs:2377)
- arrangement automation 曲線の y 位置が誤り (`arrangement_view.rs:2065/2078`)

**音声変調は正しい** (engine は `offset_norm` を直接送るので min/max 不要、redesign §4-A)。本件は
**daw_gui 表示専用の polish**。protocol 改修不要 (`AppData.plugin_params` が min/max を既にキャッシュ済 app.rs:1075)。

### 2.1 変更

| 層 | ファイル | 変更 |
|---|---|---|
| Eval SSoT | `common/src/automation.rs` | `plain_to_norm_ranged(target, plain, plugin_range: Option<(f64,f64)>)` / `norm_to_plain_ranged(...)` 追加。`PluginParam` かつ `Some((min,max))`(max>min) なら affine `(plain-min)/(max-min)`、それ以外は既存ロジック委譲。`plain_to_norm = ranged(.., None)` |
| GUI helper | `daw_gui/src/app.rs` | `plugin_param_range(track_id, &target) -> Option<(f64,f64)>`: `PluginParam{device_index, param_id}` を `plugin_params[(track_id, device_index)]` から引く |
| GUI 表示 | `app.rs::inspector_mod_data` | `base_norm` / `reach` を ranged 版 + 当該 param range で算出 |
| GUI 表示 | `arrangement_view.rs` | 曲線 y-mapping (2065/2078) を ranged 版 + lane 所有 track の plugin range で算出 |

- 非プラグイン target は range を無視するので **完全回帰** (volume/pan/image/text/group/tempo は不変)。
- audio engine 側の `plain_to_norm` 利用は無改修 (PluginParam を normalize しない経路なので影響なし)。

## 3. 検証

`cargo build --workspace` (protocol/BufRef 変更あり [[feedback_workspace_build_for_protocol_changes]]) →
`cargo test --workspace` → `cargo clippy --workspace -- -D warnings` → release build green。
実機: ① Pre-FX タップで「素の音」でフォロワーが動く (FX 後と違う挙動) を可聴/可視確認
② min/max が 0..1 でない plugin param の variation を変調 → overlay 帯と automation 曲線が正しい位置に。
