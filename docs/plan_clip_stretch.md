# クリップ ストレッチ / 再生範囲 整理 (FIXME #61)

ステータス: **実装完了・実機検証待ち**（2026-06-15）。worktree `clip-stretch`。
決定: Q1 = **Shift + 端 drag = ストレッチ**（plain = トリム、Ableton 流）、
Q2 = **ピッチ保持（granular = Stretch モード）を既定**（ユーザー確認済）。
build / clippy(`--workspace -D warnings`) / test 全 green。新規 unit test: `stretch_ratio_for` ×5、
`stretch_remap` ×3、`trim_audio_event` ×5。

FIXME #61:
> クリップの再生範囲を変える機能とストレッチ機能を整理して実装。
> いまオーディオクリップの長さを変えると、波形はストレッチですが再生は再生範囲が
> 変わるだけ、音がストレッチしていません。MIDI クリップも対応。
> ストレッチ時のマウスモディファイアも考える必要があるので、現状を洗い出して提案。

---

## 0. 結論（ひとことで）

クリップ長 (`event_length_beats` / `Clip.length_beats`) と「source の読む範囲」
(`source_start_frames..source_end_frames`) は**本来独立した 2 軸**。だが現状の **操作・
描画・再生** がこの 2 軸を混同しているため「見た目ストレッチ・音はトリム」という
ズレが出ている。これを **TRIM（再生範囲を変える）** と **STRETCH（時間を伸縮する）** の
2 操作にきれいに分離し、描画と再生を各操作に**一致**させる。

| 操作 | source 範囲 | クリップ長 | 比 (native長/clip長) | 波形描画 | 再生 |
|---|---|---|---|---|---|
| **TRIM** | 変わる（窓を動かす） | 変わる（窓と連動） | **1 のまま** | 切り取り (crop) | native rate で窓内を再生 |
| **STRETCH** | 固定 | 変わる | **1 から乖離 = 伸縮量** | 引き伸ばし (rubber-band) | 比に従って time-warp |

重要: STRETCH 量は **導出量**（`native_duration / event_length`）であり、オーディオは
**新フィールド不要**。TRIM が比を 1 に保ち、STRETCH が比を崩す——それだけ。

---

## 1. 現状（一次調査、file:line 付き）

### 1.1 データモデル
- `Clip { start_beat, length_beats, content_id }`（`common/src/model.rs:3000`）。
- 音声は共有 `AudioContent { events: Vec<AudioEvent> }`。
  `AudioEvent { event_start_in_clip_beats, event_length_beats, source_start_frames,
  source_end_frames, stretch_mode, pitch_semitones, .. }`（`common/src/model.rs:3416-3485`）。
  → **クリップ内ローカル時間 (beats)** と **source frame 窓** は独立した別フィールドで、
  両者を結ぶ不変条件は無い。`StretchMode { Raw, Repitch, Stretch, Slice }`（default Raw）。
- import 時は 1 event が「ファイル全体」を「クリップ全体」に張る慣習だが、強制不変条件
  ではない（draw は `events.first()` 前提、`daw_gui/src/view/arrangement_view.rs:2416`）。
- MIDI は `MidiContent { notes }`、`Note { start_beat, duration_beats, .. }` は
  **clip ローカル beats**（`common/src/model.rs:3305,3903`）。clip 長と note の関係に
  不変条件は無い。linked clip は content 共有。

### 1.2 操作（arrangement 端ドラッグ）
- ゾーンは 3 つだけ: `Move`（中央）/ `ResizeLeft`（左端）/ `ResizeRight`（右端）。
  **ストレッチハンドルは存在しない**（`ui/crates/ui/src/widgets/arrangement.rs:483,1740`）。
- modifier: **Alt = snap 無効のみ**、**Ctrl = linked copy**、**Ctrl+Shift = independent
  copy**（Move 限定）、**Shift 単独 = marquee 選択（clip ドラッグから除外）**
  （`arrangement.rs:5341,7790`）。resize には Ctrl/Shift は無効。
