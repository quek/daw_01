<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# FIXME #77 / #78 実装計画

理想とベストプラクティスを追求する。実装コストは無視して大胆に破壊して作り直す。

## 調査で確定した現状 (一次情報)

### 合成パスの SSoT (#77)
- 歌唱/読み上げ合成は **`daw_plugin_host` 内の `VoicevoxBuiltin`** が実行する。
  `MainToChild::SetBuiltinPluginNoteMetadata` → `set_note_metadata` → 内部 synth thread
  → `common::voicevox::synthesize_notes_for_builtin` / `synthesize_talk_for_builtin`
  (blocking HTTP) → `synth_result: Arc<ArcSwapOption<SynthResult>>` (メモリ) に格納。
- `state_save` は `VoicevoxState { speaker_id, style_name }` のみ。**合成 wav は保存しない**
  (`daw_plugin_host/src/builtin/voicevox.rs` L18-21 のコメント、意図的)。
- → プロジェクト再オープンで `set_note_metadata` が再 flush → **毎回フル再合成** (待ち時間)。
- **daw_gui 側の `VoiceVoxCache` と `common::voicevox::synthesize_song` は完全に dead code**
  (呼び出し元ゼロ、grep 確認)。旧 in-memory・プロセス寿命のみキャッシュ。

### 合成 wav はコンテンツアドレス可能
- 歌唱 wav は `build_sing_query(notes, bpm)` が作る query JSON + `singer_id` の純粋関数。
  query は base_beat 相対なので clip の絶対位置に依存しない。
- 読み上げ wav は `(text, talk_speaker_id, TalkParams)` の純粋関数。
- `note_offsets` は notes + sample_rate + bpm から決定的に再計算できる (キャッシュ不要)。

### #78 デバイススロットのボタン
- チェーン行の「GUI」ボタン → `ToggleSlotGui` → `toggle_slot_gui`。
  **既に映像 FX は `open_video_fx_params` インラインパネルに分岐**済み (`app.rs` L16687-16704)。
- `inspector_video_fx_params` + `set_video_fx_param` が **scrubable_number 行 + PluginParam lane の
  `default_value` を SSoT** とするイディオムを確立済み (`track_inspector.rs` L1253-1327)。
- ビルトインで「無意味」なのは VOICEVOX / Silence。VOICEVOX の実際の声は **per-clip**
  (`Clip::speaker_id` + `Clip::talk`、clip 選択時のみインスペクタに出る)。
- `PluginParamList` は activate 直後に**全プラグイン (builtin 含む) 無条件送出** (`main.rs` L1103)。
  daw_gui は `plugin_params: HashMap<(track,index), Vec<PluginParamInfo>>` にキャッシュ。
- 各 plugin backend は `gui_is_embed_supported()` を持つ (VOICEVOX/Silence=false)。

## #77 設計 — 永続コンテンツアドレスキャッシュ

理想 = 合成結果は (内容) の純粋関数なので、**内容ハッシュをキーに per-user global で永続化**する。

1. `common::voicevox_cache` を **disk cache** に作り直す (旧 in-memory 構造は破棄)。
   - `AppDirs` に `voicevox_cache_dir()` = `<root>/voicevox_cache/` を追加 (SSoT)。
   - キー: 安定ハッシュ。歌唱 = hash(query_json, singer_id)、読み上げ = hash(text, speaker, scales)。
     `DefaultHasher` (固定キー (0,0) でプロセス間安定。toolchain bump 時は miss=再合成1回で graceful)。
   - 値: VOICEVOX が返した **WAV bytes をそのまま** `<hex>.wav` に保存 (再エンコードなし、可聴で debug 可)。
   - 読み書きは temp ファイル + atomic rename (project.rs と同 idiom)。並行 builtin instance 安全。
   - GC: ディレクトリ総量が上限超で mtime 最古から削除 (bounded)。
2. `synthesize_notes_for_builtin` / `synthesize_talk_for_builtin` を **cache 経由**に:
   HTTP の前に disk lookup、hit で即返す。miss で HTTP → WAV を store → decode。
