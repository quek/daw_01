# plan_rmd_74_disclosure_glyph — 開閉マーク (disclosure) の向きを「開示軸」で決める

**この計画は #74 専用であり、他項目との統合順は [docs/plan_rmd_index.md](plan_rmd_index.md) を見ること。**

r.md #74 (原文ママ)。「Mixer のグループの▶と▼逆では？ 前者が子トラックも表示しているとき、
後者が小トラックを表示していないときの方が自然ではないでしょうか。」

(2 つ目は原文が「小トラック」。「子トラック」の typo と読めるが、**引用は原文のまま置く**。
r.md は編集しない (memory `feedback_defer_todos_to_fixme`)。)

この計画書だけで完走できるように書いてある。着手前に本文を通読すること。
行番号は **2026-08-28 時点 (commit `cc608d0`) の実測値**で、本書の全 file:line は
2 度の改訂で 1 件ずつ現物で確認済み (§10 に確認結果)。


## 1. 何を直すのか (確定仕様 / ユーザー承認済み・変更禁止)

### 1.1 見える挙動

| 画面 | 折り畳み中 (子が見えない) | 展開中 (子が見える) | 変更 |
|---|---|---|---|
| **Mixer** の group strip | **▼** U+25BC | **▶** U+25B6 | **入れ替える** |
| **Arrangement** の group track header | ▶ U+25B6 | ▼ U+25BC | **現状維持** |
| **Track Inspector** の modulation rack 行 | ▶ U+25B6 | ▼ U+25BC | 向きは現状と同義。**グリフ族が ▸/▾ → ▶/▼ に変わる** (下記 §4.7) |
| automation lane の開閉 (arrangement) | `+` | `-` | 変更なし (doc だけ直す) |

Mixer だけ入れ替える理由: mixer は strip が**横**に並び、group を開くと子は**右**に現れる
(`daw_gui/src/handler/grouping.rs:103-104` が group track を最上位の子の**直前**へ insert し、
`daw_gui/src/handler/view_model.rs:156` の `track_mix()` がその順を保ち、
`daw_gui/src/view/mixer_strips.rs:215-217` が index 昇順で左→右に置く)。
つまり mixer で ▼ が指す「下」には何も無く、グリフが情報を運んでいなかった。

Arrangement を触らない理由: 縦リストの業界標準 (Apple HIG / WinUI TreeView /
CSS Counter Styles Level 3 §6.3 の `disclosure-closed` = 右向き) がそのまま当てはまる。

**同じ group が Arrangement と Mixer で別のマークになるのは承知の上の仕様**である。
三角は「状態」ではなく「中身がどちらに開くか」を伝える、という 1 つの規則の帰結。
これを「不整合」とみなして片方に揃え直さないこと。

### 1.2 構造として直すこと (ここが本体)

グリフを `bool` 1 個から直接引くのをやめ、**開示方向 (reveal axis) を引数に取る関数 1 本**に
集約する。現在このリテラルは **3 か所**に複製されており、しかも 1 つは別 codepoint を使っている。
さらに **toggle ロジックも 2 か所**にインライン複製され、その両方のコメントが
**存在しない `AppEvent::ToggleGroupCollapsed`** を指す幽霊になっている。

「片方だけ直して逆転が残る」を構造的に起こせなくするのが #74 の完了条件であり、
グリフの向きを入れ替えるだけでは未完了。

### 1.3 SSoT 化の適用範囲を決める 1 つの規則

本計画は寸法 (`DISCLOSURE_ZONE_W` / modulation rack の `18.0`) にも手を入れる。
どこまで触るかは次の 1 行で決まる。**判断に「影響範囲」「規模」は使わない。**

> **ある量を名前付き SSoT に昇格させたら、その量のコピーは 1 つ残らず移行する。
> 昇格させない量には触らない。**

半分だけ移行した SSoT は、無い状態より悪い (`STRIP_PAD` を変えても移行漏れの
`6.0` が付いてこず、黙ってズレる)。逆に、値がたまたま同じだけの**別の量**
(M/S 行の下マージンの `6.0` 等) を同じ定数に畳むのは偽の SSoT で、独立に動かせる
べき 2 つのつまみが連動してしまう。#74 が昇格させる量は次の 2 つだけ:

- strip の内側余白 (mixer) — `DISCLOSURE_ZONE_W` を実 geometry から導くために必要
- disclosure ボタンの幅と、その右にある要素までの間隔 (mixer / modulation rack)


## 2. 着手の前提 (順序と anchor の探し方)

