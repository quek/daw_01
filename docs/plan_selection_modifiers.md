# 選択修飾キーの統一 (r.md #35)

クリップ / ノート / オートメーション / オーディオイベントの Shift+click・Ctrl+click を
OS 標準 (Windows Explorer / macOS Finder) および REAPER の規約に統一する。

## 1. 症状 (調査結果)

面ごとに規則がバラバラで、2 つは**完全に無反応**だった。

| 面 | 無修飾 | Ctrl+click | Shift+click |
|---|---|---|---|
| トラックヘッダ | Single | Toggle | RangeFromAnchor |
| Arranger セクション | Single | Toggle | RangeFromAnchor |
| **クリップ** | Single | Toggle | **無反応** |
| **ノート** | Single | **Single (置換)** | **無反応** |
| オートメーション点 | Single | Toggle | **Toggle** (範囲でない) |
| オートメーションクリップ | Single | Toggle | **Toggle** (範囲でない) |
| Audio Editor イベント | Single | **効かない** | **Toggle** (範囲でない) |
| 投げ縄 (全面) | REPLACE | XOR | UNION |

### 根本原因

- **Shift+クリップが無反応**: `arrangement/run.rs:269` の clip_drag gate `(!shift || ctrl || resize)`
  が Shift を弾き、`arrangement/release.rs:792` の `marquee_zone_ok` が Move zone + Shift の press を
  marquee に奪う。移動 0 px なら `DragRect` は 0 サイズで、`geometry.rs:1605` の `rects_intersect` は
  厳密不等号なので何とも交差しない → `inside` 空 → Shift=UNION で空集合を足すだけ = 変化なし。
  結果、`release.rs:714` の Shift=Union 分岐は**到達不能なデッドコード**だった。
- **Shift+ノートが無反応**: `piano_roll/run.rs:179` (note drag が `!shift` gate)、`:864`
  (marquee は `note_hit().is_none()` 必須)、`:896` (`pending_click` が `!shift` gate) の
  3 経路すべてが Shift を排除。
- **Ctrl+ノートが置換**: `pending_click` は shift しか見ておらず Ctrl が素通り →
  `run.rs:1259` で `vec![hit_id]` = 無条件置換。

## 2. 一次情報 (参照した規約)

| 出典 | 規約 |
|---|---|
| REAPER User Guide v7.78 (MIDI Note, Left click) | Ctrl+click = トグル / Shift+click = アンカーから当該ノートまでの範囲を追加 |
| REAPER User Guide (Media item, left click) | Ctrl+click = トグル |
| REAPER User Guide (Track Control Panel / automation items) | click = 単一 / Ctrl+click = 非連続追加 / Shift+click = 連続範囲 |
| Ableton Live 12 マニュアル (Session/clip/track/scene) | Shift+click = 隣接 (範囲) / Ctrl+click = 非隣接 (個別追加) |
| Bitwig ユーザーガイド (track list / browser filter) | SHIFT-click = 連続範囲 / CTRL-click = 個別トグル |
| macOS Finder (Apple) | Command+click = 個別トグル / Shift+click = アンカーからの連続範囲 |
| Windows (Microsoft) | Ctrl = 個別複数選択 / Shift = 連続複数選択 |

daw_01 は既にこの規約を `SelectModifier { Single, RangeFromAnchor, Toggle }`
(`widgets/arrangement/mod.rs:670`) として実装済み。トラックヘッダとセクションだけが使い、
クリップ / ノート / オートメーション / オーディオイベントが使っていなかったのが問題の正体。

### 矩形選択の起動 (#75 との衝突)

Shift+click が範囲選択になると、#75 で入れた「クリップの上から Shift+ドラッグで矩形選択」と衝突する。
一次情報では**どの DAW も Shift+ドラッグで矩形選択を起動しない** (Shift は選択の意味に予約)。
各社は別ボタン / 別ツール / 別修飾キーに逃がしている:

| DAW | 矩形選択の起動 | クリップの上から |
|---|---|---|
| REAPER | 右ドラッグ (既定) | ○ |
| Cubase | Range Selection ツール | ○ |
| Logic Pro | Marquee ツール | ○ |
| FL Studio | Ctrl+ドラッグ (Ctrl = 選択モード) | ○ |
| Bitwig | 空き場所からの左ドラッグ | × |

→ **REAPER 方式 (右ドラッグ)** を採る。daw_01 は既に右クリックをコンテキストメニューに使っており、
これも REAPER と同一配置 (右クリック単発 = メニュー、右ドラッグ = 矩形選択)。

## 3. 確定仕様

### 3.1 click の規則 (全選択面共通)

- 無修飾 click = **Single** (置換)
- Ctrl+click = **Toggle** (個別に足し引き)
- Shift+click = **RangeFromAnchor** (アンカーからの範囲)
- **アンカーは無修飾 click と Ctrl+click で更新し、Shift+click では更新しない**。
  同じ基点から繰り返し Shift+click して範囲を伸縮できる (Explorer / Finder / REAPER)。
  既存のトラックヘッダ実装は Single/Range で更新・Toggle で据え置きだったので逆に直す。
