# r.md #75 — VOICEVOX 合成の高速化 (塊クエリ + フレーズ合成)

> この計画は **#75 専用**であり、他項目との統合順は `docs/plan_rmd_index.md` を見ること。

本書は **実装計画の正本**。この 1 本だけを読んで完走できる密度で書く。
既存の doc コメント密度・命名・日本語コメントの流儀に合わせること (`common/src/voicevox.rs` /
`daw_plugin_host/src/builtin/voicevox.rs` が手本)。フェーズ分けはしない。**ここに書いた全部を
1 回で最終形まで実装する。**

---

## 0. 現状と、なぜ遅いのか

歌唱合成は **トラック内・同一 speaker の全 note を 1 本にまとめた 1 query** で行われている。

- `daw_gui/src/handler/voicevox.rs:335-360` — トラック全 clip の note を 1 配列へ flatten し、
  `note_id` に **`entries.len()` (= トラック内通し index)** を振る。
- `daw_plugin_host/src/builtin/voicevox.rs:790-812` — 受け取った entries を **speaker でだけ**
  グルーピング (`clip_id` は使わない = dead field)。
- `daw_plugin_host/src/builtin/voicevox_synth.rs:210/215` — `build_sing_query` → `key_for_sing`
  (= **query JSON 全体のハッシュ**) → `/sing_frame_audio_query` + `/frame_synthesis`。

結果、**1 ノート直すと曲全体のキーが変わり、曲全体を HTTP 再合成する**。

### 実測 (2026-08-28、実 engine・ユーザー機。推測ではない)

計測スクリプトの出力そのもの。数値は本計画の設計根拠なので、判断に迷ったらこの表に戻ること。

**(A) 1 クエリ + 1 合成 のスケール** (bpm 120 / 歌唱 note びっしり)

| 曲長 | frames | `/sing_frame_audio_query` | `/frame_synthesis` | 計 |
|---|---|---|---|---|
| 28.1 s | 2,631 | 0.23 s | 1.33 s | 1.55 s |
| 57.0 s | 5,315 | 0.51 s | 2.37 s | 2.88 s |
| 113.8 s | 10,658 | 1.55 s | 9.27 s | 10.82 s |
| 171.2 s | 15,979 | 3.36 s | 13.11 s | 16.47 s |
| 301 s | 28,157 | 15.08 s | **HTTP 500 で失敗** | — |
| 361 s | 33,781 | 18.40 s | (未実行) | — |
| 480 s | — | **RAM 枯渇で engine が落ちる** | — | — |

- **重いのは合成側**。クエリも合成も **二次で伸びる**。
- **301 秒では 1 回の `/frame_synthesis` が通らない**(起動直後のクリーンな engine で再現。
  GPU メモリ由来)。今の実装は長い曲では**原理的に破綻している**。

**(B) 同じ 301 秒の曲を、同じクエリから フレーズ単位に切り出して合成**

> 211 フレーズ 成功 211 / 失敗 0、合計 30.68 s (1 フレーズ 平均 145 ms / 最大 546 ms)

**分割合成は長さの限界を構造的に外す。**

**(C) クエリを分割すると音量が崩れる** (181 秒 / 127 フレーズ。フレーズごとの音量を
「全体 1 クエリ」基準で比べたときのばらつき)

| クエリ単位 | 全体ばらつき | σ | 塊間の段差 | クエリ計 |
|---|---|---|---|---|
| 全体 1 回を再実行 (= ノイズ下限) | 0.86 dB | 0.13 | 0.00 dB | 0.00 s / 1 回 |
| 30 秒の塊 (6 塊) | 4.12 dB | 0.64 | 0.64 dB | 1.16 s |
| **60 秒の塊 (3 塊)** | **2.30 dB** | **0.41** | **0.07 dB** | **1.64 s** |
| 120 秒の塊 (2 塊) | 2.33 dB | 0.37 | 0.42 dB | 2.26 s |

**60 秒が曲がり角**。30→60 で半減し、60→120 では改善せずクエリ時間だけ 38% 増える。
**塊間の段差はもともと小さい (0.07〜0.64 dB) ので、塊を重ねて段差を打ち消す仕組みは作らない。**

**(D) 切り出し合成のパディング** (57 秒 / 40 フレーズ。全体クエリ 1 本から切り出して合成し、
未編集フレーズが「全体合成」とどれだけずれるか)

| pad | ばらつき | σ | 最大ずれ | 1 フレーズあたり synth |
|---|---|---|---|---|
| 0.12 s | 3.70 dB | 0.92 | 2.88 dB | 129 ms |
| **0.50 s** | **1.31 dB** | **0.31** | **0.67 dB** | **159 ms** |
| 1.00 s | 2.43 dB | 0.42 | 1.60 dB | 166 ms |
| 2.00 s | 1.84 dB | 0.35 | 1.15 dB | 208 ms |
| 4.00 s | 0.92 dB | 0.22 | 0.56 dB | 241 ms |

**0.5 秒が最良コスパ**。1.0 s は 0.5 s より悪い (非単調) ので「長ければ良い」ではない。

**(E) 決定性**

- `/frame_synthesis` は **決定的** (同じ FrameAudioQuery を 2 回投げると bit 一致、0.00 dB)。
- `/sing_frame_audio_query` は **非決定的** (同じ楽譜を 2 回投げて max|Δf0| = 31.7 Hz、
  RMS 換算 0.86 dB)。
- ⇒ **キャッシュキーは必ず「入力 (楽譜)」から作る。クエリ出力のスライスをキーにしない**
  (毎回 miss する)。

**(F) 補正は効かない** — 塊ごとの一定ゲイン / 固定の校正フレーズ / 編集前の値、
3 通りとも失敗した。文脈が変わると音量は「一定倍」ではなく**包絡の形ごと**変わるため、
掛け算では戻せない。**補正は実装しない。**

**(G) 1 ノート編集のコスト (57 秒の曲での実測)**

> 現行: 1.86 s (query 0.49 + 全体 synth 1.33)
> 新方式: **0.62 s** (query 0.49 + 1 フレーズ synth 0.131)

これは曲が長いほど差が開く (現行は二次、新方式は「その塊 + 1 フレーズ」で **曲長に依らない**)。

---

## 1. 最終形

```
NoteMetadata[]  (daw_gui → plugin host、note_id は (clip_id, note.id) 由来の安定 id)
      │
      ├─ split_into_phrases()   フレーズ = 隙間ゼロで続く note の極大列 (同一 speaker)
      │                          クリップ境界では切らない
      ├─ group_into_chunks()    塊 = 連続する複数フレーズ。既定 60 秒。
      │                          切れ目は窓の中で最も長い休符に置く
      │
      ├─ フレーズ WAV キャッシュ (キー = フレーズ自身の楽譜 + 声 + 塊の長さ)
      │      hit → HTTP なし
      │      miss → そのフレーズが属する塊だけ:
      │               ├─ 塊クエリキャッシュ (キー = 塊の楽譜)
      │               │     hit → HTTP なし
      │               │     miss → POST /sing_frame_audio_query  (塊 1 回)
      │               │            塊の楽譜は **各フレーズの単体 query と 1 frame も
      │               │            ずれない格子**で組む (build_chunk_query、§3.2)
      │               └─ FrameAudioQuery を [phrase±0.5s] で切り出して
      │                  POST /frame_synthesis  (フレーズ 1 回)
      │
      └─ 休符の中点を継ぎ目に敷き詰め (5 ms クロスフェード) → song-absolute な 1 本の buffer
                                                              → ArcSwapOption に store
                                                                (旧 buffer は RT の quiesce を
                                                                 確認してから writer が回収)
```

- **フレーズが最小単位。ノート単位には割らない** — VOICEVOX の sing は consonant_length / f0 /
  volume をいずれも **note 列・frame 列全体**から予測する
  (`voicevox_engine/tts_pipeline/song_engine.py` の `create_phoneme_and_f0_and_volume`)。
  レガートの途中で切ると音が変わる。本家エディタもフレーズが最小単位
  (`voicevox/src/sing/songTrackRendering.ts` の `extractPhraseNotes`:
  `currentNoteEndPos !== nextNote.position` で切る)。
- **休符が無い長い区間の強制分割はしない** (本家もしない)。そこは 1 フレーズとして扱う。
  ただし塊は「1 フレーズより細かくはならない」ので、超長フレーズ 1 本が塊になるだけで壊れない。
- **クリップ境界では切らない** — クリップは content への窓にすぎず音楽的な切れ目と一致しない。
  過去に clip 単位分割で失敗している (`git show 4ebcad6`)。
- 音量補正・末尾 pau のミュート (`muteLastPauSection` 相当)・可変先頭休符は **実装しない**
  (§0 (F) と、`REST_FRAMES` を動かすと口パク配置 SSoT が連鎖するため)。

---

## 2. 触るファイル一覧 (repo 相対)

| ファイル | 何をするか |
|---|---|
| `common/src/voicevox.rs` | `SingQuery` を `NotePlacement` (start/end frame) 化、`build_sing_query` を **配置 (`place_sing_notes`) と JSON 生成 (`emit_sing_query`) に 2 分割**、`build_sing_query_with` (持ち越し母音) / `carry_vowel_after` / `normalize_frame_query` / `OUTPUT_SAMPLE_RATE` を公開 |
| `common/src/voicevox_phrase.rs` | **新規**。フレーズ分割 + 塊グルーピング + **塊クエリ生成 (`build_chunk_query`)** (純粋関数) |
| `common/src/voicevox_cache.rs` | **新規 (daw_plugin_host から移設)**。WAV/JSON 両対応、LRU、prune の予算修正 |
| `common/src/plugin_metadata.rs` | `sing_note_id()` 追加、`TalkMetadata.clip_id` 追加、`note_id` / `clip_id` / `event_id` / `TALK_EVENT_ID_BASE` の嘘 doc を訂正 |
| `common/src/process_data.rs` | `TimedEvent.note_id` doc の「(= first note in the track)」を訂正 (§3.4(c)) |
| `common/src/protocol.rs` | `SetBuiltinPluginNoteMetadata` に `chunk_secs`、`SetVocalSynthPriority` 新設、`VocalSynthProgress` 新設、`VoicevoxSynthStatus` を進捗つきに |
| `common/src/lib.rs` | 新規 module の宣言 |
| `common/build.rs` | (確認のみ。§3.6 参照) |
| `daw_plugin_host/src/builtin/voicevox_synth.rs` | HTTP を 2 段に分解、FrameAudioQuery のスライス、`synthesize_notes_for_builtin` 撤去、`OUTPUT_SAMPLE_RATE` / sample rate 正規化を common へ移設 |
| `daw_plugin_host/src/builtin/voicevox_render.rs` | **新規**。塊クエリ → フレーズ合成 → 継ぎ目 → 差分 mix → publish のオーケストレーション |
| `daw_plugin_host/src/builtin/voicevox_cache.rs` | **削除** (common へ移設) |
| `daw_plugin_host/src/builtin.rs` | `mod voicevox_cache;` (:35) の削除。`load_builtin` (:46-60) 自体は `callbacks.on_vocal_synth_status.clone()` を渡すだけなので **型が変わってもコード変更不要** |
| `daw_plugin_host/src/builtin/voicevox.rs` | `SynthJob` をフレーズ方式へ、synth thread を `voicevox_render` に委譲、優先度 / 進捗 / heartbeat、`StatusFn` (:49-51) の型と doc、`synth_report` (:468-479) の dedup 対象 |
| `daw_plugin_host/src/plugin_instance.rs` | `VocalSynth` trait に `set_priority_beats`、`synth_progress` に heartbeat 追加、`HostCallbacks::on_vocal_synth_status` の引数を `VocalSynthProgress` へ |
| `daw_plugin_host/src/main.rs` | 新 command の配線、`prepare_vocal_synth` の deadline を heartbeat ベースへ |
| `daw_gui/src/handler/voicevox.rs` | `note_id` を安定 id へ、`chunk_secs` 送出、進捗 API、`all_vocal_synth_device_ids`、口パク側は不変 |
| `daw_gui/src/handler/ipc.rs` | `VoicevoxSynthStatus` の新 payload 受理、`VocalSynthReady` を書き出しゲートにも配る |
| `daw_gui/src/handler/tick.rs` | 再生ヘッド優先ヒントの送出、書き出し watchdog の除外条件 |
| `daw_gui/src/handler/export.rs` | WAV 書き出しの合成完了ゲート (`PrepareVocalSynth` → 全 `VocalSynthReady` → `ReinitAllPlugins`) |
| `daw_gui/src/handler/automation_lanes.rs` | `handle_child_disconnected` に書き出しゲートの脱出口 |
| `daw_gui/src/handler/project.rs` | `persist_app_config` の網羅 struct literal に `voicevox_chunk_secs` |
| `daw_gui/src/state/ipc.rs` | `pending_vocal_synth_export` (書き出しゲートの待ち集合) |
| `daw_gui/src/state/voicevox.rs` | `voicevox_metadata_sent` のタプルに `chunk_secs`、優先度送出の記憶 |
| `daw_gui/src/state/ui_prefs.rs` | `voicevox_chunk_secs` フィールド |
| `daw_gui/src/app_types.rs` | `VocalSynthStatus` に進捗フィールド |
| `daw_gui/src/app_config.rs` | `voicevox_chunk_secs` 設定 + `save_load_roundtrip` テスト更新 (網羅 literal) |
| `daw_gui/src/view/settings.rs` | 「VOICEVOX」セクション (塊の長さ) |
| `daw_gui/src/view/voicevox_overlay.rs` | 「残り N トラック」→「残り N フレーズ」 |
| `daw_gui/src/view/arrangement_view.rs` | クリップ上スピナーをクリップ単位判定へ |
| `daw_gui/src/voicevox_client.rs` | 口パク query のディスクキャッシュ (正規化してから put) |
| `daw_gui/src/event.rs`, `daw_gui/src/app.rs` | 設定変更イベントの追加配線 |
| `daw_audio/src/sequencer.rs` | `note_id` を `sing_note_id()` へ、通し index の bookkeeping を全撤去、module doc 訂正 |
| `daw_audio/src/engine.rs` | `PREVIEW_NOTE_ID` doc の「通し index」記述を訂正 (:46-48) |
| `daw_gui/tests/voicevox_progress.rs` | `VocalSynthStatus` の新 payload、`voicevox_synth_busy_count` 廃止に伴う書き換え (§7.2) |
| `daw_gui/tests/app_state/state_roundtrip_watchdog.rs` | `PluginEvent::VoicevoxSynthStatus` の**網羅リテラル** (:345-349) を `{ device_id, progress }` へ (下記) |
| `docs/plan_voicevox_synth.md` / `docs/plan_voicevox_progress.md` / `docs/plan_voicevox_clip_voice.md` / `docs/plan_pakupaku.md` | 古くなる記述の訂正 |

**`daw_gui/tests/app_state/` を落とさないこと。** `state_roundtrip_watchdog.rs:345-349` が

```rust
    app.handle_event(AppEvent::Plugin(PluginEvent::VoicevoxSynthStatus {
        device_id: 7,
        busy: true,
        failure: VocalSynthFailure::None,
    }));
```

という網羅リテラルを持つ。`daw_gui/tests/app_state/` 配下に `CARGO_BIN_EXE_daw_gui` は
1 件も無い (grep 済) ので、`Makefile:65-69` (`DAW_GUI_SAFE_TESTS +=`) の
**ディレクトリ形式 target 収集**が
`--test app_state` を `DAW_GUI_SAFE_TESTS` に入れる = **`make test-nolaunch` の対象**。
`make clippy` も `--workspace --all-targets` なので、ここを直さないと §7.1 の
受け入れコマンドが 2 つとも落ちる。書き換えは 1 行:
`progress: VocalSynthProgress { busy: true, ..Default::default() }`。
`assert!(app.voicevox.voicevox_synth_status.contains_key(&7))` (:351) はそのまま通る。

**変更不要と確認済み** (無言の欠落にしないため明記する。根拠は §5.4 / §3.4(c)):
`daw_gui/src/handler/glue.rs` (既に merged content の note id を採番し直している :358-360)、
`daw_gui/src/handler/clips.rs` (Split)、`daw_gui/src/midi_import.rs` (:768)、
`daw_gui/src/handler/media.rs` (:827-829)、`daw_plugin_host/src/clap_plugin.rs` (:1655-1656 /
:1659-1660)、`daw_plugin_host/src/vst3_plugin.rs` (:1762-1764 / :1790-1792)。

---

## 3. `common` の変更

### 3.1 `common/src/voicevox.rs`

`REST_FRAMES` (:41) / `sing_base_beat` (:120) / `sing_head_beat` (:133) は **変えない**。
口パク配置 (`common/src/lipsync.rs:74-82` の `first_phoneme_local_beat` 契約) が
これらに乗っているため、動かすと音声と口のズレとして即座に出る。

#### (a) `SingQuery` を「終端 frame も返す」形へ

現在 (doc :96-101 / derive :102 / struct :103-108):

```rust
pub struct SingQuery {
    pub json: String,
    pub note_frames: Vec<(usize, i64)>,   // ← 開始 frame しか無い
}
```

これでは **フレーズの終端 frame が取れない**ので、切り出し窓を決められない。次に置き換える
(**既存の `#[derive(Debug, Clone, PartialEq, Eq)]` (:102) を落とさないこと** — `mod tests` の
`assert_eq!` が依存している):

```rust
/// 重なり解決後に query へ載る note 1 件の **絶対 frame 位置**
/// (query 先頭 = frame 0、先頭 rest 込み)。プロセス内の計算専用で IPC を渡らない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotePlacement {
    /// 入力 `notes` 内の index。
    pub index: usize,
    /// 音が始まる絶対 frame。
    pub start_frame: i64,
    /// 音が終わる絶対 frame (排他)。重なり切り詰め後の実値。
    pub end_frame: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingQuery {
    /// `POST /sing_frame_audio_query` に渡す JSON body。
    pub json: String,
    /// query 内の出現順 (= start_frame 昇順)。歌詞・長さ・重なりの都合で
    /// query に載らなかった note は含まれない。
    pub notes: Vec<NotePlacement>,
    /// 末尾まで歌詞解決したあとの「持ち越し母音」。長音符「ー」を
    /// フレーズ / 塊をまたいで解決するため、次の query 構築へ引き渡す。
    pub carry_out: Option<char>,
}
```

