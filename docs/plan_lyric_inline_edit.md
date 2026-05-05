# daw_01 #017: `Ui::piano_roll` 歌詞 inline 編集 + モーラ自動分配

## Context

daw_01 (sibling project DAW) からの要望 [F:/dev/daw_01/docs/gui_01_conversation.md](F:/dev/daw_01/docs/gui_01_conversation.md) #017 (2026-05-05 投稿、Open)。

**問題**:
- daw_01 は VOICEVOX 歌唱機能を持つ DAW
- `Note { lyric: Option<Arc<str>> }` schema は M9 Phase 44c で完備、`Ui::piano_roll` の歌詞描画も済
- **歌詞を入力する UI が無い**。JSON ファイル直編集するしかない
- 歌唱パイプライン (A1: VOICEVOX 統合) 着手の前提

**daw_01 推奨**: Option C = `Ui::piano_roll` widget 内蔵 (text_input overlay + L キー起動 + Enter で次 note へモーラ単位自動分配 + Esc cancel)。

**期待 outcome**:
- piano_roll で note 1 つ選択 → L → 該当 note rect 内に text_input overlay
- "あいうえ" + Enter → 4 note に "あ"/"い"/"う"/"え" 一括分配 (1 undo)
- "しゅんかん" → 4 モーラ ("しゅ"/"ん"/"か"/"ん") 分割
- IME (CJK preedit/commit) 対応、編集中は drag/resize/wheel/click 全短絡

## 全体方針

### Option C 採用 (widget 内蔵)
- text_input_at_focused ([crates/ui/src/widgets/text_input.rs:439](crates/ui/src/widgets/text_input.rs:439)) が「初回 show 自動 focus + 全選択 (M14 Phase 57)」を提供 → note 切替で id が変わるたびに再 focus + 全選択が **自動発火**
- `set_typing_focus(true)` が focus 中に立つ → global shortcut の自動抑制
- daw_01 AppData フィードバック量最小化 (Option B だと note rect map・lyric_editing state・buffer の 3 つを app 側で持つ必要)
- IME 状態管理が二重化しない

### daw_01 #017 への 6 質問回答

| # | 質問 | 回答 |
|---|------|------|
| 1 | Option C vs Option B | **Option C 採用** |
| 2 | L キー単独で起動、selected.len()==1 のとき | **YES + caller opt-in shortcut**。Default `style.lyric_edit_shortcut = Some("piano_roll.edit_lyric")`、daw_01 が `host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L")` を 1 度呼ぶ。修飾なし L のため typing_focus 中は自動抑制 (text_input 入力に化ける)。複数選択 / 選択無し時 no-op |
| 3 | next-note 順序 = (start_beat asc, pitch desc) | **YES**。歌詞は通常メロディラインに沿うため、同 beat なら高 pitch が次に来るほうが歌唱順と一致 |
| 4 | `split_into_morae` の置き場所 | **gui_01 widget 内に `pub fn split_into_morae(text: &str) -> Vec<String>`** (`crates/ui/src/widgets/piano_roll.rs`)。30 LOC で軽量、daw_01 以外 (将来のボーカルエディタ等) でも再利用余地あり |
| 5 | 空文字列 commit | **widget で `None` に正規化**。`Some("")` を Model に保持しても描画上等価で undo 履歴を汚すだけ |
| 6 | 残 note 数 < 入力モーラ数の境界 | **余りは捨てる**。「最後 note に全結合」は 1 note = 1 モーラの VOICEVOX 前提と整合しない。`PianoRollResponse.lyric_overflow_morae: usize` で daw_01 に通知 (status bar 表示等可能) |

### batch Edit 採用
**`SetLyrics(Vec<(NoteId, Option<String>)>)`** 単一 variant (単数 `SetLyric` は持たない)。
- 1 commit = 1 Edit = 1 undo 単位
- 1 note 編集も `vec![(id, lyric)]` で表現可、API surface 最小

## 確定 API

### `crates/ui/src/widgets/piano_roll.rs`

