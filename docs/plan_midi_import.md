<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# MIDI (SMF) ファイル取り込み — 設計正本

r.md #66「MIDI ファイル D&D ができません」。`daw_gui/src/midi_export.rs` (SMF Format 1 書き出し) の
**逆写像**として import を実装する。導線は arrangement へのドラッグ&ドロップと File メニューの 2 つ。

## 1. 現状 (なぜ動かないか)

**2 つの独立した原因**があった。

### 1.1 MIDI import が存在しない

`view/arrangement_view.rs:776-786` の drop partition は video → image → **audio が残り全部の
fallback**。`.mid` は audio バケツに落ち、`common::audio_decode` が `UnsupportedFormat` を返して
「Audio import 失敗」で終わる。MIDI import のコードは存在しない (`ImportMidi` イベントも無い)。

### 1.2 Windows では D&D の位置が常に壊れていた (audio / 画像 / 動画も同じ)

winit の `WindowEvent::DroppedFile` / `HoveredFile` は**座標を持たない**ため、daw-ui の
`InputAccumulator` は「直近の cursor 位置」(`PointerMoved` 由来) で代用していた
(`ui/crates/ui/src/input.rs` の `take_input`)。ところが Windows のドラッグ中は OLE の
ドラッグループがマウスを握るので `CursorMoved` が**一度も来ない** — winit の Windows
実装がそうなっている:

```rust
// winit-0.30.13/src/platform_impl/windows/drop_handler.rs:106-118
pub unsafe extern "system" fn DragOver(
    this: *mut IDropTarget, _grfKeyState: u32, _pt: *const POINTL, pdwEffect: *mut u32,
) -> HRESULT { /* _pt を捨て、イベントを一切出さない */ }
```

結果、drop 位置は「ドラッグを始める前に最後にマウスを動かした場所」になる。起動直後で窓に
カーソルが入っていなければ `None` → `(0, 0)` に落ち、`Ui::take_file_drop_in_rect(canvas)` の
`rect.contains(drop_pos)` が false になって **drop が無言で捨てられる** (= 何も起きない)。

**修正**: `HoveredFile` / `DroppedFile` の直前に OS へ実カーソル位置を問い合わせ、synthetic
`PointerMoved` を流して同期する (`query_cursor_pos_in_window` = `GetCursorPos` +
`ScreenToClient`。フォーカス取得を伴うクリックで既に使われている手当てと同じ)。daw_gui の
`view/runner.rs` と上流 `ui/crates/platform/src/winit_backend.rs` の両方に入れる。
取得できないプラットフォームでは no-op (従来どおり直近 cur_pos)。

## 2. 参照 DAW の挙動 (一次情報)

| DAW | SMF track の展開 | テンポ | 配置位置 |
|---|---|---|---|
| REAPER 7 User Guide p.91 / p.296 / p.445 | drop 時にダイアログ (Expand to new tracks / multichannel 1 track)。設定で既定化 | 「Import MIDI tempo map」を訊く | drop 位置 |
| Ardour (manual + `libs/ardour/import.cc`, `gtk2_ardour/editor_canvas.cc`) | Type 1 = SMF track 数だけ、Type 0 = channel 分割 (空 track は破棄) | **D&D は常に `SMFTempoIgnore`**。tempo map は Import ダイアログの明示チェックのみ | 「placed at the position where the drag ended」(snap 適用) |
| Cubase (Preferences > MIDI > MIDI File) | 「Auto Dissolve Format 0」で channel 分割、「Import Dropped File as Single Part」で 1 track 化 | 「Ignore Master Track Events on Merge」で既存プロジェクトへの取り込み時は無視できる | drop 位置 (ダイアログは出さず Import Options に従う) |
| Logic Pro (Help: Import MIDI files) | 「automatically creates software instrument tracks for each MIDI track」 | — | ポインタ位置 (小節に丸め)、1 本目の行き先も決まる |
| Studio One 6 Reference §7.1.5 | 空きへ drop = 新規 Instrument Track、既存 MIDI track へ drop = その track に Part | Tempo Track へ直接 drop でテンポだけ取り込み | drop 位置 |
| Ableton Live 12 §5.2 | — | — | Insert Marker (menu import) |