- `ResizeClips` → `AppEvent::ResizeClip` → `AppData::resize_clip`（`daw_gui/src/app.rs:14161`）。
  - **右端**: `clip.length_beats` を更新し、`event_length_beats` を「収まるように」**下方向に
    clamp するだけ**。`source_end_frames` は**一切触らない**、伸長時も伸びない
    （`app.rs:14213-14218`）。→ **TRIM 右端が壊れている**（source 窓が連動しない）。
  - **左端**: `source_start_frames` を進めて `event_start/length` を詰める（概ね正しい TRIM）。
  - 別経路: Audio Editor のグリップは `SetAudioEventTrim`（`app.rs:13767`）で
    `source_start/end_frames` と `event_length_beats` を**連動**させる正しい TRIM。
    → arrangement 右端もこれと同じ連動にすべき。
- MIDI: `resize_clip` は **note に一切触れない**（`ClipContent::Midi` 分岐が無い）。

### 1.3 描画
- 波形は `[source_start_frames, source_end_frames]` を**クリップ幅全体に rubber-band**
  （`samples_per_pixel = view_len / pixel_w`、`arrangement_view.rs:2489-2563`,
  `ui/crates/ui/src/widgets/waveform.rs:534`）。→ クリップ長を変えると常に伸縮して見える。
  これは **STRETCH の見た目としては正しい** が、現状 TRIM のとき音と矛盾する。
- MIDI mini-view は `px_per_beat = w / clip.length_beats` で再計算するだけ
  （`arrangement_view.rs:2765`）。note は scale されない。piano roll は clip 長を無視。

### 1.4 再生エンジン
- scheduler はクリップ長で MIDI note を **gate**（`start_beat >= length_beats` の note-on を
  drop、note-off を clip 端に clamp、`daw_audio/src/sequencer.rs:165-173`）= トリム。
- 音声 `render_audio_events`（`daw_audio/src/audio_clip_renderer.rs:327`）:
  - Raw/Repitch は `source_pos = event_local * effective_pitch_ratio`。
    `pitch_ratio_for` は Raw=`sr_factor`、Repitch=`sr_factor*2^(semi/12)`
    （`common/src/audio_render.rs:53`）。→ **クリップ長と無関係**に native rate で進む。
  - Stretch は granular（`tempo_ratio = current_bpm/nominal_bpm` 駆動、pitch 保持）、
    Slice は onsets を tempo_ratio で配置。→ **どれも `event_length/source長` の比を使わない**。
  - export も同じ経路（`tempo_ratio=1.0` 固定）なので同じズレを継承。
- **欠けている一点**: per-event `stretch_ratio = native_duration / event_length` を
  compile 時に算出して RenderedEvent に持たせ、DSP の source 進度に掛けること。

### 1.5 DAW 慣行（一次情報）
plain 端ドラッグ = TRIM はほぼ全 DAW 共通。STRETCH の modifier は分かれる:
- Ableton Live = **Shift**+端ドラッグ（warp 必要、pitch 保持）。MIDI も同じ Shift。
- REAPER / Bitwig / Studio One = **Alt**+端ドラッグ。
- Cubase = modifier 無し、**持続ツールモード**「Sizing Applies Time Stretch」。

daw_01 は **Alt が既に snap 無効**なので REAPER/Bitwig/Studio One の Alt 案は衝突。
**Ableton の Shift 案が唯一の単一 modifier で衝突しない**（Alt=snap 軸 / Ctrl=copy 軸 /
Shift=stretch 軸 で直交、Shift+Alt = snap 無効ストレッチも自然に表現可）。

出典:
- Ableton manual: https://www.ableton.com/en/manual/audio-clips-tempo-and-warping/
- Bitwig userguide: https://www.bitwig.com/userguide/latest/working_with_audio_events/
- Studio One: https://s1manual.presonus.com/Content/Editing_Topics/Timestretching.htm
- Cubase: https://www.steinberg.help/r/cubase-pro/15.0/en/cubase_nuendo/topics/parts_events/...time_stretch_t.html