```rust
// 1) NotesEditRequest 拡張 (1 variant 追加、破壊的)
#[derive(Debug)]
pub enum NotesEditRequest {
    Add(Vec<Note>),
    Delete(Vec<Note>),
    Move(Vec<MoveDelta>),
    Resize(Vec<ResizeDelta>),
    Select { prev: Vec<NoteId>, next: Vec<NoteId> },
    /// (NEW) note 群の lyric を一括更新。1 commit = 1 Edit = 1 undo 単位。
    /// `lyric == None` で歌詞削除 (空文字列 commit は widget 内で None に正規化)。
    /// 一括モーラ分配時は Vec の順序がそのまま分配順 (start_beat asc, pitch desc)。
    SetLyrics(Vec<(NoteId, Option<String>)>),
}

// 2) PianoRollResponse 拡張 (field 追加、破壊的だが Default::default で互換)
#[derive(Clone, Debug, Default)]
pub struct PianoRollResponse {
    // ... existing fields ...
    /// (NEW) 歌詞編集 mode 中の note id。daw_01 が「他 UI grey-out」「Ctrl+Z 抑制」
    /// 等の判断に使う。drag/resize/wheel/click と同時には None。
    pub lyric_editing: Option<NoteId>,
    /// (NEW) 直近 commit で「note 数より入力モーラが多くて捨てた数」。
    /// 0 なら通常、>0 なら daw_01 で status bar に表示等可能。
    pub lyric_overflow_morae: usize,
}

// 3) PianoRollStyle 拡張 (field 追加)
pub struct PianoRollStyle {
    // ... existing fields ...
    /// (NEW) 歌詞編集モード起動 shortcut name。`None` で機能無効。
    /// Default は `Some("piano_roll.edit_lyric")`。caller は
    /// `host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L")` を 1 度呼ぶ。
    pub lyric_edit_shortcut: Option<&'static str>,
}

// 4) split_into_morae 公開 helper (新規 pub fn)
#[must_use]
pub fn split_into_morae(text: &str) -> Vec<String>;

// 5) PianoRollState (内部) 拡張
#[derive(Debug, Default)]
pub(crate) struct PianoRollState {
    note_drag: Option<NoteDragSession>,
    /// (NEW) 歌詞編集中の note id。Some なら text_input overlay 表示 + 他 input 抑制。
    lyric_editing: Option<NoteId>,
}

// 6) collect_next_notes_for_lyric (内部 helper)
fn collect_next_notes_for_lyric(notes: &[Note], from_id: NoteId, count: usize) -> Vec<NoteId>;
```

### `crates/ui/src/widgets/text_input.rs` (5 LOC 改修、後方互換)

```rust
#[derive(Debug, Clone, Default)]  // Copy 外す (String は Copy ではない)
pub struct TextInputResponse {
    pub focused: bool,
    pub committed: bool,
    /// (NEW) Enter / NumpadEnter 押下 frame でのみ Some。commit text を直接取得できる。
    /// 通常 frame は None。変更が無いまま Enter したケースは caller passed `text` の clone。
    pub committed_text: Option<String>,
}

// text_input_at の return 直前:
let committed_text = if committed {
    Some(new_text.clone().unwrap_or_else(|| text.to_string()))
} else {
    None
};
TextInputResponse { focused: was_focused, committed, committed_text }
```

`Copy` を外すのは `committed_text: Option<String>` のため破壊的だが、`TextInputResponse` を `Copy` で受けている caller は無いはず (主用途は `if resp.committed { ... }` パターンで `Clone` も不要)。要 audit。

## L キー shortcut 登録パスとモード遷移

### 登録 (caller opt-in)
```rust
host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L");
```
`with_default_bindings()` には**含めない** (修飾なし L は他文脈で別意味になる可能性、caller opt-in)。

### State machine (擬似コード、`piano_roll` fn 内冒頭)
```rust
let wid = WidgetId::ROOT.child((b"piano_roll_widget", &id));

// 0) Frame 開始時、lyric_editing が selected と sync しているか defensive check。
//    編集対象 note が notes から消えたら lyric_editing = None
let lyric_editing: Option<NoteId> = {
    let state: &mut PianoRollState = self.widget_state(wid);
    if let Some(eid) = state.lyric_editing
        && !notes.iter().any(|n| n.id == eid)
    {
        state.lyric_editing = None;
    }
    state.lyric_editing
};

// 1) L key 検知 (lyric_editing が None のときのみ)
if lyric_editing.is_none()
    && let Some(name) = style.lyric_edit_shortcut
    && self.take_shortcut(name)
    && selected.len() == 1
{
    self.widget_state::<PianoRollState>(wid).lyric_editing = Some(selected[0]);
}

