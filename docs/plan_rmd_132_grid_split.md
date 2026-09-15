# r.md #132 Shift+E でグリッド単位に分割

索引: [plan_rmd_130_133_index.md](plan_rmd_130_133_index.md) (分担・統合順・共通規則)。
調査: `scratchpad/rep/132-split-notes.md` (コード地図) / `r132.md` (Bitwig / Live / FL / Cubase / Studio One / REAPER / Logic)。

## 理想

「E = カーソル位置で切る / **Shift+E = グリッド線ごとに切る**」をピアノロール・アレンジ・オーディオエディタの**全画面でそろえる**。
グリッド分割は既存の分割と**同じ 1 本の分割関数**を「切る位置の集合」で呼ぶ形にし、切る位置はグリッドの SSoT
(`common::snap::SnapConfig`) から作る。分割片は元の属性を継ぎ、先頭片が元の安定 id を持ち、1 操作 = 1 undo。
既存の E が抱える同じ根の欠陥 (undo の口を通らない / 後半片の歌詞 / 分割実装と下限長定数の二重化) も同時に直す。

## 確定仕様 (2026-09-15 ユーザー承認)

| # | 論点 | 決定 |
|---|---|---|
| Q1 | 分割するノート | **E と同じ規則**: ポインタ下のノートが選択外ならその 1 音 / 選択があれば選択全体 / どちらも無ければ表示中の全ノート (`handler/notes.rs:880-895`) |
| Q2 | 切る位置 | **画面のグリッド線 (曲の拍 0 を原点とする絶対グリッド)** で切る。最短ノート長 (`MIN_NOTE_LEN_BEATS` = 1/16 拍) より短い片はできないようにし、隣の片にくっつける |
| Q3 | 歌詞の付いたノート | 先頭片は元の歌詞、**後ろの片は「ー」** (音節を歌い直さず伸ばす)。歌詞の無いノートは無いまま。**既存の E (ピアノロール) とクリップ分割の跨ぎノート (`content_split.rs:114`) も同じ扱いに直す** (今は `None` = 実際には「ら」と歌われる `common/src/voicevox.rs:298`) |
| Q4 | アレンジ / オーディオエディタ | **E と同じ対象に効く**。アレンジではクリップを、オーディオエディタではオーディオイベントを、それぞれの画面のグリッド線で分割 |

### main が決めた細部 (ユーザーに報告済み)
- **グリッドの単位** = スナップ単位 (`SnapConfig::beat_unit`)。スナップ OFF のときは選んでいる分割値を `enabled: true` にして求める
  (ナッジの前例 `handler/note_nudge.rs:27-33`)。Adaptive は現在のズームの単位。ピアノロールは `piano_roll_snap_config`、
  アレンジ / オーディオエディタは `arrange_snap_config` (それぞれの画面の設定)。
- **分割後の選択**: 分割片はすべて選択されたまま (E と同じ)。
- **リンクしたクリップ**: 各画面の E と同じ扱い (ノート編集はその場、クリップ分割は `split_content_at` の fork — MIDI だけ。 下の「残件の続き」)。
- velocity / muted は全片が継ぐ。重なり解消は不要 (分割は新しい同音程の重なりを作らない、`plan_fixme_83_note_overlap.md:68-70`)。
- ノート単位の変調 (ADSR / retrigger=Note) は分割点で再トリガされる — 新しい note-on なので正しい挙動。

## 設計

### 分割の SSoT
- ノート分割は今 `handler/notes.rs:911-935` と `common/src/model/content_split.rs:103-121` の **2 実装**、下限長は
  `content.rs:27 MIN_NOTE_LEN_BEATS` と `handler/notes.rs:12 NOTE_MIN_LEN_BEATS` の **2 定数**。1 本 / 1 つにする。
- 「切る位置の集合 (クリップ内の拍、昇順)」を受けてノートを多片に割る関数を common に置き、E (1 点) と Shift+E (多点) が共用する。
  下限長の吸収 (短い片を隣へ) もこの関数が持つ。