---

## 2. 提案

### 2.1 マウス操作（★ 上流確認 §4-Q1）

推奨 = **Ableton 流 Shift**:

| ジェスチャ | 操作 | audio | MIDI |
|---|---|---|---|
| 端ドラッグ（plain） | **TRIM** | source 窓と長さを連動（比=1）。source 端で頭打ち | clip 長で note を gate（現状維持） |
| **Shift + 端ドラッグ** | **STRETCH** | source 窓固定・長さ変更（比が乖離） | note の start/length を比例 scale |
| Alt + 端ドラッグ | 上記 + snap 無効（現状の Alt を維持） | 同 | 同 |
| Ctrl / Ctrl+Shift + 中央ドラッグ | linked / independent copy（現状維持） | 同 | 同 |

- pivot: 右端ドラッグ = 左端固定、左端ドラッグ = 右端固定。
- **衝突解消**: Shift 単独 = marquee は現状 clip ドラッグから除外されている。
  → **resize ゾーン（左右端グリップ帯）に限り Shift = STRETCH** を許可し、clip 本体・空き
  レーンの Shift+ドラッグは従来どおり選択。widget 側 `(!shift || ctrl)` ゲートを「resize
  ゾーン上なら Shift も resize セッション開始」へ拡張。
- カーソル: STRETCH 中は専用カーソル / ゴーストで TRIM と区別。

代替案（§4-Q1 で選べる）: **Cubase 流ツールモード**（"Sizing = Time Stretch" トグルで
plain 端ドラッグが STRETCH に切替）。modifier 衝突ゼロだがモード状態を持つ。

### 2.2 オーディオ STRETCH（再生）
- **モデル**: 新フィールド不要。STRETCH は `source_start/end_frames` を**固定**し
  `event_length_beats`（と `clip.length_beats`）のみ変更。これで比が崩れる。
- **TRIM 修正**: 右端 TRIM を `SetAudioEventTrim::Right` と同じく `source_end_frames` を
  beats→frames 換算で連動。source 端で cap（source より長くしたい時は STRETCH を使う）。
  これで TRIM 時に波形が crop 表示になり、音と一致。
- **engine**: `compile_audio_schedule` で per-event
  `stretch_ratio = native_duration_frames / event_length_frames`
  （`native_duration_frames = source_end-source_start`、
  `event_length_frames = event_length_beats * source_sr * 60 / nominal_bpm`）を算出し
  `RenderedEvent` に f64 で持たせる（RT 安全、compile 時の除算のみ）。
- **render**: stretching モードで source 進度に `stretch_ratio` を合成:
  - **Stretch（pitch 保持・既定）**: granular の `grain_source_offset` を
    `* tempo_ratio * stretch_ratio` に。pitch 不変で長さだけ充填。
  - **Repitch（テープ式）**: `effective_pitch_ratio *= stretch_ratio`（pitch も伴って変化）。
  - **Slice**: onset 配置に `stretch_ratio` を合成。
  - **Raw**: 時間操作しない定義なので STRETCH しない。Raw クリップを Shift ドラッグした
    場合は **Stretch（pitch 保持）へ自動昇格**（既定の伸縮アルゴリズム）。Repitch を使いたい
    場合は inspector の既存 stretch-mode セッタで切替（§4-Q2、既定は pitch 保持）。
  - tempo-follow（`current_bpm/nominal_bpm`）と STRETCH は **乗算合成**（長さ充填しつつ
    後段の tempo automation にも追随）。

### 2.3 MIDI STRETCH（再生）
- **モデル編集方式**（render 比方式より単純で SSoT 1 つ）: STRETCH 時に
  `factor = new_length / old_length` で content 内 **全 note の `start_beat` と
  `duration_beats` を比例 scale**（pivot 側固定）。scheduler / piano roll / midi_export は
  scale 済み beats を読むので**追加変更不要**。