// 2) lyric_editing が Some なら drag/resize/wheel/rect-select/click を全 short-circuit
//    既存 just_pressed_on_note / pending_click / shortcut "delete" / "add_note" / drag_rect
//    を `if lyric_editing.is_none() { ... }` でガード
```

### 遷移図
```
[Idle] ──L key (selected.len()==1)──> [Editing(id=selected[0])]
[Editing(id)] ──Enter, next exists──> [Editing(next_id)] (commit + 分配 + 自動 selection 切替)
[Editing(id)] ──Enter, no next──────> [Idle] (commit + 分配)
[Editing(id)] ──Esc────────────────── > [Idle] (commit せず破棄)
[Editing(id)] ──id が notes から消失──> [Idle] (defensive)
[Editing(id)] ──L key────────────────> 無視 (text_input が "l" 文字として捕食)
```

### text_input_at_focused との接続 (commit 検出 → 分配 → 次 note へ)
```rust
if let Some(edit_id) = lyric_editing {
    let edit_note = notes.iter().find(|n| n.id == edit_id);
    let Some(edit_note) = edit_note else { /* defensive */ };
    let raw_rect = note_to_rect(edit_note, view, grid);
    let clipped_rect = clip_to_grid(raw_rect, grid);
    if clipped_rect.w < 8.0 || clipped_rect.h < style.lyric_font_px + 2.0 {
        self.widget_state::<PianoRollState>(wid).lyric_editing = None;
    } else {
        let prefill = edit_note.lyric.as_deref().unwrap_or("");
        // id は ("piano_roll_lyric", edit_id) → note 切替で id 変化 → 自動 focus + 全選択
        let resp = self.text_input_at_focused(
            ("piano_roll_lyric", edit_id),
            clipped_rect,
            prefill,
            // on_change は per-keystroke の Edit 発行を抑止する placeholder。
            // 実際の SetLyrics は commit 検出パスで 1 回発行 (= 1 undo)。
            |_new_text| Edit::mutate(|_: &mut M| {}),
        );

        if resp.committed {
            let committed_text = resp.committed_text.clone().unwrap_or_default();
            let normalized = if committed_text.is_empty() {
                vec![] // 空文字 commit → 起点 note を None に
            } else {
                split_into_morae(&committed_text)
            };
            // 起点 note の歌詞 update count: 空入力時は起点 1 つを None に、
            // それ以外は morae.len() 個分の連続 note
            let target_count = normalized.len().max(1);
            let target_ids = collect_next_notes_for_lyric(notes, edit_id, target_count);
            let mut updates: Vec<(NoteId, Option<String>)> = Vec::new();
            for (i, nid) in target_ids.iter().enumerate() {
                let lyric = normalized.get(i).cloned().filter(|s| !s.is_empty());
                updates.push((*nid, lyric));
            }
            response.lyric_overflow_morae = normalized.len().saturating_sub(target_ids.len());
            if !updates.is_empty() {
                self.push_edit(make_edit(NotesEditRequest::SetLyrics(updates)));
            }
            // 次 note へ移動 (= 分配し終わった先の note id)
            let all_sorted = collect_next_notes_for_lyric(notes, edit_id, usize::MAX);
            let next_id = all_sorted.get(target_ids.len()).copied();
            self.widget_state::<PianoRollState>(wid).lyric_editing = next_id;
            // selection も自動追従 (daw_01 UI が同期、note border 強調が次 note へ)
            if let Some(nid) = next_id
                && selected != [nid].as_slice()
            {
                let prev = selected.to_vec();
                self.push_edit(make_edit(NotesEditRequest::Select {
                    prev,
                    next: vec![nid],
                }));
                response.selection_changed = true;
            }
        } else if !resp.focused {
            // Esc 検出: text_input が clear_focus_if_focused → resp.focused = false
            self.widget_state::<PianoRollState>(wid).lyric_editing = None;
        }
        response.lyric_editing = self.widget_state::<PianoRollState>(wid).lyric_editing;
    }
}
```

## モーラ分割境界条件

| 入力 | 出力 | コメント |
|------|------|---------|
| `""` | `[]` | 空文字 |
| `"あ"` | `["あ"]` | 1 char = 1 モーラ |
| `"きゃ"` | `["きゃ"]` | 拗音結合 |
| `"しゅんかん"` | `["しゅ", "ん", "か", "ん"]` | 4 モーラ |
| `"abc"` | `["a", "b", "c"]` | ASCII は 1 char = 1 モーラ |
| `"ぁい"` | `["ぁ", "い"]` | 先頭小書きは defensive で単独 |
| `"きゃっ"` | `["きゃっ"]` | 連続小書きは結合先 char に積まれる |
| `"ぱっと"` | `["ぱっ", "と"]` | 促音 |

実装:
```rust
const SMALL_KANA: &[char] = &[
    'ぁ','ぃ','ぅ','ぇ','ぉ','ゃ','ゅ','ょ','っ','ゎ',
    'ァ','ィ','ゥ','ェ','ォ','ャ','ュ','ョ','ッ','ヮ',
];
let mut out: Vec<String> = Vec::new();
for c in text.chars() {
    if SMALL_KANA.contains(&c)
        && let Some(last) = out.last_mut()
    {
        last.push(c);
    } else {
        out.push(c.to_string());
    }
}
out
```

## テスト計画

### Unit (`split_into_morae`、純粋関数)
- `split_basic`: "あ"→1, "abc"→3
- `split_combines_yo`: "きゃ"→1
- `split_combines_tsu`: "ぱっと"→2
- `split_consecutive_small_kana`: "きゃっ"→1
- `split_leading_small_kana_defensive`: "ぁい"→2
- `split_empty`: ""→0
- `split_long`: "しゅんかんいどう"→6

### Unit (`collect_next_notes_for_lyric`)
- `collect_returns_self_first`: 起点 note を `out[0]` に含む
- `collect_sorted_by_start_beat`: 異 beat は時間順
- `collect_same_beat_pitch_desc`: 同 beat なら pitch 高→低
- `collect_truncates_when_count_exceeds`: count > 残数 で truncate
- `collect_empty_when_id_not_found`

### Integration (widget 統合、`run_frame` ベース既存パターン、`mod tests` 追加)
- `lyric_edit_l_key_enters_mode_when_single_selected`
- `lyric_edit_l_key_noop_when_zero_or_multi_selected`
- `lyric_edit_enter_commits_single_note_and_clears`
- `lyric_edit_enter_distributes_morae_to_next_notes` (4 notes "あいうえ" → SetLyrics 4 件)
- `lyric_edit_enter_advances_to_next_when_more_notes_remain` (4 notes "あい" → 2 件 + lyric_editing=Some(notes[2].id))
- `lyric_edit_enter_combines_kana_correctly` ("しゅんかん" → 4 件)
- `lyric_edit_overflow_morae_count_in_response` (2 notes + "あいう" → SetLyrics 2 件 + Response.lyric_overflow_morae==1)
- `lyric_edit_esc_cancels_without_setlyrics`
- `lyric_edit_short_circuits_drag` (編集中 primary press on note → dragging==None)
- `lyric_edit_short_circuits_delete_shortcut`
- `lyric_edit_empty_string_normalized_to_none`
- `lyric_edit_disabled_via_style_none` (style.lyric_edit_shortcut=None で L key → no-op)
- `lyric_edit_auto_clears_when_target_note_deleted`
- `lyric_edit_same_beat_pitch_desc_order` (2 notes 同 start_beat、pitch 60/72 → 高 pitch 編集後 "あい" → SetLyrics(72→"あ", 60→"い"))

### Manual verify (IME flow、cargo test では再現困難)
- `cargo run --bin piano_roll`: 日本語 IME で「し」→「しゅん」変換 → Enter で commit → 続けて Enter で SetLyrics 発行確認

## daw_01 側で必要な変更 (受領後、daw_01 Claude / user に通知)

1. `AppEvent::SetNoteLyrics { clip_ref: ClipRef, lyrics: Vec<(u32, Option<String>)> }` 追加
2. `make_edit` の `NotesEditRequest::SetLyrics` 分岐追加
3. handler 実装 (既存 `set_selected_note_lyric` を参考、`Vec<(u32, Option<String>)>` を順次適用、undo snapshot は handler 1 回 = 1 snapshot)
4. `is_undoable` に `SetNoteLyrics` 追加
5. L キー bind: `host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L");`
6. (任意) 既存 `AppEvent::SetSelectedNoteLyric` (= 全 selected note 一括設定) は廃止 or 残置 (新 `SetNoteLyrics` で代替可)
7. (任意) `PianoRollResponse.lyric_editing` を読んで status bar 「[Lyric edit] L=L1, Enter=commit&next, Esc=cancel」表示
8. (任意) `PianoRollResponse.lyric_overflow_morae > 0` で toast 「{n} morae dropped」