- **#77 (`daw_gui/src/widgets/arrangement/run.rs` の全面分割) と #71 が main へ入った後に着手する。**
  順序の正本は `docs/plan_rmd_index.md` (第 2 波: #73 → #74)。**この順序は変えない。**
  - **#77 との衝突**: #74 は run.rs の 4 か所を触るので、分割前に触ると必ず衝突する
    (run.rs は現在 2,699 行すべてが `pub fn arrangement` 1 関数 = r.md #77)。
  - **#71 との衝突**: 重なるのは **`daw_gui/src/event.rs` と `daw_gui/src/app.rs`** の 2 ファイル。
    #71 は `AppEvent` の 10 変種を device_id 化し新規 2 変種を足す
    (`docs/plan_rmd_71_device_copy.md:66` / `:480` / `:729`)、#74 は同じ enum に 1 変種を挿す
    (§4.3)。`app.rs` も #71 が cache 初期化 (`:218-237` / `:407-408`、同計画 `:466`)、
    #74 が `handle_event` の match arm (§4.4) と、同ファイル別領域を触る。
    どちらも enum / match への挿入なので行単位マージは可能だが、**後から入る側が
    「挿入位置の anchor が動いている」ことだけ確認すればよい**ようにこの順にする。
  - **`mixer_strips.rs` は衝突しない。** index は「#74 と #71 が `mixer_strips.rs` と
    `track_inspector/` を両方触る」と書いているが、#71 の計画は
    `plan_rmd_71_device_copy.md:103` / `:114` / `:1516` で **ミキサーは無変更**
    (「`mixer_strips.rs` には `device` の語が 1 つも無い」) と明言している。
    `track_inspector/` も #71 が触るのは `mod.rs` / `chain_sections.rs` / 新規 `device_panel.rs`
    (同計画 `:49` / `:84` / `:85`) で、#74 が触る `modulation_rack.rs` とはファイルが違う。
    **順序を変える理由にはならない** (順序は index が正本) が、
    「mixer 側で #71 の変更を待つ」必要は無い、という事実として記録しておく。
- 本書の `run.rs:NNNN` は **commit `cc608d0` 時点の行番号**。#77 後は
  `daw_gui/src/widgets/arrangement/header.rs` 等へ移動している見込みなので、
  **行番号ではなく次の anchor 文字列で探すこと**:

  ```
  grep -rn "disclosure_rect_for" daw_gui/src/widgets/arrangement/
  grep -rn "collapsed_groups" daw_gui/src/widgets/arrangement/
  grep -rn "disclosure_clicked" daw_gui/src/widgets/arrangement/
  ```

  1 つ目が「group disclosure のグリフ描画 + hit-test」、3 つ目が「click → toggle 発行」を含む
  ファイルを指す。#77 の分割単位次第では、この 2 つが別ファイルへ離れる
  (現状は同一関数内の別ブロックで、間に 160 行ある)。

### 2.1 最終確認 grep (実装後に必ず走らせる)

**この grep は「0 件になる」ものではない。**glyph 集合は disclosure 以外の正当な用途
(再生ボタンの ▶、dropdown の ▼、routing ラベルの区切り ▸、ASCII 図) でも使われている。
旧版の本節は「0 件」と「コメントは残ってよい」を並べて書いていて自己矛盾していたので、
**期待される残存を名指しで固定**する。

(a) **コード行の検査** — doc/コメント専用行を落として、残ったコード行を目視する:

```
grep -rn -E "25b6|25bc|25b8|25be|▶|▼|▸|▾" --include=*.rs daw_gui/src | grep -v -E "^[^:]+:[0-9]+: *(//|///|//!)"
```

期待される結果は **次の 2 つだけ**:

1. `daw_gui/src/view/disclosure.rs` の行 (新規 SSoT 本体 + そのテスト)
2. `daw_gui/src/handler/view_model.rs:463`
   `format!("{} \u{25b8} {label}", …)` — **disclosure ではない**。他トラックにある
   modulation routing のラベルに付ける「トラック名 ▸ 対象」の区切り記号で、doc は
   `handler/view_model.rs:437`、`daw_gui/tests/app_state/modulation_arm.rs:187` が
   `rows[0].label.starts_with("Drums \u{25b8} ")` で assert している。
   **ここを「取り逃し」と誤認して直すとテストが落ちる。**

`if .* { "…" } else { "…" }` の形が 1 件でも残っていたら SSoT 化が未完了。

(b) **doc/コメントの検査** — 上の grep の全ヒットを見て、automation lane を `▶/▼` と
書いた doc が 0 件であること (§7 の受け入れ基準)。触らないコメントの一覧は §4 各節の
「触らないもの」に列挙してある。


## 3. 設計 — 規則は 1 つだけ

> **展開中の三角は「中身が現れる軸」の向きを指す。折り畳み中はその軸から 90 度回した向きを指す。**

| 開示軸 | 展開中 | 折り畳み中 |
|---|---|---|
| `RevealAxis::Block` (中身が**下**に開く) | ▼ (軸方向) | ▶ (90 度回す) |
| `RevealAxis::Inline` (中身が**右**に開く) | ▶ (軸方向) | ▼ (90 度回す) |

Block の 2 状態は Apple HIG / WinUI TreeView / CSS の慣習と完全一致する。
Inline はその軸を横に倒したもので、結果として Block の裏返しになる。
CSS Counter Styles Level 3 §6.3 が directional marker について
"If the image is directional, it must respond to the writing mode of the element"
と定めているのと同じ考え方 — 向きは絶対方向ではなく**開示軸に相対**で決まる。

適用先:

| 呼び出し元 | 軸 | 理由 |
|---|---|---|
| `view/mixer_strips.rs` group strip | `Inline` | strip は横並び、子は右に現れる |
| `widgets/arrangement/` track header | `Block` | track は縦並び、子は下に現れる |
| `view/track_inspector/modulation_rack.rs` | `Block` | rack 行は縦積み、中身は下に開く (`if expanded { y += … }`) |


## 4. 変更内容 (ファイル単位)

触るファイルは 11 個 (新規 1。うち 1 個は `docs/`)。

| # | ファイル | 内容 |
|---|---|---|
| 4.1 | `daw_gui/src/view/disclosure.rs` (**新規**) | グリフ規則の SSoT + 回帰テスト |
| 4.2 | `daw_gui/src/view/mod.rs` | `pub mod disclosure;` |
| 4.3 | `daw_gui/src/event.rs` | `AppEvent::ToggleGroupCollapsed` 追加 + stale doc (:156) |
| 4.4 | `daw_gui/src/app.rs` | 上の handler (+ `handle_event` 前段 3 段の影響確認) |
| 4.5 | `daw_gui/src/view/mixer_strips.rs` | Inline 軸へ + toggle を event 経由へ + pad / disclosure 幅の手写し 5 か所を導出へ |
| 4.6 | `daw_gui/src/widgets/arrangement/run.rs` (#77 後は `header.rs`) | Block 軸へ + toggle を event 経由へ + stale doc (:2142) + net-zero コメント (:2401) |
| 4.7 | `daw_gui/src/view/track_inspector/modulation_rack.rs` | Block 軸へ (3 つ目の複製) + `18.0` 手写し 3 か所を導出へ |
| 4.8 | `daw_gui/src/state/ui_prefs.rs` | `collapsed_groups` の doc + stale doc (:16) |
| 4.9 | `daw_gui/src/widgets/arrangement/mod.rs` | stale doc × 3 (:344 / :523 / :1210) + `disclosure_color` doc (:1044) |
| 4.10 | `daw_gui/src/widgets/arrangement/geometry.rs` | doc から SSoT へ相互参照 |
| 4.11 | `docs/plan_mixer_group_collapse.md` | #74 が反転させた確定仕様行への supersede note |

`AppEvent` は **GUI ローカル**で、`#[derive(Debug, Clone, PartialEq)]` しか付いていない
(`daw_gui/src/event.rs:12`)。IPC を渡らないので bincode derive も
`common/build.rs` の `WIRE_SOURCES` も**無関係**。子 exe の作り直しも不要。
`ui_prefs.collapsed_groups` も GUI ローカル (`daw_gui/src/state/ui_prefs.rs:13`、
`UiPrefs` は `state/ui_prefs.rs:4` で derive を 1 つも持たない plain struct) で
Song にも protocol にも載らないため、3 プロセス貫通も RT 制約も無い
(`common/` と `daw_audio/` に `collapsed_groups` の参照は 0 件)。

---

### 4.1 新規: `daw_gui/src/view/disclosure.rs`

`crate::view::snap` / `crate::view::track_color` と同じ「view 配下の leaf helper を
widgets からも使う」既存 idiom に乗せる (`daw_gui/src/widgets/arrangement/view_build.rs:15-16`、
`daw_gui/src/widgets/piano_roll/run.rs:16-17` が先例)。
SPDX ヘッダは書かない (`REUSE.toml:25` の `path = "**"` blanket に集約、
memory `project_gplv3_publication`)。

ファイル全体をこの通りに作る (doc コメントの密度・日本語・全角前後の空白は
既存ファイルの流儀に合わせてある):

```rust
//! 開閉マーク (disclosure triangle) の glyph を決める唯一の場所。
//!
//! r.md #74。 以前は「bool から glyph を直接引く」 リテラルが 3 か所
//! (mixer / arrangement / modulation rack) に複製され、 うち 1 つは別 codepoint
//! (▸/▾) を使っていた。 開示方向が式に入っていないので、 横並びの mixer では
//! ▼ が「何も無い方向」 を指し、 片方だけ直せば意味が食い違うという構造だった。
//!
//! 規則は 1 つだけ:
//!
//! > **展開中の三角は「中身が現れる軸」 の向きを指す。 折り畳み中はその軸から
//! > 90 度回した向きを指す。**
//!
//! 縦に開くもの (arrangement の group track / inspector の modulation rack) は
//! [`RevealAxis::Block`] で 展開 = ▼ / 折り畳み = ▶ となり、 Apple HIG・
//! WinUI TreeView・CSS Counter Styles Level 3 §6.3 (`disclosure-open` = ▾ /
//! `disclosure-closed` = ▸) の慣習と一致する。 横に開く mixer (group strip の
//! 子は **右** に並ぶ) は [`RevealAxis::Inline`] で 展開 = ▶ / 折り畳み = ▼ と、
//! 縦の裏返しになる。
//!
//! **同じ group が arrangement と mixer で別のマークになるのは意図した結果**
//! (r.md #74 で確定)。 三角は「状態」 ではなく「中身がどちらへ開くか」 を伝える。
//! CSS が directional marker について "If the image is directional, it must
//! respond to the writing mode of the element" と定めているのと同じ考え方で、
//! 向きは絶対方向ではなく開示軸に相対で決まる。

/// 展開したときに中身が現れる方向の軸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealAxis {
    /// 縦 (block) 方向 — 展開すると中身が **下** に現れる。
    /// arrangement の group track、 inspector の modulation rack 行。
    Block,
    /// 横 (inline) 方向 — 展開すると中身が **右** に現れる。
    /// mixer の group strip (子 strip は group strip の右に並ぶ)。
    Inline,
}

/// 開閉マークの glyph。 `collapsed` は「中身が見えていない」 状態。
///
/// **全ての開閉マークはこの関数を通す。** 呼び出し側で `if collapsed { … }` と
/// リテラルを書かないこと (それが r.md #74 の root cause)。
#[must_use]
pub fn disclosure_glyph(collapsed: bool, axis: RevealAxis) -> &'static str {
    match (axis, collapsed) {
        // 展開中は軸の向きを指す。
        (RevealAxis::Block, false) => "\u{25bc}",  // ▼ 中身は下
        (RevealAxis::Inline, false) => "\u{25b6}", // ▶ 中身は右
        // 折り畳み中は軸から 90 度回す。
        (RevealAxis::Block, true) => "\u{25b6}",  // ▶
        (RevealAxis::Inline, true) => "\u{25bc}", // ▼
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #74: 向きは開示軸に相対。 リテラル複製時代の
    /// 「片方のビューだけ直して逆転が残る」 を機械で止める。
    #[test]
    fn disclosure_glyph_points_along_reveal_axis() {
        // 展開中 = 軸の向き。
        assert_eq!(disclosure_glyph(false, RevealAxis::Block), "\u{25bc}");
        assert_eq!(disclosure_glyph(false, RevealAxis::Inline), "\u{25b6}");
        // 折り畳み中 = 軸から 90 度。
        assert_eq!(disclosure_glyph(true, RevealAxis::Block), "\u{25b6}");
        assert_eq!(disclosure_glyph(true, RevealAxis::Inline), "\u{25bc}");
        // 軸が式に入っている証拠: 同じ状態でも軸が違えば必ず別のマークになる。
        for collapsed in [false, true] {
            assert_ne!(
                disclosure_glyph(collapsed, RevealAxis::Block),
                disclosure_glyph(collapsed, RevealAxis::Inline),
                "collapsed={collapsed} で Block と Inline が同じ glyph になっている"
            );
        }
    }
}
```

**テストはこの 1 本だけ**にする。描画側の rect / font size をテストで固定しない
(memory `feedback_no_tests_for_simple_cases`: 本番の算術をテストに写すだけのテストは書かない)。

---

### 4.2 `daw_gui/src/view/mod.rs`

`pub mod dirty_guard_modal;` (現 7 行目) の直後に 1 行足す (alphabetical 順もこれで合う)。

```rust
pub mod dirty_guard_modal;
/// r.md #74: 開閉マーク (disclosure triangle) の glyph 規則の SSoT。
pub mod disclosure;
```

---

### 4.3 `daw_gui/src/event.rs`

**(a) 新 variant。** 現 `GroupSelectedTracks { … }` (153-155 行目) と
`ToggleTrackAutomationCollapsed` (156-162 行目) の**間**に挿入する。

```rust
    /// r.md #74 / gui_01 #016: group track の折り畳み disclosure click。
    /// `ui_prefs.collapsed_groups` の `track_id` を反転し、 arrangement /
    /// mixer 両方の可視 track 集合が次フレームで追従する (`collapsed_groups`
    /// が 2 ビュー共通の SSoT)。 session-only な UI 状態なので Undo / save
    /// 対象外。
    ///
    /// **両ビューはこの event 経由でのみ toggle する。** 以前は
    /// `mixer_strips.rs` と arrangement widget が同じ HashSet flip を各々
    /// インラインで持ち、 コメントだけが存在しない `ToggleGroupCollapsed` を
    /// 指す幽霊になっていた (r.md #74)。
    ToggleGroupCollapsed {
        track_id: u32,
    },
```

struct variant にするのは隣の `ToggleTrackAutomationCollapsed { track_id }` と同 idiom
だから (positional tuple を避けるアーキテクチャ不変条件 1 とも整合)。

**`undo_label` (`event.rs:1650`) に arm を足さないこと。** この event は snapshot を
積まない非編集 event で、`event.rs:1643-1646` の doc が「非編集 event は catch-all
`_ => "編集"` (:1846) で足りる」と明記している。ラベルは記録されない。

**(b) stale doc。** 156 行目:

```rust
    /// gui_01 #028 (M14 Phase 63n-1): track 行の disclosure ▶/▼ click。
```

この doc は `ToggleTrackAutomationCollapsed` = automation lane の開閉に付いている。
lane の disclosure は実装では `+` / `-` (`run.rs:2183` master / `run.rs:2329` track)。次に直す:

```rust
    /// gui_01 #028 (M14 Phase 63n-1): track 行の automation lane disclosure
    /// (`+` / `-`) click。
```

---

### 4.4 `daw_gui/src/app.rs`

`AppData::handle_event` (518 行目〜) の match (577 行目〜) は網羅 (`_ =>` arm 無し) なので
arm 追加が必須。`AppEvent::ToggleTrackAutomationCollapsed` の arm (現 814-823 行目、
`AppEvent::ToggleTrackAutomationCollapsed { track_id } => {` が 814 行目) の
**直前**に置く:

```rust
            AppEvent::ToggleGroupCollapsed { track_id } => {
                // r.md #74: arrangement / mixer 両方の group disclosure が
                // ここに合流する (`collapsed_groups` が 2 ビュー共通の SSoT)。
                if !self.ui_prefs.collapsed_groups.insert(track_id) {
                    self.ui_prefs.collapsed_groups.remove(&track_id);
                }
            }
```

`insert` の戻り値で分岐する形は隣の `expanded_automation_tracks` の arm
(現 820 行目) と同じ idiom。dirty フラグは立てない (memory `project_dirty_flag_rule`:
`collapsed_groups` は「見方の都合」側)。

#### 4.4.1 `handle_event` 経由にすると何が挟まるか (確認済み・全て無害)

インライン flip を event に寄せるのは**純粋な refactor ではない**。`handle_event` は
match に入る前に 3 段を通る。3 段とも現物を追って影響が無いことを確認した:

| 前段 | 場所 | この event への影響 |
|---|---|---|
| shutdown 中の全 event drop | `app.rs:533` | 終了シーケンス中は折り畳み toggle が捨てられる。終了中に fold を変える意味は無いので正。 |
| `song_doc.begin_event(event.undo_label())` | `app.rs:541` → `state/song_doc.rs:420` | `event_scope` に gesture id を入れ `pending_label` を差すだけ。**snapshot も dirty も epoch も動かない** (`begin_event` の本体 420-428 行を確認)。`undo_label` は catch-all `"編集"` (`event.rs:1846`) に落ちるが、snapshot を積まないので履歴には現れない。 |
| export 中の block-list | `app.rs:564-576` | block されるのは `PluginEvent::SlotPluginLoaded` / `PluginEvent::AllPluginStates` / `AudioEvent::BounceClipFxComplete` の 3 つだけ (positive-default)。新 variant は素通りする。 |

つまり **undo 履歴・dirty・書き出しのいずれも汚さない**。この事実は
`run.rs:2401` の「double-click が disclosure を踏んでも net-zero」コメントの前提でもあるので、
§4.6(e) でそのコメントの文言を新しい経路に合わせる。

---

### 4.5 `daw_gui/src/view/mixer_strips.rs`

**(a) 定数 (現 30-39 行目)。** 現状:

```rust
const TOP_LABEL_H: f32 = 18.0;
/// r.md #13: strip 上端の「トラック名バンド」の高さ (px)。 この帯を押すと
/// トラックを選択する (M/S トグルや fader/knob より上なので操作と干渉しない)。
/// top pad(6) + 名前(TOP_LABEL_H=18) = 次の M/S トグル行の直前まで。
const NAME_BAND_H: f32 = 24.0;
/// group strip の名前バンド左端にある折り畳み disclosure (▶/▼) が占める幅
/// (= draw_strip の `pad(6) + disc_w(14) + gap(2)`)。 選択の press 帯はこの分
/// だけ右にずらして、 disclosure クリック (= 折り畳みトグル) が選択を巻き込まない
/// ようにする (code review: group strip で disclosure が NAME_BAND 内に重なる)。
const DISCLOSURE_ZONE_W: f32 = 22.0;
```

`22.0` は `draw_strip` の実 geometry (487 行 `let pad = 6.0;` / 493 行 `let disc_w = 14.0;` /
510 行 `+ 2.0`) の **手写しミラー**で、doc 自身がそう認めている。`NAME_BAND_H = 24.0`
(= `6 + 18`) も同じ手写し。片方だけ変えると「名前帯を押すと選択が飛ぶ / disclosure が
押せない」位置依存デッドゾーンになる (memory `feedback_positional_input_deadzone`)。
**導出に変える**:

```rust
const TOP_LABEL_H: f32 = 18.0;
/// strip の内側余白 (px)。 `draw_strip` が名前 / M/S 行 / pan 行 / fader の
/// 左右と上端に共通で使う。 **strip で「余白」 と言えばこの値**で、 下の
/// NAME_BAND_H / DISCLOSURE_ZONE_W / STRIP_FADER_* もここから導く
/// (r.md #74: 以前は同じ `6.0` が 5 か所に手写しされていた)。
const STRIP_PAD: f32 = 6.0;
/// r.md #13: strip 上端の「トラック名バンド」の高さ (px)。 この帯を押すと
/// トラックを選択する (M/S トグルや fader/knob より上なので操作と干渉しない)。
/// 上 pad + 名前 = 次の M/S トグル行の直前まで。
const NAME_BAND_H: f32 = STRIP_PAD + TOP_LABEL_H;
/// group strip の名前バンド左端に置く折り畳み disclosure ボタンの幅 (px)。
const DISCLOSURE_W: f32 = 14.0;
/// disclosure ボタンとトラック名の間隔 (px)。
const DISCLOSURE_GAP: f32 = 2.0;
/// group strip の名前バンド左端にある折り畳み disclosure (r.md #74: 展開中 ▶ /
/// 折り畳み中 ▼) が占める幅。 選択の press 帯はこの分だけ右にずらして、
/// disclosure クリック (= 折り畳みトグル) が選択を巻き込まないようにする。
/// **`draw_strip` の実 geometry から導出する** — 旧実装は `22.0` を手写ししていて、
/// 幅を変えると黙ってズレる位置依存デッドゾーンになりえた (r.md #74)。
const DISCLOSURE_ZONE_W: f32 = STRIP_PAD + DISCLOSURE_W + DISCLOSURE_GAP;
```

**値は 1 つも変わらない** (`NAME_BAND_H` = 24.0、`DISCLOSURE_ZONE_W` = 22.0)。
230 行目の `x + DISCLOSURE_ZONE_W` と 231 / 278 行目の `NAME_BAND_H` はそのまま。

**(b) import。** 先頭の `use crate::view::track_color;` (21 行目) の隣に足す:

```rust
use crate::view::disclosure::{RevealAxis, disclosure_glyph};
```

`AppEvent` は 25 行目で既に import 済み (`use crate::app::{AppData, AppEvent, ModControlDomain};`)。

**(c) 本体 (現 487-513 行目。`let pad = 6.0;` が 487、`};` が 513)。** 現状:

```rust
    let pad = 6.0;
    let mut y = rect.y + pad;

    // 名前 (group strip は左に折り畳み disclosure ▶/▼ を置く)
    let name_x = if let Some(collapsed) = group_collapsed {
        let tri = if collapsed { "\u{25b6}" } else { "\u{25bc}" }; // ▶ 折り畳み / ▼ 展開
        let disc_w = 14.0;
        ui.button_at(
            ("mixer_strip_disclosure", layout_idx),
            tri,
            Rect { x: rect.x + pad, y, w: disc_w, h: 14.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    // arrangement の ToggleGroupCollapsed と同じ toggle
                    // (collapsed_groups が両 view 共通の SSoT)。
                    if app.ui_prefs.collapsed_groups.contains(&track_idx) {
                        app.ui_prefs.collapsed_groups.remove(&track_idx);
                    } else {
                        app.ui_prefs.collapsed_groups.insert(track_idx);
                    }
                })
            },
        );
        rect.x + pad + disc_w + 2.0
    } else {
        rect.x + pad
    };
```

次に置き換える:

```rust
    let pad = STRIP_PAD;
    let mut y = rect.y + pad;

    // 名前 (group strip は左に折り畳み disclosure を置く)。 mixer は strip が
    // **横** に並び、 group の子は右に現れるので開示軸は Inline
    // (r.md #74: 展開中 ▶ = 子が右に並んでいる / 折り畳み中 ▼)。
    let name_x = if let Some(collapsed) = group_collapsed {
        ui.button_at(
            ("mixer_strip_disclosure", layout_idx),
            disclosure_glyph(collapsed, RevealAxis::Inline),
            Rect { x: rect.x + pad, y, w: DISCLOSURE_W, h: 14.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleGroupCollapsed { track_id: track_idx })
                })
            },
        );
        rect.x + pad + DISCLOSURE_W + DISCLOSURE_GAP
    } else {
        rect.x + pad
    };
```

`track_idx` は名前に反して **stable な track id**。`draw_strip` の引数宣言は
`track_idx: u32` (437 行目) で、呼び出し側は `draw_track_strip` (349 行目) /
`draw_return_strip` (385 行目) が `entry.track_id` (332 行目で束縛) を渡している
(363 / 399 行目)。positional index ではないので不変条件 1 に抵触しない
(第 3 引数 `layout_idx: usize` の方が widget id 用の並び順)。

**(d) コメント。** 同ファイルの次の 2 か所が「▶ 折り畳み / ▼ 展開」前提。新しい向きに直す:

- 178 行目 `// (自分の祖先に collapsed が無い限り) 残り、 disclosure ▶/▼ を出す。`
  → `// (自分の祖先に collapsed が無い限り) 残り、 disclosure (r.md #74: 展開中 ▶ / 折り畳み中 ▼) を出す。`
- 439-441 行目 `draw_strip` の `group_collapsed` 引数 doc
  (`// disclosure ▶/▼ を描き、 click で \`collapsed_groups\` を toggle する` = 440 行目)
  → `▶/▼` を `(r.md #74: 展開中 ▶ / 折り畳み中 ▼、 開示軸 Inline)` に書き換える。
- 490 行目のコメントは (c) で置換済み。35 行目の doc は (a) で置換済み。

**(e) 昇格した `STRIP_PAD` のコピーを全部移行する (§1.3 の規則)。**
production 2 か所 + test 2 か所。**片方だけ直すと SSoT が半分になって最悪**なので 4 つ同時に:

- 782 行目 `const STRIP_FADER_TOP_OFFSET: f32 = 6.0` の**第 1 項**
  → `const STRIP_FADER_TOP_OFFSET: f32 = STRIP_PAD` (これは `draw_strip` の上 pad)
- 790 行目 `const STRIP_FADER_BOTTOM_PAD: f32 = 6.0 + 12.0;`
  (直上 789 行目の doc が「`draw_strip` の `pad + 12.0`」と明言している)
  → `const STRIP_FADER_BOTTOM_PAD: f32 = STRIP_PAD + 12.0;`
- 1116 行目 `let inner_w = STRIP_WIDTH - 6.0 * 2.0; // draw_strip の pad = 6.0`
  → `let inner_w = STRIP_WIDTH - STRIP_PAD * 2.0;` (末尾コメントは不要になるので削る)
- 1128 行目 `let stack = 6.0 // 上 pad`
  → `let stack = STRIP_PAD // 上 pad`

**触らない `6.0`** (値が同じだけの**別の量**。畳むと独立に動かせない偽 SSoT になる):

- 75 行目 `const SEND_PAD: f32 = 6.0;` — Sends セクション内の余白。既に自前の名前を持つ。
- 562 行目 `y += TOGGLE_H + 6.0;` / 785 行目 `+ 6.0` / 1131 行目 `+ 6.0 // M/S 行の下マージン`
  — M/S 行の**下マージン**。3 か所に手写しされている点は `STRIP_PAD` と同じ形だが、
  #74 が昇格させる量ではないので触らない (§5 に別 item として記載)。

---

### 4.6 `daw_gui/src/widgets/arrangement/run.rs` (#77 後は `header.rs` 等)

**行番号は #77 前のもの。§2 の grep で現在位置を特定してから編集すること。**

**(a) import。** このファイルは `use super::*;` (4 行目) しか持たないので、明示 import を
1 行足す (`view_build.rs:15-16` が `use crate::view::snap; use crate::view::track_color;` と
同じことをしている)。

```rust
use crate::view::disclosure::{RevealAxis, disclosure_glyph};
```

**(b) グリフ (現 2297-2302 行目)。** 現状:

```rust
                // M14 Phase 63c (#016): disclosure ▼/▶ — group track のみ描画 + click で
                // ToggleGroupCollapsed Edit 発行 (loop 後に発火、 トラック選択より priority 高)。
                let is_group = is_group_set.contains(&t.id);
                let disclosure_rect = disclosure_rect_for(name_rect, style, t.depth);
                if is_group {
                    let label = if t.collapsed { "▶" } else { "▼" };
```

次に置き換える (**見た目は不変**。SSoT 経由にするだけ):

```rust
                // M14 Phase 63c (#016): group disclosure — group track のみ描画 + click で
                // `AppEvent::ToggleGroupCollapsed` を発行 (loop 後に発火、 トラック選択より
                // priority 高)。 arrangement は track が **縦** に並び group の子は下に
                // 現れるので開示軸は Block (r.md #74: 折り畳み中 ▶ / 展開中 ▼)。
                let is_group = is_group_set.contains(&t.id);
                let disclosure_rect = disclosure_rect_for(name_rect, style, t.depth);
                if is_group {
                    let label = disclosure_glyph(t.collapsed, RevealAxis::Block);
```

以降の `ui.push_text(GlyphArea { text: label.into(), … })` (2303-2312 行目) はそのまま
(`&'static str` → `Arc<str>` の `.into()` は従来と同じ)。font size は
`style.track_text_size` (2307 行目) のまま。

**(c) toggle (現 2464-2470 行目)。** 現状:

```rust
        // M14 Phase 63c (#016): disclosure click → ToggleGroupCollapsed (priority 高、 トラック選択は
        // この frame では skip = group の collapsed toggle 動作のみで selection は変えない、
        // Reaper / Live と同じ UX)。
        if let Some(tid) = disclosure_clicked {
            ui.push_edit({ let v_id = tid; Edit::mutate(move |app: &mut AppData| { if app.ui_prefs.collapsed_groups.contains(&v_id) { app.ui_prefs.collapsed_groups.remove(&v_id); } else { app.ui_prefs.collapsed_groups.insert(v_id); } }) });
            clicked_track_for_select = None;
        }
```

次に置き換える (幽霊コメントが実在する event を指すようになる):

```rust
        // M14 Phase 63c (#016): disclosure click → `AppEvent::ToggleGroupCollapsed`
        // (priority 高、 トラック選択はこの frame では skip = group の collapsed toggle
        // 動作のみで selection は変えない、 Reaper / Live と同じ UX)。 mixer の
        // disclosure も同じ event に合流する (r.md #74)。
        if let Some(tid) = disclosure_clicked {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleGroupCollapsed { track_id: tid });
            }));
            clicked_track_for_select = None;
        }
```

`AppEvent` は同ファイルで既に使われている (現 989 行目の
`AppEvent::ToggleTrackAutomationCollapsed`) ので追加 import は不要。

**(d) stale doc (現 2142 行目)。**

```rust
                // "Master" label + lane disclosure (`▶`/`▼`) のみを描画する (daw_01 #034 §B 仕様)。
```

master row の lane disclosure の実装は `+` / `-` (現 2183 行目 `let label = …`)。次に直す:

```rust
                // "Master" label + lane disclosure (`+`/`-`) のみを描画する (daw_01 #034 §B 仕様)。
```

**(e) net-zero コメント (現 2401-2402 行目)。** 「直接 flip する」が (c) の変更で嘘になる。
事実 (非 undoable / net-zero) は §4.4.1 で確認済みなので、経路の記述だけ直す。現状:

```rust
                // 2 release で折り畳みが 2 回 toggle するが、 daw_01 の `collapsed_groups` (HashSet) を直接 flip する
                // 非 undoable な view-state edit なので net-zero (= fold 状態保存、 undo 履歴も汚さない)。 M·S·R /
```
→
```rust
                // 2 release で折り畳みが 2 回 toggle するが、 `AppEvent::ToggleGroupCollapsed` は daw_01 の
                // `collapsed_groups` (HashSet) を反転するだけの非 undoable な view-state edit なので net-zero
                // (= fold 状態保存、 undo 履歴も汚さない、 r.md #74)。 M·S·R /
```

(2403 行目以降の `// lane disclosure は name 帯の **右**で…` に続く 1 文なので、
末尾の `M·S·R /` を落とさないこと。)

**(f) 触らないもの。** 現 2105 / 2215 / 2322 / 2394 行目のコメントの `▶▼` は
arrangement の group disclosure を指しており向きも変わらないので**そのまま**
(2320-2326 行目は lane disclosure を `+`/`-` にした経緯のコメントブロックで、
「旧 `▽`/`▷` は font 不在で不可視 click target」が 2321 行目、
「旧 `▼`/`▶` は group disclosure と同 glyph で混同」が 2322 行目)。
automation lane の `+` / `-` 本体 (現 2183 / 2329 行目) も**そのまま**。
`daw_gui/src/widgets/arrangement/draw.rs:57` と
`daw_gui/src/widgets/arrangement/tests.rs:1650` の `▶▼` も group disclosure の話で正しい。

---

### 4.7 `daw_gui/src/view/track_inspector/modulation_rack.rs`

**3 つ目の複製**。別 codepoint (▸ U+25B8 / ▾ U+25BE) で同じ規則を再実装しており、
しかも bool の向きが逆 (`expanded`) で書かれている。

**寸法は 1px も変えない。** 旧版の本節は「▶ は ▸ より広いので button を 16px → 20px に
広げ、名前起点を 4px ずらす」としていたが、これは**実測で否定された** (§10 の指摘 3/4/6)。
UI の既定フォントは `HackGen Console NF`
(`ui/crates/renderer/src/pipelines/glyph.rs:25`)、その実 advance は
**▶ / ▼ / ▸ / ▾ / … の 5 字すべて 540 / 1024 em = 0.527 em (16px フォントで 8.44px)**
(インストール済み `HackGenConsoleNF-Regular.ttf` の `head.unitsPerEm` と `hmtx` を
直接読んで確認)。つまり **glyph 族を変えても advance は 1 単位も変わらず**、
16px 幅のボタンに 8.44px の字を置くので `button_at` の ellipsis
(`ui/crates/ui/src/widgets/button.rs:199` の `fit_text_ellipsized`) には掛からない。
幅を変えないので、下の 3 か所の `18.0` を導出にする以外に geometry の変更は無い。

**(a) import。** 13 行目 `use crate::app::{AppData, AppEvent};` の隣に足す:

```rust
use crate::view::disclosure::{RevealAxis, disclosure_glyph};
```

**(b) 寸法定数 (新規、現 93-94 行目 `MOD_CANVAS_H` の直後に置く)。**
`18.0` は「disclosure ボタン幅 (16) + 間隔 (2)」の手写しで、**3 か所**にある
(390 行目 header の名前起点 / 770 行目 routing 行のラベル起点 / 773 行目 同じ行の幅の項)。
§1.3 の規則どおり、#74 が触る量なので全部導出にする:

```rust
/// modulation rack の header 行左端に置く開閉 disclosure ボタンの幅 (px)。
const MOD_DISCLOSURE_W: f32 = 16.0;
/// disclosure ボタンと、 その右に来る名前 / track dropdown / routing ラベルの間隔 (px)。
const MOD_DISCLOSURE_GAP: f32 = 2.0;
/// 行左端 (`lx`) から名前 / routing ラベル左端までの距離 (px)。
/// **disclosure ボタンの実寸から導出する** — 旧実装は `18.0` を header の名前起点・
/// routing 行の x・同じ行の幅の項の 3 か所に手写ししていて、 ボタン幅を変えると
/// routing 行だけ黙ってずれた (r.md #74)。
const MOD_NAME_INSET: f32 = MOD_DISCLOSURE_W + MOD_DISCLOSURE_GAP;
```

**(c) 本体 (現 376-390 行目)。** 現状:

```rust
        // 展開トグル (chevron)。
        ui.button_at(
            ("inspector_mod_src_expand", i),
            if expanded { "\u{25be}" } else { "\u{25b8}" },
            Rect { x: lx, y, w: 16.0, h: 20.0 },
            …
        );
        let name_x = lx + 18.0;
```

次に置き換える (rect の実寸は 16×20 のまま):

```rust
        // 展開トグル。 rack 行は縦積みで中身は下に開く → 開示軸は Block
        // (r.md #74 で全 disclosure の glyph を `view::disclosure` へ一本化した。
        // 旧実装はここだけ小三角 ▸/▾ を使っていて、 同じ意味のマークが 2 系統
        // 存在していた)。 ▸/▾ と ▶/▼ は既定フォントで advance が同一 (0.527 em)
        // なので、 族を変えても行のレイアウトは 1px も動かない。
        ui.button_at(
            ("inspector_mod_src_expand", i),
            disclosure_glyph(!expanded, RevealAxis::Block),
            Rect { x: lx, y, w: MOD_DISCLOSURE_W, h: 20.0 },
            …
        );
        let name_x = lx + MOD_NAME_INSET;
```

click ハンドラ (381-388 行目、`expanded_mod_sources` の insert/remove) は**そのまま**。
これは modulation rack のローカル UI 状態で、`collapsed_groups` とは別物
(§4.3 の event に寄せる対象ではない)。

**(d) routing 行 (現 769-775 行目の `Rect { … }`。`x` が 770、`w` が 773)。**
(b) で昇格させた量のコピーなので同時に移行する:

```rust
                    x: lx + 18.0,
                    …
                    w: (row_w - 18.0 - 4.0 - 46.0 - 4.0 - 22.0 - 4.0 - 20.0).max(1.0),
```
→
```rust
                    x: lx + MOD_NAME_INSET,
                    …
                    w: (row_w - MOD_NAME_INSET - 4.0 - 46.0 - 4.0 - 22.0 - 4.0 - 20.0).max(1.0),
```

(773 行目の残りの項は右側の depth / 極性 / × の幅で、disclosure とは無関係なのでそのまま。)

**(e) コメント (現 355 行目)。**

```rust
        // --- header row: [▸/▾][name/track] [meter] [arm] [×] ---
```
→
```rust
        // --- header row: [▶/▼][name/track] [meter] [arm] [×] ---
```

**(f) 触らないもの。** 320 / 325 行目の `[+ ▾]` と `\u{25be}` は「dropdown が自前で描く
シェブロンと二重になるので入れない」という**説明文**であって glyph リテラルではない。
429 行目の `▼ シェブロン` も dropdown の話。どちらもそのまま。

---

### 4.8 `daw_gui/src/state/ui_prefs.rs`

**(a) `collapsed_groups` の doc (現 11-13 行目)** に唯一の反転経路と session-only の
根拠を書き足す。**行番号は書かない** (doc に file:line を埋めると次の編集で腐る)。
反転が 1 経路であることと、生存 track に無い id が prune されることを述べる:

```rust
    /// 折り畳み中の group track id 集合。 group 自身が `kind == Group`
    /// (= 子を持つ) かつこの set に含まれていれば子孫の row を hide。
    /// **arrangement と mixer が共有する SSoT** で、 反転は
    /// `AppEvent::ToggleGroupCollapsed` の 1 経路のみ (r.md #74)。
    /// session-only: プロジェクト load / New で clear、 track 削除 / ungroup /
    /// undo-redo 後の照合で生存 id へ prune。 save / Undo 対象外。
    pub collapsed_groups: std::collections::HashSet<u32>,
```

実装者向けの参照地図 (2026-08-28 実測)。**doc には書かず、この計画書に置く**。

**これは「実装後に残るべき *コード* 参照」の一覧**であって、`grep -rn "collapsed_groups"
daw_gui/src` の生の出力ではない。生 grep は現状 24 行で、次の 3 種を追加で含む:

- **コメント行** 9 行 (`project.rs:324` / `selection_view.rs:1212` / `:1215` /
  `tracks.rs:635` / `mixer_strips.rs:176` / `:334` / `:440` / `:501` /
  `run.rs:2401`)。数は実装後に変わる (§4.5(d) / §4.6(e) で文言を直し、§4.3 / §4.4 の
  新 doc・新コメントが増える)。
- **field 宣言** 1 行 (`state/ui_prefs.rs:13`)。
- **実装で消える反転 2 か所** (`mixer_strips.rs:502-505` と `run.rs:2468`)。これが
  §4.4 の handler 1 か所に置き換わる。

コード行だけを見るには doc/コメント行を落とす (§2.1(a) と同じ形):

```
grep -rn "collapsed_groups" daw_gui/src | grep -v -E "^[^:]+:[0-9]+: *(//|///)"
```

実装**前**の期待値は次の表 + `ui_prefs.rs:13` (宣言) + `mixer_strips.rs:502/503/505` +
`run.rs:2468`、実装**後**は次の表 + `ui_prefs.rs:13` + §4.4 の handler (insert / remove の 2 行):

| 場所 | 関数 | 種別 |
|---|---|---|
| `app.rs:291` | `AppData` 初期化 | 生成 (空 set) |
| `handler/project.rs:93` | `reset_song_scoped_state` | clear |
| `handler/project.rs:325` | `after_undo_redo` | prune (`retain`。:324 は説明コメント) |
| `handler/project.rs:362` | `action_new` | clear |
| `handler/grouping.rs:223` | `action_ungroup_tracks_inner` | 該当 group を remove |
| `handler/grouping.rs:343` | `action_remove_last_track` | prune (`retain`) |
| `handler/tracks.rs:636` | `delete_track_inner` | prune (`retain`) |
| `handler/selection_view.rs:1224` | `is_hidden_under_collapsed_group` | 読み取り |
| `view/mixer_strips.rs:336` | `draw_track_strip` | 読み取り |
| `widgets/arrangement/view_build.rs:244` | `build` | 読み取り (`collapsed` bool の生成点) |

実装後に上の grep を打ち、**insert / remove (= 反転) を行うコード行が §4.4 の handler
だけ**で、他は生成 / clear / prune / 読み取りだけであることを確認する
(`tracks.rs:636` の `retain` は 636-637 に折り返している)。

**(b) stale doc (現 14-19 行目、該当は 16 行目)。** automation lane の disclosure を
`▶/▼` と書いているが実装は `+` / `-`:

```rust
    /// `automation_lanes_collapsed = true` を widget へ渡す。 ▶/▼ click
    /// で `ToggleTrackAutomationCollapsed` イベント経由に insert/remove。
```
→
```rust
    /// `automation_lanes_collapsed = true` を widget へ渡す。 `+` / `-` click
    /// で `ToggleTrackAutomationCollapsed` イベント経由に insert/remove。
```

---

### 4.9 `daw_gui/src/widgets/arrangement/mod.rs`

automation lane を `▶/▼` と書いた stale doc が **3 件**、共有 field の doc が 1 件。

**(a) 現 344 行目** (`ArrangementTrack::automation_lanes_collapsed`):

```rust
    /// M14 Phase 63n-1 (#028): track の automation lane 群を折り畳むか (▶ = collapsed / ▼ = expanded)。
```
→
```rust
    /// M14 Phase 63n-1 (#028): track の automation lane 群を折り畳むか (`+` = collapsed / `-` = expanded)。
```

**(b) 現 523-524 行目** (`ArrangementMasterRow::automation_lanes_collapsed`):

```rust
    /// `▶` (collapsed = true) / `▼` (expanded = false) を toggle すると
```
→
```rust
    /// `+` (collapsed = true) / `-` (expanded = false) を toggle すると
```

**(c) 現 1210 行目** (`automation_disclosure_size`):

```rust
    /// disclosure ▶ / ▼ glyph の描画 font size。 default = `track_text_size`。
```

この field を読むのは `run.rs:2188-2190` (master row) と `run.rs:2333-2335` (track 行) の
**`+` / `-` lane disclosure だけ**。`grep -rn "automation_disclosure_size" daw_gui/src ui/crates`
は **8 行** (宣言 `mod.rs:1211` / default `mod.rs:1406` / 読み 6 行 = 上の 2 か所 × 3 行) を返し、
group disclosure は `run.rs:2307` で `style.track_text_size` を使いこの field を一切見ない。
つまり他の 2 件と完全に同 class の stale doc:

```rust
    /// automation lane disclosure (`+` / `-`) glyph の描画 font size。 default = `track_text_size`。
```

**(d) 現 1044-1046 行目** (`disclosure_color`)。この色は group disclosure (`run.rs:2309`) と
lane disclosure (`run.rs:2191` / `:2336`) の**両方**に使われているのに、doc は group の
▼/▶ しか書いていない。1044 行目を次に直す (1045-1046 行目はそのまま):

```rust
    /// M14 Phase 63c (#016): ▼ / ▶ disclosure アイコンの色 (group 行の左端)。
```
→
```rust
    /// M14 Phase 63c (#016): disclosure アイコンの色 — group 行左端の ▼ / ▶ と、
    /// lane 行の `+` / `-` の両方に使う。
```

**(e) 触らないもの。** 996-999 行目 (`track_text_size` の doc = group disclosure グリフの
font size) は group disclosure を指していて正しい。1046 行目 (group は indent + disclosure
▶▼ で識別) も正しい。

---

### 4.10 `daw_gui/src/widgets/arrangement/geometry.rs`

`disclosure_rect_for` の doc (現 1006 行目) から glyph 規則の SSoT へ相互参照を張る。
「rect はここ、glyph はあちら」を読者が 1 hop で辿れるようにする:

```rust
/// M14 Phase 63c (#016): disclosure ▼ / ▶ アイコンの hit / 描画 rect。
```
→
```rust
/// M14 Phase 63c (#016): group disclosure アイコンの hit / 描画 rect。
/// glyph の向きは `crate::view::disclosure::disclosure_glyph`
/// (arrangement は `RevealAxis::Block`) が決める (r.md #74)。
```

---

### 4.11 `docs/plan_mixer_group_collapse.md`

mixer の折り畳みを導入した計画 (FIXME #7)。ここが **#74 の起点**であり、#74 は
この文書の確定仕様を**反転させる**。src の stale doc を 6 件潰しながら、
反転させた当の文書を矛盾したまま残さない。

該当は 4 行 (2026-08-28 実測):

| 行 | 内容 | #74 後 |
|---|---|---|
| `:16` | 「disclosure 三角 ▶/▼ と…判定は gui_01 arrangement widget が所有」 | arrangement の記述なので**正しいまま** |
| `:29` | 確定仕様 表 #3「group strip の header に **クリック可能な ▶/▼**」 | **偽** (mixer は 展開 ▶ / 折り畳み ▼) |
| `:43` | 「glyph / 色は arrangement の disclosure と揃える (▶/▼、`disclosure_color`)」 | **偽** (glyph は揃えない。色と click 経路は揃える) |
| `:49` | 受け入れ基準「mixer の group strip の ▶/▼ を click →」 | 向きを問わない記述なので**そのまま**でも読めるが、`:29` / `:43` の note で足りる |

**やること: 冒頭の引用が閉じた直後 (現 4 行目「してほしい」。の次、`## 現状 (2026-06-08)`
= 現 6 行目の前) に supersede note を 1 ブロック足す。**
本文の各行は**書き換えない** — 当時何を決めたかの記録なので、上書きすると
「いつ何が変わったか」が消える。

```markdown
> **supersede (r.md #74 / [plan_rmd_74_disclosure_glyph.md](plan_rmd_74_disclosure_glyph.md))**:
> 本書の「glyph は arrangement と揃える (▶/▼)」 (確定仕様 表 #3 / 実装方針) は **#74 で反転した**。
> mixer は strip が横に並び group の子が **右** に現れるので、開示軸は Inline =
> **展開中 ▶ / 折り畳み中 ▼** で arrangement の裏返しになる。
> **色 (`disclosure_color`) と「arrangement と同じ toggle 経路を使う」方針は #74 でも有効**
> (#74 で `AppEvent::ToggleGroupCollapsed` が実在するようになり、本書が想定した
> 「既存 `ToggleGroupCollapsed` 相当を mixer からも発火」が初めて字義どおり成立する)。
```

**他の doc は触らない。** 判断規則は 1 つ:

> **#74 が反転させた決定を書いた文書にだけ supersede note を足す。
> #74 以前に別の変更が既に無効化した記述と、会話ログは触らない。**

- `docs/gui_01_conversation*.md` — 会話ログ。当時のやり取りの記録なので書き換えない。
- `docs/plan_automation.md:416` / `:1323` — automation lane を `▶/▼` と書いたもの。
  **#028 follow-up が `+`/`-` に変えた時点で既に古い**ので、無効化したのは #74 ではない。
- `docs/plan_arrange_track_name_size.md:17`、`docs/plan_group_highlight_remove.md:24` / `:32`
  — arrangement の group disclosure `▶/▼`。**#74 で変わらない**ので、そもそも stale ではない。

**src 側の同じ stale doc は直す** — src の doc は実装時に読まれて実装を誤らせるが、
過去の計画書は「その時点の決定」の記録だから (この非対称は意図的)。


## 5. 本計画の対象外 (意図的)

- **arrangement と mixer の見た目 (presentation) の統一。** mixer は
  `ui.button_at` の枠付きボタン (`ui/crates/ui/src/widgets/button.rs:179-186` が
  border 1px / radius 6 の矩形を描く)、arrangement は `ui.push_text` の素のグリフ +
  自前 hit-test (`geometry.rs:1006`)。#74 で確定した仕様は**グリフ規則の SSoT 化と
  mixer の向き**であり、枠の有無は別件。#74 のついでに片方へ寄せない。
- **arrangement の向きの反転。** §1.1 のとおり現状維持が確定仕様。
- **disclosure の配置変更** (Logic / REAPER はストリップ**下端**に置く)。#74 の
  スコープ外。位置を動かすなら `DISCLOSURE_ZONE_W` による選択帯の切り欠き (230 行目) も
  同時に設計し直す話になるので、別 item として扱う。
- **`collapsed_groups` の 2 ビュー hard-link を option 化する** (Logic の
  View > Follow Track Stacks 相当)。現行の SSoT 共有は仕様であり #74 は触らない。
- **mixer の「M/S 行の下マージン」 (`6.0` が `mixer_strips.rs:562` / `:785` / test `:1131` の
  3 か所に手写し) の SSoT 化。** #74 が昇格させる量ではない (§1.3)。`STRIP_PAD` に畳むのは
  偽 SSoT (別の量が連動する) なので、直すなら独自の定数を持つ別 item。
- **`docs/plan_mixer_group_collapse.md` 以外の doc の追従。** 会話ログ
  (`docs/gui_01_conversation*.md`) と、#74 以前に別の変更が無効化した記述
  (`docs/plan_automation.md:416` / `:1323` の automation lane `▶/▼` 等) は触らない。
  規則と理由は §4.11。


## 6. 検証

**`make test` は使わない** (daw_gui を起動して実機の再生を壊す)。

```
make check
```
```
make test-nolaunch
```
```
make clippy
```
```
make arch-lint
```

`view/disclosure.rs` の unit test は **`make test-nolaunch` の中で回る**。
`Makefile:148` が `cargo test -p daw_gui --features daw_gui/script --lib --bins $(DAW_GUI_SAFE_TESTS)`
を打ち、`--lib` に lib crate の全 unit test が入る (`daw_gui/src/lib.rs:69` が `pub mod view;`)。

**`cargo test -p daw_gui --lib disclosure` と打たないこと。** リポジトリのガード
`.claude/guards.jsonl:73` (`no-bulk-test-run`) が `cargo test … -p daw_gui` を
`--test <name>` 無しで **block する** (実測)。このガードは substring マッチなので
「起動しない `--lib` 実行」まで巻き込むが、**#74 のためにガード行を書き換えない** —
`make test-nolaunch` が同じ unit test を回すので、迂回する理由が無い
(`DAW01_ALLOW_LAUNCH=1` は「これから daw_gui を起動します」の宣言であって、
起動しない実行に付けるのは嘘になる)。単体テストだけを速く回したい場合も
`make test-nolaunch` を使う。

**プロトコル型は変わらないので `cargo build --workspace` による子 exe 再生成は不要**
(§4 冒頭の理由)。ただし実機確認の前には `make build` でバイナリを作ること
(memory `feedback_build_after_clippy`: clippy / check は exe を作らない)。

### 実機 sign-off (最後に 1 回だけ)

上の 4 コマンドが全て緑になってから、**起動してよいか一声かけたうえで**
(memory `feedback_ask_before_launching_app`) `make run` で 1 回だけ確認する。
途中段階で何度も依頼しない (memory `feedback_no_redundant_verification`)。

確認項目:

1. Mixer で group strip の三角をクリック → 子 strip が消え、マークが **▼** になる。
   もう一度クリック → 子 strip が右に現れ、マークが **▶** になる。
2. Mixer で畳んだ group が Arrangement でも畳まれており、Arrangement 側のマークは
   **▶** (畳み) / **▼** (展開) の**まま**である。逆も同様に連動する。
3. Mixer の group strip の**名前**をクリックしても折り畳みが動かず、トラック選択だけが
   起きる (= `DISCLOSURE_ZONE_W` の切り欠きが効いている)。三角の上のクリックでは
   選択が変わらない。
4. Track Inspector の Modulation ラックの行頭マークが **▶ / ▼** で表示され (▸/▾ でも
   `…` でもない)、クリックで開閉し、右隣の名前 / dropdown と重なっていない。
   ソース直下の `→ <routing>` 行の左端が、ソース名の左端と**縦に揃っている**
   (= §4.7(d) の移行が効いている)。
5. Arrangement の automation lane の開閉マークは `+` / `-` のままである。


## 7. 受け入れ基準

- `if …collapsed… { "▶" } else { "▼" }` 形のリテラルが `daw_gui/src` から
  **0 件**になっている。glyph を返すのは `view/disclosure.rs::disclosure_glyph` のみ。
  §2.1(a) の grep の結果が **`view/disclosure.rs` の行と `handler/view_model.rs:463`
  (routing ラベルの区切り ▸、disclosure ではない) だけ**である。
- `collapsed_groups` を直接 flip するコードが **0 件** (`AppEvent::ToggleGroupCollapsed` の
  handler を除く)。判定は §4.8 の grep (doc/コメント行を落とす形)。**生 grep と表を
  突き合わせない** — 生 grep はコメント行と field 宣言を含むので数が一致しない。
  残るコード行が §4.8 の表 + `ui_prefs.rs:13` (宣言) + §4.4 の handler の insert / remove
  2 行で、insert / remove がその 2 行だけであること。
- `ToggleGroupCollapsed` を指すコメントが**実在する variant** を指している
  (幽霊コメント 3 件 = 旧 `mixer_strips.rs:500` / `run.rs:2298` / `run.rs:2464` が解消)。
- automation lane を `▶/▼` と書いた doc が **0 件**。対象は **6 件**:
  `state/ui_prefs.rs:16` / `widgets/arrangement/mod.rs:344` / `同:523` / `同:1210` /
  `event.rs:156` / `widgets/arrangement/run.rs:2142`。
  (`mod.rs:1210` は旧版が「触らない」に分類していたが、`automation_disclosure_size` の
  参照は `run.rs:2188-2190` / `:2333-2335` の `+`/`-` 専用と確認したので同 class。)
- `DISCLOSURE_ZONE_W` / `NAME_BAND_H` / `STRIP_FADER_TOP_OFFSET` / `STRIP_FADER_BOTTOM_PAD`
  が `STRIP_PAD` から**導出**されており、`draw_strip` の pad を意味する `6.0` の手写しが
  production にも test にも無い (§4.5(e) の 4 か所)。**この grep も 0 件にはならない**
  (`6.0` は別の量にも使われている) ので、期待される残存を名指しで固定する:

  ```
  grep -nE "[^0-9]6[.]0" daw_gui/src/view/mixer_strips.rs | grep -v -E "^[0-9]+: *(//|///)"
  ```

  (`[^0-9]` が無いと `16.0` = `PAN_READOUT_H` / `ADD_SEND_H` / `:989` まで拾う。)
  残ってよいコード行は **5 つだけ** — `const STRIP_PAD: f32 = 6.0;` (定義本体) /
  `const SEND_PAD: f32 = 6.0;` (現 :75) / `y += TOGGLE_H + 6.0;` (現 :562) /
  `+ 6.0` (現 :785) / test の `+ 6.0 // M/S 行の下マージン` (現 :1131)。
  後ろ 3 つは M/S 行の**下マージン** (別の量。§1.3 / §5)。
  `let pad = 6.0;` (現 :487) / `STRIP_FADER_*` の第 1 項 (現 :782 / :790) /
  `STRIP_WIDTH - 6.0 * 2.0` (現 :1116) / `let stack = 6.0` (現 :1128) が残っていたら未完了。
- modulation rack の `18.0` (disclosure 幅 + gap) の手写しが **コードから 0 件** (§4.7(b)(d))。
  **doc から 0 件にはならない** — §4.7(b) が書かせる `MOD_NAME_INSET` の doc 自身に
  「旧実装は `18.0` を … 3 か所に手写ししていて」という説明が入るからで、これは
  期待される残存である。判定は doc/コメント行を落とした形で行う:

  ```
  grep -nE "[^0-9]18[.]0" daw_gui/src/view/track_inspector/modulation_rack.rs | grep -v -E "^[0-9]+: *(//|///)"
  ```

  これが**空**であること (実装前は現 :390 / :770 / :773 の 3 行が出る)。
  正規表現はバックスラッシュを使わず POSIX ブラケット式 `[.]` で書く
  (make 経由だと `\.` の backslash が落ちる。memory `reference_make_argv_backslash_loss`)。
- modulation rack の行の寸法が**変わっていない** (ボタン 16×20、名前起点 `lx + 18`)。
- §6 の 4 コマンドが緑、§6 の実機 5 項目が sign-off 済み。
- `docs/plan_mixer_group_collapse.md` の冒頭に supersede note が入っている (§4.11)。
  同書の本文行 (`:16` / `:29` / `:43` / `:49`) は**書き換わっていない**。


## 8. 実装者への注意 (踏み抜きやすい点)

- **stale doc は 3 件ではなく 6 件。** 確定方針が挙げたのは 3 件だが、同じ root cause の
  ものが `event.rs:156` / `run.rs:2142` / `arrangement/mod.rs:1210` にもある。class ごと潰す
  (memory `feedback_sibling_occurrence_check`)。
- **`handler/view_model.rs:463` の `\u{25b8}` は直さない。** disclosure ではなく
  modulation routing ラベルの区切りで、`daw_gui/tests/app_state/modulation_arm.rs:187` が
  assert している。§2.1 の grep で必ず出てくるので、期待される残存として扱う。
- **modulation rack の見た目は「グリフ族だけ」変わる** (▸/▾ → ▶/▼)。これは「同じ意味の
  マークが 2 系統ある」という欠陥の解消であって、事故ではない。`disclosure_glyph` に
  グリフ族を選ぶ第 3 引数を足して旧 codepoint を温存しない — それは確定した
  関数シグネチャからの逸脱であり、複製を型に持ち上げただけになる。
  **寸法は変えない** (§4.7 冒頭の実測)。もし sign-off で `…` に化けたら、それは
  既定フォントが `HackGen Console NF` から外れている場合で、直し方は rect を広げること
  ではなく `ui.button_at_sized` (`ui/crates/ui/src/widgets/button.rs:56`、doc は :51-55)
  で font size を行の他の要素 (meter / routing ラベルの 11px) に揃えること。rect を広げると
  §4.7(d) の 3 点が再びずれる。
- **`disclosure_glyph` を `!expanded` で呼ぶ箇所は modulation rack だけ**
  (あそこだけ変数が `expanded`)。mixer / arrangement は `collapsed` をそのまま渡す。
  ここを取り違えると 1 か所だけ逆転する。
- **`STRIP_PAD` は §4.5(e) の 4 か所を同時に移行する** (production :782 / :790 と
  test :1116 / :1128。これに (a) の `NAME_BAND_H` / `DISCLOSURE_ZONE_W` と (c) の
  `let pad` が加わって計 7 か所)。production だけ / test だけを直すと、
  「テストが検証している定数は旧リテラルのまま」という半端な SSoT になる。
- **arrangement の `disclosure_rect_for` は `style.indent_px.max(8.0)` 幅**で、
  glyph の font size は `style.track_text_size` (default 12.0、`mod.rs:996-999`)。
  mixer の `button_at` は 16px 固定。**font size も rect も揃えようとしない** — §5 の
  対象外。
- **`make test` を打たない。** `.claude/guards.jsonl` の
  `no-app-launching-test-target` / `no-bulk-test-run` が書く瞬間に block する。
- **`cargo test -p daw_gui …` も `--test <name>` を伴わない限り block される**
  (`guards.jsonl:73`)。`--lib` だけを回すつもりでも substring で当たるので、
  unit test は `make test-nolaunch` から回す (§6)。ガード行を書き換えて迂回しない。
- コマンドを `&&` / `;` で連結しない。作業ディレクトリへ `cd` を前置しない。
- god file budget に余裕あり (`run.rs` 2,699 / `app.rs` 2,424 / `event.rs` 1,912 /
  `mod.rs` 2,413 / `mixer_strips.rs` 1,140 / `modulation_rack.rs` 853 行)。
  本計画の増分は各ファイル十数行なので 3,000 行制限には掛からない。
  > (当時の指標 = 物理行 3,000。r.md #76 で実コード行 1,000 + 関数 300 行 + インデント 6 段へ
  > 置換済み。現在値は `python scripts/loc_budget.py --report`。この計画書の他の箇所
  > (§10 の「god file budget に余裕」) も同じく当時の判断。)
  > **新指標では「余裕あり」は成り立たない。** `run.rs` (実コード 1,946) / `app.rs` (1,993) /
  > `arrangement/mod.rs` (1,249) は `scripts/arch_lint_baseline.txt` に登録済みで、
  > **天井は実測値なので 1 行も太れない**。本計画は 3 か所のグリフ複製を 1 関数へ畳む
  > 変更なので **減る方向**に動くが、着地後に `make arch-lint` の「解消」通知を読んで
  > baseline の天井を実測値へ更新すること。
  > (`mixer_strips.rs` 1,140 行 / `modulation_rack.rs` 853 行は実コードでは budget 内。)


## 9. 参照

実装コード (2026-08-28 / `cc608d0` 実測):

- `daw_gui/src/view/mixer_strips.rs:492` — 旧グリフ分岐 (mixer)
- `daw_gui/src/view/mixer_strips.rs:500-507` — 旧 toggle インライン複製 (幽霊コメント含む)
- `daw_gui/src/view/mixer_strips.rs:30-39` / `:230-231` / `:278` — pad / disclosure 幅の定数と使用点
- `daw_gui/src/view/mixer_strips.rs:782` / `:790` / `:1116` / `:1128` — pad `6.0` の手写し 4 か所
- `daw_gui/src/view/mixer_strips.rs:691-694` — `STRIP_FADER_TOP_OFFSET` と実 y 積み上げの `debug_assert`
- `daw_gui/src/view/mixer_strips.rs:175-182` / `:215-217` — 折り畳み配下の除外と左→右レイアウト
- `daw_gui/src/view/mixer_strips.rs:349` / `:363` / `:385` / `:399` — `draw_strip` 呼び出しと `entry.track_id`
- `daw_gui/src/view/mixer_strips.rs:421-442` — `draw_strip` の引数宣言 (`track_idx: u32` は :437)
- `daw_gui/src/widgets/arrangement/run.rs:2302` — 旧グリフ分岐 (arrangement)
- `daw_gui/src/widgets/arrangement/run.rs:2464-2470` — 旧 toggle インライン複製
  (幽霊コメント :2464、`push_edit` の 1 行が :2468)
- `daw_gui/src/widgets/arrangement/run.rs:2183` / `:2329` — automation lane の `+` / `-`
- `daw_gui/src/widgets/arrangement/run.rs:2320-2326` — lane disclosure を `+`/`-` にした経緯の
  コメントブロック。旧 `▽`/`▷` が font 不在で不可視 click target になった話は :2321
  (glyph 変更時に font 実在を疑う根拠)
- `daw_gui/src/widgets/arrangement/run.rs:2401-2402` — 「直接 flip なので net-zero」コメント
- `daw_gui/src/widgets/arrangement/view_build.rs:244` — `collapsed` bool の生成点 (唯一)
- `daw_gui/src/widgets/arrangement/view_build.rs:397` — `(t.collapsed, t.automation_lanes_collapsed)`
  を `heavy()` キャッシュキーに畳み込む (= 折り畳み状態が変われば再描画される)
- `daw_gui/src/view/track_inspector/modulation_rack.rs:379` — 3 つ目の複製 (▸/▾)
- `daw_gui/src/view/track_inspector/modulation_rack.rs:390` / `:770` / `:773` — `18.0` の手写し 3 か所
- `daw_gui/src/handler/selection_view.rs:1212-1224` — `is_hidden_under_collapsed_group`
- `daw_gui/src/handler/grouping.rs:98-104` — group track を最上位の子の直前へ insert
- `daw_gui/src/handler/view_model.rs:437` / `:463` — routing ラベルの区切り ▸ (disclosure ではない)
- `daw_gui/tests/app_state/modulation_arm.rs:187` — 上を assert しているテスト
- `daw_gui/src/app.rs:533` / `:541` / `:564-576` / `:577` — `handle_event` の前段 3 段と match 開始
- `daw_gui/src/state/song_doc.rs:420-428` — `begin_event` (snapshot も dirty も動かさない)
- `daw_gui/src/event.rs:1643-1646` / `:1846` — 非編集 event は catch-all ラベルで足りる旨と実体
- `ui/crates/ui/src/widgets/button.rs:38-49` / `:56-68` — `button_at` / `button_at_sized`
- `ui/crates/ui/src/widgets/button.rs:179-186` / `:199` — 枠の描画と `fit_text_ellipsized`
- `ui/crates/ui/src/ui.rs:2033` — `fit_text_ellipsized` 本体 (収まらなければ `…` に落とす)
- `ui/crates/renderer/src/pipelines/glyph.rs:25` — `DEFAULT_FONT_FAMILY = "HackGen Console NF"`

フォント実測 (2026-08-28、`%LOCALAPPDATA%\Microsoft\Windows\Fonts\HackGenConsoleNF-Regular.ttf`
の `head` / `cmap` / `hmtx` を直接パース):

| 符号位置 | advance | 16px フォントでの幅 |
|---|---|---|
| U+25B6 ▶ / U+25BC ▼ / U+25B8 ▸ / U+25BE ▾ / U+2026 … | 540 / 1024 em (0.527 em) | 8.44 px |
| 参考: U+3042 あ (全角) | 1080 / 1024 em | 16.88 px |

→ 三角 4 種は**すべて同じ advance**。glyph 族の変更で幅は変わらず、mixer の 14px 幅
ボタンでも modulation rack の 16px 幅ボタンでも ellipsis には掛からない。

一次情報:

- Apple HIG (OS 8 原典) — "the triangle rotates downward and the window expands …
  Clicking the triangle again restores the view to its original (collapsed) state and
  **turns** the triangle back to the right"
  <https://dev.os9.ca/techpubs/mac/HIGOS8Guide/thig-24.html>
- Microsoft Learn / WinUI TreeView — "Collapsed nodes use a chevron pointing to the right,
  and expanded nodes use a chevron pointing down."
  <https://learn.microsoft.com/en-us/windows/apps/design/controls/tree-view>
- CSS Counter Styles Level 3 §6.3 — `disclosure-open` = ▾ U+25BE / `disclosure-closed` = ▸ U+25B8、
  "If the image is directional, it must respond to the writing mode of the element"
  <https://www.w3.org/TR/css-counter-styles-3/>

検証環境 (実測):

- `.claude/guards.jsonl:73` — `no-bulk-test-run`。`cargo test … -p daw_gui` を
  `--test <name>` 無しで block する (§6 / §8)
- `Makefile:146-148` — `test-nolaunch`。:148 の `--lib` が `view/disclosure.rs` の
  unit test を回す
- `Makefile:141-142` — `test` (preflight + daw_gui 起動を伴う。打たない)

他項目の計画:

- `docs/plan_rmd_index.md` — 6 項目の統合順の正本 (#74 は第 2 波)
- `docs/plan_rmd_71_device_copy.md:66` / `:480` / `:729` — #71 が `event.rs` の
  `AppEvent` を触る箇所 (#74 と同じ enum)
- `docs/plan_rmd_71_device_copy.md:103` / `:114` / `:1516` — #71 が **`mixer_strips.rs`
  を触らない**根拠
- `docs/plan_rmd_71_device_copy.md:49` / `:84` / `:85` — #71 の `track_inspector/` 内訳
  (`mod.rs` / `chain_sections.rs` / 新規 `device_panel.rs`。`modulation_rack.rs` は含まない)

過去の関連 plan:

- `docs/plan_mixer_group_collapse.md` — mixer の折り畳み導入時 (FIXME #7)。
  「glyph / 色は arrangement の disclosure と揃える」(`:43`) と書いた箇所が #74 の起点。
  §4.11 で supersede note を足す。関係する行は `:16` / `:29` / `:43` / `:49`。


## 10. レビュー指摘への対応

裏取りレビューの指摘を 1 件ずつ現物で確認した結果。**却下した指摘も根拠付きで残す**。
指摘の内容はすべて**本文の該当箇所に反映済み**で、この節は「なぜそうしたか」の記録。

### 10.1 第 1 回 (2026-08-28)

| # | 指摘 | 判定 | 対応 / 根拠 |
|---|---|---|---|
| 1 | 引用行番号に軽微なズレ (定数ブロックは 33-39 でなく 35-39、`let track_id` は 331 でなく 332、421-437 は呼び出しでなく fn 宣言、button.rs は 180-187 でなく 179-186、`fit_text_ellipsized` は 199、`track_text_size` の field は 999) | **採用** | 全て現物で確認。§4.5 / §5 / §8 / §9 の該当箇所を実測値に差し替えた。呼び出し点は `:349` / `:363` / `:385` / `:399` と明記。 |
| 2 | stale doc は 5 件でなく 6 件 (`arrangement/mod.rs:1210` が漏れ)。旧版は「group disclosure を指すので正しい」と書いていたがそれは事実に反する | **採用** | `automation_disclosure_size` の全参照を grep (件数は当時「5」と書いたが実測 8 行。10.2 #5 で訂正)。読むのは `run.rs:2188-2190` / `:2333-2335` の `+`/`-` lane のみで、group は `run.rs:2307` の `track_text_size`。§4.9(c) に追加、§7 を 6 件に修正。あわせて `disclosure_color` の doc (`mod.rs:1044`) も group と lane の両方に使われているのに group しか書いていないので §4.9(d) を追加。 |
| 3 | `modulation_rack.rs:770` / `:773` の `18.0` が `:390` と同じ量の手写し。旧版の 4px シフトはこの 2 行を壊す | **採用 (ただし原因ごと消滅)** | 現物確認: `18.0` はこの 3 か所のみ。指摘 6 の実測で寸法変更そのものが不要になったため 4px シフトは撤回。あわせて 3 か所を `MOD_NAME_INSET` へ導出化した (§4.7(b)(d))。 |
| 4 | 旧版は mixer の手写しを直す一方で、rack に `20.0` / `lx + 22.0` の新しい手写しを作っていた | **採用** | 寸法変更を撤回し、既存の `18.0` 3 か所も導出にした。§1.3 に「どの量を昇格させ、どこまで移行するか」の規則を明文化して、同じ判断を再現できるようにした。 |
| 5 | 旧版 §4.5(e)「pad の手写しは test の 2 か所だけ」は誤り。production の `:782` 第 1 項と `:790` も pad、`:33` の `NAME_BAND_H` doc も同じ | **採用** | 現物確認: `:782` の第 1 項 = `draw_strip` の上 pad、`:790` は直上 doc が「`pad + 12.0`」と明言、`NAME_BAND_H = 24.0` = `6 + 18`。§4.5(a)(e) で 4 か所 + `NAME_BAND_H` を同時に移行。値は不変。`:562` / `:785` / `:1131` の `6.0` は M/S 行の下マージン (別の量) なので触らない旨も明記。 |
| 6 | `button_at_sized` が既にあるので、幅を推測で広げる必要は無い | **一部採用 (より強い結論に置換)** | `button_at_sized` の実在を確認 (`button.rs:56`、doc `:51-55`)。ただし font size を下げる必要すら無い: 既定フォント `HackGen Console NF` の `hmtx` を直接読むと ▶/▼/▸/▾/… は**すべて advance 540/1024 em (16px で 8.44px)** で、16px 幅のボタンに余裕で収まる。よって **rect も font size も変えない**のが正解。`button_at_sized` は「万一 sign-off で `…` に化けたときの唯一の直し方」として §8 に残した (rect を広げるのは §4.7(d) を壊すので禁止)。旧版の「▶ は小三角より広い」という前提は**実測で否定**。 |
| 7 | §2 の最終確認 grep が「0 件」と「コメントは残ってよい」で自己矛盾。しかも `view_model.rs:463` の正当なコード literal を拾い、直すとテストが落ちる | **採用** | §2.1 を全面書き換え。doc/コメント行を落とす grep を示し、**期待される残存を 2 つに名指し固定** (`view/disclosure.rs` と `handler/view_model.rs:463`)。§7 / §8 にも「463 は直さない」を明記。 |
| 8 | `handle_event` 経由化は純 refactor ではない (shutdown drop / `begin_event` / export block-list) のに計画が未分析。`run.rs:2401` の「直接 flip」コメントも stale になる | **採用** | §4.4.1 を新設し 3 段を現物で確認して表にした (`app.rs:533` / `:541`→`song_doc.rs:420-428` / `:564-576`)。`undo_label` に arm を足さない理由 (`event.rs:1643-1646` の catch-all 設計) も §4.3 に明記。`run.rs:2401-2402` の文言修正を §4.6(e) として追加。 |
| 9 | §4.8 に書かせる `collapsed_groups` の地図が不完全 (`project.rs:324-325` / `:362` / `grouping.rs:223` が漏れ) | **採用 (ただし書き方を変更)** | 全 10 参照を関数名付きで §4.8 の表にした。**doc コメントには file:line を書かない** — doc に行番号を埋めると次の編集で腐るので、doc には「1 経路のみ / load・New で clear / 削除・ungroup・undo 照合で prune」という規則だけを書き、座標は本計画書に置く。 |

レビューが「問題なし」と確認した項目 (再検討不要): 3 プロセス貫通・bincode・`WIRE_SOURCES`
無関係 / RT 制約なし / フェーズ分けなし / §5 の対象外はコストでなく確定スコープ根拠 /
妥協語なし / `AppEvent` は両呼び出し元で既に in scope / `track_idx` は stable id /
god file budget に余裕 / 新規ファイルに SPDX 不要 / 幽霊コメントはちょうど 3 件 /
`ui/crates/ui` の menu・dropdown の三角は別ドメイン。
(第 1 回で「`cargo test -p daw_gui --lib disclosure` は成立」とされた点だけは
**第 2 回で覆った** — ガードが block する。10.2 の #2。)

### 10.2 第 2 回 (2026-08-28、BLOCKING は 0 件)

| # | 指摘 | 判定 | 対応 / 根拠 |
|---|---|---|---|
| 1 | §7 の受け入れ基準「`grep -n "18\.0" modulation_rack.rs` が空」が §4.7(b) と自己矛盾する (`MOD_NAME_INSET` の doc 自身に `18.0` が入る) | **採用** | §2.1 と同じ形に統一。§7 を「doc/コメント行を落とした grep が空」に書き換え、期待される残存 (`MOD_NAME_INSET` の doc) を名指しした。同じ欠陥が **mixer 側の `6.0` 基準にも潜在**していた (基準が散文だけで grep が無かった) ので、そちらにも残存 5 行を名指しした grep を足した。パターンは POSIX ブラケット式 + `[^0-9]` (`16.0` / `118.0` を拾わない)。実測で両方の grep の出力を確認済み。 |
| 2 | §6 の `cargo test -p daw_gui --lib disclosure` は `guards.jsonl:73` (`no-bulk-test-run`) が block する | **採用** | 実測で確認 (`-p daw_gui` に当たり、`--test ` が無いので block)。コマンドを §6 から**削除**し、`Makefile:148` の `--lib` が同じ unit test を回すことを明記。`DAW01_ALLOW_LAUNCH=1` での迂回は「起動する」宣言の嘘になるので採らない、ガード行も書き換えない、と理由付きで書いた。§8 にも 1 行追加。 |
| 3 | §4.8 を「全参照 (`grep -rn collapsed_groups`)」と書いているが生 grep は 24 行で表は 10 行。§7 の基準が判定不能 | **採用** | 表の性格を「実装後に残るべき**コード**参照」と明示し、生 grep が追加で含む 3 種 (コメント 9 行 / field 宣言 1 行 / 実装で消える反転 2 か所) を列挙。判定用に doc/コメントを落とす grep を置き、§7 の基準を「insert / remove が §4.4 の handler だけ」に変えた。表の `project.rs:324-325` も `:325` (:324 はコメント) に訂正。 |
| 4 | `docs/plan_mixer_group_collapse.md:43` が #74 で偽になるのに扱いが未決定 | **採用** | §4.11 を新設し、supersede note を冒頭に足す作業として本文化 (§4 の対象一覧も 10 → 11 ファイル)。**本文行は書き換えない** (当時の決定の記録)。触る / 触らないの規則も明文化: **#74 が反転させた決定を書いた文書だけ**。会話ログと、#028 follow-up が既に無効化した `plan_automation.md` 等は範囲外 — その非対称の理由 (src の doc は実装時に読まれて実装を誤らせる / 過去計画は記録) も書いた。 |
| 5 | 行番号の軽微なズレ 5 件 | **採用** | 全て現物で再確認して訂正: §4.5(c) 487-512 → **487-513**、§4.4 arm 814-822 → **814-823**、§4.6(f)/§9 run.rs 2320-2325 → **2320-2326** (▽/▷ の話は :2321、▶▼ は :2322)、§4.3 の `event.rs:1643-1645` → **:1643-1646** (§9 と一致)、§4.9(c)「全 5 参照」→ **8 行** (宣言 1 / default 1 / 読み 6)。あわせて §4.7(d) を 768-774 → **769-775** (`x`=:770 / `w`=:773)、§9 の `mixer_strips.rs:692` → **:691-694**、`run.rs:2464-2469` → **:2464-2470** も訂正。 |
| 6 | 冒頭の r.md #74 引用が原文と 1 文字違う (原文「小トラック」を「子トラック」に直して引用していた) | **採用** | 原文ママに戻し、typo と読める旨を注記。**r.md は編集しない** (memory `feedback_defer_todos_to_fixme`)。 |
| 7 | §2 が #71 を前提に置く根拠を書いていない | **採用 (index の根拠は訂正)** | 実際の重なりを #71 の計画で確認し §2 に明記: **`event.rs` (同じ `AppEvent` enum、#71 は `:66`/`:480`/`:729`) と `app.rs` (#71 は cache 初期化 `:466`、#74 は match arm) の 2 ファイル**。index の「`mixer_strips.rs` を両方が触る」は #71 の計画自身 (`:103` / `:114` / `:1516`「ミキサーには何も足さない」) と矛盾するので、その旨も記録した。**順序は変えない** (index が正本)。`track_inspector/` も #71 は `mod.rs` / `chain_sections.rs` / `device_panel.rs` で、#74 の `modulation_rack.rs` とは別ファイル。 |

第 2 回で **BLOCKING は 0 件**。「unresolved (改訂側)」として挙がった 4 点はいずれも
計画の性質上その形で正しいものとして維持している (#77/#71 待ちの前提 = §2、
modulation rack のグリフ族変更 = §6 実機項目 4 で目視、arrangement と mixer で
別マークになる仕様 = §1.1 と `disclosure.rs` の module doc に意図として明記、
本改訂も Rust ソースを 1 行も触らないので `make check` 等は未実行)。
