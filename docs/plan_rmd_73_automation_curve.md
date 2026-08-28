# r.md #73 — オートメーションカーブ (線を直接曲げる / 符号の是正 / 数式 SSoT)

**この計画は #73 専用であり、他項目との統合順は `docs/plan_rmd_index.md` を見ること。**

このファイルだけを読んで実装を完走できるように書いてある。調べ直しは不要。

行番号は **2026-08-28 時点の main (#77 前)** で実測した値。#77 が landed した後は
§0 の対応表でファイルを読み替える (行番号ではなく「どの処理か」で識別する)。

---

## 0. 前提 — 着手タイミングとファイル構成

- **#77 (`arrangement/run.rs` の分割) が main に入った後に着手する。** #73 は press 振り分け /
  drag 継続 / release commit / render overlay の 4 か所すべてに触るので、`run.rs` を 1 関数から
  分割する #77 と同時に走らせると確実に衝突する
  (memory `feedback_agent_bigfile_no_parallel_split`)。
- **`docs/plan_rmd_77_arrangement_split.md` は既に存在する。着手前に必ず読むこと。**
  (旧版の本計画と裏取りレポートは「未作成」と書いていたが、2026-08-28 時点で作成済み。
  以下の対応表は同ファイル §4「ファイル構成」の由来行に合わせてある。)
- 着手前に `git log --oneline -5` と `ls daw_gui/src/widgets/arrangement/` で
  **#77 が landed したか**を実際に確認する (`frame.rs` / `press.rs` / `press_lanes.rs` /
  `drag.rs` / `sessions.rs` / `cursor.rs` が存在すれば landed)。
  **未 landed なら本計画の作業を始めてはいけない。**

  > 2026-08-28 時点の実測: `daw_gui/src/widgets/arrangement/` は
  > `content_build.rs` / `draw.rs` / `geometry.rs` / `mod.rs` / `release.rs` / `render.rs` /
  > `run.rs` / `tests.rs` / `view_build.rs` の **9 本のみ**で、#77 は**未 landed**。
  > この状態で #73 に着手してはいけない。

  | #77 後のファイル | 由来 (現 `run.rs` の行、#77 §4) | #73 が触る内容 |
  |---|---|---|
  | `press.rs` | 125-230, 334-450, 983-998 | `PressClaim` から `curve_handle` を撤去 / `session` の 11 列挙を差し替え / `point` を**立てる条件**を差し替え (seed は据え置き。§3.5) |
  | `press_lanes.rs` | 232-333, 619-980 | `curve_handle` / `alt_resize` の 2 本を関数ごと削除 / `point` の hit 結果を hoist / 区間 bend press を追加 / `automation_clip` と `lasso` の `!alt` ゲート撤去 |
  | `drag.rs` | 1008-1399 | curve param continuation → bend continuation |
  | `sessions.rs` | 1400-1683 | `LiveSessions` / `ReleasedSessions` / `Overlays` のフィールド差し替え |
  | `cursor.rs` | 1684-1849 | Alt hover 中の区間 hover 算出 + カーソル |
  | `render.rs` | 既存 + 1851-2097 | ハンドル描画の撤去 / bend preview + hover 強調 / `Overlays` フィールド名 / **`HeavyInput` に `hovered_segment` を追加** (§4.10 / §4.11) |
  | `release.rs` | 既存 (改修のみ) | 選択の相互 clear 撤去 / bend release / Alt+ダブルクリック / marquee の `no_session` 列挙 |