- **linked clip**: note の scale は content 編集 = 共有相手も変わるが、clip 長は per-clip
  なので siblings が破綻する。→ STRETCH は対象 clip の content を**自動 fork（independent
  化）**してから scale（既存 `fork_content` 流用、`app.rs:14319`）。TRIM は現状どおり共有維持。
- TRIM（plain）は現状の gate 方式を維持（note 非破壊）。

### 2.4 描画を操作に一致させる
- audio: TRIM が source 窓を連動させれば、既存の「source→clip幅 rubber-band」描画は
  TRIM=crop / STRETCH=伸縮 の**両方で正しく**なる（描画コード変更ほぼ不要）。
- MIDI: STRETCH はモデル scale なので mini-view / piano roll は自動追随。

---

## 3. 実装ファイル（予定）
- `common/src/model.rs`: TRIM/STRETCH ヘルパ（`stretch_event`, `stretch_notes`）追加、
  右端 trim の source 連動。
- `common/src/audio_render.rs`: `pitch_ratio_for` に stretch 合成 or 別 helper。
- `daw_audio/src/audio_clip_renderer.rs`: `RenderedEvent.stretch_ratio` 追加、
  compile で算出、render の Raw/Repitch/Stretch/Slice 各経路に合成。
- `daw_gui/src/app.rs`: `resize_clip` を TRIM/STRETCH 分岐（audio 右端 source 連動 +
  MIDI note scale + linked fork）。新 `AppEvent::StretchClip`（or `ResizeClip` に
  `mode: Trim|Stretch` 付与）。undo 登録。
- `daw_gui/src/view/arrangement_view.rs`: stretch ジェスチャの dispatch、カーソル/ゴースト。
- `ui/crates/ui/src/widgets/arrangement.rs`: resize ゾーンで Shift = STRETCH を許可する
  ゲート拡張、`ResizeClipDelta` に stretch フラグ、ドラッグプレビュー。
- `daw_gui/src/view/audio_editor.rs`: event グリップにも Shift = STRETCH（任意、対称化）。
- IPC: 新メッセージ不要（`LoadSong(Song)` で全部伝播、bincode）。
  ※ AudioEvent に**新フィールドは追加しない**ので protocol rebuild も最小。
  万一フィールド追加するなら `cargo build --workspace` 必須。

---

## 4. 上流決定事項（確定済）

### Q1 ストレッチの操作方式 → **Shift + 端 drag = ストレッチ**（plain = トリム）
Ableton 流。daw_01 の既存 modifier（Alt=snap無効 / Ctrl=共有コピー / Ctrl+Shift=独立
コピー）と直交し衝突なし。Shift+Alt = snap 無効ストレッチも自然に表現。
widget の resize ゾーン限定で Shift を許可（clip 本体の Shift は従来どおり選択へ fall through）。

### Q2 オーディオ ストレッチの既定アルゴリズム → **ピッチ保持（Stretch / granular）**
Raw クリップを Shift ドラッグすると `Stretch`（pitch 保持 granular）へ自動昇格。テープ式
（Repitch = 速度に伴い音程も変化）は inspector の stretch-mode セッタで切替。

## 5. スコープ外（明示）
- **Audio Editor（§3.10）の event グリップ Shift=ストレッチ**: 本対応は arrangement の
  clip 端 drag（#61 の主対象「クリップの長さを変える」）に集中。Audio Editor は per-event 詳細
  編集の副 surface で、別経路 `SetAudioEventTrim` を持つ。対称化は follow-up（KISS で今回見送り）。
