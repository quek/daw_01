# FIXME #89 — ドラッグ端オートスクロール (edge auto-scroll)

普通の DAW のように、トラック / クリップ / リージョン / ノート等をドラッグ中、ポインタが
表示領域の端に達したらその方向へ自動スクロールし、掴んでいる対象がカーソルに追従し続ける。

## 調査で確定した前提 (一次情報)

### アーキテクチャ
- **drag のステートマシンはすべて gui_01 widget 内**:
  `ui/crates/ui/src/widgets/arrangement.rs` (15k 行) / `piano_roll.rs` (7.4k 行)。
  daw_01 は完了時に高レベル `ArrangementEditRequest` / `PianoRollEditRequest` を `make_edit`
  callback 経由で受けるだけ。
- **scroll/zoom の SSoT は daw_01 `AppData`**、widget には `view` 構造体で渡る:
  - arrangement: `arrange_scroll_beat`(横・拍) / `arrange_track_top`(縦・px) / `arrange_zoom_x`(px/拍)。
    widget が `ArrangementEditRequest::SetScrollX(f64 abs)` / `SetTrackTop(f32 px)` を emit (wheel 既存経路)。
  - piano roll: `pianoroll_scroll_beat`(横・拍, clip 相対) / `pianoroll_top_pitch`(u8) / `pianoroll_zoom_x,y`。
    **scroll は現在 view 層 `piano_roll_view.rs` の wheel 処理が `resp` を見て emit**。widget には scroll
    edit request が無い → 本実装で追加する。
- 連続再描画: 既定は `ControlFlow::Wait`。`render_frame()` が true を返すと `request_redraw` が連鎖
  (再生中 playhead 追従)。widget は `self.request_redraw()` (= `Ui::request_redraw`) で次フレームを要求できる。
  `PointerFrame.primary_pressed` で「ボタン押下中」、`pos` で現在カーソルを毎フレーム取得。

### 追従の数学 (核心)
横方向 (beat) と piano roll 縦 (pitch) の drag delta は **screen 相対**
(`(last_mouse - anchor_mouse) * factor`) なので、スクロールしてもカーソルが止まっていれば delta が
変わらず**対象が追従しない**。一方 arrangement の縦 (to_track) は毎フレーム live な `track_top` から
`track_index_from_y` で再計算されるので**自動追従済み**。

→ 各 drag session に **press 時の scroll 値を保持** し、delta 計算に `(現在 scroll − press scroll)` を
加算してコンテンツ空間 delta にする (= tldraw / DAW 共通のベストプラクティス「delta は content space」)。
これでスクロール中もカーソル位置に追従、スクロール 0 のときは既存式と完全一致。

```
raw_beat_delta_content = (last_mouse.x - anchor_mouse.x) * beat_per_px      // 既存
                       + (view.start_beat - press_view_start_beat)          // 追加項
```

`compute_*_drag_beat_delta` 系 helper は `raw_beat_delta` を入力に取るので、各呼び出し位置で上記
追加項を足すだけ。snap (絶対位置 snap) / preview / commit は下流で変更不要。
short-click 化の閾値 `dist = |last_mouse.x - anchor_mouse.x|` は screen 実移動のまま (スクロール量で
誤って commit 化させない)。

## 実 DAW 挙動 (調査: Ableton / REAPER / Bitwig / Studio One + tldraw/dnd-kit)
- 両軸対応 (timeline は横、ノート/トラック方向は縦)。掴んでいる物が追従。
- hot-zone: 端からの近接帯。速度は近いほど速い (proximity ramp, linear/ease)、ズーム正規化。
- 範囲選択 (marquee) でも発火。横は content 端を越えて自由にスクロール (timeline 延長)、縦は content 内に clamp。

## 最終形の仕様

### 共通ヘルパ (`ui/crates/ui/src/widgets/` に追加, 例 `edge_scroll.rs`)
```
struct EdgeScrollCfg { zone_px: f32, max_speed_px_per_frame: f32 }
// 端からの距離で 0..1 の近接係数を線形に取り、速度 = 係数 * max_speed。
// content rect と pointer から各軸の scroll px/frame を返す。axis ごとに enable。
fn edge_scroll_delta(pointer_xy, rect, cfg, enable_x, enable_y) -> (f32 dx_px, f32 dy_px)
```
- `zone_px` 既定 = 28px (固定 px。可変領域でも端付近の操作感が一定)。
- `max_speed_px_per_frame` 既定 = ~18px/frame (端で最速、近接 0 でゼロ)。フレームレート依存だが
  `Wait` ループでも request_redraw 連鎖で実用上一定。実機で微調整。