`SingQuery.note_frames` の全参照 (repo 全体を grep 済、下記が全件) を書き換える:
- `daw_plugin_host/src/builtin/voicevox_synth.rs:233` (コメント) / `:237-238`
  (削除される `synthesize_notes_for_builtin` の中なので実質不要)
- `common/src/voicevox.rs` の tests (:636, :637, :652, :660, :670, :684, :696, :708) —
  `vec![(0, 10), (1, 57), ...]` を `NotePlacement` 比較へ書き換える。**開始 frame の期待値は
  一切変えない** (回帰の証拠として残す)。end_frame の assertion を各テストに 1 つ足す。

#### (b) `build_sing_query` を「配置」と「JSON 生成」に割る

`build_sing_query` (:165-252) は現在 1 関数の中で
「sort → `pos_of` → `kept` の重なり解決」→「rest 挿入 + 歌詞解決 + JSON 文字列化」
を続けてやっている。これを **2 本に割る**。ロジックは 1 文字も変えない (`saturating_*` も維持)。

```rust
/// notes を start_beat 昇順に整列し、Reaper 流の重なり解決 (前の note を次の開始で
/// 切り詰める / 切り詰めで 1 frame 未満に潰れたら落とす、押し出しはしない) を掛けて
/// **絶対 frame 位置**を確定する。`edge_rest_frames` は query 先頭に置く無音の長さ
/// (= 全 placement に一律で足されるオフセット。隙間の有無には影響しない)。
///
/// [`build_sing_query_with`] / [`crate::voicevox_phrase::split_into_phrases`] /
/// [`crate::voicevox_phrase::build_chunk_query`] の共通前段。3 者が同じ
/// 「どの note が載るか」「どこで隙間ゼロか」を見るための SSoT。
#[must_use]
pub fn place_sing_notes(notes: &[Note], bpm: f32, edge_rest_frames: i64) -> Vec<NotePlacement>;

/// 確定済み placement 列から `/sing_frame_audio_query` の body を組む
/// (前後の端 rest + 隙間 rest の挿入、長音符「ー」の解決、JSON 文字列化)。
///
/// `placements` は **`start_frame` 昇順・区間が重ならない**こと。`index` は `notes` への
/// index で、**`notes` の並びが `placements` の順である必要は無い**
/// (塊 query は複数フレーズの placement を連結して渡す。下記 (b2))。
///
/// 戻り値の [`SingQuery::carry_out`] は末尾時点の持ち越し母音。
pub(crate) fn emit_sing_query(
    notes: &[Note],
    placements: &[NotePlacement],
    edge_rest_frames: i64,
    carry_in: Option<char>,
) -> SingQuery;

/// 単体 query の全部入り builder。[`build_sing_query`] は
/// `build_sing_query_with(notes, bpm, None)` に委譲する (**実装は 1 本だけ**)。
/// 端 rest は常に [`REST_FRAMES`] — 口パク配置 SSoT (`sing_head_beat`) がこの値に
/// 乗っているので、単体 query 側では**可変にしない**。
#[must_use]
pub fn build_sing_query_with(notes: &[Note], bpm: f32, carry_in: Option<char>) -> SingQuery {
    let placements = place_sing_notes(notes, bpm, i64::from(REST_FRAMES));
    emit_sing_query(notes, &placements, i64::from(REST_FRAMES), carry_in)
}
```

##### (b2) なぜ「配置」と「JSON 生成」を割るのか — 塊 query の frame 格子を単体 query に揃えるため

`build_sing_query` の `pos_of` は **`base_beat`(= 先頭 note の `start_beat`) からの相対拍**を
frame へ丸める (`common/src/voicevox.rs:193-197`)。だから **base が違えば同じ note の
frame が最大 1 frame ずれる**。素朴に `build_sing_query(塊の全 note)` を投げると:

- 塊の base はその塊の先頭フレーズの先頭 note。フレーズ単体 query の base はそのフレーズの
  先頭 note。→ **フレーズ内 2 音目以降が最大 ±1 frame (≈10.7 ms) ずれる**。
  `note_offsets` (停止中プレビューの再生開始位置) は単体 query から作るので、
  切り出した音の実位置と食い違う。
- さらに悪いことに、長さ 1 frame の note は base が変わると
  「切り詰めで 1 frame 未満」(`common/src/voicevox.rs:203-215`) で**落ちる側に転ぶ**。
  フレーズの note が塊 query で全滅すると、そのフレーズは**無音になる**。

**塊 query を「フレーズ単体 query の frame 格子をそのまま連結したもの」として組めば、
この 2 つは原理的に起きない。** そのための分割が上の 2 関数で、連結は
[`crate::voicevox_phrase::build_chunk_query`] (§3.2) が行う。emitter は 1 本のままなので
二重実装にならない。

**`REST_FRAMES` / `sing_head_beat` / `sing_base_beat` は一切動かない。** 塊 query が
端 rest に `PHRASE_PAD_FRAMES` を使うのは `emit_sing_query` の引数としてであって、
定数も単体 query の既定も口パク配置 (`common/src/lipsync.rs:75-82`) も不変 (§9-4)。

#### (c) 持ち越し母音 (長音符「ー」) をフレーズへ引き渡す

`resolve_sing_lyric` (:293) は `carried_vowel` を **休符をまたいで持ち越す** (:223 で 1 本の
走査中ずっと保持し、rest でリセットしない)。フレーズに割ると、**先頭が裸の「ー」であるフレーズは
前フレーズの母音を失って fallback の「あ」になる** = 継ぎ目の息づかいとは無関係に歌詞が変わる。

`build_sing_query_with` / `emit_sing_query` の `carry_in` ((b) で定義) がその注入口。
加えて、query を組まずに carry だけ先送りしたいフレーズ分割器のために:

```rust
/// `notes` を順に歌詞解決したときの、末尾時点の持ち越し母音。
/// query を組まずに carry だけ先送りしたいとき (フレーズ分割器) に使う。
#[must_use]
pub fn carry_vowel_after(notes: &[Note], carry_in: Option<char>) -> Option<char>;
```

`carry_vowel_after` は `resolve_sing_lyric` を回して戻り値を捨てるだけ (別実装にしない)。

**この carry はキャッシュキーにも必ず含める** — 含めないと「同じ相対 frame 列だが母音が違う
フレーズ」が誤ヒットする。§3.3 のキーは query JSON (= 解決後の歌詞が入っている) から作るので、
自動的に含まれる。

#### (d) 追加テスト (`common/src/voicevox.rs` の `mod tests`)

- `note_placements_report_truncated_end_frames` — 重なる 2 note で `end_frame` が
  次の `start_frame` に切り詰められること。
- `carry_vowel_flows_across_rests` — 「ら」→ 休符 → 「ー」で 2 つ目の note の lyric が
  「あ」(母音) になること、および `carry_vowel_after(&[ら], None) == Some('あ')`。
- `carry_in_restores_prolongation_across_a_split` — notes を 2 つに割り、後半を
  `build_sing_query_with(後半, bpm, carry_vowel_after(前半, None))` で組むと、
  裸の「ー」が全体 1 本のときと **同じ lyric** に解決されること (これがフレーズ分割の要)。
- `edge_rest_frames_shifts_every_placement_uniformly` — 同じ notes を
  `place_sing_notes(.., 0)` と `place_sing_notes(.., REST_FRAMES)` に掛けると、
  全 `NotePlacement` の `start_frame` / `end_frame` が **同じ差分だけ**平行移動し、
  隙間の有無 (= フレーズ境界) も落ちる note の集合も変わらないこと。
  これが「塊 query 内のフレーズ (端 rest 0 起点で配置) と単体 query (端 rest
  `REST_FRAMES`) の相対 frame 列が一致する」という §3.2 / §4.2 の前提の回帰。

#### (e) FrameAudioQuery の sample rate 正規化を common へ移す

`daw_plugin_host/src/builtin/voicevox_synth.rs` の `OUTPUT_SAMPLE_RATE` (:71) と
`find_sample_rate_field` (:409-419) を **`common/src/voicevox.rs` へ移設**する。

```rust
/// 合成 WAV の出力 sample rate。`/sing_frame_audio_query` 応答の `outputSamplingRate`
/// を必ずこの値へ上書きしてから `/frame_synthesis` に渡す。
pub const OUTPUT_SAMPLE_RATE: u32 = 48_000;

/// FrameAudioQuery 応答 JSON を **キャッシュに置ける正規形**にする
/// (= `outputSamplingRate` を [`OUTPUT_SAMPLE_RATE`] へ上書き。key が無ければそのまま)。
///
/// daw_gui (口パク) と daw_plugin_host (塊クエリ) は **同じ鍵関数
/// (`voicevox_cache::key_for_sing_query`)・同じ schema・同じ prefix** を使う。
/// 両者のエントリが実際に混ざらないのは、いま**楽譜 JSON の端 rest が違う**からに
/// すぎない (口パク = `build_sing_query` で `rest_start`/`rest_end` が `REST_FRAMES` = 10、
/// 塊 = `build_chunk_query` で `PHRASE_PAD_FRAMES` = 47 → 別ハッシュ)。
/// **これは誰も強制していない偶然の分離**であって、鍵空間の分離ではない。
///
/// なので「この鍵空間に入る値は必ず正規形」を不変条件にして、put の前に両方がこれを
/// 通す。破ると、生 body を put した側の 24 kHz 指定 query をもう片方がスライスして
/// `/frame_synthesis` に投げ、24 kHz の WAV が 48 kHz 前提の buffer に混ざる
/// (`daw_plugin_host/src/builtin/voicevox.rs:586`/`:611`/`:652` の `out_sr` は
/// last-writer-wins なので、混ざっても気付けない)。
#[must_use]
pub fn normalize_frame_query(body: &str) -> String;
```

実装は現行 `find_sample_rate_field` + `body.replace(...)` そのまま (文字列置換のままでよい。
`serde_json` 経由にすると engine が返した他 field の表現が変わり、鍵が engine の
JSON 整形に依存してしまう)。

**「衝突が実在する」とは書かないこと。** 初版はそう書いていたが実コードでは起きない
(上記のとおり端 rest が違うので歌唱 query は必ず別ハッシュになる。実際に共有されるのは
talk の `key_for_talk_query` だけ)。正規化を入れる理由は「今は当たらない」を
**設計の前提にしない**ため。この分離が事実であることは
`voicevox_cache` の `lipsync_and_chunk_queries_do_not_share_a_key` (§3.3) が固定する。

---

### 3.2 `common/src/voicevox_phrase.rs` (新規)

純粋関数だけ。HTTP も JSON スライスもここには置かない。

```rust
//! 歌唱合成の**分割単位** — フレーズ (合成の単位) と 塊 (クエリの単位)。
//!
//! - フレーズ = 隙間ゼロで連続する note の極大列 (本家 `extractPhraseNotes` と同一定義)、
//!   かつ声 (speaker) が変わる位置でも切る。**クリップ境界では切らない**。
//! - 塊 = 連続する複数フレーズ。`/sing_frame_audio_query` を 1 回投げる単位。
//!   既定 60 秒 (docs/plan_rmd_75_voicevox_phrase.md §0 (C) の実測)。
//!   切れ目は**窓の中で最も長い休符**に置く。
//!
//! ここで定義する型はすべて**プロセス内の計算専用**で IPC を渡らない
//! (アーキ不変条件 7 / `common/build.rs` の `WIRE_SOURCES` に足さない)。
//!
//! 分割の一次情報と実測は docs/plan_rmd_75_voicevox_phrase.md を参照。

use crate::model::Note;
use crate::plugin_metadata::NoteMetadata;

/// 塊 (= 1 クエリ) の既定の長さ (秒)。実測で 30 秒はばらつきが倍、120 秒は改善せず
/// クエリ時間だけ 38% 増える。**60 秒が曲がり角**。
pub const DEFAULT_CHUNK_SECS: f32 = 60.0;
/// 設定で受け付ける下限 / 上限。300 秒を超えると engine の RAM / GPU が持たない
/// (361 秒でクエリ 18.4 秒、480 秒で engine が落ちる)。
pub const MIN_CHUNK_SECS: f32 = 15.0;
pub const MAX_CHUNK_SECS: f32 = 300.0;

/// フレーズ切り出しのパディング (前後、秒)。実測で 0.5 s が最良
/// (0.12 s → 2.88 dB / **0.5 s → 0.67 dB** / 1.0 s → 1.60 dB / 4.0 s → 0.56 dB。非単調なので
/// 「長ければ良い」ではない。§0 (D))。
pub const PHRASE_PAD_SECS: f64 = 0.5;
/// 同 frame 数 = `round(PHRASE_PAD_SECS * FRAME_RATE)` = 47。
/// **塊 query の端 rest でもある** — 端 rest をこの値にしておくと、先頭フレーズの
/// `[origin - PAD, ..)` と末尾フレーズの `[.., origin + len + PAD)` がちょうど塊の
/// frame 範囲の端に一致し、どのフレーズでも切り出し窓がクランプ不要になる (§4.2)。
pub const PHRASE_PAD_FRAMES: i64 = 47;

/// 合成 1 単位。
#[derive(Debug, Clone)]
pub struct Phrase {
    /// 解決済みの歌唱 style id (`0` は呼び出し前に `DEFAULT_SINGER_ID` へ潰しておく)。
    pub speaker_id: u32,
    /// このフレーズの note (song-absolute beat、start 昇順)。`build_sing_query*` に
    /// そのまま渡せる。
    pub notes: Vec<Note>,
    /// `notes` と同じ index の安定 note_id (`plugin_metadata::sing_note_id`)。
    pub note_ids: Vec<u32>,
    /// このフレーズに note を持つ clip の id (昇順・重複なし)。進捗表示のクリップ帰属。
    pub clip_ids: Vec<u32>,
    /// フレーズ先頭で有効な持ち越し母音 (長音符「ー」の解決)。
    pub carry_in: Option<char>,
    /// フレーズ先頭 note の `start_beat` / 末尾 note の終端 beat (song-absolute)。
    pub start_beat: f64,
    pub end_beat: f64,
}

/// クエリ 1 単位。`phrases[range]` を 1 本の query にまとめる。
#[derive(Debug, Clone)]
pub struct Chunk {
    pub speaker_id: u32,
    /// `split_into_phrases` の戻り値に対する index 範囲 (連続、同一 speaker)。
    pub phrases: std::ops::Range<usize>,
    /// 塊先頭の持ち越し母音 (= `phrases.start` のフレーズの `carry_in`)。
    pub carry_in: Option<char>,
}

/// `entries` をフレーズへ割る。戻り値は **speaker id 昇順 → start_beat 昇順**の決定論的な順序。
pub fn split_into_phrases(entries: &[NoteMetadata], bpm: f32) -> Vec<Phrase>;

/// フレーズ列を塊へまとめる。`chunk_secs` は呼び出し側で
/// `MIN_CHUNK_SECS..=MAX_CHUNK_SECS` にクランプ済みであること。
pub fn group_into_chunks(phrases: &[Phrase], bpm: f32, chunk_secs: f32) -> Vec<Chunk>;

/// 塊 1 個ぶんの `/sing_frame_audio_query` body と、各フレーズが塊 frame 空間で
/// 占める範囲。
#[derive(Debug, Clone)]
pub struct ChunkQuery {
    /// `POST /sing_frame_audio_query` に渡す JSON body。
    pub json: String,
    /// `phrases` と同じ index。`[origin, origin + len)` = そのフレーズの
    /// **先頭 note の開始 frame 〜 末尾 note の終端 frame** (塊 query の絶対 frame)。
    /// 切り出し窓は `[origin - PHRASE_PAD_FRAMES, origin + len + PHRASE_PAD_FRAMES)`。
    pub phrase_windows: Vec<std::ops::Range<i64>>,
    /// 塊 query の総 frame 数 (= `phrase_windows` 末尾 + `PHRASE_PAD_FRAMES`)。
    /// `frame_query_len` の実測値と一致するはず (§4.2 手順 6 で `debug_assert!`)。
    pub total_frames: i64,
}

/// 塊 query を「**各フレーズの単体 query と 1 frame もずれない格子**」で組む (§3.1(b2))。
///
/// 素朴に `build_sing_query(塊の全 note)` を投げると、丸めの基準 (`base_beat`) が
/// フレーズ単体 query と違うため (1) フレーズ内 2 音目以降が最大 ±1 frame ずれ、
/// (2) 1 frame 長の note が塊側だけで落ちてフレーズが無音になり得る。
/// ここでは **フレーズごとに `place_sing_notes(&ph.notes, bpm, 0)` を掛け**、その
/// 相対格子を平行移動して連結するので、どちらも原理的に起きない。
///
/// `phrases` は同一 speaker の連続列 (= `group_into_chunks` が返した `Chunk::phrases` の
/// スライス)。`carry_in` は `Chunk::carry_in`。
pub fn build_chunk_query(phrases: &[Phrase], carry_in: Option<char>, bpm: f32) -> ChunkQuery;
```

#### `split_into_phrases` の手順 (この通りに書く)

1. `entries` を **解決済み speaker** でグルーピング。
   `let speaker = if e.speaker_id != 0 { e.speaker_id } else { voicevox::DEFAULT_SINGER_ID };`
   グループは speaker id 昇順で処理する (決定論的順序)。
2. 各グループを `Vec<Note>` へ変換 (`Note { id: 0, start_beat, duration_beats, pitch,
   velocity, lyric: (空なら None), muted: false }`)。`NoteMetadata` の並び順は保つ。
   同じ index に対応する `note_id` / `clip_id` を平行配列で持つ。