- ~~**granular の source SR ≠ engine SR 補正**: 既存実装が granular で sr_factor 未適用（48k 前提）。
  pre-existing で #61 と独立。stretch_ratio は秒基準で SR 非依存に算出済。~~
  → **2026-08-01 修正済**。「スコープ外」に置いたまま残した結果、44.1 kHz 素材 / 48 kHz エンジンで
  Stretch（granular）/ Slice が source を 8.8% 速く消費し、**クリップ末尾 8.1% が無音 + ピッチが
  約 1.5 半音上ずる**実害になっていた（= 「波形より音が短い」）。granular / slice の
  「出力 sample → source frame」換算すべてに `native_stride = source_sr / engine_sr` を掛け、
  小数位置は linear interpolation で読むよう修正（`daw_audio/src/audio_clip_renderer.rs` の
  `source_frame_lerp` / `granular_sample_at` / `slice_sample_at`）。回帰テストは
  `stretch_sample_rate_tests`。新規 audio event の既定 mode は `Stretch`（`AudioEvent::default`）
  なので、Shift ストレッチしていない読み込み直後のクリップも同じ影響を受けていた。
- **stretch ドラッグ中の専用カーソル / ゴースト波形プレビュー**: commit 後の波形＋音が一致する
  ことが検証になるため未実装（polish）。

## 6. 時間軸とピッチ軸の分離（2026-08-01）

上記 sr_factor 修正と同じ根 — 「1 output frame が source の何 frame か」を **1 個の
`pitch_ratio_for(mode, ...)`** に mode 分岐込みで押し込んでいたため、Raw / Stretch / Slice で
ピッチ比が捨てられ、**インスペクタのピッチ（semitones）が既定モードで無反応**だった
（効いていたのは Repitch のみ）。直交する 2 量に分離した:

| 量 | 定義 | 掛かる場所 |
|---|---|---|
| `sample_rate_ratio` | `source_sr / engine_sr` | 出力 sample → source frame の**時間写像**すべて（Raw/Repitch の stride、granular の grain **配置**、slice の trigger 写像） |
| `pitch_factor` | `2^(semitones/12)` | source を**読む速度**（`read_stride = sr_ratio × pitch_factor`） |

mode ごとの合成は render loop が持つ:
- **Raw**: stride = `read_stride`（tempo/stretch 非追従、ピッチはテープ式 = Ableton Warp-off + Transpose 相当）
- **Repitch**: stride = `read_stride × tempo追従`（従来どおり）
- **Stretch**: r.md #40 でスペクトル方式に置換。 時間写像は beat 領域
  （`source_sr × 60 / nominal_bpm × stretch_ratio` = tempo 非依存）、移調は半音値を
  エンジンへ直接渡す → **長さを変えずに移調**し、さらにフォルマントも独立（§7）
- **Slice**: trigger 写像 = `time_stride × stretch × tempo`、slice 内読み = `read_stride`
  → trigger グリッドは動かず slice の鳴る長さだけ変わる（Ableton Beats mode の Transpose 相当）

回帰テスト: `pitch_shift_keeps_length_in_stretch_mode` / `pitch_shift_moves_the_pitch_in_stretch_mode` /
`pitch_scales_playback_rate_in_tape_and_slice_modes`。

## 7. フォルマント (r.md #40) — Stretch のスペクトル化

### 7.1 なぜ granular を捨てたか

固定 hop の granular OLA は、grain の**配置**が長さを、grain 内部の**読み速度**が
音程を決める。読み速度を変えるとスペクトル全体（倍音列 *と* その包絡 =
フォルマント）が同率で写るので、「ピッチを上げるとフォルマントも必ず一緒に上がる」
= チップマンク化はアルゴリズムの定義そのもので、パラメータでは外せない。
フォルマントを音程から外すには周波数軸で包絡を別に写す必要があり、Stretch の DSP
ごと差し替えるのが唯一の道だった。

### 7.2 採用エンジン

**Signalsmith Stretch**（MIT、header-only C++）を `signalsmith-sys/vendor/` に
取り込み、C ABI shim 経由で使う（`signalsmith-sys/VENDOR.md` に由来と更新手順）。
Qt 6.10 の QMediaPlayer が採用している実装で、`setFormantSemitones(s,
compensatePitch)` を本体 API に持ち、`process()` が確保をしない（= RT 適合）。
自前の位相ボコーダ / PSOLA は品質（包絡推定・位相ロックの再発明）で劣り、
Rubber Band は GPL で不可、オフライン事前ベイクは将来の「時間変化する formant /
pitch の point 列」を不可能にするので不可。