- アンカーが無い / 別クリップ・別レーンに居る → Single にフォールバック。

### 3.2 範囲の定義

| 面 | 範囲 |
|---|---|
| クリップ | (可視トラック行 × 時間) の**長方形ブロック**。行 index が [min,max] かつ時間帯が交差する全クリップ |
| ノート | (音程 × 時間) の**長方形ブロック**。pitch が [min,max] かつ時間帯が交差する全ノート |
| オートメーション点 | 同一 automation clip 内の beat 範囲 (点は時間順に一意なので 1 次元でブロックと等価) |
| オートメーションクリップ | (可視 lane 行 × 時間) の長方形ブロック |
| トラック / セクション | 既存の 1 次元順序範囲 (可視順 / 開始拍順) |
| Audio Editor イベント | clip 内 event の時間順 1 次元範囲 |

時間帯の交差判定は投げ縄と同じ「触れていれば入る」(REAPER の marquee と同じ)。

### 3.3 投げ縄 (矩形選択)

修飾の意味は現状維持 (無修飾 = REPLACE / Shift = UNION / Ctrl = XOR)。起動方法だけ変える:

- **左ドラッグ**: 空きレーン / 空きグリッドからのみ (クリップ・ノートの上からは起動しない)
- **右ドラッグ**: lanes / grid のどこからでも (クリップ・ノートの上からも) 起動
- **右クリック単発** (移動 < 4px): コンテキストメニュー

daw-ui の `context_menu_for` は右ボタン **press** で開いていたので **release かつ移動 4px 未満**
に変える。Windows の `WM_CONTEXTMENU` も右ボタン UP で飛ぶので、これが本来の標準でもある。

## 4. 実装

### 4.1 daw-ui (`ui/crates/ui`)

- `ui.rs`: `secondary_press_pos` を frame 跨ぎで保持し、release frame に移動量 < 4px なら
  `pending_secondary_click` を立てる (`last_click` / `pending_double_click` と同 idiom)。
  `take_secondary_click_in_rect(rect)` で消費。
- `ui.rs`: `take_secondary_drag_rect_in_rect(wid, bounds)` を追加。既存 `take_drag_rect_in_rect` と
  共通の private impl にボタン別の press/release edge を渡す (DRY)。
- `menu.rs`: `context_menu_for` の open trigger を `secondary_just_pressed` から
  `take_secondary_click_in_rect` へ。

### 4.2 daw_gui 共通 (`widgets/select_modifier.rs` 新設)

`SelectModifier` を `arrangement/mod.rs` からここへ移し (ピアノロール / Audio Editor も使うため)、
選択集合の遷移と範囲計算を 1 箇所に集約する:

- `SelectModifier::from_modifiers(shift, ctrl)`
- `SelectModifier::resolve(prev, clicked, range_fn) -> Vec<T>`
- `range_block(items, anchor, clicked) -> Vec<T>` — (行 × 時間) 長方形ブロック
- `range_ordered(order, anchor, clicked) -> Vec<T>` — 1 次元順序範囲

### 4.3 アンカー (SSoT)

「選択集合の末尾 = アンカー」は Shift+click で選択集合ごと書き換わるため基点として使えない。
`SelectionState` (`state/selection.rs`) に面ごとの明示フィールドを置く。
`ArrangementState.selection_anchor` (widget state 上のトラックアンカー) もここへ移す。

### 4.4 面ごとの改修

- **クリップ**: `run.rs:269` の gate から `!shift` を外して Shift+press でも clip_drag session を
  作り、既存の demote → `clip_short_click_pos` 経路に乗せる (Shift+Move ドラッグは通常の移動)。
  Shift+resize の time-stretch (#61) は不変。`release.rs:691` を `SelectModifier` + `range_block` へ。
  `marquee_zone_ok` から Move zone の Shift 起動を外す (空きレーン専用に戻す)。
- **ノート**: `piano_roll/run.rs:179` / `:241` / `:896` の `!shift` gate を外し、demote 経路で
  `(ctrl, shift)` を持ち回る (arrangement と同 idiom)。`:1253` を `SelectModifier` + `range_block` へ。
- **オートメーション点 / クリップ**: `release.rs:326` / `:518` の `shift || ctrl` 一括トグルを
  `SelectModifier` へ。
- **Audio Editor イベント**: `view/audio_editor.rs:832` の Shift トグルを `SelectModifier` へ
  (Ctrl が効かない穴も塞ぐ)。投げ縄 (`:911`) にも Ctrl=XOR を追加。
- **トラック / セクション**: アンカー更新規則を 3.1 に合わせる。
- **右ドラッグ矩形選択**: arrangement (`lanes` 全域) と piano_roll (`grid` 全域) に追加。