3. `voicevox::place_sing_notes(&notes, bpm, i64::from(voicevox::REST_FRAMES))` を呼ぶ。戻り値は
   **重なり解決後に載る note だけ**を start_frame 昇順で返す。
   **ここに現れない note はフレーズに入れない** (query にも載らないため)。
   端 rest は全 placement に一律で足されるだけなので、ここで何を渡してもフレーズ境界は
   変わらない (§3.1(d) の `edge_rest_frames_shifts_every_placement_uniformly` が保証)。
   既定値を渡して「分割は口パクと同じ既定 query 空間で見る」を明示する。
   なお、この配置は**グループ全体を基準にした丸め**なので、フレーズ単体で配置し直すと
   ごく短い note が 1 件落ちることがある。その差はフレーズの**中身**の話であり
   境界 (隙間の有無) は上記のとおり変わらない。中身は `build_chunk_query` と
   単体 query が **同じフレーズローカル配置**を使うので両者で必ず一致する。
4. `placements` を走査し、`placements[i].end_frame != placements[i + 1].start_frame` で切る。
   **加えて 1 グループは単一 speaker なので、speaker による追加の切断は不要**
   (グルーピングが先に効いている)。
5. 各フレーズの `notes` / `note_ids` を `placements[].index` 経由で拾い、`clip_ids` は
   `sort_unstable` + `dedup`。`start_beat` = 先頭 note の `start_beat`、`end_beat` =
   末尾 note の `start_beat + duration_beats`。
6. `carry_in` はグループ先頭を `None` として順に伝播:
   `phrase.carry_in = carry; carry = voicevox::carry_vowel_after(&phrase.notes, carry);`

#### `group_into_chunks` の手順 (この通りに書く)

`seconds(a_beat, b_beat) = (b_beat - a_beat) * 60.0 / bpm` とする。

同一 speaker の連続フレーズ列ごとに、`s` (塊の先頭 index) から始めて:

1. `k_max` = `[s, k)` の長さ (= `seconds(phrases[s].start_beat, phrases[k-1].end_beat)`) が
   `chunk_secs` 以下になる最大の `k` (ただし最低 `s + 1` — **フレーズは絶対に割らない**)。
2. `k_max` がグループ末尾なら塊は `[s, 末尾)` で終了。
3. そうでなければ、`k_lo` = 長さが `chunk_secs * 0.5` 以上になる最小の `k`
   (無ければ `k_lo = k_max`)。`k ∈ [k_lo, k_max]` の中で
   **休符の長さ `phrases[k].start_beat - phrases[k-1].end_beat` が最大**になる `k` を選ぶ。
   同点なら大きい `k` (= 長い塊)。
4. `Chunk { speaker_id, phrases: s..k, carry_in: phrases[s].carry_in }` を積み、`s = k` で継続。

#### `build_chunk_query` の手順 (この通りに書く)

`PAD = PHRASE_PAD_FRAMES`、`c = 60.0 / bpm * FRAME_RATE` (= 1 拍あたりの frame 数)、
`base = phrases[0].start_beat` とする。

1. 各フレーズ `i` について **フレーズローカル配置**を取る:
   `let local = voicevox::place_sing_notes(&phrases[i].notes, bpm, 0);`
   (端 rest 0 = 先頭 note が frame 0)。`local` は空にならない (先頭 note は
   自分より前に note が無いので切り詰められない)。念のため `debug_assert!(!local.is_empty())`。
   `len_i = local.last().end_frame`。
2. **塊 frame 空間での原点**を決める:
   - `natural_i = PAD + ((phrases[i].start_beat - base) * c).round() as i64`
   - `origin_i = natural_i.max(prev_end + 1)` (`prev_end` = `origin_{i-1} + len_{i-1}`、
     `i == 0` では `origin_0 = PAD`)。
   - **クランプの意味**: フレーズ間には必ず休符があるが、フレーズローカル配置に組み替えた
     結果その隙間が 0 frame に丸まることがある。0 だと `emit_sing_query` が rest を
     挿入せず**隣接フレーズが engine から見て 1 本に融合**してしまうので、最低 1 frame
     空ける。ずれるのは塊内の休符長だけで、**各フレーズの音の中身にも、曲上の配置にも
     影響しない** (配置は §4.2 手順 3 のとおり曲 sample 空間で拍から直接決める)。
3. 連結する:
   - `chunk_notes` = 各フレーズの `notes` を **`phrases` の順**で連結した `Vec<Note>`。
   - `placements` = 各フレーズの `local` を `origin_i` だけ平行移動し、`index` を
     `chunk_notes` 内の index (= 先行フレーズの note 数の累計 + `local[].index`) に
     付け替えたものを連結。全体で `start_frame` 昇順・非重複になる (手順 2 のクランプ)。
   - `phrase_windows[i] = origin_i .. origin_i + len_i`。
   - `total_frames = origin_last + len_last + PAD`。
4. `let q = voicevox::emit_sing_query(&chunk_notes, &placements, PAD, carry_in);`
   → `ChunkQuery { json: q.json, phrase_windows, total_frames }`。

**この構成が保証すること** (§4.2 が全面的に依拠する):
- フレーズ `i` の note `k` は、塊 frame 空間で `origin_i + local[k].start_frame` に居る。
  単体 query では `REST_FRAMES + local[k].start_frame` に居る (端 rest ぶんの平行移動だけ)。
  → **切り出した WAV 内の相対位置が単体 query と完全に一致する** = `note_offsets` が正確。
- 単体 query に載った note は塊 query にも必ず載る (同じ `local` を使うので)。
  → 「フレーズが塊 query で全滅して無音になる」が起きない。
- 切り出し窓 `[origin_i - PAD, origin_i + len_i + PAD)` は
  `i = 0` で下端 `0`、`i = last` で上端 `total_frames` にちょうど一致し、
  中間フレーズは手順 2 のクランプで必ずその内側 → **クランプ不要**。

#### テスト (`#[cfg(test)] mod tests`、engine 不要)

- `phrases_break_only_on_gaps` — 隙間ゼロで続く 4 note は 1 フレーズ、間に休符を挟むと 2 つ。
- `phrases_never_cross_speakers` — 同じ時刻に別 speaker の note があっても混ざらない。
- `phrases_ignore_clip_boundaries` — `clip_id` が途中で変わっても、隙間ゼロなら 1 フレーズで
  `clip_ids` に両方入ること (= クリップ上スピナーが両方点く根拠)。
- `carry_in_flows_between_phrases` — 「ら」で終わるフレーズの次のフレーズの `carry_in` が
  `Some('あ')`。
- `chunks_cut_at_the_longest_rest` — 長さが同じ候補が複数あるとき、最長休符の位置で切れる。
- `chunk_never_splits_a_phrase` — `chunk_secs` より長い単一フレーズが 1 塊になる。
- `chunks_are_deterministic` — 同じ入力で 2 回呼んで完全一致 (キャッシュキーの前提)。
- `chunk_query_grid_matches_each_phrase_solo_query` — 3 フレーズの塊で、
  `build_chunk_query` の `placements` 由来の「フレーズ内相対 frame 列」が、各フレーズの
  `build_sing_query_with(&ph.notes, bpm, ph.carry_in)` の `notes[].start_frame - REST_FRAMES`
  と **完全一致**すること。**この計画の核**なので、意図的に「本番の式を写すだけ」に
  ならない形 (2 つの独立な経路の突き合わせ) で書く。
- `chunk_query_keeps_every_note_the_solo_query_keeps` — 1 frame 長の note を含む
  フレーズでも、単体 query に載った note が塊 query から落ちないこと
  (= 「フレーズが無音になる」の回帰)。
- `chunk_query_windows_need_no_clamping` — 先頭 / 末尾を含む全フレーズで
  `0 <= origin - PAD` かつ `origin + len + PAD <= total_frames`。
- `chunk_query_separates_phrases_by_at_least_one_frame` — フレーズローカル配置で隙間が
  0 に丸まる入力でも、`origin_{i+1} >= origin_i + len_i + 1` になること。

---

### 3.3 `common/src/voicevox_cache.rs` (`daw_plugin_host` から移設 + 拡張)

**なぜ移すのか**: 口パク (daw_gui) にも同じキャッシュを効かせる必要があるが、
`daw_gui/Cargo.toml` の依存は `common` だけで `daw_plugin_host` を含まない。
現在地 (`daw_plugin_host/src/builtin/voicevox_cache.rs`) からは daw_gui が触れない。

移設後の変更点:

```rust
/// キャッシュ値の種別 = 拡張子。**GC の予算には両方を数える**
/// (旧実装は `.wav` しか見ておらず、JSON を足すと上限の外で無制限に増えた)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// 合成 WAV (`<hex>.wav`)。
    Wav,
    /// `/sing_frame_audio_query` / `/audio_query` の応答 JSON (`<hex>.json`)。
    Json,
}
```

- `CACHE_SCHEMA_VERSION: u32` を **2 → 3** に上げ、doc に
  `/// - 3: r.md #75 — 塊クエリ + フレーズ単位 frame_synthesis へ分割 (合成内容の定義が変わる)`
  を足す。
- キー関数を差し替える:

```rust
/// 歌唱 **フレーズ WAV** のキー
/// = `hash(schema, "sing-phrase", phrase_query_json, singer_id, chunk_secs.to_bits())`。
///
/// `phrase_query_json` は **そのフレーズ単体で** `build_sing_query_with(&ph.notes, bpm,
/// ph.carry_in)` を通した JSON = フレーズ先頭起点の相対 frame 列 + 解決後の歌詞 + pitch。
/// よって他フレーズの編集・クリップの移動 / 分割 / 複製・トラック名変更では変わらない。
///
/// **`chunk_secs` を混ぜるのは、それが合成結果を実際に変える入力だから**
/// (§0 (C): 30 秒の塊は全体 1 クエリ基準で 4.12 dB ばらつくのに対し 60 秒は 2.30 dB)。
/// 混ぜないと「設定つまみを動かしても全フレーズが cache hit して音が 1 サンプルも
/// 変わらない」= このモジュールの doc が禁じている「直したのに変わらない」誤診に
/// なる。混ぜた結果、設定変更は**曲全体の再合成**を意味する (それが正しい挙動。旧値の
/// エントリは別キーで残るので、戻せば即座に鳴る)。
///
/// 一方 **「その塊に他のどのフレーズが同居していたか」(塊の構成) は意図的にキーへ
/// 入れない**。入れると 1 音の編集で塊内の全フレーズが miss し、この設計の目的
/// (「1 音直す = 1 フレーズだけ再合成」) が消える。構成差で音量がどれだけ動くかは
/// §0 (D) の実測で **pad 0.5 s のとき最大 0.67 dB / σ 0.31 dB**
/// (= 全体 1 クエリを 2 回投げ直したときのノイズ下限 0.86 dB と同オーダー)。
/// 実運用 (編集を繰り返して別々の塊構成のフレーズが混ざった状態) はまだ測れていない
/// ので、§7.3 に「未測定」と明記し実機 sign-off (§7.4-8) で耳で確認する。
///
/// **エンジンが返した FrameAudioQuery のスライスをキーにしてはいけない**
/// (`/sing_frame_audio_query` は非決定的 = max|Δf0| 31.7 Hz。毎回 miss する)。
pub fn key_for_sing_phrase(phrase_query_json: &str, singer_id: u32, chunk_secs: f32) -> CacheKey;

/// `/sing_frame_audio_query` **応答 JSON** のキー = `hash(schema, "sing-query", score_json)`。
/// speaker は常に `QUERY_SPEAKER` (= 6000、`common/src/voicevox.rs:26`。
/// `voicevox_synth.rs:88` / `voicevox_client.rs:105` の両方が固定で使う) なので混ぜない。
///
/// **daw_gui の口パク query と daw_plugin_host の塊クエリはこの 1 本の鍵関数を共有する。**
/// ただし現状の 2 者は実際には衝突しない — 口パクは `build_sing_query`
/// (端 rest = `REST_FRAMES` = 10)、塊は `build_chunk_query` (端 rest =
/// `PHRASE_PAD_FRAMES` = 47) なので、score JSON の `rest_start` / `rest_end` が必ず違う。
/// **これは誰も強制していない偶然の分離**なので、「当たらないから何を put してもよい」
/// とはしない:
///
/// **値は必ず [`common::voicevox::normalize_frame_query`] を通してから put すること。**
/// これを不変条件にしておけば、将来どちらかの端 rest が変わって鍵が一致しても、
/// 掴んだ側が 24 kHz 指定の query をスライスして 24 kHz の WAV を得る事故が起きない
/// (§3.1(e))。
pub fn key_for_sing_query(score_json: &str) -> CacheKey;

/// (talk) `/audio_query` **応答 JSON** のキー = `hash(schema, "talk-query", text, speaker_id)`。
/// 応答は `(text, speaker)` の純粋関数で、speed / pitch 等は応答へ後から patch するので
/// 鍵に混ぜない (= 話速を変えても query は再取得しない)。
pub fn key_for_talk_query(text: &str, speaker_id: u32) -> CacheKey;

/// (talk) 合成 WAV のキー。現行 `key_for_talk` をそのまま (schema だけ 3 へ)。
pub fn key_for_talk(text: &str, speaker_id: u32, scales: &TalkParams) -> CacheKey;
```

- `VoiceVoxDiskCache` の API (現行の行番号は移設前 `daw_plugin_host/src/builtin/voicevox_cache.rs`):
  - `path_for(&self, key, kind)` (現行 :102-104) — `<hex>.wav` / `<hex>.json`。
  - `pub fn get(&self, key: CacheKey, kind: CacheKind) -> Option<Vec<u8>>` (現行 :108-110) —
    読めたら **mtime を touch** する (下記)。
  - `pub fn put(&self, key: CacheKey, kind: CacheKind, bytes: &[u8])` (現行 :115-121)。
  - `prune()` (現行 :145-) — 収集条件を `.wav` **または** `.json` に広げる
    (現行 :154-156 の `if path.extension() != Some("wav") { continue; }` を差し替え)。
- **LRU 化** (現行は `get` が mtime を触らないので実質 FIFO。フレーズ単位で小さいエントリが
  大量に増える新設計では「Undo で戻したら即鳴る」が成立しなくなる):

```rust
/// hit したエントリの mtime を「今」に進める間隔。毎 hit で書くと 1 ジョブで数百回の
/// FS 書き込みになるので、mtime が既に十分新しいときは触らない (粗い LRU で足りる)。
const TOUCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);
```

  `get` の中で、読めた後に `metadata().modified()` を見て `TOUCH_INTERVAL` より古ければ
  `std::fs::File::options().write(true).open(path)` → `f.set_modified(SystemTime::now())`
  (失敗は握り潰す = キャッシュは最適化)。`put_inner` の「既存なら早期 return」(:126-129) にも
  同じ touch を入れる。

- **2 プロセス同時 prune の race を doc に書く**: put は tmp+rename で安全だが、prune の
  read_dir → remove は daw_gui と daw_plugin_host で並走し得る。最悪「今書いたばかりの
  エントリを他方が消す」= miss して再合成 (= 致命ではない)。module doc に明記する。

- 既存テストは移設 + kind 引数対応で書き換え (`key_sing_stable_and_singer_sensitive` は
  `key_for_sing_phrase` 版へ)。追加:
  - `phrase_key_changes_with_chunk_secs` — 同じ楽譜・同じ声でも `chunk_secs` が違えば
    別キーになること (= 設定つまみが効くことの回帰。§3.3)。
  - `prune_counts_json_toward_budget` — `MAX_CACHE_BYTES` をテストから触れるよう
    `prune_to(&self, budget: u64)` を切り出し、`prune()` は `self.prune_to(MAX_CACHE_BYTES)`。
    `.json` を大量に置いて budget を超えさせ、`.json` も削除対象になることを assert。
  - `get_touches_mtime_for_lru` — 古い mtime を人工的に設定 → `get` 後に新しくなる。
  - `lipsync_and_chunk_queries_do_not_share_a_key` — 同じ notes から作った
    口パク query (`voicevox::build_sing_query`) と塊 query
    (`voicevox_phrase::build_chunk_query` の 1 フレーズ版) が
    **別の `key_for_sing_query` になる**こと。§3.1(e) / §9-9b が「今は当たらない」と
    書いている事実を機械で固定し、将来どちらかの端 rest を変えた人がここで気付ける
    ようにする (当たるようになっても正規化があるので壊れないが、**気付かずに**
    当たるのは避ける)。

`daw_plugin_host/src/builtin/voicevox_cache.rs` は **削除**、`daw_plugin_host/src/builtin.rs`
の `mod voicevox_cache;` も削除。`common/src/lib.rs` に `pub mod voicevox_cache;` を追加。

---

### 3.4 `common/src/plugin_metadata.rs`

#### (a) `note_id` の安定 id 化 (アーキ不変条件 1 の是正)

現状は「トラック内通し index」で、`daw_gui/src/handler/voicevox.rs:344` と
`daw_audio/src/sequencer.rs:121-171` が **独立に再計算**している
(sequencer 側のコメント自身が「builtin plugin 側の expected note_id とずれる可能性がある」と
自白している)。クリップ先頭に 1 音足すと以降の全 note_id がずれ、フレーズキャッシュも
停止中プレビューも壊れる。

`talk_event_id` (:114) の直前に置く:

```rust
/// (sing) 1 clip あたりの最大 note 数 / 1 track あたりの最大 clip 数
/// (`sing_note_id` の基数)。積が [`TALK_EVENT_ID_BASE`] にちょうど収まる値。
pub const MAX_NOTES_PER_CLIP: u32 = 16_384;
pub const MAX_CLIPS_PER_TRACK_FOR_NOTE_ID: u32 = 16_384;

