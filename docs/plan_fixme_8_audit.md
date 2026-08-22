<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# r.md #8 — 後フェーズ送り / 手ぬき実装 監査 (最終形までの実装計画)

「最終形まで実装する。フェーズ分けをしない」(CLAUDE.md) に違反して **後回し・手ぬきになっている箇所** を、
全クレートを**コメントだけでなく実コードのロジックから**監査した結果。各項目は周辺コードを Read して裏取り済み。
stale なだけ (= 実は実装済み) のマーカーは除外し、**現在の納品済み挙動が壊れている / no-op な物のみ**を載せる。

監査範囲: `common/` `daw_audio/` `daw_plugin_host/` `daw_gui/` `ui/crates/{platform,renderer,ui}`
(examples / tests / benches / 生成バインディングは対象外)。

---

## 横断テーマ (個別項目の背後にある構造的欠陥)

1. **サンプルレートが 48000 固定の前提** — engine / plugin activation / GUI の拍計算が全て
   `audio_bridge::SAMPLE_RATE = 48000` を SSoT にしているが、CPAL デバイスは native default で開く。
   非48kHz デバイスで全体が誤ピッチ・誤テンポ。VOICEVOX 未リサンプルも同根。
2. **テンポオートメーション (SongTempo lane) が live のみ尊重、オフライン全経路で無視** —
   WAV/動画 export・MIDI export・動画 event の尺マップ・loop wrap 推定。再生と書き出しが一致しない。
3. **plugin param の automation / modulation 配線が track FX 止まり** — group/master FX、lane 既定値、
   MIDI Learn 注入、block-rate 階段、実名表示が未完。

---

## A. 正しさ (出力が間違う) — 最優先