共通項: **ソング先頭 0 固定にする DAW は無い** / SMF track 単位で daw track を作るのが多数派 /
テンポ取り込みは「訊く」か「D&D では取り込まない」。

## 3. 決定事項

### 3.1 導線

- arrangement view への D&D。拡張子 `mid` / `midi` / `smf` / `kar` / `rmi` (RMID は midly が
  RIFF を剥がして読む: `midly-0.5.3/src/smf.rs:262-267`)。partition は **MIDI → video → image →
  audio** の順 (audio は fallback バケツなので MIDI をその前に置く)。
- File メニュー「Import MIDI...」(Export MIDI と対称)。`spawn_file_dialog` 経由 (同期 dialog 禁止)。

### 3.2 SMF track / channel → daw_01 track

**1 SMF track = daw_01 track 1 本**（export の逆写像）。ただし 1 つの SMF track に複数 MIDI channel
が混在する場合は **channel ごとに分割**する。理由: `common::model::Note` に channel フィールドが無く
(`common/src/model/content.rs:1245-1265`)、再生も channel 0 固定 (`daw_audio/src/graph/execute.rs:316`)
なので、分割しないと別楽器のノートが 1 つの content に混ざって復元不能になる。Type 0 (全 channel が
1 track) も同じ規則で自動的に channel 分割される (Ardour と同じ挙動)。ノートが 1 つも無い SMF track
(= tempo/marker 専用 track) は捨てる。

Format 2 (sequentially independent patterns) は各パターンを**時間軸に連結**して 1 track に落とす
(パターンは同時に鳴らす物ではないため)。連結境界は小節に切り上げる。

### 3.3 配置