/// (sing) `(clip_id, note.id)` から決定論的に `note_id` を導出する。
///
/// flush (daw_gui `sync_vocal_metadata`) と再生トリガ (daw_audio `sequencer`) が
/// **同じ式**で計算するので、「クリップ先頭に 1 音足すと以降の全 note_id がずれる」
/// という旧「トラック内通し index」の欠陥が構造的に消える (アーキ不変条件 1)。
///
/// 値域は `[0, TALK_EVENT_ID_BASE)` に**必ず**収まる (= talk の high band を侵さない)。
/// clip / note が基数を超えた場合は剰余で畳むので、極端な project では 2 note が同じ
/// id を共有し得る (= 停止中プレビューがもう一方の位置から鳴る)。再生・書き出しには
/// 影響しない縮退で、現実的な曲では起きない。
#[must_use]
pub fn sing_note_id(clip_id: u32, note_id: u32) -> u32 {
    (clip_id % MAX_CLIPS_PER_TRACK_FOR_NOTE_ID) * MAX_NOTES_PER_CLIP
        + (note_id % MAX_NOTES_PER_CLIP)
}
```

`TALK_EVENT_ID_BASE = 1 << 28` (`plugin_metadata.rs:104`) は **変えない** (`= 16_384 * 16_384`)。
`daw_audio/src/engine.rs:49` の `PREVIEW_NOTE_ID = u32::MAX` とも衝突しない。
`clap_plugin.rs:1656/1660` と `vst3_plugin.rs:1763/1792` が `note_id <= i32::MAX` を確認して
i32 に詰めるが、新値域も `1<<28` 未満なので **どちらも変更不要**。

テスト追加:
- `sing_note_ids_stay_below_talk_band` — `sing_note_id(u32::MAX, u32::MAX) < TALK_EVENT_ID_BASE`。
- `sing_note_id_is_stable_against_sibling_insertion` — 同じ `(clip_id, note.id)` なら
  他 note の増減に依らず同値。
- `sing_and_talk_id_spaces_do_not_overlap`。

#### (b) `TalkMetadata` にも `clip_id` を持たせる

`TalkMetadata` (:81-98) には **`clip_id` が無い**。このままだと §3.5(c) の
`pending_clips` / §5.1(c) の `clip_wav_synthesizing` が talk (読み上げ) クリップの帰属を
出せず、**Text クリップにスピナーが一切点かなくなる**
(現行 `track_wav_synthesizing` はトラック単位なので Text clip にも出ている = 機能後退)。

`event_id` から `(id - TALK_EVENT_ID_BASE) / MAX_TEXT_EVENTS_PER_CLIP` で逆算はしない
(`talk_event_id` が `saturating_*` を含む以上、逆関数は全域では正しくない。
そもそも「持っている情報を捨てて割り算で復元する」のは安定 id addressing の逆)。
`NoteMetadata` と同じく素直に持たせる:

```rust
    /// Stable `Clip::id` (track 内一意) of the Text clip this utterance belongs to。
    /// `event_id` の導出元 (`talk_event_id(clip_id, event_index)`) であり、合成進捗の
    /// クリップ帰属 (`VocalSynthProgress::pending_clips`) にも使う。
    #[serde(default)]
    pub clip_id: u32,
```

同ファイル内なので `WIRE_SOURCES` は現状のまま (§3.6)。書く側は
`daw_gui/src/handler/voicevox.rs:384-395` の literal に `clip_id: clip.id` を足すだけ。
テストの literal も更新する (`common/src/plugin_metadata.rs:139-150`、
`common/src/protocol.rs:1040-1049`、`daw_plugin_host/src/builtin/voicevox.rs:1098`)。

#### (c) 嘘になる doc / コメントの全列挙

`note_id` を安定 id へ変える時点で、**「通し index」と書いてある記述はすべて嘘になる**。
以下を全部直す (1 つでも残すと次の実装者が旧仕様として読む)。

`common/src/plugin_metadata.rs`
- `:41-45` `NoteMetadata.note_id` の doc は「daw_gui uses the clip-internal note index (= position
  in `Clip.notes` *of the same content*)」と書いてあるが、実装は **track 内通し index**。
  新しい実装 (`sing_note_id(clip_id, note.id)`) を説明する文へ全面差し替え。
  daw_audio の `TimedNoteEvent` と同じ式で計算される点を明記。
- `:62-68` `NoteMetadata.clip_id` の doc は「The VOICEVOX builtin groups the flushed metadata by
  `clip_id`」と書いてあるが **daw_plugin_host に `clip_id` は 0 ヒット = dead field**。
  新しい実際の用途に書き換える:
  「(1) `note_id` の導出元、(2) builtin が合成進捗を**クリップ単位で報告**するための帰属情報
  (`VocalSynthProgress::pending_clips`)。グルーピングには使わない (合成単位はフレーズ)」。
- `:83-85` `TalkMetadata.event_id` の doc 「sing の `note_id` 空間 (= 通し index、小さい値)」
  → 「sing の `note_id` 空間 (= `sing_note_id`、`[0, TALK_EVENT_ID_BASE)`)」。
- `:103-104` `TALK_EVENT_ID_BASE` の doc 「sing の `note_id` (= track 内 note 通し index、
  現実的に < 数万)」→ 「sing の `note_id` (= `sing_note_id` の値域 `[0, 1<<28)`) の**直上**。
  両者は定義上ちょうど接する (`16_384 * 16_384 == 1 << 28`)」。
- `:134` テスト内コメント「sing note_id (= 小さい通し index) とは重ならない」→ 新定義に合わせる
  (assertion 自体は `> 100_000` のままでよいが、`sing_note_ids_stay_below_talk_band` (下記) が
  上界側の本命)。

`daw_audio/src/sequencer.rs`
- `:19-22` `NoteTransition` の doc 「audio engine が track 内全 clip notes を flatten した
  「通し index」を振り」→ `sing_note_id(clip.id, note.id)` の説明へ。
- `:120-131` の長いコメント (「どちらでも builtin plugin 側の expected note_id とずれる可能性が
  ある」という自白) → §6 の新コメントへ差し替え。
- `:256` 「note_id (= 小さい通し index) と event_id (= high band) は衝突しない」→
  「note_id (= `sing_note_id`、`[0, TALK_EVENT_ID_BASE)`) と event_id (= high band)」。
- `:421` テスト doc 「(= enumerate 通し index)」→ §6 の書き換えに合わせる。

`daw_audio/src/engine.rs`
- `:46-48` `PREVIEW_NOTE_ID` の doc 「sequencer が振る通し index (= 0.. の小さい値)」→
  「sequencer が振る `sing_note_id` / `talk_event_id` (= `[0, 1<<28)` ∪ high band)」。
  `u32::MAX` はどちらとも衝突しない。

`common/src/process_data.rs`
- `:133-140` `TimedEvent.note_id` の doc は概ね中立だが、末尾の
  「`0` is a valid id (= **first note in the track**)」が通し index 前提。
  「`0` is a valid id (`sing_note_id(0, 0)`), so consumers must not treat 0 as "unset"」へ直す。
  (裏取りレポートは「中立なので変更不要」としていたが、この括弧書きだけは旧仕様の説明なので直す。)

---

### 3.5 `common/src/protocol.rs`

#### (a) `SetBuiltinPluginNoteMetadata` に `chunk_secs`

`:500-506` の variant に足す:

```rust
    SetBuiltinPluginNoteMetadata {
        device_id: u64,
        bpm: f32,
        /// 塊 (= `/sing_frame_audio_query` 1 回) の長さ (秒)。アプリ設定
        /// (`app_config.json` の `voicevox_chunk_secs`) が SSoT で、
        /// `voicevox_phrase::{MIN,MAX}_CHUNK_SECS` にクランプ済みの値が来る。
        /// **合成結果を変える入力**なので (§0 (C))、daw_gui 側の再送デデュープの
        /// 比較対象に含め、かつ **フレーズ WAV のキャッシュキーにも混ぜる**
        /// (§3.3)。両方に入れて初めて「設定を変えたら音が変わる」が成立する
        /// (キーに入れないと、再送しても全フレーズが cache hit して何も変わらない)。
        chunk_secs: f32,
        entries: Vec<crate::plugin_metadata::NoteMetadata>,
        talk: Vec<crate::plugin_metadata::TalkMetadata>,
    },
```

#### (b) 再生ヘッド優先ヒント (**新規・再合成をトリガしない**)

`PrepareVocalSynth` (:509) の隣に:

```rust
    /// builtin VOICEVOX の合成順序ヒント。再生位置に近いフレーズから合成させる
    /// (本家 `selectPriorPhrase`: 再生位置を含む → 後ろ → 前)。
    ///
    /// **`SetBuiltinPluginNoteMetadata` に相乗りさせてはいけない** — あちらは
    /// daw_gui 側で `(bpm, chunk_secs, entries, talk)` の一致による再送デデュープが
    /// 掛かっており、playhead を比較に入れれば**トランスポートを動かすたびに再合成**、
    /// 入れなければ **playhead が永久に stale** になる。どちらも壊れているので、
    /// 合成をトリガしない専用の軽量メッセージにする。
    SetVocalSynthPriority { device_id: u64, playhead_beats: f64 },
```

#### (c) 進捗つき状態報告

`VocalSynthFailure` の定義 (`:789`) の直前に:

```rust
/// builtin VOICEVOX 合成の進捗。`(busy, failure)` だけだった報告を、フレーズ単位の
/// 残件数とクリップ帰属まで含む 1 つの形に統一する (callback の引数と IPC の payload が
/// 同じ型 = SSoT)。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Default)]
pub struct VocalSynthProgress {
    /// 合成中か。
    pub busy: bool,
    /// 直近試行の失敗種別 (engine 到達可否で区別)。
    pub failure: VocalSynthFailure,
    /// 未完了フレーズ数 (talk の 1 発話も 1 件として数える)。
    pub pending: u32,
    /// この job の総フレーズ数。`pending / total` は **percent にしない**
    /// (HTTP は中間進捗を返さない = 偽の % を出さない、という既存判断を維持)。
    pub total: u32,
    /// 未完了フレーズ / 未完了 talk 発話が掛かっている clip id (昇順・重複なし)。
    /// クリップ上スピナーを「そのクリップに未完了の仕事があるときだけ」点けるために使う。
    /// 歌唱は `Phrase::clip_ids`、talk は `TalkMetadata::clip_id` (§3.4(b)) から集める
    /// — talk を入れ忘れると Text クリップのスピナーが消える。
    pub pending_clips: Vec<u32>,
}
```

`VocalSynthFailure` に `#[derive(Default)]` と `#[default]` を `None` に付ける
(`VocalSynthProgress` の `Default` 導出のため)。

`PluginEvent::VoicevoxSynthStatus` (:776-782) を差し替える:

```rust
    /// builtin VOICEVOX plugin の合成スレッドの状態遷移 + 進捗。
    VoicevoxSynthStatus { device_id: u64, progress: VocalSynthProgress },
```

#### (d) テスト更新

`:1025-1051` の `builtin_note_metadata_roundtrip` に `chunk_secs: 60.0` と、
`TalkMetadata` literal (:1040-1049) へ `clip_id: 7` を足す。
`VocalSynthProgress` の bincode roundtrip テストと、`SetVocalSynthPriority` の
roundtrip テストを 1 件ずつ足す (既存 `roundtrip()` ヘルパを使う)。

---

### 3.6 `common/build.rs`

`WIRE_SOURCES` (:18-38) には `src/protocol.rs` / `src/plugin_metadata.rs` / `src/process_data.rs` が
既に入っており、本計画で wire に載る型の変更 (`VocalSynthProgress` / `PluginCommand` の
2 variant / `NoteMetadata` doc / **`TalkMetadata.clip_id`**) はすべてこの 3 ファイルの中にある。
よって **`WIRE_SOURCES` の変更は不要** (`common/build.rs:18-38` を実際に確認済み)。

ただし `common/src/voicevox_phrase.rs` / `common/src/voicevox_cache.rs` / `common/src/voicevox.rs`
は **未登録**なので、**将来ここに wire を渡る型を置いたら必ず `WIRE_SOURCES` に足すこと**
(アーキ不変条件 7)。本計画で作る `Phrase` / `Chunk` / `NotePlacement` / `CacheKind` は
プロセス内の計算専用で IPC を渡らない — その旨を各型の doc に 1 行書いておく。

---

## 4. `daw_plugin_host` の変更

### 4.1 `daw_plugin_host/src/builtin/voicevox_synth.rs`

HTTP を「塊クエリ」と「フレーズ合成」の 2 段に割り、FrameAudioQuery のスライスを足す。

#### 削除するもの

- `fn sing_query_to_wav` (:80-129) — 2 段が 1 関数に閉じていて分割できない。
- `pub struct BuiltinNoteSpec` (:135-163) — フレーズ分割は `NoteMetadata` を直接扱うので不要。
- `pub struct BuiltinSynthOutput` (:166-180) — 配置は `voicevox_render` が持つ。
- `pub fn synthesize_notes_for_builtin` (:188-249) — `voicevox_render` が置き換える。

#### 追加するもの

```rust
/// 合成 HTTP client (timeout 付き) を作る。塊クエリ / フレーズ合成 / talk が共用する。
fn synth_client() -> Result<reqwest::blocking::Client, SynthError>;

/// `POST /sing_frame_audio_query` (塊 1 回)。応答を
/// [`common::voicevox::normalize_frame_query`] に通した `FrameAudioQuery` JSON を返す。
///
/// `outputSamplingRate` は**ここで 1 回だけ** `OUTPUT_SAMPLE_RATE` に差し替える。
/// 以降の全スライスがこの値を継承するのでフレーズごとに sample rate がぶれず、
/// **キャッシュへ入るのも正規形だけ**になる (daw_gui の口パク query と鍵空間を共有する
/// ので、片方が生 body を put すると他方が 24 kHz の WAV を掴む。§3.1(e))。
pub fn fetch_sing_frame_query(
    client: &reqwest::blocking::Client,
    score_json: &str,
) -> Result<String, SynthError>;

/// `POST /frame_synthesis` (フレーズ 1 回)。WAV bytes を返す。
pub fn frame_synthesis(
    client: &reqwest::blocking::Client,
    frame_query_json: &str,
    singer_id: u32,
) -> Result<Vec<u8>, SynthError>;

/// FrameAudioQuery の frame 総数 (= `f0` 配列長)。
pub fn frame_query_len(fq: &serde_json::Value) -> Result<usize>;

/// FrameAudioQuery を frame 範囲 `[a, b)` で切り出す (純粋関数)。
///
/// - `f0` / `volume` は `[a, b)` をそのままスライス。
/// - `phonemes` は先頭から `frame_length` を積んで区間を出し、`[a, b)` と重なるものだけを
///   境界で切り詰めて残す (完全に外のものは落とす)。
/// - それ以外の field (`volumeScale` / `outputSamplingRate` 等) はそのまま引き継ぐ。
///
/// **phoneme の長さ field は `frame_length` (snake_case)**。engine の
/// `FramePhoneme` がそう定義しており (`voicevox_engine/tts_pipeline/song_engine.py` の
/// `phoneme.frame_length`)、本番の `daw_gui/src/voicevox_client.rs:129` も
/// `frame_length` だけを読んで動いている。同じ応答の中で `outputSamplingRate` /
/// `volumeScale` は camelCase という混在があるが、**phoneme 側が camelCase で返った
/// 観測は無い**ので推測で両対応を書かない。欠けていたら `Err` にして表に出す
/// (黙って 0 扱いにすると全 phoneme が落ちて無音になる)。
pub fn slice_frame_query(fq: &serde_json::Value, a: usize, b: usize) -> Result<String>;
```

`SYNTH_HTTP_TIMEOUT_SECS` (:69) の doc を書き換える —
「歌唱は曲全体の全 note を 1 query にまとめて frame_synthesis する…数十秒かかり得る」は嘘になる。
新しい説明: 「合成は **フレーズ単位** (実測 平均 145 ms / 最大 546 ms)。クエリは **塊単位**
(60 秒 ≈ 0.55 s、上限 300 秒でも 15 s 程度)。120 秒は engine のコールドスタートと
極端に長いフレーズのための余裕」。

`synthesize_talk_for_builtin` (:307-340) は
`VoiceVoxDiskCache` の import 先を `common::voicevox_cache` へ変え、
`get`/`put` に `CacheKind::Wav` を渡すだけ。加えて `/audio_query` 応答を
`key_for_talk_query` で JSON キャッシュする (口パク側と共有される)。
そのために `synthesize_talk` (:257-303) を「audio_query 取得」と「patch + synthesis」に割る。

#### テスト (engine 不要)

`slice_frame_query` の unit test を `mod tests` に足す:
- `slice_frame_query_cuts_arrays_and_phonemes` — f0/volume の長さが `b - a`、
  phoneme の合計 frame_length が `b - a`、範囲外 phoneme が落ちる、他 field が残る。
- `slice_frame_query_rejects_phonemes_without_frame_length` — `frame_length` が無い
  phoneme を含む入力で `Err` になること (黙って無音を作らない)。
- `frame_query_len_reads_f0` 。

### 4.2 `daw_plugin_host/src/builtin/voicevox_render.rs` (新規)

塊クエリ → フレーズ合成 → 配置 → mix のオーケストレーション。**synth thread (非 RT) から
だけ呼ばれる**。ここに RT 制約は無いのでヒープ確保・ログ・HTTP すべて可。

