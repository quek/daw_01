# FIXME #81 — オートメーションの値を数値入力したい

## ゴール

オートメーションの値を、ドラッグだけでなく **人間可読単位での数値入力** で編集できる
ようにする。対象は 2 面:

1. **各オートメーション点の値** — 点をダブルクリックして、その場でインライン数値入力。
2. **レーンのデフォルト値** (`AutomationLane.default_value`) — レーンヘッダの横スライダー帯を
   廃止し、BPM と同じ **ドラッグスクラブ + クリックでタイプ** できる数値入力 (`scrubable_number`)
   に置き換える。

加えて (grill-me 2026-06-21 で確定):
3. **点ドラッグ中の現値表示** — 点をドラッグしている間、カーソル近くに現在値を人間可読単位で表示。

## grill-me で確定した設計判断

| # | 論点 | 確定 |
|---|---|---|
| 1 | 値の単位 | **インスペクタと同じ人間可読単位** (Volume=dB, Pan=-1..1, Rotation=度, FontSize=px, Tempo=BPM, PluginParam=native)。target→単位/format/range/変換 を **単一 SSoT 記述子** に集約。 |
| 2 | 点入力のトリガ | **点をダブルクリック → その場にインライン入力欄** (自動フォーカス + 全選択、Enter 確定 / Esc 取消)。空き場所の dblclick は従来通り新規点追加。 |
| 3 | 入力対象 | **値のみ** (時間=拍位置はドラッグのまま)。 |
| 4 | ドラッグ中表示 | **あり** (現値を人間可読単位でカーソル近くに)。 |
| 5 | 複数選択時 | **ダブルクリックした 1 点だけ** 変更 (現状 dblclick の 1 click 目でその点のみ選択になる挙動と整合)。 |
| — | デフォルト値の形 | **scrubable_number** (BPM 風、ドラッグ + タイプ)。横スライダー帯は廃止。 |

## 現状 (調査済み)

- 点 = `common::model::AutomationPoint { time_beat: f64, value: f64 (plain), curve }`。
  値はドラッグでしか変えられない。ドラッグ中の数値表示なし。点の上の dblclick は今は
  「同位置に新点追加」(点編集ではない)。
- デフォルト値 = `AutomationLane.default_value` (plain)。レーンヘッダの横スライダー帯
  (`default_band_rect`、`automation_default_band_h` 既定 4px) でドラッグ編集 → `SetLaneDefault`。
  レーン本体には `default_value_norm` 位置の水平ガイド線。
- 値の内部表現は **plain 単位**、widget は **0..1 正規化** (`value_norm`) で受け取る。
  変換 SSoT = `common::automation::{plain_to_norm, norm_to_plain}` (range-aware 版あり)。
- 既存数値入力 idiom:
  - `scrubable_number_at(id, rect, value, default, format, style, label, on_change, placeholder, modulation)`
    — ドラッグスクラブ + 短クリックでテキスト編集 (内部 `text_input_at_focused`)、format/range 対応、
    range clamp は widget 内。BPM (transport) / インスペクタで使用。
  - `text_input_at_focused(id, rect, text, on_change) -> TextInputResponse`
    — 初表示で自動フォーカス + 全選択、Enter/blur で `committed_text`、Esc で無確定終了。
- **HeavyCtx 内では `text_input` / `scrubable_number` は使えない** (push_edit / button_at / label_at /
  push_text / context_menu_for のみ)。インライン入力欄は **heavy の外で overlay** として置く
  (piano_roll の歌詞編集 / clip rename と同じ idiom)。
- widget は `ArrangementResponse.automation_point_rects: Vec<(AutomationPointKey, Rect)>`
  (可視点ごとの画面 Rect) を公開済。`automation_lane_header_layout(header_rect, style)` は `pub`。
- 単位変換ユーティリティ: `common::meter::linear_to_db` / `MeterScale::{db_to_frac, frac_to_db}`。
  track volume は mixer で dB 表示 (`mixer_strips.rs`: `20*log10(volume)`)。

## アーキテクチャ

daw-ui (widget) は **audio/target を一切知らない** 不変条件を維持する。よって
**値の人間可読フォーマット/パースは全て daw_01 側** が持ち、widget は「rect とイベントを公開する」
だけにする。3 つの overlay は全て daw_01 (`arrangement_view.rs`) が heavy の外で描く:

```
widget (daw-ui)                        daw_01 (arrangement_view.rs / app.rs)
─────────────────────────────────────  ──────────────────────────────────────────────
点の dblclick を hit-test し            DoubleClickAutomationPoint(key) を受けて
  → DoubleClickAutomationPoint(key)       editing_automation_point = Some(key) をセット
点ドラッグ中、live (key,value_norm,pos)  automation_point_drag を読んで
  を ArrangementResponse に公開           現値を人間可読に整形しカーソル近くに push_text
レーンヘッダの「デフォルト値フィールド    automation_lane_default_rects の各 rect に
  rect」を公開 (帯描画は廃止)             scrubable_number_at を overlay
```