- グリッド線の列挙: 曲の拍でグリッド線を出し、`Clip::content_origin_beat` でクリップ内の拍へ換算 (ランチャーのセルは start 0)。

### ショートカット
- `shortcuts.rs` の `SHORTCUTS` に Shift+E (例: `daw.split_at_grid`、category ClipNote) を追加。重複キー禁止テストを通す。
  仮想鍵盤を開いている間は Shift+E も key grab に取られる (E と同じ、`key_grab.rs:68-78`) — 仕様どおり。
- **`root.rs::dispatch_shortcuts` は 519 / 530 (arch-lint baseline の天井)**。E / Alt+E / J / Shift+E の振り分けは root.rs に分岐を足さず、
  別関数 (別ファイル可) へ出して root.rs からは 1 呼び出しにする。

### undo の口
- ピアノロールの E / J は `Edit::mutate` から `action_*` を直接呼び、`handle_event` を通っていない (`root.rs:1087-1089, 1101-1104`)。
  `begin_event` が呼ばれず undo ラベルと scope が直前イベントのまま。**Shift+E / E / J を AppEvent + `undo_label` にそろえる**。
- `AppData::handle_event` (1531 / 1605) の arm は 1 行で handler へ委譲。

### 同じ根として調べて直すもの
- 同じ content の linked clip を 2 つ同時に表示して両方に鍵盤行が掛かると、分割 (非冪等) が同じ content に 2 回走る疑い
  (`note_selection.rs:144-157`、推測)。実際に起きるか確かめ、起きるなら content 単位で 1 回にする。
- `sing_note_id` は `note.id % MAX_NOTES_PER_CLIP (16384)` で畳む (`plugin_metadata.rs:146,162-165`)。グリッド分割は id を大量に消費し
  `next_note_id` は単調増加なので、1 content の累積採番が 16384 を超えると**生きているノート同士の note_id 衝突**が起きうる。
  実際に衝突する条件を確かめ、起きるなら衝突しない導出に直す (上限を上げて先送りしない)。

## テスト (高いレイヤーで。本番の算術を写すだけのテストは書かない)
- ピアノロール: 1/4 グリッドで 1.5〜3.5 拍のノート → 1.5-2 / 2-3 / 3-3.5 の 3 片、先頭が元 id、歌詞「あ」→「あ / ー / ー」、1 undo で戻る。
- 下限長: グリッド線がノート端から 1/32 拍の位置 → 短い片ができない。
- E の回帰: 後半片の歌詞が「ー」、undo が 1 step。
- アレンジ: クリップが arrange グリッドで割れる / オーディオエディタ: イベントが割れる。
- テストの足場: `daw_gui/tests/note_nudge_lock.rs` (起動しない AppData 直組み)、root.rs の `dispatch_char_key` ヘルパ。
  `split_glue_smoke.rs` / `note_overlap_smoke.rs` は **daw_gui を起動する**ので回さない。

## 完了条件
索引の共通規則どおり (全件系を回さない、daw_gui を起動しない、branch に commit、逸脱を報告)。

## 残件: 分割の忠実度 (v42)

理想: **分割は切れ目を入れるだけ** — 分割直後の再生・書き出し・字幕・映像・読み上げは分割前と同じ。 片を後から
動かしても片ごとに自然に振る舞う。 模型の正本は各フィールドの doc (ここは地図だけ)。

| 欠陥 | 根 | 直し方 (正本) |
|---|---|---|
| Text を割ると切り口ごとに同じ文を読み直す | 続きの片を表現できない + 読み上げの一覧が clip の窓を見ない | `TextEvent::continuation` (ユーザー決定: 読むのは最初の片だけ) と `TextEvent::starts_reading`、窓の門 `Clip::window_has_onset` を sequencer / VOICEVOX / 口パク / Glue で共有 |
| audio を割ると元の音にならない | event に「写像の単位」と「見せる窓」の区別が無い | `AudioEvent::take_head_beats` / `take_tail_beats` (take の窓)、`VideoEvent::take_head_beats`、fade ランプの張り出し `fade_in_lead_beats` / `fade_out_trail_beats`、片の作り方の SSoT `model::event_window::event_piece` (逆は `join_pieces`) |
| 片の境界でスペクトル処理 / tape の積分が途切れる | engine の状態を event 単位の位置 / id で持つ | `RenderedEvent::stream_key` = 素材 id、`acquire_engine` / `acquire_tape_cursor` が出力位置の連続で引き継ぐ (tape の積分器は event 数の上限も無くなった) |