```rust
//! 歌唱合成のレンダリング — 塊クエリ + フレーズ単位 `frame_synthesis` + 差分 mix。
//! 設計と実測は docs/plan_rmd_75_voicevox_phrase.md。

/// フレーズの継ぎ目 (= 休符の中点) に入れるクロスフェードの**全長**。
/// 隣り合うフレーズは別々に合成されるので、継ぎ目に微小な不連続が残り得る。
/// 継ぎ目を中心に前後 `SEAM_XFADE_SECS / 2` が相補ランプになる。
const SEAM_XFADE_SECS: f64 = 0.005;

/// 完成 buffer を `ArcSwapOption` へ publish する最短間隔。
const PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 1 フレーズの生の合成結果 (= キャッシュに入る単位。継ぎ目のトリムもフェードも
/// **掛けていない**)。トリム量は隣接フレーズの拍に依存するので、掛けてから
/// キャッシュすると隣を編集しただけで miss する。
struct RenderedPhrase {
    /// 合成 WAV を mono 化したもの。`Arc` はメモリキャッシュと共有する
    /// (= 同じ実体を 2 度持たない)。
    samples: Arc<Vec<f32>>,
    /// `samples[0]` が来る曲 sample 位置 (符号付き。曲頭より手前なら負)。
    place: i64,
    /// 継ぎ目トリム後に残す曲 sample 範囲 `[keep_start, keep_end)` (§手順 5)。
    keep: std::ops::Range<i64>,
    /// 先頭 / 末尾にフェードを掛けるか (= その側に隣接フレーズがあるか)。
    fade_in: bool,
    fade_out: bool,
    /// `note_id → 曲 sample 位置` (停止中プレビュー用)。
    note_offsets: Vec<(u32, u64)>,
}

/// 曲全体の mono buffer を **差分で**組み立てる器 (手順 8)。長さは job 開始時に確定し、
/// talk がはみ出したときだけ `resize` で伸ばす。`applied` までは加算済み。
struct MixBuffer {
    buf: Vec<f32>,
    applied: usize,
    /// publish で貸し出し中の buffer が返ってくるまでの控え (手順 9)。最大 2 本。
    pool: Vec<Vec<f32>>,
}

/// フレーズ 1 本が終わるたびにコールバックへ渡す「積み上がった結果」。
/// 進捗の集計 (`pending` / `total` / `pending_clips`) と mix / publish を持つ。
/// `PhraseRenderer` 本体 (HTTP / キャッシュ / 塊 query) とは別 struct にして、
/// コールバック中に借用が衝突しないようにする。
pub struct PhraseRenderState {
    rendered: Vec<RenderedPhrase>,
    mix: MixBuffer,
    retired: Vec<(Arc<SynthResult>, u64)>,
    last_publish: std::time::Instant,
    total: u32,
    pending_clips: std::collections::BTreeSet<u32>,
    // …
}

/// この job のレンダリング全体。synth thread が 1 job につき 1 つ持つ。
/// `phrases` / `chunks` / 塊 query のキャッシュ / `PhraseRenderState` を抱える。
pub struct PhraseRenderer { /* 上記をまとめて持つ */ }
```

#### 手順 (この通りに実装する)

**入力**: `bpm: f32`, `chunk_secs: f32`, `entries: &[NoteMetadata]`,
`priority_beats: Option<f64>`, `shutdown: &AtomicBool`, `superseded: &dyn Fn() -> bool`,
`on_phrase_done: &mut dyn FnMut(&mut PhraseRenderState)`。

**すべての位置計算を「曲の sample 位置」1 つの空間で行う。** 塊クエリの frame 空間は
「どの frame を切り出すか」を決めるためだけに使い、配置・継ぎ目・note offset には
**一切使わない**。こうすると、キャッシュ hit で塊クエリを持っていないフレーズも
miss のフレーズと**完全に同じ式**で置ける (旧案は塊 frame 空間で継ぎ目を計算していたので、
hit のときに計算不能 or 別式になっていた)。

1. `let phrases = common::voicevox_phrase::split_into_phrases(entries, bpm);`
   `let chunks = common::voicevox_phrase::group_into_chunks(&phrases, bpm, chunk_secs);`
2. **各フレーズの「単体 query」を先に全部作る**。これがキャッシュキー・配置・note offset の
   SSoT で、**HTTP には投げない** (投げるのは塊クエリのスライス)。
   ```rust
   let pq = voicevox::build_sing_query_with(
       &ph.notes, bpm,
       SingQueryOptions { carry_in: ph.carry_in, ..Default::default() }, // 端 rest = REST_FRAMES
   );
   let key = voicevox_cache::key_for_sing_phrase(&pq.json, ph.speaker_id, chunk_secs);
   ```
   単体 query は**平行移動不変** (`pos_of` の基準が自分の先頭 note なので、曲中のどこへ
   動かしても同じ JSON) 。よってキーはクリップの移動・分割・複製で変わらない。
3. **フレーズの曲 sample 位置を確定する** (`sr` = `OUTPUT_SAMPLE_RATE`。実測値は decode 後に
   検証する):
   - `head = sing_place_samples(ph.start_beat, bpm, sr)` — 単体 query の **frame 0** が来る
     曲 sample (負にもなる)。
   - `pad_s = frames_to_samples(PHRASE_PAD_FRAMES as f64, sr).round() as i64`。
   - `wav_place = head + frames_to_samples(f64::from(REST_FRAMES), sr).round() as i64 - pad_s`
     — 合成 WAV の sample 0 が来る曲 sample。
     (WAV の先頭は「先頭 note の pad 手前」。塊クエリの端 rest を `PHRASE_PAD_FRAMES` に
     したので、これは**どのフレーズでも必ず成立する** = クランプが要らない。)
   - `wav_len = frames_to_samples((p_end_local - p_start_local + 2 * PHRASE_PAD_FRAMES) as f64,
     sr).round() as i64`。`p_*_local` は `pq.notes` の先頭 `start_frame` / 末尾 `end_frame`。
   - **note_offsets**: `pq.notes[i]` について
     `abs = (head + frames_to_samples(start_frame as f64, sr).round() as i64).max(0) as u64`。
     **`- REST_FRAMES` はしない** — `sing_place_samples` は `sing_head_beat`
     (`common/src/voicevox.rs:133`) 経由で既に `REST_FRAMES` ぶん手前を指しており、
     `start_frame` は先頭 rest 込みの絶対 frame だから、そのまま足すのが正しい
     (現行の正しい合成 `daw_plugin_host/src/builtin/voicevox.rs:612-623` の
     `place_samples + off` と同じ形。引くと **10 frame = 約 107 ms 早くなる**)。
   - `note_id` は `ph.note_ids[i]` (= `sing_note_id`)。
4. **合成順序を決める** — `priority_beats` があれば
   「再生位置を含むフレーズ → 位置より後ろを昇順 → 前を降順」で並べる
   (本家 `selectPriorPhrase` と同じ)。無ければ start_beat 昇順。
   `Vec<usize>` (phrases への index) として持つ。
5. **継ぎ目 (敷き詰め位置) を曲 sample 空間で決める。** 同一 speaker の `phrases` 列で
   隣り合うフレーズ同士について (**塊をまたいでも同じ扱い**。塊境界だけ特別扱いすると
   そこだけ二重発音 / 欠落する):
   - `seam(prev, next) = round(((prev.end_beat + next.start_beat) * 0.5) * spb)`
     (`spb = sr * 60 / bpm`)。**両隣が同じ 2 つの拍から同じ値を出す**ので、
     敷き詰めは定義上ぴったり合う (どちらか一方でしか計算しない、という実装にしない)。
   - `keep_start = 前の隣がいれば seam(prev, this) を `wav_place` でクランプ、いなければ
     `wav_place`。`keep_end` も対称に (`wav_place + wav_len` でクランプ)。
   - `fade_in = keep_start > wav_place`、`fade_out = keep_end < wav_place + wav_len`。
   - 休符が `2 * PHRASE_PAD` より長いと、両隣とも自分の WAV 端でクランプされて
     `keep_end(i) < keep_start(i+1)` になる — **その間は意図した無音**であって欠落ではない。
6. 順に処理する。各フレーズについて:
   - `shutdown` が立ったら即 return。`superseded()` が true なら **この job を捨てる**
     (= より新しい入力が来た。`done_gen` は進めない)。
   - **メモリキャッシュ** (`HashMap<CacheKey, Arc<Vec<f32>>>`、synth thread が保持) →
     **ディスクキャッシュ** (`get(key, CacheKind::Wav)` → `decode_wav_to_f32`) →
     **HTTP** の順で mono サンプルを得る。前 2 者で得られたら HTTP は 1 回も打たない
     (塊クエリすら取らない)。
   - HTTP に落ちる場合だけ、そのフレーズが属する塊の `FrameAudioQuery` を用意する:
     ```rust
     let score = voicevox::build_sing_query_with(&chunk_notes, bpm, SingQueryOptions {
         carry_in: chunk.carry_in,
         edge_rest_frames: PHRASE_PAD_FRAMES,   // ← クランプ不要にするため
     });
     let qkey = voicevox_cache::key_for_sing_query(&score.json);
     ```
     (`chunk_notes` = 塊内全フレーズの notes を連結。**必ず `phrases` の順**)
     `get(qkey, CacheKind::Json)` → miss なら `fetch_sing_frame_query` (= 正規化済) →
     `put(qkey, CacheKind::Json, ..)`。取得した JSON は `serde_json::from_str::<Value>` して
     **塊単位で 1 回だけ**保持する (同じ塊の別フレーズが再利用する)。
   - **切り出し窓** (塊クエリの frame 空間):
     - フレーズが `chunk_notes` の中で占める **index 範囲** `[off, off + ph.notes.len())` を
       持っておく (`off` = 塊内の先行フレーズの note 数の累計)。
     - `p_start` / `p_end` = `score.notes` のうち **`index` がその範囲に入る** placement の
       `start_frame` 最小 / `end_frame` 最大。
       **順番 (ordinal) ではなく `NotePlacement.index` で対応付ける** — 塊 query は自分の
       base_beat で丸めるので、単体 query では残った note が塊では「切り詰めで 1 frame 未満」
       (`common/src/voicevox.rs:203-215`) として落ち得る。ordinal 対応にすると、その 1 件で
       以降の窓が全部ずれる。
     - その範囲の placement が **1 件も無い**フレーズは、この塊では歌われない = 合成を
       skip して `warn!` を 1 行出す (無音として扱い、進捗は完了扱いにする)。
     - `a = p_start - PHRASE_PAD_FRAMES`, `b = p_end + PHRASE_PAD_FRAMES`。
       端 rest を `PHRASE_PAD_FRAMES` にしてあるので `0 <= a < b <= total` が**常に成立**する
       (`total` = `frame_query_len`)。**それでも `debug_assert!` を置く**
       (端 rest の既定値を将来触った人が気付けるように)。
   - `slice_frame_query(&fq, a as usize, b as usize)` → `frame_synthesis(.., singer_id)` →
     `decode_wav_to_f32` → `put(key, CacheKind::Wav, &wav)` + メモリキャッシュへ挿入。
     decode 後の `sample_rate` が `OUTPUT_SAMPLE_RATE` と違ったら `warn!` を出して
     **その値で** `wav_place` / `wav_len` を計算し直す (24 kHz の混入を無音で見逃さない)。
     長さが手順 3 の `wav_len` と違ったら実測値を採用し、mix buffer を必要なら伸ばす。
   - `RenderedPhrase { samples, place: wav_place, keep, fade_in, fade_out, note_offsets }`
     を積み、`on_phrase_done` を呼ぶ (進捗報告 + 必要なら publish)。
7. talk (`TalkSynthSpec`) は現行どおり 1 件ずつ `synthesize_talk_for_builtin` で合成し、
   `talk_place_samples` (:122-139) で位置を出して同じ mix buffer へ積む
   (`keep` = 全域、フェード無し)。**talk も進捗の 1 件として数え、`pending_clips` にも
   その `clip_id` を入れる**。
8. **mix — 差分加算 + buffer 使い回し** (§9-9 の RT 制約の本体):
   - 曲全体の長さ `total_samples` は **合成前に確定できる** (手順 3 の
     `wav_place + wav_len` の最大値。frame 演算だけで HTTP に依らない)。talk は長さが
     事前に分からないので、超えたぶんだけ `resize` で伸ばす (writer 側の確保、RT には無関係)。
   - `MixBuffer` は `Vec<f32>` 1 本 + 「何本目の `RenderedPhrase` まで加算済みか」を持つ。
     新しく完成したぶんだけを加算するので、publish のたびに全フレーズを足し直さない
     (旧案は `vec![0.0; total]` を毎回確保して全部足し直す = 5 分曲で 1 回 57 MB × 60 回、
     加算は O(フレーズ数²))。
   - 加算は現行 `mix_placed_groups` (`voicevox.rs:142-167`) と同じ規則:
     `place` が負なら曲頭より手前を切り落とす (ずらさない)、重なりは加算。
     `keep` 範囲だけを書き、`fade_in` / `fade_out` 側には継ぎ目を中心とする長さ
     `2 * xf` (`xf = round(SEAM_XFADE_SECS * 0.5 * sr)` = 120 sample) の線形ランプを掛ける。
     隣り合うフレーズは同じ窓で相補ランプになるので加算利得は 1。
     (WAV が継ぎ目 + `xf` まで届かない = 休符がちょうど `2 * PAD` 付近のときだけ、
     ランプが窓の途中で切れて和が 1 未満になり得る。そこは元々ほぼ無音なので
     可聴にならない。この条件を doc コメントに書く。)
   - `mix_placed_groups` は `MixBuffer` に置き換えて **削除**する (二重実装を残さない)。
     現行の配置テスト (`voicevox.rs:1191-1270`) は `MixBuffer` を使う形に書き直す (§4.3)。
9. **publish — RT スレッドに解放させない** (§9-9):
   - `PUBLISH_INTERVAL` 以上経過し、かつ再利用可能な buffer があるときだけ publish する。
     最後の 1 本が終わったときは必ず publish する。
   - 手順:
     1. `retired` (publish 済みで RT がまだ触っているかもしれない `Arc<SynthResult>`) を
        走査し、**quiesce 条件**を満たしたものを回収する。
     2. 回収できた `Arc` は `Arc::into_inner` → `SynthResult` → `Arc::into_inner(samples)` で
        `Vec<f32>` を取り戻し、`MixBuffer` の予備 buffer プールへ返す
        (= **2 本目以降の publish は大きな確保をしない**)。
     3. プールから buffer を 1 本取り、未加算のフレーズを加算してから
        `Arc::new(SynthResult { .. })` を作って `result_arc.store(Some(..))`。
        直後に `rt_epoch` を読んで `retired` へ `(旧 Arc, その epoch)` を積む。
     4. プールが空なら **今回の publish は見送る** (次の機会に回す。部分結果の表示が
        0.5 秒遅れるだけで、正しさには影響しない)。プールは 2 本で足りる。
   - **quiesce 条件** (= 「store より前に始まった `process()` は全部終わった」):
     audio half が `process()` の入口と出口で `rt_epoch.fetch_add(1, Release)` する
     (入口後は奇数、出口後は偶数)。store 直後に読んだ値 `e` が
     **偶数なら即座に安全**、奇数なら **`rt_epoch` が `e` から変化した時点で安全**。
     `Guard` は `process()` の中でしか生きないので、これで「その Guard は既に drop 済み」が
     保証される。RT 側の追加コストは 1 コールバックあたり atomic RMW 2 回だけ
     (確保・ロック・I/O は無い)。
   - `SynthResult` の `samples_per_beat` / `sample_rate` / `note_offsets` の意味は現行のまま
     (r.md #23 の「buffer index N = 曲の sample 位置 N」契約を維持)。

> **なぜ retire を放置してはいけないか** (旧計画の §9-9 は「retired な `Arc` は writer 側で
> drop される」と書いていたが、**これは誤り**)。arc-swap 1.9.1 の既定 (hybrid) 戦略では、
> `store` した writer が `Debt::pay_all` (`src/debt/mod.rs:81-108`) で **未払いの debt 1 件ごとに
> strong count を +1 して**から入れ替える。その後 reader 側の `Guard` が落ちるとき、
> `HybridProtection::drop` (`src/strategy/hybrid.rs:105-125`) は「debt が既に払われていたら
> 自分で `ManuallyDrop::drop` する」= **RT スレッドが最後の所有者になり得る**。
> 今までは job あたり publish 1 回なので踏みにくかっただけで、逐次 publish にすると
> 頻度が約 60 倍になる。上の quiesce + プールはこれを構造的に消す
> (RT が触り得る `Arc` は writer が必ず 1 本保持している)。

#### エラーの扱い (現行の区別を維持)

- `SynthError::Unreachable` — 全フレーズに影響するので **即中断して retry**。`done_gen` は
  進めない (bounce 待ちは engine 復帰まで待つ)。
- `SynthError::Rejected` — そのフレーズだけ諦めて続行。最初の `detail` を報告に載せる。

#### テスト

`mod tests` に engine 不要のものを置く。継ぎ目計算は
`fn phrase_window(prev_end_beat, this: &Phrase, next_start_beat, bpm, sr)
-> (place, len, keep, fade_in, fade_out)` という純粋関数へ切り出してテストする:

- `seams_tile_without_overlap` — 休符が `2 * PHRASE_PAD` **以下**の 3 フレーズで
  `keep_end(i) == keep_start(i+1)` (= 隙間なく敷き詰まる)。
- `long_rest_leaves_intentional_silence` — 休符が `2 * PHRASE_PAD` を**超える**ときは
  `keep_end(i) < keep_start(i+1)` で、**重ならない・順序が保たれる**こと。
  (旧計画は全ケースで `k1[i] == k0[i+1]` を主張していたが、それは成立しない。
  長い休符では両隣とも自分の WAV 端でクランプされ、間は意図した無音になる。)
- `window_is_inside_the_chunk_query` — 端 rest `PHRASE_PAD_FRAMES` で組んだ塊 query に対し、
  先頭 / 末尾のフレーズでも `0 <= a && b <= total` (= クランプが不要) であること。
- `crossfade_ramps_sum_to_unity` — 相補ランプの和が全域で 1 ± 1e-6。
- `note_offset_lands_on_the_note_beat` — 単体 query 由来の `note_offsets[0]` が
  `round(start_beat * spb)` と 1 sample 以内で一致すること
  (= r.md #39 契約。`- REST_FRAMES` を書くとここが 10 frame ずれて落ちる)。
- `mix_buffer_incremental_equals_full_remix` — フレーズを 1 本ずつ加算した buffer と、
  全部まとめて加算した buffer が一致すること (差分 mix の同値性)。

### 4.3 `daw_plugin_host/src/builtin/voicevox.rs`

- module doc (:1-26) を新方式に合わせて更新 (「speaker 単位まとめ合成」の説明を削除)。
- `SynthJob` (:74-82) を差し替え:

```rust
struct SynthJob {
    bpm: f32,
    /// 塊 (= 1 クエリ) の長さ (秒)。GUI の設定から来る。
    chunk_secs: f32,
    /// フレーズ分割の入力 (= flush された全 note)。分割は synth thread が行う
    /// (純粋・安価。plugin-main を重くしない)。
    entries: Vec<NoteMetadata>,
    talk: Vec<TalkSynthSpec>,
    generation: u64,
}
```
  `struct SpeakerSynthSpec` (:84-87) は **削除**。
- `sing_place_samples` (:112-115) / `talk_place_samples` (:122-139) / `SynthResult` (:172-185) は
  **そのまま**。`voicevox_render` から呼ぶので `pub(super)` にする (二重実装しない)。
- `mix_placed_groups` (:142-167) は `voicevox_render::MixBuffer` へ置き換えて **削除**
  (§4.2 手順 8)。`PlacedGroup` (:103) も不要になるので削除。
- **audio half (:205-414) に RT quiescence epoch を足す** (§4.2 手順 9)。他は変えない
  — 1 本の buffer を読むだけなので、フレーズ数が増えても影響しない。
  **`ArcSwapOption::load()` のみを使い `load_full()` を使わない既存 idiom (:262-267) は維持**
  するが、**その理由として現在 :262-266 に書いてあるコメント「retired 値は writer
  (synth thread、非 RT) 側で drop される」は誤りなので書き直す** (根拠は §4.2 の引用:
  arc-swap 1.9.1 `src/debt/mod.rs:81-108` と `src/strategy/hybrid.rs:105-125`)。
  正しくは「`load()` は Arc を clone しないので**通常は** RT が所有者にならない。ただし
  store と同時に走った `process()` は debt を払われて所有者になり得るので、
  **writer 側の quiesce 付き retire (voicevox_render) と対で初めて解放が起きない**」。

```rust
    /// RT quiescence epoch。`process()` の入口 / 出口で +1 する (入口後 = 奇数、
    /// 出口後 = 偶数)。synth thread は publish 直後にこれを読み、
    /// 「store より前に始まった process() が終わった」ことを確認してから旧 Arc を
    /// 回収する (§4.2 手順 9)。**RT 側のコストは atomic RMW 2 回だけ**。
    rt_epoch: Arc<AtomicU64>,
