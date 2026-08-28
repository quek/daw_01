# r.md #77 — `arrangement/run.rs` の分割 (実装計画)

**この計画は #77 専用であり、他項目との統合順は `docs/plan_rmd_index.md` を見ること。**

対象: `daw_gui/src/widgets/arrangement/run.rs` (2,699 行、7〜2,699 行が `pub fn arrangement` 1 本、
トップレベル項目は `use super::*` (`run.rs:4`) と `pub fn arrangement` (`run.rs:7`) の 2 つだけ)。

この計画は**それ単体で完走できる密度**で書いてある。実装者は再調査を必要としない。
行番号はすべて **2026-08-28 時点の main** (`run.rs` = 2,699 行 / `render.rs` = 861 行 /
`release.rs` = 1,335 行 / `mod.rs` = 2,413 行) で実測したもの。

---

## 0. ゴールと完了条件

`arrangement()` を **フェーズ軸で分割**し、フレーム不変の地形を束ねた `ArrangementFrame` を全フェーズが
`&ArrangementFrame` で受ける形にする。**フェーズ分けをせず、下の完了条件を 1 回で全部満たす。**

完了条件 (すべて必須):

1. `run.rs` が「フェーズを順に呼ぶだけの `pub fn arrangement`」だけになる (目安 70 行)。
2. `render.rs:5` と `release.rs:5` の `#![allow(clippy::too_many_arguments)]` を**削除**しても
   `make clippy` が通る (= `render_arrangement_heavy` の 37 引数 / `commit_releases` の 33 引数が
   4 引数に落ちている)。**この allow を消せることが完了条件。**
3. `run.rs:6` の `#[allow(clippy::too_many_lines)]` を削除する。
   **これは clippy のゲートではない。** `daw_gui/Cargo.toml` に `[lints]` セクションが無く
   workspace lints を opt-in していない (`Cargo.toml:124-125` が「daw_01 既存 crate は opt-in
   しないので pedantic は適用されない」と明記)。`clippy::too_many_lines` は pedantic なので
   この allow は最初から無効で、削除は「効いていない抑制を消す」だけの cosmetic な作業。
   **対して完了条件 2 は本物のゲート**: `clippy::too_many_arguments` は complexity (default warn) で、
   `make clippy` = `cargo clippy --workspace --all-targets -- -D warnings` (`Makefile:168-169`)
   なので、allow を消せば 8 引数以上で必ず落ちる。
   この非対称の帰結として、**新しく作る長い関数 (`press_lanes::automation` ≈360 行 /
   `header::draw_rows` ≈400 行 / `drag::update_sessions` ≈170 行) を clippy は 1 つも止めない**。
   関数の長さは §4 の行数見積りと §6 の分割粒度で守る (linter を当てにしない)。
4. 分割前後で **描かれた `Scene.primitives` の順序つきダンプ / 適用後の `AppData` / 返す
   `ArrangementResponse` / 要求されたカーソル が 1 byte も変わらない** (§9-A の等価性トランスクリプト)。
   **`header_w = 0` と `header_w = 160` の 2 パスで採る** (§9-A)。
5. §9-B の恒久テストが追加され、`cargo test -p daw_gui --test arr_widget` /
   `cargo test -p daw_gui --test arrange_fit_layout` が green。
6. `make check` / `make clippy` / `make test-nolaunch` / `make arch-lint` が green。
7. 実機 sign-off (§10)。

### やらないこと (スコープ外・この計画に含めない)

- **ドラッグを離した瞬間の「最後の 1px」判定規則そのものの統一。** 現行は対象ごとに 4 通りに割れている
  (§6-E2)。**判定を「どの軸を見るか」を引数にした 1 つの関数へまとめる**が、**どの対象がどの軸を見るかは
  1 つも変えない**。規則自体の統一はユーザーに見える挙動が変わるので別件。
- **`daw_gui/src/widgets/piano_roll/run.rs` (物理 2,177 行 / 実コード 1,633 行、
  `piano_roll` 関数本体は実コード 1,389 行) の分割。**
  今回は触らない。**r.md #76 で関数長の機械検査は既に入っており** (2026-08-28)、
  `scripts/arch_lint_baseline.txt` には `FILE-BUDGET | …/piano_roll/run.rs | 1633` /
  `FN-BUDGET | …::piano_roll | 1389` / `FN-NESTING | …::piano_roll | 9/296` が
  「r.md #77 系の分割で解消」の理由つきで **登録済み**。#77 側で新規登録する作業は無い。
- `release.rs` が抱えている wheel / double-click / secondary-click / marquee の切り出し。
  今回は `commit_releases` の**署名だけ**縮める。
- 性能を目的とした変更。描画結果が変わる最適化 (カリング条件・cache key の材料変更) は入れない。
  §7 の重複除去は「同じ値を 2 か所で持たない」の帰結として付いてくるだけ。

---

## 1. 最初にやること (Step 0) — `'static` 前提の単独検証

`run.rs` の 3 か所のコメントが `ui.heavy` / `hctx.cached` のクロージャに `'static` を要求すると書いている:

- `run.rs:1924` 「`'static` 制約で外側 borrow を持ち込めないため」
- `run.rs:1929` 「heavy closure は `'static` 要求なので owned Vec<u32> で渡す」
- `run.rs:2066` 「heavy() closure は `'static` 要求なので id を hash 化して move capture」
- `render.rs:57-58` 「'static borrow が必要なため caller scope の tops_for_draw は持ち込めない」

**これは誤り。**実際の署名にライフタイム境界は無い:

```rust
// ui/crates/ui/src/widgets/heavy.rs:53-56
pub fn heavy<F>(&mut self, id: impl Hash, f: F)
where F: for<'b> FnOnce(&mut HeavyCtx<'b, 'a, M>)

// ui/crates/ui/src/widgets/heavy.rs:76-79
pub fn cached<K, F>(&mut self, viewport_key: K, draw_fn: F)
where K: Hash, F: for<'c> FnOnce(&mut HeavyCtx<'c, 'a, M>)
```

`cached` が内部で呼ぶ `Ui::with_widget_node` (`ui.rs:1570` 付近) も `F: FnOnce(&mut Self)` で `'static` 無し。

**ただし HRTB (`for<'b>`) と借用キャプチャの相互作用は、実際にコンパイルするまで断定しない。**
実装の**最初に**この 1 点だけを単独で検証する:

1. `run.rs:2095-2097` の `ui.heavy(...)` から `move` を外し、`tracks_owned` の代わりに
   `&visible_tracks` を、`sections_for_draw` の代わりに `sections` を借用キャプチャする形に**だけ**書き換える。
2. `cargo check -p daw_gui` を通す。
3. 通れば以降の設計 (`&ArrangementFrame` を heavy に持ち込む) をそのまま進める。
4. **通らなかった場合のみ**、`ArrangementFrame` とは別に heavy 専用の owned 版
   (`HeavyFrame` = `visible_tracks: Arc<[ArrangementTrack]>` / `tops: Arc<[f32]>` / `sections: Arc<[SectionView]>`
   を持つ) を作り、`render::dispatch` が `ArrangementFrame` から 1 度だけ組む。この場合も
   `render_arrangement_heavy` の引数は 4 に落ちる (完了条件 2 は満たせる)。

### 混同してはいけないこと: この関数に残る本物の `'static` は `Edit<M>` 側

`Edit<M> = Box<dyn FnOnce(&mut M) + Send + 'static>` (`ui/crates/ui` の `Edit` 定義) なので、
`Edit::mutate` に渡すクロージャは今後も owned capture が必要。press / header フェーズの
owned capture (`run.rs:583` / `:593` / `:602` の `press_lane_button`、`run.rs:688` の
`press_delete_point`、`run.rs:2493-2496` の `let visible_ids: Vec<u32> = ... .collect();`) は
**この由来**であって heavy の `'static` 誤解とは無関係。「'static の名残」と誤認して消すと壊れる。

裏取り済みの事実として、**`render.rs` は `push_edit` / `Edit::` を 1 件も含まない** (grep 0 件)。
つまり heavy 側の clone 群は全部 borrow に落とせる。

---

## 2. 壊しやすい不変条件 (実装中ずっと意識する)

### 不変条件 1: `Scene.primitives` の push 順 = z 順

`daw_gui/tests/arr_widget.rs:95-97` のコメントと `dragged_section_band_is_drawn_in_front`
(`arr_widget.rs:527-563`、`#[test]` が 527 / `fn` が 528) が実際にこれを assert している。

**`ui.heavy` (`run.rs:2095`) → `release::commit_releases` (`run.rs:2099`) → track header ループ
(`run.rs:2101-2461`) の順を 1 か所でも入れ替えると視覚 regression になる。**
しかも build / test / clippy を全部すり抜ける (CLAUDE.md「Visual regression smoke test」の系譜)。
§5 の `run.rs` 最終形でフェーズ順を 1 画面に固定し、§9-B の 3 番 (描画順テスト) で機械的に止める。

### 不変条件 2: press の優先順位が非局所的

`splitter_press` (`run.rs:168` で束縛) は以降 **9 か所**のゲートに使われる:
`run.rs:237` (audio grip) / `:286` (clip) / `:337` (arranger) / `:384` (ruler) / `:633` (curve handle) /
`:669` (point) / `:774` (automation clip) / `:845` (Alt+drag fallback) / `:932` (lasso)。
`run.rs:219` / `:336` / `:386` は同名がコメント中に出るだけでゲートではない (実測)。

`no_session` ブロック (`run.rs:848-861` と `run.rs:935-948`) は **11 session の `is_none()` を列挙**した
コピペで、`section_drag` / `header_resize_drag` / `automation_lasso_drag` を**含まない**。
含まなくて正しい理由は**コードに書かれていない**:

- `section_drag`: arranger 帯 (`arranger_rect`) は `lanes` / `header_pane` と y 領域が排他なので、
  Alt+drag フォールバックのゲート
  `in_arr = in_lanes || (header_w > 0.0 && header_pane.contains(px, py))` (`run.rs:841`) が
  そもそも false になる。lasso も `in_lanes` ゲートで同様。
- `header_resize_drag`: 起動時に必ず `splitter_press == true` になる (`run.rs:215-227`) ので、
  両フォールバックの `!splitter_press` ゲートで先に弾かれる。
- `automation_lasso_drag`: 2 か所とも lasso ブロック (`run.rs:929-980`) より**前**にあるので、
  この時点では起動し得ない。

§6-B の `PressClaim` はこの 3 つの理由を**doc コメント 1 か所**に集約する。**列挙の中身 (11 種) は変えない。**

### 不変条件 3: `widget_state` の借用と `push_edit` の排他

`Ui::widget_state` (`ui.rs:2588`) は `&mut Ui` を占有して `&mut S` を返すので、その参照を持ったまま
`ui.push_edit` (`ui.rs:1550`、`pub`) を呼べない。press の 4 個の遅延スロット (`run.rs:129`/`:132`/
`:135`/`:137`) と per-frame 発火の `emit` 一時変数 (`run.rs:1271-1298` / `:1300-1326` / `:1328-1350` /
`:1352-1376` / `:1378-1398`) は**すべてこの制約への対処**。

**発火タイミングを 1 フレームでも遅らせないこと。** 借用を避けようとして次フレームに倒すと
「ドラッグが 1 フレーム遅れて追従する」体感劣化になる。ブロックで borrow を閉じてから同フレーム内で
emit する現行の形を、関数境界で表現し直す (§6-B の `PressActions`、§6-E4 の `emit_*`)。

### 不変条件 4: フェーズを跨ぐ暗黙の状態は `ArrangementFrame` に入れない

以下は `ArrangementState` / `ArrangementResponse` 経由のままにし、**各フェーズ fn の doc に
「誰が書いて誰が読むか」を明記する**:

| 状態 | 書く場所 | 読む場所 |
|---|---|---|
| `state.press_modifiers` | press の冒頭 (`run.rs:147-150`) | header の選択確定 (`run.rs:2480-2483`) |
| `state.edge_scroll_press` | 端スクロール (書き込みは `run.rs:1193` の 1 行、囲みブロックが `1190-1198`) | 同ブロック内のみ (`run.rs:1196`) |
| `response.hovered_clip` | hover (`run.rs:1693`) | heavy (`run.rs:2094` → `render.rs` の hovered_clip) |
| `response.dragging` / `reordering` / `dragging_track_volume` / `dragging_section` | hover 末尾 (`run.rs:1736-1745`) | cursor (`run.rs:1766-1790`) |

`press_modifiers` を release フレームの `pointer.modifiers` 生読みに戻してはいけない
(ModifiersChanged が MouseInput(Released) より先に届く race で Ctrl/Shift+click が Single に化ける)。

### 不変条件 5: `response.dragging_automation_clip` の位置

`run.rs:2696` は **rect 収集の後**、関数の最後で `automation_clip_drag_session` (non-Copy) を消費して
代入される。`run.rs:1947-1948` のコメントが「原本は後段の `dragging_automation_clip` で kind を取り出すまで
生かす」と意図を明記している (コメント中の「9528 付近」は旧 `app.rs` 時代の行番号で、現在の
`run.rs:2696` を指す)。**`rects::collect` の最後の文として置く** (§6-I)。
`LiveSessions` が session を所有するので non-Copy 制約は解消するが、`response` への書き込み順を
1:1 に保つため位置は動かさない。

### 不変条件 6: cursor の 2 つの state 読みは release take の**後**でなければならない

`run.rs:1756-1759` の `resize_active` と `run.rs:1762-1765` の `header_resize_active` は
`state.automation_lane_resize_drag` / `track_row_resize_drag` / `header_resize_drag` を直接読む。
これらは release take (`run.rs:1573` / `:1583` / `:1590`) より**後**の読みなので、
release フレームでは `None` になる。
**`LiveSessions` に取り込まず、`cursor::apply` 内で `ui.widget_state` を読む現行の形を維持する。**

---

## 3. 新設する型

### 3-1. `ArrangementFrame` (frame.rs)

「このフレームの不変な地形」。全フェーズが `&ArrangementFrame` で読む。**誰も書かない。**

```rust
/// 1 フレーム分の「地形」。 `arrangement()` の各フェーズが共有する読み取り専用の束。
///
/// 旧実装ではこれが 20 個以上のローカル変数として 2,000 行を跨いで生存し、 フェーズを
/// 切り出すたびに引数が増えていた (`render.rs` 37 引数 / `release.rs` 33 引数)。
///
/// **可変な状態は入れない。** フェーズを跨ぐ可変状態は `ArrangementState` (widget state) と
/// `ArrangementResponse` が持つ (不変条件 4)。
pub(super) struct ArrangementFrame<'a> {
    // ---- 入力ビュー (`view_build::build` の結果からの借用) ----
    /// caller の全 track list (visible filter 前)。 `is_group_set` 生成と `resolve_track_drop` が使う。
    pub tracks: &'a [ArrangementTrack],
    pub sections: &'a [SectionView],
    pub view: ArrangementView,          // Copy
    pub style: &'a ArrangementStyle,
    /// `Option` を落とさないこと — `commit_releases` が `Option<&ArrangementMasterRow>` で受ける。
    pub master_row: Option<&'a ArrangementMasterRow>,
    pub selected_clips: &'a [ClipKey],
    pub selected_tracks: &'a [u32],
    pub selected_automation_clips: &'a [AutomationClipKey],
    pub selected_automation_points: &'a [AutomationPointKey],

    // ---- rect 分割 (run.rs:24-59 と 1:1) ----
    pub rect: Rect,                     // = caller の area
    pub header_pane: Rect,
    pub ruler: Rect,
    pub arranger_rect: Rect,
    pub arranger_header_rect: Rect,
    pub lanes: Rect,
    pub header_w: f32,
    pub arranger_lane_h: f32,
    pub lanes_h: f32,

    // ---- 尺度 ----
    pub beat_per_px: f64,
    pub zoom_x_px_per_beat: f32,

    // ---- 行モデル ----
    /// collapsed 親配下を除外し、 先頭に synthetic master row を prepend した描画順の行。
    /// **描画 / hit-test / release / rect 収集がすべてこの 1 本を共有する** (旧 `visible_tracks` /
    /// `tracks_for_draw` / `tracks_owned` の 3 つ名は同一内容だった)。
    pub visible_tracks: Vec<ArrangementTrack>,
    /// `visible_tracks` の prefix-sum row top。 **`header_pane.y == lanes.y` は rect 分割
    /// (`run.rs:34-49`) から自明に成り立つので、 header 側と lanes 側で同一。**
    /// 旧 `press_tops` / `header_tops` / `tops_owned_for_heavy` の 3 重計算をこの 1 本に統合。
    pub tops: Vec<f32>,
    /// 「他 track の parent_id として参照されている id」 の集合 (= group 判定)。
    /// **caller の full `tracks` から作る** — `visible_tracks` から作ると collapsed で子が
    /// filter され group 判定が false 化する。
    pub is_group_set: HashSet<u32>,

    // ---- widget identity / 入力 ----
    pub wid: WidgetId,
    pub id: &'static str,
    /// このフレームの pointer スナップショット (`PointerFrame` は Copy、 `ui` を借りない)。
    pub pointer: PointerFrame,
}
```

