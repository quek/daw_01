# plan: IME composition 確定 Enter を text_input commit から隔離する

## 主訴 (ユーザー報告 2026-06-01)

クリップのリネーム中、IME (rtry) の **まぜ書き変換を Enter で確定** すると、その Enter が
**リネーム自体の確定** (= text_input commit → 編集終了) として食われてしまう。

つまり「変換を確定して、まだ文字を打ち続けたい / 微修正したい」のに、1 回目の Enter で
rename editor が閉じる。期待挙動は「変換確定の Enter は IME に消費され、rename editor は
開いたまま。rename を確定したいときに改めて Enter を押す」。

これは Web のテキストフィールド・Win32 標準 edit control・各 DAW の名前編集すべてに共通の
標準挙動 (IME を確定する Enter は submit に波及しない)。

## 根本原因 (調査済み)

対象 widget は gui_01 の共有 `text_input` (`crates/ui/src/widgets/text_input.rs`)。
daw_01 の clip rename / track rename はこの `text_input_at_focused` を使う
(`daw_gui/src/view/arrangement_view.rs:501-515` clip / `:676-688` track)。

イベントフロー (daw_01 = winit IMM 経路、TSF text store は未配線):

1. ユーザーがまぜ書きを Enter で確定 → Windows IME が `WM_IME_COMPOSITION` (GCS_RESULTSTR)
   を生成 → winit `Ime::Commit(text)` → daw_01 runner が `PlatformEvent::ImeCommit`
   → `ImeEvent::Commit` に変換 (`runner.rs:752-754`)。
2. **同じ Enter キーストロークが `WM_KEYDOWN` としても配送され**、winit が
   `KeyboardInput { physical_key: Enter }` を出す → `PlatformEvent::Keyboard`
   → `KeyEvent` に変換 (`runner.rs:757-766`)。physical_key はハードウェア scancode 由来
   なので、vkey が VK_PROCESSKEY (IME 処理中) でも Enter の scancode で来る。
3. `text_input` は focus 中、`ImeEvent::Commit` を先に処理して preedit をクリア + 確定文字を
   挿入 (`text_input.rs:252-263`)。続いて key_events ループで Enter を見て
   **無条件に `committed = true`** (`text_input.rs:343-345`)。
4. `committed` を見た daw_01 が `CommitRenameClip` を発行 → rename editor が閉じる。

= **`text_input` が「IME composition を確定する Enter」と「ユーザーが submit する Enter」を
一切区別していない** のが欠陥。Enter → committed は IME 状態を全く参照しない。

### frame batching の補足

daw_01 runner は各 WindowEvent を `InputAccumulator::ingest` して `request_redraw()` するだけ
(`runner.rs:582-601`)。Windows の WM_PAINT は最低優先度なので、キューに溜まった
`Ime::Commit` と `KeyboardInput{Enter}` は **同一 frame の `take_input()` にまとめて** 入る
公算が高い。よって widget 視点では「同 frame で `ImeEvent::Commit` を処理した直後に
Enter key_event が来る」。ただし連続再描画 (playback / video preview) 中は別 frame に
割れる可能性もあるため、機構は **frame 跨ぎでも堅牢** であることが望ましい。

## 望む挙動 (最終形態)

`text_input` (および `text_input_at_focused`) が、**IME composition を確定 / 操作している
Enter を `TextInputResponse.committed` に昇格させない**。具体的に:

- 次のいずれかが成り立つ frame の Enter / NumpadEnter は IME 確定とみなし、
  `committed` を立てない (= rename / submit に波及しない):
  - その frame の入力処理開始時点で `state.preedit` が非空 (composition 進行中)、または
  - その frame で `ImeEvent` (Preedit / Commit / ReplaceRange / SetSelection) を 1 つ以上処理した、
    または
  - 直前 frame で composition が active だった (frame 跨ぎ guard。`state` に 1 frame 分の
    bool を持たせる)。
- composition が全く絡まない素の Enter は従来どおり `committed = true` (回帰なし)。
  既存テスト `commit_still_fires_on_main_enter` / `commit_fires_on_numpad_enter` は維持。

### Escape の対称ケース (副次・任意)

同様に「composition を Esc でキャンセルした Esc」が rename cancel (`text_input.rs:346-348`
の `escape_pressed` → 自己 blur) に波及しないのも理想。ただし今回の主訴は Enter なので、
Escape まで含めるかは gui_01 判断に委ねる。

## gui_01 側 source の当たり

- `crates/ui/src/widgets/text_input.rs`
  - `state.preedit` (`:40`)
  - ime_events ループ (`:242-291`) — ここで「この frame に IME activity あり」を記録できる
  - key_events ループの Enter 分岐 (`:343-345`) — ここで guard する
  - `TextInputState` (`:29-64`) — frame 跨ぎ bool の置き場
- 機構案: ime ループ前に `let preedit_was_active = !state.preedit.is_empty();` を取り、
  ime ループ内で `ime_activity = true` を立て、Enter 分岐で
  `if !(preedit_was_active || ime_activity || state.composing_last_frame) { committed = true; }`。
  frame 末で `state.composing_last_frame = !state.preedit.is_empty() || ime_activity;`。
  最終的な API / 機構は gui_01 にお任せ。

## 影響 / SSoT

- 修正は widget 内で完結 (daw_01 側は無修正で恩恵を受ける、`committed` の意味が
  「ユーザー submit」に純化されるだけ)。
- TSF 経路 (gui_01 example) でも、IMM 経路 (daw_01) でも、どの IME でも一貫して正しくなる。
