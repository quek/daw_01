# パーノート変調 (r.md #117)

grill-me (2026-09-07) の決定: **Bitwig 流ポリフォニック変調** (Q1 = 1)。 指定は retrigger トグルの
3 番目 `Note` (Q2 = 1)。 note expression / ノート由来ソースは含めない (別件)。

## 0. 何が変わるか

- モジュレーター (LFO / Random / MSEG / Steps) の retrigger に **`Note`** が増える。 `Note` のソースは
  「鳴っている各ノートの note-on を起点に 1 本ずつ走る」。 新設の **ADSR エンベロープ** は常に
  ノート起点 (retrigger 欄なし)。
- routing 先が **CLAP の per-note 変調対応 param** (`CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID`) なら、
  ノートごとに別の値で `clap_event_param_mod { note_id, key, channel }` を送る。
- それ以外の先 (builtin / VST3 / 非対応 param / トラックの音量など) は **最新ノート** の値で
  global に落とす (従来の値面に載る)。 GUI のプレビュー / ツマミの live tick もこの値。

## 1. モデル (`common::model::modulation`)

```rust
pub enum RetriggerMode { FreeRun, FromBeat { anchor_beat }, Note }   // Note を追加
pub struct AdsrConfig { attack_ms, decay_ms, sustain (0..=1), release_ms }
pub enum ModSourceKind { …, Adsr(AdsrConfig) }
pub enum ModParam { …, AdsrAttack, AdsrDecay, AdsrSustain, AdsrRelease }  // 全部変調先になる
```

- `Note` は曲位置の純関数ではなく **(note-on 時刻, 曲位置) の純関数**。 シーケンサのノートは
  決定論的なので書き出しは再現する。 ライブ入力 / 鍵盤プレビューのノートは再現対象外。
- ADSR は秒基準 (Bitwig の ADSR と同じ ms)。 attack は線形、 decay / release は指数
  (`exp(-3 t / T)` で T の後に 5% 以下)。 note-off 後は release、 終端で voice 終了。

## 2. ボイス表 (daw_audio、 RT)

- **plugin instance ごと** に [`VoiceTable`](../daw_audio/src/graph/voices.rs) (`ChainProgram::voices`、
  `ChainOp::Plugin::voice_slot` で引く。 再 compile 跨ぎは device id で移送)。 その plugin が受けた
  note-on / note-off を `run_plugin` が記録する = plugin が見るノートと同じ集合 (note_out device で
  変換された後)。 容量 64、 溢れたら最古を捨てる。
- 終了条件: CLAP の `CLAP_EVENT_NOTE_END` (plugin がボイスを閉じた通知、 `EventKind::NoteEnd` で
  engine へ返す) / note-off から [`VOICE_TAIL_SECS`] 経過 / 同じ note_id の再 note-on。 VST3 は
  note_end が無いので後者 2 つ。
- **最新ノート** (global 落とし込み用) は **トラックの device chain 入力** の note-on を
  `PerTrackState::latest_note` に記録する (`process_track_owned` が `midi_bus_a` から拾う)。

## 3. 評価

- per-note: `fill_pd_param_events` (plugin ごと) が、 `Note` / ADSR ソースを source にする
  **この device 宛の routing** について、 voice × 刻みで値を出し `ParamMod { note_id, key, channel }`
  を積む。 値は閉形式 (`generator_scalar` に anchor = note-on の beat / secs を渡す。 rate 等の
  変調は global の値面の実効値を使う = 位相だけノート起点)。 同じ routing の **global 値**
  (最新ノート) も従来どおり積む — どちらを使うかは host が param のフラグで決める。
- global 値面 (`mod_graph::tick`): `Note` / ADSR の slot は engine が刻みごとに
  `ModRuntime::set_note_anchor(slot, anchor)` で最新ノートの起点 (無ければ None = 開始値) を書く
  (フォロワーの `set_follower` と同じ contract)。 tier は常に Closed。
- `ParamMod` に `note_id: i32 (-1 = global) / key: i16 / channel: i16` を足す (shmem ABI、
  `make build`)。 容量は従来の [`MAX_PARAM_MODS`] の間引きロジックが吸収する (param 数 × voice 数で割る)。

## 4. plugin host

- CLAP: `param_meta` に `per_note: bool` (`MODULATABLE_PER_NOTE_ID`) を持つ。 buffer 内に
  その param 宛の per-note mod が 1 件でもあれば **global mod は捨てる** (二重掛けしない)。
  per-note 非対応 param 宛の per-note mod は捨てる (global が効く)。 出力 event の `NOTE_END` を
  `events_out` (`EventKind::NoteEnd`) で返す。
- VST3: per-note mod は捨てる (global のみ)。 note expression は別件。

## 5. GUI

- retrigger トグル: `Free` → `⟲here` → `Note` の循環。 `Note` のプレビューは「beat 0 で 1 ノートが
  鳴った」 波形 (anchor 0)。
- **カーソルはボイスごと** (Bitwig の per-voice 表示): engine が track ごとの鳴っているボイス
  (chain 最初の plugin の `VoiceTable`、 上限 `MAX_PUBLISHED_VOICES`) を `AudioBridge` の
  seqlock 面に毎 buffer publish し、 GUI の poller が `TrackVoicesTick` で `transport.track_voices`
  へ写す。 `Note` 起点の LFO / Random / Steps / MSEG はボイスごとに `ModTime::at_note` で位相を
  出し、 ADSR はプレビューと同じ時間軸 (押している間は保持区間の端で止まり、 離したら R を進む)
  にボイスを置く。 鳴っていなければカーソル無し。
- ADSR 本体: プレビュー (1 拍押して離す) + `A [ms] D [ms] S [0..1] R [ms]` (`mod_param_field`、
  全部 ◉ で変調先)。 add-menu に `ADSR`。
- ツマミの live tick / 深さ帯は global 値 (最新ノート)。
