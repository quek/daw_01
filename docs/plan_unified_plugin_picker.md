<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_unified_plugin_picker — 全カテゴリ混合の単一プラグインピッカー + 種別自動振り分け

FIXME #26。grill-me（2026-06-10）で「1 ボタン・混合リスト・種別による自動スロット振り分け・
修飾キー 2 軸・VST3 note-effect 判定」まで詰めた。

## 現状 (2026-06-10)

- **3 つの category 別ボタン**が picker を開く**前に** `PickerTarget`（Instrument / Fx / MidiFx）を
  決める（[track_inspector.rs:2024-2072](F:/dev/daw_01/daw_gui/src/view/track_inspector.rs)）。
  master は [:2026-2039](F:/dev/daw_01/daw_gui/src/view/track_inspector.rs) で「+ FX」1 個のみ。
- picker は `app.plugin_picker_visible` を描画（[plugin_picker.rs:148](F:/dev/daw_01/daw_gui/src/view/plugin_picker.rs)）。
  リストは category で **pre-filter** されている（[app.rs:14555-14567](F:/dev/daw_01/daw_gui/src/app.rs)、
  第 1 filter が `feature_key` 一致で絞る）。行は name / vendor / format のみ
  （[plugin_picker.rs:187-210](F:/dev/daw_01/daw_gui/src/view/plugin_picker.rs)）。category は非表示。
- 選択 `select_plugin_from_db`（[app.rs:14063-14173](F:/dev/daw_01/daw_gui/src/app.rs)）:
  冒頭で picker を閉じ（`is_plugin_picker_open = false`, :14064）、`PickerTarget` → `dest_slot`
  （:14114-14125）、master 特例（:14081-14106）、`pending_added_plugin_finalize` に積んで
  **ロード完了後 GUI 自動 open**。
- **カテゴリ source**: CLAP / builtin は `features` を完全保持
  （[plugin_db.rs:369](F:/dev/daw_01/common/src/plugin_db.rs)、builtins は :173-205 で手書き）。
  VST3 は `instrument` | `audio-effect` の二択に潰れ、生の `subcategories` は DB 境界で破棄
  （[vst3_scan.rs:170-182](F:/dev/daw_01/common/src/vst3_scan.rs)、
  [plugin_db.rs:253-264](F:/dev/daw_01/common/src/plugin_db.rs)）。
- 修飾キーは `ui.pointer().modifiers`（ctrl/shift/alt/logo）で読める
  （gui_01 [event.rs:43-70](F:/dev/gui_01/crates/platform/src/event.rs) /
  [input.rs:26-44](F:/dev/gui_01/crates/ui/src/input.rs)、capture は
  [runner.rs:738-746](F:/dev/daw_01/daw_gui/src/view/runner.rs)）。現 picker は未参照。

## 確定仕様 (grill-me 2026-06-10) — 見える挙動

- **1 ボタン「+ Plugin」**。インスペクタの +Inst / +FX / +MIDI を統合。リストは**全カテゴリ混合の
  フラット 1 本**。
- **各行に種別タグ**（`楽器` / `FX` / `MIDI`）。選ぶ前に行き先が一目で分かる。
- **種別で自動スロット振り分け**。優先順 **note-effect > instrument > audio-effect**（音を出す方が勝つ）。
  リバーブ内蔵シンセは楽器スロットへ。
- **未分類（どの主カテゴリも名乗らない）→ FX チェーン**。
- **2 つ目の楽器 → 黙って差し替え**（楽器スロットは 1 個、`Track.instrument: Option`、聴き比べ用途）。
- **master 選択時はリストに FX のみ表示**（行き先が受け付けるものだけ出す）。
- **修飾キー 2 軸**（click と Enter 確定の両方に効かせる）:

  | | GUI を開く | GUI を開かない |
  |---|---|---|
  | **閉じる** | 無修飾 | Shift |
  | **開いたまま** | Ctrl | Ctrl+Shift |

  Ctrl で開いたまま連続選択 → 各プラグインが自分の種別で正しいチェーンに積まれる
  （FX を立て続けに追加できる。楽器を 2 連続なら差し替え）。

## VST3 note-effect 判定（確定: スキャン時判定 + プロセス隔離）

