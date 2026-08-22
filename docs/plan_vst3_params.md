<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_vst3_params — VST3 パラメータ automation 一式

作成: 2026-05-31 / status: 実装済み (実機 click-through 確認待ち)

## 背景

「plugin param を触った後 `A` キーで automation lane を作る」 (Bitwig/Live 流 last-touched
workflow、 `shortcuts.rs:67` の `daw.add_automation_from_last_touched`) は CLAP では完全動作するが
**VST3 では効かなかった**。 原因は VST3 backend の param 機能がほぼ骨組みだけで、 8 機能中 6 つが
未実装だったため (gesture 通知 / param 一覧 / automation 入力 等)。

## 実装内容 (CLAP パリティ達成)

### 1. gesture 通知 (plugin GUI → daw_gui)
VST3 は param 編集を `IComponentHandler::beginEdit/performEdit/endEdit` で **main thread** から
host に通知する (CLAP は process out_events = audio thread)。

- `HostCallbacks` (`plugin_instance.rs`) に `on_param_gesture_begin/value/end` の 3 closure を追加
- `Vst3ComponentHandler` (`vst3_host.rs`) の beginEdit/performEdit/endEdit がそれを呼ぶ
- `make_callbacks` (`main.rs`) が resize/closed と同 idiom で `evt_tx` に
  `PluginEvent::PluginParamTouched/ValueChanged/GestureEnd` を流す

→ `evt_tx → PluginEvent → ChildToMain → daw_gui` の既存経路 (CLAP と同じ) に合流。
**audio thread を介さず main thread イベントなのでロック不要** (RT 規約と無関係)。
`PluginEvent` の `plugin_id` は変換時に破棄されるので 0 placeholder。

### 2. param 一覧取得
`Vst3Plugin::enumerate_params` (`vst3_plugin.rs`) を `IEditController::getParameterCount/
getParameterInfo` で実装。 VST3 param は仕様上常に normalized [0,1] なので min/max=0/1、
default=`defaultNormalizedValue`。 flags は VST3 `ParameterFlags` → daw_01 `plugin_param_flags`
にマップ (kCanAutomate→AUTOMATABLE 等、 stepCount>0→STEPPED)。 module は空 (VST3 の unitId
階層は flat 表示では不要)。 呼び出しは plugin-main thread (`SetSlotPlugin` 経路)。

### 3. automation 入力 (daw_gui → plugin)
新規 `vst3_params.rs`: host 側 `IParameterChanges` + `IParamValueQueue` の COM 実装
(`vst3_events.rs` の `Vst3InEventList` と同 UnsafeCell パターン)。

- `Vst3InParamChanges`: 固定プール (64 queue) を事前確保。 `process()` 毎に `set_changes` で
  `TimedParamEvent` を param_id ごとに queue へグルーピング (steady state でヒープ確保なし)
- `getParameterData(i)` は queue の `IParamValueQueue` を **borrowed ptr** で返す (ComWrapper が
  自前 ref を保持するので temp ComPtr drop 後も生存、 `inputEvents` と同 idiom)
- `vst3_plugin.rs::process` で `set_changes` 呼び出し + `ProcessData.inputParameterChanges` に接続
  (従来 null だった)。 値は normalized = enumerate_params の min0/max1 と整合

### 4. 現在値クエリ (getParamNormalized) — 不要と判断
`lane_default_for_target` (`app.rs:7911`) は plugin param に対し **0.0 固定** (CLAP も同じ)。
lane default に現在値は使われないため、 getParamNormalized seeding は CLAP パリティに不要。
recording mode の値 source (`plugin_param_values` cache) は performEdit (上記 1) が供給済み。

## 検証

- `cargo build --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo test --workspace`
  全 green
- unit test `vst3_params::tests` (set_changes の param グルーピング / プール reuse / overflow)
- [ ] **実機 click-through 確認待ち**: 実 VST3 plugin GUI で knob ドラッグ → `A` → lane 生成 →
  再生で lane 値が param を動かすか + record-arm 中の point 書き込み

## 変更ファイル

- `daw_plugin_host/src/plugin_instance.rs` — HostCallbacks に param gesture closure
- `daw_plugin_host/src/vst3_host.rs` — Vst3ComponentHandler の 3 callback 実装
- `daw_plugin_host/src/main.rs` — make_callbacks の closure + mod vst3_params
- `daw_plugin_host/src/vst3_plugin.rs` — enumerate_params + process の param 入力配線
- `daw_plugin_host/src/vst3_params.rs` (新規) — IParameterChanges/IParamValueQueue COM 実装

## 既知の非対応 (CLAP も同様 or VST3 固有)

- output param changes (plugin → host の param 書き戻し automation) は未配線 (GUI gesture は
  IComponentHandler 経由で取得済みなので当面不要)
- VST3 param の module/grouping (unitId 階層) は flat 表示 (module 空)
