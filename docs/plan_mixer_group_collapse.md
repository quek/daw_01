# plan_mixer_group_collapse — mixer の group strip を折り畳み可能にする

FIXME #7。「ミキサーのグループトラックをアレンジメントのように子を折り畳めるように
してほしい」。

> **supersede (r.md #74 / [plan_rmd_74_disclosure_glyph.md](plan_rmd_74_disclosure_glyph.md))**:
> 本書の「glyph は arrangement と揃える (▶/▼)」 (確定仕様 表 #3 / 実装方針) は **#74 で反転した**。
> mixer は strip が横に並び group の子が **右** に現れるので、開示軸は Inline =
> **展開中 ▶ / 折り畳み中 ▼** で arrangement の裏返しになる。
> **「arrangement と同じ toggle 経路を使う」方針は #74 でも有効**
> (#74 で `AppEvent::ToggleGroupCollapsed` が実在するようになり、本書が想定した
> 「既存 `ToggleGroupCollapsed` 相当を mixer からも発火」が初めて字義どおり成立する)。
> 同じ文の「色は `disclosure_color` と揃える」は **#74 とは無関係に、そもそも実装されたことがない**。
> `disclosure_color` は `ArrangementStyle` の field で読み手は arrangement widget だけであり、
> mixer の disclosure は `ui.button_at` の枠付きボタン = button の text 色を使う。
> presentation (枠の有無 / 色) を片方へ寄せるかは #74 のスコープ外
> (#74 §5)。

## 現状 (2026-06-08)

- mixer ([mixer_strips.rs:106-209](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs))
  は `track_mix()` ([app.rs:1593-1661](F:/dev/daw_01/daw_gui/src/app.rs)) の
  normals を **全 track 横並び**で描画。子 strip を常に全部出し、折り畳みは無い。
  group strip は名前 prefix `"↳".repeat(depth)` で深さを示すのみ
  ([mixer_strips.rs:222-226](F:/dev/daw_01/daw_gui/src/view/mixer_strips.rs))。
- arrangement は折り畳み済: 状態 SSoT は **`AppData.collapsed_groups: HashSet<u32>`**
  ([app.rs:644](F:/dev/daw_01/daw_gui/src/app.rs))。toggle は
  ([arrangement_view.rs:1409-1416](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs))、
  disclosure 三角 ▶/▼ と「親 chain のいずれかが collapsed なら子を hide」判定は
  gui_01 arrangement widget が所有 (`is_visible_track` 親 chain walk)。

## 確定仕様 (grill-me 2026-06-08)

mixer の group strip にも arrangement と同じ折り畳み機構を持たせる。**折り畳み
状態は `collapsed_groups` を arrangement と共有 (SSoT 1 つ)**。どちらの view で
畳んでも両方に反映され、view 間で食い違わない。

| # | 項目 | 内容 |
|---|---|---|
| 1 | state | **既存 `collapsed_groups` を mixer も参照** (別 state を持たない)。arrangement の toggle / 永続化をそのまま継承 |
| 2 | 可視判定 | mixer は gui_01 widget でなく daw_01 が直接描くので、**daw_01 側で「親 chain のいずれかが `collapsed_groups` に含まれる strip を skip」フィルタ**を実装 (arrangement widget の `is_visible_track` と同ロジックを daw_01 で。`group_compose.rs` 等に helper 化して SSoT 共有が望ましい) |
| 3 | disclosure | group strip の header に **クリック可能な ▶/▼**。click → `collapsed_groups` を toggle (arrangement と**同じ toggle**経路を使い同期保証)。group 自身の strip は畳んでも残す (子だけ隠す) |
| 4 | レイアウト | strip 幅 80px は据え置き。group strip 名前行の左に小さな disclosure glyph を置く (現 `"↳"` depth prefix を disclosure 三角に置換/併用)。M/S/knob/fader 構成は不変 |

## 実装方針

- `mixer_strips.rs::draw` の normals ループに **collapse フィルタ**を追加:
  各 entry の `parent_group_id` chain を root まで walk し、途中の group id が
  `collapsed_groups` に含まれれば描画 skip (= arrangement の可視 track と一致)。
  `TrackMixEntry` に `parent_group_id` が無いので、フィルタ用に追加するか
  loop 内で `song` lookup する。
- group strip header に disclosure rect を確保し `ui.button_at` 等で hit-test。
  click → arrangement と同じ collapse toggle (`collapsed_groups` insert/remove)。
  既存 `ToggleGroupCollapsed` 相当を mixer からも発火 (handler 共通化、二重定義
  しない)。
- glyph / 色は arrangement の disclosure と揃える (▶/▼、`disclosure_color`)。
- 永続化は `collapsed_groups` の現行方針を継承 (arrangement と同じ。session/save
  の扱いは現状確認の上、共有なので新規判断は不要)。

## 受け入れ基準

- mixer の group strip の ▶/▼ を click → その group の子 strip が mixer から
  隠れ、group strip だけ残る。再 click で復帰。
- mixer で畳むと **arrangement でも同じ group が畳まれる** (逆も同様)。
  = `collapsed_groups` 共有で view 間一貫。
- M/S/pan/fader/meter など strip の他要素は不変 (regression なし)。

## 非範囲

- mixer 独立の折り畳み状態 (grill で「arrangement と共有」を選択。Reaper TCP/MCP
  式の独立 state は採らない)。
- strip 幅の動的拡大や header レイアウトの大幅再設計 (disclosure を既存 80px に
  収める。収まらなければ最小の調整に留める)。