3. dead path 撤去: `synthesize_song`、daw_gui の `voicevox_cache` フィールド + 構築、`CachedClip`。
4. plugin host は `AppDirs::production()` を直接呼べる (`dirs::data_local_dir` は env ベース)。
   テストは `AppDirs::under(tempdir)` 注入で隔離。

## #78 設計 — GUI ボタン → パラメータ表示トグル (汎用 + VOICEVOX 声)

ユーザー選択 = **両方** (汎用 param 一覧 + VOICEVOX は声選択も)。

### ボタン分岐
- `PluginParamList` に `has_embedded_gui: bool` を追加 (plugin host が `gui_is_embed_supported()` で算出)。
  daw_gui は `slot_has_gui: HashMap<(track,index),bool>` にキャッシュ。
- `toggle_slot_gui`:
  - 映像 FX (`ports.is_video()`) → 既存 `open_video_fx_params` (不変)。
  - embedded GUI あり → 「GUI」ボタンで editor window を開く (不変、外部 CLAP/VST3)。
  - embedded GUI なし → `open_plugin_params` インラインパネルをトグル (VOICEVOX / no-GUI plugin)。
- チェーン行のボタンラベルも分岐: GUI あり=「GUI」、なし=「⚙」(パラメータ)、param も GUI も無い
  (Silence) = ボタン非表示。判定に `ChainEntry` へ `format` / `has_gui` / `has_params` を追加。

### 汎用パラメータパネル
- 新 `open_plugin_params: Option<(u32,u32)>` + `inspector_plugin_params()`。
  `plugin_params` の `PluginParamInfo` 列を scrubable_number 行で表示・編集。
  norm↔real は `min_value`/`max_value`、stepped は整数表示。
- 編集は `SetPluginParam { device_index, param_id, value_real }` → `set_plugin_param` で
  `AutomationTarget::PluginParam` lane の `default_value` (norm) を書く (= `set_video_fx_param` 一般化、
  音は daw_audio の automation 機構で反映)。undo は Begin/EndInspectorScrub で bracket。

### 専用セクションの Par 集約 (実機 feedback 2026-06-20 で option 2 に確定)
当初は「device 既定の声」を新設する案 (両方) だったが、字幕 X/Y・talk 話速・声は
**既にクリップ選択時に専用セクションで常時表示**されており、新パネルが冗長 (VOICEVOX)
/ 空振り (字幕) になった。ユーザー選択 = **「Par にまとめて専用欄を隠す」**。
- device 既定の声 (model `voicevox_*` フィールド / `SetDeviceVoice` / NoteMetadata 解決) は
  **撤去**。
- 「Par」ボタンを押した device の専用セクションだけを表示する gate を追加:
  - 字幕 (`SUBTITLE_ID`) Par → Text Event 欄 (X/Y/font...)。`subtitle_param_panel_open()`。
  - VOICEVOX Par → Clip Voice + Talk + 口パク出力先。`voicevox_param_panel_open()`。
  これらは従来「clip 選択で常時表示」だったのを **Par 開時のみ**に変更 (重複解消)。
- 字幕は video device だが video_fx def を持たないので、`toggle_slot_gui` の video 分岐から
  `SUBTITLE_ID` を除外し `open_plugin_params` 経路へ流す (Text Event を Par で描画)。
- `inspector_plugin_params()` は host param を持つ汎用 plugin のみ Some (VOICEVOX / 字幕 /
  Silence は None = 汎用パネルを出さない)。
- ヘルパ `open_param_panel_plugin_id()` が cursor track 上で Par が指す device の plugin_id を
  返し、`voicevox_param_panel_open` / `subtitle_param_panel_open` が判定する。

## 検証
- 自動: cache のユニットテスト (hit/miss/atomic/GC、`AppDirs::under(tempdir)`)、param norm↔real、
  声解決の単体。`cargo test --workspace` / `clippy -D warnings`。
- headless: 既存 `daw_gui --script` 経路があれば cache hit の往復を確認。
- 実機 sign-off (VOICEVOX engine 要、ユーザー): ① 再オープンで再合成が走らない (#77)、
  ② ビルトイン行の「⚙」でパネル開閉・声選択・汎用 param 調整が効く (#78)。