### 7.3 モード別の意味論

| mode | pitch の効き | `formant = 0` の意味 | `formant = F` |
|---|---|---|---|
| Stretch（スペクトル） | 長さを変えずに移調 | **原音のフォルマントを保持** (`compensatePitch=true`) | 原音の包絡を F 半音移動 |
| Raw / Repitch（テープ） | 速度 = ピッチ | **完全バイパス**（出力が 1 サンプルも変わらない） | テープ結果の包絡をさらに F 半音移動 |
| Slice | slice 内テープ | 同上 | 同上 |

Stretch だけ「0 = 保持」なのは、スペクトル方式の定義が音程と包絡の分離だから
（Ableton Complex Pro の Formants=100%、Bitwig Elastique Pro、Cubase VariAudio、
Melodyne と同じ流儀）。テープ系で「0 = 未処理」なのは Repitch の存在意義が
テープ挙動そのものだから（Ableton Re-Pitch にフォルマント制御が無いのと同じ理由）。
範囲は ±48 半音（`common::model::FORMANT_SEMITONES_LIMIT`）。

### 7.4 時間写像を beat 領域へ

スペクトル経路の source 進度は「1 拍あたり消費する source frame 数」
`source_sr × 60 / nominal_bpm × stretch_ratio` で持つ。この量は **tempo に依らない**
ので、tempo automation でも source 位置が跳ばず拍にロックしたまま追従する。
これにより旧実装の grain-trigger lock-in ring（E5）と LP smoothed bpm
（`GRANULAR_LP_COEF`）は不要になり、両方とも撤去した。

### 7.5 エンジンの所有と RT 契約

- 1 発音 = 1 エンジン。`StretchEngine::new` だけが確保する（内部で white-noise
  warm-up を回し、C++ 側の `std::vector` 高水位を **off-RT で**確定させる）。
- 必要数は compile 時に **区間グラフの貪欲彩色**で出す（`assign_engine_slots`）＝
  track ごとの最大同時発音数。1 個 ~1 MB なので「track 内 event 数」で確保すると破綻する。
- off-thread の `publish_audio_clip_schedule` が不足分を作って ring で RT へ送り、
  RT は `TrackScratch::stretch_engines` へ `push` するだけ（容量予約済 = 再確保なし）。
  **pool → schedule の順**で publish するので、RT が新 schedule を見るときには
  エンジンが揃っている。pool は grow-only（縮めると走行中の発音まで prime し直しになる）。
- ストリーム同一性は 3 点で判定する: `stream_key`（安定 clip id + audio event id）/
  「次に出る `event_local`」/ **`u` 座標系そのもの**（`du` と `u_of` の現在地）。
  3 点目が要るのは、Stretch 経路（`u` = 絶対 source frame）と tape/slice 経路
  （`u` = event-local sample）で座標系が別物だから。再生中に stretch mode を
  切り替えると `key` も `el` も連続なのに `cursor_u` だけ旧空間に残り、
  数秒間フリーズしたドローンになる。ズレたら `sms_output_seek` で詰め直す。
  素材は全部メモリ上にあるので、出力位置より `input_latency + output_latency` ぶん
  先の入力を先読みして食わせられる = **実効レイテンシ 0**。
- エンジンの引き当ては **`stream_key` による安定マップ**（`acquire_engine`）。
  貪欲彩色の色番号を pool の位置として使うと、無関係な clip の追加/削除で色が
  玉突きし、発音中の clip が別エンジンへ移って `sms_output_seek` の内部 `reset`
  （= OLA テール破棄 = クリック）が起きる（アーキ不変条件 #1）。彩色は
  **必要数の計画にだけ**使う（`count_engines_per_track`）。
- 乱数位置は発音の頭で必ず巻き戻す（`sms_reseed`）。これが無いと pool の使用履歴で
  位相スメアが変わり、live と export が食い違う（`signalsmith-sys/VENDOR.md`）。

