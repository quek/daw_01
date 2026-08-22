<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Piano Roll: smart note length (FL Studio 互換)

## Context

ユーザー要望: ピアノロールでノートを新規追加するとき、長さを **直前にさわったノートと同じ** にする (FL Studio の "smart length" / "last note length" 挙動)。

現状の不具合点:
- ダブルクリック追加: 常に `DEFAULT_NOTE_DURATION = 0.25` 拍 (固定)
- Insert キー追加: gui_01 widget 側で `len_beats: 0.5` 拍が硬コード ([piano_roll.rs:1154](file://F:/dev/gui_01/crates/ui/src/widgets/piano_roll.rs))
- 直前のノートを resize して長さを変えても、次に追加するノートはまた 0.25 / 0.5 拍に戻る → ストレス

期待挙動 (FL Studio 互換): 直近に**作成 / 手動リサイズ / クリック選択**したノートの長さを覚えておき、次の新規追加で再利用する。移動 (drag-move) は長さを変えないので無関係 (move 開始時に発火する select で代理される)。session 内 in-memory 状態でよい (永続化不要)。

## 影響範囲

| ファイル | 変更点 |
|---|---|
| [daw_gui/src/app.rs](daw_gui/src/app.rs) | `AppData` に `last_note_duration_beats: f64` フィールド追加 / 初期化 / `add_note` と `resize_notes` と `SetNoteSelection` ハンドラで更新 |
| [daw_gui/src/view/piano_roll_view.rs](daw_gui/src/view/piano_roll_view.rs) | `AppEvent::AddNote` を発火する 2 箇所 (Add request handler / 空白 dbl-click) で `app.last_note_duration_beats` を渡す |

`common/` 側の `Note` 型・bincode protocol は変更不要。既存 song の保存形式にも影響なし。

## 実装

### 1) `AppData` への field 追加 — [app.rs:104-239](daw_gui/src/app.rs)

```rust
pub struct AppData {
    // ... 既存フィールド ...
    /// 直近に作成 or リサイズしたノートの長さ (拍)。次の新規追加のデフォルト長として使う。
    /// FL Studio の smart length 互換。session 内のみ保持し、永続化はしない。
    pub last_note_duration_beats: f64,
}
```

`AppData::new()` 等の初期化箇所で `last_note_duration_beats: DEFAULT_NOTE_DURATION` を設定。

### 2) `add_note()` の末尾で記録 — [app.rs:2102-2137](daw_gui/src/app.rs)

`clip.notes.push(Note { ... })` 直後に追記:

```rust
self.last_note_duration_beats = duration; // 既に max(0.0625) 適用済み
```

(`duration` は関数冒頭 line 2111 で `max(0.0625)` clamp 済みなのでそのまま使える)

### 3) `resize_notes()` の末尾で記録 — [app.rs:2160-2179](daw_gui/src/app.rs)

ループ後・`sync_song_to_plugin_host()` 前に追記:

```rust
if let Some(&(_, _, duration)) = entries.last() {
    self.last_note_duration_beats = duration.max(0.0625);
}
```

複数ノート同時 resize の場合は末尾要素を採用 (シングル resize なら自然、マルチ resize は仕様としても妥当)。

### 4) `SetNoteSelection` ハンドラで記録 — [app.rs:1007-1009](daw_gui/src/app.rs)

クリック選択 (= 末尾要素 = 最後にクリックされたノート) の長さを記録:

```rust
AppEvent::SetNoteSelection(targets) => {
    self.selected_notes = targets;
    // FL Studio smart length: 最後にクリックしたノートの長さを次の追加で使う。
    // 末尾は Shift+click 連続選択でも「最新クリック対象」を表す。
    if let Some(&last_idx) = self.selected_notes.last()
        && let Some(r) = self.selected_clip
        && let Some(note) = self
            .song
            .tracks
            .get(r.track as usize)
            .and_then(|t| t.clips.get(r.clip as usize))
            .and_then(|c| c.notes.get(last_idx as usize))
    {
        self.last_note_duration_beats = note.duration_beats.max(0.0625);
    }
}
```

空 Vec (deselect 全解除) の場合は `.last()` が `None` なので何もしない (= 直前の値を維持)。

### 5) view 側 2 箇所で `last_note_duration_beats` を使う

**(a) `NotesEditRequest::Add` ハンドラ** — [piano_roll_view.rs:107-119](daw_gui/src/view/piano_roll_view.rs)

widget が指定する `n.len_beats` (= Insert キー時 0.5 hardcoded) を無視し、ホスト側の値を使う:

```rust
NotesEditRequest::Add(notes) => {
    let Some(n) = notes.into_iter().next() else {
        return Edit::mutate(|_| {});
    };
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::AddNote {
            track: target.track,
            clip: target.clip,
            start_beat: n.start_beat,
            duration: app.last_note_duration_beats, // ← n.len_beats から変更
            pitch: n.pitch,
        });
    })
}
```

**(b) 空白上ダブルクリック** — [piano_roll_view.rs:185-193](daw_gui/src/view/piano_roll_view.rs)

```rust
ui.push_edit(Edit::mutate(move |app: &mut AppData| {
    app.handle_event(AppEvent::AddNote {
        track: target.track,
        clip: target.clip,
        start_beat: snapped_beat,
        duration: app.last_note_duration_beats, // ← DEFAULT_NOTE_DURATION から変更
        pitch,
    });
}));
```

`DEFAULT_NOTE_DURATION` 定数は AppData の初期化に残るので削除不要。

## 設計判断

- **「さわった」の定義**: add + resize + click-select の 3 種。FL Studio 挙動準拠 — クリックしただけのノートも長さソースになる。move (drag) は長さを変えないが、ドラッグ開始時に widget が select event を発火するので select 経由で自動的に対象になる。
- **永続化**: しない。session 内 in-memory のみ。プロジェクトロード時、`AppData` は再生成されないので値は持ち越されるが、新規プロジェクト直後は `DEFAULT_NOTE_DURATION` (= 0.25) で開始。これも FL Studio と同じ。
- **集中 vs 分散**: 「全 AddNote パスが smart length を使う」前提なので、`AppEvent::AddNote` の `duration` field を削って `add_note()` 内部で `self.last_note_duration_beats` を直接読む集中案も検討した。が、event を「ホストが伝えたい量」「ハンドラが解釈する量」に分けるのは不自然だし、AppEvent 形を変えると piano_roll_view 以外のテスト/呼び出し箇所にも波及する。**view 側 2 箇所を書き換える分散案を採用**。
- **gui_01 `len_beats` の改修要望**: 不要。daw_01 ホスト側で受け取った `n.len_beats` を上書きするだけで対応可能 — gui_01 conversation file への投げ込みは発生しない。

## 検証

1. `cargo clippy --workspace -- -D warnings` (型エラー / 未使用 warning 確認)
2. `cargo build -p daw_gui` (実行バイナリ生成 — clippy は exe 生成しないことに注意)
3. `cargo run -p daw_gui` で smoke test:
   - 新規プロジェクトでピアノロール開く → 空白ダブルクリック → ノート追加 → 長さ 0.25 拍 (デフォルト) を確認
   - そのノートを drag-resize で 1.0 拍に伸ばす
   - 別の空白位置をダブルクリック → 新規ノートが **1.0 拍**で生成されることを確認
   - Insert キーでもう 1 つ追加 → 同じく 1.0 拍 を確認 (gui_01 の hardcoded 0.5 を上書きできていることを確認)
   - **その 1.0 拍ノートを drag-resize で 2.0 拍に伸ばす → 別の短いノート (例: 0.5 拍) をクリックして select → 空白ダブルクリック → 新規ノートが 0.5 拍で生成されること** (click-select で smart length が更新される FL Studio 挙動)
   - Drag-move (位置移動) → ドラッグ前の click で select が発火するので、対象ノートの長さが新規追加に反映されること
4. 既存ピアノロール挙動 (resize / move / delete / undo) に regression が無いことを確認
