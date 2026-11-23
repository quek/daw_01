# r.md #157 コードトラック

曲に 1 本のコード進行の帯。コードを書く・見る・鳴らすだけでなく、**他のトラックがコードに追従して鳴る**
(Cubase の Follow Chord Track / Studio One の Follow Chords)。参照製品の一次情報は §6。

## 1. 確定仕様 (2026-11-22 ユーザー承認、1 問ずつ)

| # | 論点 | 決定 |
|---|---|---|
| Q1 | コードの長さ | **長さを持つ箱** (Studio One 型)。端をドラッグで伸縮、箱の無い空白区間がありうる (空白では追従しない) |
| Q2 | 追従の対象 | **MIDI ノート + VOICEVOX 歌声 + オーディオクリップ**。r.md #130 の移調と同じ非破壊 (書いたノートは書き換えず、鳴る音だけ変わる)。追従の既定は OFF |
| Q3 | 帯の場所 | **Arranger 帯の直下に固定の帯**。曲に 1 本、縦スクロールしても上に残る。見出し側に「Chords」ラベル・追従の全体 On/Off・試聴・表示切替 |
| Q4 | コードの編集 | **コード直下のポップアップ** (Studio One の Chord Selector 型)。空きのダブルクリックで 1 小節の箱を作って開く / 箱のダブルクリックで開く。文字入力欄にフォーカスして開き `Am7` + Enter で確定。五度圏の輪 (ルート) + 種類 + テンション + ベースのボタン + 次のコードの候補。変更は即反映して試聴音が鳴る。←→ で隣のコード、Tab で次のコードを追加、外クリック / Esc で閉じる。位置は固定、開いている間も再生は続く。Inspector には編集面を作らない |
| Q5 | 追従中のピアノロール | **書いた音を表示し、鳴る音を薄い枠で重ねる**。編集するのは書いた音。「鳴る音で確定」(Studio One の Freeze Pitch) コマンドを持つ |
| Q6 | 追従モードの設定 | **トラックヘッダの右クリックメニュー** (「移調に追従」の隣)。`コードに追従 ▸ Off / 平行移動 / 最寄りのコード音 / ベース / スケール`。右クリックしたトラックが選択に含まれれば選択全体。追従中はヘッダの名前の横にモードの印 |
| Q7 | 試聴音 | **既存の音源トラックを 1 本選んで鳴らす** (Cubase 型)。見出し側のドロップダウン。未選択なら無音。鳴る場面: コードのクリック / 編集、ポップアップの候補選択、「再生中も鳴らす」ON の再生中 |
| Q8 | 何を基準に動かすか (r.md #159、2026-11-23 ユーザー確定) | 帯に (抽出でも手でも) コードを **置いた時点のコードがメロディーの基準**。メロディーが動くのは **後からそのコードを書き換えたときだけ** (置いただけでは動かない)。ピアノなど **和音のパートは今どおり**、自分の和音から帯のコードへ合わせて動く |

### main が決めた細部

- **機能の範囲**: 両参照製品の和集合。入れる = voicing の選択 / 五度圏と近さによる候補 / MIDI 鍵盤のリアルタイム録音で
  コード列を作る / 演奏入力のリアルタイム変換 / ピアノロールでのコード適合の色分け / 度数表記 (Nashville / Roman) /
  今と次のコード表示 / コードの色 / 同じコードの一括選択 / コード列の移調 / コード進行プリセット。
  外す = daw_01 に土台が無い譜面連携・Chord Pads・Show Page・Chorder / HALion 連携。
- **オーディオの追従は全体の平行移動だけ** (和音の中の 1 音だけは動かせない)。オーディオでは「最寄りのコード音」
  「スケール」も「平行移動」と同じ量になる (ユーザーに報告済み)。ただし置いた時点のコードから動かすとき (下の
  基準) は、寄せ先 (構成音 / スケール) が変わらない書き換えでは動かさない (MIDI の旋律が動かないのと揃える)。
- **コードはノートの開始位置で決まる**。伸びているノートは途中のコード変更で鳴り直さない (Studio One と同じ)。
  オーディオの移動量は buffer ごとの今の位置で決まる。
- **評価の順序は 追従 → 移調**。コードも Key も書いた音の空間にあり、r.md #130 の移調は最後に足す。
- **基準 (r.md #159、Q8)**: 何から今のコードへ合わせるかは素材の音から決める (トラックに役割を持たせない)。
  式の SSoT は `common/src/chord_follow.rs` (`Basis`)、記録の規則の正本は `common/src/model/chord_basis.rs` の module doc。
  - **自分の和音** (素材が一緒に弾いている和音): MIDI はノートから毎回検出する (`chord_detect::own_chords`、保存しない)。
    検出するのは **鳴らすノートだけ** で、素材の単位はトラックのアレンジ (全クリップを曲の拍で 1 つに) / ランチャーの
    セル 1 つ (`common/src/chord_follow/own.rs`。窓の外に隠したノートは数えない — 分割・trim・Glue の前後で鳴りが
    変わらない)。入力が前と同じ素材は検出を使い回す (engine の索引の作り直し / GUI の世代キャッシュ)。
    「コードを抽出」と同じ鳴り始めの区切りで、始まりか終わりが揃った 3 音以上 (2 pitch class なら 3 音以上) の組が
    ある区間だけ — 2 拍の窓に集めた旋律の音は和音にしない。ノートの頭で鳴っていなければ、ノートに重なるか隙間なく
    接する和音 (`own_chord_touching`: 伸ばした分散和音の 1 音目 / ブンチャッのベース音は一緒に動く。離れた和音には
    属さない — 旋律に和音が 1 つ混ざっても旋律全体を和音のパートにしない)。食って入る和音 (箱の頭の半拍前から) は
    次の箱に従う。和音のパートの式は #157 のまま (確定仕様「和音のパートは今どおり」— C の和音の位置に Am7 を置けば
    平行移動では A へ動く)。ただしベースは、自分の和音のベース音 (無ければ根音) → 今のコードのベース音 (無ければ根音)
    (Studio One の Bass。#157 は自分の和音の根音から写していたので、転回形 (C/E) の和音のパートからコードを抽出した
    直後に +4 動いた)。
  - **置いた時点のコード** (`Clip::chord_basis`): 和音を持たない素材の基準。記録 1 件は (素材, 箱の系統
    `ChordEvent::lineage`) の組で、同じ系統の箱の下でだけ効く。新しく基準を決めて記録するのはコードを **書き換えた**
    とき (`Song::set_chord` / `transpose_chords`) だけで、その箱の下でその系統の記録の無い素材へ書き換える前のコードを
    書く (ほかに記録を書くのは、元の素材の基準を新しい素材へ移す経路 — 分割の写し / 時間ごとの貼り付け / 焼き込み
    (下の「バウンス」) / Glue)。置く・貼る・複製・抽出した箱は新しい系統なので、その下の素材は動かない。素材を別の箱の
    下へ動かす / 写すと書いた音。書き換えで動いていた素材の上に箱を置く / 貼る / 重ねると、その部分は書いた音に戻る
    (同じコードでも — 系統が変わるため)。記録は素材の事実なので、追従 Off のトラックと曲全体のスイッチ Off の間の
    書き換えも記録する (ON にすると、ずっと ON だったときと同じ音)。1 件は bincode 約 7〜12 B (Note 1 個の約 4 割)
    で、ノートと同じく毎回の LoadSong と undo の snapshot に載る (書き換えで記録を足すクリップからもう効かない系統の
    記録を落とすので、開いている間も「ノート × 今ある系統」で頭打ち)。クリップ / セル / トラックの貼り付けは、同じ id 空間 (同じプロジェクトの
    同じ開き方、`SongDoc::id_session`) からのときだけ記録を運ぶ (`ClipChordBasis::pasted`)。
  - モードごとの式 (Q1 = 1): 平行移動 / ベースは差だけ移調 (呼び名だけの書き換え = 構成音が同じなら動かない)、
    最寄りのコード音 / スケールは元のコードの構成音 / スケールの音だけを寄せ、経過音は動かさない。種類だけの書き換え
    (C → Cm) は平行移動では根音の差が 0 なので、ピアノ (C の和音) もメロディー (E) も動かない。
  - オーディオ: 自分の和音は再生位置の音源フレームを覆う、和音を弾いている区間 (`AudioChordSpan::chordal`、持ち越さ
    ない)。記録は (take, 系統, 音源フレームの範囲) で、engine は compile で event ごとに曲の拍へ写し
    (`Song::audio_event_basis`)、RT は拍と箱の系統で引く。クロスフェードの張り出しは窓の端の値で鳴らす。
  - トニックを基準にする規則は廃止 (r.md #159 の不具合の原因)。
- **重なりの規則**: クリップと同じ「後から置いた方が勝つ」。時間操作 (セクション移動 / Delete Time / Insert / Paste
  Time) では、箱もクリップの窓と同じく境界で割って同伴する。
- **聴き方の状態は Song に載せない** (不変条件 6): 試聴トラック・「再生中も鳴らす」・帯の表示 / 非表示・表記は
  session / ViewState。書き出しに試聴音は入らない。追従の全体スイッチとトラックのモードは鳴る音を変えるので Song。
- **ランチャーのセル**: 追従は再生時の曲の playhead の位置のコードで決める (#130 のセルの MIDI と同じ)。和音の
  セルはそのコードへ合わせて動き、旋律のセルは書いた音のまま (セルは曲の位置を持たないので記録を読まない。MIDI も
  オーディオも同じ。記録は残すので、アレンジへ戻すと元どおり鳴る)。
- **バウンス**: #130 と同じ「外に出すもの = 鳴る音 / プロジェクトの中に作るもの = 書いた音」。焼く Song は追従を
  切り、できたトラック / クリップの追従モードは元と同じ。r.md #159: 素材が箱ごとに動いていた元のコード (置いた
  時点のコード / 和音のパートの自分の和音) を、その箱の系統で焼いた音源の記録へ写す (`Song::baked_chord_basis`、
  バウンスとオーディオの Glue。In Place は張り替える linked clip ごとにその位置で `baked_window_bases`)。焼く前と後で
  鳴りが揃うのは平行移動 / ベースだけ — オーディオの追従は全体の移動なので、最寄りのコード音 / スケールで構成音だけが
  寄っていた素材は、焼いた後は根音の差で全体が動く。
- **ARA (Melodyne)**: 追従できない (プラグインがソースを読む)。#130 と同じ扱い。

## 2. 土台 (base branch `rmd-157-chord` に実装済み)

ここにある API が 4 本の作業の境界。**土台のファイルの既存の関数の意味を変えない** (足すのは可)。変える必要が
出たら main セッションに相談する。

| 何 | どこ |
|---|---|
| 型: `Chord` / `ChordKind` / `Tensions` / `Spelling` / `ChordEvent` / `ChordFollow` / `SourceChord` / `AudioChordSpan` / `ClipChordBasis` (r.md #159) | `common/src/model/chords.rs` (wire、`WIRE_SOURCES` 登録済み) |
| `Song.chords` / `Song.chord_follow_enabled` / `Song.audio_chords`、`Track.follow_chords`、`Clip.chord_basis`、`ids.next_chord_id`、保存 v48 | `common/src/model.rs` / `track.rs` / `content.rs` / `ids.rs` |
| 置いた時点のコードの記録 (書き換え `set_chord` / `transpose_chords` と置き直し `recast_chords`、引き継ぎ `copy_note_basis` / `forget_note_basis` / `relineaged`、正規化、オーディオの記録を曲の拍へ写す `audio_event_basis`、焼き込み `baked_chord_basis` / `baked_window_bases`)。箱の系統の採番は `chord_ops.rs` | `common/src/model/chord_basis.rs` |
| 箱の編集 (`place_chord` 上書き規則 / `move_chords` / `resize_chord` / `duplicate_chords` / `delete_chords` / `set_chord` / `transpose_chords` / `set_chord_color` / `split_chords_at` / `normalize_chords`) と参照 (`chord_at` / `next_chord_after` / `chord_by_id` / `track_chord_follow` / `chord_follow_can_apply`) | `common/src/model/chord_ops.rs` |
| 時間操作への同伴 (ripple / `move_section` / copy・delete・insert・paste time、`TimeRangeCopy.chords`) | `common/src/model/section_ops.rs` / `time_ops.rs` |
| **追従の評価の SSoT**: `follow_pitch` (ノート) / `follow_shift` (オーディオ) / `live_pitch` (演奏入力) / `TrackChordFollow` (読み口) / `chord_scale_mask` / `snap_to_mask`。自分の和音の素材と検出の使い回し (`OwnScope` / `OwnChordsMemo`) は `chord_follow/own.rs` | `common/src/chord_follow.rs` |
| コード名: `format_chord` (音名 / 数字 / ローマ数字) / `parse_chord` / `note_name` / `kind_suffix` | `common/src/chord_name.rs` |
| 検出: `detect_chords` / `extract_chords` (ノート列 → 区間) / `match_chord` (pitch class の強さ → コード。オーディオの chroma もここへ渡す) / 追従の基準の `own_chords` / `own_chord_at` / `own_chord_touching` / `audio_own_chord_at` | `common/src/chord_detect.rs` |
| voicing と変換: `chord_pitches` / `Voicing` / `Song::chords_to_clip` / `Song::chords_from_clips` | `common/src/chord_voicing.rs` |
| イベント 3 族: `AppEvent::Chord(ChordTrackEvent)` / `ChordFollow(ChordFollowEvent)` / `ChordTool(ChordToolEvent)` | `daw_gui/src/event_chord.rs` |
| handler | `daw_gui/src/handler/chord_event.rs` / `chord_follow_event.rs` / `chord_tool_event.rs` |
| 表示名の口: `AppData::chord_label(chord, beat)` / `chord_notation()` (**コード名を出す表示面は必ずこれを呼ぶ**) | `daw_gui/src/handler/chord_event.rs` |
| headless script: `daw.chordEvent(json)` (3 族を保存形のまま) / `daw.chordsJson()` / `daw.parseChord(text)` / `daw.soundingKey(trackId, clipId, noteId)` | `daw_gui/src/script/chords.rs` |

## 3. 分担 (4 本、base から分岐)

| 名前 (herdr agent / branch) | 範囲 |
|---|---|
| **`c157-band`** / `rmd-157-band` | 帯とポップアップ (§3.1) |
| **`c157-follow`** / `rmd-157-follow` | 追従 (§3.2) |
| **`c157-roll`** / `rmd-157-roll` | ピアノロールと今 / 次のコード表示 (§3.3) |
| **`c157-tools`** / `rmd-157-tools` | 試聴・voicing・検出・録音 (§3.4) |

### 3.1 `c157-band` — 帯とポップアップ

- **帯**: Arranger 帯 (`widgets/arrangement/` の `arranger_rect` / `SECTION_LANE_H`) の直下に同じ流儀でコード帯を足す。
  描画・hit・ドラッグの session は新しいファイル (`widgets/arrangement/chord_lane*.rs`) に置き、既存ファイルには呼び出しだけ
  足す (`draw.rs` / `geometry.rs` / `mod.rs` は budget 超過、`release.rs` / `view_build.rs` / `arrangement_view.rs` は上限間近)。
- **操作** (Arranger 帯・クリップと同じ感触): 空きのダブルクリックで 1 小節の箱を作る (範囲選択があればその長さ) /
  箱のドラッグで移動 (複数選択は相対位置を保つ) / 端のドラッグで伸縮 (Studio One: 境界の下半分は隣と連動、上半分は単独。
  Alt ドラッグで複数選択を比例伸縮) / Ctrl+ドラッグで複製 / Cut・Copy・Paste (OS クリップボード、`ChordTrackEvent::PlaceMany`) /
  Delete / 矩形・Shift・Ctrl の選択 (`state/selection.rs`、Delete 等は last-selection-wins) / snap は既存の `view.snap`。
- **ポップアップ** (Q4): `open_overlay` 系 (再生継続)。文字入力は `common::chord_name::parse_chord`、読めない間は確定しない。
  r.md #159: そのポップアップで作った箱 (空きのダブルクリック / Tab) のコードを決めるのは「置く」(`ChordTrackEvent::Recast`、
  閉じるまで ←→ で戻っても同じ)、ほかの箱のコードを変えるのは「書き換え」(`SetChord`)。
  五度圏の輪 (クリックでルート、同じ位置の再クリックで異名同音 `Spelling`) / 種類 (`ChordKind::ALL`) / テンション
  (`Tensions::ALL`) / ベース。**次のコードの候補**: 直前のコードとキーから (ダイアトニック / 五度圏の近さ / 終止形) を並べ、
  上下キーで巡回。←→ で隣の箱、Tab で次の箱を追加。変更のたびに試聴の口 (`c157-tools` が用意、§4) を呼ぶ。
- **右クリックメニュー**: 色 / 同じコードを全部選択 / 選択を移調 (±1 / ±12) / 全部削除 / プリセットに保存・プリセットを挿入
  (`.chords` = JSON。挿入先のキーへ移調)。
- **見出し側**: 「Chords」ラベル / 追従の全体 On/Off (`ChordFollowEvent::SetEnabled`) / 表記 (音名・数字・ローマ数字) /
  試聴トラックのドロップダウンと「再生中も鳴らす」(状態と口は `c157-tools`。§4)。帯の表示 / 非表示は Arranger 帯の近くに
  トグルを置く。表示状態と表記は ViewState に保存し、`AppData::chord_notation()` の中身をそこから読むよう差し替える。
- **ドラッグの受け渡し**: 箱 → トラックレーンへドロップで `ChordToolEvent::ChordsToClip`、MIDI クリップ → 帯へドロップで
  `ChordToolEvent::ExtractFromClips`。オーディオクリップ → 帯は `c157-tools` の検出イベント (§4) を撃つ。
- 着手前に `daw_gui/CLAUDE.md` と `ui/CLAUDE.md` を読む。可変背景の上の文字はコントラストを保証する。

### 3.2 `c157-follow` — 追従

`docs/plan_rmd_130_transpose.md` の経路を全部辿り、**移調量を読んでいる箇所の手前に追従を入れる**
(`rg "transpose|sounding_key|transposable"` の全件を表にしてから着手)。

- **engine (`daw_audio`)**: `sequencer.rs` の note-on の鍵盤 (`sounding_key(note.pitch, transpose)`) を
  「追従 → 移調」に。追従の実効モードは `SongIndex` に焼く (RT で Song を線形走査しない、確保しない)。追従する全モードの
  自分の和音 (トラックのアレンジ / セルごとの検出結果、鳴らすノートが前と同じなら使い回す `OwnChordsMemo`) も
  compile 時に off-RT で索引に焼き、クリップの記録 (r.md #159) は
  sequencer が持っている clip から二分探索で引く (`PitchMap::note_key`、箱の系統で)。発音台帳 (`note_ledger`) は
  送った鍵盤を持つ既存の形のまま。ランチャーのセルは playhead の位置のコードで、記録は読まない (`PitchMap::for_cell`)。
- **オーディオ**: `audio_clip_renderer.rs` の `transposable` / buffer ごとの移調量に、`follow_shift` を足す。自分の和音は
  再生位置の音源フレームを覆う、和音を弾いている区間 (`audio_own_chord_at`)。アレンジの event は、さらに compile で
  曲の拍へ写した記録 (`RenderedEvent::chord_basis` = `Song::audio_event_basis`、写像は compile で 1 回作る) を
  `TrackChordFollow::audio_shift` で引く。移調 0 かつ追従なしの event は今までどおりエンジンを通さない。
- **オフライン**: VOICEVOX (`handler/voicevox.rs` の `sung_pitch` / 口パクの fingerprint — 開いただけで `*` にならない冪等性)、
  SMF 書き出し (`midi_export.rs`)、バウンス (`bounce_ops.rs` の `isolated_track` で追従を切る)。`TrackTranspose::key` に
  相乗りできるならそこへ (読み口を 1 本に保つ)。
- **GUI**: 演奏プレビュー (`handler/transpose.rs` の `preview_key`: MIDI 入力モニター / 仮想鍵盤 / 鍵盤レーン)
  = **演奏入力のリアルタイム変換** (Cubase の Live Input)。最寄りのコード音 / スケールで追従するトラックで弾いた音は、
  弾いた音が **行く先** のコードへ寄り (`live_input_key` / `live_input_landing`)、r.md #159 Q2 で **寄せた音を書く**
  (書いた音は記録を持たないので再生でもそのまま = 聴いた音 = 書いた音 = 再生の音)。行く先は、録音中は playhead
  (`record_midi_note_on_tracks`)、ステップ入力の先のトラックはステップカーソル (窓へ戻した位置、モニターも同じ)、
  ランチャーのセルへのステップ入力は寄せない (セルは書いた音のまま鳴るので、モニター・書く音・再生とも弾いた音)、
  書かれない録音待機トラックのモニターは playhead。平行移動 / ベースは弾いた音のまま (移調だけ)。録音の台帳は弾いた
  鍵盤で引き、同じ音へ寄る 2 つ目の鍵は同じノートを一緒に押さえる (重なったノートを作らない)。録音中の Snap Live
  Input もモニターと録音で同じ口を通す。MIDI Capture の試聴は置く位置も置くトラックも決まっていないので弾いた音
  (移調だけ)、置いたときに置く位置のコードへ寄せた音を書く (`live_input_key_at`)。**既にあるノートの試聴** (ナッジ) は
  playhead ではなくノートの位置のコードで鳴らす (`note_preview_key` = ピアノロールの薄い枠と同じ鍵盤 + 移調)。
- **ヘッダ** (Q6): `view/track_header_menu.rs` に「コードに追従」(メニューは平坦な文字列リストなので、選ぶとモードの
  メニューを開く 2 段、または `コードに追従: <モード>` を 5 行並べる — 既存の `context_menu` の形で実現できる方)。
  印は `widgets/arrangement/header.rs` の `♮` の隣 (`ChordFollow::mark()`)。
- **鳴る音で確定** (Q5): 選択したクリップ / ノートの書いた音を、今の追従結果で書き換えるコマンド (`ChordFollowEvent` に足す)。
  r.md #159: 確定したノートの記録は消す (二重に動かない)。選んだ音だけの確定で残りの音の鳴り方が変わるクリップは
  確定しない (ステータスに出す)。
- **ピアノロール用の読み口**: 「このクリップのノート 1 つが鳴る鍵盤」を返す関数を GUI 側に 1 つ用意する
  (`c157-roll` が薄い枠の描画に使う。§4)。

### 3.3 `c157-roll` — ピアノロールと今 / 次のコード表示

- **上端のコード帯** (表示専用): `widgets/piano_roll/` のルーラー直下に、開いているクリップの範囲のコード名を出す
  (`AppData::chord_label`)。新しいファイルに置く (`piano_roll/run.rs` は budget 超過)。
- **色分け**: コードの構成音に当たるノート / スケール内 / どちらでもない、を区別する (Cubase の Event Colors: Scale/Chords)。
  トラック色を潰さない形 (輪郭・明度) で、明暗両方のクリップ色でコントラストを保証する。背景の行にも今のコードの構成音の
  ガイドを出す (既存のスケールのハイライト `draw_grid_background` と同じ流儀、ON/OFF のトグル)。
- **鳴る音の薄い枠** (Q5): 追従するトラックのクリップで、書いた音と鳴る音が違うノートに、鳴る音の位置へ薄い枠を重ねる。
  鳴る音は `c157-follow` の読み口 (§4、`ClipSoundingKeys`) を呼ぶ。
- **今と次のコード** (Studio One の Chord Display): トランスポートの Key の近くに、playhead の位置のコードと次のコードを
  出す (`Song::chord_at` / `next_chord_after`)。トランスポートの幅が足りなければ配置を main に相談。
- ARA: `kARAContentTypeSheetChords` をホストが答えると Melodyne がコードを読める (`daw_plugin_host/src/ara/host_controllers.rs`)。
  **余力があれば**。先に上の 4 つを通す。

### 3.4 `c157-tools` — 試聴・voicing・検出・録音

- **試聴** (Q7): 試聴トラック (session の状態) へプレビューのノートを送る口を 1 つ作る
  (`AppData::audition_chord(&Chord)` 相当。既存の `send_preview_on/off` と台帳を使う。追従しないよう書いた音で送る)。
  「再生中も鳴らす」は engine 側でコードトラックを試聴トラックのノート源にする — **Song に載せない** (不変条件 6、
  SC Listen と同じ「聴き方の状態」を別 command で渡す)。`sequencer.rs` は `c157-follow` も触るので、足すのは呼び出し
  1 か所にして本体は新しいファイルに置く。
- **voicing**: `common::chord_voicing::Voicing` に Piano / Guitar / 声部の跳躍を抑える自動 (Cubase の Adaptive Voicings) を足す。
  試聴とノート化 (`chords_to_clip`) が同じ `chord_pitches` を通る形を保つ。選択は見出し側 (ViewState)。
- **オーディオからの検出**: 音源 → chroma (`realfft`、`master_meter/spectrum.rs` と `handler/clip_events.rs` の onset 検出の流れ) →
  `common::chord_detect::match_chord` → `ChordToolEvent::SetAudioChords` で保存。重いので off-thread、進捗表示は既存の
  activity の仕組み。追従を ON にしたオーディオトラックで未検出の音源があれば自動で検出を走らせる。r.md #159: 窓ごとに
  「和音を弾いているか」(`chordal` — FFT フレームの半分以上が単音の倍音列に乗らず 3 pitch class 以上。音程が揺れて
  いるフレーム (ビブラート) は短い FFT で倍音列を測り直す、`daw_gui/src/chord_audio_detect.rs` の module doc) を持ち、
  追従の基準にするのはその窓だけ (抽出は全窓)。v48 未満のファイルの検出結果は捨てて測り直す。
  オーディオクリップ → 帯のドロップ / クリップの右クリック「コードを抽出」で、検出結果を帯にも置く。
- **検出の設定** (Cubase の Create Chord Events): 窓の長さ / ベース音を含める / テンションを含める / 短いノートを無視。
  クリップの右クリック「コードを抽出…」から。
- **MIDI 鍵盤でコード列を作る**: 帯の録音待機 (見出し側のボタン) を ON にして録音すると、弾いた和音がコードの箱になる
  (`state/midi_capture.rs` の和音まとめと `match_chord`)。ポップアップが開いている間に鍵盤で弾いた和音を、そのコードに設定する
  (Studio One の Instrument Input)。
- **MIDI ファイル → 帯**: エクスプローラから `.mid` を帯へドロップするとコードを検出して置く (`docs/plan_midi_import.md`)。

## 4. 作業間の約束

- **コード名は `AppData::chord_label` / `common::chord_name::format_chord` だけ**で作る。
- **試聴の口**は `c157-tools` が `daw_gui/src/handler/chord_audition.rs` に `AppData::audition_chord(&Chord)` を作る。
  `c157-band` は届くまで同名の空の関数を自分の branch に置かず、呼び出し箇所に 1 行で `self.audition_chord(..)` を書ける形に
  して待つ (統合時に main がつなぐ)。先に要るなら agent 同士で相談する。
- **鳴る鍵盤の読み口**は `c157-follow` が `daw_gui/src/handler/chord_follow_view.rs` に作る (ノート 1 つの鳴る鍵盤)。
- **新しい状態 / イベントは自分のファイルに足す**。`event_chord.rs` は族ごとに持ち主が違う:
  `ChordTrackEvent` = band、`ChordFollowEvent` = follow、`ChordToolEvent` = tools。`event.rs` / `app.rs` には足さない
  (3 族の arm は配線済み)。
- **重なるファイル**: `sequencer.rs` (follow が本体、tools は呼び出し 1 か所)、`arrangement_view.rs` (band が本体、
  tools はクリップの右クリックメニューに 1 項目)、`view/transport.rs` (roll)、`widgets/arrangement/header.rs` と
  `view_build.rs` (follow は印の 1 か所、band は帯)。ほかの作業の持ち場を触る必要が出たら、最小の差分にして報告に書く。
- **相談**: 相手の名前で `herdr agent prompt <名前> "<質問>"` を送ってよい (返事を待たずに進められる形で聞く)。
  作るものが変わる規模の判断は main (`c157-main`) に聞く。

## 5. 全作業共通の規則

- 規範は root の `CLAUDE.md`。**最小の動く形を先に作り、動かして確かめてから広げる**。
- 着手時: `cp -r F:/dev/daw_01/third_party <worktree>/third_party` (実コピー。junction / symlink 禁止) → `make build`。
- 検証: 触った crate の `cargo check -p <crate>`、関係するテスト target を名指しで。GUI / IPC / 再生を跨ぐ確認は
  `daw_gui --script <js>` (headless、無音) を自分で回す。**全件系 (`make test` / `make test-nolaunch` / `make clippy` /
  `make arch-lint` / `make gates`) は回さない** (統合後に main が 1 回)。窓が出る起動 (`--gui` / `make run`) はしない
  (ユーザーが席を外している。見た目の確認は統合後に main がまとめて行う)。
- 自明な修正に回帰テストを書かない。本番の算術を写すだけのテストは書かない。
- RT 制約 (確保・ロック・I/O 禁止)、アーキテクチャ不変条件 (特に 1 安定 id / 5 `edit_song` / 6 聴き方の状態を Song に載せない /
  7 `WIRE_SOURCES` / 9 サイズ budget — 超過ファイルには足さず新しいファイルへ) を守る。protocol 型を足したら `make build`。
- commit: 自分の branch に、区切りごとに日本語メッセージで commit する (`git add` はパスを列挙)。main へは入れない。
- **止める基準**: 最小の動く形が 2 時間で動かなければ、書きかけを WIP commit して止め、何が塞いでいるかを報告する。
  見積もりが膨らみ続けたら続ける前に報告する。
- 終わったら: 最終メッセージに「実装したこと / 計画から変えた判断 / 回した検証と結果 / 残したこと / 他の作業の持ち場を
  触った箇所」を書く。同じ内容を `F:/dev/daw_01/.claude/herdr/157/<名前>_report.md` にも書く。

## 6. 参照製品の一次情報 (2026-11-22 調査)

- Cubase Pro 16 Chord Functions: <https://www.steinberg.help/r/cubase-pro/16.0/en/cubase_nuendo/topics/chord_functions/chord_functions_chord_events_c.html>
  (Follow Chord Track のモード: `…/chord_functions_follow_chord_track_r.html`、検出: `…/chord_functions_make_chords_dialog_r.html`、
  voicing: `…/chord_functions_voicings_c.html`、Chord Assistant: `…/chord_functions_assistant_r.html`)。
  追従は破壊的 (イベントを書き換え、OFF にしても戻らない)。
- Fender Studio Pro 8.2 (旧 Studio One) Chord Track: <https://fenderstudiopromanual.fender.com/en/Content/Arranging_Topics/Chord%20Track.htm>。
  追従は非破壊 (OFF で元に戻る、Freeze Pitch で確定)。エディタには動いた後の音が出る
  (<https://www.soundonsound.com/techniques/studio-one-focal-chords>)。
- Logic Pro: 普通の MIDI リージョンはコードトラックに追従できない (Session Player だけ)。
  <https://support.apple.com/guide/logicpro/control-transposition-pitch-source-parameter-lgcp047d0131/mac>