## 想定 LOC と Phase 分割

| 箇所 | 追加 LOC |
|------|---------|
| `NotesEditRequest::SetLyrics` 追加 | 4 |
| `PianoRollState.lyric_editing` 追加 | 2 |
| `PianoRollResponse` 拡張 | 5 |
| `PianoRollStyle.lyric_edit_shortcut` 追加 | 3 |
| `split_into_morae` 公開 helper | 30 |
| `collect_next_notes_for_lyric` 内部 helper | 15 |
| L key 検知 + state 遷移 | 15 |
| text_input_at_focused 接続 + commit dispatch | 60 |
| 他 input 抑制 (drag/resize/wheel/click/shortcut の if-guard) | 10 |
| `TextInputResponse.committed_text` 追加 | 5 |
| Unit tests | 110 |
| Integration tests (14 件) | 350 |
| Doc 更新 (`docs/plan.md` 履歴 / piano_roll module doc / piano_roll example 更新) | 80 |
| **合計** | **約 690 LOC** (うち test 460、prod 230) |

### Phase: M14 Phase 59 として 1 commit
- 59a (split_into_morae + collect_next + unit test、110 LOC) と 59b (本体、580 LOC) に分けても良いが、daw_01 が gui_01 を git path dep で参照しているため separate publish の利点無し → **1 phase / 1 commit**
- piano_roll.rs は現 1100 LOC、+230 prod LOC で約 +20% (CLAUDE.md「巨大膨張」許容範囲)