### 1. SSoT 値記述子 (新規 `daw_gui/src/automation_value.rs`)

```rust
pub struct AutomationValueDisplay {
    pub unit: &'static str,            // "dB" / "°" / "px" / "BPM" / "" など
    pub format: ScrubableNumberFormat, // Integer / Decimal(n)
    pub range: (f64, f64),             // 表示単位での min/max (clamp 用)
    // plain (model) ↔ display (人間可読) の相互変換。
    pub to_display: fn(f64) -> f64,
    pub from_display: fn(f64) -> f64,
}
pub fn automation_value_display(target: &AutomationTarget,
                                plugin_range: Option<(f64, f64)>) -> AutomationValueDisplay;
```

target ごとの表 (既存表示サイトと一致させる):

| target | unit | format | range(display) | to_display / from_display |
|---|---|---|---|---|
| TrackBuiltin(Volume) | dB | Decimal(1) | (-60, 6) | linear↔dB: `20*log10`, `10^(db/20)`、0/負は -inf→-60 floor |
| TrackBuiltin(SendGain) | dB | Decimal(1) | (-60, 6) | 同上 |
| TrackBuiltin(Pan) | "" | `SignedLabeled{L,R,C,×100}` (= `PAN_FORMAT`) | (-1, 1) | 恒等 |
| TrackBuiltin(Mute) | "" | Integer | (0, 1) | 恒等 (0/1) |
| PluginParam | "" | Decimal(3) | plugin_range or (0,1) | 恒等 (plain=native) |
| SongTempo | BPM | Decimal(1) | (1, 400) | 恒等 |
| SongTimeSigNumerator | "" | Integer | (1, 32) | 恒等 |
| ImageBuiltin(X/Y/W/H/Opacity) | "" | Decimal(3) | (0, 1) | 恒等 (※ image inspector の表示に合わせ実装時確認) |
| ImageBuiltin(Rotation) | ° | Decimal(1) | (-180, 180) | rad↔deg |
| TextBuiltin(X/Y/W/H/Opacity/color) | "" | Decimal(3) | (0, 1) | 恒等 |
| TextBuiltin(Rotation) | ° | Decimal(1) | (-180, 180) | rad↔deg |
| TextBuiltin(FontSize/Outline/Shadow*) | px | Decimal(1) | 各 field の実レンジ | 恒等 |
| GroupTransform(X/Y/Anchor*/Opacity) | "" | Decimal(3) | (0, 1) | 恒等 |
| GroupTransform(Rotation) | ° | Decimal(1) | (-180, 180) | rad↔deg |
| GroupTransform(ScaleX/ScaleY) | × | Decimal(3) | (0.1, 10) | 恒等 (linear 表示) |

> 注: TextBuiltin の color/px 系・Image の %/px 表示は実装時に各 inspector のハードコードと
> 突き合わせて厳密一致させる。可能なら inspector 側もこの記述子へ寄せて SSoT 化 (range/format の
> 二重定義解消)。少なくとも automation の表示はこの 1 関数を SSoT とする。

### 2. 点の値インライン入力 (ダブルクリック)

**widget**:
- `enum ArrangementEditRequest` に `DoubleClickAutomationPoint(AutomationPointKey)` を追加。
- dblclick ハンドラ (現 `take_double_click_in_rect` → AddAutomationPoint 経路、~L9159) の **先頭**で
  既存の点 hit-test (`automation_point_at`, ~L4316) を行い、点に当たれば
  `DoubleClickAutomationPoint(key)` を発火して return (AddAutomationPoint より優先)。
  点に当たらなければ従来の AddAutomationPoint / CreateAutomationClip。

**daw_01**:
- `AppData.editing_automation_point: Option<AutomationPointKeyRef>` (session-only) を追加。
- `ArrangementEditRequest::DoubleClickAutomationPoint` → `AppEvent::BeginEditAutomationPointValue { key }`
  → `editing_automation_point = Some(key)`。
- `arrangement_view.rs`: `ui.arrangement(...)` の **後**で、`editing_automation_point` が `Some` かつ
  `automation_point_rects` にその key の Rect があれば、その Rect に
  `text_input_at_focused(("automation_point_value", track, lane, clip, idx), rect, &display_str, ...)`
  を overlay。
  - 初期表示文字列 = 現 point.value を `automation_value_display(target).to_display` → format。
  - `committed_text` を受けたら parse → `from_display` → range clamp → `plain_to_norm` 経由ではなく
    **plain を直接** `AppEvent::SetAutomationPointValue { key, value_plain }` で送り、`editing_*` をクリア。
  - パース失敗時は無視してクリア (元値維持)。blur / Esc (focused が落ちた) でもクリア。
