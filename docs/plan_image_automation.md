<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Image Automation (x/y/w/h/opacity 自動化) 計画

ステータス: **設計確定** (2026-05-26)、 着手前。

関連:
- [plan_image_overlay.md](plan_image_overlay.md) — image PiP の data model / composite pipeline。
- [plan_automation.md](plan_automation.md) — 既存 automation 機構 (Lane / Clip / Content / Target、 Bitwig 流)。

## 0. 動機と要件

- ユーザー要望 (2026-05-26): 画像の位置などをオートメーション可能にする。
- 用途: MV 制作で「画像が動画の上に重なって、 時間とともに動く / フェードイン・アウトする / 大きさが変わる」 という動画編集の標準需要。
- 現状 (2026-05-26 時点):
  - `ImageEvent.x/y/w/h/opacity` は inspector で数値入力可能 (Phase 4 完了)。
  - automation は track-level の audio TrackBuiltin (volume/pan/mute/send) と plugin param のみ。 image field は未対応。

## 1. 採用方針 (一次情報根拠)

### 1.1 lane 配置 = **track-level (1 image track あたり 5 lane)**

- 採用理由: 同 track の全 image clip が同じ x/y/w/h/opacity lane で駆動される (= 「画像が切り替わってもカメラパンが続く」 が自然)。 lane 数は track-global で 5 個固定、 N clips でも 5 lane のまま。
- 拒絶: clip-bound keyframe (= ImageEvent 内に envelope を埋め込む After Effects 流)。 理由: arrangement の lane 行 UI が既存 audio TrackBuiltin と一貫しない、 mental model が分裂、 既存 lane infrastructure (gui_01 #028) を再利用できない。
- 拒絶: clip-id を持つ track-level lane (= AutomationTarget に clip_id を含める)。 理由: clip 削除 / reorder で参照切れ、 N clips × 5 lane の爆発、 lane 時間軸と clip-local 時間軸のズレ。

### 1.2 lane と ImageEvent.field の関係 = **override モデル**

- 採用理由: 既存 audio `TrackBuiltin::Volume` / `Pan` と完全に同 idiom (= track.volume は default、 lane が存在すれば lane の値が override)。 image clip ごとに初期位置を変えられる、 lane を削除すれば event 値に戻る。
- 拒絶: lane SSoT (= ImageEvent.x/y/w/h/opacity を廃止し lane が唯一の真値)。 理由: image clip ごとに違う初期位置を持ちたい場合 lane に point を打つ手間が増える、 既存 audio との idiom が割れる。

### 1.3 評価タイミング = **preview composite 時に毎フレーム lane を sample**

- 動画 / 音響と異なり、 image は静止なので「frame レート」 という概念は無い。 preview composite が毎 GUI frame 走るので、 そのタイミングで playhead の lane 値を読む。
- engine 側 (daw_audio) は image を扱わないので IPC 追加なし。 全て daw_gui プロセス内で完結。
- 既存 `daw_gui/src/image_compose.rs::active_image_sources_at` (playhead_beat) を拡張して、 各 image event の `(x, y, w, h, opacity)` 出力時に lane の値を override する。

## 2. データモデル変更

### 2.1 `AutomationTarget` に variant 追加

```rust
// common/src/model.rs

pub enum AutomationTarget {
    TrackBuiltin(TrackBuiltinParam),
    PluginParam { slot: PluginSlot, param_id: u32 },
    SongTempo,
    SongTimeSigNumerator,
    /// v14: image track 上の PiP 数値 (x/y/w/h/opacity)。 lane の
    /// 時間軸は track-global beats、 値域は 0.0..=1.0。 image clip が
    /// 存在する時間範囲だけ lane 値が画像に適用される。
    ImageBuiltin(ImageBuiltinParam),  // 新規
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ImageBuiltinParam {
    X,
    Y,
    W,
    H,
    Opacity,
}
```

`TrackBuiltinParam` (Volume / Pan / Mute / SendGain) と並ぶ enum で、 image 専用。 image を持たない audio track 上に `ImageBuiltin` lane を作成しても害は無い (= 単に評価機が見ない) が、 inspector の lane 追加 UI は image track だけに出す。

### 2.2 `automation_target_display_name` を拡張

```rust
pub fn automation_target_display_name(target: &AutomationTarget) -> String {
    match target {
        ...,
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::X) => "Image X".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Y) => "Image Y".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::W) => "Image W".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::H) => "Image H".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Opacity) => "Image Opacity".into(),
    }
}
```

### 2.3 Migration (`CURRENT_VERSION: u32 = 14`)

- v13 file は forward-migrate (= AutomationTarget enum に variant 追加のみ、 既存 data に影響なし)。
- bincode encoding は variant index に依存するので、 既存 4 variant の後ろに追加すれば backward-compat。 順序を保つ。

## 3. Engine / composite 側の変更

### 3.1 `daw_gui/src/image_compose.rs::active_image_sources_at`

現状: `ImageEvent.x/y/w/h/opacity` を直接 `ActiveImageFrame` に乗せて返す。
変更後: 引数に `song: &Song` を追加 (既に渡ってる)、 `track.automation_lanes` から `ImageBuiltinParam::{X,Y,W,H,Opacity}` を target に持つ lane を探し、 lane があれば lane の値を override する。

```rust
fn resolve_image_field(
    lanes: &[AutomationLane],
    param: ImageBuiltinParam,
    playhead_beat: f64,
    event_value: f32,
) -> f32 {
    let Some(lane) = lanes.iter().find(|l| matches!(l.target, AutomationTarget::ImageBuiltin(p) if p == param)) else {
        return event_value;
    };
    // 既存 audio TrackBuiltin と同じ評価機を使う
    common::automation::evaluate_lane_at(lane, playhead_beat)
        .map(|v| v as f32)
        .unwrap_or(event_value)
}
```

`evaluate_lane_at` は既存の `common/src/automation.rs` (audio TrackBuiltin / PluginParam の評価で使われている helper) を流用。 lane に clip が無い / 範囲外なら `None` を返す → event_value を fallback。

### 3.2 値域 clamp

lane の point.value は plain なので、 image field 用に `0.0..=1.0` で clamp して返す。 inspector / drag 編集で 0-1 外の point を作れないよう値域を制限。

## 4. UI / UX

### 4.1 Inspector: 「automate this knob」 機能

各 x/y/w/h/opacity 数値入力欄の横に「📈」 ボタンを置く (= 「この knob に lane を作る」)。 押すと:
1. 現在の track の `automation_lanes` に `AutomationTarget::ImageBuiltin(field)` の lane を追加。
2. lane の `default_value` は ImageEvent の現値。
3. arrangement 側に lane 行が表示される (gui_01 #028 既存 widget で自動描画)。

既存の `A` キー shortcut (= last-touched param に lane 追加) も image field に対応 (= inspector の数値入力欄に touch したら `touched_param` を image field にセット)。

### 4.2 Arrangement: lane 行表示

既存の `arrangement_view` で track の `automation_lanes` を描画している。 `AutomationTarget::ImageBuiltin` も同じ widget で表示 (= 0-1 範囲の point を打って繋いだ折れ線が出る)。 lane の `display_name` は `automation_target_display_name` で「Image X」 等と表示。

### 4.3 lane 削除で event 値に戻る

lane を arrangement の「×」 ボタンで削除すると、 override が消えて ImageEvent.field が effective になる。 inspector の数値入力欄が再び active になる (= lane があるときは数値欄を disabled or 半透明にする)。

### 4.4 preview window 上の drag 編集 (= P5 統合)

`docs/plan_image_overlay.md` §4 P5 で preview window 上で PiP rect を drag できる予定。 lane があるときは drag が「現在 playhead 位置に point を打つ」 動作になる (= AE の keyframe recording と同じ)。

## 5. Phase 分け

### P1. データモデル + migration + display name (= 単体動作可)

- `common/src/model.rs`: `AutomationTarget::ImageBuiltin(ImageBuiltinParam)` 追加、 `ImageBuiltinParam` enum 追加。
- `CURRENT_VERSION = 14`、 v13→v14 forward-migrate (空 default)。
- `daw_gui/src/app.rs::automation_target_display_name` 拡張。
- bincode Encode/Decode derive 確認、 v13 file 読込 test。

### P2. Engine: lane override

- `daw_gui/src/image_compose.rs::active_image_sources_at` 拡張、 各 image field を lane で override。
- `common/src/automation.rs::evaluate_lane_at` (既存) を流用。
- preview と render 両方で動く。

### P3. UI: inspector の「automate」 ボタン + lane 追加 / 削除

- Inspector の image event section に 5 個の「📈」 ボタンを追加。
- click で `AppEvent::AddAutomationLane { target: ImageBuiltin(field), default_value: event.field }` 発火。
- 既存 `A` shortcut の touched_param tracking を image field に拡張。

### P4. arrangement lane 行表示

- gui_01 #028 の既存 lane widget が `display_name` を読むので、 P1 で `automation_target_display_name` を拡張すれば自動的に動く。
- 必要なら gui_01 conversation file で要望 (= 既存 widget は audio field を前提に knob 色 / range を決めているなら image field 用の調整が必要かも)。

### P5. lane があるとき inspector 数値欄を半透明 + preview drag 連動

- lane が存在する field の inspector 数値入力欄を「現在値表示のみ」 (disabled or 半透明) に。
- preview window の drag handle が lane に point を打つ動作 (P5 統合)。

## 6. gui_01 への要望リスト

### 要望 1: automation lane widget が ImageBuiltin field を描画できること

- 現状: 既存 lane widget は `automation_target_display_name` で得た文字列を label に出すだけ (= image でも audio でも同じ widget)。
- 期待: そのまま動くはず。 ただし 0-1 range の clamp で y 軸 scale が崩れる可能性を確認。
- 関連仕様: `docs/plan_image_automation.md` §4.2

## 7. Out-of-scope (post-MVP)

- per-event keyframe (= After Effects 流の clip-bound keyframe)。 採用案 (track-level lane) でユーザー要望は満たされるので、 これは後付け検討。
- rotation / scale / 任意角度の transform。 まず x/y/w/h/opacity の 5 field のみ。
- per-image effects (blur / sharpen / color grading) の automation。
- Modulator / LFO による image field 駆動 (Bitwig 流 "Modulators")。

## 8. 未確定事項

- preview composite で lane を sample する頻度 (= 毎 GUI frame / playhead 変化時のみ): まず毎 frame で sample (= 30 ms ごと、 image 5 field × 1 lane sample = 軽い)、 重ければ後で最適化。
- lane の clamp 範囲: 0.0..=1.0 で固定。 ただし x/y は 0..1 外の point があると画像が画面外に出るが、 動画 MV では「画面外から flying in」 が普通の演出 → -0.5..=1.5 程度に緩める方向も検討。 まず 0..=1 で固定し、 ユーザー要望で広げる。
- `A` shortcut の touched tracking: inspector の数値入力欄に focus したタイミングで `touched_param` 更新 (= 既存 audio TrackBuiltin と同 idiom)。
