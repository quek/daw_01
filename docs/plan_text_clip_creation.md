# plan: text clip 生成を「空きレーン右クリック」に統一（File→Add Text Clip 廃止）

## 主訴（ユーザー報告 2026-06-03）

「text トラックは他のトラックと違うのか？ 同じなら File メニューの **Add Text Clip** を削除して、
他のトラックと同じように **C-t** で（普通の track を）追加し、その後 Text クリップを追加するように
したい。」

→ ユーザー選択: **空きエリア右クリック** で Text クリップを追加する（option A）。

## 調査結論: text トラックは他トラックと同一

v16（`docs/plan_text_overlay.md`）で旧 `Track.kind: TrackKind { Audio, Video }` は**廃止済み**。
今は全 track が unified に audio path + visual composite path 両方を保持する（REAPER 流、同 track 上で
audio / midi / video / image / text clip を混在可能）。`common/src/model.rs:1083-1091`。

つまり **「text トラック」という種別は存在しない**。text は `ClipContent::Text(TextContent)` という
**clip 内容の一種**にすぎず（`model.rs:1482-1510`）、どの track にも載る。

現状の File → **Add Text Clip**（`AppEvent::AddTextClip` → `app.rs:14610 action_add_text_clip`）は、

- `ClipContent::Text` を alloc し、
- `name="Text"` の**普通の track を新規に index 0（先頭）に挿入**し、
- そこに text clip を 1 個載せる、

という「track 新規作成 + clip 作成」の合体処理。text を File メニューに置き、先頭 track 強制という
特権扱いは、他の clip 種別（後述）と不整合。

## 現状の clip 生成経路（調査）

| 種別 | 生成経路 |
|------|----------|
| MIDI | 空きレーン **dblclick** → `ArrangementEditRequest::DoubleClickEmpty { track, beat }`（`arrangement_view.rs:1383`）→ `AppEvent::CreateClip` → `create_clip()`（`app.rs:11868`、`ClipContent::default()` = MIDI） |
| Audio / Video / Image | File メニュー Import（外部メディアなので妥当） |
| Automation | lane 上 dblclick → `CreateAutomationClip` |
| **Text** | **File メニュー Add Text Clip のみ**（← 本件で廃止） |

text は MIDI と同じく「アプリ内で著作する内容」なのに、生成経路だけ File メニューに孤立している。
→ **MIDI と同じくタイムライン上で生成する**のが自然（REAPER の「右クリック空きエリア →
Insert new MIDI item」idiom）。

## 望む最終形

1. **C-t**（`daw.add_track` → `AppEvent::AddInstrumentTrack` → `action_add_instrument_track`）で
   普通の track を末尾に追加。← 現状維持、変更なし。
2. タイムラインの**空きレーン（clip の無い領域）を右クリック** → コンテキストメニュー
   **「Text クリップ」** → その beat 位置（snap 済み）にその track 上へ `ClipContent::Text` clip を 1 個追加。
3. File メニューの **Add Text Clip は削除**。

メニュー項目は今は「Text クリップ」1 つだが、将来 MIDI 等も同じ空きエリアメニューに統合できる
（dblclick=MIDI と併存）。本件のスコープは Text のみ（要件外の MIDI 追加はしない）。

## gui_01 依存（#071）

空きレーン右クリックの

- どの track の空き領域か（track id）
- どの beat か（snap 済み）
- メニューを出す画面座標（右クリック位置）

は **arrangement widget がレイアウト SSoT として所有**（header_pane / lanes / ruler の分割、
`px_to_beat`、snap、track 行の y 範囲はすべて widget 内部）。daw_01 側で再計算するのは SSoT 違反で
脆い（ruler 高・scroll・zoom・track 高に追従できない）。

→ widget が空きレーンの **secondary（右）click** を検出し、`DoubleClickEmpty` と対になる新 request
で `{ track, beat(snap 済み), pos }` を emit する設計を gui_01 に依頼（`docs/gui_01_conversation.md`
**#071**）。最終的な機構（新 request か、lane body rect の公開か）は gui_01 にお任せするが、daw_01 の
希望は b1（新 request、snap 済み beat を widget が計算して emit）。理由は #071 に記載。

## daw_01 側の変更（gui_01 #071 landing 時に atomic に実施）

- `daw_gui/src/view/root.rs:216-220` — File メニューの `m.item("Add Text Clip", ...)` を削除。
- `daw_gui/src/app.rs`
  - `AppEvent::AddTextClip`（`:3490` 付近）と `action_add_text_clip`（`:14610`）を削除。
  - text clip 構築（content alloc + clip 構築）を再利用する形で、
    **`add_text_clip_to_track(track_idx, start_beat)`** を追加。`create_clip`（`:11868`）と同 idiom
    で対象 track に push する（先頭強制ではなく、指定 track）。
  - 新 `AppEvent::AddTextClipAt { track: u32, start_beat: f64 }`（`CreateClip` と同 shape）。
- `daw_gui/src/view/arrangement_view.rs`
  - 新 request handler を `make_edit` の match に追加（`DoubleClickEmpty` の隣、`:1383`）。
  - 右クリック位置に「Text クリップ」コンテキストメニューを表示し、選択で
    `AppEvent::AddTextClipAt` を発火。表示は color_picker の overlay idiom
    （`open_color_picker` / `render_color_picker_overlay` `arrangement_view.rs:901`）か、
    `context_menu_for` のいずれか（#071 で確定する API 次第）。

## 機能を壊さない sequencing（重要）

text clip 生成経路は現状 File メニューしか無い。**File メニュー項目を先に消すと、gui_01 #071 landing
まで text clip を作る手段が消える**。→ `feedback_recovery_priority`（機能を消したまま放置しない）に従い、

1. 本ドキュメント + gui_01 #071 要望提出（本セッション）。
2. gui_01 が #071 を landing（widget に新 request 追加）。
3. daw_01 で **新経路の wire + 旧 File メニュー項目/旧 action の削除を 1 commit で atomic に**実施。
   （rust-analyzer の non-exhaustive match で新 variant landing を自動検知 → 即着手、
   `feedback_gui_01_auto_resume`）

これにより text clip 生成手段が途切れる瞬間が無い。

## SSoT

- beat 位置 / snap / 空きレーンの hit-test / レイアウトは **widget が単一所有**。daw_01 は widget が
  emit した beat をそのまま使う（`DoubleClickEmpty` と同様、daw_01 側 post-process 無し）。
- 「どの track の空きか」も widget が track id で渡す（daw_01 は id→index 変換のみ、`DoubleClickEmpty`
  handler `arrangement_view.rs:1387` と同 idiom）。
- text clip の中身（default text / 64px / 中央横帯 / 8 beats 既定）の決定は daw_01 が所有
  （現 `action_add_text_clip` の defaults を踏襲）。