### 駆動 (各 widget の drag continuation 直後に 1 ブロック)
drag session が active なら:
1. `(dx_px, dy_px) = edge_scroll_delta(pointer.pos, content_rect, cfg, axes)`
2. 非ゼロなら:
   - 横: `new_start = view.start_beat + dx_px * beat_per_px` → arrangement は `SetScrollX(new_start.max(0))`、
     piano roll は新 variant `SetScrollBeat(new_abs)` (view 層で `- clip_start` して `SetPianoRollScrollX`)。
   - 縦(arrangement): `new_top = (view.track_top + dy_px).max(0)` を content 高で clamp → `SetTrackTop`。
   - 縦(piano roll pitch): 端数 px を session に貯め、|累積| ≥ zoom_y で `±1` semitone → 新 variant
     `SetTopPitch(u8)` (view 層で `SetPianoRollTopPitch`)。pitch は 11..127 で clamp (既存 handler)。
   - `self.request_redraw()` で次フレーム確保 (カーソル静止でもスクロール継続)。
3. press-scroll capture (`press_view_start_beat` 等) により対象が追従 (上記数学)。

### 対象 drag (網羅)
**arrangement**: クリップ移動(横追従+縦自動) / クリップ resize L,R(横) / セクション(リージョン)
move,resize,create(横) / トラック並べ替え(縦) / オートメーションクリップ move,resize(横+縦自動) /
オートメーションポイント move(横のみ。縦は値で scroll 軸でない) / 範囲選択 lasso(横+縦) /
ループ範囲・playhead scrub(横)。
**piano roll**: ノート移動(横追従+縦pitch追従) / ノート resize L,R(横) / ノート新規作成 drag(横, warp
settled 後のみ) / 範囲選択(横+縦) / ループ範囲・playhead(横)。
**除外** (scroll 軸でない局所操作): audio fade、velocity lane drag、splitter / row resize、track volume band。

### 移動量ゲート (click-and-hold を除外)
端近くの clip / note を **クリックして保持しただけ** で view が飛ぶのを防ぐ。実 DAW は「実際に
ドラッグして初めて」端スクロールする (静止クリックでは動かない)。各 widget state に press 位置
(`edge_scroll_press`) を持ち、press からの移動が `ACTIVATE_PX` (= 4px、short-click 化閾値と同値) 以上に
なって初めて端スクロールを許可する。一度ドラッグが成立すれば、端でカーソルを止めても (press から
十分離れているので) スクロールは継続する。marquee も同ゲート (drag_rect の drag_start は press 即セット
されるため、ゲート無しだと空き領域の click-hold でスクロールしてしまう)。

### 境界
- 横: 左 0 で clamp、右は無制限 (timeline 延長。既存 handler が `.max(0.0)` のみ = 互換)。
- 縦 arrangement: 既存 `SetTrackTop` と同じく下限 0 のみ (上限 clamp は handler 非対象 = wheel 挙動と互換。
  要件外の scroll 境界変更を避ける)。
- 縦 piano roll: top_pitch 11..127 (既存 handler clamp)。

### 横スクロールの floor clamp (左端 runaway 防止)
piano roll の `ScrollByBeats` は delta で渡るが、widget が **`PianoRollView.min_start_beat` (= clip 開始拍)**
を受け取り、`new_start = (start_beat + dx*bpp).max(min_start_beat)` で clamp してから「実際に適用される
delta」を emit + anchor 補正する (arrangement の `SetScrollX` と同パターン)。これにより左端 (clip 先頭) で
view が止まっても anchor は要求 px でなく実スクロール px ぶんしか shift せず、掴んだ note が画面外へ飛ぶ
runaway を防ぐ (review CRITICAL)。縦 (pitch) は `applied = cur - clamp(11..127)` で同様に実適用量を使う。

### 新規 API (gui_01 widget)
- `PianoRollEditRequest::SetScrollBeat(f64)` (絶対 song beat) / `SetTopPitch(u8)` を追加。
  `piano_roll_view.rs` の `make_edit` closure で `SetPianoRollScrollX(abs - clip_start)` /
  `SetPianoRollTopPitch` に変換。
- arrangement は既存 `SetScrollX` / `SetTrackTop` を再利用 (新 variant 不要)。
- 各 drag session 構造体に `press_view_start_beat: f64` (+ piano roll は `press_top_pitch_f: f32` 等) を追加。

## テスト方針
- `edge_scroll_delta` の純粋ロジック: zone 内外・各端・近接 ramp・両軸・ズーム正規化をパラメタライズドで検証。
- content-space delta の追従: press_view_start を与えて scroll 変化時に beat delta が正しく増えることを検証
  (widget helper を純粋関数に切り出してテスト)。
- 実 drag (UI 操作) は実機検証 (`/verify-app`)。視覚・操作感は build/test をすり抜けるため必須。

## 非対象 / 留意
- FIXME #87 (zoom/scroll をプロジェクト保存) は別 worktree。本実装は scroll 値の mutate 経路を共有するが
  保存ロジックには触れない。
- RT パス (daw_audio) には無関係 (GUI のみ)。