```
  出口の +1 は **早期 return / panic でも必ず走らせる**ため、入口で作る小さな RAII
  guard (`struct RtEpochGuard<'a>(&'a AtomicU64)` の `Drop`) で行う。確保はしない。
- `VoicevoxBuiltin` (:420-445) に追加:

```rust
    /// synth thread が完了フレーズごとに +1 する heartbeat。bounce の完了待ちが
    /// 「固まったのか、まだ進んでいるのか」を区別するために使う (30 秒の固定
    /// deadline では 5 分の曲が部分ミックスで書き出されてしまう)。
    synth_heartbeat: Arc<AtomicU64>,
    /// 再生ヘッド優先ヒント (`f64::to_bits`。`f64::NAN` = 未設定)。
    /// `SetVocalSynthPriority` が書き、synth thread が合成順序の決定で読む。
    /// **再合成はトリガしない。**
    priority_beats: Arc<AtomicU64>,
    /// audio half と共有する RT quiescence epoch (上記)。`new()` (:451) で 1 本作り、
    /// `VoicevoxAudioHalf::new` と `start_synth_thread` の両方へ `Arc::clone` する
    /// (`synth_result` と同じ配り方)。
    rt_epoch: Arc<AtomicU64>,
```

- `start_synth_thread` (:486-758) の中身を大幅に置換する。骨格:

```
loop {
    shutdown チェック / retry backoff (現行 :506-540 のまま)
    coalesce slot から job を take (現行 :541-556 のまま)
    job が空 (entries も talk も空) → result_arc.store(None); done_gen.store(gen);
                                      report(idle); continue;   (現行 :557-570 のまま)
    report(busy, prev_failure)                                   (現行 :572-579 のまま)

    let mut renderer = PhraseRenderer::new(&job, &memory_cache, priority_beats.load());
    renderer.render(|state: &mut PhraseRenderState| {
        // フレーズ 1 本完了ごとのコールバック
        synth_heartbeat.fetch_add(1, SeqCst);
        report_progress(VocalSynthProgress {
            busy: true, failure: prev_failure.clone(),
            pending: state.pending(), total: state.total(),
            pending_clips: state.pending_clips(),
        });
        // publish は §4.2 手順 9 (retire 回収 → buffer 再利用 → store → epoch 記録)。
        // プールが空なら見送る (次の機会に回す)。
        state.publish_if_due(&result_arc, &rt_epoch);
    });

    match renderer.outcome() {
        Superseded => { /* 新しい job が来た。done_gen は進めない */ }
        Unreachable(reason) => { 現行 :678-701 と同じ (job を戻して 1.5s backoff) }
        Done { rejected } => {
            renderer.state_mut().publish_final(&result_arc, &rt_epoch);  // 空なら store(None)
            done_gen.store(job.generation, SeqCst);   // ★ 全フレーズ完了後にだけ進める
            現行 :735-753 と同じ終端報告 (pending job があれば送らない)
        }
    }
}
```
  最終 publish はプールの空きを待つ (RT が走っていれば数 ms で quiesce する。走って
  いなければ epoch は偶数のまま = 即座に回収できる)。**待ちは有界にする** — 1 ms sleep の
  ループで最大 1 秒、それでも空かなければ `Vec` を 1 本だけ新規確保して publish する
  (synth thread は非 RT なので確保してよい。ここで無限に待つと job が終わらず
  `done_gen` が進まず、bounce / 書き出しが止まる)。

  **`done_gen` は全フレーズ (+ talk) 完了後にだけ進める。** 逐次 publish しても
  ここを緩めると、bounce / 書き出し (`PrepareVocalSynth` → `VocalSynthReady`) が
  **部分ミックスを掴む**。この一文をコードのコメントにも書くこと。

  メモリキャッシュ (`HashMap<CacheKey, Arc<Vec<f32>>>`) は synth thread のローカル変数として
  loop の外に置き、**job 開始時に「今回の job のキー集合」で `retain` する** (= 自然な GC)。
  これが「1 ノート編集 = 未変更フレーズは decode すらしない」を成立させる。

- `set_note_metadata` (:777-848): `by_speaker` グルーピング (:789-812) を **全削除**し、
  `entries.to_vec()` をそのまま job に載せる。`self.lyrics` の更新 (:779-784) は維持。
  signature が `chunk_secs` を受けるよう `VocalSynth` trait ごと変える (§4.4)。
- `synth_progress` (:852-858) は 3 つ組 `(queued, done, heartbeat)` を返す形に。
- `set_priority_beats` を実装 (atomic store のみ)。
- `stop_synth_thread` (:761-774) の最後の `(self.report)(false, VocalSynthFailure::None)` は
  `VocalSynthProgress::default()` を送る形に変える。
- 既存テストの追随:
  - `set_note_metadata_replaces_lyrics_buffer` (:1070) は signature 変更に追随。
  - `voicevox_talk_synth_places_wav`(:1097 付近) の `TalkMetadata` literal に `clip_id`。
  - **配置テストは契約 (r.md #39) を変えないまま、API だけ `MixBuffer` へ移す**
    (`mix_placed_groups` を消すので):
    `sing_placement_and_reader_compose_to_song_sample_identity` (:1191) /
    `placement_before_song_start_is_trimmed_not_shifted` (:1245) /
    `overlapping_groups_are_summed` (:1259)。
    `placement_before_song_start_is_trimmed_not_shifted` の
    `assert_eq!(buf.len(), 500)` だけは、`MixBuffer` が長さを事前確定する設計になるので
    「`buf[0]` が `wav[lead]` であること」+「有効長 (= 書かれた最終 sample) が 500」に
    書き換える (曲頭より手前を**ずらさずに捨てる**という不変条件は維持する)。
  - `talk_placement_is_speed_independent` (:1227) は `talk_place_samples` を直接呼ぶだけ
    なので **変更不要**。

### 4.4 `daw_plugin_host/src/plugin_instance.rs`

```rust
pub trait VocalSynth {
    /// Per-note metadata flush (歌詞 + talk + 塊の長さ)。
    fn set_note_metadata(
        &mut self,
        bpm: f32,
        chunk_secs: f32,
        entries: &[NoteMetadata],
        talk: &[TalkMetadata],
    );

    /// 再生ヘッド優先ヒント。**再合成はトリガしない** (順序だけを変える)。
    fn set_priority_beats(&mut self, playhead_beats: f64);

    /// `(queued_gen, done_gen, phrase_heartbeat)`。bounce の完了待ちは
    /// `done >= queued` を待ち、**heartbeat が動いている間は打ち切らない**。
    fn synth_progress(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>);
}
```

`HostCallbacks::on_vocal_synth_status` (:149-150) を
`Arc<dyn Fn(common::protocol::VocalSynthProgress) + Send + Sync>` に変更、
default (:318) を `Arc::new(|_| {})` に。

### 4.5 `daw_plugin_host/src/main.rs`

- `:794-805` の `on_vocal_synth_status` を `progress` 1 引数で `PluginEvent::VoicevoxSynthStatus
  { device_id, progress }` を送る形へ。
- `:1115-1130` の `SetBuiltinPluginNoteMetadata` arm に `chunk_secs` を通す。
- 新 arm:

```rust
            PluginCommand::SetVocalSynthPriority { device_id, playhead_beats } => {
                if let Some(rec) = self.instances.get_mut(&device_id)
                    && let Some(vs) = rec.plugin.as_vocal_synth()
                {
                    vs.set_priority_beats(playhead_beats);
                }
            }
```
  device 未発見でも **warn を出さない** (トランスポート中に毎秒来るヒントなので、
  ログを汚さない)。
- `:2362-2370` のログ arm に `chunk_secs` を足し、`SetVocalSynthPriority` の arm を
  **必ず**追加する (`tracing::trace!`)。この match の末尾は
  `other => tracing::info!(?other, "received command")` (:2383-2385) という catch-all なので、
  arm を足さないと**毎 tick の優先度ヒントが info! でログに流れ込む**。
- `prepare_vocal_synth` (:1604-1635) の **30 秒固定 deadline を撤去**する。
  5 分の曲の初回合成は実測 30.68 s なので、そのままだと **部分ミックスで書き出される**。

```rust
    /// 合成完了待ちの「停滞」判定。フレーズ 1 本の最大実測は 546 ms、engine の
    /// コールドスタートを見ても 60 秒進捗が無ければ異常。**総時間では打ち切らない**
    /// (5 分の曲の初回合成は実測 30 秒超で、固定 30 秒 deadline は部分ミックスを
    /// 書き出していた)。
    const SYNTH_STALL_TIMEOUT: Duration = Duration::from_secs(60);
```
  poll ループを「`done >= target` になるまで 50 ms 間隔で待つ。ただし `(done, heartbeat)` が
  `SYNTH_STALL_TIMEOUT` の間まったく変化しなければ諦めて `VocalSynthReady` を送る」に変える。

---

## 5. `daw_gui` の変更

### 5.1 `daw_gui/src/handler/voicevox.rs`

#### (a) `sync_vocal_metadata` (:303-421)

- `:334-338` のコメント (「note_id は…全 clip 連結 index を使う…PR-V2.4 で改めて clip 単位に
  する予定」) を削除し、`:344` の `let note_id = entries.len() as u32;` を

```rust
                    // 安定 id (アーキ不変条件 1): `(clip.id, note.id)` から決定論的に導出。
                    // daw_audio の sequencer が **同じ関数**で同じ値を作るので、
                    // 「クリップ先頭に 1 音足すと以降の全 note_id がずれる」が起きない。
                    let note_id = common::plugin_metadata::sing_note_id(clip.id, n.id);
```
  に置き換える。`clip_id: clip.id` の行 (:359) のコメント (:355-358) も
  「builtin が clip 単位で声を分けるための grouping key」→
  「`note_id` の導出元 + 合成進捗のクリップ帰属 (§3.4)」に直す。
- talk 側 (:384-395) の `TalkMetadata` literal に **`clip_id: clip.id` を足す** (§3.4(b))。
  これが無いと Text クリップのスピナーが点かない。
- 送信 (:415-420) に `chunk_secs` を足す:

```rust
        let chunk_secs = self.voicevox_chunk_secs();
```
  `fn voicevox_chunk_secs(&self) -> f32` を同 impl に足し、
  `self.ui_prefs.voicevox_chunk_secs.clamp(MIN_CHUNK_SECS, MAX_CHUNK_SECS)` を返す。
- 再送デデュープ (:405-412) の比較タプルを `(bpm, chunk_secs, entries, talk)` へ拡張
  (`state/voicevox.rs` の型も合わせる)。**playhead はここに入れない。**

#### (b) 進捗の受け口

`apply_voicevox_synth_status` (:186-217) を `progress: VocalSynthProgress` を取る形へ。
`entry.busy` / `failing_since` / `rejected` の更新ロジックは現行のまま、
`pending` / `total` / `pending_clips` を entry へコピーする。
idle かつ失敗なしのときに entry を消す (:214-216) 挙動は維持 (= 進捗もクリアされる)。

#### (c) 集計 API の差し替え

- `voicevox_synth_busy_count` (:266-277) を削除し、代わりに:

```rust
    /// 合成待ちのフレーズ総数 (= 全体オーバーレイの「残り N フレーズ」)。
    pub fn voicevox_pending_phrase_count(&self) -> u32 {
        self.voicevox.voicevox_synth_status.values().map(|s| s.pending).sum()
    }
```
- `track_wav_synthesizing` (:243-250) を残しつつ (overlay / 他の判定で使う)、
  クリップ単位の判定を足す:

```rust
    /// このクリップに **未完了フレーズが掛かっているか** (= クリップ上スピナーの点灯条件)。
    /// 「トラックが busy」ではないので、1 ノート直しただけで同トラックの全クリップが
    /// 回ることがなくなる。
    pub fn clip_wav_synthesizing(&self, track_id: u32, clip_id: u32) -> bool {
        let Some(track) = self.song_doc.song().tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pid) = self.voicevox_plugin_id_for_track(track) else { return false };
        self.voicevox
            .voicevox_synth_status
            .get(&pid)
            .is_some_and(|s| s.pending_clips.binary_search(&clip_id).is_ok())
    }
```

- 書き出しゲート (§5.10) 用に、曲中の全 VOICEVOX device を列挙する API を足す
  (`voicevox_plugin_id_for_track` (:232-241) を全 track に回すだけ。新しい判定は作らない):

```rust
    /// 曲中の builtin VOICEVOX device の安定 id (load 済のものだけ、track 順)。
    /// 書き出し前の合成完了ゲート (§5.10) が「誰を待つか」を決めるのに使う。
    pub(crate) fn all_vocal_synth_device_ids(&self) -> Vec<u64> {
        self.song_doc.song().tracks.iter()
            .filter(|t| t.is_voicevox_vocal())
            .filter_map(|t| self.voicevox_plugin_id_for_track(t))
            .collect()
    }
```

#### (d) 口パク (`regenerate_lipsync_for_track` :637-780) は **構造を変えない**

per-clip モデルのまま。合成側とはクエリが別物 (粒度もビート空間も違う) なので共有しない。
速くなるのはキャッシュ (§5.7) による。`lipsync_input_fingerprint` (:442-506) も変更不要。

### 5.2 `daw_gui/src/handler/ipc.rs`

- `:363-370` の `VoicevoxSynthStatus` arm を `{ device_id, progress }` へ。
- `:207-231` の `VocalSynthReady` arm に **書き出しゲートの分岐**を足す (§5.10)。
  bounce (`pending_vocal_synth_bounce`) の既存処理はそのまま残し、その前に
  「`pending_vocal_synth_export` からこの `device_id` を除き、空になったら
  `ReinitAllPlugins` を送る」を行う。`let _ = device_id;` (:212) は消える
  (id を実際に使うようになる)。

### 5.3 `daw_gui/src/handler/tick.rs`

`on_tick` の `playhead_beat` 更新 (:104-106) の直後に、優先度ヒントの送出を足す
(`self.transport.playhead_beat` は `Option<f64>`。停止中も GUI 側が権威なので、
seek / 停止で動いた値もこの経路で届く):

```rust
        // r.md #75: 合成中の VOICEVOX device に「いまここを再生している」を伝える。
        // 再合成はトリガしない (`SetVocalSynthPriority` は順序ヒント専用)。1 拍動くまでは
        // 送らない = トランスポート中でも IPC は数 Hz 以下に収まる。
        self.send_vocal_synth_priority_if_moved();
```

`daw_gui/src/handler/voicevox.rs` に実装:

```rust
    /// 合成中の builtin VOICEVOX へ再生ヘッド位置を送る。前回送信から 1 拍以上
    /// 動いたときだけ送る。busy な device が 1 つも無ければ何もしない。
    pub(crate) fn send_vocal_synth_priority_if_moved(&mut self);
```
`state/voicevox.rs` に `pub priority_sent: std::collections::HashMap<u64, f64>` を足して
前回送信値を持つ。停止 / seek でも `playhead_beat` が動くので同じ経路で届く。

さらに **書き出し watchdog の除外条件**を同ファイルで直す (§5.10 の待ちで誤発火するため):

```rust
        if matches!(self.transport.export_stage, Some(ExportStage::AudioRender { .. }))
            // r.md #75: 合成完了ゲート (§5.10) で待っている間は daw_audio がまだ
            // render を始めていないので、この watchdog の対象外。待ち自体は
            // plugin host 側の停滞判定 (SYNTH_STALL_TIMEOUT) で必ず終わる。
            && self.ipc.pending_vocal_synth_export.is_empty()
            && let Some(since) = self.transport.export_progress_at
            && since.elapsed() > EXPORT_WATCHDOG