### 7.6 RT 無確保の機械検査

`make test-rt`（= `cargo test -p daw_audio --features rt-assert`）が
**Rust 側と C++ 側を別々に**検査する。Rust の `#[global_allocator]` フック
（`assert_no_alloc`）は C++ の確保を一切見られない（CRT へ直行する）ので、
`signalsmith-sys/alloc-count` が global `operator new` を置換して数える方も要る。
`make test` から呼ばれるので既定のワークフローに乗る。

## 8. 波形描画 = `event_wave_spans` を SSoT に（2026-08-09, r.md #41）

§6 で確立した写像は **再生側だけ**の SSoT で、描画側は「1 event = 1 連続レンジ」を返す
`audible_source_span` を別に持っていた。Slice はその関数で必ず `rate = stretch` を使い
`窓 / (source_fpb × stretch) == len_beats` が恒等成立するため、**常に「窓全体を clip 幅
いっぱいに引き伸ばした連続波形」に退化**していた（= r.md #41「スライスの配置ではなく
連続波形のまま」の直接原因）。10053df のコミットメッセージが「別件」としていた項目。

`audible_source_span` を廃止し、`common::audio_render::event_wave_spans` に置き換えた。
戻り値は **span 列**（`WaveSpan { start_beat, end_beat, source_start, source_end, reversed }`）で、
「出力の event-local 拍区間 → source frame 範囲」の区分線形写像を並べたもの。
engine SR / current bpm は約分されて消えるので、GUI が `song.bpm` と source SR だけで
engine と同じ写像を再現できる。

| mode | span |
|---|---|
| `Raw` | 1 本。1 拍 = `source_fpb × pitch` frame |
| `Repitch` | 1 本。1 拍 = `source_fpb × stretch × pitch` frame |
| `Stretch` | warp marker ≥ 2 なら marker 境界で区切った区分線形（`warp_source_frame`）、無ければ 1 本で `source_fpb × stretch`。#40 のスペクトル経路 `u_of`（= `beat × src_frames_per_beat`）と同一量で、engine 不在時の degrade 経路（`tape_ratio = time_stride × follow_instant`）も拍領域では同じ |
| `Slice` | onset ごとに 1 本 |

Slice の式（`source_fpb = source_sr × 60 / bpm`、`place_fpb = source_fpb × stretch`
（恒等的に `窓 / clip 長`）、`read_fpb = source_fpb × pitch`）:

- trigger 拍 `start_beat_i = onsets[i] / place_fpb`
- 鳴る拍数 `= (min(onsets[i+1], 窓) − onsets[i]) / read_fpb`（slice 本体は伸縮しない）
- `end_beat_i = min(start_beat_i + 鳴る拍数, start_beat_{i+1}, clip 長)`
- ⇒ `stretch < pitch` で **gap**（無音・フェード無しのハードカット）、`stretch > pitch` で **cut**。
  伸縮もピッチ変更もしていない取り込み直後は隙間ゼロで連続波形と一致する

どの mode でも「窓を鳴らし切って余った」区間には span を張らない（= 無音）。`reversed` は
窓全体を反転して読む（`source_frame_lerp` と同じく slice 単位ではない）ので、span の source
範囲を窓の反対側へ写して `reversed` を立てる。

描画側は mode 分岐を持たない:
- daw-ui `Ui::waveform_segments`（1 id・1 LOD ピラミッド・複数区間、`WaveformView.reversed`、
  scissor による区間カリング）。`Ui::waveform` はその 1 区間ラッパ。
- アレンジビュー `draw_clip_waveform_inner`: clip 拍 → x の線形写像 1 本 + 全 event の span を
  そのまま並べる（旧: 先頭 event 1 件のみ + `audible_frac` で幅を縮める）。gap には薄い中央線、
  Slice はスライス頭に区切り線。
