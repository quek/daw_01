# FIXME #87: ズーム / スクロール位置をプロジェクトに保存

## 要望

「ズームやスクロール位置もプロジェクト情報として保存できますか。」
= プロジェクトを開き直したとき、閉じたときの「見た目 (表示状態)」が復元される。

## ユーザー確定事項 (2026-06-27 AskUserQuestion)

1. **復元範囲** = ズーム/位置 + 表示設定一式。プラグイン画面の再オープンは **含まない**。
   - 各画面のズーム・スクロール・トラック行高・ヘッダ幅
   - スナップ単位 (arrange / piano roll)、piano roll の fold / snap-on-draw / snap-live-input
   - 下部パネルのタブ、オートメーション展開状態 (track / master)
2. **ピアノロール / オーディオエディタ** = **per-clip 記憶** (Ableton Live / Bitwig 流)。
   クリップごとに最後に見ていたズーム/位置を覚え、開き直すとそのクリップが前回の範囲で開く。
   アレンジビューはタイムライン 1 つなので **グローバル**。
3. **ダーティ (`*`)** = ズーム/スクロール変更だけでは **立てない** (閲覧操作)。
   次の保存 / 自動保存 (autosave) で同梱される。
4. **置き場所** = `ProjectFile.view: Option<ViewState>` (Song は汚さない)。

## SSoT 設計

- **Song は楽曲ドメインの SSoT**。view 状態は GUI の見え方であり audio engine は使わない。
  Song に混ぜると IPC (bincode) を毎回渡って 3 プロセス rebuild が要る
  ([[feedback_workspace_build_for_protocol_changes]] の事故圏)。
- view 状態は `ProjectFile` 直下の **兄弟フィールド** に置く (= IPC レイアウト不変)。
  `ViewState` は **serde 専用** (bincode derive 無し) = 「IPC を渡らない」の型レベル保証。
- live SSoT は `AppData`。save 時に `AppData → ViewState` を snapshot、load 時に流し込む
  (`loop_*_beat` と同じ往復構造)。

### per-clip live モデル (piano roll / audio editor)

- `AppData.piano_roll_views: HashMap<ClipKey, PianoRollViewState>` が live SSoT。
  フラットな `pianoroll_zoom_x` 等は **撤去**し、`selected_clip` (= ClipKey) で map を引く
  accessor (`pianoroll_zoom_x()` 等) に置換 (= 重複所有を作らない)。
- `AppData.audio_editor_views: HashMap<ClipKey, AudioEditorViewState>` 同様、`audio_editor_clip` で引く。
- **first-open**: select / open でそのクリップに entry が無ければ従来どおり fit (= 範囲に合わせる)。
  entry があれば fit せず、draw が map の値を読んで復元。
- **fit (X キー / Fit ボタン)** は常に再 fit して entry を上書き (明示操作)。

### 互換性

- ディスクは JSON。`ProjectFile.view` は `#[serde(default)]` で旧ファイルは `None` → 復元せず従来挙動
  (fit-to-content / 既定値)。Song の bincode レイアウト不変 → IPC / 3 プロセス影響ゼロ。
- `ClipKey` は struct なので JSON の map key にできない → ViewState では
  `Vec<(ClipKey, *ViewState)>` で保持。
- `CURRENT_VERSION` は 27 → 28 へ (メタデータ目的、互換は serde(default) が担保)。

## 実装 (最終形)

### common
- `model.rs`: `PianoRollViewState { zoom_x, zoom_y, top_pitch, scroll_beat }` (Default = 64/14/84/0)、
  `AudioEditorViewState { start_beat, len_beats }`、`ViewState { ...globals..., piano_roll_views, audio_editor_views }`。
  `ProjectFile` に `#[serde(default)] view: Option<ViewState>`。
- `project.rs`: `save_project(path, &Song, Option<&ViewState>)` / `load_project(path) -> LoadedProject{song, view}`。
  既存 `save` / `load` は view=None / `.song` で **委譲** (テスト・headless は無改修)。

### daw_gui
- `app.rs`: フィールド置換 (6 撤去 → 2 map)、accessor + `snapshot_view_state` / `restore_view_state`
  (restore は範囲 clamp + orphan GC)、handler / fit / select_clip / set_clip_selection /
  open_audio_editor / close / scroll / zoom を per-clip 化、`action_open_path` / `restore_recovery` で
  `load_project` + restore、`finish_save` / autosave で `save_project` + snapshot。
- `view/{piano_roll_view,audio_editor,root}.rs`: 直接フィールド読みを accessor 呼び出しへ。

## テスト
- `common/project.rs`: ViewState round-trip (save_project → load_project)、legacy ファイル → view=None。
- `daw_gui/tests/view_state.rs`: clip A で zoom 変更 → clip B は既定 → clip A 再選択で復元 (per-clip 隔離)、
  `snapshot_view_state` ⇄ `restore_view_state` 往復。
- 実機: 保存 → 開き直しで各画面のズーム/位置が戻る。`*` がスクロールだけでは出ない。