```
(`tick.rs:34-46`。これを入れないと、キャッシュが冷たい長い曲で合成が 60 秒を超えた瞬間に
「音声エンジンが応答しないため書き出しを中止しました」になる。)

### 5.4 `Note.id` の一意性 — 全経路を確認済み、**コード変更なし**

`note_id = sing_note_id(clip.id, note.id)` の一意性は「1 つの content の中で `Note.id` が
一意」に乗る。全生成経路を実際に読んで確認した結果、**追加の対処は要らない**:

| 経路 | 実装 | 判定 |
|---|---|---|
| Glue (複数 content → 1 content) | `handler/glue.rs:358-360` が `for (i, n) in notes.iter_mut().enumerate() { n.id = i as u32 + 1; }` + `next_note_id = len + 1` (:363)。v29 で Audio event (:333-338) と同時に入った | **既に採番し直している。変更不要** |
| Split (1 content → 2 content) | `handler/clips.rs:900-955`。straddle した note は前後半で `id` を共有するが、**行き先の content が別**なので content 内一意は保たれる | 変更不要 |
| MIDI import | `midi_import.rs:768` が `note.id = i as u32 + 1` | 変更不要 |
| import 後の clip 生成 | `handler/media.rs:827-829` は `ptrack.notes` をそのまま使う (上で採番済み) | 変更不要 |
| ピアノロール等の新規 note | `MidiContent::alloc_note_id` (`common/src/model/content.rs:572-575`) 経由 (`handler/notes.rs:86,660` / `handler/midi.rs:279,343` / `app_types.rs:1980`) | 変更不要 |
| load 時 | `common/src/model.rs:1879` の `ensure_element_ids` が sentinel (0) を埋める | 変更不要 |

> 裏取りレポートが指摘したとおり、旧計画の「merged content で id を 0 に倒してから
> `ensure_element_ids`」は **既存ロジックの二重実装**だった。書くと `n.id = i + 1` を
> 消して同じことをやり直すだけで、間違えれば v29 の不変条件を壊す。書かない。
> 回帰テストも足さない (既存挙動であって、この変更で新しく入るロジックではない。
> プロジェクト規約「単純ケースにテストを書かない」)。

### 5.5 `daw_gui/src/app_types.rs`

`VocalSynthStatus` (:1646-1655) に足す:

```rust
    /// 未完了フレーズ数 / 総フレーズ数 (`VocalSynthProgress` の写し)。
    pub pending: u32,
    pub total: u32,
    /// 未完了フレーズが掛かっている clip id (昇順・重複なし)。
    pub pending_clips: Vec<u32>,
```
`LipsyncClipResult` (:1661-) は **変更不要**。

### 5.6 `daw_gui/src/app_config.rs` + `daw_gui/src/view/settings.rs` + イベント

- `AppConfig` に:

```rust
    /// r.md #75: VOICEVOX 歌唱合成の「塊」(= `/sing_frame_audio_query` 1 回) の長さ (秒)。
    /// 曲の内容ではなく **合成品質のつまみ**なのでプロジェクトには保存しない。
    /// 既定 60 秒 (実測で 30 秒はばらつきが倍、120 秒は改善せずクエリだけ遅くなる)。
    #[serde(default = "default_voicevox_chunk_secs")]
    pub voicevox_chunk_secs: f32,
```
  `fn default_voicevox_chunk_secs() -> f32 { common::voicevox_phrase::DEFAULT_CHUNK_SECS }`、
  `Default` impl (:73-90) にも追加。**load 時に必ずクランプする** (`load()` (:93-101) の中で
  `cfg.voicevox_chunk_secs = cfg.voicevox_chunk_secs.clamp(MIN_CHUNK_SECS, MAX_CHUNK_SECS)`。
  `serde_json::from_str(..).unwrap_or_default()` の戻り値を書き換える形にする)。
- **`app_config.rs:119-150` の `save_load_roundtrip` テストを更新する。**
  `..Default::default()` を持たない**網羅的な struct literal** なので、field を足した瞬間に
  ビルドが通らなくなる。`voicevox_chunk_secs: 120.0` を足し、
  `assert_eq!(loaded.voicevox_chunk_secs, 120.0)` を 1 行足す。
  ついでに `load_clamps_chunk_secs_out_of_range` を 1 件足す (壊れた / 手書きの
  `app_config.json` で 5 秒や 9999 秒が来たときに engine を落とさないことの回帰)。
- **`daw_gui/src/handler/project.rs:234-263` の `persist_app_config` も網羅 literal** なので
  `voicevox_chunk_secs: self.ui_prefs.voicevox_chunk_secs,` を足す (足さないとビルド不可)。
- `daw_gui/src/app.rs:315-340` 付近の `UiPrefs` 初期化に
  `voicevox_chunk_secs: app_config.voicevox_chunk_secs,` を追加
  (`state/ui_prefs.rs` にフィールド追加)。
- `AppEvent::SetVoicevoxChunkSecs(f32)` を `daw_gui/src/event.rs` に追加、
  `daw_gui/src/app.rs` の handler (`ToggleSettings` (:730) と同じ match) で
  `ui_prefs.voicevox_chunk_secs` をクランプして更新 → `persist_app_config()` →
  **`voicevox.voicevox_metadata_sent.clear()` → `sync_vocal_metadata()`**。
  §3.3 でキーに `chunk_secs` を混ぜたので、これは実際に**曲全体の再合成**になる
  (混ぜていなければ全 hit で無反応 = つまみが嘘になる)。
- `daw_gui/src/view/settings.rs`: 「テーマ」セクション見出し (:181-190) と同じ体裁で
  「VOICEVOX」セクションを足す。数値欄は inspector / export range と同じ idiom:

```rust
    let _ = ui.scrubable_number_at(
        "settings_vv_chunk",
        field_rect,
        f64::from(app.ui_prefs.voicevox_chunk_secs),
        f64::from(common::voicevox_phrase::DEFAULT_CHUNK_SECS),
        ScrubableNumberFormat::Integer,
        &ScrubableNumberStyle {
            font_size: 13.0,
            sensitivity: 0.5,
            // クランプは widget 側に持たせる (= 掴んで振り切っても範囲外にならない)。
            range: Some((
                f64::from(common::voicevox_phrase::MIN_CHUNK_SECS),
                f64::from(common::voicevox_phrase::MAX_CHUNK_SECS),
            )),
            ..ScrubableNumberStyle::from_palette(p)
        },
        |v| Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetVoicevoxChunkSecs(v as f32))
        }),
        None,
        None,
    );
```
  ラベルは「合成の塊の長さ (秒)」、下に小さく
  「短いほど 1 音直したときが速く、長いほど音量が揃う。既定 60 秒。
  **変えると曲全体を合成し直します** (前の設定の音はキャッシュに残るので、戻せばすぐ鳴ります)。」
  と添える。テーマ一覧の `list_rect` の高さ計算 (:194-200) をセクション 1 つぶん減らすこと。

### 5.7 `daw_gui/src/voicevox_client.rs` — 口パク query のディスクキャッシュ

現在 `query_phonemes` (:100-118) と `query_talk_phonemes` (:145-172) は
`regenerate_lipsync_for_track` から **クリップごとに毎回・キャッシュ無しで** HTTP を叩く。

```rust
pub fn query_phonemes(notes: &[Note], bpm: f32) -> Result<Vec<Phoneme>> {
    let query = build_sing_query(notes, bpm);
    let cache = common::voicevox_cache::VoiceVoxDiskCache::production();
    let key = common::voicevox_cache::key_for_sing_query(&query.json);
    if let Some(hit) = cache.as_ref().and_then(|c| c.get(key, CacheKind::Json))
        && let Ok(text) = String::from_utf8(hit)
    {
        return parse_phonemes(&text);
    }
    // …現行の HTTP…
    // 鍵空間は plugin host の塊クエリと共有なので、**正規化してから put する**
    // (生 body を置くと、向こうが 24 kHz 指定の query をスライスして
    // 24 kHz の WAV を得る。§3.1(e))。口パクは phoneme しか見ないので影響は無い。
    let body = common::voicevox::normalize_frame_query(&body);
    if let Some(c) = cache.as_ref() {
        c.put(key, CacheKind::Json, body.as_bytes());
    }
    parse_phonemes(&body)
}
```
`query_talk_phonemes` も同様に `key_for_talk_query(text, speaker_id)` でキャッシュする
(**speed は鍵に混ぜない** — `parse_talk_phonemes` が応答を受けた後で割るため、
話速を変えても query は再取得しなくてよい)。

これで、口パク再生成は「入力が変わっていないクリップ」では HTTP が丸ごと消える。

### 5.8 `daw_gui/src/view/voicevox_overlay.rs`

- module doc (:5-6) の「残り N トラック」を「残り N フレーズ」に。
- `:50` `let wav_n = app.voicevox_synth_busy_count();` →
  `let phrases_left = app.voicevox_pending_phrase_count();`
- `:77-79` のラベルを
  `lines.push(format!("VOICEVOX 合成中\u{2026}  残り {phrases_left} フレーズ"));`
- `:55` の早期 return 条件の `wav_n == 0` を、`phrases_left == 0 && !busy` に相当する形へ
  (busy だが pending がまだ 0 件の一瞬でパネルが消えないよう、`voicevox_any_generating()` を
  併用する)。**percent は出さない** (既存判断を維持)。

### 5.9 `daw_gui/src/view/arrangement_view.rs`

`draw_clip_synth_spinner` (:1140-1181):
- doc コメント (:1136-1139) の「歌唱/読み上げトラックが合成中ならそのトラックの全 clip に」を
  「そのクリップに未完了の合成があるクリップだけに」へ。
- `:1154` `let wav = app.track_wav_synthesizing(clip_key.track);` →
  `let wav = app.clip_wav_synthesizing(clip_key.track, clip_key.clip);`
- `:1147` の早期 return (`voicevox_synth_status.is_empty() && lipsync_inflight.is_empty()`) は
  **そのまま** (idle フレームで track 探索をしない最適化)。
- 呼び出し (`:223`) は変更不要。

### 5.10 WAV 書き出しの合成完了ゲート (`export.rs` / `ipc.rs` / `state/ipc.rs` / `automation_lanes.rs`)

**現状、曲全体の WAV 書き出しには合成の完了待ちが無い。** `PrepareVocalSynth` の送出元は
`daw_gui/src/handler/bounce.rs:330` (クリップ bounce) **だけ**で、
`handler/export.rs` は待たずに `ReinitAllPlugins` → `ExportWav` へ進む。今までは
「job 完了時に 1 回だけ publish」だったので、書き出しは**前回の完全な buffer**を掴んでいた。
逐次 publish にすると、ここが **部分ミックス**を掴む = 歌が途中まで、あるいは無音で
書き出される。§4.2 で `done_gen` を守っても、待つ人がいなければ意味がない。

`begin_wav_export` (`export.rs:140-163`) の `ReinitAllPlugins` の**前**にゲートを置く:

```rust
        self.transport.pending_export = Some((path, range, write_mod_sidecar));
        // r.md #75: 合成が終わる前に render すると部分ミックスが焼かれる。
        // 全 VOICEVOX device に最新メタデータを流し直して完了を待つ。
        // **reinit より前**に待つ — deactivate は synth thread を止めるので、
        // 走っている job があるとそこで捨てられ、done_gen が永久に追いつかない。
        let devices = self.all_vocal_synth_device_ids();
        if devices.is_empty() {
            self.send_plugin(PluginCommand::ReinitAllPlugins);
            return;
        }
        for &device_id in &devices {
            // bounce と同じ理由で差分キャッシュを迂回する (前回失敗していても再試行)。
            self.voicevox.voicevox_metadata_sent.remove(&device_id);
        }
        self.sync_vocal_metadata();
        self.ipc.pending_vocal_synth_export = devices.iter().copied().collect();
        for device_id in devices {
            self.send_plugin(PluginCommand::PrepareVocalSynth { device_id });
        }
```

- `daw_gui/src/state/ipc.rs` (`pending_vocal_synth_bounce` (:139-141) の隣) に:

```rust
    /// r.md #75: WAV 書き出し前の合成完了待ち。`PrepareVocalSynth` を送った device の
    /// 集合で、`VocalSynthReady` で 1 つずつ減らす。空になったら `ReinitAllPlugins` へ
    /// 進む。bounce (1 件) と違い **曲中の全 VOICEVOX device**が対象。
    pub pending_vocal_synth_export: std::collections::HashSet<u64>,
```

- `handler/ipc.rs` の `VocalSynthReady` arm (:207-231) の冒頭に:

```rust
                if self.ipc.pending_vocal_synth_export.remove(&device_id)
                    && self.ipc.pending_vocal_synth_export.is_empty()
                {
                    // 待ちが全部揃った。ここから先は現行の書き出し手順
                    // (reinit → PluginsReinitDone → ExportWav)。
                    self.transport.export_progress_at = Some(std::time::Instant::now());
                    self.send_plugin(PluginCommand::ReinitAllPlugins);
                }
```
  (`export_progress_at` を打ち直すのは、合成に掛かった時間を書き出し watchdog の
  60 秒に食わせないため。watchdog 側の除外条件は §5.3。)

- `handler/automation_lanes.rs` の `handle_child_disconnected` — 既存の
  「bounce の pending を畳む」ブロック (:1008-1015 付近) と同じ場所に、書き出しゲートの
  脱出口を足す。plugin host が落ちたら `VocalSynthReady` は永遠に来ない:

```rust
        if !self.ipc.pending_vocal_synth_export.is_empty() {
            self.ipc.pending_vocal_synth_export.clear();
            self.transport.pending_export = None;
            // export_stage は既に AudioRender なので、既存の脱出口がそのまま使える
            // (overlay / 入力 gate / temp WAV / SetRenderMode(Realtime) を畳む)。
            self.abort_audio_export(
                "子プロセスが切断されたため書き出しを中止しました".into(),
            );
        }
```

---

## 6. `daw_audio/src/sequencer.rs`

`collect_events_for_buffer` (:96-249) から **通し index の bookkeeping を全撤去**する。

- `:120-131` のコメント (「どちらでも builtin plugin 側の expected note_id とずれる可能性が
  ある」) を削除し、新しい説明に差し替える:

```rust
    // note_id は `(clip.id, note.id)` からの決定論的導出
    // (`common::plugin_metadata::sing_note_id`)。daw_gui の `sync_vocal_metadata` が
    // **同じ関数**で同じ値を flush するので、clip の追加 / 削除 / 並べ替え / muted で
    // 番号がずれない (旧「track 内通し index」の欠陥、アーキ不変条件 1)。
```
- `:133` `let mut note_id_base: u32 = 0;` と、`:148` / `:153` / `:161` / `:247` の
  `note_id_base += clip_note_count;` を **すべて削除**。`clip_note_count` (:142) も不要なら削除。
- `:171` `let note_id = note_id_base + note_idx as u32;` →
  `let note_id = common::plugin_metadata::sing_note_id(clip.id, note.id);`
  (`note_idx` が他で使われていなければ `for note in notes` へ戻す)。
- talk 側 (:293) は `talk_event_id` のまま **変更なし**。
- doc / コメントの訂正 (:19-22 / :256 / :421) は §3.4(c) に全列挙してある。
- テスト `muted_note_skipped_but_sibling_keeps_running_note_id` (:423-467) を書き換える。
  新しい不変条件は「muted sibling を skip しても、鳴る note の note_id は
  `sing_note_id(clip.id, note.id)` のまま」。テスト名も
  `muted_note_skipped_but_sibling_keeps_stable_note_id` へ。
  期待値は `note_id == 1` (= enumerate index) から
  `sing_note_id(clip.id, 2)` へ変わる (fixture の 2 音目は `id: 2`)。
  さらに `note_id_is_unaffected_by_inserting_a_note_before_it` を追加
  (= 今回直した欠陥の直接の回帰テスト)。
- **フィクスチャの `Note.id` は確認済みで作業不要**: `one_note_song` は `:347` が `id: 1` /
  `:355` が `next_note_id: 2`、もう一方は `:745` `id: 1` / `:754` `id: 2` /
  `:763` `next_note_id: 3` で、ファイル全体に `id: 0` は 1 件も無い。
  **新しく足すフィクスチャでも 0 にしないこと** (0 のままだと 1 clip 内の全 note が
  同じ `sing_note_id` になる)。

---

## 7. 検証

### 7.1 コマンド (この順で)

```
make check
make clippy
make test-nolaunch
make arch-lint
```
- **protocol を変えるので、実機確認の前に必ず `cargo build --workspace`**
  (子 exe が古いと bincode decode 失敗 = 「再生が止まる」形で出る)。
- `make test` は daw_gui を起動するので、**この計画の検証手順には入れない**。
  daw_gui 側の対象テストだけを見たいときは
  `cargo test -p daw_gui --test voicevox_progress` のようにピンポイントで指定する。

### 7.2 自動テスト一覧 (engine 不要)

| 場所 | テスト |
|---|---|
| `common/src/voicevox.rs` | `note_placements_report_truncated_end_frames` / `carry_vowel_flows_across_rests` / `carry_in_restores_prolongation_across_a_split` / `edge_rest_frames_shifts_every_placement_uniformly` + 既存 8 件の期待値更新 |
| `common/src/voicevox_phrase.rs` | `phrases_break_only_on_gaps` / `phrases_never_cross_speakers` / `phrases_ignore_clip_boundaries` / `carry_in_flows_between_phrases` / `chunks_cut_at_the_longest_rest` / `chunk_never_splits_a_phrase` / `chunks_are_deterministic` |
| `common/src/voicevox_cache.rs` | 既存 5 件の移設 + `phrase_key_changes_with_chunk_secs` / `prune_counts_json_toward_budget` / `get_touches_mtime_for_lru` |
| `common/src/plugin_metadata.rs` | `sing_note_ids_stay_below_talk_band` / `sing_note_id_is_stable_against_sibling_insertion` / `sing_and_talk_id_spaces_do_not_overlap` + 既存 literal 2 件に `clip_id` |
| `common/src/protocol.rs` | `builtin_note_metadata_roundtrip` (更新) / `vocal_synth_progress_roundtrip` / `vocal_synth_priority_roundtrip` |
| `daw_gui/src/app_config.rs` | `save_load_roundtrip` (網羅 literal の更新。放置するとビルド不可) / `load_clamps_chunk_secs_out_of_range` |
| `daw_plugin_host/src/builtin/voicevox_synth.rs` | `slice_frame_query_cuts_arrays_and_phonemes` / `slice_frame_query_rejects_phonemes_without_frame_length` / `frame_query_len_reads_f0` |
| `daw_plugin_host/src/builtin/voicevox_render.rs` | `seams_tile_without_overlap` / `long_rest_leaves_intentional_silence` / `window_is_inside_the_chunk_query` / `crossfade_ramps_sum_to_unity` / `note_offset_lands_on_the_note_beat` / `mix_buffer_incremental_equals_full_remix` |
| `daw_plugin_host/src/builtin/voicevox.rs` | 既存の配置テスト 3 件を `MixBuffer` へ移植 (契約は不変) |
| `daw_audio/src/sequencer.rs` | `muted_note_skipped_but_sibling_keeps_stable_note_id` (書き換え) / `note_id_is_unaffected_by_inserting_a_note_before_it` |
| `daw_gui/tests/voicevox_progress.rs` | `VocalSynthProgress` 対応に更新 + `pending_clips` によるクリップ単位スピナー判定 (`clip_wav_synthesizing`) / `voicevox_pending_phrase_count` / **talk (Text clip) にもスピナーが点くこと** (`TalkMetadata.clip_id` 経由) |

`daw_gui/tests/voicevox_progress.rs` は `CARGO_BIN_EXE_daw_gui` を含まない (grep 済) ので
`cargo test -p daw_gui --test voicevox_progress` で単体実行してよい。
`split_glue_smoke.rs` は含む (= daw_gui を起動する) が、§5.4 のとおり glue には手を入れないので
このテストは対象外。

**単純ケースにテストを書かない** (プロジェクト規約)。上の各件は「本番の算術をテストへ写す」
ものではなく、分割・継ぎ目・id 空間という**壊れると音が変わる不変条件**の回帰テストである。

### 7.3 実 engine を要する統合テスト (`#[ignore]`)