- VST3 には note-effect を示す category tag が **存在しない**（Steinberg `PlugType` に該当なし。
  一次情報: `ivstaudioprocessor.h` の `PlugType`、`ivstcomponent.h` の `MediaTypes`）。
  判定は **bus 構成**でしか取れない: `getBusCount(kEvent, kInput) > 0` かつ
  `getBusCount(kEvent, kOutput) > 0` かつ `getBusCount(kAudio, kOutput) == 0`
  （channelCount ではなく bus 存在で判定）。
- 現状の scan は **メタデータのみ**読み、plugin code を一切走らせない
  （[vst3_scan.rs:9-13,255-289](F:/dev/daw_01/common/src/vst3_scan.rs)、
  [plugin_db.rs:210-211](F:/dev/daw_01/common/src/plugin_db.rs) で「起動は load 時に遅延」と明記）。
  bus を読むには **scan が各 VST3 を起動する必要**があり、固まる/クラッシュの risk が新規に生じる。
- **回避**: plugin code を動かす役は既に別プロセス `daw_plugin_host`
  （`create_instance` + `IHostApplication` + `getBusCount` は
  [vst3_plugin.rs:264-287,429-438,659-678](F:/dev/daw_01/daw_plugin_host/src/vst3_plugin.rs) に存在）。
  scan 時の bus 探りを **plugin_host に IPC で投げ、per-plugin timeout + 使い捨て**で隔離する
  （Ardour / Bitwig 等と同じ out-of-process scan）。残コストは「scan が多少遅い（キャッシュ済）」のみ。
- `PluginEntry` に `is_note_effect: bool` を追加（`#[serde(default)]` で additive、version bump 不要。
  既存キャッシュは一度再スキャンで埋まる）。`Vst3ClassEntry::features()`
  （[vst3_scan.rs:170-182](F:/dev/daw_01/common/src/vst3_scan.rs)）に `note-effect` 分岐を追加。
  CLAP は変更不要。

## 実装メモ

1. **ボタン統合**: track_inspector の 3 ボタンを単一「+ Plugin」に（:2024-2072）。master は引き続き 1 ボタン
   だがリスト側を FX-only に。
2. **pre-filter 撤去**: app.rs:14566 の第 1 filter（category）を外す。第 2 filter（検索クエリ）は維持。
   master のときだけ「FX のみ」に切替。view は `plugin_picker_visible` を素通しなので変更不要。
3. **dest_slot を features 由来に**: app.rs:14114-14125 を「picked entry の `features` → 優先順規則
   → slot」へ。`PickerTarget` は廃止 or *derived* に転用。`OpenPluginPickerFor`（app.rs:4710-4714）
   を引数なし open へ。
4. **2nd instrument 差し替え**分岐を追加。
5. **行 category タグ**を heavy block の `push_text` で描画。
6. **修飾キー**: select 経路（click / Enter）で `ui.pointer().modifiers` を読む。Ctrl のとき
   `is_plugin_picker_open=false` をスキップ、Shift のとき `pending_added_plugin_finalize` に積まない。
7. **VST3 probe**: scan_system から plugin_host への bus-probe IPC を新設（timeout + 隔離）。
   結果を `PluginEntry.is_note_effect` に格納。

## 実装状況 (2026-06-10)

- **Phase A landed** (commit 58f9d2d): 1 ボタン統合 / 混合リスト / `from_features` 自動振り分け /
  種別タグ / master FX-only / 2nd instrument 差し替え / 修飾キー / close-on-select。
- **Phase B landed**: VST3 note-effect を再スキャン時の使い捨て probe プロセス
  (`daw_plugin_host --probe-vst3 <path> <id>` → bus 構成 event-in/out + no-audio-out 判定) で検出し、
  features に `"note-effect"` を追記 (routing は `from_features` が拾う)。 プロセス隔離 + 8s timeout で
  壊れた / ハングする VST3 が scan を巻き込まない。 probe 失敗 / timeout は false fallback (= FX 扱い、
  退行なし)。 再スキャン進捗を load_overlay に表示。 **実機 VST3 での挙動確認は最終バッチで**。

## 未深掘り（既定で進める）

- リスト並びは現状維持（名前順）。種別タグで区別。
- タグ文言: `楽器` / `FX` / `MIDI`。

関連: [plan_font_picker.md](plan_font_picker.md)（同じ検索付きピッカー widget を共有）、
[plan_plugin_picker_keyboard_nav.md](plan_plugin_picker_keyboard_nav.md)（既存の ↑↓/Enter ナビ）。