判断:
- 片を単独で動かしても / 最初の片を消しても、続きの片は読み上げを始めない (「ー」が歌い直さないのと同じ)。 Glue で前の片とつなぐと元の 1 つに戻る。
- 片の端 trim は窓を動かすだけ (`AudioEvent::trim_left` / `trim_right`)。 take の外まで伸ばすときだけ同じ伸縮率で source を伸ばす。
- 移調 / 逆再生 / 伸縮 mode を片に掛けるときは先に take を窓へ詰め直す (`AudioEvent::rebase_take`) — 片自身の頭を起点に効く。
- 検証: `daw_audio` の `split_fidelity_tests` (engine の render で分割前と比較。 tape / slice は 1 sample も違わない、スペクトル経路は buffer 長の違いと同じ桁)、`common::audio_render` の片の波形、`text_compose` / `video_playback` の分割前比較、`app_state::split_fidelity` (読み上げ・歌唱・trim・移調)。

### 残件の続き (2026-09-15 main 決定、ユーザー不在時)

| 決定 | 内容 | 正本 |
|---|---|---|
| 続きの片の見え方 = 3 | アレンジに「続き」のチップ (逆極性の縁取り、どのクリップ色でも読める)、Inspector の読み上げ節に「ここから読む」トグル (1 undo step、ひと続きは窓の頭から読む) | `AppData::clip_text_reads` / `set_clip_text_reads`、`view::arrangement_view::draw_clip_continuation_badge` |
| 編集の効く範囲 = 1 | クリップへの編集 (Inspector の値・fade・写像、Auto-Fade、Auto-Warp / onset、アレンジの fade 角) は **窓に見えている片だけ**。 窓の中でひと続きの片 (同じ take の連続) はつないで編集して切り直す = 分割前に掛けてから割ったのと同じ。 表示もひと続きから読む | `common::model::window_edit`、`handler::clip_window`、`ClipContent::window_fades` |
| Auto-Crossfade | 窓の端に接する片の take の続きを、両側で揃えた 1 本の区間だけ重ねる (素材の足りない側で落ち込まない)。 再生は窓に見えている片だけを載せ、張り出しは端の片の take を伸ばす (隣の片を 2 回鳴らさない) | `Song::crossfade_adjacent`、`audio_clip_renderer::push_clip_events` |
| Bounce In Place | 同じ窓を見るクリップだけが焼いた content に置き換わる (別の窓を見る分割の片は元の content) | `Song::replace_window_content` |
| ARA = 1 | persistent id を安定 id に (素材 / content と take と素材 / クリップと event)。 片は modification を共有し、document は差分で編集 (作り直さない)、restore は新しい object にだけ filter で。 destroy する modification は直前にその状態を partial archive に取り、作り直す (undo / redo) ときはそこから戻す。 共有を解いた content (Make Unique / 共有 content の伸縮) の modification は、document に初めて現れるとき複製元の編集を写す (`cloneAudioModification`、`Song::content_forked_from`)。 v41 以前のアーカイブは読み替え表 (linked clip は content を分けてクリップごとの編集を保つ) | `common::ara_ids`、`daw_plugin_host::ara::graph_plan` / `session` |

付随して決めたこと:
- 時間軸を持つ event (audio / video / image / text) の content は、共有されていても分割で fork しない (切れ目を入れるだけで鳴り方が変わらない。 リンクと ARA の編集の共有を保つ)。 MIDI は切り口で発音し直すので従来どおり fork する (`Song::split_content_at_points`)。
- `AudioEvent::take_id` (片が継ぐ take の安定 id)。 貼り付け / 複製は別の take (ARA の編集を共有しない、`AudioContent::adopt_events`)。