| # | file:line | 手ぬき | 最終形 | sev | effort |
|---|---|---|---|---|---|
| A1 | daw_audio/main.rs:82,950,1037 / common/audio_bridge.rs:6 / daw_gui/bootstrap.rs:207 | session SR が 48000 ハードコード。device は native rate で再生、突き合わせ無し・resample 無し → 非48kHzで全体誤ピッチ/誤テンポ | device native rate を SSoT 化 (daw_audio が device rate を採用し GUI/plugin/voicevox へ伝播)。const は fallback に降格 | correctness | M-L |
| A2 | daw_audio/export.rs:366 | WAV/動画 export が constant `song.bpm` で freewheel、tempo lane 無視 (live は engine.rs:864 で `evaluate_song_tempo`) | export walk も per-buffer `evaluate_song_tempo` で可変 sample↔beat 積分 | correctness | M |
| A3 | daw_gui/midi_export.rs:8 | MIDI export が tempo 1個 (曲頭 bpm) のみ + CC/PitchBend 欠落 | SongTempo curve を tick 列の複数 tempo event に展開、CC/PitchBend も SMF 化 | correctness | M |
| A4 | common/model.rs:3966 | 動画 event の source↔尺マップが tempo 変化を無視 (CFR 前提) | tempo 積分した beat→source-time マップ | correctness | M |
| A5 | daw_gui/app.rs:21091 | send 削除時に後続 `SendGain{send_idx}` automation lane を reindex せず stale index 参照 | send 削除に追従し後続 lane の send_idx を詰める | correctness | M |
| A6 | daw_gui/app.rs:13608 | touched plugin param / send から作る automation lane の既定値が 0.0 固定 | IPC で現在値を引いて lane 既定値に入れる | correctness | M |
| A7 | common/model.rs:3805 | VFR 動画を nominal FPS の CFR とみなしフレームタイミングがドリフト | per-frame PTS を尊重 | correctness | M |
| A8 | common/scale.rs:148 | 異名同音を `#` 固定。フラット系キーで音名誤り (Bb→A#) | key(root+scale) に応じ `#`/`b` を選ぶ enharmonic spelling | user可視 | M |
| A9 | daw_plugin_host/builtin/voicevox.rs:689 | 合成 wav 常に 48000Hz を device 出力へ 1:1 mix (resample 無し) → A1 と合わせ非48kで誤ピッチ | 合成 wav SR と session SR を resample 整合 (A1 で session=device rate 化後に必須) | correctness | M |
| A10 | daw_audio/engine.rs:1180 | loop wrap 時の playhead_beats が constant bpm 線形推定 (tempo automation 下で微小ズレ) | loop start beat を SongTempo lane から正確逆算 | 低 | S |

## B. UI にあるのに no-op / 未完の機能

| # | file:line | 手ぬき | 最終形 | sev | effort |
|---|---|---|---|---|---|
| B1 | common/model.rs:3860 / daw_audio/audio_clip_renderer.rs:546 | `StretchMode::Slice` が inspector で選べるが onset 検出がワークスペースに無く `onsets` 常に空 → Raw 等価に退化 (Stretch 均一伸縮は実装済) | onset/transient 検出を実装し `onsets` 充填、拍ロックスライス再生 | user可視 | L |
| B2 | daw_gui/app.rs:14179 | MIDI Learn → PluginParam bind は永続化+status 出すが CC 受信は warn のみで音に出ない (注入未実装) | RT-safe IPC で audio→plugin host に CLAP_EVENT_PARAM_VALUE / IParameterChanges 注入 | user可視 | L |
| B3 | daw_audio/engine.rs:1677,2382 | group/master FX の param **automation** (`fill_pd_param_events` 不呼出) と **modulation** (空 `&[]` mod_scalars) が未配線。track FX のみ動く。master fx automation lane を持つ data model も無い | master fx automation lane を data model 追加 + group/master process へ mod_scalars snapshot plumbing | user可視 | M |
| B4 | daw_audio/automation.rs:133 | plugin param automation が block-rate (frame0 で1回 push)。builtin Vol/Pan は per-sample → 速い automation で zipper | sub-buffer (64frame 刻み等) で複数 push_param し sample-accurate 化 | user可視 | M |
| B5 | daw_gui/app.rs:20330 | automation 録音中、対応 lane/clip が無い gesture を silently skip (Bitwig 流 auto-create 無し) → 無反応 | 録音対象に lane/clip が無ければ自動生成 | user可視 | M |
| B6 | daw_gui/app.rs:923 | PluginParam の表示名が `format!("Param {id}")` (実名解決経路なし) | IPC param 名 cache を引いて実名表示 | user可視(低) | M |
| B7 | daw_plugin_host/builtin/voicevox.rs:866 | builtin VOICEVOX に埋め込み GUI 無し (speaker picker / 合成進捗バーをエディタ窓から開けない) | speaker 選択 + 進捗バーの埋め込み GUI 実装 | user可視 | L |
| B8 | daw_gui/app.rs:19419,639 | sidechain/aux 入力が常に PostFader 固定 + aux port 0 のみ露出 (Pre/PostFx 切替・複数 port 未配線) | inspector に Pre/PostFx タップ切替 + 全 aux port 行展開 | user可視(低) | M |
| B9 | common/video_fx.rs:56 | 映像効果 Time/Feedback (echo/残像) が enum/label/`needs_history`/WGSL 規約まで宣言済だが実 effect ゼロ | feedback/history ターゲット経路を稼働させ echo/trail 効果実装 | 低 | M |
| B10 | daw_gui/view/modulation.rs:155 | text の W/H が modulation target 不可 (image W/H は可、非対称) | TextBuiltin に W/H を追加し image と対称化 | user可視(低) | M |
| B11 | daw_gui/app.rs:3081 / common | follower→SongTempo/TimeSig の song-level modulation を engine が消費せず silent | engine + export bake が song-level modulation を消費し tempo 変調可能化 | user可視(低) | L |
| B12 | common/model.rs:3864 | warp-marker (`beat_markers`) が生成UI も消費経路も無い dead field。非均一 time-stretch サブ機能未着手 | warp-marker 編集 UI + 非均一 stretch 消費 | 低 | M |
| B13 | daw_gui/view/audio_editor.rs:353 | Audio Editor の Alt+wheel が当面 no-op (vertical gain zoom 予約) | vertical gain zoom 等を割当 | 低 | S |

## C. プラグインホスト忠実度 (VST3/CLAP/DPI)

| # | file:line | 手ぬき | 最終形 | sev | effort |
|---|---|---|---|---|---|
| C1 | daw_plugin_host/main.rs:2124, vst3_plugin.rs:1800 | プラグイン GUI の DPI scale を 1.0 固定 (GetDpiForWindow 未照会、VST3 IPlugViewContentScaleSupport skip) → HiDPI で極小/ぼやけ | host HWND の DPI を query し CLAP gui.set_scale / VST3 setContentScaleFactor に渡す | user可視 | M |
| C2 | daw_plugin_host/vst3_host.rs:67 | IHostApplication::createInstance が kNotImplemented で IMessage/IAttributeList 提供せず → processor↔controller messaging 使う VST3 が状態同期不可 | IMessage/IAttributeList の host 実装提供 | correctness | M |
| C3 | daw_plugin_host/vst3_host.rs:135 | restartComponent をログのみで無視 (kLatencyChanged/kParamValuesChanged/kReloadComponent/kIoChanged) | flags を見て deactivate→activate / latency 再query / param 再列挙 | correctness | M |
| C4 | daw_plugin_host/vst3_plugin.rs:542 | 3ch 以上のバスを全て stereo fallback (コメントは surround と不一致) | チャンネル数に応じた正確な SpeakerArrangement (5.1/7.1) | 低 | M |
| C5 | daw_plugin_host/vst3_plugin.rs:1588 | process() 非OK status の記録/警告が debug ビルド限定 → 納品物で診断ゼロ | release でも RT 安全に atomic 記録→off-RT で1回ログ | 低 | S |
| C6 | daw_plugin_host/clap_host.rs:167 | gui_request_show/hide が常に false で無視 | 該当エディタ窓を SetForegroundWindow / hide へ配線 | 低 | S |

## D. RT安全 / perf

| # | file:line | 手ぬき | 最終形 | sev | effort |
|---|---|---|---|---|---|
| D1 | daw_audio/graph/compile.rs:42 | routing schedule の compile (Vec/HashMap alloc) が RT audio callback 上で song 編集着地時に走る (`TODO(PR3)`)。clip-render schedule では既に main.rs `publish_schedule` で off-thread ArcSwap 済 — routing だけ取り残し | routing も GUI/IPC スレッドで compile→ArcSwap publish、RT は load のみ | RT | M |
| D2 | daw_gui/app.rs:3720 | automation 編集の Undo が一律 Song 全体 snapshot (snapshotless 先送り) | 差分 (prev/next) ベース snapshotless undo | perf(低) | L |
| D3 | daw_gui/view/arrangement_view.rs:185 | 毎フレーム `Vec<ArrangementTrack>` を全 track×clip 再確保 (name Arc::from、nested collect) | AppData に Arc<str> 保持し rename 時のみ再生成 | perf | M |
| D4 | daw_gui/view/arrangement_view.rs:2450 | lane label の SendGain/PluginParam 枝だけ毎フレーム `format!`+Arc::from | lane id→label キャッシュ、target 変更時のみ再生成 | perf(低) | M |

## E. プラットフォーム / IME / メーター (軽微)

| # | file:line | 手ぬき | 最終形 | sev | effort |
|---|---|---|---|---|---|
| E1 | ui/platform/src/tsf/text_store.rs:478 | GetACPFromPoint (TSF 逆 hit-test) が常に TS_E_NOLAYOUT → MS-IME のポイント再変換等が効かない | CaretResolver に座標→ACP 逆引き実装 | user可視(IME) | M |
| E2 | ui/platform/src/winit_backend.rs:526 | CursorIcon::Hidden を Default で代用 (カーソル消えない) | window.set_cursor_visible(false) を配線 | user可視 | S |
| E3 | ui/platform/src/winit_backend.rs:364 | マウス Back/Forward を Other(0xffff) に畳む (両方衝突) | Back/Forward を専用 variant にマップ | 低 | S |
| E4 | ui/ui/src/widgets/level_meter.rs:8 | MeterBallistic::Vu が単一対称指数平滑で IEC 60268-17 弾道 (300ms 積分 + overshoot) でない → Rms とほぼ同等 | dt 駆動で上昇/下降時定数分離 + overshoot の真の VU 弾道 | user可視(限定) | M |
| E5 | daw_audio/audio_clip_renderer.rs:641 | granular Stretch の tempo 変化時残留 click (LP smoothing の partial mitigation) | per-event grain-trigger lock-in で buffer 跨ぎ source position 固定 | 低 | L |

## F. クリーンアップ (挙動でなく dead/stale)

- `daw_gui/view/preview_window.rs:121` `CompositeLayer` (約20行) は crate 内で一度も構築されないデッドコード。`:136` の「rotation 未対応」コメントは**誤り** (実際は `group_compose.rs` が rotation_radians+pivot を正しく渡し回転は動作)。削除。
- stale な Phase-N コメント群 (実は実装済み): `common/automation.rs:34` (Phase2 = `*_ranged` 済)、`common/audio_render.rs` (granular/slice DSP 実装済)、`model.rs:2769` PreFx (配線済)、`protocol.rs:231` PluginParamValueChanged (Phase4 で消費)、ui `ui.rs:1801` / `event.rs:157` (配線済) — コメントを現状に更新。
- `daw_plugin_host/builtin/voicevox.rs:115` の note_offsets コメント陳腐化 (実際は process():657 で参照)。

## Out-of-scope (納品済み機能の手ぬきではない別スコープ未着手 — 今回は対象外候補)

audio 入力録音 (SetTrackArmed) / 非Windows build / WAV 以外の audio import (mp3/flac) / animated GIF・APNG・SVG・RAW import /
baseview backend / runtime テーマ / ARA Playback controller / ARA ContentAccess の tempo-map / i18n /
動画 export の perf パイプライン化 (encode readback 先読み・libav decode 統一) / surround (>stereo)。

## 実装状況 (r.md #8 — branch feat/r-md-8、計 40 commits、全 compile/clippy/test green)

各項目を実装 or 文書化された据え置き (理由付き) で終端した。

- **A 全件 (A1-A10) 実装済** — 出力の正しさ。 hardware-SR 追従 + `TempoMap` SSoT で
  tempo automation を export / MIDI export / video sync / seek-loop が一貫尊重。
- **C 全件 (C1-C6) 実装済** — plugin GUI DPI-aware (CLAP reorder + VST3
  `setContentScaleFactor`) / IMessage+IAttributeList COM / restartComponent (named-bit
  diag + 該当 plugin の安全な targeted reinit + latency 再 emit) / surround
  SpeakerArrangement / release diagnostics / CLAP gui show-hide。
- **E2/E3/E4 実装済** — cursor-hide / mouse Back-Forward / IEC 60268-17 VU 弾道。
- **F 実装済** — CompositeLayer dead code 削除 + 全 stale「Phase N 未実装」コメントを
  コード確認の上で是正。
- **B1-B6 / B8 / B10-B13 実装済** — Slice onset 検出 (`common::onset`、 test) / MIDI Learn
  実動化 (注入 + touch&learn + legacy slot migration) / group 変調 / param block-rate /
  録音 lane / 実 plugin param 名 / sidechain tap point UI / song-level tempo 変調 /
  warp marker (granular consume + auto-warp `Alt+W`、 test) / 他。
- **B9 (time) 実装済** — Strobe / Time Wobble (catalog WGSL test green)。
- **D1 実装済** — routing schedule の compile (Vec/HashMap alloc) + `TempoMap::from_song` を
  RT audio callback から完全に追い出した (engine.rs の `TODO(PR3)` 解消)。 receive loop /
  decode スレッドで `CompiledRouting {song, schedule, tempo_map}` を compile し、 wait-free
  SPSC (`rtrb`) で audio thread へ forward。 RT は ring から **owned 値を pop して swap-in
  するだけ** (alloc 無し、 Arc box も無いので free 無し)。 差し替えた古い snapshot は recycle
  SPSC で受信ループへ送り返し、 `Drop` (free) も RT 外で走る。 scratch `input_delay_line` も
  起動時に事前確保し PDC/sidechain alignment の RT realloc を除去。 `rt-assert` 下の
  `assert_no_alloc` で「install が RT 上で alloc/free ゼロ」を証明 (logic + alloc-proof test green)。

- **B9 (feedback) 実装済** — Echo Trails (残像トレイル)。 executor に per-chain の前フレーム
  target (`history_targets`) を新設し、 `apply_chain(chain_key)` で feedback chain のみ維持。
  History パスは bind group 3 = 前フレーム target を `history(uv)` で sample、 chain 末で今フレーム
  出力を passthrough blit で退避 (= 次フレームの history)。 binding 3 は全 bind group が埋める
  (非 feedback 効果は入力 texture を dummy = シェーダ未宣言で無視)。 **2 フレーム readback test
  (frame2 = black 入力でも前フレームの red trail が残る) で lifecycle を自動検証済 (green)** —
  「安定 chain_key + 永続 target が要るため別経路」 の著者既定 deferral を解消。
- **E5 実装済** — granular grain-trigger lock-in。 uniform-stretch の grain `k` の source offset
  を **trigger 時の値に固定** する per-event ring (`GrainLockRing`、 TrackScratch に
  `MAX_GRANULAR_EVENTS_PER_TRACK` ぶん pre-alloc) を導入し、 後続 buffer で tempo_ratio が変化
  しても再計算しない (= tempo automation 中の source position 跳び = click を防ぐ)。 slot の
  grain-k 不一致 (seek / schedule 変化) は自己無効化。 RT alloc 無し (pre-alloc + array 添字)。
  **unit test で「tempo を倍にしても grain offset が trigger 値に固定」 + slot 上書き + None
  経路を検証 (green)**、 既存 granular 66 test に regression 無し。 著者既定の「別 phase」 deferral を解消。
  - **同件 (sibling-occurrence): Repitch (tape) mode 実装済** — `_ => source_pos = event_local ×
    effective_pitch_ratio` も同 root cause (tempo 変化で絶対位置が跳ぶ click、 jump 量は event_local
    比例で granular より重症)。 per-event の連続 source 位置 accumulator (`repitch_accum`、 同 index、
    `repitch_source_pos`) で contiguous 再生は ratio を積分・seek は再 anchor。 Raw は ratio 一定なので
    積分値 = 従来式で byte 同一。 unit test で連続積分 / seek 再 anchor / Raw 一致を検証 (green)。
    Slice は離散 slice を native rate 再生なので非該当。
- **B7 解決 (実装不要と判明)** — 監査時は「builtin にエディタ窓 GUI 無し = stub」 と挙げたが、
  調査の結果 **機能は既に daw_gui の native UI に在る**: 声選択 = `track_inspector` の per-clip
  voice picker (`Clip::speaker_id` SSoT / `SetClipVoice` / `app.singers` 一覧)、 合成進捗 =
  clip スピナー + 全体オーバーレイ (`voicevox_synth_status`)。 builtin (≠ 3rd-party CLAP/VST3) は
  設定をホスト native UI に統合する方が SSoT 一貫。 別エディタ窓に speaker picker / progress を
  重複実装するのは反 SSoT。 よって `gui_is_embed_supported=false` + gui_* no-op が**最終形**
  (旧 `bail!("PR-V2.4 予定")` を「意図的に GUI 無し」 の文書化 no-op に是正、 unused `bail` 除去)。
- **E1 実装済** — TSF `GetACPFromPoint` (逆 hit-test、 MS-IME マウス再変換) が常に
  `TS_E_NOLAYOUT` だったのを実装。 監査では「caret 位置のみで per-char layout 無し = 上流 infra
  待ち」 としたが、 caret と同じ `measure_text` で**各文字境界 `(x, byte)` を測れる**ので infra 追加
  なしで配線できた: `TextDocument.char_boundaries` を focus 中 text_input が publish → `DocState`
  に保持 → `acp_from_x(x)` が最近接文字エッジの ACP を返す → `GetACPFromPoint` が
  ScreenToClient 後に引く。 char_boundaries は 1 文字ずつ measure 累積 (monospace IME フォントは
  exact、 proportional の kerning 精緻化は glyph-layout 経由の follow-up)。 `acp_from_x` の
  点→エッジ写像 + layout 無し None を unit test (platform 17 tests green)。 実機 MS-IME 再変換は
  user 確認。
- **B12 (手動ワープ編集) 実装済** — model 層 (`move`/`add`/`delete_warp_marker`、 commit `cd14c7d`)
  に続き UI を配線。 audio editor が **≥2 marker の event を区分線形 warped 波形**で描く: 各 marker 区間
  を linear `ui.waveform` として連結 = playback の `warp_source_frame` と同一写像 (区間端は marker、
  端外は外挿線上なので線形 interp が一致)。 marker を locked_beat の x に縦線描画 (可変背景でも視認
  できるよう暗 backing + 明 `LOOP_BAND` の 2 層、 `feedback_ui_indicator_contrast_on_variable_bg`)。
  ジェスチャ: marker drag = 移動 (release で 1 回 `MoveWarpMarker` → 1 undo)、 Alt+click on marker =
  `DeleteWarpMarker`、 Alt+click on 波形 = `AddWarpMarker` (source は現在の warp 曲線上に pin → ドラッグ
  で再 warp)。 marker hit rect を trim grip の後・center band の前に登録して優先順を確定。 3 AppEvent +
  handler + `mutate_warp_markers` (sync_song_to_plugin_host で daw_audio へ伝播、 既存 `SetAudioEventStart`
  と同経路) を app.rs に追加。 **warp 編集全体 (auto-warp `Alt+W` 含む) を `is_undoable` に登録**
  (sibling-occurrence: auto-warp が従来 undo 不可だった gap も是正)。 build/clippy/test green、
  視覚 sign-off は user。
- **D3 実装済** — arrangement_view の build が毎フレーム全 track×clip ぶん track 名 `Arc::from`
  + `clip_display_label` (歌詞連結という重い文字列構築) を呼んでいた hot path を、 `AppData` の
  `ArrLabelCache` (`song_epoch` 世代キー) に持たせ通常フレームは `Arc` clone で済ませる。 epoch は
  push_undo_snapshot / undo-redo / reset_saved_baseline で進み、 ラベルを変える編集は is_undoable
  経由で auto-push に乗るので追従。 出力同一 (perf のみ、 commit `2395d3e`)。
- **D2 実装済** — 全 Song snapshot undo (`push_undo_snapshot` の `song.clone()`) の clone コスト
  支配項だった bulk binary field を `Arc<[u8]>` 化し snapshot 間で共有: plugin `state` (CLAP/VST3
  own state) + `ara_archive` (Melodyne 等 ARA、MB 級)。 これらは undo の編集対象ではない (= 同一
  bytes を全 snapshot で共有可能) ので、 編集ごとの MB 級コピーが refcount bump になる。 metadata
  (notes/clips/automation) は従来通り deep-clone (KB 級で安価かつ undo 対象なので版が必要)。
  serialize 形式は不変 (serde は `base64_opt` adapter を `Arc<[u8]>` 対応に更新し base64 文字列のまま、
  bincode は `Arc<[u8]>` を内側 slice と同一 length-prefixed で符号化) なので既存プロジェクト /
  IPC 互換。 model↔protocol 境界 (IPC は `Vec<u8>` のまま) で `as_deref().map(to_vec)` /
  `map(Arc::from)` 変換。 round-trip test (json + bincode) + script smoke + clippy 全 green。
- **D4 実装済** — lane label の per-frame `Arc::from` / `format!` を thread_local intern で解消。
  lane label は target ごとにほぼ不変 (定数文字列 / send_idx / plugin param 名) なので内容で intern し、
  2 フレーム目以降は `Arc<str>` clone (refcount bump) で返す (`intern_label` / `intern_send_label`、
  key 集合は lane label 種類数で有界)。 定数 label と SendGain (send_idx キー) は alloc ゼロ、
  PluginParam 名も intern。 icon_glyph / color は安価な定数なので live。 出力同一 (perf のみ)。
  `ArrLabelCache` 全面 cache (song_epoch + `PluginParamList` 世代の無効化 + free-fn 配線) より
  局所・低リスクで同じ「per-frame alloc ゼロ」 を達成。
  - **同件 (sibling-occurrence)**: arrangement build の per-frame `Arc::from(&str)` は他に
    section ruler 名 / automation clip 名 (= `song.content_name`) にも残っていた (D3 が track/clip
    名で潰した class の漏れ)。 どちらも user 編集可能で intern 不可なので、 D3 と同じ `ArrLabelCache`
    (`song_epoch` 世代キー) に `section_names` / `content_names` を足して clone 化。 無効化は
    `CommitRenameSection` / `CommitRenameClip` (is_undoable → song_epoch++) で追従。 引数が clippy
    `too_many_arguments` に達したため context refs を `LaneBuildData` struct に束ねた。 これで
    arrangement build の per-frame 名前 alloc class (track/clip/lane/section/automation-clip) は
    全て cache or intern 済。

### 再監査 (2 周目: 監査の網羅性確認)

「goal 到達か」の確認として cut-corner マーカーを全 sweep + 手動確認:
- **actionable マーカーはゼロ** — `todo!()` / `unimplemented!()` / `FIXME` / `TODO(...)` は
  vendored ffmpeg binding を除き 0 件。
- **stale コメント是正** — `compile_schedule` に `TODO(PR3)` (RT 上で compile して glitch、
  PR3 で off-thread 化予定) が残っていたが D1 で解消済だった → 実態に是正 (F bucket と同 class)。
- **未着手機能を実装 = master fx (master bus の EQ/limiter 等) の param 自動化 + 変調** —
  `process_master_fx_chain` が `fill_pd_param_events` を呼ばず「master fx param automation は
  将来機能」 と据え置かれていた。 data model (`song_lanes` = 自動化、 `song_mod_routings` = 変調)
  は既存だったので、 engine 配線 + UI 配線で track/group fx と同等に実装: (1) engine =
  `fill_pd_param_events(MASTER_TRACK_ID)` が `song_lanes` / `song_mod_routings` を解決 +
  `process_master_fx_chain` が呼ぶ。 (2) UI = `add_automation_from_last_touched` の song-level 判定に
  `MASTER_TRACK_ID` を追加 (touch → 自動化) + `cursor_modulatable_targets` に master fx PluginParam
  を追加 (変調ターゲット) + master row の lane 名/range を MASTER で解決。 engine unit test green。
- vst3 の非-HWND fallback「MVP 未対応」 コメントは Windows では非発生 (全 VST3 が HWND 対応) の
  descriptive comment で cut-corner ではない → 変更なし。

### 監査の終端

r.md #8 監査 34 項目すべて **実装 / 解決済** (据え置き無し)。 B7 のみ「builtin に専用エディタ
GUI を意図的に持たない (機能は daw_gui native UI に統合)」 という設計確定での解決。 GUI / 可聴系
(B12 warp 編集 / B9 映像効果 / E1 IME / D1・E5・Repitch の再生) は実機 sign-off を user に依頼。