- オーディオエディタ: 旧 `markers.len() >= 2` 分岐を撤去（再生が marker を無視する
  Raw / Repitch / Slice でも warp 形状を描いていた同種のバグ）。Slice は transient マーカーを描く。

**束縛テスト**: `daw_audio` の `wave_span_binding_tests` が ramp 素材を実レンダリングし、
「span 区間は span の source 写像どおりに鳴る / span の無い区間は完全無音」を assert する。
片方の写像だけ変えると CI が落ちる（従来はコメントでしか結び付いていなかった）。
Raw / Repitch / Stretch（uniform + warp）/ Slice / reversed / SongTempo automation を網羅する。

### 8.1 tempo は `TempoMap` 経由（スカラー bpm では表せない）

engine は buffer ごとに `evaluate_song_tempo(song, playhead_beats)` で `current_bpm` を
評価し `samples_per_beat` を作り直す。式を展開すると：

| 量 | current_bpm 依存 | 理由 |
|---|---|---|
| Slice の trigger 拍 / Stretch のスペクトル写像 / Repitch | **不変** | Stretch は `u_of` が **beat 領域**（`beat × src_frames_per_beat`、#40 §7.4）で tempo に依らない。Slice trigger / Repitch は `tempo_follow_ratio` の `current/nominal` が `samples_per_beat` と約分される |
| **Raw の消費速度 / Slice 本体の read 速度** | **反比例** | `source_pos = event_local × read_stride` で `event_local` が `samples_per_beat` 由来 |

つまり native rate 再生（Raw 全体・slice 本体）だけが「1 拍あたりの source 消費量」を
tempo に応じて変える。描画が定数 `song.bpm` を使っていると、SongTempo lane を持つ曲で
60 BPM 区間の Raw クリップが実音の 2 倍幅に描かれる（= #41 の不変条件の破れ）。
そのため `event_wave_spans` はスカラー bpm ではなく **`TempoMap`**（`nominal_bpm` +
SongTempo lane 参照）と **event の song 絶対拍**を取る。tempo が曲線のときは native rate
区間を 1/8 拍刻みの区分線形 span 列に分割する（2 本目以降は `WaveSpan.head = false` で、
スライス境界の縦線は `head` の span にだけ出す）。SongTempo lane が無い曲は
`TempoMap::is_constant()` で従来どおり閉形式 1 span（曲線評価コストゼロ）。

既知の限界: engine は automation の上に song modulation（LFO/MSEG → `SongTempo`）を
重ねるが、modulator の位相は audio thread が持つので GUI からは再現できない。変調中の
tempo は automation 値で近似する。

### 8.2 その他の一致条件

- **warp が窓外を指す区間は無音**（#40 で意味論が変わった）: spectral 経路の `u_of` は
  `warp_source_frame(beat) − source_start_frames` を **clamp せずに** `source_frame_lerp`
  へ渡すので、`u < 0` / `u >= 窓` は `None` = 無音。描画も span を張らない。
  旧 granular は grain ごとに `.max(0)` していたため窓手前は「先頭 frame を保持（flat）」で、
  描画側もそれに合わせて交点で分割していた。#40 の置き換えでこの clamp が消えたので、
  flat 区間を作ると**無音のはずの場所に波形が出る**。auto-warp 後に左 trim した event
  （marker 据え置きで `source_start_frames` だけ前進）で実際に到達する。
- **warp marker は forward ドメイン**: `WaveSpan` の source 範囲は「実際に鳴る」
  （逆再生なら反転後）座標だが、marker は engine が反転前の座標で解釈し
  `source_frame_lerp` が改めて反転する。Alt+click で marker を置く UI は
  `source_frame_at_beat` の戻り値を forward に戻してから保存する。

未実装（Ableton にはあるが daw_01 に無い）: Transient Loop Mode（gap を loop で埋める）、
Transient Envelope（slice 境界のフェード）。現状 gap 境界は無フェードのハードカットなので、
素材によっては境界でクリックノイズが出る（`slice_sample_at` が 0.0 を返す）。