## Critical Files to Modify

- `crates/ui/src/widgets/piano_roll.rs` (主要、+230 prod + 460 test LOC)
- `crates/ui/src/widgets/text_input.rs` (`TextInputResponse.committed_text` 追加 5 LOC)
- `crates/examples/piano_roll/src/main.rs` (L key bind 追加 + SetLyrics 分岐)
- `docs/plan.md` (M14 Phase 59 の進捗・DoD・履歴)
- `F:/dev/daw_01/docs/gui_01_conversation.md` (#017 `### gui_01 →` 返答 + `[Replied]` に変更)
- `crates/ui/src/widgets/mod.rs` (もし新 pub re-export が必要なら、現状では多分不要)

## Verification

1. `cargo build --workspace` で型エラー無し
2. `cargo test --workspace` で unit/integration 全合格 (新 14 + 12 個含む)
3. `cargo clippy --workspace --tests -- -D warnings` で warning 無し
4. `cargo run --bin piano_roll` で:
   - L キーを押したら note 内 text_input が出現 (selection 1 つのとき)
   - 日本語入力 (IME 経由 / IME OFF 直接) で文字が text_input に入る
   - "あいうえ" + Enter で 4 つの note に "あ"/"い"/"う"/"え" 反映
   - Esc で編集キャンセル (歌詞変わらず)
   - 編集中に note 上 click しても drag が始まらない
5. **commit 前 user 目視確認** (memory `feedback_visual_check_before_commit.md`):
   `cargo run --bin daw_prototype` で実機動作確認 (※ daw_01 ディレクトリは編集禁止 (memory `feedback_no_daw_01_edit.md`)、commit も禁止 (memory `feedback_no_daw_01_commit.md`)。daw_01 への path 依存先 break は conversation file 経由で通知のみ)

## daw_01 への返答内容 (`F:/dev/daw_01/docs/gui_01_conversation.md` #017 `### gui_01 →`)

返答 body の核となる内容:
- **結論**: Option C 採用、M14 Phase 59 で実装
- 6 質問への回答 (上表)
- 確定 API (`NotesEditRequest::SetLyrics` / `PianoRollResponse.lyric_editing/overflow_morae` / `PianoRollStyle.lyric_edit_shortcut` / `split_into_morae`)
- daw_01 側で必要な作業 (上記 1-5 必須、6-8 任意)
- L キー shortcut 名 = `"piano_roll.edit_lyric"` 確定
- ステータス: gui_01 側 Phase 59 着手時に再度 issue ベースで連絡。`[Open]` → `[Replied]` に更新

## Post-implement check

- `cargo run --bin piano_roll` → 実機 IME で歌詞入力できる
- daw_01 #017 を `[Replied]` に更新
- gui_01 / daw_01 双方 build 確認 (daw_01 は user / 別 Claude が follow-up commit するので gui_01 単独 build まで)