### 3-2. `PressHit` / `PressClaim` / `PressActions` (press.rs) — §6-B 参照

### 3-3. `LiveSessions` / `ReleasedSessions` (sessions.rs) — §6-F 参照

### 3-4. `Overlays` (sessions.rs) / `HeavyInput` (render.rs) — §6-F / §6-J 参照

### 3-5. `RewindAxes` (drag.rs) — §6-E2 参照

### 3-6. `HeaderClicks` (header.rs) — §6-H 参照

---

## 4. ファイル構成

既存の流儀 (`use super::*` で親の可視名を継承 / `pub(super) fn` / 日本語 doc コメント /
`M14 Phase NN (daw_01 #NN)` の由来注記を残す) に**そのまま**合わせる。新しい流儀は導入しない。

| ファイル | 状態 | 由来行 (run.rs) | 目安 |
|---|---|---|---|
| `frame.rs` | 新規 | 9-123 | ~210 |
| `press.rs` | 新規 | 125-230, 334-450, 983-998 | ~340 |
| `press_lanes.rs` | 新規 | 232-333, 619-980 | ~600 |
| `press_header.rs` | 新規 | 451-617 | ~190 |
| `drag.rs` | 新規 | 1008-1399 | ~430 |
| `sessions.rs` | 新規 | 1400-1683 | ~340 |
| `cursor.rs` | 新規 | 1684-1849 | ~200 |
| `header.rs` | 新規 | 2101-2501 | ~440 |
| `rects.rs` | 新規 | 1000-1006 (コメントのみ), 2503-2696 | ~230 |
| `render.rs` | 改 | + 1851-2097 | 861 → ~1,060 |
| `release.rs` | 改 | — | 1,335 (署名のみ) |
| `run.rs` | 残 | — | **~70** |