既存 idiom (`daw_plugin_host/src/builtin/voicevox_synth.rs:448-463`) に合わせ、
`#[ignore = "requires a running VOICEVOX engine at localhost:50021"]` を付ける。
**engine を起動するテストは既定では走らせない。**

`daw_plugin_host/src/builtin/voicevox_render.rs` に 1 件:

```rust
/// 受け入れ条件を機械で測る: 同じ楽譜を
///   (a) 全体 1 クエリ + 全体 frame_synthesis
///   (b) 60 秒の塊クエリ + フレーズ単位 frame_synthesis
/// で描き、フレーズごとの RMS のばらつき (dB) の **σ が 0.5 dB 以下**であること。
/// 実測は σ 0.41 dB (docs/plan_rmd_75_voicevox_phrase.md §0 (C))。
/// bit 一致は原理的に成立しない (`/sing_frame_audio_query` が非決定的) ので比べない。
///
/// **この条件が保証するのは「一貫した塊分けで一度に描いたとき」だけ** (= probe16 の再現)。
/// 実運用ではキャッシュが効き、**別々の塊構成で合成されたフレーズが 1 本の buffer に
/// 混ざる** (それがこの設計の狙いでもある)。その patchwork 状態の音量ばらつきは
/// **まだ測っていない** — 塊構成をキーに入れない以上、測れる形にするには
/// 「編集を繰り返した曲」を再現する必要があり、それは実機 sign-off (§7.4-8) に回す。
/// 数字が無いまま閾値を作らない。
#[test]
#[ignore = "requires a running VOICEVOX engine at localhost:50021"]
fn chunked_render_matches_whole_render_within_half_db_sigma() { … }
```

もう 1 件、長い曲が通ることの回帰:

```rust
/// 5 分 (= 単発 frame_synthesis が HTTP 500 になる長さ) の楽譜でも、全フレーズが
/// 合成できること。実測 211 フレーズ / 失敗 0 / 合計 30.68 s。
#[test]
#[ignore = "requires a running VOICEVOX engine at localhost:50021"]
fn five_minute_song_renders_every_phrase() { … }
```

### 7.4 実機 sign-off (ユーザーに依頼する項目)

**起動前に必ず一声かける。** headless で切り分けられるものは自分で済ませ、最後に一度だけ依頼する。

1. 長い歌唱トラックで 1 ノートの音程を変える → **1 秒以内に**音が変わること。
   全体オーバーレイが「残り 1 フレーズ」を出して消えること。
2. スピナーが **直したクリップにだけ**出ること。読み上げ (Text) クリップの合成中にも
   そのクリップにスピナーが出ること (`TalkMetadata.clip_id`)。
3. Undo → Redo で **HTTP なしで即座に**戻ること (ログに `cache = "hit"` が並ぶ)。
4. 5 分級の曲を新規に合成 → 最後まで鳴ること (現行は engine 500 で無音になる)。
5. 歌唱クリップの Bounce / **曲全体の WAV 書き出し** → **全フレーズぶんが入っている**こと。
   特に **キャッシュが空の状態から書き出しを始めて**、合成の完了を待ってから
   render が始まること (§5.10。待たないと部分ミックスが焼かれる)。
6. 口パクが音声とずれていないこと (`REST_FRAMES` / `sing_head_beat` は触っていないので不変のはず)。
7. 再生しながら編集 → 再生位置の近くから先に音が戻ること。
8. **編集を何度も繰り返した曲**を通しで聴いて、フレーズの継ぎ目に音量の段差や
   途切れが無いこと (= §7.3 が測れていない patchwork 状態の確認。ここだけは耳で見る)。
9. 設定の「合成の塊の長さ」を 60 → 120 に変える → 曲全体が合成し直され、
   **60 に戻すと即座に (HTTP なしで) 元の音に戻る**こと (キー設計 §3.3 の確認)。

---

## 8. ドキュメントの更新

- `docs/plan_voicevox_progress.md:21-27` — 「WAV 合成は FIXME #36 で音量一貫性のため
  『声(speaker)単位まとめ合成』を採用しており、clip 別合成はしない設計 → WAV 側の最小粒度は
  トラック単位」を、**「最小粒度はフレーズ」**へ書き換える。§「進捗 % が作れない理由」の
  「声/トラック単位の…件数が最も正直な粒度」も **フレーズ単位** へ。
  「percent は出さない」という判断自体は維持する。
- `docs/plan_voicevox_synth.md:97-108` (PR-V2.4) — 「track 内全 clip の notes を flatten した
  『通し index』を note_id として振る」の記述に、
  **「r.md #75 で `sing_note_id(clip_id, note.id)` へ置換済 (歴史的経緯)」** の追記をする。
- `docs/plan_voicevox_clip_voice.md` — 「clip 単位で声を分ける」記述は生きているが、
  「合成の単位」がフレーズになった旨を 1 節足す (声の解決は per-clip のまま)。
- `docs/plan_pakupaku.md` §7 — 口パク query がディスクキャッシュを持つこと、
  合成側とはクエリが別物なので共有しないことを追記。
- 本書 (`docs/plan_rmd_75_voicevox_phrase.md`) が **#75 の設計正本**。上記 4 本からリンクする。
- **`r.md` は編集しない。**

---

## 9. 落とし穴 (先に読むこと)

1. **クエリ出力をキャッシュキーにしない。** `/sing_frame_audio_query` は非決定的
   (同じ楽譜で max|Δf0| 31.7 Hz)。キーは必ず「入力の楽譜」から作る。
   `/frame_synthesis` のほうは決定的なので、WAV キャッシュは正しく効く。
   **同時に、合成結果を変える入力 (`chunk_secs`) はキーに入れる。**
   入れないと設定つまみが全 hit で無反応になる (§3.3)。
2. **`done_gen` を逐次 store で進めない。** 進めると bounce / 書き出しが部分ミックスを掴む。
   `PrepareVocalSynth` → `VocalSynthReady` の 30 秒固定 deadline も同じ理由で撤去する。
   **待つ側も要る** — 曲全体の WAV 書き出しには今まで合成完了ゲートが無かった (§5.10)。
3. **playhead を `SetBuiltinPluginNoteMetadata` に相乗りさせない。**
   再送デデュープ (`voicevox_metadata_sent`) と正面衝突する。専用の軽量 command を使う。
4. **`REST_FRAMES` / `sing_head_beat` / `sing_base_beat` を触らない。**
   口パク配置 (`common/src/lipsync.rs:74-82`) と合成 wav の配置
   (`daw_plugin_host/src/builtin/voicevox.rs:107-115`) が同じ値に乗っている。動かすと
   音声と口のズレとして即座に出る。今回の設計はこれらを一切変えずに成立する。
5. **長音符「ー」の持ち越し母音を必ずフレーズへ引き渡す。** 引き渡さないと、先頭が裸の「ー」の
   フレーズが fallback の「あ」になり、**継ぎ目の息づかいとは無関係に歌詞が変わる**。
   そして carry はキャッシュキーにも入れる (フレーズ単体 query JSON から作れば自動的に入る)。
6. **塊の継ぎ目もフレーズの継ぎ目と同じ扱いにする。** 別扱いにすると、塊境界だけ
   二重発音 (重なり) か欠落が起きる。**継ぎ目は塊の frame 空間ではなく曲 sample 空間で、
   隣り合う 2 フレーズの拍だけから計算する** (§4.2 手順 5)。こうすると両隣が同じ値を出し、
   キャッシュ hit で塊クエリを持っていないフレーズも同じ式で置ける。
7. **音量補正を実装しない。** 3 通り試して全部失敗している (§0 (F))。
   文脈が変わると包絡の形ごと変わるので、掛け算では戻せない。
8. **`Note.id` の一意性が新しい前提になる。** 全生成経路を確認済みで
   **コード変更は不要** (§5.4 の表)。Glue は既に採番し直している (`glue.rs:358-360`)。
   テスト用フィクスチャで id を 0 のままにしないことだけ守る。
9. **RT 制約**: audio half に足すのは quiescence epoch の atomic RMW 2 回だけ
   (確保・ロック・I/O は無い)。`ArcSwapOption::load()` のみ (`load_full()` 禁止) を維持。
   フレーズ差分 mix・decode のメモリキャッシュ・GC はすべて synth thread (非 RT)。
   **「retire された `Arc` は writer 側で drop される」は誤り。**
   arc-swap 1.9.1 の hybrid 戦略では、`store` した writer が `Debt::pay_all`
   (`src/debt/mod.rs:81-108`) で未払い debt ごとに strong count を +1 し、その後
   `HybridProtection::drop` (`src/strategy/hybrid.rs:105-125`) が
   「debt が既に払われていたら自分で `ManuallyDrop::drop`」= **RT が最後の所有者になり得る**。
   publish 頻度が約 60 倍になるので、§4.2 手順 9 の quiesce + buffer プールで
   構造的に潰す (writer が必ず 1 本保持している状態を作る)。
9b. **同じ鍵空間へ入れる値は正規化してから put する。** daw_gui の口パク query と
   plugin host の塊クエリは `key_for_sing_query` を共有する。片方が
   `outputSamplingRate` 未 patch の生 body を置くと、もう片方がそれをスライスして
   `/frame_synthesis` に投げ、24 kHz の WAV が 48 kHz の buffer に混ざる (§3.1(e))。
9c. **塊 query の placement とフレーズの note を ordinal で対応付けない。**
   塊 query は自分の base_beat で丸めるので、単体 query では残った note が
   塊では落ちることがある (`common/src/voicevox.rs:203-215` の「1 frame 未満は落とす」)。
   `NotePlacement.index` が `chunk_notes` の index 範囲に入るかで引くこと (§4.2 手順 6)。
10. **`cargo build --workspace` を忘れない。** `NoteMetadata` の doc / `PluginCommand` /
    `PluginEvent` を変えるので `PROTOCOL_FINGERPRINT` が変わる。子 exe が古いと
    handshake で弾かれる (それが検出網の目的なので、正しく作り直せばよい)。
11. **god file budget**: `daw_plugin_host/src/builtin/voicevox.rs` は現在 1,300 行。
    レンダリングは `voicevox_render.rs` へ出すので増えない (むしろ synth thread の
    inline 実装が減る)。3,000 行に近づいたら更に分ける。
12. **キャッシュ世代を上げるので、実装直後の 1 回だけ全曲再合成が走る。**
    ユーザーに事前に伝えること (「直した直後は遅い」という体験になる)。
    旧 1 GB のエントリは prune が mtime 順に押し出す。

---

## 10. 計測の常設

`voicevox_render` のフレーズ処理ループに、1 フレーズ 1 行で出す:

```rust
tracing::info!(
    phrase = phrase_index,
    speaker = phrase.speaker_id,
    frames = b - a,
    cache = if hit { "hit" } else { "miss" },
    query_ms,      // その塊のクエリに掛かった時間 (このフレーズで初めて取ったときだけ非 0)
    synth_ms,
    "voicevox phrase rendered"
);
```
synth thread は RT ではないのでログ可。現状は計測が一切無く、
`voicevox_synth.rs:67-69` の「数十秒かかり得る」という**推定コメント**しか無い。
これを実測に置き換えるのが本項の目的。

---

## 11. 裏取りの結果 (2026-08-28、実コードで再確認したうえでの反映)

初版に対する裏取りレビューの指摘を 1 件ずつ実コードで確認した。反映先と、
**指摘のほうが誤っていた分**の根拠を残す (黙って無視しない)。

| 指摘 | 判定 | 反映 |
|---|---|---|
| `note_offsets` の `- REST_FRAMES` は 10 frame 早い | **正** (`voicevox.rs:112-115` の `sing_place_samples` が `sing_head_beat` 経由で既に REST を引いており、正しい合成 `voicevox.rs:612-623` は `place + off`) | §4.2 手順 3 で `- REST_FRAMES` を削除。旧「±1 frame」リスクは根拠ごと撤回 |
| `chunk_secs` を変えても再合成されない (キーに入らない) | **正** | §3.3 でフレーズ WAV キーに `chunk_secs` を追加。§3.5(a) / §5.6 の文言も「実際に全曲再合成になる」へ |
| キャッシュ共有が sample rate を汚染する | **正** (`QUERY_SPEAKER=6000` は `voicevox_synth.rs:88` / `voicevox_client.rs:105` で一致、鍵は実際に衝突し得る) | §3.1(e) で `normalize_frame_query` を common へ。§4.1 / §5.7 の両方で put 前に正規化 |
| Glue の note id 重複は既に解決済み | **正** (`glue.rs:358-360` が v29 で採番し直している) | §5.4 を「全経路を確認 → 変更なし」の表に置換。テストも足さない |
| 嘘になる doc の列挙が不完全 | **正** | §3.4(c) に 6 ファイルぶんを全列挙。`process_data.rs:133-140` も **「(= first note in the track)」だけは旧仕様なので直す** (レポートは「中立で変更不要」としていたが、この括弧書きは通し index 前提) |
| talk クリップにスピナーが点かない | **正** (`TalkMetadata` に `clip_id` が無い) | §3.4(b) で `TalkMetadata.clip_id` を追加 (event_id からの逆算はしない)。§7.4-2 に sign-off 項目 |
| 逐次 publish の mix コストと arc-swap の retire | **正** (arc-swap 1.9.1 `src/debt/mod.rs:81-108` / `src/strategy/hybrid.rs:105-125` を読んで確認。RT が最後の所有者になり得る) | §4.2 手順 8-9 を差分 mix + buffer プール + RT quiescence epoch へ全面書き換え。§9-9 の誤った断定を撤回 |
| 曲全体 WAV 書き出しに合成ゲートが無い | **正** (`PrepareVocalSynth` の送出元は `bounce.rs:330` のみ) | §5.10 を新設 (export.rs / ipc.rs / state/ipc.rs / automation_lanes.rs / tick.rs)。watchdog 誤発火も同時に潰す |
| フレーズ境界と塊 query の対応が未定義 | **正** | §4.2 手順 6 で「`NotePlacement.index` の範囲で引く / 0 件なら skip + warn」を明文化 |
| `seam_windows_tile_without_overlap` は成立しないケースがある | **正** | 継ぎ目を曲 sample 空間へ移したうえで、テストを「短い休符 = 敷き詰め」「長い休符 = 意図した無音」の 2 件に分割 (§4.2 テスト) |
| 受け入れ条件が patchwork をカバーしない | **正** | §7.3 に「測っていない」と明記し、§7.4-8 の実機 sign-off へ回す。数字が無いまま閾値は作らない |
| `frameLength` 両対応は「実測で確認済」ではない | **正** (probe は防御的に両対応しているだけで、`frameLength` が返った出力は 1 件も無い。engine 側は `song_engine.py` の `phoneme.frame_length`) | §4.1 で断定を撤回し `frame_length` 固定 + 欠損は `Err` へ。camelCase のテストも削除 |
| `app_config.rs` の網羅 literal でビルドが壊れる | **正** (`:119-150`) | §5.6 に明記。さらに `persist_app_config` (`handler/project.rs:234-263`) も同じ網羅 literal なので追加 (レポート未指摘) |
| `SingQuery` の derive が落ちている | **正** | §3.1(a) で維持を明記 |
| §6 の「テスト用 Note に非 0 の id」は既に満たされている | **正** (`sequencer.rs` に `id: 0` は 0 件) | §6 を「確認済み・作業不要。ただし新規 fixture でも 0 にしない」へ |
| 行番号のずれ (`synthesize_talk` :257 / `synthesize_talk_for_builtin` :307 / `put_inner` :126-129 / `clip_id: clip.id` :359 / `track_wav_synthesizing` :1153) | **正** | 全部訂正。あわせて `apply_voicevox_synth_status` :186 / overlay :55,:77 / `draw_clip_synth_spinner` :1140 / `PREVIEW_NOTE_ID` :49 / `prune` :154-156 / `on_vocal_synth_status` :794 も再確認して修正 |