- **クリップ先頭 = ドロップ位置**を原点として SMF の tick 0 を写す。`ImportTrackTarget::Track(i)` が
  有効ならその track に 1 本目、2 本目以降は**その直下**へ順に挿入 (Logic 流。`parent_group_id` は
  アンカー track から継承 = `action_add_instrument_track` と同じ階層規則)。それ以外
  (`NewTrackBottom` / `NoHint` / 範囲外 index) は全部**一番下に追加** (r.md #31 の統一規則)。
- 各 track 1 クリップ。**clip は content の窓**として作る (`Clip::content_offset_beats`, v32/r.md #44):
  - `content_offset_beats = floor_to_bar(最初のノート)`
  - `start_beat = drop 拍 + content_offset_beats`
  - `length_beats = ceil_to_bar(最後のノート終端) - content_offset_beats`
  - → content-local 拍は **SMF tick 0 起点のまま**保たれ、クリップは音が始まる小節から始まる。
    複数 track 間の相対位置も保たれる (`content_origin_beat()` は全 track で drop 拍に一致)。
- 複数ファイルを同時に drop したら、全ファイルが同じ drop 拍を原点に、track は下へ積む。

### 3.4 テンポ / 拍子 (ユーザー確定: 「空の曲のときだけ合わせる」)

**曲にクリップが 1 つも無いとき**(track の clips / automation clips / song_lanes clips が全部空)
だけ、SMF のテンポと拍子を取り込む:

- 先頭 Tempo meta → `Song.bpm`。Tempo meta が 2 個以上 (値が変わる) なら
  `AutomationTarget::SongTempo` lane を `Song.song_lanes` に作り、各 breakpoint を
  `AutomationCurve::Hold` (= 階段) の point として置く。これは export 側 (`midi_export.rs:105-131`
  の階段近似) の逆写像。
  - **automation clip は最後の breakpoint を厳密に内側に含む長さにする**。clip の範囲は
    半開区間 `[start, start+length)` (`common/src/automation.rs` の `clip_covering`) なので、
    clip 長を「最後の breakpoint の拍」ぴったりにすると最後のテンポ変化が評価されず、しかも
    clip 外は `lane.default_value` に戻るため、テンポ変化 2 点の普通のファイルでテンポマップが
    丸ごと無効になる。clip 終端 = `max(取り込み素材の終端, 最後の breakpoint + 1 小節)`。
  - `lane.default_value` (= clip の外で使う値) は新規作成でも既存 lane 再利用でも曲頭 BPM に
    揃える。
- 先頭 TimeSignature meta → `Song.time_sig` (SMF の denominator は log2 なので `1 << d`)。
  曲中の拍子変化はモデルが表現できない (分母の automation target が無く、小節線グリッドも静的
  `song.time_sig` しか見ない: `common/src/timing.rs:113`) ので**先頭 1 個だけ**採用する。

クリップがある曲では BPM を一切触らない。テンポを変えると既存のオーディオ / 動画クリップの
実時間位置が全部ずれるため (Ardour の D&D と同じ判断)。

### 3.5 ノートの解釈

- **velocity 0 の NoteOn は NoteOff** として扱う (midly は変換しない: `src/event.rs:243-251`)。
- 同一 (channel, key) の NoteOn/NoteOff 対応は **FIFO** (Ardour `Sequence.cc:479` の
  `FirstOnFirstOff` と同じ。SMF 仕様は規定していない)。
- **NoteOff が来ないノート**は track 末尾 (EndOfTrack か最終イベント tick) まで伸ばす
  (Ardour の ResolveStuckNotes)。長さが 0 になる場合は 1 tick 分を最低長とする。
- 対応する NoteOn の無い孤立 NoteOff は無視する。
- 同一ピッチの重なりは daw_01 のモデル不変条件 (`app_types.rs:1983` `resolve_note_overlaps`) で
  解消する。全ノートを winner にして「後から始まる方が勝ち、前のノート末尾をトリム」。

### 3.6 歌詞 (VOICEVOX 歌唱に直結)

Lyric meta (FF 05) を `Note.lyric` に取り込む。`@` で始まる行 (KAR のメタ行) と空文字は捨て、
行区切りの `/` `\` は先頭から剥がす。

- **Text meta (FF 01) は「ファイルが .kar だと自称しているとき」だけ歌詞に昇格させる**
  (= `@KMIDI KARAOKE FILE` / `@L` / `@T` 等の `@` 行がどこかにある)。Text meta は著作権表示・
  制作者名・区間名にも普通に使われ、無条件に歌詞にすると conductor track の "Produced by ..." が
  先頭ノートの歌詞になって VOICEVOX がそれを歌う。
- 紐付けは「歌詞 tick に最も近い、まだ歌詞の付いていないノート」へ順に。固定 ±1 tick で
  「歌詞より前のノート」を飛ばす方式だと、歌詞 meta が音の頭より数 tick 後ろに置かれた
  ファイル (打ち込み由来では普通) で全歌詞が 1 音ずれ、末尾が落ちる。
- 付け先の **channel** は「歌詞 tick に音の頭が (1/32 拍の許容内で) 合う数」が最多のものを選ぶ。
  Type 0 の .kar はメロディとドラムが同じ tick に並ぶので、選ばずに配るとドラムに乗る。
  同点なら距離合計が小さい方 → 非ドラム (ch10) → 番号の小さい方。
- ノートを持たない歌詞専用 track (.kar の「Words」track) の歌詞は、同じスコアで選んだ
  **歌のパートらしい** note track へ回す (SMF の並び順で先頭を取るとドラム track に乗る)。

文字コードは SMF 仕様が規定していない (「printable ASCII ... other character codes using the
high-order bit may be used」) ので **UTF-8 を試し、失敗したら Shift-JIS** (日本語 .mid/.kar の実情)。

### 3.7 取り込まないもの

CC / PitchBend / ProgramChange / Aftertouch / SysEx はモデルに受け皿が無いので破棄し、**件数を
ステータスバーに出す** (黙って捨てない)。Marker / KeySignature も現状は取り込まない。

### 3.8 タイミング

- PPQ は SMF ヘッダの値を使う (`Timing::Metrical`)。480 決め打ちにしない。
- SMPTE タイム (`Timing::Timecode`、division 負値) も受ける。tick → 秒 (fps × subframe) →
  取り込み先プロジェクトの `TempoMap::seconds_to_beat` で拍へ。
  (Ardour/libsmf は SMPTE を拒否するが、拒否は妥協なので変換して受ける。)
  - **テンポを採用する場合は、採用後のテンポで解き直す**。換算に使った BPM と再生 BPM が
    食い違うと、SMPTE 経路が守ろうとした実時間位置がそのままずれる。
  - SMPTE は絶対時刻が正本で tempo meta は再生タイミングの正本ではないので、**テンポ
    カーブ (SongTempo lane) は作らない**。曲頭 BPM だけ採用する。

### 3.9 その他

- 新規 track に **音源 (instrument) は自動挿入しない**。daw_01 は「役割判定しない / port 直結」で
  既定音源を持たない (`common/src/model/track.rs:20-30`)。取り込み直後は無音なのでステータスで案内する。
- track 名 = SMF の TrackName meta、無ければファイル名 (stem)。channel 分割時は `名前 ch2` の形式。
- clip 名 (`Song.clip_content_names`) は **歌詞が無いときだけ** SMF track 名を入れる。歌詞があると
  明示名がクリップ表示で歌詞を隠すため (`widgets/arrangement/view_build.rs:459-470` の表示優先順位、
  MIDI 録音が無名で作っているのと同じ理由: `handler/midi.rs:463-467`)。
- 取り込みが `Song.length_beats` を超えたら伸ばす (伸ばさないと「全曲」書き出しが既定 64 拍で切れる:
  `handler/export.rs:127`)。縮めることはしない。
- 1 回の import = 1 undo ステップ (`handle_event` の gesture squash)。dirty は `edit_song` が立てる。
- 同一ピッチの重なり解消は **bulk 用の線形パス** (`truncate_same_pitch_overlaps`) で行う。編集操作向けの
  一般形 `resolve_note_overlaps` は pitch ごと総当たりで O(n²)、数万ノートで GUI が数分止まる。
  規則 (後勝ち・前をトリム・同開始は前を削除) は同じ。
- 防御 (取り込んだ物はすべて Song に載り、undo snapshot の clone と `LoadSong` を通る。
  16MB wire 上限は「大きくして解決」しない)。**サイズ検査はファイルを読む前**に行う:
  - ファイル 64 MB / ノート 100,000 / tempo breakpoint 20,000 / 歌詞 100,000
  - meta テキスト 1 個あたり 512 バイト (track 名 / 歌詞がそのまま Song に載る)
  - 取り込む拍範囲 100,000 拍 (SMF の delta は 1 個で最大 0x0FFF_FFFF tick 進むので、
    壊れたファイルが `Song.length_beats` を数億拍に伸ばして `TempoMap` 構築を破綻させるのを防ぐ)

## 4. 実装

| ファイル | 内容 |
|---|---|
| `daw_gui/src/midi_import.rs` (新規) | `looks_like_midi` / `parse_midi_bytes` (SMF → `ParsedMidi`) + 単体テスト |
| `daw_gui/src/handler/media.rs` | `action_import_midi` / `action_open_import_midi_dialog` |
| `daw_gui/src/event.rs` | `AppEvent::ImportMidi` / `OpenImportMidiDialog` + undo label |
| `daw_gui/src/app.rs` | dispatch arm |
| `daw_gui/src/app_types.rs` | `FileDialogKind::ImportMidi`、`resolve_image_drop_target` → `resolve_media_drop_target` に改名 (audio/image/midi 共通) |
| `daw_gui/src/handler/export.rs` | dialog result arm |
| `daw_gui/src/view/root.rs` | File メニュー項目 |
| `daw_gui/src/view/arrangement_view.rs` | drop partition に MIDI を追加 (audio より前) |
| `Cargo.toml` / `daw_gui/Cargo.toml` | `encoding_rs` (Shift-JIS 歌詞のデコード) |
| `daw_gui/src/view/runner.rs` | §1.2 の drop 位置修正 (`sync_pointer_from_os`) + drop の 1 行ログ |
| `ui/crates/platform/src/winit_backend.rs` | §1.2 の同修正 (上流の正準実装にも入れる) |
| `daw_gui/tests/import_drop_midi.rs` (新規) | `handle_event(ImportMidi)` レベルの統合テスト |

`midly = "0.5"` は既に workspace 依存 (`Cargo.toml:72`) で `midi_export.rs` が使用中。