この表と §6 で `run.rs` 2,699 行を**穴なく被覆する**。`run.rs:1000-1006` (右クリック context menu は
caller 責務 / 旧設計が popup anchor を 1 フレームで壊した理由 / #028 §11.4 で確定した idiom) は
**`rects.rs` のモジュール doc へ移す** — この段落が説明しているのは「widget は
`response.automation_point_rects` を毎フレーム返し、caller が anchor を毎フレーム呼ぶ」という
`rects.rs` の存在理由そのものだから。§6-A と同じく**非局所的な理由を書いた記録は 1 行も落とさない**。

全ファイルがサイズ budget (実コード 1,000 行 / 関数 300 行 / インデント 6 段。
`scripts/loc_budget.py`) の内側であることを、分割後に
`python scripts/loc_budget.py --report` で確認する。
**上表の分割後サイズ見積り (`render.rs` ~1,060 等) は物理行なので、新指標での値は未検証**。
特に `render.rs` は現在 物理 861 行 / **ファイルの実コード 721 行**
(そのうち `render_arrangement_heavy` 1 関数が 681 行) で、あと 279 行しか余裕が無いまま
`FILE-BUDGET` の baseline に**載っていない**。`run.rs` から 244 行 (`:1851-2094`) を
受け取ると **新規違反になり得る**。分割後に `--report` で確認し、超えるなら分割単位を切り直す。

`scripts/arch_lint_baseline.txt` には arrangement 関連の `FILE-BUDGET` / `FN-BUDGET` /
`FN-NESTING` が登録済み (r.md #76、2026-08-28)。内訳は `FILE-BUDGET` × 4
(`run.rs` 1946 / `draw.rs` 1565 / `geometry.rs` 1435 / `mod.rs` 1249)、`FN-BUDGET` × 3
(`run.rs::arrangement` 1944 / `release.rs::commit_releases` 962 /
`render.rs::render_arrangement_heavy` 681)、`FN-NESTING` × 10
(上の 3 本 + `view_build.rs::build` 10/67 + `build_arrangement_lanes_from_slice` /
`geometry.rs::automation_point_at` / `collect_points_in_rect` /
`content_build.rs::build_one` / `geometry.rs::find_curve_param_handle_at` /
`mod.rs::fold_arrangement_clip_hash`)。
分割で消えた行は「解消」として通知されるので削除し、残った関数の天井は実測値に更新する。
新しく生えたファイル / 関数が違反するなら、**分割単位を切り直す** (baseline を増やして
着地させない)。

> **着地後の実測 (2026-08-28、#76 を #77 の上に統合した時点)**:
> - 解消 3 件 — `FILE-BUDGET run.rs` (1946 → **19**) / `FN-BUDGET run.rs::arrangement`
>   (1944) / `FN-NESTING run.rs::arrangement` (11/520)。
> - `render.rs` は **上の懸念どおりにはならなかった** — 実コード 721 → **746** で
>   budget 1,000 の内側。`render_arrangement_heavy` も 681 → **627**、
>   ネストは 12/399 → **10/220** と大きく改善した。
> - 一方で **2 件が太った**: `arrangement/mod.rs` 1249 → **1261** (兄弟モジュール宣言と
>   glob re-export の 18 物理行。分割の必然)、`release.rs::commit_releases` 962 → **996**
>   で `release.rs` が 977 → **1003** と新たに budget を超えた。原因は §0 の
>   「`commit_releases` の**署名だけ**縮める」というスコープで、33 引数を 4 引数へ畳んだ
>   代わりに本体先頭へ旧引数名を束ね直す 33 行が入ったこと。**署名の複雑度は下がったが
>   実コード行は増えた**ので、wheel / double-click / secondary-click / marquee の切り出し
>   (同じく §0 でスコープ外とした項目) が残件として残る。
> - 切り出された `press_header.rs::lane_header` (7/14) と `rects.rs::push_point_rects`
>   (7/10) が FN-NESTING に新規登録。どちらも元は `arrangement()` の中で 11 段だった
>   部分なので、**7 段は改善の途中経過**。

### この計画で編集するファイル (完全な一覧)

| ファイル | 何をするか |
|---|---|
| `daw_gui/src/widgets/arrangement/frame.rs` | 新規 (§6-A) |
| `daw_gui/src/widgets/arrangement/press.rs` | 新規 (§6-B) |
| `daw_gui/src/widgets/arrangement/press_lanes.rs` | 新規 (§6-C) |
| `daw_gui/src/widgets/arrangement/press_header.rs` | 新規 (§6-D) |
| `daw_gui/src/widgets/arrangement/drag.rs` | 新規 (§6-E) |
| `daw_gui/src/widgets/arrangement/sessions.rs` | 新規 (§6-F) |
| `daw_gui/src/widgets/arrangement/cursor.rs` | 新規 (§6-G) |
| `daw_gui/src/widgets/arrangement/header.rs` | 新規 (§6-H) |
| `daw_gui/src/widgets/arrangement/rects.rs` | 新規 (§6-I) |
| `daw_gui/src/widgets/arrangement/render.rs` | 改 (§6-J、allow 削除 + 4 引数化) |
| `daw_gui/src/widgets/arrangement/release.rs` | 改 (§6-K、allow 削除 + 4 引数化) |
| `daw_gui/src/widgets/arrangement/run.rs` | 改 (§5、~70 行へ) |
| `daw_gui/src/widgets/arrangement/mod.rs` | 改 (下記 `mod` / `use` 宣言 + 作業中だけ `#[cfg(test)] mod equivalence;`) |
| `daw_gui/src/widgets/arrangement/draw.rs` | 改 (§6-H 3: `draw_lanes_bg` の恒等 `compute_visible_indices` 撤去) |
| `daw_gui/src/state/ui_prefs.rs` | 改 (`#[derive(Debug)]` 追加。§9-A) |
| `daw_gui/src/state/selection.rs` | 改 (`#[derive(Debug)]` 追加。§9-A) |
| `daw_gui/tests/arr_widget.rs` | 改 (§9-B の恒久テスト 3 種 + `header_w > 0` の fixture variant) |
| `daw_gui/src/widgets/arrangement/equivalence.rs` | 新規 → **作業完了後に削除** (§9-A の等価性トランスクリプト。crate 内 `#[cfg(test)]` に置く理由は §9-A) |

**触らない**: `daw_gui/src/widgets/arrangement/{view_build,content_build,geometry,tests}.rs`、
`daw_gui/src/view/arrangement_view.rs` (唯一の production caller、`arrangement()` の署名は不変)、
`daw_gui/tests/arrange_fit_layout.rs` (回帰確認に回すだけ)、`ui/crates/**`、`common/**`。

### `mod.rs` の変更 (`mod.rs:59-69`)

```rust
pub(crate) mod view_build;
mod content_build;
mod draw;
use draw::*;
mod geometry;
use geometry::*;
mod cursor;        // 追加
mod drag;          // 追加
mod frame;         // 追加
use frame::*;      // 追加 (ArrangementFrame)
mod header;        // 追加
mod press;         // 追加
use press::*;      // 追加 (PressHit / PressClaim / PressActions)
mod press_header;  // 追加
mod press_lanes;   // 追加
mod rects;         // 追加
mod sessions;      // 追加
use sessions::*;   // 追加 (LiveSessions / ReleasedSessions / Overlays)
mod render;        // 既存 (この順序のまま。実ファイルは render → release → run)
mod release;       // 既存
mod run;           // 既存
pub use geometry::{pixel_snapped_scroll_beat, view_len_beats};
pub use run::arrangement;
```

**既存 3 行 (`mod render; mod release; mod run;`) の順序を入れ替えないこと。** 実ファイル
(`mod.rs:65-67`) はこの順で並んでいる。アルファベット順に整えたくなるが、それは #77 と無関係な
行を差分に混ぜるだけ。新規 `mod` は `use geometry::*;` の直後にまとめて挿入する。

**`use <新モジュール>::*;` は必須。** 子ファイルは `use super::*;` しか書かない流儀なので、
兄弟モジュールで定義した型は「親 (`arrangement`) のスコープに名前が入っていること」で初めて見える。
これは既存の `use draw::*;` / `use geometry::*;` と同じ仕掛けで、実際 `run.rs` は
`use super::*;` だけで `geometry.rs:11` の `visible_track_row_tops` を呼べている
(`mod.rs:63-64` の `mod geometry; use geometry::*;` があるから)。これを書き忘れると
`ArrangementFrame` / `PressHit` / `PressClaim` / `PressActions` / `LiveSessions` /
`ReleasedSessions` / `Overlays` が §6 のどの署名からも名前解決しない。

**glob を足すのは「型を兄弟へ出すモジュール」だけ** (`frame` / `press` / `sessions`)。
`drag::RewindAxes` / `header::HeaderClicks` / `render::HeavyInput` はそれぞれのモジュール内で
閉じる (`run.rs` は `let clicks = header::draw_rows(..)` と型推論で受けるだけ) ので glob を足さない
— 足すと `unused_imports` が `-D warnings` で落ちる。

`#[cfg(test)] mod tests;` (`mod.rs:2412-2413`) はそのまま。`tests.rs` は `run` / `render` /
`release` を 1 件も参照しないので (grep 0 件) 分割の影響を受けない。

`ArrangementState` (`mod.rs:1900-1953`) は `pub(crate)` だがフィールドは private。子モジュールは
親モジュールの private 項目にアクセスできるので `state.clip_drag` 等はそのまま書ける
(現行 `run.rs` / `release.rs` と同じ)。

---

## 5. `run.rs` 最終形

```rust
//! arrangement widget の 1 フレームのパイプライン。 **フェーズを順に呼ぶだけ**で、
//! 個々のフェーズの中身は同ディレクトリの兄弟モジュールが持つ。
//!
//! **この並び順は不変条件**: `Scene.primitives` は push 順 = z 順なので
//! (`daw_gui/tests/arr_widget.rs:95-97`)、 `render::dispatch` → `release::commit_releases`
//! → `header::draw_rows` の順を入れ替えると視覚 regression になる (build / test / clippy を
//! すり抜ける種類の壊れ方)。 `arr_widget.rs` の描画順テストが機械的に止める。

use super::*;

pub fn arrangement(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> ArrangementResponse {
    // 入力ビューは `arrangement()` のスタックに置いたまま、 `frame` がそこから借りる。
    let built = view_build::build(app, area);
    let f = frame::build(&built, area, ui);
    let mut response =
        ArrangementResponse { ruler_rect: f.ruler, ..Default::default() };

    // 1. レイアウトを app にミラー (auto-fit / 縦ズーム用)。
    frame::mirror_layout(app, ui, &f, &mut response);
    // 2. press 振り分け (splitter → clip → arranger → ruler → header → automation)。
    press::dispatch(ui, &f);
    // 3. drag 継続 + 端オートスクロール + per-frame live 発火。
    drag::advance(ui, &f);
    // 4. session の overlay 用スナップショットと release take。
    let (live, released) = sessions::take(ui, &f, &mut response);
    let overlays = sessions::overlays(&f, &live, &released);
    // 5. hover 判定 → cursor 決定 (cursor は hover が書いた response を読む)。
    cursor::hover(&f, &live, &mut response);
    cursor::apply(ui, &f, &live, &response);
    // 6. heavy 描画 → release commit → track header 描画 (この 3 つの順が z 順)。
    render::dispatch(ui, app, &f, &live, &overlays, &response);
    release::commit_releases(ui, &f, &mut response, released);
    let clicks = header::draw_rows(ui, &f, &live, &mut response);
    header::commit_clicks(ui, &f, clicks, &mut response);
    // 7. caller 向け rect 群の収集。
    rects::collect(&f, &live, &mut response);

    response
}
```

**`#[allow(clippy::too_many_lines)]` (`run.rs:6`) は削除する。**

---

## 6. 各ファイルの詳細

### 6-A. `frame.rs` (新規)

由来: `run.rs:9-123`。

```rust
//! arrangement widget の 1 フレームの「地形」 (`ArrangementFrame`) の構築と、
//! レイアウトの `AppData` へのミラー。
//!
//! `ArrangementFrame` は読み取り専用で、 全フェーズが `&ArrangementFrame` で受ける。
//! 可変な状態は `ArrangementState` (widget state) と `ArrangementResponse` が持つ。

use super::*;
// `BuiltArrangement` は `view_build.rs:33` の `pub(super)`。 `mod.rs` は
// `pub(crate) mod view_build;` を宣言するだけで re-export していないので、 `use super::*` では
// 型名が入らない (現行 `run.rs` は `let built = view_build::build(app, area);` と型名を書かずに
// 済ませているので、 この問題は分割して初めて表面化する)。
use super::view_build::BuiltArrangement;

pub(super) struct ArrangementFrame<'a> { /* §3-1 */ }

/// `BuiltArrangement` + caller の `area` + `ui` の pointer スナップショットから 1 フレームの
/// 地形を組む (旧 `run.rs:9-100`)。
pub(super) fn build<'a>(
    built: &'a BuiltArrangement,
    area: Rect,
    ui: &Ui<'_, AppData>,
) -> ArrangementFrame<'a>;

/// r.md #63: auto-fit (`X` / Fit ボタン) と縦ズーム (`Z`) 用に、 このフレームの **実レイアウト** を
/// `app.ui_ephemeral` にミラーし、 `response.arranger_rect` / `lanes_rect` / `rows` を埋める
/// (旧 `run.rs:102-123`)。 差分があるときだけ `push_edit` する。
///
/// **lanes 高さを式で再導出しないこと** — `area.h - RULER_H` で再導出して Arranger 帯 18px を
/// 引き忘れたのが r.md #63 の症状 (`daw_gui/tests/arrange_fit_layout.rs` が回帰テスト)。
pub(super) fn mirror_layout(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
);
```

`build` の中身 (すべて `run.rs` からの移設、式は 1 文字も変えない):

1. `run.rs:10-22` の入力束縛 → フィールドへ。`wid = WidgetId::ROOT.child((b"arrangement_widget", &id))`、
   `id = "arrangement"`、`pointer = ui.pointer()`。
2. `run.rs:24-59` の rect 分割 (`header_w` / `ruler_h` / `arranger_lane_h` / `lanes_h` / `lanes_w` /
   `header_pane` / `ruler` / `arranger_rect` / `arranger_header_rect` / `lanes` / `beat_per_px` /
   `zoom_x_px_per_beat`)。`ruler_h` は `ruler` rect の中にしか現れないのでフィールドにしない。
3. `run.rs:67-100` の行モデル: `compute_visible_indices(tracks)` (`:72`) → clone (`:82-85`) →
   `master_row` があれば `synthesize_master_track(master)` を `insert(0, ..)` (`:86-88`) →
   `visible_track_row_tops(&visible_tracks, lanes.y, view.track_top, view.track_row_h)` (`:94-95`)
   → `is_group_set` (`:99-100`)。

`run.rs:61-65` の `response` 初期化 (`ruler_rect`) は `frame::build` ではなく `run.rs` に残る (§5)。

**注意**: `run.rs:27-30` / `:67-71` / `:73-81` / `:89-93` / `:96-98` の日本語コメント (arranger 帯の
y 原点と `header_pane.y == lanes.y` の不変条件、visible-domain row index の約束、synthetic master の
意図、prefix-sum tops が描画 / hit-test の SSoT である理由、`is_group_set` を full `tracks` から
作る理由) は**そのまま移設する**。これらは非局所的な理由を書いた唯一の記録。

---

### 6-B. `press.rs` (新規)

由来: `run.rs:125-230` (遅延スロット宣言 + press ガード + splitter)、`run.rs:334-450`
(arranger 帯 + ruler)、`run.rs:983-998` (遅延発火)。

#### 型

```rust
/// press フレームの座標と modifier のスナップショット (旧 `run.rs:141-144` のローカル群)。
#[derive(Clone, Copy)]
pub(super) struct PressHit {
    pub px: f32,
    pub py: f32,
    pub in_lanes: bool,
    pub in_ruler: bool,
    pub shift: bool,
    pub ctrl: bool,
}

/// 「この押下を誰が消費したか」。 旧実装が `ui.widget_state` を読み直して判定していた
/// 優先順位を、 **制御フローの値**として持ち回る。
///
/// 旧実装との対応 (挙動を 1:1 に保つための表):
/// - `splitter`      = 旧 `splitter_press` (`run.rs:168`)。 以降 **9 か所**の `!splitter_press`
///   ゲート (`run.rs:237` / `:286` / `:337` / `:384` / `:633` / `:669` / `:774` / `:845` / `:932`)。
/// - `curve_handle`  = 旧 `handle_press_started` (宣言 `run.rs:632`、 立てるのは `:659`)。
/// - `point`         = 旧 `already_taken_by_point` (`run.rs:762-765`)。
/// - `session`       = 旧 `no_session` (`run.rs:848-861` / `run.rs:935-948`) の否定。
///
/// **`session` の列挙は 11 種で、 `section_drag` / `header_resize_drag` /
/// `automation_lasso_drag` を意図的に含まない** (旧実装と同一)。 含まなくて正しい理由:
/// - `section_drag`: arranger 帯は `lanes` / `header_pane` と y 排他なので、 この bit を読む
///   2 つの fallback のゲート (`in_arr` (`run.rs:841`) / `in_lanes`) が先に false になる。
/// - `header_resize_drag`: 起動時に必ず `splitter` が立つので `!splitter` ゲートで弾かれる。
/// - `automation_lasso_drag`: この bit を読む 2 か所より後で起動する。
///
/// `session` / `point` は press 分岐に入る**前**の live session (前フレームからの残存を含む) で
/// seed し、 以降は 11 種のいずれかを起動した分岐が `= true` を立てる。 旧実装が各ゲートで
/// `widget_state` を読み直していたのと**厳密に等価**: press ブロック内で session を `None` に
/// 戻す箇所は 1 つも無い (`run.rs:138-981` に `state.*= None` / `.take()` は grep 0 件) ので、
/// 「seed + 単調に立てる」 と「毎回読み直す」 の結果は必ず一致する。
///
/// `splitter` / `curve_handle` は live seed **しない** (旧 `splitter_press` / `handle_press_started`
/// も `false` から始まるローカル)。
#[derive(Clone, Copy)]
pub(super) struct PressClaim {
    pub splitter: bool,
    pub curve_handle: bool,
    pub point: bool,
    pub session: bool,
}

impl PressClaim {
    /// press 分岐に入る前の live session から seed する
    /// (`splitter: false` / `curve_handle: false` / `point: automation_point_drag.is_some()` /
    /// `session:` 11 種のいずれかが `Some`)。 **旧 `no_session` の 11 列挙はここ 1 か所だけ。**
    pub(super) fn from_live(state: &ArrangementState) -> Self;
}

/// press ブロック内では `widget_state` の借用が走って `push_edit` を呼べないため、
/// 発行すべき `Edit` を貯めるスロット (旧 `run.rs:129` / `:132` / `:135` / `:137` の 4 変数)。
///
/// **発火は同一フレーム内**。 次フレームに倒すとドラッグが 1 フレーム遅れて追従する
/// 体感劣化になる (不変条件 3)。
#[derive(Default)]
pub(super) struct PressActions {
    pub seek_beat: Option<f64>,          // ruler plain click
    pub lane_toggle: Option<u32>,        // track 行右端の lane disclosure
    pub lane_button: Option<Edit<AppData>>, // lane header の ★/👁/✕
    pub delete_point: Option<Edit<AppData>>, // Alt+click on point
}

impl PressActions {
    /// 旧 `no_press_action` の否定 (`run.rs:862-865` / `:949-952`)。
    pub(super) fn any(&self) -> bool;
    /// 旧 `run.rs:983-998` と **同じ順** (seek → lane_toggle → lane_button → delete_point) で発行。
    /// `push_edit` の発行順が `Edit` の適用順を決めるので、 この順序は挙動そのもの。
    pub(super) fn emit(self, ui: &mut Ui<'_, AppData>);
}
```

#### 関数

```rust
/// press フレームの振り分け全体。 呼び出し順は旧 `run.rs:138-998` と 1:1
/// (splitter → clip zone → arranger → ruler → header → automation → 遅延発火)。
pub(super) fn dispatch(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    if !f.pointer.primary_just_pressed {
        return;
    }
    let Some((px, py)) = f.pointer.pos else { return };      // run.rs:138-140
    let hit = PressHit { px, py,
        in_lanes: f.lanes.contains(px, py), in_ruler: f.ruler.contains(px, py),
        shift: f.pointer.modifiers.shift, ctrl: f.pointer.modifiers.ctrl };  // run.rs:141-144
    snapshot_modifiers(ui, f);                                   // run.rs:147-150
    let mut claim = { let s: &ArrangementState = ui.widget_state(f.wid); PressClaim::from_live(s) };
    let mut actions = PressActions::default();
    splitter(ui, f, &hit, &mut claim);                           // run.rs:152-230
    press_lanes::clip_zone(ui, f, &hit, &mut claim);             // run.rs:232-333
    arranger(ui, f, &hit, &mut claim);                           // run.rs:334-383
    ruler(ui, f, &hit, &mut claim, &mut actions);                // run.rs:384-450
    press_header::dispatch(ui, f, &hit, &mut claim, &mut actions); // run.rs:451-617
    press_lanes::automation(ui, f, &hit, &mut claim, &mut actions); // run.rs:619-980
    actions.emit(ui);                                            // run.rs:983-998
}

/// release フレームで確定する click 系 (track header のトラック選択) 用に、
/// press 時の modifier を `ArrangementState.press_modifiers` に記録する (`run.rs:147-150`)。
/// 読むのは `header::commit_clicks` (`run.rs:2480-2483`)。 release フレームの
/// `pointer.modifiers` 生読みは ModifiersChanged 先行 race で Ctrl/Shift が落ちる。
fn snapshot_modifiers(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>);

/// lane 下端 → track 行下端 → header/lanes 境界 の 3 段 splitter 判定 (`run.rs:152-230`)。
/// **`claim.splitter` = 旧 `splitter_press`** で、 以降 9 か所のゲートになる。
/// lane / row resize を起動した枝は `claim.session` も立てる (下表)。
fn splitter(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, hit: &PressHit,
            claim: &mut PressClaim);

/// Arranger 帯の press (既存 section の Move/Resize、 空き帯は Create 範囲 drag) (`run.rs:334-383`)。
/// ゲートは `!claim.splitter && f.arranger_lane_h > 0.0 && f.arranger_rect.contains(..)` のみ
/// (arranger 帯は他ゾーンと y 排他)。 **`section_drag` は 11 列挙外なので `claim.session` は
/// 立てない。**
fn arranger(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, hit: &PressHit,
            claim: &mut PressClaim);

/// ruler の press。 Shift で loop 編集 (Start/End/Middle/NewRange)、 plain で playhead seek
/// session + `actions.seek_beat` (`run.rs:384-450`)。 `loop_drag` / `playhead_drag` は 11 列挙内
/// なので起動したら `claim.session = true`。
fn ruler(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, hit: &PressHit,
         claim: &mut PressClaim, actions: &mut PressActions);
```

**`claim` は全分岐に `&mut` で渡す。** 旧実装は `no_session` を読む地点 (`run.rs:848` / `:935`) で
`widget_state` を読み直しており、**同フレームでそれより前の分岐が起動した session も見えていた**。
これを値で持ち回るには、11 列挙のどれかを起動した分岐がその場で `claim.session = true` を立てる
必要がある。共有参照で渡すと立てられず、旧挙動を再現できない。

press フレームで session を起動する箇所は **17 か所** (`grep -n "state[.][a-z_]* = Some[(]" run.rs` の
125-1000 行 + `automation_lane_resize_drag` の 2 行 = `176` / `204` / `221` / `273` / `323` / `352` /
`370` / `429` / `444` / `510` / `549` / `649` / `738` / `822` / `887` / `909` / `973`)。
`automation_lane_resize_drag` は `= Some(` が改行で割れている (`run.rs:176-177` / `:887-888`) ので
1 行 grep では出ない — **見落とさないこと**。どれが `claim.session` を立てるかは
「11 列挙に入っているか」だけで決まる (実測):

| 起動行 (run.rs) | session | 担当 fn | `claim.session` |
|---|---|---|---|
| `176` | `automation_lane_resize_drag` | `press::splitter` | **立てる** |
| `204` | `track_row_resize_drag` | `press::splitter` | **立てる** |
| `221` | `header_resize_drag` | `press::splitter` | 立てない (11 列挙外) |
| `273` | `audio_drag` | `press_lanes::clip_zone` | **立てる** |
| `323` | `clip_drag` | `press_lanes::clip_zone` | **立てる** |
| `352` | `section_drag` (既存 section の Move / Resize) | `press::arranger` | 立てない (11 列挙外) |
| `370` | `section_drag` (空き帯の Create 範囲 drag) | `press::arranger` | 立てない (11 列挙外) |
| `429` | `loop_drag` | `press::ruler` | **立てる** |
| `444` | `playhead_drag` | `press::ruler` | **立てる** |
| `510` | `track_volume_drag` | `press_header::track_row` | **立てる** |
| `549` | `track_reorder` | `press_header::track_row` | **立てる** |
| `649` | `automation_curve_param_drag` | `press_lanes::curve_handle` | **立てる** |
| `738` | `automation_point_drag` | `press_lanes::point` | **立てる** (+ `claim.point`) |
| `822` | `automation_clip_drag` | `press_lanes::automation_clip` | **立てる** |
| `887` | `automation_lane_resize_drag` | `press_lanes::alt_resize` | **立てる** |
| `909` | `track_row_resize_drag` | `press_lanes::alt_resize` | **立てる** |
| `973` | `automation_lasso_drag` | `press_lanes::lasso` | 立てない (11 列挙外) |

11 列挙の中身は `track_volume_drag` / `track_reorder` / `audio_drag` / `clip_drag` /
`automation_point_drag` / `automation_clip_drag` / `automation_lane_resize_drag` /
`track_row_resize_drag` / `playhead_drag` / `loop_drag` / `automation_curve_param_drag`
(= `run.rs:850-860` と同じ順)。**11 種を 14 種に「直さない」** (退化ケースで挙動が変わる)。

なお `splitter` が立てる `claim.session` と `arranger` / `ruler` が立てる `claim.session` は、
`claim.session` を読む 2 か所 (`run.rs:866` / `:949`) のゲートが `!claim.splitter` と
`in_arr` / `in_lanes` で先に閉じるため、**現行コードでは結果に影響しない**。それでも立てるのは
「11 列挙に入っている session を起動したら立てる」という規則を 1 本にして、後から
ゲートの形が変わったときに静かに壊れないようにするため (旧実装の `widget_state` 読み直しと同義)。

---

### 6-C. `press_lanes.rs` (新規)

由来: `run.rs:232-333` (audio grip / clip)、`run.rs:619-980` (curve handle / point /
automation clip / Alt+drag フォールバック / lasso)。

```rust
//! lane 領域 (clip 描画域 = `f.lanes`) の press 振り分け。 ゾーンごとに排他な 7 本のチェーンで、
//! 優先順位は audio grip > clip > curve handle > automation point > automation clip >
//! Alt+drag resize > lasso。 各分岐は消費したことを `PressClaim` に立てて後続を止める。

use super::*;

/// audio grip (gain band / fade corner) → MIDI/Audio clip の Move/Resize (`run.rs:232-333`)。
/// audio grip が先勝したら clip drag は起動しない (`else if` の排他をそのまま維持)。
pub(super) fn clip_zone(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>,
                        hit: &PressHit, claim: &mut PressClaim);

/// automation 系 5 本 + Alt+drag フォールバック + lasso (`run.rs:619-980`)。
/// 内部で `curve_handle` → `point` → `automation_clip` → `alt_resize` → `lasso` を
/// **この順**で呼ぶ (優先順位そのもの)。
pub(super) fn automation(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>,
                         hit: &PressHit, claim: &mut PressClaim, actions: &mut PressActions);

fn curve_handle(..) ;      // run.rs:619-660 → claim.curve_handle (+ claim.session、起動行 649)
fn point(..) ;             // run.rs:662-753 → claim.point / claim.session / actions.delete_point
fn automation_clip(..) ;   // run.rs:755-833 → claim.session
fn alt_resize(..) ;        // run.rs:835-919 → claim.session
fn lasso(..) ;             // run.rs:921-980 → claim は立てない (automation_lasso_drag は 11 列挙外)
```

ゲートの書き換え表 (**左右が同値であることを 1 行ずつ確認しながら移す**。行番号は実測):

| 由来 | 旧 | 新 |
|---|---|---|
| `run.rs:237` | `let audio_press = if !splitter_press && in_lanes && !shift && !ctrl {` | `!claim.splitter && hit.in_lanes && !hit.shift && !hit.ctrl` |
| `run.rs:286` | `} else if !splitter_press && in_lanes && clip_hit(..)` | `} else if !claim.splitter && hit.in_lanes && clip_hit(..)` |
| `run.rs:633-636` | `!splitter_press && in_lanes && find_curve_param_handle_at(..)` | `!claim.splitter && hit.in_lanes && ..` |
| `run.rs:669-673` | `!splitter_press && !handle_press_started && in_lanes && ..` | `!claim.splitter && !claim.curve_handle && hit.in_lanes && ..` |
| `run.rs:762-765` | `already_taken_by_point = state.automation_point_drag.is_some()` | `claim.point` (seed + point 分岐が立てる) |
| `run.rs:774-779` | `!splitter_press && !already_taken_by_point && !handle_press_started && in_lanes && !alt && ..` | `!claim.splitter && !claim.point && !claim.curve_handle && hit.in_lanes && !f.pointer.modifiers.alt && ..` |
| `run.rs:841-866` | `in_arr` を `:841` で束縛 → `:842-847` で `alt && !shift && !ctrl && !splitter_press && in_arr` → `:848-865` で `no_session` / `no_press_action` → `:866` で `if no_session && no_press_action` | `in_arr = hit.in_lanes \|\| (f.header_w > 0.0 && f.header_pane.contains(hit.px, hit.py))` はそのまま + `!claim.session && !actions.any()` |
| `run.rs:929-934` | `primary_just_pressed && let Some((px,py)) = pos && !alt && !splitter_press && in_lanes` then `:935-948` `no_session` / `:949-952` `no_press_action` | 外側の `primary_just_pressed` / `pos` 再テストは `dispatch` 冒頭 (`run.rs:138-140`) と同一条件の再評価なので**落とす** (このブロックは `run.rs:981` で閉じる外側 press ブロックの内側にあり、`px`/`py` を同値で shadow しているだけ)。残りは `!f.pointer.modifiers.alt && !claim.splitter && hit.in_lanes && !claim.session && !actions.any()` |

---

### 6-D. `press_header.rs` (新規)

由来: `run.rs:451-617`。

```rust
//! track header pane (`f.header_pane`) の press 振り分け。 track 行 (volume band /
//! M·S·R 除外 / group disclosure / lane disclosure / reorder) と lane 行 (★/👁/✕) に分岐する。
//!
//! **popup が開いているフレームは丸ごと止める** (`ui.has_open_popups()`)。 context menu は
//! `capture_input == false` で背景 pointer を mask しないので、 menu item の press が背後の行に
//! 届いて volume band drag / reorder session が起動する (r.md #43 の同件)。

use super::*;

pub(super) fn dispatch(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>,
                       hit: &PressHit, claim: &mut PressClaim, actions: &mut PressActions);

fn track_row(..);    // run.rs:473-560  volume band / reorder / disclosure / lane disclosure
fn lane_header(..);  // run.rs:561-614  ★ (enabled) / 👁 (visible) / ✕ (delete)
```

**外側ゲートは `run.rs:462-467` をそのまま移す**:
`f.header_w > 0.0 && !ui.has_open_popups() && f.header_pane.contains(px, py)`
→ `track_index_from_y(py, f.header_pane.y, &f.tops)` → `f.visible_tracks.get(idx)`。
**`f.header_w > 0.0` を落とさない** — `header_w == 0` のときは header pane 幅が 0 で、
このフェーズは丸ごと no-op になるのが現行挙動 (§9-A の fixture がこれを踏む)。

**注意**: `run.rs:482-488` の indent 適用 (`let indent = f32::from(t.depth) * style.indent_px;` →
`row = Rect { x: header_pane.x + indent, .., w: (header_pane.w - indent).max(2.0), .. }`) を
落とさない。draw 側の `row_for_layout` と同 indent にすることで press↔draw を SSoT 化した修正
(M14 Phase 118 / #092 review) が入っている。

`track_row` が `state.track_volume_drag` (`run.rs:510`) / `state.track_reorder` (`:549`) を書いたら
`claim.session = true`。`actions.lane_toggle` / `actions.lane_button` を埋める。

---

### 6-E. `drag.rs` (新規)

由来: `run.rs:1008-1178` (session continuation)、`run.rs:1180-1268` (端オートスクロール)、
`run.rs:1270-1399` (per-frame live 発火)。

```rust
//! drag 継続フェーズ: 生きている session の `last_*` 更新 → 端オートスクロール →
//! per-frame の live 値発火。 **この 3 つの順序は現行と同一に保つ** (端スクロールが
//! anchor を逆補正した結果を per-frame 発火が読む)。

use super::*;

pub(super) fn advance(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    update_sessions(ui, f);
    edge_autoscroll(ui, f);
    emit_lane_height(ui, f);
    emit_row_height(ui, f);
    emit_header_w(ui, f);
    emit_playhead(ui, f);
    emit_track_volume(ui, f);
}
```

#### 6-E1. `update_sessions` (`run.rs:1008-1178`)

14 個の session (`ArrangementState` の `Option<*Session>` 全部 = `clip_drag` / `loop_drag` /
`track_reorder` / `track_volume_drag` / `playhead_drag` / `audio_drag` / `automation_point_drag` /
`automation_lane_resize_drag` / `track_row_resize_drag` / `header_resize_drag` /
`automation_clip_drag` / `automation_lasso_drag` / `automation_curve_param_drag` / `section_drag`)
の `last_mouse` / `last_alt` / `last_ctrl` / `last_shift` を更新する。

`ui.widget_state(f.wid)` を 1 度だけ取り、その `&mut ArrangementState` で 14 個を順に処理する
(現行 `run.rs:1022` と同じ)。**push_edit をこのブロック内で呼ばない** (不変条件 3)。

#### 6-E2. 「最後の 1px」判定を 1 つの関数へ (挙動は 1mm も変えない)

現行は同じ規則を 14 回書いていて、判定が **4 通り**に割れている (行番号は実測で、
いずれも「巻き戻し判定を書いている行」):

| 判定 | 対象 (判定行) |
|---|---|
| タプル完全一致 | clip (`1032`) / audio (`1093`) / automation point (`1113`) / automation clip (`1148`) / lasso (`1157`) |
| x のみ ε | section (`1044`) / loop (`1054`) / track volume (`1073`) |
| y のみ ε | curve param (`1170`) |
| 軸ごとに独立 | track reorder (`1065` = y / `1068` = x) |
| 巻き戻し対応なし (`!is_release` のみ) | playhead (`1079-1083`) / lane resize (`1119-1123`) / row resize (`1126-1130`) / header resize (`1133-1137`) |

(4 件はいずれも `if let Some(ref mut ..) = state.<session> && !is_release { .. }` の形。
直前の行 (`1117-1118` / `1125` / `1132`) は由来を書いたコメントなので**一緒に移設する**。)

構文の形も 2 通りある。**どちらも同じ規則で、`accept_release_pos` に寄せると 1 本になる**:

- `if !is_release { last_mouse = ..; last_alt/ctrl/shift = ..; } else if <巻き戻し判定> { last_mouse = ..; }`
  … clip (`1024-1035`) / section (`1038-1047`) / loop (`1048-1057`) / point (`1109-1116`) /
  automation clip (`1142-1151`) / curve param (`1166-1177`)
  → 新実装は
  `if accept_release_pos(is_release, axes) { last_mouse = ..; }` と
  `if !is_release { last_alt/ctrl/shift = ..; }` の **2 文に分ける** (modifier 更新は継続フレームのみ、
  という現行規則を保つ)。分けても同値: `!is_release` なら前者は必ず true になる。
- `if <session> && (!is_release || <巻き戻し判定>) { last_* = ..; }`
  … reorder (`1065`, `1068`) / track volume (`1071-1075`) / audio (`1093`) / lasso (`1156-1160`)
  → 新実装は `if accept_release_pos(is_release, axes) { .. }` 1 文。

**規則そのものは変えず、「どの軸を見るか」を引数にした 1 つの関数にまとめる**:

```rust
/// release フレームで winit が pointer を press 位置へ巻き戻す現象への対処。
/// **判定軸は対象ごとに違うので引数で受ける** (現行の 4 通りをそのまま表現する。
/// 規則そのものの統一は「離した瞬間の最後のわずかな動きが反映されるか」 が対象ごとに
/// 変わる = ユーザーに見える挙動の変更なので、 別件として扱う)。
#[derive(Clone, Copy)]
pub(super) enum RewindAxes {
    /// タプル完全一致で「巻き戻っていない」 と判定する。
    BothExact { cur: (f32, f32), anchor: (f32, f32) },
    /// x のみ `f32::EPSILON` 比較。
    X { cur: f32, anchor: f32 },
    /// y のみ `f32::EPSILON` 比較。
    Y { cur: f32, anchor: f32 },
}

/// `last_*` を `cur` で更新してよいか。
/// - 継続フレーム (`!is_release`) は常に true。
/// - release フレームは pointer が anchor から動いている (= 巻き戻っていない) ときだけ true。
pub(super) fn accept_release_pos(is_release: bool, axes: RewindAxes) -> bool {
    if !is_release {
        return true;
    }
    match axes {
        RewindAxes::BothExact { cur, anchor } => cur != anchor,
        RewindAxes::X { cur, anchor } | RewindAxes::Y { cur, anchor } => {
            (cur - anchor).abs() > f32::EPSILON
        }
    }
}
```

- `track_reorder` は x と y で**別々に**呼ぶ (`RewindAxes::Y` (`run.rs:1065`) と
  `RewindAxes::X` (`:1068`) の 2 回)。
- 「巻き戻し対応なし」の 4 つ (playhead / lane resize / row resize / header resize) は
  `accept_release_pos` を**通さず** `&& !is_release` のままにする
  (これも現行どおり — 通すと巻き戻し判定が付いて挙動が変わる)。
- `last_alt` / `last_ctrl` / `last_shift` の更新は**継続フレームのみ**という現行規則を維持する
  (release フレームは ModifiersChanged 先行 race を避けて据え置き)。
- `audio_drag` の sticky direction lock (`run.rs:1096-1103`) は巻き戻し判定の**外**
  (session が生きていれば毎フレーム走る)。`accept_release_pos` の `if` の中に入れないこと。
- `automation_curve_param_drag` の `preview_value` 再計算 (`run.rs:1173-1176`) も同様に
  巻き戻し判定の外。

#### 6-E3. `edge_autoscroll` (`run.rs:1180-1268`)

移動量ゲート (`ACTIVATE_PX`) → `arrangement_edge_scroll_axes` / marquee 判定 → `edge_scroll_delta`
→ `SetArrangeScroll` / `arrange_track_top` の push_edit → 実 scroll px ぶんの anchor 逆補正
(`arrangement_compensate_anchor` / `DragRectState.drag_start`) → `ui.request_redraw()`。
そのまま移設する。`state.edge_scroll_press` はこのブロックが唯一の所有者 (不変条件 4)。

#### 6-E4. `emit_*` 5 本 (`run.rs:1270-1399`)

各 fn は現行と同じ「ブロックで `widget_state` の borrow を閉じてから `push_edit`」の形を保つ。
呼び出し順は lane height → row height → header w → playhead → track volume。

---

### 6-F. `sessions.rs` (新規)

由来: `run.rs:1400-1650` (snapshot + release take)、`run.rs:1653-1683` (overlay 前計算)、
`run.rs:1990-2032` (reorder overlay)、`run.rs:1968-1975` (drag overlay min_len)。

```rust
//! drag session の「overlay 用スナップショット」 と「release フレームの take」 を 1 か所に集約する。
//!
//! 旧実装はこの 2 つを session ごとに交互に 14 回書いており、 どの session が overlay を持ち
//! どれが release commit を持つのかが読み取れなかった。

use super::*;

/// このフレームに生きている session (overlay / hover / cursor / heavy / header / rects が読む)。
///
/// **14 session のうち 4 つを意図的に入れていない。 理由は 2 種類ある**:
///
/// - `automation_lane_resize_drag` / `track_row_resize_drag` / `header_resize_drag` —
///   cursor がこの 3 つを読むのは release take の **後** なので
///   (`run.rs:1756-1759` の `resize_active` / `run.rs:1762-1765` の `header_resize_active`)、
///   live snapshot に入れると release フレームで `Some` に化けてカーソル形状が 1 フレーム変わる。
///   `cursor::apply` 内で `ui.widget_state` を読む形を維持する (不変条件 6)。
/// - `playhead_drag` — **live snapshot を読む消費者が 1 つも無い** (実測: widget 内の
///   `playhead_drag` 参照は `run.rs:444` 起動 / `:858`・`:945` の `no_session` 列挙 /
///   `:1079` continuation / `:1364` per-frame emit / `:1539` release take の discard、
///   および `release.rs:851` の marquee ゲートと `mod.rs:2071` の端スクロール軸判定だけで、
///   後者 2 つはどちらも `&ArrangementState` を直接読む)。 最終値は per-frame emit
///   (`run.rs:1364`) が出しているので release commit も無く、 take して捨てるだけ
///   (`run.rs:1535-1539`)。 **「読む人がいないから入れない」であって「入れ忘れ」ではない。**
///   復活させたくなったら、 まず消費者を 1 つ挙げること。
#[derive(Default)]
pub(super) struct LiveSessions {
    pub clip_drag: Option<ClipDragSession>,
    pub loop_drag: Option<LoopDragSession>,
    pub section_drag: Option<SectionDragSession>,
    pub track_reorder: Option<TrackReorderSession>,
    pub track_volume: Option<TrackVolumeDragSession>,
    pub audio_drag: Option<AudioDragSession>,
    pub point_drag: Option<AutomationPointDragSession>,
    pub automation_clip_drag: Option<AutomationClipDragSession>,
    pub automation_lasso: Option<AutomationLassoSession>,
    pub automation_curve_param: Option<AutomationCurveParamDragSession>,
}

/// release フレームで `take()` した session。 **`release::commit_releases` だけが読む。**
#[derive(Default)]
pub(super) struct ReleasedSessions {
    pub clip_drag: Option<ClipDragSession>,
    /// 短クリックに格下げされた clip drag の `(last_mouse, last_ctrl, last_shift)`。
    pub clip_short_click_pos: Option<((f32, f32), bool, bool)>,
    pub audio_drag: Option<AudioDragSession>,
    pub point_drag: Option<AutomationPointDragSession>,
    pub automation_clip_drag: Option<AutomationClipDragSession>,
    pub automation_curve_param: Option<AutomationCurveParamDragSession>,
    pub automation_lasso: Option<AutomationLassoSession>,
    pub lane_resize: Option<AutomationLaneResizeDragSession>,
    pub section_drag: Option<SectionDragSession>,
    pub loop_drag: Option<LoopDragSession>,
    pub track_volume: Option<TrackVolumeDragSession>,
    /// `(source_track_ids, parent, anchor_after)`。 `resolve_track_drop` で解決済。
    pub pending_drop: Option<(Vec<u32>, Option<u32>, Option<u32>)>,
    /// `pending_drop` の hash。 release フレームの optimistic preview で cache miss を強制するため
    /// `viewport_key` に混ぜる (`run.rs:1513-1520`)。
    pub pending_reorder_hash: u64,
}

/// session の clone (overlay 用) と release take を 1 度に行う (`run.rs:1400-1650`)。
/// `response.automation_lasso_active` はここで立てる (`run.rs:1624-1626`)。
///
/// **`playhead_drag` / `track_row_resize_drag` / `header_resize_drag` は release フレームで
/// `take()` して捨てるだけ** (per-frame emit で最終値が出ているので release commit は不要)。
pub(super) fn take(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
) -> (LiveSessions, ReleasedSessions);

/// heavy 描画が重ねる overlay 群 (`run.rs:1653-1683` + `:1969-1975` + `:1990-2032`)。
#[derive(Default)]
pub(super) struct Overlays {
    /// `(session, beat_delta, track_delta)`。 press 直後 (delta=0) から出す — 閾値ゲートを
    /// 張ると mouse down のハイライトが消える (r.md #24)。
    pub clip: Option<(ClipDragSession, f64, i32)>,
    /// Resize ゴーストの最小長 (snap unit と `MIN_CLIP_LEN_BEATS` の大きい方)。
    /// alt の真値は session の `last_alt` (r.md #68: preview ≠ commit を防ぐ)。
    pub clip_min_len: f64,
    pub audio: Option<AudioDragSession>,
    pub point: Option<AutomationPointDragSession>,
    pub automation_clip: Option<AutomationClipDragSession>,
    pub curve_param: Option<AutomationCurveParamDragSession>,
    pub lasso: Option<AutomationLassoSession>,
    pub section: Option<SectionDragSession>,
    pub reorder: Option<ReorderOverlay>,
    /// snap 適用済の loop preview 範囲 (commit と同一値)。
    pub loop_preview: Option<(f64, f64)>,
    /// `released.pending_reorder_hash` の写し (`viewport_key` の材料)。
    pub reorder_hash: u64,
}

pub(super) fn overlays(
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    released: &ReleasedSessions,
) -> Overlays;
```

`overlays` の中身は `run.rs:1653-1683` (clip delta / loop preview)、`run.rs:1967-1974`
(`MIN_CLIP_LEN` / `drag_overlay_alt` / `drag_overlay_min_len`)、`run.rs:1981`
(`section_drag_overlay` → `Overlays::section`)、`run.rs:1990-2031` (`reorder_overlay`) をそのまま移設。
`reorder_overlay` は `resolve_track_drop` を **commit (`pending_drop`) と同じ pure 関数**で通す構造を維持する
(preview = commit の保証)。

**`run.rs:1980` の `sections_for_draw: Vec<SectionView> = sections.to_vec();` は `Overlays` に
入れない — 消えるだけ。** `Overlays` に `sections` フィールドは無い。この行は heavy closure へ
owned で持ち込むためだけの毎フレーム `Vec` clone で、`render.rs:794` の 1 か所でしか使われて
いないので、`&f.sections` の借用に落ちて消滅する (§6-J の置換表の
`sections_for_draw` → `f.sections` が正)。`section_drag_overlay` (`run.rs:1981`) の方だけが
`Overlays::section` として残る。

**呼び出し位置が前に動くことの正当性** (計画で唯一「文の順序」を変える箇所なので明示する):
`sessions::overlays` は現行では `cursor::apply` (`run.rs:1747-1849`) の**後**にあった 3 つの計算
(`drag_overlay_min_len` / `section_drag_overlay` / `reorder_overlay`) を含むが、
これらは**純粋**である:

- 入力は `f.*` (地形、 誰も書かない) / `live.*` (この時点で確定済の session clone) /
  `view.snap` / `style` だけ。実測で `run.rs:1966-2032` に `ui.` も `app.` も `response.` も
  1 件も現れない (`resolve_track_drop` は `mod.rs` の pure 関数)。
- 出力は `Overlays` のフィールドのみ。`ArrangementState` にも `ArrangementResponse` にも書かない。
- したがって `cursor::hover` / `cursor::apply` より前に評価しても、両者の入力・出力とも変わらない。

`sessions::take` (`run.rs:1400-1650`) は `response.automation_lasso_active` (`:1625`) を書き、
`ui.widget_state` を触るので**位置を動かさない** (現行どおり `drag::advance` の直後、
`cursor::hover` の直前)。

---

### 6-G. `cursor.rs` (新規)

由来: `run.rs:1684-1745` (hover)、`run.rs:1747-1849` (cursor)。

```rust
//! hover 判定と cursor 決定。 **`apply` は `hover` が書いた `response` を読む**ので、
//! この 2 つの呼び出し順を入れ替えないこと。

use super::*;

/// `hovered_track` / `hovered_clip` / `hovered_zone` / `hovered_automation_lane` /
/// `hovered_section` / `hovered_section_zone` / `section_rects` / `dragging*` を埋める
/// (`run.rs:1684-1745`)。
///
/// `response.hovered_clip` は **このフレーム中に**確定し、 heavy (`render::dispatch`) が
/// フェードの掴む正方形を出す clip の判定に使う (r.md #58)。
/// **`viewport_key` にも `fold_arrangement_clip_hash` にも入れないこと** —
/// 入れるとマウスを動かすたびにアレンジ全体が再構築される。
pub(super) fn hover(f: &ArrangementFrame<'_>, live: &LiveSessions,
                    response: &mut ArrangementResponse);

/// cursor 形状の決定 (`run.rs:1747-1849`)。 優先順位は
/// header resize > lane/row resize > drag 種別 > reorder > volume > hover zone >
/// hover section zone > splitter hover (Ns) > header splitter hover (Ew) > automation clip zone。
///
/// **`automation_lane_resize_drag` / `track_row_resize_drag` / `header_resize_drag` は
/// `ui.widget_state` から直接読む** — この読みは `sessions::take` の release take より後に
/// 位置する必要があるため (release フレームでは None になるのが現行挙動、 不変条件 6)。
pub(super) fn apply(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>,
                    live: &LiveSessions, response: &ArrangementResponse);
```

---

### 6-H. `header.rs` (新規)

由来: `run.rs:2101-2463` (描画)、`run.rs:2464-2501` (クリック確定)。

```rust
//! track header 列の immediate-mode 描画と、 そこで検出した click の確定発行。
//!
//! **描画は heavy と `commit_releases` の後**に走る (= `Scene` の最前面。 不変条件 1)。

use super::*;

/// この 1 フレームで検出した header の click (loop 内で `push_edit` すると複数発行に
/// なるため、 loop 後に 1 度だけ発行する — 旧 `clicked_track_for_select` /
/// `disclosure_clicked`)。
#[derive(Default)]
pub(super) struct HeaderClicks {
    pub clicked_track: Option<u32>,
    pub disclosure: Option<u32>,
}

/// header 行の描画 + click 検出 (`run.rs:2101-2463`)。
/// `response.track_header_rects` を積む。
pub(super) fn draw_rows(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>,
                        live: &LiveSessions, response: &mut ArrangementResponse) -> HeaderClicks;

/// disclosure toggle → `app.ui_prefs.collapsed_groups` の insert / remove、
/// それ以外は `app.apply_select_tracks(tid, modifier, &visible_ids)` (`run.rs:2464-2501`)。
/// modifier は **press 時 snapshot** (`state.press_modifiers`) を真値にする。
///
/// **disclosure は `AppEvent` を経由しない。** `run.rs:2468` は `Edit::mutate` の中で
/// `collapsed_groups` を直接 toggle する (コード中のコメント (`run.rs:2298` / `:2464`) は
/// これを「ToggleGroupCollapsed」と呼ぶが、 同名の `AppEvent` variant は存在しない —
/// `AppEvent::ToggleTrackAutomationCollapsed` と混同しないこと)。 発行順は
/// disclosure が先で、 立ったら `clicked_track` を `None` に落とす (`run.rs:2469`) という
/// priority も現行のまま移す。
pub(super) fn commit_clicks(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>,
                            clicks: HeaderClicks, response: &mut ArrangementResponse);
```

`draw_rows` の実装で**必ず直すこと**:

1. `run.rs:2114-2117` / `run.rs:2462` の clip rect scope の open-code
   (`ui.current_clip_rect()` / `ui.set_current_clip_rect(Some(merge_clip(..)))` / 復元) を
   **正規の `ui.with_clip_rect(f.header_pane, |ui| { .. })` に戻す** (`ui.rs:1154-1162`)。
   `with_clip_rect` の中身は `let prev = self.current_clip; self.current_clip =
   Some(merge_clip(prev, Some(rect)).unwrap_or(rect)); f(self); self.current_clip = prev;` で、
   open-code と**完全に同一**なので挙動は変わらない。
   open-code の理由 (`run.rs:2110-2113`)「closure 化すると `ui.xxx` の大量 rename を要する」は、
   header ループが独立 fn になった時点で消える。`HeaderClicks` は closure の外で
   `let mut clicks = HeaderClicks::default();` して closure 内から `&mut` で埋め、closure 後に返す。
   `response.track_header_rects` も同じく closure 外の `&mut` で積む。
2. `run.rs:2118` の `visible_idx_for_headers = compute_visible_indices(&tracks_for_draw)` を**撤去**し、
   `f.visible_tracks.iter().enumerate()` で回す。
   根拠: `visible_tracks` は既に `is_visible_track(t, tracks)` (full list) で filter 済で
   (`mod.rs:1434-1462`)、親チェーンの全 ancestor も同じ条件を満たすので `visible_tracks` 内に存在し、
   いずれも `collapsed == false`。synthetic master は `parent_id == None` で即 true。よって
   `compute_visible_indices(&visible_tracks) == (0..visible_tracks.len())` (恒等)。
   `run.rs:2493-2496` の `visible_ids` も `f.visible_tracks.iter().map(|t| t.id).collect()` になる。
3. **同件チェック (同じ恒等が `draw.rs` にもある)**: `draw.rs:45` の
   `let visible_indices = compute_visible_indices(tracks);` も**撤去**し、
   `for (visible_i, t) in tracks.iter().enumerate()` に直す。`draw_lanes_bg` の唯一の呼び出しは
   `render.rs:80` で、渡すのは filter 済リスト (現行 `&tracks_owned`、新実装 `&f.visible_tracks`) なので
   2 と同じ恒等が成り立つ。撤去は cached ブロック内の毎フレーム `Vec<usize>` 確保も消す。
   これで production の `compute_visible_indices` 呼び出しは `frame::build` の 1 か所
   (旧 `run.rs:72`、full `tracks` に対する本物の filter) だけになる。
4. `run.rs:2122-2127` の `header_tops` 再計算を**撤去**し `f.tops` を使う
   (`header_pane.y == lanes.y` は rect 分割 (`run.rs:34-53`) から自明)。
5. `run.rs:2130` の `if header_w > 0.0 {` は `if f.header_w > 0.0 {` としてそのまま残す
   (`header_w == 0` で header 行を 1 本も描かないのが現行挙動)。

---

### 6-I. `rects.rs` (新規)

由来: `run.rs:2503-2696` (本体) + `run.rs:1000-1006` (モジュール doc に移すコメント)。

```rust
//! caller (`arrangement_view.rs`) が context menu / overlay の anchor に使う rect 群を
//! `ArrangementResponse` に積む。 積む順序は **描画順 (上から下、 左から右)** で、
//! caller はこの順序に依存している (`ArrangementResponse::clip_rects` の doc 参照)。
//!
//! M14 Phase 63n-2 (#028): 右クリック on point の context menu は **caller 責務**。
//! widget は `response.automation_point_rects: Vec<(AutomationPointKey, Rect)>` を毎 frame
//! 返し (clip_rects と同 idiom)、 caller は loop で `context_menu_for(*rect, &["Hold",
//! "Linear", "Bezier"], ...)` を呼ぶ。 widget 内で secondary press を消費する旧設計は popup の
//! anchor_rect が **右クリック frame だけ Some** で次 frame 以降 caller が context_menu_for を
//! 呼ばないため popup state が消える bug を持っていた (= 一瞬で popup が閉じる)。 #028 §11.4
//! で確定した「caller が anchor を毎 frame 呼ぶ」 idiom に統一。
//! (旧 `run.rs:1000-1006`。 press フェーズと rect 収集フェーズの間に浮いていたが、
//!  説明しているのはこのモジュールの存在理由そのものなのでここへ移した。)

use super::*;

pub(super) fn collect(f: &ArrangementFrame<'_>, live: &LiveSessions,
                      response: &mut ArrangementResponse) {
    push_clip_rects(f, response);                    // run.rs:2503-2523
    push_lane_default_rects(f, response);            // run.rs:2524-2553
    push_point_drag_live(f, live, response);         // run.rs:2554-2581
    push_point_rects(f, response);                   // run.rs:2582-2641
    push_automation_clip_and_lane_rects(f, response);// run.rs:2642-2694
    // 不変条件 5: 現行と同じく **最後**に書く。
    response.dragging_automation_clip =
        live.automation_clip_drag.as_ref().map(|acd| acd.kind);
}
```

---

### 6-J. `render.rs` (改修)

**削除**: `render.rs:5` の `#![allow(clippy::too_many_arguments)]`。

**追加**: `run.rs:1851-1917` (cache key)、`run.rs:1918-2034` (heavy 用キャプチャ整備)、
`run.rs:2035-2094` (TimeMapping / grid・ruler style / clip content)、`run.rs:2095-2097`
(`ui.heavy(("arrangement_inner", &id), ..)` の dispatch 自体) を `render::dispatch` として移設。
`run.rs:1966-2031` のうち overlay 計算 3 本は `sessions::overlays` へ行く (§6-F) ので
`render::dispatch` には来ない。

`render::dispatch` は **outer `app` を必要とする** (`run.rs:2075` の
`TempoMap::from_song(app.song_doc.song())` と `content_build` 系)。他のフェーズは
`Edit::mutate` のクロージャ引数としてしか `app` を使わないので `app` を受け取らない
(実測: press / drag / cursor / header / rects / release の各範囲に outer `app` の使用は無い)。

```rust
/// heavy 描画フェーズ。 cache key の構築 → `HeavyInput` の組み立て → `ui.heavy` の dispatch。
///
/// **`ui.heavy` / `hctx.cached` のクロージャに `'static` 境界は無い**
/// (`ui/crates/ui/src/widgets/heavy.rs:53-56` / `:76-79`)。 `&ArrangementFrame` は `ui` から
/// 一切借りていないのでそのまま持ち込める。 旧実装が `Arc::from(visible_tracks.clone())` の
/// 毎フレーム deep clone と prefix-sum の 3 重計算をしていたのは、 この境界を誤読していたため。
pub(super) fn dispatch(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    overlays: &Overlays,
    response: &ArrangementResponse,
);

/// heavy クロージャに渡す「このフレームだけの owned 値」。 `f` から借りられるもの
/// (`visible_tracks` / `tops` / `sections` / `selected_*`) は**入れない**。
pub(super) struct HeavyInput {
    pub viewport_key_hash: u64,
    pub id_hash: u64,
    /// r.md #58: フェードの掴む正方形を出す clip。 `response.hovered_clip` の写し。
    /// **`viewport_key_hash` の材料にしてはいけない。**
    pub hovered_clip: Option<ClipKey>,
    pub clip_content: HashMap<ClipKey, ClipContentDraw>,
    pub stretch_ghost_content: HashMap<ClipKey, ClipContentDraw>,
    pub selected_clip_set: HashSet<ClipKey>,
    pub selected_automation_clip_set: HashSet<AutomationClipKey>,
    pub selected_automation_point_set: HashSet<AutomationPointKey>,
    pub mapping: TimeMapping,
    pub sample_viewport: ViewportState1D,
    pub grid_style: BarBeatGridStyle,
    pub ruler_style: TimeRulerStyle,
}

/// 旧 `render_arrangement_heavy` (37 引数)。 **4 引数**にする。
fn render_arrangement_heavy(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    heavy: &HeavyInput,
    overlays: &Overlays,
);
```

本体の機械的な置換 (`render.rs:54-861`):

| 旧 | 新 |
|---|---|
| `tracks_owned` (`:11`、`&tracks_owned` / `.iter()` / `.len()` で 20 か所) | `&f.visible_tracks` |
| `tops_owned_for_heavy` (`:59-64` の再計算) | `&f.tops` (**再計算ごと削除**) |
| `view_copy` / `style_copy` | `f.view` / `f.style` |
| `lanes` / `ruler` / `header_pane` / `header_pane_copy` | `f.lanes` / `f.ruler` / `f.header_pane` (**二重引数を撤去**) |
| `arranger_rect_copy` / `arranger_header_rect_copy` / `arranger_lane_h_copy` | `f.arranger_rect` / `f.arranger_header_rect` / `f.arranger_lane_h` |
| `beat_per_px` / `zoom_x_px_per_beat` | `f.beat_per_px` / `f.zoom_x_px_per_beat` |
| `selected_tracks_for_heavy` (`:35`、`:86` で 1 回) | `f.selected_tracks` |
| `sections_for_draw` (`:50`、`:794` で 1 回) | `f.sections` |
| `id_for_inner` / `viewport_key_hash` / `hovered_clip` / `clip_content` / `stretch_ghost_content` / `selected_set` / `selected_automation_*_for_heavy` / `mapping` / `sample_viewport` / `grid_style` / `ruler_style` | `heavy.*` |
| `drag_overlay_clone` / `drag_overlay_min_len` / `audio_drag_overlay` / `point_drag_overlay` / `automation_clip_drag_overlay` / `curve_param_overlay` / `lasso_overlay` / `section_drag_overlay` / `reorder_overlay` / `loop_preview_clone` | `overlays.*` |

`heavy id` は `("arrangement_inner", &f.id)` にする (旧 `("arrangement_inner", &id)` と型・hash が同一)。
`id_hash` は `hash_inputs(f.id)` (旧 `hash_inputs(id)` (`run.rs:2067`) と同一)。

`viewport_key` (`run.rs:1885-1917`) の材料は 1 つも変えない。`pending_reorder_hash` は
`overlays.reorder_hash` から取る。

**heavy は header pane の背景も描く。** `render.rs:76-77` の
`hctx.cached(viewport_key_hash, |hctx| { push_filled_rect(hctx, header_pane, style_copy.header_bg); .. })`
がそれで、`header::draw_rows` が描く行はこの上に重なる。§9-B 3 の描画順テストはこの事実に依存する
(heavy が置いた `lanes` 全面の背景 (`draw.rs:38` の `push_filled_rect(hctx, lanes, style.bg)`) より
**後**に header 行の panel が積まれることを assert する)。

---

### 6-K. `release.rs` (改修)

**削除**: `release.rs:5` の `#![allow(clippy::too_many_arguments)]`。

**署名** (33 → 4):

```rust
pub(super) fn commit_releases(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
    released: ReleasedSessions,
) {
```

本体の置換は機械的:

| 旧引数 | 新 |
|---|---|
| `wid` | `f.wid` |
| `pointer` | `f.pointer` |
| `view` / `style` / `master_row` / `sections` | `f.view` / `f.style` / `f.master_row` / `f.sections` |
| `selected_clips` / `selected_automation_clips` / `selected_automation_points` | `f.selected_clips` / … |
| `visible_tracks` / `press_tops` | `&f.visible_tracks` / `&f.tops` |
| `lanes` / `ruler` / `header_pane` / `arranger_rect` / `lanes_h` / `arranger_lane_h` | `f.*` |
| `beat_per_px` / `zoom_x_px_per_beat` | `f.*` |
| `*_release` 10 種 + `clip_short_click_pos` + `pending_drop` | `released.*` |

`release.rs` は `use daw_ui_core::PointerFrame;` (`release.rs:8`) を持つが、`f.pointer` 経由になっても
型注釈で使うなら残す。未使用になったら削除する。

**`release.rs` 内の `ui.widget_state(wid)` 読みを `released.*` にも `live.*` にも
置き換えないこと。** `release.rs:842-855` の `marquee_press` は 11 session の `is_none()` を
**`widget_state` から**読む。この読みは
`marquee_zone_ok`(`release.rs:822`) が `pointer.primary_just_pressed` を要求するので
**press フレームで走る** — つまり「同フレームの `press::dispatch` が起動したばかりの session」を
見なければ正しくない。しかも読む 11 個には `LiveSessions` が意図的に外した
`automation_lane_resize_drag` / `track_row_resize_drag` / `playhead_drag` が含まれる
(§6-F の除外理由を参照)。`released.*` は release フレームでしか埋まらず、`live` は 3 つを
持たないので、**どちらに差し替えても現行と等価にならない**。
`commit_releases` は毎フレーム呼ばれる (`run.rs:2099`) ことを忘れないこと。
置換表の対象は**引数だけ**で、関数本体の `widget_state` 読みは `wid` → `f.wid` の
書き換えに留める。

---

## 7. 同時に消える重複 (すべて挙動不変)

分割の副産物として消えるもの。**性能改善を目的にしない** — 「同じ値を 2 か所で持たない」の帰結。

1. `tracks_for_draw = Arc::from(visible_tracks.clone())` (`run.rs:1919`) — 全 track + clip の
   毎フレーム deep clone。`tracks_owned` (`run.rs:1920`) はその `Arc::clone`。→ `f.visible_tracks` の借用。
2. prefix-sum tops の 3 重計算: `press_tops` (`run.rs:94`) / `header_tops` (`run.rs:2122`) /
   `tops_owned_for_heavy` (`render.rs:59`) → `f.tops` 1 本。
3. `header_pane` と `header_pane_copy` の二重引数 (`render.rs:16-17`、`run.rs:1976` で
   `let header_pane_copy = header_pane;`)。
4. heavy 用 selection 集合の再構築 4 本 (`run.rs:1925-1932` / `:1951-1953`) → `HeavyInput` に 1 度だけ。
   `selected_tracks_for_heavy` は借用に落ちて消える。
5. filter 済リストへの `compute_visible_indices` は恒等 — `visible_idx_for_headers` (`run.rs:2118`) と
   `draw_lanes_bg` 内の同型 (`draw.rs:45`) の **2 か所とも**撤去する (§6-H 2 / 3)。
6. `view_copy` / `style_copy` / `arranger_rect_copy` / `arranger_header_rect_copy` /
   `arranger_lane_h_copy` / `loop_preview_clone` / `drag_overlay_clone` /
   `sections_for_draw` などの `*_copy` / `*_clone` 別名群。
7. `run.rs:2114-2117` / `:2462` の open-code した clip rect scope → `ui.with_clip_rect`。
8. `no_session` ブロック 2 か所 (`run.rs:848-861` / `:935-948`) → `PressClaim` 1 本。
9. 「最後の 1px」判定の 14 回コピー → `accept_release_pos` + `RewindAxes` (§6-E2。**規則は不変**)。

---

## 8. 実装の進め方 (推奨手順)

**フェーズ分割ではなく、1 回の作業の中の順序**。途中で止めない。

1. **Step 0** (§1): `ui.heavy` の借用キャプチャを単独で検証。
2. **§9-0 の 2 つの前提を先に片付ける**: `arr_widget.rs` に `build_app_with_header` を足し
   (`build_app()` は `build_app_with_header(0.0)` を呼ぶ薄いラッパにする。既存 15 本のテストの
   座標定数は据え置きなので `cargo test -p daw_gui --test arr_widget` が引き続き 15 passed で
   あることをここで確認する)、`UiPrefs` / `SelectionState` に `#[derive(Debug)]` を足して
   `cargo check -p daw_gui` を通す (フィールド型に Debug が無いものがあればここで出る。
   §9-0 (b) に各型の derive 位置を file:line で列挙してあるので辿れる)。
   これは分割前の main に対する変更で、トランスクリプトの「前」と「後」の両方で同じ harness を
   使うために**先**でなければならない。
3. **等価性トランスクリプトの採取** (§9-A): 分割前の main で fixture を採る。
   `git stash` / worktree / WIP commit を使って「分割前」の状態を確保できるようにしておく。
   **2 回採って決定性セルフチェック**を先に通す (§9-A 手順 1)。
4. `frame.rs` を作り、`run.rs` の先頭 123 行を置き換えて `cargo check -p daw_gui` を通す
   (この時点では `f.xxx` を旧ローカル名に再束縛するのではなく、**旧ローカル名を全部 `f.` に書き換える**)。
5. `sessions.rs` → `cursor.rs` → `rects.rs` → `header.rs` → `drag.rs` →
   `press_header.rs` → `press_lanes.rs` → `press.rs` の順に切り出す
   (後ろのフェーズから切ると、切るたびに `run.rs` が短くなって見通しが良くなる)。
   `mod.rs` の `mod` / `use ...::*;` は**モジュールを作るたびに同時に足す** (§4)。
6. `render.rs` / `release.rs` の署名を縮め、両方の `#![allow(clippy::too_many_arguments)]` を削除。
   `draw.rs:45` の恒等 `compute_visible_indices` もここで撤去する (§6-H 3)。
7. `run.rs` を §5 の形にし、`#[allow(clippy::too_many_lines)]` を削除。
8. §9-A の突き合わせ → §9-B の恒久テスト追加 → §11 の検証コマンド → 実機 sign-off。
9. 等価性トランスクリプトの一時ファイル (`daw_gui/src/widgets/arrangement/equivalence.rs`) と
   `mod.rs` の `#[cfg(test)] mod equivalence;` の 2 行を**削除**する。
   **`build_app_with_header` と 2 つの `#[derive(Debug)]` は残す** (恒久テストが使う / 単体で有用)。

**巨大ファイルを並行 agent で分割編集しない** (過去に全滅事故あり)。長走行の前に WIP commit する。

---

## 9. テスト

### 9-0. まず直す 2 つの前提 (テストを書き始める前に)

#### (a) header pane を踏める fixture を用意する

`arr_widget.rs:35-60` の `build_app` は **`app.ui_prefs.arrange_header_w = 0.0`** を設定している
(`arr_widget.rs:53`)。この値だと press 側 (`run.rs:462` の `if header_w > 0.0`) と描画側
(`run.rs:2130` の `if header_w > 0.0`) が**ともに丸ごと skip** される。つまり `press_header.rs`
(由来 451-617 = 167 行) と `header.rs` (由来 2101-2501 = 401 行) は既存 fixture では
**1 フレームも実行されない**。この 568 行は分割対象 2,699 行の約 21% で、ここを踏まない
トランスクリプトで「1 byte も違わない」を出しても header フェーズは無検証のままになる。

そこで **`arr_widget.rs` に幅つきの fixture を足す** (恒久側にも要るので本体に置く):

```rust
/// header pane を踏むテスト用。 `arrange_header_w = 160.0` (production default、`app.rs:304`)。
/// `build_app()` は `build_app_with_header(0.0)` にする (既存テストの座標定数は据え置き)。
fn build_app_with_header(header_w: f32)
    -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>);
```

座標の読み替え (`view_build.rs:268` / `geometry.rs:96-98` から導出、snap は無効):

| | `header_w = 0` (既存) | `header_w = 160` (追加) |
|---|---|---|
| `header_pane` | `x∈[0,0)` = 無 | `x∈[0,160)`, `y≥38` |
| `lanes.x` / `lanes.w` | `0` / `800` | `160` / `640` |
| `view.len_beats` | `800/64 = 12.5` | `640/64 = 10.0` |
| `beat_per_px` | `1/64` | `1/64` (`len_beats/lanes.w` なので header_w に依らない) |
| beat → x | `beat * 64` | `160 + beat * 64` |

**既存テストの座標定数 (`ZOOM=64` / `WIDGET_RECT.w=800` / `track0_y()` 等) を
`header_w=160` の側と共有しない。** 共有すると x が 160px ずれる。header 側のテストは
`160.0 + beat * ZOOM` を明示的に書く。

#### (b) `AppData` をダンプできるようにする

`Song` (`common/src/model.rs`) と `ArrangementResponse` (`mod.rs:780-781`) は `Debug` を持つが、
**`UiPrefs` (`daw_gui/src/state/ui_prefs.rs:4`) / `SelectionState` (`daw_gui/src/state/selection.rs:6`) /
`TransportState` (`daw_gui/src/state/transport.rs:6`) / `UiEphemeral`
(`daw_gui/src/state/ui_ephemeral.rs:12`) はいずれも derive を 1 つも持たない** (実測)。
そのままでは `format!("{:?}", app.ui_prefs)` はコンパイルしない。

**やること 1 — derive を足す (恒久。この 2 つは分割完了後も残す)**:

- `ui_prefs.rs:4` の `pub struct UiPrefs` に `#[derive(Debug)]`。
  全フィールドの型が `Debug` を持つことは確認済 (**以下はいずれも `#[derive(..)]` 行**で、
  struct / enum 定義はその次行):
  `MeterSettings` (`daw_gui/src/master_meter/settings.rs:151`、struct は `:153`)、
  `AppDirs` (`common/src/app_dirs.rs:22`、struct は `:23`)、
  `RecentFiles` (`daw_gui/src/recent.rs:14`、struct は `:15`)、
  `EditorWindowGeometry` (`common/src/model.rs:275`) / `PianoRollViewState` (`:286`) /
  `AudioEditorViewState` (`:311`) / `FollowMode` (`:325`)、
  `ClipKey` / `AutomationLaneKey` (`common/src/model/automation.rs:434-446`)、
  `daw_ui_renderer::Rect` (`ui/crates/renderer/src/scene.rs:100`)。
- `selection.rs:6` の `pub struct SelectionState` に `#[derive(Debug)]`。
  フィールドは `Vec` / `Option` の model key と `EditSurface` / `AutomationPointKeyRef`
  (`daw_gui/src/app_types.rs:844` / `:822` でどちらも `Debug`)。

**やること 2 — `TransportState` / `UiEphemeral` は derive せず、
「全フィールドを分解束縛して、ダンプしないものだけ理由つきで `_` に落とす」形でダンプする**。
derive しない理由は「できないから」ではなく **whole-struct ダンプが目的に反するから**:
両者は `std::time::Instant` (`transport.rs:50` `panic_reinit_due` / `:89` `export_progress_at`、
`ui_ephemeral.rs:269` `anim_epoch` / `:274` `frame_now`) を持ち、値が実行のたびに変わるので
`diff` が必ず赤くなる。GPU texture handle (`ui_ephemeral.rs:35`/`:44`/`:53`) も同様。

**「到達できるフィールドを列挙する」方式は採らない。** 初版はそれを
`grep -o "AppEvent::[A-Za-z0-9_]*" daw_gui/src/widgets/arrangement/` (43 variant + doc 中の
`AppEvent::X` は `mod.rs:13` の doc コメント) と `grep "app[.]ui_ephemeral[.]"` から
導いていたが、この 2 本の grep は
**「widget が emit した `AppEvent` を受けたハンドラが、さらに別の `ui_ephemeral` フィールドを
書く」1 段先を見ていない**。ハンドラ側で `self.ui_ephemeral.<field> = ..` を書く行は
`daw_gui/src/handler/` だけで 80 行以上あり (`status_message` / `last_touched_param` /
`clip_edit_buffer_target` など)、43 variant を 1 つずつ辿らないと allowlist の完全性を
主張できない。**allowlist の穴は「差分が出ない」という形で現れる**ので、等価性の証明としては
最悪の壊れ方をする。

そこで **`..` を使わない完全分解束縛**にする。フィールドが増減したらコンパイルが落ちるので、
列挙が黙って陳腐化しない (`ui_ephemeral` は 86 フィールド / `transport` は 20 フィールド):

```rust
// 一時テスト内。 `..` を書かないこと — 書いた瞬間に「列挙し忘れ」が復活する。
let UiEphemeral {
    // ---- ダンプする (Debug を持ち、 実行ごとに変わらない) ----
    last_arrange_lanes_size, last_arrange_rows, track_rename_id, track_rename_text,
    section_rename_id, section_rename_text, section_menu, section_menu_open,
    clip_create_menu, clip_create_menu_open, editing_automation_point,
    audio_editor_clip, audio_editor_hover_beat_in_clip, status_message,
    /* … 残りも全部ここに書く … */
    // ---- ダンプしない (理由を 1 件ずつ書く) ----
    anim_epoch: _,   // Instant: 実行ごとに変わる
    frame_now: _,    // Instant: 同上
    arr_label_cache: _,   // RefCell<ArrLabelCache>: Debug 無し (描画キャッシュ、 観測対象でない)
    /* … Debug を持たない型も同様に理由つきで `_` … */
} = &app.ui_ephemeral;
```

- **この分解は `daw_gui` crate の中でしか書けない。** `UiEphemeral` は
  `arr_label_cache` (`ui_ephemeral.rs:15`) / `tempo_map_cache` (`:18`) /
  `home_toggle_at_first` (`:118`) / **`arrange_zoom_history` (`:175`) /
  `arrange_zoom_anchor` (`:178`)** の 5 つが `pub(crate)` で、`daw_gui/tests/` (別 crate) からは
  そもそも**見えない**。後ろの 2 つは arrangement のズーム履歴そのものなので、
  「見えないフィールドが観測から落ちている」ことに気付けない。
  → §9-A のトランスクリプトは **crate 内の `#[cfg(test)]` モジュール**に置く (§9-A)。
- `TransportState` は全 20 フィールドが `pub` (`transport.rs:13-116`) なので分解は容易。
  `Instant` 2 つ (`:50` / `:89`) と `export_cancel: Option<Arc<AtomicBool>>` (`:92`) を `_` に落とす。
- **`Instant` を内側に隠し持つ型に注意**: `last_touched_param: Option<TouchedParam>` は
  `TouchedParam` が `Debug` を導出している (`app_types.rs:1088`) ので**コンパイルは通るが**、
  `touched_at: Instant` (`app_types.rs:1097`) が入っているので値は毎回変わる。
  こういうものは `{:?}` を丸ごと使わず `track_id` / `target` / `display_name` だけを出す。
  取りこぼしは下の**決定性セルフチェック**が機械的に捕まえる。
- `ui_prefs` / `selection` は `Instant` を含まないので、分解せず derive した `{:?}` を丸ごと使う。
  `ui_prefs` は widget が `arrange_track_top` / `collapsed_groups` を直接書き、さらに
  `SetArrangeScroll` / `SetArrangeZoom` / `SetArrangeHeaderW` / `SetArrangeTrackRowH` /
  `SetSingleTrackRowH` / `ToggleTrackAutomationCollapsed` / `SelectBottomPanel` 経由でも書く。

**決定性セルフチェック (必須。等価性比較の前に 1 回やる)**: 分割前の同じコードでトランスクリプトを
**2 回**採り、`diff -u before1.txt before2.txt` が空であることを先に確認する。空でなければ、
まだ非決定なフィールド (Instant / handle / HashMap のイテレーション順など) が混じっている
証拠なので、そのフィールドを `_` 側へ落としてから本番の比較に進む。
**この手順を飛ばすと、after の差分が「壊した」のか「もともと揺れる」のか判別できない。**

### 9-A. 等価性トランスクリプト (一時。作業完了後に捨てる)

これが「挙動が変わっていない」ことの唯一の直接証明。

**置き場所は `daw_gui/src/widgets/arrangement/equivalence.rs`** (crate 内の `#[cfg(test)]`
モジュール。`mod.rs` の `#[cfg(test)] mod tests;` (`mod.rs:2412-2413`) の隣に
`#[cfg(test)] mod equivalence;` を**一時的に**足し、完了後にファイルごと削除する)。

**`daw_gui/tests/` (別 crate) に置かない。** §9-0 (b) の完全分解束縛は `UiEphemeral` の
`pub(crate)` フィールド 5 つ (`arr_label_cache` / `tempo_map_cache` / `home_toggle_at_first` /
`arrange_zoom_history` / `arrange_zoom_anchor`) に触る必要があり、統合テストからは
**そもそも名前が見えない**。とくに後ろ 2 つは arrangement のズーム履歴なので、
見えないまま比較すると「観測できていない state が変わった」を見逃す。
crate 内 `#[cfg(test)]` から `UiHost` を回す前例は `daw_gui/src/widgets/time_grid.rs:551-559`
(`run_frame` ヘルパ)、crate 内で `AppData::new` する前例は `daw_gui/src/app_tests.rs:1099`。
実行は `cargo test -p daw_gui --lib arrangement::equivalence` (lib unit test なので
`CARGO_BIN_EXE_daw_gui` を含む target と無関係 = daw_gui を起動しない)。

- `arr_widget.rs:35-115` の `build_app` / `build_app_with_header` (§9-0 (a)) / `modifiers` /
  `press` / `hold` / `release` / `frame` / `drive_scene` を複製する
  (widget の内部に依存しないので分割の影響を受けない)。
  crate 内へ移すので `use` は `daw_gui::` → `crate::` に書き換える
  (`arr_widget.rs:21-25` の `daw_gui::app::{track_with, AppData}` /
  `daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher,
  RecordingDispatcher}` はいずれも crate の `pub` 項目なのでそのまま届く。
  `daw_gui::widgets::arrangement::arrangement` は `super::arrangement`)。
  **ただし `drive_scene` はそのままでは使えない。** `arr_widget.rs:103` は
  `let _ = arrangement(app, ui, WIDGET_RECT);` で **response を捨てている**が、
  下のダンプ項目 5 は `format!("{:?}", response)` を要求する。
  `daw_gui/tests/arrange_fit_layout.rs:54-58` と同じ手口で捕捉する形に直すこと:
  ```rust
  let mut captured = None;
  host.frame(app, &mut scene, screen, frame(p), |app, ui| {
      captured = Some(arrangement(app, ui, WIDGET_RECT));
  });
  (scene, captured.expect("arrangement() は毎フレーム response を返す"))
  ```
- `UiHost::set_cursor_sink` (`ui/crates/ui/src/ui.rs:396`、
  `Box<dyn Fn(CursorIcon) + Send + Sync>`) で要求されたカーソルを `Arc<Mutex<Vec<_>>>` に拾う。
- **`header_w = 0.0` と `header_w = 160.0` の 2 パス**を流す。同じシナリオ表を 2 回まわすのではなく、
  下の表の「幅」欄に従って割り振る (header pane が存在しないパスで header シナリオを流しても
  何も起きないため)。
- 各フレームについて次を 1 つのテキストに追記:
  1. `Scene.primitives` の順序つき `{:?}` ダンプ (Scene / Primitive は `Debug` 導出済)
  2. `format!("{:?}", app.song_doc.song())`
  3. `format!("{:?}", app.ui_prefs)` + `format!("{:?}", app.selection)` (§9-0 (b) の derive)
  4. §9-0 (b) の完全分解束縛で得た `transport` / `ui_ephemeral` の全フィールドを
     1 行 1 フィールドで (`_` に落としたものは行ごと出さない)
  5. `format!("{:?}", response)` (`ArrangementResponse` は `#[derive(Clone, Debug)]`、`mod.rs:780-781`)
  6. その frame で拾ったカーソル要求
- 出力先は `std::env::var("DAW01_ARR_TRANSCRIPT")` のパス。スクラッチに書く (リポジトリには残さない)。

フレーム列に**必ず含める**シナリオ (現行テストが 1 つも触っていない領域):

| シナリオ | 幅 |
|---|---|
| 静止 (press なし) 1 本 | 両方 |
| lane 下端 splitter / track 行下端 splitter の press → hold → release | 両方 |
| **header/lanes 境界 splitter** の press → hold → release | 160 のみ (`header_resize_splitter_at` は `header_w` 依存) |
| audio grip (gain band / fade corner) | 両方 |
| clip の Move / ResizeLeft / ResizeRight / 短クリック / Shift+click / Ctrl+click / Alt+drag | 両方 |
| arranger 帯の Move / Resize / 空き帯 Create / 短クリック | 両方 |
| ruler の plain click (seek) / Shift+drag (loop NewRange / Start / End / Middle) | 両方 |
| **header pane**: volume band drag / reorder drag&drop / group disclosure / lane disclosure / M·S·R ボタン / 名前欄ダブルクリック / 行の catch-all 選択 (Shift / Ctrl つき) | **160 のみ** |
| **lane header の ★ / 👁 / ✕** | **160 のみ** |
| **popup が開いている frame の header press** (`ui.has_open_popups()` ゲート) | **160 のみ** |
| automation: point drag / Alt+click 削除 / curve handle drag / clip Move/Resize / 空き zone lasso | 両方 |
| Alt+drag フォールバック (lane resize / row resize)。`in_arr` の header 側枝も踏む | 両方 (header 側枝は 160) |
| 端オートスクロール (lanes の端でホールド) | 両方 |

手順:

1. 分割前に `cargo test -p daw_gui --lib arrangement::equivalence` を **2 回**流して
   `before1.txt` / `before2.txt` を採り、`diff -u before1.txt before2.txt` が**空**であることを
   確認する (§9-0 (b) の決定性セルフチェック)。空でなければ揺れているフィールドを `_` に落とす。
2. 分割後に `after.txt` を採り、`diff -u before1.txt after.txt` が**空**であることを確認する。
   差分が出たら、その frame と primitive の位置から原因を特定する。

**採取は分割前に済ませる。** 作業を始めてしまうと「前」の状態を再現できないので、
`git stash` / worktree / WIP commit のいずれかで分割前 HEAD を確保してから着手する。

### 9-B. 恒久テスト (`daw_gui/tests/arr_widget.rs` に追加)

`arr_widget.rs` / `arrange_fit_layout.rs` はどちらも `CARGO_BIN_EXE_daw_gui` を含まない
(= **daw_gui を起動しない**) ので `cargo test -p daw_gui --test arr_widget` で回せる。

1. **押した場所ごとに何が起きるかのテスト** — §9-A の未カバー分岐ごとに 1 本。
   `build_app` / `build_app_with_header(160.0)` / `press` / `hold` / `release` / `drive` /
   `drive_scene` を使う。適用後の `app` (Song / ui_prefs / selection) を assert する。
   **header pane 系 (volume band / reorder / disclosure / M·S·R / lane header ★👁✕) は
   `build_app_with_header(160.0)` 側に置く** (§9-0 (a))。
2. **優先順位の排他テスト** — `!splitter_press` ゲート **9 本** (`run.rs:237` / `:286` / `:337` /
   `:384` / `:633` / `:669` / `:774` / `:845` / `:932`) と `claim.point` / `claim.curve_handle` /
   `claim.session` の各読み点につき 1 本。最低限:
   - clip の内側にある lane 下端 splitter を押す → lane resize だけ起き `clip_drag` は起動しない (`:286`)
   - track 行下端 splitter を押す → row resize だけ起きる
   - audio grip の上の splitter を押す → audio drag は起動しない (`:237`)
   - arranger 帯と header 境界が重なる x を押す → header 幅 resize が起き section drag は起きない (`:337`)
   - header 境界と ruler が交差する角を押す → header 幅 resize が起き、playhead は動かない (`:384`)
   - curve handle の上を押す → point drag も automation clip drag も起動しない (`:669` / `:774`)
   - point の上を押す → automation clip drag は起動しない (`:774` の `!claim.point`)
   - Alt+drag は既存 session が立っているとき起動しない (`:845` + `claim.session`)
   - lasso は clip / point / splitter の上では起動しない (`:932` + `claim.session`)
   - popup が開いているフレームの header press は volume drag / reorder を起動しない
     (`run.rs:463` の `!ui.has_open_popups()`)
3. **描画順テスト** — `dragged_section_band_is_drawn_in_front` (`arr_widget.rs:527-563`) と
   同じ手口で `primitives` 内の位置を引き、**heavy → header の順**を assert する。
   色パレットに依存しないよう**幾何**で引く:

   - `build_app_with_header(160.0)` で 1 フレーム描く (`drive_scene`、pointer は
     `PointerFrame::default()`)。
   - marker A = heavy が最初に置く lanes 全面の背景 = `Rect { x:160, y:38, w:640, h:562 }` に
     一致する最初の `Primitive::Rect` (`draw.rs:38` の `push_filled_rect(hctx, lanes, style.bg)`)。
   - marker B = master 行 header の panel = `Rect { x:0, y:38, w:160, h:ROW_H }` に一致する
     `Primitive::Rect` (`run.rs:2154` の `ui.panel(("arr_master_thbg", 0), row, master_bg, 0.0)`)。
     heavy が置く header pane 背景 (`render.rs:77`、`h = 562`) とは `h` で区別できる。
   - `assert!(index(B) > index(A))`。

   フェーズ順の入れ替えを機械で止める唯一の手段。**`h` が同じ rect を marker にしない**
   (heavy の header pane 背景と取り違える)。

**自明な算術を写経するだけのテストは書かない。** 上の 3 種はいずれも「押した場所 → 起きること」
「積まれた順序」という**観測可能な振る舞い**を assert するもので、本番の式を写していない。

---

## 10. 実機 sign-off

`docs/plan_arch_refactor.md:419` 以降が S4b/c の完了判定として実機目視を必達にしている。
同じ関数の分割なので同じ基準を適用する。

**起動は事前にユーザーへ断る** (窓が前面に出て作業を妨げる)。断ってから
`cargo build -p daw_gui` → `cargo run -p daw_gui` を background で起動する。
`| tail` に流さない。二重起動しない (先に `tasklist` で確認)。

目視項目: clip の drag / resize / split / 短クリック選択、Arranger 帯の作成・移動・リサイズ、
automation lane の point 編集 / lasso / clip 移動 / カーブハンドル、track reorder のドロップ位置、
ruler の seek と Shift+loop、lane/row/header の splitter リサイズ、端オートスクロール、
track header の M·S·R / 名前変更 / group 折り畳み。

---

## 11. 検証コマンド

コマンドは `&&` / `;` で連結せず、1 つずつ実行する。作業ディレクトリへの `cd` を前置しない。
**結果をパイプに流さない** (パイプの exit code は `tail` のもの)。ファイルに落として grep する。

```
make check
make clippy
make test-nolaunch
make arch-lint
cargo test -p daw_gui --test arr_widget
cargo test -p daw_gui --test arrange_fit_layout
```

作業中だけ使う (完了後に test ファイルごと消す):

```
cargo test -p daw_gui --lib arrangement::equivalence
```

`--lib` は crate 内の unit test だけを回すので、`CARGO_BIN_EXE_daw_gui` を含む
`daw_gui/tests/*.rs` を 1 つもビルド / 実行しない (= daw_gui を起動しない)。

`make test` は使わない (`CARGO_BIN_EXE_daw_gui` を含む target が daw_gui を起動し、
開いているプロジェクトの再生を壊す)。

`make clippy` は `cargo clippy --workspace --all-targets -- -D warnings` (`Makefile:168-169`)。
`render.rs` / `release.rs` の `#![allow(clippy::too_many_arguments)]` を削除した状態で通ることを確認する。

---

## 12. スコープ外であることの確認 (3 プロセス / RT / IPC)

変更は `daw_gui` の widget 内部に閉じる。

- `common` の protocol 型・bincode derive・`common/build.rs` の `WIRE_SOURCES`・shmem に触れない
  → 子 exe (`daw_audio` / `daw_plugin_host`) の再ビルドは不要。
- オーディオコールバック / CLAP `process()` のパスに触れない → RT 制約 (ヒープ確保 / ロック / I/O) は無関係。
- `make arch-lint` の 12 チェックのうち `FILE-BUDGET` / `FN-BUDGET` / `FN-NESTING` が
  **直接関係する** (r.md #76 で指標が実コード行 + 関数長 + ネストへ入れ替わった)。
  分割は違反を減らす方向に動くが、**受け取り側 (`render.rs`) が新たに閾値を超えないこと**を
  `python scripts/loc_budget.py --report` で確認する。
- 安定 id addressing: `ArrangementFrame` は positional index を新たに導入しない。
  `visible_tracks` の index は**このフレームの描画順**として既存コードが使っているもので、
  プロセス境界・イベント・永続参照には出ない (clip / lane / point / section はすべて安定 id で
  参照される `ClipKey` / `AutomationLaneKey` / `AutomationClipKey` / `AutomationPointKey`)。
- daw-ui core (`ui/crates`) は編集しない。DAW 固有 widget は `daw_gui/src/widgets/` のまま。
- `edit_song()` チョークポイントは widget からは触らない (すべて `Edit::mutate` →
  `AppData::handle_event` 経由)。この構造を変えない。

---

## 13. 参照 (すべて実測で確認済み)

- `daw_gui/src/widgets/arrangement/run.rs:1-7` — doc と `#[allow(clippy::too_many_lines)] pub fn arrangement`
- `run.rs:24-59` — rect 分割 (`header_pane.y == lanes.y` の由来は `:27-30` のコメント)
- `run.rs:67-100` — `visible_tracks` / `press_tops` / `is_group_set`
- `run.rs:102-123` — r.md #63 のレイアウトミラー (`last_arrange_lanes_size` / `last_arrange_rows`)
- `run.rs:129` / `:132` / `:135` / `:137` と `:983-998` — press の遅延発火スロット 4 本と発火
- `run.rs:147-150` — `state.press_modifiers` の記録
- `run.rs:168-230` — `splitter_press` (以降 **9 か所**のゲート: `237` / `286` / `337` / `384` /
  `633` / `669` / `774` / `845` / `932`。`219` / `336` / `386` はコメント)
- `run.rs:462-467` — header press の外側ゲート (`header_w > 0.0` / `!ui.has_open_popups()`)
- `run.rs:848-861` / `:935-948` — 11 session を列挙する `no_session` の重複
  (読み点は `:866` / `:949`)
- press 中に session を起動する **17 行**: `176` / `204` / `221` / `273` / `323` / `352` / `370` /
  `429` / `444` / `510` / `549` / `649` / `738` / `822` / `887` / `909` / `973`。
  `176` と `887` は `state.automation_lane_resize_drag =` と `Some(..)` が別行なので
  `state[.][a-z_]* = Some[(]` の 1 行 grep には出ない
- `run.rs:1193` — `state.edge_scroll_press` の唯一の書き込み (囲みブロックは `1190-1198`、
  読みは `:1196`)
- `run.rs:1000-1006` — 右クリック context menu は caller 責務、という唯一の記録 (→ `rects.rs` の doc)
- `run.rs:1008-1177` — 14 session の continuation。巻き戻し判定は
  clip=1032 / section=1044 / loop=1054 / reorder=1065(y)・1068(x) / volume=1073 / audio=1093 /
  point=1113 / automation_clip=1148 / lasso=1157 / curve=1170、
  巻き戻し対応なし= playhead 1079-1083 / lane resize 1119-1123 / row resize 1126-1130 /
  header resize 1133-1137 (各 1-2 行手前のコメントは移設対象)
- `run.rs:1400-1650` — session snapshot / release take。`:1513-1520` が `pending_reorder_hash`、
  `:1625` が `response.automation_lasso_active`
- `run.rs:1693` — `response.hovered_clip` の確定
- `run.rs:1919-1924` — `Arc::from(visible_tracks.clone())` と「'static 制約」コメント
- `run.rs:1929` / `:2066` — heavy の `'static` 誤解コメント
- `run.rs:1947-1948` — `automation_clip_drag_session` の原本を最後まで生かす意図
- `run.rs:1976` — `let header_pane_copy = header_pane;`
- `run.rs:1980` — `sections_for_draw: Vec<SectionView> = sections.to_vec();` (heavy へ owned で
  渡すためだけの毎フレーム clone。使用は `render.rs:794` の 1 か所 → `&f.sections` に落ちて消える)
- `run.rs:1981` — `section_drag_overlay` (→ `Overlays::section`)
- `run.rs:1756-1759` / `:1762-1765` — cursor が `widget_state` から直接読む `resize_active` /
  `header_resize_active` (不変条件 6。release take より後という位置が意味を持つ)
- `run.rs:2095-2099` — heavy dispatch → `commit_releases` (z 順の不変条件)
- `run.rs:2110-2127` — `with_clip_rect` の open-code (`:2114-2117`) / `visible_idx_for_headers`
  (`:2118`) / `header_tops` (`:2122-2127`)
- `run.rs:2130` — `if header_w > 0.0` (header 行描画のゲート)
- `run.rs:2154` — master 行 header の `ui.panel(("arr_master_thbg", 0), row, master_bg, 0.0)`
  (§9-B 3 の marker B)
- `run.rs:2417` — 名前欄 double-click → `AppEvent::BeginRenameTrack`
- `run.rs:2462` — header の clip scope 復元
- `run.rs:2467-2501` — `disclosure_clicked` / `clicked_track_for_select` の確定 (modifier は `:2480-2483`)。
  `:2468` は `AppEvent` を経由せず `app.ui_prefs.collapsed_groups` を直接 toggle する
  (`AppEvent::ToggleGroupCollapsed` という variant は**存在しない** — `run.rs:2298` / `:2464` /
  `view/mixer_strips.rs:500` のコメント中の呼び名だけ)。`:2469` で `clicked_track_for_select` を落とす
- `run.rs:2696` — `response.dragging_automation_clip` (rect 収集の後)
- `render.rs:5` — `#![allow(clippy::too_many_arguments)]`
- `render.rs:9-53` — 37 引数の署名 (`hctx` + 36 値)。`:16-17` で `header_pane` / `header_pane_copy` を二重に受ける
- `render.rs:57-64` — tops の 3 回目の再計算と 'static の理由づけ
- `render.rs:76-80` — `hctx.cached` の入口。`:77` が header pane 背景、`:80` が `draw_lanes_bg`
- `draw.rs:38` — `push_filled_rect(hctx, lanes, style.bg)` (§9-B 3 の marker A)
- `draw.rs:45` — filter 済リストへの恒等 `compute_visible_indices` (撤去対象)
- `release.rs:5` — `#![allow(clippy::too_many_arguments)]`
- `release.rs:10-43` — 33 引数の署名。`:17` で `master_row: Option<&ArrangementMasterRow>`
- `release.rs:822-855` — marquee ゲート。`marquee_zone_ok` (`:822`) が `primary_just_pressed` を
  要求するので **press フレームで走り**、`:842-855` が 11 session を `ui.widget_state` から読む
  (`live` にも `released` にも置き換えられない。§6-K)
- `mod.rs:59-69` — 子モジュール宣言 (`use draw::*; use geometry::*;`) の既存の流儀。
  `:65-67` の **`mod render;` → `mod release;` → `mod run;` という順序は動かさない**
- `mod.rs:2071` — `state.loop_drag.is_some() || state.playhead_drag.is_some()` (端スクロール軸判定。
  `&ArrangementState` を直接受けるので `LiveSessions` を要らない側の根拠)
- `mod.rs:780-781` — `#[derive(Clone, Debug)] pub struct ArrangementResponse`
- `mod.rs:1434-1462` — `is_visible_track` / `compute_visible_indices`
- `mod.rs:1900-1953` — `ArrangementState` の 14 session + `edge_scroll_press` (`:1947`) +
  `press_modifiers` (`:1952`)
- `mod.rs:2412-2413` — `#[cfg(test)] mod tests;` (`tests.rs` は run/render/release を参照しない)
- `view_build.rs:33-47` — `pub(super) struct BuiltArrangement` と `build()` (re-export 無し)
- `view_build.rs:268` / `geometry.rs:96-98` — `lanes_w = area.w - arrange_header_w` /
  `view_len_beats = lanes_w / zoom` (= `beat_per_px` が `header_w` に依らない根拠)
- `daw_gui/src/app.rs:304` — `arrange_header_w: 160.0` (production default)
- `daw_gui/src/state/ui_prefs.rs:4` / `selection.rs:6` / `transport.rs:6` /
  `ui_ephemeral.rs:12` — **どれも derive を持たない** (§9-0 (b))
- `daw_gui/src/state/ui_ephemeral.rs` — 86 フィールド。`Instant` は `:269` `anim_epoch` /
  `:274` `frame_now`、`TextureHandle` は `:35` / `:44` / `:53`。
  **`pub(crate)` は `:15` `arr_label_cache` / `:18` `tempo_map_cache` / `:118` `home_toggle_at_first` /
  `:175` `arrange_zoom_history` / `:178` `arrange_zoom_anchor` の 5 つ** (= 統合テストからは見えない)
- `daw_gui/src/state/transport.rs:13-116` — 20 フィールド (全部 `pub`)。`Instant` は `:50`
  `panic_reinit_due` / `:89` `export_progress_at`、`:92` `export_cancel: Option<Arc<AtomicBool>>`
- `daw_gui/src/app_types.rs:1088` / `:1097` — `#[derive(Debug, Clone)] struct TouchedParam` と
  その中の `touched_at: Instant` (Debug は通るが値が毎回変わる例)
- `daw_gui/src/widgets/time_grid.rs:551-559` — crate 内 `#[cfg(test)]` から `UiHost::frame` を
  回す前例 (`run_frame`)
- `daw_gui/src/app_tests.rs:1099` — crate 内 test で `AppData::new` する前例
- `daw_gui/src/view/arrangement_view.rs:74` — 唯一の production caller (署名不変なら編集不要)
- `daw_gui/src/view/arrangement_view.rs:83-85` — `ui_ephemeral.arrange_hover_content` を書くのは
  **caller** であって widget ではない (`view_build.rs:87` / `:106` が読む)。widget を直接駆動する
  トランスクリプトでは動かないが、完全分解束縛なら勝手に観測対象に入る
- `daw_gui/tests/arr_widget.rs:35-115` — `build_app` / `press` / `hold` / `release` / `drive_scene`
- `daw_gui/tests/arr_widget.rs:103` — `let _ = arrangement(app, ui, WIDGET_RECT);`
  (= `drive_scene` は response を捨てている。§9-A で捕捉する形に直す)
- `daw_gui/tests/arr_widget.rs:53` — `app.ui_prefs.arrange_header_w = 0.0` (header 帯が踏めない原因)
- `daw_gui/tests/arr_widget.rs:95-98` — 「primitives は call order = z-order」
- `daw_gui/tests/arr_widget.rs:527-563` — 描画順を assert する既存テストの前例
- `daw_gui/tests/arrange_fit_layout.rs:50-59` — `response` を捕捉する drive ヘルパ
  (`arrange_header_w` を設定しないので default 160 で走る = header 描画は踏む)
- `ui/crates/ui/src/widgets/heavy.rs:53-56` — `heavy` の署名 ('static 境界なし)
- `ui/crates/ui/src/widgets/heavy.rs:76-79` — `cached` の署名 ('static 境界なし)
- `ui/crates/ui/src/widgets/heavy.rs:163` — `HeavyCtx::push_edit` (現存)
- `ui/crates/ui/src/widgets/heavy.rs:177` — `HeavyCtx::with_clip_rect`
- `ui/crates/ui/src/ui.rs:396` — `UiHost::set_cursor_sink` (headless でカーソル要求を拾う)
- `ui/crates/ui/src/ui.rs:1083` / `:1087` — `Ui::palette` (`&'a Palette`) / `Ui::pointer`
- `ui/crates/ui/src/ui.rs:1095-1103` — `current_clip_rect` / `set_current_clip_rect`
- `ui/crates/ui/src/ui.rs:1154-1162` — `Ui::with_clip_rect`
- `ui/crates/ui/src/ui.rs:1550` — `Ui::push_edit` は `pub`
- `ui/crates/ui/src/ui.rs:2588` — `Ui::widget_state` (`&mut Ui` を占有)
- `docs/plan_arch_refactor.md:399-403` — S4b の宣言「4,600 行関数を interaction 単位に分解」
- `docs/plan_arch_refactor.md:419-` — S4b/c は build/test green に加え実機 sign-off が完了判定
- `scripts/loc_budget.py` — サイズ budget (実コード 1,000 行 / 関数 300 行 / インデント 6 段)。
  `arch_lint.sh` の check 6-10 がこれを呼ぶ
- `Makefile:168-169` — `cargo clippy --workspace --all-targets -- -D warnings`
- `Cargo.toml:124-125` — 「daw_01 既存 crate は `[lints]` を opt-in しないので pedantic は
  適用されない」(= `too_many_lines` は最初から無効、`too_many_arguments` は complexity で有効)。
  なお r.md #76 で `[workspace.lints.clippy] too_many_lines = "allow"` を足し、
  関数長ゲートは `scripts/loc_budget.py` に一本化済み (ui/crates だけ閾値 100 が効く非対称を解消)

---

## 14. 裏取りで受けた指摘への対応

この計画は 2026-08-28 に実コードとの突き合わせレビューを受けた。指摘とその処理:

| 指摘 | 判定 | 対応 |
|---|---|---|
| `mod.rs` に `use <新モジュール>::*;` が無いと全 signature が名前解決しない | **正** (`mod.rs:59-69` の `use draw::*; use geometry::*;` を実測) | §4 に `use frame::*; use press::*; use sessions::*;` を明記。glob を足す範囲の判断基準も書いた |
| `BuiltArrangement` は `pub(super)` で re-export されておらずパスが要る | **正** (`view_build.rs:33`、`mod.rs` に re-export 無し) | §6-A に `use super::view_build::BuiltArrangement;` を追加 |
| `UiPrefs` / `UiEphemeral` は `Debug` を持たずトランスクリプトがコンパイルしない | **正** (`ui_prefs.rs:4` / `ui_ephemeral.rs:12` に derive 無し。`SelectionState` / `TransportState` も同様) | §9-0 (b) を新設。`UiPrefs` / `SelectionState` は derive を足し、`Instant` を持つ `TransportState` / `UiEphemeral` はフィールドを列挙してダンプ (**第 2 稿で「到達可能フィールドの allowlist」から「`..` 無しの完全分解束縛」へ変更**、下表) |
| fixture が `arrange_header_w = 0.0` で header 帯 568 行を 1 度も踏まない | **正** (`arr_widget.rs:53` / `run.rs:462` / `run.rs:2130`) | §9-0 (a) を新設。`build_app_with_header` と 2 パス構成、座標読み替え表を追加。§9-B の描画順テストも `header_w=160` 側へ |
| `arranger` / `ruler` に `&PressClaim` を渡しながら `claim.session = true` を要求していて自己矛盾 | **正** | §6-B で全分岐を `&mut PressClaim` に統一。起動行 (第 2 稿で 17 行に訂正) と「立てる/立てない」の対応表を追加。ゲートが先に閉じるので現行コードでは結果に影響しないことも明記 |
| 「`splitter` が書くのは `header_resize_drag` だけ」は誤り。`automation_lane_resize_drag` (`:176`) と `track_row_resize_drag` (`:204`) も書く | **正** | §6-B の対応表で 3 つとも列挙 |
| `splitter_press` のゲートは 10 か所でなく 9 か所 | **正** (`237`/`286`/`337`/`384`/`633`/`669`/`774`/`845`/`932`。`219`/`336`/`386` はコメント) | §2 / §6-B / §9-B / §13 を 9 に統一し、9 本を列挙 |
| 完了条件 3 (`too_many_lines` allow 削除) は clippy のゲートにならない | **正** (`daw_gui/Cargo.toml` に `[lints]` 無し、`Cargo.toml:124-125`) | §0 に非対称を明記。新設する長い関数も clippy では止まらないので、長さは §4 の見積りで守ると書いた |
| 行番号のずれ (in_arr 838→841、header_resize 214-224→215-227、clip zone 236→237、else if 281→286、curve 621→633、巻き戻し判定の各行、`arr_widget.rs` 36-110→35-115、`arrange_fit_layout.rs` 52-61→50-59 ほか) | **正** | 全件を実測値に修正。追加で `press_modifiers` 146-149→147-150、`hovered_clip` 1690→1693、header indent 481-487→482-488、`press_delete_point` 699→688、`render.rs` 署名 9-52→9-53 も修正 |
| `run.rs:1000-1006` のコメントだけ移設先が無い | **正** (他の範囲は穴なく被覆) | §4 と §6-I で `rects.rs` のモジュール doc へ移すと明記 |

**指摘のうち、計画を変えなかったもの**: 「`accept_release_pos` が現行の 4 通りを 1 関数にまとめる」
という方針そのものは維持する (ユーザー承認済みの方針であり、規則の統一はしない)。ただし
現行が **2 通りの構文形**を持つことは指摘で気付いたので §6-E2 に書き足した。

また、レビューで **問題なしと確認された**主要な事実 (render.rs = 37 引数 / release.rs = 33 引数 /
`*_release` 10 種 / drag session 14 種 / `no_session` が 2 か所とも同一の 11 列挙 /
`heavy` `cached` に `'static` 境界が無い / `Ui::with_clip_rect` が open-code と完全同一 /
`compute_visible_indices` の恒等性 / `render.rs` に `push_edit` 0 件 / IPC・RT・arch-lint は無関係 /
`arr_widget.rs` `arrange_fit_layout.rs` は daw_gui を起動しない) は、この計画の該当節で
引用元つきに整えたうえで維持している。

### 14-2. 第 2 稿 (2 回目のレビュー) で直したもの

BLOCKING は 0 件。引用の裏取り (実ファイルとの全件照合)・呼び出し側の洗い出し・受け入れ条件の
実走 (`bash scripts/arch_lint.sh` = exit 0 / `arr_widget` 15 passed / `arrange_fit_layout` 3 passed) は
すべて計画どおりだった。残りの指摘の処理:

| 指摘 | 判定 | 対応 (反映先) |
|---|---|---|
| §6-B 本文の「16 か所」は実際には **17 か所** (`176`/`887` は `= Some(` が改行で割れて 1 行 grep に出ない。表は 352/370 を 1 行にまとめていた) | **正** | §6-B の本文を 17 に訂正し、17 行を全部書き下した。表も `352` / `370` を別行に分けて 17 行に。§13 に「grep に出ない 2 行」を明記 |
| §9-A が複製すると書いた `drive_scene` は response を捨てている (`arr_widget.rs:103`) のに、ダンプ項目 5 が `format!("{:?}", response)` を要求していて成立しない | **正** | §9-A に `arrange_fit_layout.rs:54-58` 方式の捕捉コードを直書き。§13 に `arr_widget.rs:103` を追加 |
| §6-F は `sessions::overlays` が `sections_for_draw` も吸収すると書くが、`Overlays` に該当フィールドは無く、§6-J は `f.sections` に落としている (矛盾) | **正** (§6-J が正しい) | §6-F の由来を `run.rs:1981` だけに絞り、「`:1980` の `Vec` clone は `Overlays` に入れず消えるだけ」を明示。§13 に `:1980` / `:1981` を分けて追加 |
| §4 の `mod.rs` スニペットが既存行の順序 (`mod render;` → `mod release;`) を入れ替えている | **正** (`mod.rs:65-67`) | スニペットを実ファイルの順序に戻し、「既存 3 行を動かさない」を注記。§13 にも明記 |
| 行番号の軽微なずれ: 描画順テスト `528-563` / `527-563` の書き分け、巻き戻し非対応 3 件の開始行 (コメント行を含んでいた)、`edge_scroll_press` の書き込み行、§6-F の cursor 読み `1756-1764`、`model.rs:275/286/311/325` の名前の対応順、derive 行 vs struct 行 | **正** | 全件を実測値に統一 (`527-563` / `1119-1123`・`1126-1130`・`1133-1137` / `:1193` / `:1756-1759`・`:1762-1765` / `275=EditorWindowGeometry` `286=PianoRollViewState` `311=AudioEditorViewState` `325=FollowMode` / derive 行であることを明記) |
| §9-0 (b) の `ui_ephemeral` 列挙は「widget が emit する `AppEvent` → **ハンドラが書くフィールド**」の 1 段を辿っていないので穴が残り得る | **正** | 列挙 (allowlist) をやめ、**`..` を使わない完全分解束縛**に変更 (フィールド増減でコンパイルが落ちる)。あわせて `UiEphemeral` の `pub(crate)` フィールド 5 つ (うち 2 つは `arrange_zoom_history` / `arrange_zoom_anchor` = arrangement のズーム履歴) が `daw_gui/tests/` から見えないことが判明したので、**トランスクリプトを crate 内 `#[cfg(test)] mod equivalence;` へ移した**。前後 2 回採って diff が空であることを先に確かめる決定性セルフチェックも追加 |
| `LiveSessions` から `playhead_drag` を外した理由が書かれていない (判断自体は正しい) | **正** | §6-F の doc に「live snapshot を読む消費者が 1 つも無い」ことを参照元 (`release.rs:851` / `mod.rs:2071` はどちらも `widget_state` 直読み) つきで明記。復活させたければ消費者を 1 つ挙げること、も書いた |

第 2 稿で自分の照合中に見つけて直したもの (レビュー指摘の外):

| 見つけたもの | 対応 |
|---|---|
| §6-H の doc が disclosure を `ToggleGroupCollapsed` と書いていたが、`AppEvent` にその variant は存在せず `run.rs:2468` が `ui_prefs.collapsed_groups` を直接 toggle している | §6-H を実装どおりに書き直し、`AppEvent::ToggleTrackAutomationCollapsed` との混同を注記 |
| §6-K の置換表を「引数だけ」と読まずに `release.rs:842-855` の `widget_state` 読みまで `released.*` へ寄せると壊れる (このゲートは **press フレーム**で走り、`LiveSessions` が外した 3 session を読む) | §6-K に禁止事項として追記。§13 に `release.rs:822-855` を追加 |