- **#77 で導入され、#73 が名指しで触る型** (`docs/plan_rmd_77_arrangement_split.md` §6-B / §6-F / §6-J):
  - `PressClaim { splitter, curve_handle, point, session }` (press.rs) と
    `PressClaim::from_live` の **11 session 列挙**。
  - `PressActions { seek_beat, lane_toggle, lane_button, delete_point }` (press.rs)。
  - `LiveSessions` / `ReleasedSessions` / `Overlays` (sessions.rs)。
  - `HeavyInput` (render.rs、#77 §6-J:1137-1152)、
    `render_arrangement_heavy(hctx, f, heavy, overlays)`。
    **#73 は `HeavyInput` に `hovered_segment: Option<AutomationPointIdKey>` を 1 本足す**
    — 既存の `hovered_clip` (#77 §6-J:1142) と同じ「response の写しを heavy へ渡す」経路で、
    §4.11 の hover 強調はこれが無いと描けない。`viewport_key_hash` の材料にしないことも同じ
    (§8-12)。
  - `press_lanes::{clip_zone, automation}` と内部の
    `curve_handle` / `point` / `automation_clip` / `alt_resize` / `lasso`。
  - `cursor::{hover, apply}`。
- **#77 が別構成で landed したときは、行番号ではなく処理内容で読み替えること。**

---

## 1. この作業のゴール (確定済み。変更しないこと)

1. **(A) 上下逆の是正** — モデル (`AutomationCurve` / `apply_curve`) は**変えない**。壊れているのは
   UI 側の写像だけ。保存形式が変わらないので `CURRENT_VERSION` の bump も migration も**不要**。
2. **(B) 線を直接曲げる** — レーン本体の線を **Alt+ドラッグ**して曲げる。Hold / Linear の区間は
   自動で「曲線」に変換してから量を付ける。**Alt+ダブルクリック**で直線に戻す。
   掴んだ場所が指に付いてくるよう、感度定数ではなく**逆算**で決める。
3. **(C) 中央ハンドルの撤去** — `find_curve_param_handle_at` 一式を全部消し、
   `AppEvent::SetAutomationCurve` **1 本**に集約する。
4. **(D) カーブ種別メニューの平易化** — 4 項目のまま **階段 / 直線 / 曲線 / S 字**に改名。
5. **(E) 選択の共存** — 点クリックとクリップクリックの**相互 clear を両方削除**する。
   オートメーションクリップにタイトル帯は**付けない**。
6. **(F) 数式の SSoT** — 曲線の形を計算する関数を `common::automation::apply_curve` **1 本**にする。
7. **(G) 安定 id の前提を成立させる** — `SetAutomationCurve` は point を安定 id で指す。
   その前提を壊している唯一の実行時経路 (`thin_collinear_and_insert` が `id: 0` を挿す =
   オートメーション録音) を塞ぐ。§2.7 / §3.7 / §4.1 / §4.18。

**やらないこと (明示的に却下済み)**
- モデルを画面基準に作り替える (旧調査レポートの案)。**採用しない。**
- `AutomationCurve` の variant 名変更 / 追加 / 削除。**しない。**
- オートメーションクリップのタイトル帯。**付けない。**
- 下端スプリッタの当たり判定を広げる。**広げない。**
- Alt+Shift の新規割り当て。**使わない。**
- `SelectAutomationPoints` / `MoveAutomationPoints` / `DeleteAutomationPoints` /
  `QuantizeSelectedAutomationPoints` / `app_types.rs` の `AutomationPointKeyRef` の
  positional → id 化。**#73 では触らない** (§8-10)。

---

## 2. 何がどう壊れているか (実コードで確認済み)

### 2.1 符号 — パラメータの定義軸とジェスチャの軸が違う

`common/src/automation.rs:250-257` (`apply_curve` の Exponential arm):

```rust
AutomationCurve::Exponential { bend } => {
    let k = 2f64.powf(f64::from(bend));
    a + (b - a) * u.powf(k)
}
```

`bend` は「区間の**進捗**をどれだけ後ろ倒しにするか」で、画面の上下ではない。`bend > 0` は
`u^k < u` なので値は常に **a 寄り**に留まる = 上り区間 (b>a) では直線より**下**、下り区間 (b<a) では
直線より**上**。同じ `bend` が画面上では逆向きの膨らみを意味する。

一方ドラッグは画面基準 (`daw_gui/src/widgets/arrangement/geometry.rs:1523-1526`、doc 1518-1522):

```rust
pub(super) fn curve_param_delta_from_dy(dy: f32, effective_h: f32, alt: bool) -> f32 {
    let raw = -dy * 2.0 / effective_h.max(1.0);   // 上ドラッグ = bend 増加
```

区間の向き (符号付き高さ) を一切見ていないので、上り区間ではハンドルを上へ引くと線が下へ沈む。

**業界の答え (一次情報)**: 保存する値は **progress 基準が多数派**
(REAPER `tension` / Tracktion `curve` / Vital `power` / Surge XT MSEG `cpv` / Ableton
`CurveControl*` — screen 基準は Zrythm のみ)。一方 **ドラッグのジェスチャは読めた実装すべてが
画面基準**で、UI 層でマウス移動量を**区間の符号付き高さで割って反転**させている
(Surge XT `float segdx = nv1 - v0;` → `dv = -2*dy/vscale/(0.5*segdx);` /
Vital `if (getPoint(index).second < getPoint(index+1).second) alternate_mult = -1.0f;`)。
出典は調査成果物 `scratchpad/curve_research.md`。

したがって **モデルは正しい。UI の写像だけを直す。**
本計画では感度定数を捨てて**逆算**にする (§3.4) — 逆算の式は `(b - a)` で割るので、
**区間の符号付き高さが自動的に効き、符号は構造的に正しくなる** (定数 `dir` を掛ける小細工は不要)。

`AutomationCurve` の定義 (`common/src/model/automation.rs:149-171`) 自身が
`tension` / `bend` の値域を **`-1.0..=1.0`** と宣言している。この値域は #73 でも変えない
(§3.4 の clamp はこの宣言に従うだけであって、UI 都合の妥協ではない)。

### 2.2 Bezier のハンドルは原理的に動かない

`geometry.rs:1388-1398` の `evaluate_bezier_y` を t=0.5 で評価すると
`y = ⅛a + ⅜c1y + ⅜c2y + ⅛b`、かつ `c1y + c2y` は tension に依らず常に `a + b` なので
**常に `(a+b)/2`**。つまりハンドルは 1px も動かない (曲線自体は
`render.rs:501-541` の preview で変わるので「効いていない」わけではないが、
**直接操作の不変条件が壊れている**)。ハンドル方式は撤去する。

### 2.3 線そのものへの当たり判定が無い

`geometry.rs:1445-1447` は `selected_points.is_empty()` で即 `None`、`geometry.rs:1481-1489` は
`Hold` / `Linear` を `_ => continue` で除外 (`find_curve_param_handle_at` = `geometry.rs:1435-1516`、
doc 1427-1434)。つまり「選択済み」かつ「既に Bezier/Exponential」の
区間にしかハンドルが無い。区間の hit-test は存在しない。

### 2.4 Alt が別機能に占有されている

`run.rs:835-919` が「Alt+ドラッグ = レーン / トラック行の高さ変更」を持っている
(`run.rs:841` の `let in_arr = in_lanes || (header_w > 0.0 && header_pane.contains(px, py));` で
**ヘッダ列も含む**)。

> **削除範囲は 835-919 ちょうど。** 834 と 920 は空行で、**921 は次の lasso ブロックの
> 先頭コメント** (「M14 Phase 63n-8 (#033): automation point の lasso press —」)。
> 「834-921」で削ると lasso の説明コメントを巻き込む。

**撤去してよい根拠 (実コードで確認済み)**: `release.rs:989-1073` の Alt+ホイールが
`content_below_ruler`
(= `header_pane.x` から `header_pane.w + lanes.w` の幅、`lanes_h` の高さ = **ヘッダ列を含む**、
`release.rs:989-995`) で `take_scroll_in_rect` しており、`SetArrangeTrackRowH` (`:1037`) /
各 track の `SetSingleTrackRowH` (`:1054`) / 各 visible lane の `SetLaneHeight` (`:1070`) を
同一 factor で一括発行する。よって
**ヘッダ列の上でも、レーン本体の上でも、Alt+ホイールで行 / レーンの高さを変えられる。**
加えてレーン下端 / 行下端 / ヘッダ境界のスプリッタ (`run.rs:152-230`) が残る。

### 2.5 選択の相互 clear が規約と矛盾している

- `release.rs:335-338` — 点の無修飾クリックで `SelectAutomationClips { next: vec![] }`
  (理由コメントは **332-334**)。
- `release.rs:546-549` — クリップの無修飾クリックで `SelectAutomationPoints { next: vec![] }`
  (理由コメントは **543-545**)。

理由はコメント上「見た目の混乱」だけ。一方
`daw_gui/src/handler/selection_view.rs:51-52` の `edit_surface` 自身の doc は
「選択集合は面を跨いで共存できる … clip 選択は automation 選択を消さない」と書いており、
`daw_gui/src/view/root.rs:1138-1143` の Ctrl+A 2 段目のコメント
(「tier2 で点とクリップが両方選択された状態になるが、直近選択 (= clip) が last-wins で
copy/cut/delete の対象になる (edit_surface 参照)」) も共存前提。
**同一機能内で規約が自分自身と矛盾している。** 曖昧さは `edit_surface` の last-wins が既に解決済み。

### 2.6 曲線の数式が 3 か所に散っている

| 場所 | 何を計算しているか |
|---|---|
| `common/src/automation.rs:244-259` (`apply_curve`、doc 218-243) | 再生の SSoT。plain 値空間。 |
| `daw_gui/src/widgets/arrangement/draw.rs:1776-1840` (`flatten_lane_segment`、doc 1770-1775) | 描画。**screen y で式を再実装**。 |
| `daw_gui/src/widgets/arrangement/geometry.rs:1388-1425` (`evaluate_bezier_y` + `compute_curve_handle_pos`) | ハンドル位置。3 本目。 |

さらに `ArrangementCurveKind` (`mod.rs:431-437`、doc 411-430) は
`common::model::AutomationCurve` の
mirror 型で、**アーキテクチャ不変条件 8「DAW 固有 widget は `common::model` 直結、mirror 型を
作らない」に反している** (`view_build.rs:755-764` の `model_curve_to_widget` が 4:1 変換)。
式が分かれているから「評価と描画は一致しているのにハンドルの向きだけ間違う」が成立した。

### 2.7 安定 id に穴がある — オートメーション録音が `id: 0` の点を挿している

`common/src/automation.rs:458-488` の `thin_collinear_and_insert` (doc コメントは**無い**) は
`AutomationPoint { id: 0, .. }` (`:481`) を挿入する (= v29 の未採番 sentinel)。

**この関数は死んでいない。実行時経路がある**:
`daw_gui/src/handler/tick.rs:648` (`insert_recording_point`) ← `tick.rs:236`
(= オートメーション録音の live tick)。つまり **録音した点はセッション中ずっと `id == 0`** で、
保存 → 再読込で `Song::ensure_ids` (`common/src/model.rs:1747` → `ClipContent::ensure_element_ids`、
`common/src/model/content.rs:281-315`) が初めて採番する。

#73 は `SetAutomationCurve` を **安定 id** でアドレスするので、この穴を放置すると
「録音した点が全部 id 0 → `find(|p| p.id == point_id)` が先頭の別の点を掴む」が成立する。
**#73 の一部として塞ぐ** (§3.7 / §4.1 / §4.18)。

> **実行時に `id: 0` の `AutomationPoint` を作る箇所はここ 1 つだけ**であることを
> 全件確認した (2026-08-28、`grep -rn "AutomationPoint {" | grep "id: 0"` + 各 site の
> `#[cfg(test)]` 位置を照合)。列挙は次で**全部**:
>
> | 場所 | 判定 |
> |---|---|
> | `common/src/automation.rs:481` (`thin_collinear_and_insert`) | **これが唯一の production 経路。§3.7 / §4.1 で塞ぐ** |
> | `handler/automation.rs:317` / `:800` / `:1216`、`handler/media.rs:959` | `alloc_point_id()` か 1 始まりの連番を入れている (= `id: 0` を作らない) |
> | `common/src/audio_render.rs:1191-1192` | `#[cfg(test)]` (`:879`) の中 |
> | `common/src/tempo_map.rs:163-164` | `#[cfg(test)]` (`:139`) の中 |
> | `daw_gui/src/midi_export.rs:287-293` | `#[cfg(test)]` (`:237`) の中 |
> | `daw_gui/src/video_playback.rs:526-527` | `#[cfg(test)]` (`:444`) の中 |
> | `daw_audio/src/metronome.rs:253-254` | `#[cfg(test)]` (`:174`) の中 |
> | `daw_audio/src/audio_clip_renderer.rs:2815-2816` | `#[cfg(test)]` (`:2186`) の中 |
> | `common/src/automation.rs:1150` / `:1156` / `:1287` / `:1293` | 同ファイルの `#[cfg(test)]` (`:603`) の中 |
> | `common/src/model/tests.rs:1639` / `:1645` | テストファイル |
> | `impl Default for AutomationPoint` (`common/src/model/automation.rs:196-205`、`id: 0`) | **呼び出し 0 件** (`AutomationPoint::default()` / `..Default::default()` の grep が空) なので実行時に到達しない |
>
> `daw_audio/src/automation.rs` は `AutomationPoint` を 6 か所 (`:277` / `:283` / `:570` /
> `:576` / `:682` / `:688`) 組むが **`id: 1` / `id: 2` を入れており `id: 0` 経路ではない**
> (かつ `#[cfg(test)]` (`:257`) の中)。裏取りレポートがここを `id: 0` として挙げていたのは誤り。

---

## 3. 設計

### 3.1 モデルは触らない

`common/src/model/automation.rs:145-171` の `AutomationCurve` は**そのまま**。
`common/src/automation.rs` の `apply_curve` / `eval_bezier` の**数式もそのまま**。

したがって:
- `common/src/model.rs` の `CURRENT_VERSION` は **bump しない**。
- `common/src/project.rs` の `VALUE_MIGRATIONS` / `SONG_MIGRATIONS` に **1 行も足さない**。
- `common/build.rs` の `WIRE_SOURCES` は **変更不要**。wire を渡る型を新ファイルへ切り出さないし、
  `WIRE_SOURCES` には `src/model/automation.rs` は載っているが `src/automation.rs` は載っていない
  ので、§4.1 の述語追加も `thin_collinear_and_insert` の signature 変更も fingerprint を動かさない。
  `AppEvent` は `daw_gui` 内部型で IPC を渡らない。
- `daw_gui/src/clipboard.rs` は **変更不要** — `CopiedPoint { time_beat, value_norm, curve:
  AutomationCurve }` (`clipboard.rs:44-50`) が OS クリップボードへ serde JSON で書かれるが、
  `AutomationCurve` の variant 名も形も変わらないので互換が保たれる。
  `CLIPBOARD_MAGIC` (`clipboard.rs:22`) の bump も不要。**確認済みなので、迷って触らないこと。**
- **子 exe (`daw_audio` / `daw_plugin_host`) の再ビルドは不要**だが、§6 では確認のために
  `cargo build --workspace` を 1 度通す。

#### 「保存形式が変わらない」の正確な意味 — **スキーマは不変、出力キーは 1 種類増える**

§4.1 で `thin_collinear_and_insert` が id を採番するようになると、
**オートメーション録音した content の JSON に今まで出ていなかったキーが出る**。
`AutomationPoint::id` は `#[serde(default, skip_serializing_if = "is_zero_u32")]`
(`common/src/model/automation.rs:179-180`)、`AutomationContent::next_point_id` も同じ
(`:240-241`) なので、**0 のときだけ書かれていなかった**からである。

これは互換性の問題にならない。根拠:

- **スキーマ (型 / フィールド名 / 意味) は 1 つも変わらない。** 両フィールドとも
  `#[serde(default)]` なので、新しい save を旧コードで読んでも既存 field として復元される。
  よって `CURRENT_VERSION` の bump も migration も**不要** (上のリストのとおり)。
- **dirty-on-open 契約 (r.md #9) に影響しない。** `ClipContent::ensure_element_ids`
  (`common/src/model/content.rs:281-318`) は「`id == 0` のときだけ採番、非 0 なら
  `next_point_id` を bump するだけ」なので **冪等**。録音時に採番済の content を
  保存 → 再読込しても id は 1 つも動かず、`next_point_id` も既に最大 id + 1 なので変わらない。
  つまり load 直後の Song は保存時と bit 単位で同じ = `*` は付かない。
- 逆に**修正前**の挙動 (録音点が全部 `id: 0`) では、保存 → 再読込のときに
  `ensure_element_ids` が初めて採番するので、**開いただけで内容が変わっていた**。
  §4.1 はその非対称も同時に消す。

### 3.2 UI の語彙

| UI 表示 | モデル |
|---|---|
| 階段 | `AutomationCurve::Hold` |
| 直線 | `AutomationCurve::Linear` |
| 曲線 | `AutomationCurve::Exponential { bend }` |
| S 字 | `AutomationCurve::Bezier { tension }` |

内部の型名 (`Bezier` / `Exponential`) は変えない。UI から実装都合の名前を消すだけ。

### 3.3 数式の SSoT — どの空間で評価するか

#### 結論: **plain (再生値) 空間で評価し、サンプルごとに y へ写す。**

根拠:

1. **再生は plain 空間で起きる。** `evaluate_clip` (`common/src/automation.rs:215`) は
   `apply_curve(prev.value, next.value, u, next.curve)` を呼ぶ。`AutomationPoint::value` は
   plain (target のネイティブ単位)。RT 経路は `lane_value_at` → `evaluate_clip` → `apply_curve`。
2. **`mod.rs:427-428` が既に「描画と再生の数値完全一致を保証」と宣言している。**
   plain で評価すれば全 target でそれが真になる。
3. **`apply_curve` は `a` / `b` に対して affine 同変。** Hold / Linear / Exponential は
   `a + (b-a)·g(u)` の形、Bezier も `diag1` / `diag2` / `c1y` / `c2y` / Bernstein 和がすべて
   `(a, b)` の 1 次式なので、任意の affine 写像 φ に対し
   `apply_curve(φ(a), φ(b), u, c) = φ(apply_curve(a, b, u, c))`。

#### 描画が変わるのはどこか (3 つのクラス。**「affine なら 1px も変わらない」は嘘**)

`plain_to_norm_ranged` は末尾で **`v.clamp(0.0, 1.0)`** している
(`common/src/automation.rs:111`)。したがって「affine」は **表示窓 (norm 0..=1) の内側でだけ**
成り立つ。旧版の本計画は §3.3 / §8.2 で「affine な target では見た目は 1px も変わらない」と
書いていたが、これは **偽**。正しくは:

| クラス | target | plain 評価にすると |
|---|---|---|
| (a) 全単射 affine | `TrackBuiltin::Volume` / `Pan` / `SendGain`、`SongTempo`、`SongTimeSigNumerator`、`ImageBuiltin` 全部、`TextBuiltin::FontSize` / `Rotation`、`GroupTransform::Rotation`、`PluginParam` (range を持つとき) | **本当に 1px も変わらない** (端点が窓の内側にしか存在しない写像なので clamp が発火しない) |
| (b) 非 affine | `GroupTransform::ScaleX` / `ScaleY` (log)、`TrackBuiltin::Mute` (0.5 閾値の階段) | 形が変わる。**これが修正** (`Linear` 区間が log 曲線として描かれる、Mute が段で描かれる) |
| (c) 恒等 + clamp 飽和 | `GroupTransform::X` / `Y` / `AnchorX` / `AnchorY`、`TextBuiltin::X` / `Y` / `W` / `H` / 各色 / `OutlineWidth` / `ShadowOffsetX` / `ShadowOffsetY` / `ShadowBlur`、`PluginParam` (range が無いとき) | **端点が窓の外に出ている区間だけ**形が変わる。窓の中に収まっている区間は (a) と同じで不変 |

(c) が起きる根拠はコード自身のコメント (`common/src/automation.rs:104-109`):
「X/Y は『アンカー基準オフセット』で負 / >1 を取りうる … 下の clamp(0,1) で base が [0,1] 外だと
base_norm が端に飽和し」。`OutlineWidth` / `Shadow*` は px 単位の恒等写像なので 1 を軽く超える
(`common/src/model/automation.rs:100-103`)。

(c) でも **plain 評価のほうが正しい**: 例えば a=-0.5, b=0.5 の `Linear` 区間は、
実際の値は前半ずっと窓の外 (画面下端) にいて後半で立ち上がる。旧実装は
「端の飽和値どうしを直線で結ぶ」ので、窓の中央で値が 0.25 にあるかのように描いていた。
**plain 評価にすると『実際に鳴っている値が窓のどこにいるか』を描く。**

#### 窓の外に出た端点を widget が知るために — `value_plain` を運ぶ

`ArrangementAutomationPoint.value_norm` は `plain_to_norm_ranged` の結果なので **clamp 済**。
そこから `norm_to_plain` で戻しても真の plain は復元できない (a=-0.5 → norm 0 → plain 0.0)。
よって **widget の point に plain 値をそのまま持たせる** (§4.2)。

- `value_plain: f64` … 曲線の評価 / hit-test / 逆算はすべてこれを使う。**曲線の真実。**
- `value_norm: f32` … 点 dot の y、drag の delta、選択矩形。**画面への射影 (clamp 済)。**

2 つは `view_build.rs` の同じ 1 か所で同じ model の点から作られる (片方から片方を再変換しない)。
`value_norm` は `LaneValueMap::to_norm(value_plain)` と常に一致する。

#### 追加する 2 つの述語 (common)

`common/src/automation.rs` の `plain_to_norm_ranged` の直後に置く (この写像の性質なので同居させる):

```rust
/// `plain_to_norm_ranged` が **表示窓の内側で affine** (`α·plain + β` の 1 次式) か。
///
/// 窓の内側なら「plain で `apply_curve` を評価してから norm へ写す」のと
/// 「norm 値どうしを直接補間する」のは恒等に一致する (`apply_curve` は a / b に
/// 対して affine 同変)。 **末尾の `clamp(0.0, 1.0)` は含まない** — 端点が窓の外に
/// 出ている区間では写像が端で飽和して 1 次でなくなる。 描画側でこの述語を使うときは
/// 端点が窓の内側であることを別途確かめること
/// (`daw_gui/.../curve.rs::segment_is_straight_on_screen`)。
///
/// 窓の内側でも非 affine なのは `GroupTransform::ScaleX` / `ScaleY` (log 空間) と
/// `TrackBuiltin::Mute` (0.5 閾値の階段) の 2 つだけ。
#[must_use]
pub fn norm_mapping_is_affine(target: &AutomationTarget) -> bool {
    !matches!(
        target,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute)
            | AutomationTarget::GroupTransform(
                GroupTransformParam::ScaleX | GroupTransformParam::ScaleY
            )
    )
}

/// `plain_to_norm_ranged` が **狭義単調 (= 逆写像 `norm_to_plain_ranged` を持つ)** か。
///
/// 階段の `TrackBuiltin::Mute` だけが false。 画面上の点を掴んで値を逆算する
/// 直接操作 (r.md #73 の Alt+ドラッグ) は、これが true の lane でしか成立しない
/// (Mute lane の曲線は必ず 0 / 1 の段なので、指に追従させる連続解が無い)。
#[must_use]
pub fn norm_mapping_is_invertible(target: &AutomationTarget) -> bool {
    !matches!(target, AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute))
}
```

`apply_curve` の doc に 1 段落足す:

```rust
/// **この関数が曲線の形の唯一の実装** (r.md #73)。 再生 (daw_audio) も
/// arrangement widget の描画も hit-test も逆算も、全部ここを呼ぶ。
/// 画面座標で式を書き直さないこと — 「評価と描画は一致しているのに
/// ハンドルの向きだけ逆」 という #73 の不具合はそれが原因だった。
///
/// `a` / `b` に対して **affine 同変**: 任意の 1 次写像 φ について
/// `apply_curve(φ(a), φ(b), u, c) == φ(apply_curve(a, b, u, c))`。
/// ただし `plain_to_norm_ranged` は末尾で `clamp(0, 1)` するので、
/// **端点が表示窓の外にある区間では norm 空間の補間と一致しない**
/// (plain 側が真、norm 側は飽和した嘘)。
```

### 3.4 Alt+ドラッグの逆算 (掴んだ場所が指に付いてくる)

感度定数 (`2.0 / lane_height`) は捨てる。**掴んだ x での曲線値がカーソル値に一致するよう
パラメータを逆算**する。

記号:
- `u0 ∈ (0, 1)` — press 時に掴んだ位置の区間内進捗。**press 時に確定して drag 中は不変**
  (横スクロール / ズームしても掴んだ場所が動かない)。
- `a`, `b` — 区間の始点値 / 終点値 (**plain**、= `value_plain`)。drag 中は model 不変なので anchor 固定。
- `start_curve` — press 時に「これから量を付ける対象」となる curve。
  `Hold` / `Linear` なら `Exponential { bend: 0.0 }` (= 直線)、それ以外は press 時の curve そのもの。
- `dy = cursor_y - anchor_mouse_y`、`clip_h` = press 時の clip 描画域の高さ。

手順:

```text
v_anchor_norm = to_norm(apply_curve(a, b, u0, start_curve))   // press 時に確定、drag 中不変
v_target_norm = clamp01(v_anchor_norm - dy / clip_h)          // 指が動いた px ぶんだけ動かす
v_target_plain = to_plain(v_target_norm)
w = ((v_target_plain - a) / (b - a)).clamp(1e-6, 1.0 - 1e-6)  // 区間の符号付き高さで割る
```

- **曲線 (`Exponential`)**: `g(u) = u^k`、`k = 2^bend`。
  `k = ln(w) / ln(u0)` → `bend = k.log2().clamp(-1.0, 1.0)`。
  `u0` は `(1e-4, 1-1e-4)` に clamp してから使う (`ln(u0) = 0` を避ける)。
- **S 字 (`Bezier`)**: 正規化形状関数は
  `g(t) = t + tension·D` (tension ≥ 0)、`g(t) = t + 2·tension·D` (tension < 0)、
  ただし `D = t(1-t)(2t-1)`。
  (`eval_bezier` から導出して一致を確認済: a=0,b=1 で tension≥0 なら制御点の対角線からの
  ずれが `∓τ/3` なので `Δy = τ·t(1-t)(2t-1)`、tension<0 なら `±2|τ|/3` で `Δy = 2τ·D`。)
  よって `Δ = w - u0`、`D = u0(1-u0)(2u0-1)` として
  `t0 = Δ / D` を作り、`tension = if t0 >= 0.0 { t0 } else { Δ / (2.0 * D) }` を
  `[-1.0, 1.0]` に clamp。
  `|D| < 1e-6` (= 掴んだ場所が区間の中点 u0=0.5 か端) のときは **直前の preview を維持**して
  何もしない — S 字は u=0.5 を必ず通る (数学的な固定点) ので、そこを掴んでも曲線は動かせない。

**符号が自動的に正しくなる理由**: `w` は `(b - a)` で割っている。上り区間 (b>a) でカーソルを
上げると `v_target` が大きくなり `w > u0` → `k < 1` → `bend < 0`。下り区間 (b<a) で同じく
上げると `w < u0` → `k > 1` → `bend > 0`。**同じ画面ジェスチャが区間の向きに応じて逆符号の
progress 値を生む** = 2.1 で調べた業界標準の挙動そのもの。

#### 「指に付いてくる」が成り立たない 3 つの例外 (すべて数学的性質。隠さず書く)

1. **Hold 区間は press 直後に 1 度だけジャンプする。**
   Hold の描画は `u<1` で水平線 (値 = a) なので hit-test は水平線に当たる。しかし
   `start_curve` は `Exponential { bend: 0.0 }` (= 対角線) で、`anchor_value_norm` は
   その対角線上の `u0` の値。よって最初の 1px で線が水平線から対角線へ飛ぶ。
   **連続な解は存在しない** — `k ∈ [0.5, 2]` のどの `Exponential` も `u0<1` で値 `a` を通らない
   (通るには `k → ∞`)。「Hold を掴んだら直線化してから曲がる」は確定方針そのものなので、
   ジャンプは仕様。ジャンプ後は指に付いてくる。
2. **到達可能な範囲 (飽和) がある。** `bend` / `tension` を `[-1, 1]` に clamp する
   (= `AutomationCurve` 自身が宣言する値域、`common/src/model/automation.rs:159-169`) ので:
   - `Exponential`: `k ∈ [0.5, 2]` なので `u0` で到達できる `w` は **`[u0², √u0]` だけ**。
     区間の右端に近い `u0` ほど帯が狭い (u0=0.9 なら w ∈ [0.81, 0.949] = 区間高さの約 14%)。
   - `Bezier`: `Δ = w - u0` が `D>0` のとき `[-2D, D]`、`D<0` のとき `[D, -2D]` だけ。
     `|D|` は最大でも 0.0962 (u0 = (3±√3)/6) なので、S 字で動かせる幅は元から小さい。
   到達不能な目標を渡されたら clamp された端の値になる = **線がそこで止まって指から離れる**。
   これは「その曲線はそれ以上曲がれない」という真実の表示であって、破綻ではない。
   (NaN / 符号反転は起きない: `w` は `(1e-6, 1-1e-6)` に clamp 済なので
   `ln(w)` は有限負、`u0` も端を避けているので `k` は有限正。)
3. **表示窓の外に端点がある区間 ((c) クラス)** では、`w` の分母 `(b-a)` が真の plain 差で、
   分子は窓の中の値なので、`w` が `[0,1]` の外へ出て clamp される頻度が上がる = 2 と同じ飽和。
   窓の中に見えている部分は正しく追従する。

**曲げられない区間**: `a ≈ b` (= 水平区間) は `w` が定義できない。`Mute` lane は逆写像が無い。
どちらも**press 時に session を起動しない** (§3.5 の hit-test で除外する)。

### 3.5 区間の当たり判定と press の優先順位

新しい hit-test `automation_segment_at` (`geometry.rs`) を足す。

判定:
1. `lanes.contains(cx, cy)` を満たす lane body 内であること。
2. lane の `target` が `norm_mapping_is_invertible` を満たすこと (Mute lane は対象外)。
3. clip 内の各区間 `points[i-1] -> points[i]` (i >= 1) について
   `x_prev < cx < x_next` かつ `x_next - x_prev > 1e-3` であること。
4. 区間の端点の screen y の差が **1.0 px 以上**あること (= 水平区間は曲げられないので除外)。
   これは端点が両方とも窓の外で同じ端に飽和している区間 ((c) クラス) も自動で弾く。
5. `points[i].id != 0` であること (= 安定 id で指せない点は対象外。§3.7 の修正後は起きないが、
   古いセッション由来の点が残っている可能性があるので防御する)。
6. `u = (cx - x_prev) / (x_next - x_prev)` で `curve::eval_norm` を評価して y を出し、
   `|cy - y| <= style.automation_curve_segment_hit_px` (既定 6.0) なら hit。
7. 複数 hit したら **cy に近い方**を採用 (通常は lane が y で disjoint なので 1 つ)。

**press の優先順位 (既存の並びから 1 段消して 1 段足すだけ)**:

```
splitter (lane 下端 / 行下端 / header 境界)
  → audio grip / clip drag (track row)
  → automation point                              ← Alt なら即削除 (温存)
  → [新] Alt + automation segment (6px)           ← 区間 bend
  → automation clip (Move / Resize、Alt = スナップ無効)
  → lasso (真の空き zone)
```

**ガードは「点の当たり」と「残存 point session」の OR に一本化する。**
旧版の本計画は press スニペットで `point_claimed` という**存在しない**ローカル変数を使っていた。
実在するのは `already_taken_by_point` (= `state.automation_point_drag.is_some()`、`run.rs:762-765`)
だが、**これでは足りない** — Alt+クリック (点) では point drag session は立たず
`press_delete_point` (`run.rs:686`) だけが立つので、`already_taken_by_point` を見ると
**Alt+クリックで「点の削除」と「bend session の起動」が同フレームで両方走る。**

正しい形: `automation_point_at` の結果を **press ブロックの手前で 1 度だけ**評価して
`point_hit: Option<(AutomationPointKey, Rect)>` に持ち、`already_taken_by_point` と OR する:

```rust
// r.md #73: 「point 層がこの押下を消費した」の唯一の述語。
// - `point_hit.is_some()`        … 今フレームの当たり (Alt+削除 / drag 起動の両方を含む)
// - `already_taken_by_point`     … 前フレームから残存する point drag session (防御。§3.5 の注)
let point_consumed = point_hit.is_some() || already_taken_by_point;
```

- 既存の point press ブロック … `if let Some((point_key, _)) = point_hit`
  (`automation_point_at` の 2 度呼びを残さない)
- 新しい bend press ブロック … `!point_consumed` を要求
- automation clip press ブロック … `!already_taken_by_point` を **`!point_consumed` に置換**
  (Alt+クリック時の取りこぼしも同時に閉じる)

**`already_taken_by_point` を OR に残す理由 (捨てない)**: これは #77 の
`PressClaim::from_live` が `point: automation_point_drag.is_some()` として seed している値そのもの
(`docs/plan_rmd_77_arrangement_split.md` §6-B:507 / :535-536)。#77 はこの seed を
「旧実装が各ゲートで `widget_state` を読み直していたのと**厳密に等価**」であることの根拠に
している (同 §6-B:517-521)。#73 がこれを「今フレームの hit」で**置き換えて**しまうと、
#77 の等価性トランスクリプトがその 1 行で壊れる。OR にすれば

- seed は据え置き = #77 の等価性はそのまま成立、
- 新しく増えるのは「今フレーム点に当たった」ケースだけ = 単調に強くなる方向

なので、どちらの契約も壊れない。**#77 後の形**: `PressClaim::from_live` は**変更しない**。
`press_lanes::point` が point に当たったら `claim.point = true` を立てる
(Alt+削除の arm でも立てる — 現行実装が drag session の有無でしか立たないのが穴だった)。
以降 `press_lanes::{automation_clip, /* 新 */ segment_bend}` は `claim.point` を読む。
つまり **#73 が変えるのは `point` を「立てる条件」であって「seed」ではない。**

### 3.6 Alt の再配分

- **撤去**: `run.rs:835-919` (post-#77 は `press_lanes::alt_resize`) の Alt+ドラッグ =
  レーン / 行の高さ変更。ブロックごと削除する (**834 は空行、921 は lasso のコメント。
  巻き込まないこと**)。
  ヘッダ列からの高さ変更は **Alt+ホイール** (`release.rs:989-1073`) が
  ヘッダ列を含む `content_below_ruler` で効くので失われない (§2.4)。
  **下端スプリッタの当たり判定は広げない。**
- **温存**: Alt+クリック (点) = 点の削除。
- **温存**: Alt = スナップ無効 (時間軸を動かす drag)。曲がり具合に時間量子化は無いので衝突しない。
- **新規**: Alt+ドラッグ (レーン本体の線の上) = その区間を曲げる。
- **新規**: Alt+ダブルクリック (線の上 6px 以内) = その区間を直線に戻す。
  線から離れていれば従来どおり「スナップ無しで点を追加」。
- **Alt+Shift は使わない。**

#### Alt+ドラッグの死角を作らない — 「Alt を予約」していた 3 つのゲートを外す

Alt+drag resize を消すと、**その予約のために Alt を弾いていたゲート**が根拠を失う。
残すと「Alt を押してドラッグすると何も起きない」領域が新しく生まれる (現状は resize が起きていた)。
コメント自身が予約の事実を書いているので、resize と一緒に外す:

| 場所 | 現在のゲート | コメントが書いている理由 | #73 後 |
|---|---|---|---|
| `run.rs:778` automation clip press | `&& !pointer.modifiers.alt` | `run.rs:766-772`「Alt 修飾は **lane Alt+drag for resize に予約** する … 既存 automation clip Alt-snap-off 機能は失われるが」 | **外す** → automation clip の Alt = スナップ無効が復活し、MIDI / audio clip と対称になる |
| `run.rs:931` lasso press | `&& !pointer.modifiers.alt` | `run.rs:926-927`「Alt は lane resize に予約済 (上の Alt+drag fallback で先勝) なので `!pointer.modifiers.alt` で除外」 | **外す** → 空き lane zone の Alt+drag が lasso になる |
| `release.rs:826` marquee press | `&& !pointer.modifiers.alt` | (コメント無し。空き track row zone でのみ起動し、そこは Alt+drag row resize が取っていた) | **外す** → 空き track row の Alt+drag が marquee になる |

外すことで新たな衝突が起きないことの確認:
- automation clip press は `!point_consumed` (§3.5) と「bend session が起動していない」を
  要求する (§4.7)。よって Alt+drag が線の上なら bend が先勝し、線から離れていれば clip の
  スナップ無し move になる。
- lasso と marquee の `no_session` 列挙に `automation_segment_bend.is_none()` を入れる
  (§4.7 / §4.12)。**「外す」だけやって列挙を直し忘れると、線の上の Alt+drag で bend と
  lasso が同時に起動する。**
- lasso / marquee には snap の概念が無いので Alt に別の意味は無い。
- track header 列の Alt+drag は track reorder が拾う (reorder の press は modifier を見ない、
  `run.rs:535-557`)。よって無反応にはならない。

#### ヘッダ列の扱い (**「元から何も起こさない」は誤りだったので訂正する**)

`run.rs:841` の `let in_arr = in_lanes || (header_w > 0.0 && header_pane.contains(px, py));`
により、**現状は lane header 列 / track header 列の Alt+drag も lane / row resize を起こしている。**
旧版の本計画は「lane header 列の drag は元から何も起こさない」と書いていたが、
それは**無修飾 drag の話**であって Alt+drag には当てはまらない。実測での訂正:

| 列 | 無修飾 drag (現状) | Alt+drag (現状) | Alt+drag (#73 後) |
|---|---|---|---|
| track header 列 | track reorder (`run.rs:535-557`) | **row resize** (`in_arr` 経由) | track reorder (modifier を見ないので継続) |
| lane header 列 | 何も起こさない (lane header の press は ★/👁/✕ ボタンと disclosure のクリックのみ) | **lane resize** (`in_arr` 経由) | **何も起こさない** |
| lanes (レーン本体) | point / clip / lasso | lane resize | bend / clip スナップ無効 / lasso (§3.6 の表) |

**lane header 列の Alt+drag が無反応になるのは死角ではない。** §3.6 が守っている不変条件は
「Alt+drag が**無修飾 drag で何かが起きる場所**で無反応にならないこと」であって、
lane header 列は無修飾 drag でも元から何も起きない = Alt の有無で挙動が変わらない、で一貫する。
高さ変更の手段は Alt+ホイール (ヘッダ列でも効く、§2.4) とレーン下端スプリッタ
(`run.rs:152-230`) の 2 つが残るので、機能としても失われない。
**実機確認 §6-7 はこの列も対象に含めること** (「lane header 列の Alt+drag で高さが変わらない」
「同じ場所で Alt+ホイールなら変わる」を両方見る)。
`marquee_zone_ok` は `lanes.contains(px, py)` を要求する (`release.rs:825-840`) ので、
ヘッダ列に marquee が漏れて出ることも無い。

`AutomationLaneResizeDragSession` / `TrackRowResizeDragSession` の型と
`automation_lane_resize_drag` / `track_row_resize_drag` の state field は
**下端スプリッタ経路 (`run.rs:152-230`) がまだ使うので残す**。消すのは Alt+drag の起動側だけ。

### 3.7 安定 id addressing (不変条件 1)

`AutomationPointKey.point_idx: u32` (`common/src/model/automation.rs:479-482`) は配列 index で、
点の追加 / 削除でずれる。曲線編集は press → release を跨ぐ (= 途中で別の Edit が走り得る) ので、
**`SetAutomationCurve` 経路だけは `AutomationPoint::id` (v29 の安定 id) で指す。**

前提を成立させるために **2 つ**やる:

1. **`thin_collinear_and_insert` が id を採番するようにする** (§2.7 の穴)。
   引数を `&mut Vec<AutomationPoint>` から **`&mut AutomationContent`** に変え、
   `content.alloc_point_id()` で採番してから挿入する。
   「点を作る唯一の共有関数が id を必ず振る」形にすれば、呼び出し側が忘れられない
   (呼び出し側で `points[insert_at].id = ...` を後付けする形は、次の呼び出し側が忘れる)。
2. **`point_id == 0` を no-op として弾く**。`set_automation_curve` handler と
   `automation_segment_at` の両方で。古いセッション由来の未採番点を掴んで
   「別の点が曲がる」を起こさない。

安定 id が全点に存在することの確認 (上の 1 の後):
- `common/src/model/content.rs:281-315` の `ensure_element_ids` が load 時に全 automation point へ採番する
  (呼ぶのは `common/src/model.rs:1747` の `Song::ensure_ids`)。
- 実行時の生成経路は `handler/automation.rs:317` / `:800` / `:1216`、`handler/media.rs:959`、
  そして修正後の `common/src/automation.rs::thin_collinear_and_insert` の 5 つで、すべて採番する。

**スコープ**: 本計画で id 化するのは新設する `AppEvent::SetAutomationCurve` と
widget 内の bend session のみ。`SelectAutomationPoints` / `MoveAutomationPoints` /
`DeleteAutomationPoints` などの既存経路は positional のまま残す (別件)。
`daw_gui/src/app_types.rs` の `AutomationPointKeyRef` は**触らない**。
残る positional 依存は §8 の open risk に記載する。

---

## 4. ファイル別の変更

以下、行番号は**現在 (#77 前) のもの**。#77 後は §0 の対応表でファイルを読み替える。

### 4.1 `common/src/automation.rs`

- `plain_to_norm_ranged` (doc 41-47 + fn 48-112、末尾の `clamp` は 111) の直後に
  `norm_mapping_is_affine` / `norm_mapping_is_invertible` を追加 (§3.3 のコード全文)。
  次の item は `norm_to_plain` (114-118) なので、そこを押し下げる形で挿す。
- `apply_curve` (doc 218-243 + fn 244-259) の doc に §3.3 の 1 段落を追加。
  **数式は 1 文字も変えない。**
- `thin_collinear_and_insert` (458-488、**doc コメントは現状ゼロ**) の signature を変える (§3.7):

  ```rust
  /// r.md #73: **点を作る唯一の共有経路なので、ここで安定 id を採番する。**
  /// 旧実装は `id: 0` (未採番 sentinel) を挿していた。 v29 の id はセッション中に
  /// 採番されず、保存 → 再読込の `Song::ensure_ids` まで 0 のままだったので、
  /// 「オートメーション録音した点は全部 id 0」 という状態が実在した
  /// (`daw_gui/src/handler/tick.rs::insert_recording_point` が唯一の caller)。
  /// #73 の曲線編集は point を安定 id で指すため、この穴があると別の点が曲がる。
  ///
  /// `&mut AutomationContent` を取るのは `alloc_point_id()` を呼ぶため
  /// (呼び出し側で後付けする形にすると次の caller が忘れる)。
  pub fn thin_collinear_and_insert(
      content: &mut crate::model::AutomationContent,
      time_beat: f64,
      plain_value: f64,
      epsilon: f64,
  ) -> ThinInsertResult {
      let id = content.alloc_point_id();
      let points = &mut content.points;
      // …以降の thinning / insert ロジックは 1 行も変えない。挿す点だけ `id` を入れる。
  }
  ```

- test module の更新 / 追加:
  - **更新**: `thin_collinear_and_insert` の呼び出し 9 か所
    (1016 / 1035 / 1041 / 1066 / 1070 / 1082 / 1098 / 1106 / 1118) を
    `AutomationContent { points, next_point_id: 1 }` を組む形に書き換える。
    **assert も機械的に書き換わる** — 既存テストは `pts` (= `Vec<AutomationPoint>`) を
    直接見ているので、`content.points` 越しの参照になる。実測で書き換えが必要な行:
    `pts.len()` (1020 / 1067 / 1071 / 1084 / 1099 / 1107 / 1119)、
    `pts[0]` / `pts[1]` (1021-1024 / 1085-1086)、
    `pts.iter().any(..)` (1053)、`pts.last()` (1057)、
    および `{pts:?}` を含む assert メッセージ (1020 / 1049 / 1054 / 1084)。
    5 本のテスト (`thin_linear_drag_collapses_to_endpoints` 1010-1025 /
    `thin_v_shape_drag_keeps_inflection_point` 1029-1060 /
    `thin_skips_when_fewer_than_two_points` 1063-1073 /
    `thin_constant_value_collapses_to_two_points` 1077-1087 /
    `thin_epsilon_boundary` 1091-1108 / `thin_skips_when_dt_zero` 1112-1123) が対象。
    **変えてよいのは「どこから points を借りるか」だけで、期待値 (件数 / 時刻 / 値 /
    `insert_at` / `removed_prev`) は 1 つも変えない** — thinning の挙動は不変だから。
    期待値を触りたくなったら、それは signature 変更が挙動を変えてしまった合図。
  - **新規** `thin_insert_allocates_stable_ids` — 3 回続けて挿し、
    3 点の `id` が全部非 0 かつ相異なることを assert。
  - **新規** `norm_mapping_predicates_cover_every_target` — `AutomationTarget` の全 variant を列挙して
    2 述語が期待どおりであることを assert (新 target を足したとき落ちるようにする)。
  - **新規** `apply_curve_is_affine_equivariant` — 適当な φ (α<0 を含む) で
    `apply_curve(φ(a), φ(b), u, c) ≈ φ(apply_curve(a, b, u, c))` を 4 種すべてで確認
    (= 描画を norm 空間に移しても (a) クラスの target では変わらないことの根拠を固定する)。
  - **新規** `plain_to_norm_saturates_outside_the_window` — `GroupTransform::X` で
    plain = -0.5 / 1.5 が norm 0 / 1 に飽和し、`norm_to_plain` が元に戻らないことを assert
    (= §3.3 (c) クラスが実在することをテストで固定し、「affine だから不変」と再び書かせない)。

### 4.2 `daw_gui/src/widgets/arrangement/mod.rs`

**削除**
- `ArrangementCurveKind` (**431-437**、doc 411-430) — mirror 型。不変条件 8 の回復。
- `SetAutomationCurveParamKind` (443-446) と その doc (439-442)。
- `AutomationCurveParamDragSession` (1808-1835) と
  `ArrangementState::automation_curve_param_drag` (1930-1936)。
- style の `automation_curve_param_handle_radius_px` (1143) /
  `automation_curve_param_handle_fill` (1146) / `automation_curve_param_handle_border` (1149) /
  `automation_curve_param_handle_offset_px` (1152) と、その `Default` 実装
  (1382-1385)。

**変更**
- `ArrangementAutomationPoint` (448-455):

  ```rust
  /// automation point の clip 窓ローカル座標 + 補間形状。
  ///
  /// r.md #73: **`value_plain` が曲線の真実、`value_norm` は画面への射影**。
  /// `plain_to_norm_ranged` は末尾で `clamp(0, 1)` するので、窓の外に出る値
  /// (`GroupTransform::X` / `TextBuiltin::OutlineWidth` 等) は norm から
  /// 復元できない。曲線の評価 / hit-test / 逆算は `value_plain` を使い、
  /// 点 dot の y / drag の delta / 選択矩形は `value_norm` を使う。
  /// 2 つは `view_build.rs` の同じ場所で同じ model 点から作る (再変換しない)。
  #[derive(Clone, Copy, Debug)]
  pub struct ArrangementAutomationPoint {
      /// r.md #73: `common::model::AutomationPoint::id` (per-content 安定 id)。
      /// 曲線編集は press → release を跨ぐので positional index では指せない
      /// (不変条件 1)。`0` は未採番 sentinel = 曲線編集の対象外。
      pub id: u32,
      pub time_beat: f64,
      /// `0.0..=1.0` 正規化 (clamp 済)。
      pub value_norm: f32,
      /// target のネイティブ単位。`common::automation::apply_curve` に渡す値。
      pub value_plain: f64,
      /// r.md #73: mirror 型 `ArrangementCurveKind` を廃止して model 直結
      /// (不変条件 8)。
      pub curve: common::model::AutomationCurve,
  }
  ```

- `ArrangementAutomationLane` (**475-493**) に 2 フィールド追加。
  **型の doc (473-474) も書き換えること** — 現状は

  > 「automation lane (**target を持たず**、widget は label / icon の表示しか扱わない)。
  > caller (daw_01) が `target` (Track の volume/pan、plugin parameter 等) を別途保持する。」

  と書いてあり、`target` を足すとこの doc が**そのまま嘘になる**。新しい doc:

  ```rust
  /// M14 Phase 63n-1 (#028): automation lane。
  ///
  /// r.md #73: **`target` / `plugin_range` を widget が持つようになった** (旧 doc の
  /// 「target を持たず label / icon の表示しか扱わない」は撤回)。曲線の形を
  /// **再生と同じ plain 空間**で評価する (`common::automation::apply_curve` が唯一の
  /// 実装) ために、値 ↔ 画面 y の写像を widget 側で解決する必要があるため。
  /// この 2 つは `fold_arrangement_clip_hash` の cache key にも入る (曲線の形が
  /// 依存するので、入れないと plugin param の range が後から埋まったときに古い形が残る)。
  ```

  追加するフィールド:

  ```rust
  /// r.md #73: 曲線を **再生と同じ plain 空間**で評価するために必要
  /// (`common::automation::{plain_to_norm_ranged, norm_to_plain_ranged}`)。
  pub target: common::model::AutomationTarget,
  /// `PluginParam` の実 min/max (caller の plugin_params cache 由来、
  /// 非 PluginParam は `None`)。`plain_to_norm_ranged` にそのまま渡す。
  pub plugin_range: Option<(f64, f64)>,
  ```

  `AutomationTarget` は `Clone` だが `Copy` ではないので、`ArrangementAutomationLane` の
  `#[derive(Clone, Debug)]` はそのままでよい。

- `ArrangementResponse` (781-887) に 1 フィールド追加:

  ```rust
  /// r.md #73: Alt 押下中にポインタが乗っている「曲げられる区間」。
  /// `hovered_clip` と同じ毎フレーム算出の hover state。
  /// **caller はこれを heavy cache キーに混ぜないこと** (マウス移動のたびに
  /// アレンジ全体が再構築される)。強調描画は overlay 層で行う。
  pub hovered_automation_segment: Option<AutomationPointIdKey>,
  ```

  **`impl Default for ArrangementResponse` (890-922) にも 1 行足すこと**
  (`automation_point_drag: None` の隣、919 行目付近)。
  `run.rs:62` が `ArrangementResponse { ruler_rect: ruler, ..Default::default() }` なので
  Default を更新しないとコンパイルが通らない。

  **この値は heavy 層へ明示的に渡す必要がある** (§4.10 / §4.11)。
  `render_arrangement_heavy` は `response` を受け取らないので、`hovered_clip` と同じく
  `run.rs:2094` の `let hovered_clip_for_heavy: Option<ClipKey> = response.hovered_clip;` の
  隣に `let hovered_segment_for_heavy = response.hovered_automation_segment;` を作り、
  `run.rs:2096` の呼び出しに引数として並べる。**「response に足した」だけでは描画に届かない。**

- `fold_arrangement_clip_hash` (**2228-2401**、doc 2218-2227) を 3 点変更する:
  - point の match (**2381-2393**) を `common::model::AutomationCurve` に読み替える
    (variant 名は同じ)。
  - point ループ (**2376-2396**) に `h ^= p.value_plain.to_bits();` を足す
    (`p.value_norm` を混ぜている 2379-2380 の直後)。
  - **lane ループ (2343-2398) の scalar フィールド末尾 — `lane.label.len()` を混ぜる
    2363-2364 の直後、`for ac in &lane.clips` (2365) の直前 — に `target` / `plugin_range`
    を足す**:

    ```rust
    // r.md #73: 曲線の形は plain 空間で評価するので、同じ value_norm でも
    // target / plugin_range が違えば別の形になる。cache key に入れないと
    // plugin param の range が後から埋まったときに古い形が残る。
    h ^= daw_ui_core::hash_inputs(&lane.target);
    h = h.wrapping_mul(PRIME);
    h ^= daw_ui_core::hash_inputs(
        lane.plugin_range.map(|(lo, hi)| (lo.to_bits(), hi.to_bits())),
    );
    h = h.wrapping_mul(PRIME);
    ```

    (`AutomationTarget` は `#[derive(Debug, Clone, PartialEq, Eq, Hash, ...)]` 済 —
    `common/src/model/automation.rs:22`。`hash_inputs` は
    `ui/crates/ui/src/scenegraph.rs:101`、`mod.rs:29` で既に import 済み。)

- style に 3 フィールド追加 (`Default` 実装にも):

  ```rust
  /// r.md #73: レーン本体の線 (区間) の当たり判定半径 (px)。
  /// point dot (半径 2 倍 = 8px) より後に評価するので、点の上では点が勝つ。
  pub automation_curve_segment_hit_px: f32,          // = 6.0
  /// Alt hover 中に曲げられる区間を強調する色。
  pub automation_curve_bend_hover_color: Color,      // = p.selection_warm
  /// Alt+ドラッグ中の live preview 線の色 (旧 automation_curve_param_preview_color を改名)。
  pub automation_curve_bend_preview_color: Color,    // = p.selection_warm
  ```

  既存の `automation_curve_param_preview_color` (1157 / 1386) は上の
  `automation_curve_bend_preview_color` に **改名**する (削除ではない)。

**追加**

```rust
/// r.md #73: automation point の **安定 id** による addressing。
/// `AutomationPointKey` は `point_idx` (positional) なので、点の追加 / 削除で
/// 指す先が変わる (不変条件 1)。曲線編集は press → release を跨ぐので、
/// この経路だけは `common::model::AutomationPoint::id` で指す。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AutomationPointIdKey {
    pub clip: AutomationClipKey,
    /// `AutomationPoint::id` (per-content 安定 id、`0` は未採番 sentinel)。
    /// `0` はこの経路の対象外 (hit-test で弾く / handler で no-op)。
    pub point_id: u32,
}

/// r.md #73: レーン本体の線 (= 2 点の間の区間) を Alt+ドラッグして曲げる session。
///
/// 掴んだ場所が指に付いてくるよう、感度定数ではなく **逆算** で curve を決める
/// (`curve::solve_bend`)。逆算は区間の符号付き高さ `(b - a)` で割るので、
/// 上り区間 / 下り区間で自動的に正しい符号の progress 値になる。
/// commit は release で 1 回だけ (undo 1 段)。
#[derive(Clone, Copy, Debug)]
pub(super) struct AutomationSegmentBendSession {
    /// 曲げる区間の **入射側** point (= `curve` 属性を持つ後ろの点)。
    pub point: AutomationPointIdKey,
    /// press 時に掴んだ位置の区間内進捗 `u ∈ (0, 1)`。drag 中不変。
    pub grab_u: f64,
    /// 区間の始点値 / 終点値 (plain)。drag 中 model 不変なので anchor 固定。
    pub a_plain: f64,
    pub b_plain: f64,
    /// press 時の curve (= release の no-op 判定に使う anchor)。
    pub anchor_curve: common::model::AutomationCurve,
    /// 逆算の基準になる curve。`anchor_curve` が Hold / Linear なら
    /// `Exponential { bend: 0.0 }` (= 直線へ自動変換)、それ以外は `anchor_curve`。
    pub start_curve: common::model::AutomationCurve,
    /// press 時点の `apply_curve(a, b, grab_u, start_curve)` を norm に写した値。
    /// 指の移動 px をここに足して目標値を作る (= 変換直後の線から相対で追従)。
    /// **Hold 区間ではここで 1 度だけ線が飛ぶ** (§3.4 の例外 1、連続解が無い)。
    pub anchor_value_norm: f32,
    /// press 時点の clip 描画域 (norm ↔ y の anchor。view scroll 耐性)。
    pub clip_rect_anchor: Rect,
    pub anchor_mouse_y: f32,
    /// 直近の cursor y (release frame は anchor と異なるときだけ更新 — 既存 pattern)。
    pub last_mouse_y: f32,
    /// drag 中の live curve (overlay 描画 + release commit の SSoT)。
    /// `anchor_curve` と同値なら release で no-op。
    pub preview_curve: common::model::AutomationCurve,
}
```

`ArrangementState` に `automation_segment_bend: Option<AutomationSegmentBendSession>` を追加。

**モジュール宣言を 2 本足す** (`mod.rs`):

- `mod curve;` … 子モジュール宣言の並び (59-69) に。既存の `mod draw; use draw::*;` と
  同じ位置づけ (`use curve::*;` は**しない** — `curve::eval_norm` のように修飾して呼ぶ。
  同名の自由関数が draw / geometry と衝突しやすいため)。
- `#[cfg(test)] mod tests_curve;` … ファイル末尾の `#[cfg(test)] mod tests;` (2412-2413) の隣。
  §5.1 で新設する曲線テストの置き場 (god file budget の理由。§7)。

### 4.3 新規 `daw_gui/src/widgets/arrangement/curve.rs`

曲線 ↔ 画面の変換をここに集める。`mod.rs` の子モジュール宣言 (59-69) に
`mod curve;` を足し、既存の流儀どおり `use super::*;` で始める。

```rust
//! r.md #73: automation の曲線と画面の間の変換を 1 か所に集める。
//!
//! **曲線の形の実装は `common::automation::apply_curve` ただ 1 本**で、
//! ここはその評価結果を screen 座標へ写すだけ。以前は draw.rs (screen y) /
//! geometry.rs (handle 位置) / common (再生) の 3 か所に式が散っていて、
//! 「評価と描画は一致しているのにハンドルの向きだけ逆」という不具合を生んでいた。
//!
//! 評価は **plain (再生値) 空間**で行う。`apply_curve` は a / b に対して affine
//! 同変なので、表示窓の内側に収まる affine な target では norm 空間評価と一致する。
//! 一致しないのは 3 つ: `GroupTransform::ScaleX/ScaleY` (log)、`TrackBuiltin::Mute`
//! (階段)、そして **端点が表示窓の外に出る恒等 target** (`GroupTransform::X` 等 —
//! `plain_to_norm_ranged` 末尾の `clamp(0,1)` で飽和する)。そこでは plain 評価だけが
//! 「鳴る形が窓のどこにいるか」を描く。

use super::*;
use common::automation::{
    apply_curve, norm_mapping_is_affine, norm_mapping_is_invertible,
    norm_to_plain_ranged, plain_to_norm_ranged,
};
use common::model::AutomationCurve;

/// 1 レーンぶんの「値 ↔ 画面 y」写像。clip 描画域 (縦 padding 適用済) を含む。
#[derive(Clone, Copy)]
pub(super) struct LaneValueMap<'a> {
    pub target: &'a common::model::AutomationTarget,
    pub plugin_range: Option<(f64, f64)>,
    /// clip 描画域の上端 y と高さ (= lane body から縦 padding を引いたもの)。
    pub clip_y: f32,
    pub clip_h: f32,
}

impl LaneValueMap<'_> {
    /// `lane.target` / `lane.plugin_range` と clip 描画域から作る。
    #[must_use]
    pub(super) fn from_lane(lane: &ArrangementAutomationLane, clip_rect: Rect) -> LaneValueMap<'_>;

    /// norm (0..=1) → screen y。
    #[must_use] pub(super) fn norm_to_y(self, norm: f32) -> f32;
    /// screen y → norm (0..=1、clamp 済)。
    #[must_use] pub(super) fn y_to_norm(self, y: f32) -> f32;
    /// plain → screen y (`norm_to_y(to_norm(plain))`)。点 dot / 曲線の共通経路。
    #[must_use] pub(super) fn plain_to_y(self, plain: f64) -> f32;
    #[must_use] pub(super) fn to_plain(self, norm: f32) -> f64;
    #[must_use] pub(super) fn to_norm(self, plain: f64) -> f32;
    /// この lane で「線を掴んで曲げる」直接操作が成立するか
    /// (= `norm_mapping_is_invertible`)。
    #[must_use] pub(super) fn is_bendable(self) -> bool;
}

/// この plain 値が **表示窓の内側**にいるか (= `clamp(0,1)` で潰れていないか)。
///
/// `plain_to_norm_ranged` は末尾で `clamp(0.0, 1.0)` する
/// (`common/src/automation.rs:111`)。窓の外の値は端に飽和し、`norm_to_plain_ranged`
/// で戻らない。norm が端にいるときだけ round-trip を確かめる形にして、
/// f32 量子化を誤検出しないようにする (端でない値は定義上飽和していない)。
#[must_use]
fn plain_is_inside_window(map: LaneValueMap<'_>, plain: f64) -> bool {
    let n = map.to_norm(plain);
    if n > 0.0 && n < 1.0 {
        return true;
    }
    (map.to_plain(n) - plain).abs() <= 1e-6 * (1.0 + plain.abs())
}

/// この区間が **画面上でも 1 次**か (= 2 点の polyline で厳密に描けるか)。
/// `norm_mapping_is_affine` に加えて **端点が両方とも表示窓の内側**であることを要求する。
/// `Linear` の値域は端点の間に収まる (区間は凸) ので、端点が窓の中なら区間全体が中。
#[must_use]
pub(super) fn segment_is_straight_on_screen(
    map: LaneValueMap<'_>,
    a_plain: f64,
    b_plain: f64,
) -> bool {
    norm_mapping_is_affine(map.target)
        && plain_is_inside_window(map, a_plain)
        && plain_is_inside_window(map, b_plain)
}

/// 区間の任意進捗 `u` における **norm 値**。曲線の唯一の評価入口。
/// 描画 / hit-test / 逆算がすべてこれを通る。
#[must_use]
pub(super) fn eval_norm(
    map: LaneValueMap<'_>,
    a_plain: f64,
    b_plain: f64,
    u: f64,
    curve: AutomationCurve,
) -> f32 {
    map.to_norm(apply_curve(a_plain, b_plain, u, curve))
}

/// 1 区間 (前 point → 次 point) を polyline に flatten して `out` へ push。
/// caller は始点を 1 度 push 済の前提、終点 (= 次 point) を含めて push する。
///
/// - `Hold` は階段なので 2 点 (`(x1, y0)` → `(x1, y1)`) で厳密。
/// - `Linear` かつ `segment_is_straight_on_screen` なら 1 点 (= 終点) で厳密。
/// - それ以外は uniform sampling (`sample_count`)。旧実装の adaptive de Casteljau
///   は「y が制御点の 1 次式」を前提にしていて非 affine / 飽和する写像に持ち込めないので廃止。
pub(super) fn flatten_segment(
    map: LaneValueMap<'_>,
    prev: (f32, f64),          // (screen x, plain value)
    next: (f32, f64),          // (screen x, plain value)
    curve: AutomationCurve,
    max_segment_px: f32,
    out: &mut Vec<(f32, f32)>, // screen (x, y)
);

/// 区間 1 本のサンプル数。`max(16, ceil(|dx| / max_segment_px))` を 512 で cap。
/// 16 は短い区間でも形が視認できる最小段数 (旧 Exponential 分岐と同じ既定)。
#[must_use]
fn sample_count(dx_px: f32, max_segment_px: f32) -> usize;

/// clip 1 本ぶんの curve を flatten して screen 座標の点列で返す
/// (旧 `draw::flatten_lane_curve` の置き換え)。
/// `beat_to_px` は **screen-wide な拍 → px 換算** (= `body_w / view.len_beats`)。
/// clip 長 ≠ view 長のとき point dot 描画とずれないための SSoT。
#[must_use]
pub(super) fn flatten_clip_curve(
    clip: &ArrangementAutomationClip,
    map: LaneValueMap<'_>,
    view_start_beat: f64,
    body_origin_x: f32,
    beat_to_px: f64,
    max_segment_px: f32,
) -> Vec<(f32, f32)>;

/// r.md #73: 「掴んだ場所が指に付いてくる」逆算。
///
/// `target_norm` は目標の値 (norm)、`grab_u` は掴んだ位置の区間内進捗。
/// `start_curve` が `Hold` / `Linear` のときは呼び出し側で
/// `Exponential { bend: 0.0 }` に変換してから渡すこと (session の `start_curve`)。
///
/// 解けないとき (`|D| < 1e-6` の S 字固定点、`a ≈ b`) は `None` を返す
/// = caller は直前の preview を維持する。
///
/// **到達不能な目標は clamp された端の値になる** (= 線が指から離れる)。
/// `bend` / `tension` の値域 `-1.0..=1.0` は `AutomationCurve` 自身の宣言
/// (`common/src/model/automation.rs:159-169`) で、`Exponential` で `grab_u` から
/// 到達できる `w` は `[grab_u², √grab_u]` だけ。区間の端に近いほど狭い (§3.4)。
#[must_use]
pub(super) fn solve_bend(
    map: LaneValueMap<'_>,
    a_plain: f64,
    b_plain: f64,
    grab_u: f64,
    start_curve: AutomationCurve,
    target_norm: f32,
) -> Option<AutomationCurve>;
```

`solve_bend` の本体は §3.4 の手順をそのまま実装する。要点だけ再掲:

```text
if (b - a).abs() <= 1e-12         -> None
u  = grab_u.clamp(1e-4, 1 - 1e-4)
w  = ((to_plain(target_norm) - a) / (b - a)).clamp(1e-6, 1 - 1e-6)

Exponential:  k = w.ln() / u.ln();  bend = k.log2().clamp(-1, 1)
Bezier:       D = u * (1 - u) * (2 * u - 1);  if |D| < 1e-6 -> None
              Δ = w - u
              t0 = Δ / D
              tension = (if t0 >= 0 { t0 } else { Δ / (2 * D) }).clamp(-1, 1)
Hold/Linear:  到達しない (caller が start_curve を Exponential へ変換済)
```

### 4.4 `daw_gui/src/widgets/arrangement/draw.rs`

- `flatten_lane_segment` (1776-1840、doc 1770-1775) / `MAX_LANE_FLATTEN_DEPTH` (1842) /
  `perpendicular_dist_lane` (1849-1857、doc 1844-1848) / `flatten_lane_cubic` (1859-1885) /
  `flatten_lane_curve` (**1893-1925**、doc 1887-1892) を **すべて削除**
  (`curve.rs` に置き換わる)。

  **呼び出し元の全件 (2026-08-28 実測)**。「tests.rs の 7 本だけ」ではないので、
  grep を省かず 1 つずつ潰すこと:

  | 呼び出し元 | どうなるか |
  |---|---|
  | `draw.rs:1922` (`flatten_lane_curve` の中から `flatten_lane_segment`) | 関数ごと削除されるので消える |
  | `draw.rs:2174` (`draw_automation_lane` から `flatten_lane_curve`) | `curve::flatten_clip_curve` に差し替え (下記) |
  | **`render.rs:518`** (ハンドル preview 内から `flatten_lane_segment`) | §4.11 の「418-572 を丸ごと削除」に含まれる |
  | `tests.rs:2279` / `2295` / `2331` / `2362` / `2390` / `2413` / `2436` (7 本) | §5.1 (2) で `curve::flatten_segment` へ書き換え + `tests_curve.rs` へ移設 |
  | `geometry.rs:1385` | コメント内の言及のみ (`evaluate_bezier_y` の doc)。関数ごと削除される (§4.5) |

  **`render.rs:518` を見落とすと `flatten_lane_segment` 削除でコンパイルが通らない。**
  削除前に `grep -rn "flatten_lane_" daw_gui/src/` を 1 度打つこと。
- `draw_automation_lane` (1937-、doc 1927-1936) の curve 描画 (**2174**):

  ```rust
  let map = curve::LaneValueMap::from_lane(lane, clip_rect);
  let flat = curve::flatten_clip_curve(c, map, view.start_beat, body_rect.x, beat_to_px, 2.0);
  ```

  以降 (`push_lines`、2175-2189) はそのまま。
- point dot の y (**2200**、`clip_rect.y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_rect.h`) は
  `map.norm_to_y(p.value_norm)` に置き換える (同じ式の再実装を残さない)。
  x (2197-2199) は触らない — `body_rect.x + (abs_beat - view.start_beat) * beat_to_px` が
  curve 側と同じ SSoT で、ここを崩すと #028 user 指摘 2 (curve が point を通らない) が再発する。

### 4.5 `daw_gui/src/widgets/arrangement/geometry.rs`

**削除** (いずれも doc コメントごと。行番号は doc の先頭から関数の閉じ括弧まで)
- `evaluate_bezier_y` — doc 1384-1387 + fn 1388-1398
- `compute_curve_handle_pos` — doc 1400-1404 + fn 1405-1425
- `find_curve_param_handle_at` — doc 1427-1434 + fn 1435-1516
- `curve_param_delta_from_dy` — doc 1518-1522 + fn 1523-1526

(= **1384-1526 が連続した削除範囲**。前は `find_lane_clip` (1374-1382)、
後は `find_automation_point_data` (doc 1528-1531 + fn 1532-1541) で、どちらも残す。)

**追加**

```rust
/// r.md #73: lane body 内の cursor から、曲げられる区間の当たりを返す。
///
/// 判定は「cursor x から区間内進捗 `u` を出し、`curve::eval_norm` で曲線の y を
/// 評価して `|cy - y| <= style.automation_curve_segment_hit_px`」。
/// 曲線の形の評価は `common::automation::apply_curve` 1 本を通る (SSoT)。
///
/// **`automation_point_at` が `None` のときだけ呼ぶこと** — 点の当たり判定 (半径 2 倍)
/// が区間より先に効く (Alt+クリックの点削除と共存させるため)。
///
/// 除外するもの:
/// - `norm_mapping_is_invertible` が false の lane (= Mute。逆算に連続解が無い)
/// - 端点の screen y の差が 1px 未満の区間 (= 水平区間。数学的に曲げられない。
///   端点が両方とも窓の外で同じ端に飽和している区間もここで落ちる)
/// - 幅 0 の区間
/// - 入射側 point の `id == 0` (未採番 sentinel。安定 id で指せない)
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(super) fn automation_segment_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<AutomationSegmentHit>;

/// `automation_segment_at` の戻り値。bend session の anchor をそのまま作れる形。
#[derive(Clone, Copy, Debug)]
pub(super) struct AutomationSegmentHit {
    /// 入射側 point (= curve 属性を持つ後ろの点)。
    pub point: AutomationPointIdKey,
    /// 掴んだ位置の区間内進捗。
    pub grab_u: f64,
    /// 区間の始点値 / 終点値 (plain)。overlay の再描画にもそのまま使う。
    pub a_plain: f64,
    pub b_plain: f64,
    /// 現在の curve。
    pub curve: common::model::AutomationCurve,
    /// clip 描画域 (縦 padding 適用済)。norm ↔ y の anchor。
    pub clip_rect: Rect,
    /// 区間の端点 screen x (overlay の強調描画で使う)。
    pub x_prev: f32,
    pub x_next: f32,
}
```

引数の並びは `automation_point_at` (**1262-1329**、doc 1257-1261) と**同じ**
(`.., lanes, cx, cy, style`)。
実装も同じ骨格 — `for_each_visible_lane` (1110) を回し、lane ごとに
`clip_y = body_rect.y + style.automation_clip_v_pad_px` (1295-1296) /
`clip_h = (body_rect.h - pad * 2.0).max(2.0)` (1297) を出し、
`beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6)` (1294) で x を出す。
**この式を書き直さず、`automation_point_at` と同じ形にすること** (point dot と curve の
x がずれる既知バグ (#028 user 指摘 2) の再発防止)。分母が `lanes.w` ではなく
**`body_rect.w`** である点に注意 (`automation_point_at:1294` の実測)。

`find_automation_point_data` (doc 1528-1531 + fn 1532-1541) は positional のまま残す
(既存経路が使う)。新たに id 版を足す:

```rust
/// r.md #73: 安定 id で point を引く (`find_automation_point_data` の id 版)。
#[must_use]
pub(super) fn find_automation_point_by_id(
    visible_tracks: &[ArrangementTrack],
    key: AutomationPointIdKey,
) -> Option<&ArrangementAutomationPoint>;
```

### 4.6 `daw_gui/src/widgets/arrangement/view_build.rs`

- import (14-23) の 20 行目から `ArrangementCurveKind` を外す。
- `model_curve_to_widget` (**755-764**、doc 755) を **削除**。
- 点の構築 (**541-551**、`.map(|p| ArrangementAutomationPoint { .. })`):

  ```rust
  .map(|p| ArrangementAutomationPoint {
      id: p.id,
      time_beat: p.time_beat - c.content_offset_beats,
      value_norm: common::automation::plain_to_norm_ranged(&lane.target, p.value, range),
      value_plain: p.value,
      curve: p.curve,
  })
  ```

- lane の構築 (`ArrangementAutomationLane { .. }`、**575-588**) に
  `target: lane.target.clone(), plugin_range: range,` を追加
  (`default_value_norm` を渡す 586 行目の隣)。
  `range` は既に **528** 行目で `let range = range_of(&lane.target);` として求まっている
  (`default_value_norm` (529-530) と点の `value_norm` (545-549) が同じ `range` を使う)。
  **この関数 (`build_arrangement_lanes_from_slice`、515-591) は track lane と
  master / song lane の両方から呼ばれる** (`build_arrangement_automation_lanes` 506-513 経由)
  ので、1 か所直せば両方に効く。

### 4.7 `daw_gui/src/widgets/arrangement/press_lanes.rs` (現 `run.rs`)

**削除**
- curve handle press ブロック (`run.rs:619-660`、説明コメント 619-631 込み)。
  `handle_press_started` ローカル (`run.rs:632`、立てるのは `:659`) と、
  それを参照する 2 か所のガード (`run.rs:670` / `run.rs:776`) も一緒に消す。
  #77 後は `press_lanes::curve_handle` 関数ごと削除し、`PressClaim.curve_handle` フィールドと
  `press.rs` のゲート表からも消す。
- Alt+ドラッグ = レーン / 行の高さ変更ブロック (**`run.rs:835-919`**) を**丸ごと削除**
  (説明コメント 835-840 込み。**834 は空行、920 も空行、921 は lasso ブロックの
  先頭コメントなので巻き込まない**)。
  #77 後は `press_lanes::alt_resize` 関数ごと削除。
- `no_session` 列挙の `s.automation_curve_param_drag.is_none()` は **削除ではなく差し替え**。
  現在 3 か所ある:

  | 場所 | 何の列挙か | #73 後 |
  |---|---|---|
  | `run.rs:860` (列挙 848-861) | Alt+drag resize の `no_session` | **ブロックごと消えるので列挙も消える** |
  | `run.rs:947` (列挙 935-948) | lasso の `no_session` | `s.automation_segment_bend.is_none()` に差し替え |
  | `release.rs:853` (列挙 841-856) | marquee の `no_session` | `s.automation_segment_bend.is_none()` に差し替え (§4.12) |

  #77 後は `PressClaim::from_live` の **11 列挙** (`docs/plan_rmd_77_arrangement_split.md` §6-B) の
  `automation_curve_param_drag` を `automation_segment_bend` に差し替える。
  `release.rs` の marquee 列挙は #77 後も release.rs に残るので**別途直す**。

**変更 — `automation_point_at` の結果を hoist する (§3.5)**

point press ブロック (`run.rs:662-753`、説明コメント 662-668) の直前で 1 度だけ評価する:

```rust
// r.md #73: point の当たりは **この 1 回だけ**評価して、point press / 区間 bend /
// automation clip press の 3 つのゲートで共有する。
// 旧実装は clip press 側で `already_taken_by_point = automation_point_drag.is_some()`
// を見ていたが、Alt+クリック (点) は drag session を立てず `press_delete_point` だけを
// 立てるので、この述語では「点の削除」と後続 press が同フレームで両方走る。
let point_hit = if !splitter_press && in_lanes {
    automation_point_at(
        &visible_tracks, &press_tops, view.track_row_h, view,
        header_pane.x, header_pane.w, lanes, px, py, style,
    )
} else {
    None
};
// 前フレームから残存する point drag session も「point 層が消費した」に含める。
// これは #77 の `PressClaim::from_live` の seed と同じ値なので、OR にしておけば
// #77 の等価性トランスクリプト (§6-B:517-521) を壊さない (§3.5)。
let already_taken_by_point = {
    let s: &ArrangementState = ui.widget_state(wid);
    s.automation_point_drag.is_some()
};
let point_consumed = point_hit.is_some() || already_taken_by_point;
```

既存の point press ブロックは `if let Some((point_key, _r)) = point_hit` に書き換える
(`automation_point_at` の 2 度呼びを残さない)。
`already_taken_by_point` の定義は現在 clip press の直前 (`run.rs:762-765`) にあるので、
**この hoist で上へ移す** (定義を 2 つに増やさない)。

**hoist で意味が弱くならないことの確認**: 現行の `already_taken_by_point` は point press
ブロックの**後**で読むので「前フレームの残存 ∪ このフレームで point block が起動した session」
を意味する。hoist すると前者だけになるが、後者は `automation_point_at` が `Some` を返した
ときにしか起きない (`run.rs:669-753` の起動は `point_hit` の内側) ので
`point_hit.is_some()` に完全に含まれる。したがって
`point_consumed ⊇ 現行の already_taken_by_point` が常に成り立ち、ゲートは**単調に強くなる**
方向にしか動かない。新しく増えるのは「点に当たったが session を起動しなかった」ケース
— Alt+クリック (削除) と、`find_lane_clip` / `automation_lane_at` の lookup が外れた場合 —
で、これが §3.5 で塞ぎたかった穴そのもの。

**追加** — automation point press ブロックの**直後**、automation clip press の**直前**に置く:

```rust
// r.md #73: Alt + レーン本体の線 → 区間の曲げ (bend) session。
// 優先順位は point (半径 2 倍) の **後**、automation clip の **前**。
// point が先に効くので Alt+クリック (点) の削除と共存する。
// Hold / Linear の区間は「曲線」(= Exponential) へ自動変換してから量を付ける。
// commit は release で 1 回だけ (undo 1 段)。
if !splitter_press
    && !point_consumed
    && in_lanes
    && pointer.modifiers.alt
    && !shift
    && !ctrl
    && let Some(hit) = automation_segment_at(
        &visible_tracks, &press_tops, view.track_row_h, view,
        header_pane.x, header_pane.w, lanes, px, py, style,
    )
{
    let start_curve = match hit.curve {
        AutomationCurve::Hold | AutomationCurve::Linear => {
            AutomationCurve::Exponential { bend: 0.0 }
        }
        other => other,
    };
    let anchor_value_norm = find_lane_clip(&visible_tracks, hit.point.clip)
        .map(|(lane, _)| curve::LaneValueMap::from_lane(lane, hit.clip_rect))
        .map_or(0.0, |map| {
            curve::eval_norm(map, hit.a_plain, hit.b_plain, hit.grab_u, start_curve)
        });
    let state: &mut ArrangementState = ui.widget_state(wid);
    state.automation_segment_bend = Some(AutomationSegmentBendSession {
        point: hit.point,
        grab_u: hit.grab_u,
        a_plain: hit.a_plain,
        b_plain: hit.b_plain,
        anchor_curve: hit.curve,
        start_curve,
        anchor_value_norm,
        clip_rect_anchor: hit.clip_rect,
        anchor_mouse_y: py,
        last_mouse_y: py,
        preview_curve: hit.curve,
    });
}
```

`preview_curve` の初期値は **`anchor_curve`** (= press 直後は今の見た目のまま)。
最初の continuation で `start_curve` を基準に解いた結果へ切り替わる
(= Hold 区間は最初の 1px 動かした瞬間に直線化してから曲がる。§3.4 の例外 1)。

**automation clip press ブロック (`run.rs:755-833`、説明コメント 755-773) の変更**
- `&& !pointer.modifiers.alt` (`run.rs:778`) を **外す** (§3.6)。
- `!already_taken_by_point` (`run.rs:775`、定義 762-765) を **`!point_consumed`** に置き換える
  (定義自体は上の hoist で移動済)。
- `&& !handle_press_started` (`run.rs:776`) は curve handle ごと消えるので削除。
- 「bend session が起動していたら skip」を足す
  (`state.automation_segment_bend.is_none()`。#77 後は `!claim.session`)。
- コメント (`run.rs:766-772`) の「Alt 修飾は **lane Alt+drag for resize に予約**する …
  既存 automation clip Alt-snap-off 機能は失われるが」を
  「r.md #73: Alt+drag resize を撤去したので Alt = スナップ無効が復活 (MIDI / audio clip と対称)。
  線の上の Alt+drag は 1 段上の bend が先勝する」に書き換える。
  773 行目の「handle press が先勝した場合 clip drag も skip」も消す。

**lasso press ブロック (`run.rs:921-980`、説明コメント 921-928) の変更**
- `&& !pointer.modifiers.alt` (`run.rs:931`) を **外す** (§3.6)。
- `no_session` (935-948) に `s.automation_segment_bend.is_none()` を入れる (上の差し替え表)。
- コメント (`run.rs:927-928`) の「Alt は lane resize に予約済 (上の Alt+drag fallback で先勝)
  なので `!pointer.modifiers.alt` で除外」を撤回する 1 行に書き換える。

### 4.8 `daw_gui/src/widgets/arrangement/drag.rs` (現 `run.rs:1166-1177`、説明コメント 1161-1165)

curve param continuation を bend continuation に置き換える:

```rust
// r.md #73: 区間 bend の continuation。release frame は last_mouse_y を
// anchor と異なるときだけ更新する (既存 OS event 順序 race 回避 pattern)。
// preview_curve は毎 frame **逆算** で作り直す (= live preview の SSoT、
// release で final 値として使う)。解けない frame は直前値を維持する。
if let Some(ref mut bd) = state.automation_segment_bend {
    if !is_release {
        bd.last_mouse_y = py;
    } else if (py - bd.anchor_mouse_y).abs() > f32::EPSILON {
        bd.last_mouse_y = py;
    }
    // lane は毎 frame 引き直す (session に target を持たせない = Copy を保つ)。
    if let Some((lane, _clip)) = find_lane_clip(visible_tracks, bd.point.clip) {
        let map = curve::LaneValueMap::from_lane(lane, bd.clip_rect_anchor);
        let dy = bd.last_mouse_y - bd.anchor_mouse_y;
        let target_norm =
            (bd.anchor_value_norm - dy / bd.clip_rect_anchor.h.max(1.0)).clamp(0.0, 1.0);
        if let Some(next) = curve::solve_bend(
            map, bd.a_plain, bd.b_plain, bd.grab_u, bd.start_curve, target_norm,
        ) {
            bd.preview_curve = next;
        }
    }
}
```

`find_lane_clip` は既存 (`geometry.rs:1374-1382`、doc 1370-1373。`AutomationClipKey` を取って
`(lane, clip)` を返す)。**新しい lane 専用 helper は作らない。**

`ui.widget_state::<ArrangementState>(wid)` の借用中に `ui.push_edit` を呼ばないこと (既存の制約)。

### 4.9 `daw_gui/src/widgets/arrangement/sessions.rs` (現 `run.rs:1633-1641`、説明コメント 1628-1632)

`automation_curve_param_session` / `automation_curve_param_release` を
`automation_segment_bend_session` / `automation_segment_bend_release` に差し替える
(clone + `take()` の形はそのまま)。

#77 後は 3 つの struct のフィールド名を差し替える
(`docs/plan_rmd_77_arrangement_split.md` §6-F):

| struct | 旧 | 新 |
|---|---|---|
| `LiveSessions` | `automation_curve_param: Option<AutomationCurveParamDragSession>` | `automation_segment_bend: Option<AutomationSegmentBendSession>` |
| `ReleasedSessions` | `automation_curve_param: ...` | `automation_segment_bend: ...` |
| `Overlays` | `curve_param: Option<AutomationCurveParamDragSession>` | `segment_bend: Option<AutomationSegmentBendSession>` |

### 4.10 `daw_gui/src/widgets/arrangement/cursor.rs` (現 `run.rs:1684-1849`)

- hover 計算 (`run.rs:1684-1745`、#77 後は `cursor::hover`) に 1 段追加:

  ```rust
  // r.md #73: Alt 押下中に「曲げられる区間」の上にいるかを公開する
  // (overlay の強調 + カーソル形状)。point が先に当たっていたら None。
  response.hovered_automation_segment = if f.pointer.modifiers.alt {
      f.pointer.pos.and_then(|(cx, cy)| {
          if automation_point_at(/* .., cx, cy, style */).is_some() {
              return None;
          }
          automation_segment_at(/* .., cx, cy, style */).map(|h| h.point)
      })
  } else {
      None
  };
  ```

- cursor の分岐 (`run.rs:1747-1849`、#77 後は `cursor::apply`) に 1 段追加。
  `hovered_section_zone` の後、lane/row splitter の**前**に置く:

  ```rust
  } else if response.hovered_automation_segment.is_some() {
      // r.md #73: Alt hover 中の区間は縦ドラッグで曲げる → NsResize。
      // lane/row splitter も NsResize なので視覚的な衝突は無い。
      ui.set_cursor(CursorIcon::NsResize);
  ```

  bend drag 中も NsResize にする — 先頭付近の `resize_active` に
  `|| live.automation_segment_bend.is_some()` を足す。

  **新しい `CursorIcon` variant は追加しない** (`ui/crates/platform/src/window.rs:11-25` に
  Default / Pointer / Text / Crosshair / EwResize / NsResize / Move / Hidden。
  daw-ui core に DAW 都合を持ち込まない = 不変条件 8)。

- **算出した値を heavy 層へ渡す配線を必ず一緒に入れる。**
  `render_arrangement_heavy` は `ArrangementResponse` を受け取らないので、
  **response に足しただけでは §4.11 の hover 強調が描けない。**
  既存の `hovered_clip` が同じ問題を明示的に解いていて (`run.rs:2090-2094` のコメント
  「`response.hovered_clip` は上の『hover 計算』で **このフレーム中に** 確定済」→
  `let hovered_clip_for_heavy: Option<ClipKey> = response.hovered_clip;`、
  `run.rs:2096` で実引数として渡し、`render.rs:25-28` で受ける)、#73 はその 1:1 の写しを作る:

  ```rust
  // run.rs:2094 の隣。
  // r.md #73: Alt hover 中の「曲げられる区間」。`hovered_clip` と同じく
  // **`viewport_key` にも `fold_arrangement_clip_hash` にも入れないこと**
  // (hover でアレンジ全体が再構築される)。強調は overlay 層で描く。
  let hovered_segment_for_heavy: Option<AutomationPointIdKey> =
      response.hovered_automation_segment;
  ```

  `run.rs:2096` の `render::render_arrangement_heavy(..)` 呼び出しに
  `hovered_clip_for_heavy` の直後で並べる。
  **#77 後は `HeavyInput` (`docs/plan_rmd_77_arrangement_split.md` §6-J:1137-1152) に
  `pub hovered_segment: Option<AutomationPointIdKey>,` を 1 本足し、`render::dispatch`
  (同 §6-J:1126-1133、`response: &ArrangementResponse` を受け取る) の中で
  `hovered_clip` の隣に詰める** — 引数は増えない。

### 4.11 `daw_gui/src/widgets/arrangement/render.rs`

**削除**
- ハンドル描画 + preview ブロック (**418-572**) を**丸ごと削除**
  (説明コメント 418-421 + `if !selected_automation_points_for_heavy.is_empty() {` (422) から、
  `if let Some((nd, bd, td)) = drag_overlay_clone {` (573) の直前まで)。
  このブロックは `flatten_lane_segment` (**518**) と style の handle 4 フィールド
  (425 / 426 / 561 / 562) を使っている唯一の場所なので、§4.2 / §4.4 の削除と対になる。
- 引数 `curve_param_overlay: Option<AutomationCurveParamDragSession>` (47) を
  `segment_bend_overlay: Option<AutomationSegmentBendSession>` に差し替え。
  #77 後は `Overlays.segment_bend`。

**引数の追加 (§4.10 の配線の受け側)**
- `hovered_clip: Option<ClipKey>` (**28**、doc 25-27) の直後に足す:

  ```rust
  // r.md #73: Alt hover 中に「曲げられる区間」があるならその入射側 point。
  // Alt 強調 (下の overlay 層) を出す対象を決めるためだけに使う。
  // **`viewport_key_hash` の材料にしてはいけない** (hover でアレンジ全体が再構築される)。
  hovered_segment: Option<AutomationPointIdKey>,
  ```

  #77 後は `HeavyInput.hovered_segment` を読むだけで、引数は増えない (§4.10)。
  なお `render.rs:5` の `#![allow(clippy::too_many_arguments)]` は #77 が消す予定なので、
  **#73 が新しい `#[allow]` を足すことは無い** (#77 が landed 済 = 4 引数の世界で作業する)。

**追加** — overlay 層 (`hctx.cached` の外、= 223 行目以降のゾーン) に 2 つ:

```rust
// r.md #73: (1) Alt hover 中の区間を強調 (どこを掴むと何が起きるかの可視化)。
//           cached ではなく overlay に描く — hover は毎フレーム変わるので
//           cache キーに混ぜると全再構築になる。
// r.md #73: (2) bend drag 中の preview 曲線。cached の base curve を
//           line_width × 1.5 の `automation_curve_bend_preview_color` で覆う。
//           形の評価は cached 側と同じ `curve::flatten_segment` を通す
//           (= 「プレビューだけ別式」を作らない)。
```

どちらも `find_lane_clip` で lane を引いて `curve::LaneValueMap::from_lane` を作り、
`curve::flatten_segment(map, (x_prev, a_plain), (x_next, b_plain), c, 2.0, &mut pts)`
→ `push_lines` で描く。
既存の「selected automation points overlay」(343 行目) と同じゾーンに置く。

(1) の入力は上で足した `hovered_segment` (= `AutomationPointIdKey`)。
区間の端点 x / 端点 plain 値 / clip 描画域は、その key から
`geometry::find_lane_clip` + `geometry::find_automation_point_by_id` で引き直す
(**hover のたびに `automation_segment_at` を再実行しない** — cursor 層で 1 度出した結果を
key で運ぶ)。(2) の入力は `segment_bend_overlay`
(`AutomationSegmentBendSession` は `clip_rect_anchor` / `a_plain` / `b_plain` /
`preview_curve` を全部持っているので lane の再 lookup は `LaneValueMap` 用の
`target` / `plugin_range` を取るためだけ)。

### 4.12 `daw_gui/src/widgets/arrangement/release.rs`

**署名 (現状)**
- `commit_releases` (**10-44**、引数は 11-43) の引数
  `automation_curve_param_release: Option<AutomationCurveParamDragSession>` (**37**) を
  `automation_segment_bend_release: Option<AutomationSegmentBendSession>` に差し替える。
- 呼び出し側 `run.rs:2099` の実引数 `automation_curve_param_release` も差し替える。
- `run.rs:1959` の `let curve_param_overlay = automation_curve_param_session;` と
  `run.rs:2096` の `render_arrangement_heavy(.., curve_param_overlay, ..)` も差し替える。
- #77 後はこの 4 か所が `ReleasedSessions.automation_segment_bend` /
  `Overlays.segment_bend` に畳まれているので、フィールド名の差し替えだけで済む。

**削除**
- **332-338** — 点の無修飾クリックで `SelectAutomationClips { next: vec![] }` を撃つブロックと、
  その理由コメント (332-334) だけ。
  **330-331 (`SelectModifier` / 「アンカーが別 clip / 別 lane に居るときは filter で落として
  Single に倒れる」) は残す** — 別の話をしている。代わりに 1 行:

  ```rust
  // r.md #73: ここで clip 選択を消さない。選択集合は面を跨いで共存でき
  // (`handler/selection_view.rs:51-52` の `edit_surface` doc)、Delete / Copy / Cut の
  // 宛先は last-wins が解決する。Ctrl+A の 2 段目も共存前提で書かれている。
  ```

- **543-549** — クリップの無修飾クリックで `SelectAutomationPoints { next: vec![] }` を
  撃つ対称ブロックと、その理由コメント (543-545)。同じく削除 + 同趣旨のコメント。
- **373-382** — curve param drag release ブロック (説明コメント 373-377)。
  下の bend release に差し替える。

  `selected_automation_clips` / `selected_automation_points` の引数は
  他 (293 / 326 / 406 / 456 / 540) で使うので**残す**。

**変更**
- marquee の `no_session` 列挙 (**841-856**) の `s.automation_curve_param_drag.is_none()` (853) を
  `s.automation_segment_bend.is_none()` に差し替える。
- `marquee_zone_ok` (**825-840**) の `&& !pointer.modifiers.alt` (826) を **外す** (§3.6)。
  ここは `lanes.contains(px, py)` (829) を要求するので、ヘッダ列には元から届かない
  (= ヘッダ列の Alt+drag が marquee になることは無い)。
  外す理由を 1 行コメントで残す:

  ```rust
  // r.md #73: Alt を弾いていたのは空き track row の Alt+drag が行高さ変更に
  // 予約されていたから。その機能を撤去したので、ここで弾くと Alt+drag が
  // 何も起こさない死角になる。marquee に snap は無いので Alt に別の意味は無い。
  ```

**追加 (1) bend release**

```rust
// ---- r.md #73: automation_segment_bend release → SetAutomationCurve ----
// preview が anchor と同値なら no-op (= Alt+クリックしただけで動かしていない)。
// point は **安定 id** で指す。undo は snapshot 方式なので prev は載せない。
if let Some(bd) = automation_segment_bend_release
    && bd.preview_curve != bd.anchor_curve
    && bd.point.point_id != 0
{
    ui.push_edit({
        let (k, next) = (bd.point, bd.preview_curve);
        Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetAutomationCurve {
                track_id: k.clip.track,
                lane_id: k.clip.lane,
                clip_id: k.clip.clip,
                point_id: k.point_id,
                next,
            });
        })
    });
}
```

**追加 (2) Alt+ダブルクリックで直線に戻す**

double-click ブロック (**1196-1292**、説明コメント 1185-1195) の分岐順を次のようにする。
**`automation_point_at` (1201-1215) の後、`automation_lane_at` (1216-) の前**に 1 段挟むだけ:

```
1. clip_hit (track row 内の MIDI/Audio clip) -> DoubleClickClip                  [既存]
2. automation_point_at                        -> BeginEditAutomationPointValue   [既存]
3. [新] Alt && automation_segment_at          -> SetAutomationCurve { next: Linear }
4. automation_lane_at + automation_clip_at    -> AddAutomationPoint (Alt でスナップ無効) [既存]
5. automation_lane_at + clip ギャップ          -> CreateAutomationClip            [既存]
6. track row の空き                            -> DoubleClickEmpty                [既存]
```

3 の実装:

```rust
} else if pointer.modifiers.alt
    && let Some(hit) = automation_segment_at(/* .., cx, cy, style */)
    && hit.point.point_id != 0
{
    // r.md #73: 線の上 (6px 以内) で Alt+ダブルクリック → その区間を直線に戻す。
    // 線から離れていれば下の AddAutomationPoint 経路に落ちて、
    // 従来どおり Alt = スナップ無効で点を足す。
    if hit.curve != AutomationCurve::Linear {
        ui.push_edit({
            let k = hit.point;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAutomationCurve {
                    track_id: k.clip.track,
                    lane_id: k.clip.lane,
                    clip_id: k.clip.clip,
                    point_id: k.point_id,
                    next: common::model::AutomationCurve::Linear,
                });
            })
        });
    }
}
```

**Alt+ホイールのブロック (989-1073) は一切触らない。**

### 4.13 `daw_gui/src/event.rs`

- `SetAutomationCurveType` (256-266、doc 256-258) /
  `SetAutomationCurveBezierTension` (267-280、doc 267-272) /
  `SetAutomationCurveExponentialBend` (281-292、doc 281-284) の
  **3 variant を削除**し、1 本に置き換える:

  ```rust
  /// r.md #73: 1 区間の補間形状を設定する **唯一の** event。
  /// 右クリックメニュー (階段 / 直線 / 曲線 / S 字) と、レーン本体の線の
  /// Alt+ドラッグ (release で 1 件)、Alt+ダブルクリック (直線に戻す) が
  /// すべてこれを発火する。
  ///
  /// 点は **安定 id** (`common::model::AutomationPoint::id`) で指す —
  /// 曲線編集は press → release を跨ぐので positional index では追加 / 削除で
  /// ずれる (アーキテクチャ不変条件 1)。`point_id == 0` は未採番 sentinel で no-op。
  ///
  /// **`prev` は持たない。** undo は `SongDoc::edit` の snapshot 方式で、
  /// 旧 `is_undoable` whitelist と手動 `push_undo_snapshot` は arch refactor で
  /// 全廃されている (`daw_gui/src/state/song_doc.rs:1-15`)。旧 3 event は
  /// `prev` を運んでいたが `app.rs:880-907` で `prev: _` として捨てられており、
  /// 誰も読んでいなかった (doc の「`SetTrackVolume` と同じ pattern」も誤り —
  /// `SetTrackVolume` は `{ track, amp }` だけで prev を持たない、`event.rs:991`)。
  SetAutomationCurve {
      track_id: u32,
      lane_id: u32,
      clip_id: u32,
      point_id: u32,
      next: common::model::AutomationCurve,
  },
  ```

- **`undo_label`** (`event.rs:1650`) の 3 arm (1787-1789) を 1 arm に:

  ```rust
  E::SetAutomationCurve { .. } => "カーブ変更",
  ```

  (関数名は `is_undoable` ではない。`is_undoable` whitelist は arch refactor で撤去済み。)

### 4.14 `daw_gui/src/app.rs`

- 880-907 の 3 arm を 1 arm に:

  ```rust
  AppEvent::SetAutomationCurve { track_id, lane_id, clip_id, point_id, next } => {
      self.set_automation_curve(track_id, lane_id, clip_id, point_id, next)
  }
  ```

### 4.15 `daw_gui/src/handler/automation.rs`

- `set_automation_curve_type` (457-) / `set_automation_curve_bezier_tension` (491-、doc 487-490) /
  `set_automation_curve_exponential_bend` (528-560、doc 524-527) の
  **3 本を削除**し、1 本に置き換える (= **457-560 が連続した削除範囲**。
  前は `delete_automation_points` 系の末尾 455、後は `quantize_selected_automation_points`
  の doc 562-568):

  ```rust
  /// r.md #73: 1 区間の補間形状を設定する唯一の handler。
  /// 点は安定 id で引く (positional index は追加 / 削除でずれる)。
  /// 該当 point が見つからなければ no-op (= drag 中に点が消えた race)。
  pub(crate) fn set_automation_curve(
      &mut self,
      track_id: u32,
      lane_id: u32,
      clip_id: u32,
      point_id: u32,
      next: common::model::AutomationCurve,
  ) {
      // `0` は未採番 sentinel。先頭の別の点を掴まないよう最初に弾く。
      if point_id == 0 {
          return;
      }
      // 値域は `AutomationCurve` 自身の宣言 (`-1.0..=1.0`) に合わせる。
      // widget 側でも clamp するが、event は外から来るので handler でも守る。
      let next = match next {
          common::model::AutomationCurve::Bezier { tension } => {
              common::model::AutomationCurve::Bezier { tension: tension.clamp(-1.0, 1.0) }
          }
          common::model::AutomationCurve::Exponential { bend } => {
              common::model::AutomationCurve::Exponential { bend: bend.clamp(-1.0, 1.0) }
          }
          other => other,
      };
      self.edit_song_checked(|song| {
          let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
              return false;
          };
          let Some(clip) = lane.clip_by_id(clip_id) else { return false };
          let content_id = clip.content_id;
          let Some(common::model::ClipContent::Automation(a)) =
              song.clip_contents.get_mut(&content_id)
          else {
              return false;
          };
          let Some(p) = a.points.iter_mut().find(|p| p.id == point_id) else {
              return false;
          };
          if p.curve == next {
              return false;   // 同値なら dirty を立てない
          }
          p.curve = next;
          true
      });
  }
  ```

  旧 3 本にあった `matches!` による「既存 curve type と一致するときだけ更新」の race ガードは
  **不要になる** (event が 1 本になり、`next` が完全な `AutomationCurve` を持つため)。

### 4.16 `daw_gui/src/view/arrangement_view.rs`

- 257-327 の curve type popup (説明コメント 257-288、`context_menu_for` ループ 289-327):
  - 項目名 (`&["Hold", "Linear", "Bezier", "Exponential"]`、**293**) を
    `&["階段", "直線", "曲線", "S 字"]` に変更。
  - 275-288 のコメント (「widget の `ArrangementCurveKind` を介さず」
    「Phase 63n-9 (tension/bend handle) で landing 予定」) を現状に合わせて書き直す。
    **264-274 の「point popup と clip popup が同 frame で両方 open される bug」の
    説明は残す** — 別の話をしていて、今も生きている制約。
  - idx → curve のマップ:

    ```rust
    // r.md #73: 「曲線」は片側の膨らみ (Exponential)、「S 字」は両端が緩い形 (Bezier)。
    // 既定量は 0.5 (0.0 は直線と同一なので「選んだのに何も起きない」を避ける)。
    // 「曲線」の符号だけは区間の向きから決める — 保存する値は progress 基準
    // (= 上り区間と下り区間で符号が逆になる) なので、定数のままだと
    // 上り区間で「曲線」を選んだ瞬間に線が下へ沈む (#73 の元の症状)。
    // 画面上は常に上へ膨らませる。
    ```

    | idx | 表示 | 生成する `AutomationCurve` |
    |---|---|---|
    | 0 | 階段 | `Hold` |
    | 1 | 直線 | `Linear` |
    | 2 | 曲線 | `Exponential { bend: if next_value >= prev_value { -0.5 } else { 0.5 } }` |
    | 3 | S 字 | `Bezier { tension: 0.5 }` |

    (検算: 上り (b>a) で bend=-0.5 → k=2^-0.5≈0.707 → `u^k > u` → 直線より b 寄り = 画面上。
    下り (b<a) で bend=+0.5 → k≈1.414 → `u^k < u` → 直線より a 寄り = 画面上。)

  - `prev_value` / `next_value` は **plain**。既存 lookup は **306-315** で、
    `.and_then(|pts| pts.get(key.point_idx as usize))` (**313**) →
    `.map(|p| p.curve)` (314) → `let Some(prev) = prev else { return };` (315) という
    「prev curve を Undo 用に引く」形になっている。#73 では **`prev` を捨てる**ので、
    この chain を「`pts` (= `&[AutomationPoint]`) を掴んで
    `pts.get(idx - 1)` / `pts.get(idx)` の `.value` と `pts.get(idx)?.id` を取る」形へ
    書き換える。前の点が無い (= `key.point_idx == 0`) なら curve は意味を持たないので
    `return`。
  - 発火する event (**316-323**) を `SetAutomationCurve` に変更し、`point_id` は同じ lookup で
    得た `p.id` を使う (`prev` / `point_idx` は載せない)。`p.id == 0` なら `return`。
    **`automation_point_rects` の型は変えない** (`Vec<(AutomationPointKey, Rect)>` のまま。
    rect は popup の anchor にしか使わず、id は上の lookup で解決する)。

### 4.17 `daw_gui/src/view/shortcuts_help.rs`

オートメーションの行 (101-106) を次に置き換える:

```rust
MouseGestureDef { category: MouseCategory::Automation, gesture: "ダブルクリック (空き)", description: "ポイントを追加" },
MouseGestureDef { category: MouseCategory::Automation, gesture: "ダブルクリック (点)", description: "値を入力" },
MouseGestureDef { category: MouseCategory::Automation, gesture: "ドラッグ (点)", description: "ポイントを移動" },
MouseGestureDef { category: MouseCategory::Automation, gesture: "Alt+クリック (点)", description: "ポイントを削除" },
MouseGestureDef { category: MouseCategory::Automation, gesture: "Alt+ドラッグ (線)", description: "カーブの曲がり具合を変える" },
MouseGestureDef { category: MouseCategory::Automation, gesture: "Alt+ダブルクリック (線)", description: "カーブを直線に戻す" },
MouseGestureDef { category: MouseCategory::Automation, gesture: "右クリック (点)", description: "カーブの種類を選ぶ" },
```

(= 「ドラッグ (線の中央)」を削除し、Alt の 2 行を追加)

Zoom カテゴリ (110-113) に 1 行足す (Alt+ホイールが行 / レーンの高さを担うことを明示する。
Alt+ドラッグの高さ変更を撤去するので、代替手段が help に載っている必要がある):

```rust
MouseGestureDef { category: MouseCategory::Zoom, gesture: "Alt+ホイール", description: "トラック行とオートメーションレーンの高さを変更" },
```

### 4.18 `daw_gui/src/handler/tick.rs`

`insert_recording_point` (**626-656**、doc 617-625) が `thin_collinear_and_insert` に
`&mut a.points` を渡している (呼び出しは **648-653**、`let points = match entry { .. }` の
束縛が 644-647)。§4.1 の signature 変更に合わせて `&mut AutomationContent` を渡す形にする:

```rust
self.song_doc.edit_checked(scope, move |song| {
    let entry = song.clip_contents.entry(content_id).or_insert_with(|| {
        common::model::ClipContent::Automation(common::model::AutomationContent::default())
    });
    let common::model::ClipContent::Automation(a) = entry else {
        return false;
    };
    // r.md #73: content ごと渡して安定 id を採番させる (旧実装は id: 0 のまま挿していた)。
    common::automation::thin_collinear_and_insert(a, time_beat, plain_value, THIN_EPSILON_PLAIN);
    true
}) == Some(true)
```

doc コメント (617-625) の「Step D thinning は … に抽出」の段落に、
id 採番もこの関数が担うことを 1 行足す。

### 4.19 `docs/plan_automation.md`

- 800-804 の「Curve type popup を **4 択化**」の項に、#73 で UI 名が
  階段 / 直線 / 曲線 / S 字 になり、`SetAutomationCurveType` /
  `SetAutomationCurveBezierTension` / `SetAutomationCurveExponentialBend` が
  `SetAutomationCurve` 1 本に統合されたことを追記し、本計画へリンクする。
- 1201 の表 (curve type popup の行) を現状に合わせる。
- 「Phase 3 完了」節 (796-) の末尾と、Phase 63n-9 の記述 (840-849、
  `SetAutomationCurveParam` / handle drag) に、#73 で中央ハンドル方式が撤去され
  `SetAutomationCurve` 1 本に統合されたことを 1 行追記 (履歴として残す。消さない)。
- 1284 の `SetAutomationCurveType` schema と 1327 / 1358 の右クリックメニュー記述
  (`["Hold", "Linear", "Bezier"]`) を現状 + #73 後に合わせる。
- 154-170 の `AutomationCurve` / `AutomationPoint` 定義は**変更なし**であることを明記する
  (「#73 でも保存形式 (スキーマ) は変えていない。ただし録音点に id が振られるので、
  `id` / `next_point_id` が JSON に出るようになる (§3.1)」の 2 行を足す)。

### 4.20 この計画で編集するファイル (完全な一覧)

上の §4.1-§4.19 の対象を 1 表にしたもの。**着手前にこの表と `git status` を突き合わせ、
表に無いファイルを触っていないことを確認する。**

| ファイル | 新規/改 | 何をするか (節) |
|---|---|---|
| `common/src/automation.rs` | 改 | 2 述語追加 / `apply_curve` doc / `thin_collinear_and_insert` の signature + id 採番 / test 群 (§4.1) |
| `daw_gui/src/widgets/arrangement/curve.rs` | **新規** | 曲線 ↔ 画面の変換 SSoT (§4.3) |
| `daw_gui/src/widgets/arrangement/tests_curve.rs` | **新規** | 曲線テスト (移設 7 本 + 新規 8 本) (§5.1) |
| `daw_gui/src/widgets/arrangement/mod.rs` | 改 | mirror 型 / handle style / handle session の削除、`value_plain` / `id` / `target` / `plugin_range` / bend session / id key の追加、hash、`mod curve;` + `mod tests_curve;` (§4.2) |
| `daw_gui/src/widgets/arrangement/draw.rs` | 改 | `flatten_lane_*` 一式の削除 → `curve::*` 呼び出し (§4.4) |
| `daw_gui/src/widgets/arrangement/geometry.rs` | 改 | handle 系 4 本の削除 / `automation_segment_at` + `find_automation_point_by_id` の追加 (§4.5) |
| `daw_gui/src/widgets/arrangement/view_build.rs` | 改 | `model_curve_to_widget` 削除 / point に `id` + `value_plain` / lane に `target` + `plugin_range` (§4.6) |
| `daw_gui/src/widgets/arrangement/run.rs` (#77 後は `press_lanes.rs` / `drag.rs` / `sessions.rs` / `cursor.rs` / `press.rs`) | 改 | handle press / Alt+drag resize の削除、`point_hit` の hoist、bend press / continuation、session 差し替え、`!alt` ゲート撤去、hover 算出 + heavy への配線 (§4.7-§4.10) |
| `daw_gui/src/widgets/arrangement/render.rs` | 改 | handle 描画の削除 / bend preview + hover 強調 / 引数差し替え + `hovered_segment` 追加 (§4.11) |
| `daw_gui/src/widgets/arrangement/release.rs` | 改 | 選択の相互 clear 撤去 / bend release / Alt+ダブルクリック / marquee の `!alt` と `no_session` (§4.12) |
| `daw_gui/src/widgets/arrangement/tests.rs` | 改 | lane リテラル 4 か所に 2 フィールド追加 / 曲線テストを `tests_curve.rs` へ移設 (§5.1) |
| `daw_gui/src/event.rs` | 改 | 3 variant → `SetAutomationCurve` 1 本 / `undo_label` (§4.13) |
| `daw_gui/src/app.rs` | 改 | dispatch 3 arm → 1 arm (§4.14) |
| `daw_gui/src/handler/automation.rs` | 改 | handler 3 本 → `set_automation_curve` 1 本 (§4.15) |
| `daw_gui/src/handler/tick.rs` | 改 | `insert_recording_point` を `&mut AutomationContent` 渡しに (§4.18) |
| `daw_gui/src/view/arrangement_view.rs` | 改 | popup の項目名 / idx→curve マップ / 発火 event (§4.16) |
| `daw_gui/src/view/shortcuts_help.rs` | 改 | マウス操作一覧の行 (§4.17) |
| `daw_gui/tests/arr_widget.rs` | 改 | automation の足場 2 本 + テスト 12 本 (§5.2) |
| `docs/plan_automation.md` | 改 | 履歴と現状の追記 (§4.19) |

**触らない**: `common/src/model/**` (モデル不変、§3.1) / `common/src/project.rs` /
`common/build.rs` / `daw_gui/src/clipboard.rs` (§8-11) / `daw_gui/src/app_types.rs` (§8-10) /
`ui/crates/**` (§4.10 の `CursorIcon`) / `daw_audio/**` / `daw_plugin_host/**`。

---

## 5. テスト

### 5.1 widget の pure fn テスト (`cargo test -p daw_gui --lib`)

**曲線のテストは `tests.rs` に足さない。新規 `daw_gui/src/widgets/arrangement/tests_curve.rs`
に置く** (`mod.rs` の宣言は §4.2)。理由は god file budget (不変条件 9 / arch-lint チェック 6、
3,000 行): `tests.rs` は現状 **2,591 行**で、(2) の 7 本を上り / 下りの 2 ケース化し (3) の
新規 8 本を足すと 3,000 行に到達する。**「超えそうになったら切り出す」ではなく、
書き始める前に切り出す** — 到達してから分けると、分割 commit と機能 commit が混ざって
どちらの diff も読めなくなる。移動するのは下記 (2) の 7 本 + ヘルパ 1 本 (約 210 行) と、
新規に書く (3) の 8 本。**(1) は行レイアウトのテストなので `tests.rs` に残す。**

**(1) `ArrangementAutomationLane` の構造体リテラル 4 か所を直す (`tests.rs` に残す)**

`tests.rs:220` / `245` / `268` / `291` が `ArrangementAutomationLane { .. }` を直に組んでいる。
§4.2 で `target` / `plugin_range` を足すと 4 か所すべてコンパイルエラーになる。
`target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), plugin_range: None,` を足す
(この 4 本は行レイアウトのテストで curve を見ないので、affine な既定 target でよい)。
`ArrangementAutomationPoint` の構造体リテラルは `tests.rs` に **0 件** (実測: grep 0 hit) なので、
`id` / `value_plain` の追加でコンパイルエラーになる既存テストは無い。

**(2) `flatten_lane_segment` の 7 本 (2263-2450) を「方向不変性」の検証に書き換えて移設**

対象は `flatten_segment_endpoints_exact_for_all_curve_kinds` (2264-2286) /
`bezier_tension_{zero,positive,negative}` / `exponential_bend_{positive,negative,zero}` の **7 本**
(`flatten_lane_segment` の呼び出しも 7 か所: 2279 / 2295 / 2331 / 2362 / 2390 / 2413 / 2436)。
ヘルパ `sample_polyline_y` (**2242-2261**) も `tests_curve.rs` へ**一緒に移す**
(`tests.rs` の他のテストは使っていない)。移動元 (2237-2450、節見出しコメント込み) は
`tests.rs` から消える。

**機械的に新 API へ移植するだけにしない** — 旧テストは
「p1=(10,100) → p2=(50,40)」のように **screen y で 1 方向だけ**を見ており、
#73 の不具合 (上り区間で符号が逆) を通してしまう形だった。次の形に書き換える:

- 各テストを **上り区間 (v_prev=0.2 → v_next=0.8) と下り区間 (0.8 → 0.2) の 2 ケース**で回す。
- assert は「量 (`tension` / `bend`) を **画面上へ膨らむ向き**に増やすと、
  区間の中ほどで polyline の y が**小さくなる (= 画面上で上がる)**」。
  上りは `Exponential { bend: -x }` / 下りは `Exponential { bend: +x }` が上向き
  (§4.16 の符号表と同じ)。
- 端点厳密 (`flatten_segment_endpoints_exact_for_all_curve_kinds`) と
  「量 0 は直線と一致」(`bezier_tension_zero` / `exponential_bend_zero`) は
  性質としてそのまま残す (両方向で確認する)。
- `curve::flatten_segment` + affine な lane
  (`TrackBuiltin::Volume`、plain 0..2 ↔ norm 0..1) の `LaneValueMap` を組んで呼ぶ。

**(3) 新規 (これが #73 の回帰網。すべて `tests_curve.rs`)**

```rust
/// r.md #73 の本体: 上り区間でも下り区間でも「カーソルを上へ動かすと
/// 画面上で線が上がる」。旧実装は上り区間で逆になっていた。
#[test]
fn bend_drag_up_raises_the_line_on_both_rising_and_falling_segments()
```
上り (0.2 → 0.8) と下り (0.8 → 0.2) の 2 ケースで、
`solve_bend` に「直線より上の目標値」を渡し、得た curve を `eval_norm` で
grab_u で評価すると **直線より値が大きい (= 画面上で上)** ことを assert。
`Exponential` と `Bezier` の両方で。**符号 (bend の正負) を assert しないこと** —
progress 基準なので符号は区間の向きで変わる。見えるもの (y) を assert する。

```rust
/// 掴んだ場所が指に付いてくる: 到達可能な範囲内の目標なら、
/// 解いた curve を grab_u で評価すると目標値に一致する (1e-3 以内)。
#[test]
fn bend_solve_puts_the_curve_under_the_finger()

/// 到達不能な目標では端に飽和して止まる (発散も符号反転も NaN も起きない)。
/// grab_u=0.9 で w < 0.81 を要求すると bend が +1.0 に張り付く。
#[test]
fn bend_solve_saturates_instead_of_diverging()

/// S 字は u=0.5 を必ず通る (数学的な固定点) ので、そこを掴んでも解けない。
#[test]
fn bend_solve_returns_none_at_the_s_curve_fixed_point()

/// 水平区間 (a == b) と Mute lane は曲げられない。
#[test]
fn bend_is_refused_on_flat_segments_and_mute_lanes()

/// 区間 hit-test は描かれた曲線の上で当たり、20px 離れると当たらない。
/// 点の上では `automation_point_at` が先に当たる。id == 0 の点は対象外。
#[test]
fn automation_segment_at_hits_the_drawn_curve()

/// 描画は「鳴る形」と一致する: (a) affine + 窓の内側では旧 screen-y 実装と同値、
/// (b) log な target (GroupTransform::ScaleX) では
///     `plain_to_norm(apply_curve(plain_a, plain_b, u))` と一致、
/// (c) 端点が窓の外 (GroupTransform::X で a=-0.5, b=0.5) では
///     前半が下端に張り付いてから立ち上がる (= 旧 norm 直線とは別の形)。
#[test]
fn flatten_matches_apply_curve_in_plain_space()

/// `segment_is_straight_on_screen`: Volume の 0.5→1.5 は true、
/// GroupTransform::X の -0.5→0.5 は false、ScaleX はどこでも false。
#[test]
fn straight_on_screen_requires_affine_and_in_window_endpoints()
```

### 5.2 `daw_gui/tests/arr_widget.rs` (widget を実際に駆動。daw_gui は**起動しない**)

`arr_widget.rs` には現在 automation を扱うテストが 1 本も無い (grep 0 件) ので、
**足場から書く**。既存ハーネス (`build_app` **35-60** / `modifiers` **62-64** / `press` **66-74** /
`hold` **76-78** / `release` **80-87** / `frame` **89-93** / `drive_scene` **99-106** /
`drive` **108-111** / `no_mods` 113-115) はそのまま使う。
`CARGO_BIN_EXE_daw_gui` を含まないので `make test-nolaunch` で回せる。
`arr_widget.rs` は arch-lint チェック 6 の対象外 (検査対象は `daw_gui/src` 配下のみ、
`scripts/arch_lint.sh:285`) なので、ここは行数を気にせず書いてよい。

**`build_app` は `app.ui_prefs.arrange_snap_enabled = false` を設定している (58 行目)。**
スナップの有無を見るテスト (下の `alt_drag_off_the_line_moves_the_clip_without_snapping`) は、
**テスト側でまず `true` に戻してから** Alt を付ける / 付けないを比較すること。
既定のまま書くと「スナップ無しで動いた」が無条件に成立して**テストが空振りする**。

**足場 (1) — response を返す driver を足す**

`drive_scene` は Scene しか返さない。lane / point の実 rect が要るので、
`daw_gui/tests/arrange_fit_layout.rs:49-59` と同じ形で 1 本足す
(あちらは `FrameInput::default()` 固定なので、`PointerFrame` を受け取れるよう 1 引数増やす):

```rust
/// 1 フレーム走らせ、Edit を app に適用し、**widget が返した response** を得る。
/// レーン / 点の実 rect は widget が返す `automation_lane_rects` /
/// `automation_point_rects` を SSoT にする (テスト側でレイアウト式を複製しない)。
fn drive_resp(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame)
    -> daw_gui::widgets::arrangement::ArrangementResponse
{
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let mut captured = None;
    host.frame(app, &mut scene, screen, frame(p), |app, ui| {
        captured = Some(arrangement(app, ui, WIDGET_RECT));
    });
    captured.expect("arrangement() は毎フレーム response を返す")
}
```

**足場 (2) — automation lane + clip + 点を持つ Song を組む**

`daw_gui/tests/automation_clip_zoom.rs:89-116` の `add_track_automation_clip` (doc 87-88) と
`arr_widget.rs:216-` の `add_midi_track_with_clip` を合わせた形:

```rust
use common::model::{
    AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
    AutomationTarget, TrackBuiltinParam,
};

/// track 1 本 + Volume lane 1 本 + 4 拍の automation clip 1 本 + 点 2 つ。
/// `values` は **plain** (Volume は 0.0..=2.0、norm = plain / 2)。
/// 戻り値は `(track_id, lane_id, clip_id, content_id)`。
fn add_automation_lane_with_two_points(
    app: &mut AppData,
    values: (f64, f64),
    curve: AutomationCurve,
) -> (u32, u32, u32, ContentId) {
    let (track_id, lane_id, clip_id) = (10_u32, 1_u32, 100_u32);
    let mut cid = 0;
    app.edit_song(|song| {
        song.tracks.clear();                    // 既定 "Track 1" を除いて row 0 に固定
        cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: values.0,
                                      curve: AutomationCurve::Linear },
                    AutomationPoint { id: 2, time_beat: 4.0, value: values.1, curve },
                ],
                next_point_id: 3,
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = track_id;
            t.automation_lanes = vec![AutomationLane {
                id: lane_id,
                target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                default_value: 1.0,
                enabled: true,
                visible: true,
                height_px: 60,
                clips: vec![AutomationClip {
                    id: clip_id, name: String::new(),
                    start_beat: 0.0, length_beats: 4.0,
                    content_id: cid, content_offset_beats: 0.0,
                }],
                next_clip_id: clip_id + 1,
            }];
        }));
    });
    // lane を展開する (これが無いと lane 行が描かれず hit-test も走らない)。
    app.ui_prefs.expanded_automation_tracks.insert(track_id);
    (track_id, lane_id, clip_id, cid)
}

/// 現在の curve を model から読む。
fn point_curve(app: &AppData, cid: ContentId, point_id: u32) -> AutomationCurve;

/// 2 点の中点 (= 区間の真ん中) の screen 座標。
/// widget が返す `automation_point_rects` の dot 中心から出すので、
/// テスト側で ruler / arranger 帯 / master 行の高さを一切知らなくてよい。
/// `Linear` 区間なら曲線はこの中点を通る (= 線の上を掴める)。
fn segment_midpoint(resp: &ArrangementResponse) -> (f32, f32) {
    let mut c: Vec<(f32, f32)> = resp.automation_point_rects.iter()
        .map(|(_, r)| (r.x + r.w * 0.5, r.y + r.h * 0.5)).collect();
    c.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(c.len(), 2, "点 2 つを想定");
    ((c[0].0 + c[1].0) * 0.5, (c[0].1 + c[1].1) * 0.5)
}
```

手順は毎回同じ: `build_app` → `add_automation_lane_with_two_points` →
`drive_resp(host, app, PointerFrame::default())` で 1 フレーム描いて rect を得る →
`segment_midpoint` → `press`/`hold`/`release` を `drive` で流す → model を assert。
Alt は `modifiers(false, false, true)`。

**テスト**

```rust
/// Alt+ドラッグで上り区間を曲げると、model の curve が
/// Exponential になり、evaluate した値が直線より上になる。
#[test] fn alt_drag_bends_a_rising_segment_upward()

/// 同じジェスチャを下り区間でやっても、evaluate した値は直線より上になる
/// (= 画面上で上がる)。保存された bend の符号は上り区間と逆になる。
#[test] fn alt_drag_bends_a_falling_segment_upward_too()

/// Hold 区間を Alt+ドラッグすると「曲線」に自動変換されてから量が付く。
#[test] fn alt_drag_converts_hold_segment_to_a_curve()

/// Alt+ドラッグは release で 1 件だけ Edit を出す (undo 1 段)。
/// `app.song_doc` の undo 段数が 1 だけ増えることで確認する。
#[test] fn alt_drag_commits_once_on_release()

/// Alt+ダブルクリック (線の上) で直線に戻る。
#[test] fn alt_double_click_on_the_line_resets_to_linear()

/// Alt+ダブルクリック (線から離れた場所) は従来どおりスナップ無しで点を足す。
#[test] fn alt_double_click_off_the_line_still_adds_an_unsnapped_point()

/// r.md #73 (E): 点の無修飾クリックでクリップ選択が消えない。
#[test] fn clicking_a_point_keeps_the_automation_clip_selection()

/// 逆方向も同じ。
#[test] fn clicking_an_automation_clip_keeps_the_point_selection()

/// r.md #73: レーン本体の Alt+ドラッグはもうレーン高さを変えない
/// (`lane.height_px` が press 前後で不変)。
#[test] fn alt_drag_in_a_lane_no_longer_resizes_it()

/// r.md #73 (§3.6): 線から離れた場所の Alt+ドラッグは死角にならず、
/// automation clip が動く。しかも Alt がスナップを無効にしている
/// (= MIDI / audio clip と対称)。
///
/// **ハーネスの `build_app` は `arrange_snap_enabled = false` なので、
/// このテストは冒頭で `app.ui_prefs.arrange_snap_enabled = true;` に戻す。**
/// 戻さないと「Alt 無しでもスナップしない」ので比較が成立しない (空振りする)。
/// 手順: snap を ON にして (a) Alt 無しの drag → グリッドに吸着、
/// (b) Alt 付きの drag → 吸着しない、を同じ移動量で 2 回確認する。
#[test] fn alt_drag_off_the_line_moves_the_clip_without_snapping()

/// r.md #73 (§3.6): lane header 列の Alt+ドラッグはレーン高さを変えない
/// (撤去後は無反応。高さは Alt+ホイールとスプリッタが担う)。
/// `arrange_header_w` を 0 でない値にした fixture で回す
/// (既定ハーネスは `arrange_header_w = 0.0`、`build_app` 53 行目)。
#[test] fn alt_drag_in_the_lane_header_column_no_longer_resizes()

/// r.md #73 (§3.5): Alt+クリック (点) は点を消すだけで、
/// 同フレームに bend session も clip drag も起動しない。
#[test] fn alt_click_on_a_point_only_deletes_it()
```

### 5.3 触ってはいけないテスト

`common/src/automation.rs` の **曲線評価**のテストは **1 行も変更しない**:
`bezier_tension_zero_is_exactly_linear` (792) / `bezier_endpoints_exact_for_all_tensions` (815) /
`bezier_tension_positive_makes_s_curve` (836) / `bezier_tension_negative_inverts_s_curve` (863) /
`exponential_bend_zero_is_linear` (883) / `exponential_bend_one_is_quadratic_ease_in` (897) /
`evaluate_song_tempo_with_bezier_curve` (1268)。
モデルを変えていないので通るはず。**落ちたらモデルを変えてしまっている = 本計画違反。**
そのときはテストを直さず、モデルへの変更を戻すこと。

`common/src/model/tests.rs` も**触らない**。ただし旧版が挙げていた `:1648` は
**テスト関数ではなく fixture の中の `curve:` 行**なので、「そこにテストがある」と思って
探さないこと (同ファイルの automation fixture は 1636-1650 付近、`next_point_id: 0` /
`id: 0` の sentinel を含む = `ensure_ids` の採番を検証するための意図的な 0)。
§4.1 は `common/src/automation.rs` の関数しか触らないので、この fixture には影響しない。

**例外**: 同じファイル (`common/src/automation.rs`) の `thin_collinear_and_insert` の
テスト群 (1003-1123) は §4.1 の signature 変更に合わせて書き換える。
**呼び出しと points の借り方だけ**を変え、**期待値は 1 つも変えない** (§4.1 に対象行を列挙)。
こちらは曲線の数式とは無関係。

---

## 6. 検証手順

コマンドは 1 つずつ実行する (`&&` / `;` で連結しない)。作業ディレクトリへの `cd` は前置しない。

```bash
make check
```
```bash
make clippy
```
```bash
cargo test -p common --lib
```
```bash
cargo test -p daw_gui --lib
```
```bash
cargo test -p daw_gui --test arr_widget
```
```bash
make test-nolaunch
```
```bash
make arch-lint
```
```bash
cargo build --workspace
```

- **`make test` は使わない** (daw_gui を起動する)。
- `cargo build --workspace` は protocol 変更が無いことの確認 (子 exe も作り直しておく)。

### 実機確認 (最後に 1 度だけ、ユーザーに一声かけてから)

`make run` の前に必ず「起動します」と断ること (窓が前面に出て作業を妨げる)。
確認項目:

1. **上り区間**のオートメーション線を Alt+ドラッグで上へ → 線が上へ膨らむ。
   掴んだ場所がカーソルに付いてくる。
2. **下り区間**で同じ操作 → 同じく上へ膨らむ。
3. Hold 区間を Alt+ドラッグ → 直線化してから曲がる (最初の 1px で 1 度飛ぶ。§3.4 の例外 1)。
4. Alt+ダブルクリック (線の上) → 直線に戻る。undo で戻る。
5. Alt+ダブルクリック (線から離れた場所) → スナップ無しで点が増える。
6. Alt を押している間だけ、ポインタ下の区間が強調され、カーソルが縦矢印になる。
7. **レーン本体**の Alt+ドラッグでレーンの高さが変わらないこと。
   **レーンヘッダ列**の Alt+ドラッグでも変わらないこと (= 現状はここでも変わる。§3.6)。
   **トラックヘッダ列**の Alt+ドラッグは従来どおりトラック並べ替えになること。
   ヘッダ列とレーン本体の両方で **Alt+ホイール**で高さが変わること。
   レーン下端 / 行下端のスプリッタで高さが変わること。
8. **Alt+ドラッグの死角が無いこと** (§3.6): 線から離れた lane 本体で Alt+ドラッグ →
   オートメーションクリップがスナップ無しで動く。空きレーンで Alt+ドラッグ → 投げ縄。
   空きトラック行で Alt+ドラッグ → 矩形選択。
9. 点をクリックしてもオートメーションクリップの選択が消えないこと (逆も)。
   その状態で Delete が「直前にクリックした面」を消すこと。
10. 右クリックメニューが **階段 / 直線 / 曲線 / S 字** の 4 項目で、
    「曲線」を選ぶと**上り区間でも下り区間でも上へ膨らむ**こと。
11. **立ち絵グループの ScaleX レーン** (log) で、`直線` の区間が**曲線として**描かれること。
    再生して、線の高さと実際の拡大率が一致していること (= §3.3 (b) の意図した変化)。
12. **立ち絵グループの X レーン**で、点の値を画面の外 (負 / 1 超) まで振った区間が、
    **窓の下端 (上端) に張り付いてから立ち上がる**形で描かれること
    (= §3.3 (c)。旧実装は端どうしを直線で結んでいた)。
13. **Mute レーン**で Alt hover しても強調が出ず、Alt+ドラッグしても曲がらないこと
    (段の位置は右クリックメニューで変えられること)。
14. **オートメーション録音** (§2.7) をしてから、録音した点の区間を Alt+ドラッグして曲げ、
    **狙った区間だけ**が曲がること (= id 採番が効いている証拠)。保存 → 再読込しても同じ。
15. 既存プロジェクトを開いて `*` (dirty) が付かないこと (スキーマを変えていない証拠)。
    **オートメーション録音した曲を保存 → 再度開いた場合も `*` が付かないこと** —
    §3.1 のとおり保存 JSON に `id` / `next_point_id` が出るようになるが、
    `ensure_element_ids` は非 0 id を動かさないので load 結果は保存時と一致するはず。
    ここで `*` が付いたら §4.1 の採番が `ensure_element_ids` と食い違っている
    (= r.md #9 の dirty-on-open 契約違反) なので、先に直すこと。

---

## 7. 既存の流儀 (守ること)

- **doc コメントは日本語、密度は既存に合わせる。** 「なぜそうしたか」「何を踏んだか」を書く。
  新設する関数・型・フィールドには必ず doc コメントを付け、r.md #73 を明記する。
- 早期リターンは `let-else`、`?` を `match` より優先。
- `#[must_use]` を pure fn に付ける (既存の geometry.rs / draw.rs の流儀)。
- 数値キャストは `#[allow(clippy::cast_possible_truncation)]` を既存と同じ粒度で付ける。
- `ui.widget_state::<ArrangementState>(wid)` の借用中に `ui.push_edit` を呼ばない。
- **god file budget (3,000 行、`scripts/arch_lint.sh:284-290` のチェック 6)**。
  2026-08-28 実測と #73 後の見込み:

  | ファイル | 現在 | #73 後の見込み |
  |---|---|---|
  | `draw.rs` | 2,211 | 約 2,060 (`flatten_lane_*` 一式 約 155 行を削除) |
  | `geometry.rs` | 1,946 | 約 1,910 (1384-1526 の 約 145 行を削除 / `automation_segment_at` + id 版 lookup 約 110 行を追加) |
  | `mod.rs` | 2,413 | 約 2,450 (session 型 / id key / style 3 本を足し、handle 系を消す) |
  | `render.rs` | 861 | 約 750 (418-572 を削除、overlay 2 本を追加) |
  | `tests.rs` | 2,591 | 約 2,380 (曲線テスト 約 210 行を `tests_curve.rs` へ移設) |
  | `curve.rs` | — | 約 280 (新規) |
  | `tests_curve.rs` | — | 約 500 (新規。移設 約 210 + 新規 8 本) |

  **`tests.rs` を膨らませない**のが §5.1 の分割理由。「3,000 に近づいたら考える」ではなく、
  **書き始める前に `tests_curve.rs` を切る** (§5.1)。
  `daw_gui/tests/arr_widget.rs` はチェック 6 の対象外 (検査対象は `common/src` `daw_gui/src`
  `daw_audio/src` `daw_plugin_host/src` `ui/crates`、`arch_lint.sh:285`)。
- **アーキテクチャ不変条件 8** の回復として `ArrangementCurveKind` を削除する。
  `scripts/arch_lint.sh:299-302` の UI-DOMAIN 検査は `ui/crates/ui/src` 配下しか見ないので
  この mirror 型は検出されない (= baseline に載せる選択肢は無い)。機械検査に頼らず消すこと。
- **アーキテクチャ不変条件 5**: Song 編集は `edit_song_checked` (= `edit_song`
  チョークポイント) 経由のみ。`push_undo_snapshot` を直接呼ばない
  (そもそも撤去済み。`daw_gui/src/state/song_doc.rs:1-15`)。
- **RT 制約**: `common/src/automation.rs` に足すのは 2 つの `matches!` 述語だけで、
  RT 経路 (`apply_curve`) からは呼ばない。`apply_curve` の中身は変えない。
  `thin_collinear_and_insert` は GUI tick 経路 (`handler/tick.rs`) 専用で RT から呼ばれない。
  `curve.rs` の `Vec` 確保は GUI heavy 層のみ (既存 `flatten_lane_curve` と同じ)。

---

## 8. 判明したリスク / 注意点

1. **#77 との衝突。** #73 は `run.rs` の press / drag / release / render すべてに触る。
   #77 が main に入る前に着手してはいけない (§0)。
2. **描画が変わる 3 つのクラス** (§3.3 の表)。
   - (b) `GroupTransform::ScaleX` / `ScaleY` (log) と `TrackBuiltin::Mute` (階段) は
     lane 全体の見た目が変わる。
   - (c) 恒等写像 + `clamp(0,1)` の target (`GroupTransform::X` / `Y` / `AnchorX` / `AnchorY`、
     `TextBuiltin` の px 系と色、range 未取得の `PluginParam`) は
     **端点が表示窓の外に出ている区間だけ**変わる。
   - (a) それ以外は 1px も変わらない。
   **「affine なら不変」ではない** — `plain_to_norm_ranged` 末尾の
   `v.clamp(0.0, 1.0)` (`common/src/automation.rs:111`) が (c) を作る。
   どれも「今までの見た目」→「実際に鳴る形」への修正なので、実機確認で驚かないこと。
3. **log レーンでは曲げの可動域が狭い。** ScaleX (plain 0.1..10 を log 表示) では、
   plain 空間の `u^k` (k ∈ [0.5, 2]) で表せる形が log 表示上の可動域の一部にしかならない。
   これは daw_01 が **plain 空間で補間している**ことの帰結で、#73 のスコープ外
   (直すなら「補間を norm 空間で行う」= 音が変わる別件)。plain 描画にすることで
   初めてこの事実が目に見えるようになる。
4. **Hold 区間は press 直後に 1 度だけ線が飛ぶ。** hit-test は Hold の水平線に当たるが、
   anchor は変換後の直線 (`Exponential { bend: 0.0 }`) 上で取る。
   `k ∈ [0.5, 2]` のどの曲線も `u<1` で値 `a` を通らないので**連続な解が存在しない**。
   「Hold を掴んだら曲線へ自動変換」は確定方針そのものなので、これは仕様。
   飛んだ後は指に付いてくる (§3.4 の例外 1)。
5. **到達可能な範囲 (飽和) がある。** `bend` / `tension` の値域 `-1.0..=1.0` は
   `AutomationCurve` 自身の宣言 (`common/src/model/automation.rs:159-169`)。
   `Exponential` は `grab_u` で `w ∈ [grab_u², √grab_u]` にしか到達できず、
   区間の端に近い場所を掴むほど帯が狭い (u0=0.9 で区間高さの約 14%)。
   `Bezier` は `|D| ≤ 0.0962` なので元から幅が小さい。
   到達不能な目標では clamp された端で止まる = **線が指から離れる**。
   数学的性質でありバグではないが、実機で「途中から効かない」と誤解される可能性がある
   (§3.4 の例外 2、テストで固定する)。
6. **S 字は中点で曲げられない。** `Bezier` は u=0.5 を必ず通る (固定点) ので、
   区間の中点付近を掴むと `|D| < 1e-6` で no-op になる。中点から離れた場所を掴めば動く。
   中点のわずかに外側では利得が非常に大きく、小さなドラッグで `±1` に飽和する。
7. **Mute レーンは曲げられない。** 表示写像が階段で逆写像を持たないため、
   hover 強調も出さず、bend session も起動しない。カーブ種別は右クリックメニューから
   選べる (段が立つ位置が変わるので意味はある)。
   実装時に `norm_mapping_is_invertible` を素通りさせないこと。
8. **Alt+ダブルクリックの守備範囲が変わる。** 線から 6px 以内では「直線に戻す」が優先され、
   そこに**スナップ無しで点を足すことはできなくなる**。6px 離れれば従来どおり。
   これは確定方針で承認済み。
9. **`!alt` ゲートを 3 つ外すことの副作用** (§3.6)。automation clip press / lasso / marquee が
   Alt でも起動するようになる。**`no_session` 列挙への `automation_segment_bend` 追加を
   忘れると、線の上の Alt+drag で bend と lasso (or clip drag) が同フレームで両方起動する。**
   §4.7 / §4.12 の差し替え表を 1 行ずつ潰すこと。
10. **positional addressing が残る。** #73 で安定 id 化するのは `SetAutomationCurve` 経路のみ。
    `SelectAutomationPoints` / `MoveAutomationPoints` / `DeleteAutomationPoints` /
    `QuantizeSelectedAutomationPoints` と `app_types.rs` の `AutomationPointKeyRef` は
    positional (`point_idx`) のまま。`handler/automation.rs:450-452` の
    「point_idx は positional なので削除で全 index がずれる」というコメントが示すとおり、
    ここは別途の課題として残る (不変条件 1 の未回収分)。
    **#73 でこれを一緒に直そうとしないこと** — 確定方針が明示的にスコープを切っている。
    ただし `thin_collinear_and_insert` の id 採番 (§2.7) は**別件ではない** —
    それが無いと #73 の id addressing 自体が成立しないので、#73 の内側で塞ぐ。
11. **`daw_gui/src/clipboard.rs` は触らない。** `CopiedPoint.curve: AutomationCurve` が
    OS クリップボードへ JSON で出るが、型が変わらないので互換は保たれる。
    `CLIPBOARD_MAGIC` を上げると**既にコピー済みの点が貼れなくなる**ので上げないこと。
12. **`ArrangementResponse.hovered_automation_segment` を heavy cache キーに混ぜない。**
    マウスを動かすたびにアレンジ全体が再構築される (`hovered_clip` の doc と同じ罠)。
    強調は overlay 層に描く。
    一方 **`lane.target` / `lane.plugin_range` は cache キーに入れる**
    (`fold_arrangement_clip_hash`、§4.2) — 曲線の形がこの 2 つに依存するようになるので、
    入れないと plugin param の range が後から埋まったときに古い形が残る。
13. **adaptive de Casteljau の廃止。** `flatten_lane_cubic` は「y が制御点の 1 次式」を
    前提にした平坦化なので、非 affine な表示写像や clamp 飽和を通すと成立しない。
    uniform sampling (16 段以上、`dx / max_segment_px` 段、512 で cap) に統一する。
    (a) クラスのレーンでの見た目の差はサンプル間隔以下 (2px 未満) に収まる。
    `Linear` は `segment_is_straight_on_screen` が true のときだけ 2 点で済ませる。
14. **`AppEvent::SetAutomationCurve` から `prev` を落とす。** undo は snapshot 方式なので
    誰も読まない (§4.13)。旧 3 event の `prev` は `app.rs:880-907` で捨てられていた。
    「Undo 構築用」と書いてある doc をそのまま写さないこと。
15. **保存 JSON に `id` / `next_point_id` が出るようになる** (§3.1)。スキーマは不変で
    `CURRENT_VERSION` bump も不要だが、**「保存形式が 1 byte も変わらない」ではない**。
    実機確認 §6-15 で「録音 → 保存 → 再読込で `*` が付かない」を必ず見ること。
16. **`hovered_automation_segment` は response に足すだけでは描画に届かない** (§4.10 / §4.11)。
    `render_arrangement_heavy` は response を受け取らないので、`hovered_clip` と同じく
    明示的な引数 (#77 後は `HeavyInput` のフィールド) として渡す配線が要る。
    **配線を忘れると「Alt を押しても強調が出ない」だけで、コンパイルは通ってしまう。**
17. **レーンヘッダ列の Alt+ドラッグは無反応になる** (§3.6)。現状はここでも lane resize が
    起きているので、実機では「変化」として見える。無修飾 drag も元から無反応なので
    Alt の死角ではないが、実機確認 §6-7 で意図した変化であることを目視すること。
18. **`PressClaim.point` の seed を落とさない** (§3.5)。#73 は `point` を**立てる条件**に
    「今フレームの点の当たり」を足すのであって、`from_live` の
    `automation_point_drag.is_some()` seed を置き換えるのではない。
    置き換えると #77 §6-B:517-521 の等価性の根拠が崩れる。

---

## 9. 裏取りへの回答 (指摘のうち、実コードと食い違っていたもの)

本計画の改訂にあたり裏取りの指摘を 1 件ずつ実コードで確認した。次の 2 件は**指摘のほうが
実態と違っていた**ので、根拠を残す。

1. **「`thin_collinear_and_insert` は現状 caller が 0 件なので実害は無い」→ 偽。**
   `daw_gui/src/handler/tick.rs:648` (`insert_recording_point`) が呼んでおり、
   その caller は `tick.rs:236` の**オートメーション録音の live tick**。
   つまり録音した点はセッション中ずっと `id == 0` で、#73 の id addressing を直接壊す。
   「影響小」ではなく **#73 の前提条件**なので、§2.7 / §3.7 / §4.1 / §4.18 で塞ぐ。
2. **「REAPER を根拠として書かないこと (一次情報が未確認)」→ 解消済み。**
   その caveat は調査セッションで PDF を取得できなかった時点のもの
   (`scratchpad/item_73.md` の open risk)。その後 `scratchpad/curve_research.md:211-216` に
   REAPER User Guide §18.18 p.368 の mouse modifier 表が逐語で取り込まれ、
   同ファイル :317 / :321 が「§18.18(p.368) の表と散文まで逐語一致・捏造なし」として
   検証を記録している (`scratchpad/reaper.pdf` も取得済み)。
   ただし本計画では**ジェスチャ設計の一次根拠を Ableton / Bitwig の逐語引用に置き**、
   REAPER は「同じ配置である」という傍証としてのみ扱う (コード中のコメントでも
   「REAPER と一致」のような断定を根拠にしない — §4.7 / §4.12 のコメント案から削除済み)。

その他の指摘 (コンパイル不能 2 件 / 機能バグ 1 件 / 事実誤り 4 件 / 密度不足 2 件 /
未列挙リスク 3 件 / 行番号ずれ) はすべて正しく、本文に反映済み。

### 9.1 2 回目の裏取り (2026-08-28) — 反映と、指摘が誤りだったもの

反映したもの (各節に織り込み済。ここは索引):

| 指摘 | 反映先 |
|---|---|
| `hovered_automation_segment` を heavy へ渡す手当てが無い | §0 (対応表 + 型一覧) / §4.2 / §4.10 / §4.11 / §8-16 |
| Alt+drag resize の削除範囲は 834-921 ではなく **835-919** (921 は lasso のコメント) | §2.4 / §3.6 / §4.7 |
| `fold_arrangement_clip_hash` は 2325-2400 ではなく **2228-2401** (point ループ 2376-2396 / lane ループ 2343-2398) | §4.2 |
| `flatten_lane_segment` の呼び出し元は tests.rs の 7 本だけではない (`render.rs:518` / `draw.rs:1922`) | §4.4 (全件表) |
| §2.7 の `id: 0` 全件列挙に漏れ (metronome / audio_clip_renderer / 自ファイルの test / model/tests.rs / `impl Default for AutomationPoint`) | §2.7 (全件表) |
| 「lane header 列の drag は元から何も起こさない」は無修飾 drag の話で、Alt+drag は現状 resize している | §3.6 (訂正表) / §6-7 / §8-17 |
| `PressClaim.point` の seed をどうするか未決 | §3.5 (seed 据え置き + OR。#77 の等価性を壊さない) / §8-18 |
| `alt_drag_off_the_line_moves_the_clip_without_snapping` はハーネスが snap を切っているので空振りする | §5.2 (テスト doc に手順を明記) |
| `thin_collinear_and_insert` のテストは assert も書き換わる | §4.1 (対象行を列挙。**期待値は変えない**と再定義) |
| 保存 JSON に `id` / `next_point_id` が出るようになる点が未記載 | §3.1 (新節) / §6-15 / §8-15 |
| `ArrangementAutomationLane` の doc (mod.rs:473-474) が「target を持たず」と書いている | §4.2 (doc 全文の差し替え) |
| 行番号のずれ (多数) | §2.1 / §2.4 / §2.5 / §2.6 / §4.1 / §4.2 / §4.4 / §4.5 / §4.6 / §4.7 / §4.8 / §4.9 / §4.12 / §4.13 / §4.15 / §4.16 / §4.18 / §5.1 / §5.2 / §5.3 で実ファイルと照合して修正 |
| `tests.rs` の予算 (2,591 行) が 3,000 に近づく | §5.1 / §7 — **条件付きではなく、着手前に `tests_curve.rs` を切ると確定** |

指摘のほうが実態と違っていたもの:

1. **「§2.7 の列挙に `daw_audio/src/automation.rs:277/283/570/576/682/688` が抜けている」→ 偽。**
   同ファイルの `AutomationPoint` リテラルは 6 か所とも **`id: 1` / `id: 2`** を入れており
   (`daw_audio/src/automation.rs:278` / `:284` / `:571` / `:577` など)、`id: 0` を作っていない。
   したがって「`id: 0` を作る箇所」の列挙に載せるのは誤り
   (なお 6 か所すべて `#[cfg(test)]` (`:257`) の中でもある)。
   §2.7 の表には「`id: 0` 経路ではない」ことを明記して残した — 次に同じ grep をした人が
   もう一度迷わないように。
2. **「§5.2 のテスト名か手順のどちらかを直す」→ 手順を直し、名前は変えない。**
   検証したい性質は「Alt+drag が死角にならず、しかもスナップ無効として働く」ことなので、
   名前 (`..._moves_the_clip_without_snapping`) は正しい。壊れていたのは
   「ハーネスが最初からスナップを切っている」という前提の見落としだけなので、
   テスト側で `arrange_snap_enabled = true` に戻してから比較する手順を doc に書いた (§5.2)。