- `enum AppEvent` に `SetAutomationPointValue { key: AutomationPointKeyRef, value: f64 }` を追加。
  handler: key を解決し `points[idx].value = value` (sort 不要、time 不変)。**undoable** (構造変化系に登録)。

### 3. デフォルト値の scrubable_number 化

**widget**:
- レーンヘッダの **default slider 帯描画を廃止** (`draw_automation_lane` の band fill / band hit-test /
  `automation_lane_default_drag` セッション一式 / `SetLaneDefault` 発火を撤去)。
  - 本体の水平ガイド線 (`default_value` 位置) は **残す** (デフォルト値の視覚位置として有用)。
- `AutomationLaneHeaderLayout` の `default_band_rect` を、**読める高さの数値フィールド rect**
  (`default_field_rect`、高さ ~18px、行高に余裕がある時のみ `Some`) に置換。
- `ArrangementResponse` に `automation_lane_default_rects: Vec<(AutomationLaneKey, Rect)>` を追加
  (可視レーンの `default_field_rect`)。`SetLaneDefault` は widget からは発火しなくなる
  (daw_01 の scrubable が直接 `AppEvent::SetLaneDefault` を出す)。

**daw_01**:
- `arrangement_view.rs`: arrangement() の後、`automation_lane_default_rects` の各 `(laneKey, rect)` に
  `scrubable_number_at` を overlay。
  - value = `lane.default_value` を `to_display`、format/range/unit = 記述子。
  - `on_change(display_v)` → `from_display` → clamp → `plain_to_norm` → `AppEvent::SetLaneDefault { track, lane, next_norm }`
    (既存イベント踏襲。set_lane_default は norm→plain 変換済)。
  - **undo**: 現状 `SetLaneDefault` は undo step 化されていない (knob 系)。scrubable は commit 境界が
    明確 (Enter / drag 終了) なので、確定時の 1 値を undo step 化するのが理想。実装時に
    「drag 開始時の prev を捕捉して commit で 1 step 登録」を検討 (過剰 step を避けつつ undoable に)。

### 4. 点ドラッグ中の現値表示

**widget**:
- `ArrangementResponse` に `automation_point_drag: Option<AutomationPointDragInfo>` を追加。
  `AutomationPointDragInfo { key: AutomationPointKey, value_norm: f32, cursor: (f32, f32) }`。
  point drag セッションの continuation frame で live 値を埋める (release frame では `None`)。

**daw_01**:
- arrangement() の後、`automation_point_drag` が `Some` なら、`value_norm` を
  `norm_to_plain(target, value_norm)` → `to_display` → `"{value}{unit}"` に整形し、
  カーソル近く (cursor + offset) に `ui.label_at` (背景 rect 付き) で表示。

## 実装範囲とファイル

- `daw_gui/src/automation_value.rs` (新規) — SSoT 記述子。
- `daw_gui/src/app.rs` — `editing_automation_point` field、`SetAutomationPointValue` /
  `BeginEditAutomationPointValue` イベント + handler、undoable 登録。
- `daw_gui/src/view/arrangement_view.rs` — 3 overlay (点入力 / デフォルト scrubable / ドラッグ表示)、
  edit-request → event 配線。
- `ui/crates/ui/src/widgets/arrangement.rs` — `DoubleClickAutomationPoint` 発火、band 廃止 +
  `default_field_rect` / `automation_lane_default_rects` / `automation_point_drag` 公開。
- `ui/crates/ui/src/widgets/arrangement.rs` の既存 test fixture (`default_value_norm: 0.5` 等) は
  field 名変更に追従。

## テスト

- **純粋ロジック (`automation_value.rs`)**: target ごとに `to_display`/`from_display` の往復が
  厳密逆になること、dB floor、rad↔deg、scale、各 range clamp をパラメタライズドテスト。
  特に Volume: `to_display(1.0)=0dB`, `to_display(2.0)≈6.02dB`, `from_display(to_display(x))≈x`。
- **app.rs**: `SetAutomationPointValue` が点の plain を正しく書く / time 不変 / undo 復元。
  `SetLaneDefault` 経路 (display→norm→plain) の往復一致。
- widget 側の rect 公開は heavy 依存で unit test 困難 → 実機検証 (§実機)。
- 既存 automation テスト (`common/src/automation.rs`、arrangement widget) を壊さない。

## 実機検証 (最終 sign-off)

- 点をダブルクリック → 入力欄が点位置に出て現値が選択状態 → 数値入力 Enter で点が移動。Esc 取消。
- 点ドラッグ中にカーソル近くへ現値 (dB/度/px) が出る。
- レーンヘッダのデフォルト値が BPM 風フィールドになり、ドラッグでも数値打ちでも変わる。
  本体の水平ガイド線が追従。
- Volume=dB / Pan=-1..1 / 回転=度 / Tempo=BPM など単位がインスペクタと一致。
- 保存/読込・undo/redo で値が保持される。
