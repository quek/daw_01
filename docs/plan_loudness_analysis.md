# 範囲ラウドネスのオフライン解析 (r.md #54)

ループ範囲 (や任意の範囲) のラウドネスを、再生を待たずに **全速で測って** EBU R128 の
レポートとして出す。Ardour の Loudness Analysis / Loudness Assistant、REAPER の
dry run render に相当する機能。

## 0. 理想

範囲のラウドネスは、**書き出す WAV と 1 サンプル一致する音**を、再生を待たずに全速で
測って出す。**測定器は 1 つ**(ライブメーターと同一コード)、**レンダ経路も 1 つ**
(既存 freewheel の出力先を「WAV 書き込み」から「ラウドネス積算」へ差し替える)。
結果は EBU R128 の完全なレポートとして独立窓に出し、納品ターゲットとの差まで一続きで分かる。

## 1. grill-me (2026-08-16) で確定した決定

| 論点 | 決定 |
|---|---|
| 解析する範囲 | **ループ範囲を既定にした範囲ピッカー**。ループ / 選択 / セクション / 曲全体をワンクリック切替 + 拍で微調整 |
| 走査の起点 | **範囲の先頭から (cold)**。何度測っても同じ値、範囲長にしか比例しない |
| 減衰 tail | **測らない** (範囲ちょうど)。「範囲のラウドネス」は範囲の音であって残響ではない |
| 正規化 | **測る + 目標との差 (何 dB) を数値表示**まで。マスターゲインへの自動適用ボタンは作らない |
| 目標値の所有者 | 既存 `MeterSettings.loudness_target_lufs` (マスターパネルの 0 LU 線と共有) + 新設のトゥルーピーク上限 |
| レポートの中身 | 数値 + short-term 時系列グラフ + ヒストグラム + 各最大値の発生位置 (クリックで playhead ジャンプ) |
| 表示先 | 独立した **浮動ウィンドウ** (移動・リサイズ・位置永続) |
| 起動導線 | ルーラー右クリック / マスターパネルの「範囲を解析...」 / 解析メニュー / `Ctrl+L` の 4 つ |
| 解析中の見え方 | **レポート窓を先に開き**、その中に進捗バーと中止ボタン。走査中は背景を暗転して操作を遮断。完了で暗転が消え窓は残る |
| 適用範囲 | **解析メニューのときだけ**。通常の WAV 書き出し / バウンスにはレポートを付けない |
| 拍→サンプル換算 | **engine 側 SSoT に一本化**。解析・範囲書き出し・バウンス・シークをまとめて修正 |
| メニュー位置 | メニューバーに新規「解析」メニュー |

## 2. データの流れ

```
daw_gui                                   daw_audio
───────                                   ─────────
解析メニュー / Ctrl+L / ルーラー右クリック
  → 範囲ピッカー (拍)
  → begin_loudness_analysis
      stop() → flush_song_sync()
      → SetRenderMode(Offline)
      → ReinitAllPlugins ─────────────▶ (plugin_host)
  ◀── PluginsReinitDone ───────────────
      → AudioCommand::AnalyzeLoudness { range: 拍 }
                                          ├ export_running を compare_exchange で予約
                                          ├ live CPAL が park するまで待つ
                                          └ render_loop (freewheel)
                                              render_master_buffer()      ← live / 書き出しと同一
                                                 ↓ 窓 [start, end) のみ
                                              RenderSink = LoudnessSink
                                                 └ common::loudness_report::LoudnessCollector
                                                     ├ LoudnessMeter (K-weighting / M / S / I / LRA)
                                                     ├ TruePeakMeter (48-tap 4-phase FIR)
                                                     └ サンプルピーク / クリップ数 / 曲線 / ヒストグラム
  ◀── LoudnessAnalysisProgress(report) ── 250ms ごと (途中経過も数値と曲線を含む)
  ◀── LoudnessAnalysisComplete{report}
      → SetRenderMode(Realtime)、レポート確定
```

- **測定器は `common` が所有する**。`daw_gui/src/master_meter/{loudness,truepeak}.rs` を
  `common/src/{loudness,truepeak}.rs` へ移設し、ライブメーター (`MasterAnalyzer`) と
  オフライン解析が**同じ型**を通る。「メーターの数値と解析レポートの数値が食い違う」が
  構造的に起きない。
- **走査経路も 1 つ**。`daw_audio/src/export.rs` の `render_loop` が持っていた
  `&mut WavWriter` を `&mut dyn RenderSink` に一般化した (`WavSink` / `LoudnessSink`)。
  PDC 窓シフト・tail 判定・cancel・live park ハンドシェイクを二重実装しない
  (アーキテクチャ不変条件 6 の延長)。
- **engine を排他共有する**。解析も `export_running` を予約するので、書き出しと解析が
  同時に走ることはない。中断も `CancelExport` を共有する。

## 3. wire に載る形

`common/src/loudness_report.rs` (= `common/build.rs` の `WIRE_SOURCES` に登録)。

```rust
pub const LOUDNESS_CURVE_COLUMNS: usize = 768;    // spectrum / scope と同じ列数
pub const LOUDNESS_HISTOGRAM_BINS: usize = 100;   // 1 LU 刻み、下端 -70 LUFS

pub struct LoudnessReport {
    range_start_beat, range_end_beat, range_start_frame, sample_rate,
    done_frames, total_frames, complete,
    integrated_lufs, lra_lu, lra_provisional,
    max_momentary_lufs, max_momentary_at_secs,
    max_short_term_lufs, max_short_term_at_secs,
    true_peak_dbtp, true_peak_at_secs,
    sample_peak_dbfs, sample_peak_at_secs,
    clipped_samples, measured_secs,
    short_term_curve: [f32; LOUDNESS_CURVE_COLUMNS],
    momentary_curve:  [f32; LOUDNESS_CURVE_COLUMNS],
    histogram: [u32; LOUDNESS_HISTOGRAM_BINS],
}
```

- 曲線とヒストグラムは **固定長配列**。範囲の長さに依らずメッセージサイズが一定
  (約 6.5KB) なので、「protocol に bulk を直載せしない」不変条件と正面から整合する。
  グラフは**表示物**であって解析の生データではない、という切り分け。
- `AudioEvent` の variant は `Box<LoudnessReport>` (enum 全体が 6.5KB になるのを避ける。
  `MasterMeterTick(Box<..>)` と同じ)。
- 到達していないラウドネス値は `f32::NEG_INFINITY`、位置は `Option<f32>`
  (`LoudnessReadout` と同じ規約)。

## 4. 拍 → サンプル換算の一本化

**旧構造の問題**: GUI 側 `export_beats_to_frames` が定数 BPM の線形換算
(`sr*60/bpm`) でフレームを作って送っていたが、daw_audio 側の走査は
`common::automation::beats_to_samples` (SongTempo カーブの積分) で進んでいた。
テンポオートメーションのある曲では**指定した小節と実際に走査する位置がずれる**。
ラウドネス解析は「範囲の定義がずれたら値がずれる」機能なので、ここを直さずには載せられない。

**新構造**: 範囲は **拍のまま** wire を渡り、換算は daw_audio の `RenderWindow::resolve`
1 箇所だけが行う。同じ root cause を持つ経路をまとめて直した:

| 経路 | 旧 | 新 |
|---|---|---|
| `AudioCommand::ExportWav` | `range: Option<(u64, u64)>` フレーム | `Option<(f64, f64)>` 拍 |
| `AudioCommand::BounceClipFxOnline` | `start_frame` / `end_frame` | `start_beat` / `end_beat` |
| `AudioCommand::AnalyzeLoudness` | (新規) | `range: Option<(f64, f64)>` 拍 |
| `AppData::seek_playhead_to` | 定数 BPM で `SeekTo{samples}` | `beats_to_samples` |
| `AppData::stop()` の「開始位置へ戻る」 | 定数 BPM で `SeekTo{samples}` | `beats_to_samples` |
| headless `daw.exportWavRange` | フレーム | 拍 |

`beats_to_samples` / `samples_to_beats` は 1/64 拍刻みの数値積分なので `O(拍数)`。
ルーラードラッグのように毎フレーム換算する呼び出し元があるため、
**SongTempo レーンを持たない曲では閉形式に早期リターン**する
(`has_song_tempo_automation`)。テンポ一定なら積分と厳密に一致する。

**残っている非対称 (別件)**: `common::timing::effective_loop_bounds` (= 再生ループの
折り返し位置) は今も定数 BPM 換算。これは daw_audio の **RT コールバック内**
(`engine.rs` の loop wrap 判定) から毎バッファ呼ばれており、`beats_to_samples` は
`O(拍 × 64)` の積分ループなので RT パスでは呼べない。直すなら「`SetLoop` /
`LoadSong` の受信時に off-RT で境界を計算して ArcSwap で差し替える」という別設計が要る。
テンポカーブのある曲でループ再生すると、解析した範囲と実際にループする区間がわずかに
ずれる (テンポ一定の曲では完全に一致する)。

## 5. GUI

| 置き場所 | 中身 |
|---|---|
| `common/src/loudness.rs` | K-weighting / 100ms サブブロック / M・S・I・LRA (+ 最大値の発生位置) |
| `common/src/truepeak.rs` | BS.1770 Annex 2 の 48-tap 4-phase FIR (+ 最大値の位置) |
| `common/src/loudness_report.rs` | `LoudnessReport` (wire) + `LoudnessCollector` (走査中の積算) |
| `daw_audio/src/export.rs` | `RenderSink` trait / `WavSink` / `LoudnessSink` / `RenderWindow` / `run_loudness_analysis` |
| `daw_gui/src/state/loudness.rs` | `LoudnessState` / `LoudnessPhase` (session-only) |
| `daw_gui/src/handler/loudness.rs` | 起動 / ハンドシェイク / 進捗 / 完了 / 中止 / 目標設定 / 位置ジャンプ |
| `daw_gui/src/view/loudness_report.rs` | レポート窓 (floating + 走査中は全画面予約で遮断) |
| `ui/crates/ui/src/widgets/loudness_graph.rs` | 汎用 `loudness_graph` / `loudness_histogram` (ドメイン知識ゼロ) |

### 5.1 レポート窓

- 走査中: `Ui::reserve_floating_region(screen)` で **画面全体**を予約 → 背景の pointer が
  丸ごと落ちる。暗転の矩形は `with_floating_region` の中 (= 窓と同じ層) に描くので、
  「暗いのに押せる」「押せるのに効かない」のどちらも起きない
  (`docs/plan_export_modal.md` で潰した症状の再発防止)。
- 走査していないとき: 窓の rect だけを予約する通常の floating window
  (`settings` / `undo_history` と同じ機構)。移動・リサイズ・位置は `app_config.json` に永続。
- 走査中は窓を動かせず、閉じられない (中断せずに窓だけ消すと暗転だけが残るため)。
  抜け道は「中止」ボタンと Esc。

### 5.2 表示内容

数値 8 行 (Integrated / LRA / 最大 Momentary / 最大 Short-term / True Peak /
Sample Peak / クリップ数 / 測定長)、目標との差、配信プリセット 5 種
(EBU R128 / Spotify / YouTube / Apple Music / Amazon — 値は Ardour の
`loudness_settings.cc` に合わせた) の適合 ○/×、short-term + momentary の時系列グラフ
(目標線を跨いだ部分を色分け)、ヒストグラム。位置を持つ行と グラフのクリックで
プレイヘッドがその位置へ飛ぶ。

### 5.3 キャッシュしない

ラウドネスは全トラック・全プラグイン・全オートメーション・マスターチェーンの合成結果で、
プラグイン内部状態 (VCV Rack のパッチ / ARA の編集 / VOICEVOX の合成キャッシュ) まで含む。
これを fingerprint に畳むのは原理的に不可能なので、**毎回測る**。

「古い」の判定は **レポートを取った時点の `SongDoc::edit_epoch` と現在値の比較**
(`AppData::loudness_report_stale`)。epoch は `edit_song` だけでなく **undo / redo /
履歴ジャンプ / プロジェクト差し替え**でも進むので、編集の口ごとに印を立てるより
穴が開かない。プロジェクトを切り替えたときは `reset_song_scoped_state` が
レポート自体を捨てる (前の曲の拍で「範囲 x – y」を出さないため)。

## 6. 解析中に止まるもの

engine が `export_running` で排他するので、解析中は**物理的に**再生できない
(オーディオ出力は無音、プラグインは走査スレッドが占有)。GUI 側も同じ述語
`AppData::offline_render_busy()` に集約して止める:

- 再生 / 録音 (`play()` が Refused)
- Song の編集 (`SongDoc` の export lock。走査の開始・終了で `sync_export_lock` を呼ぶので、
  `edit_song` を経由しない直接編集経路も塞がる)
- プラグイン再構成の round-trip (`handle_event` の block-list)
- 背景の pointer 入力 (レポート窓の全画面予約 + 暗転)。予約は生 pointer 由来の
  **右クリック / ダブルクリックも落とす** — 落とさないと暗幕の上に context menu が
  開き、その popup は mask された pointer しか読めないので item も outside-click も
  効かない「消せないメニュー」になる
- **キーボードショートカット全部 (Esc = 中止 を除く)**。Ctrl+Z / Ctrl+Y は
  `edit_song` を通らないので編集ロックでは止まらず、次フレームの `flush_song_sync` が
  走査中の daw_audio へ `LoadSong` を送ってしまう。Ctrl+E は走査中に
  `ReinitAllPlugins` を撃つ。書き出しは進捗モーダルが真のモーダル
  (capture_keyboard) なので元から止まっている — 解析だけこの保護を欠いていた
- 他の floating window (設定 / 編集履歴) の描画そのもの。これらは
  `with_floating_region` で raw pointer に戻すので、暗転の下でも押せてしまう

### 6.1 中断の契約

走査を GUI 側から終わらせる経路 (中止 / watchdog / 子プロセス切断 / プロジェクト
切替) は **必ず `AudioCommand::CancelExport` を送る** (`abort_loudness_analysis`)。
送らないと daw_audio の `export_running` が立ちっぱなしになり、CPAL コールバックが
無音を書き続けて「再生しても音が出ない」状態になり、以後の書き出し / バウンス /
解析も全部 `"export already in progress"` で弾かれる (書き出し側 `abort_audio_export`
が同じ理由で送っている)。

併せて解析セッションには**世代 id** を持たせ、`AudioEvent` は世代が一致するときだけ
採用する。中断後に前セッションの完了が後着すると、それを新セッションの結果として
受理してしまい、phase が Idle に落ちて次の `PluginsReinitDone` で新セッションが
発火しなくなるため。

進捗も完了も 60 秒来なければ watchdog (`handler::tick`) が同じ経路で畳む
(書き出しの watchdog と同型)。

## 6.5 測定器側で併せて直したもの (レビューで発覚)

いずれもライブメーター (r.md #50) から持ち越していた欠陥で、オフライン解析を
載せたことで表面化した / 検証されたもの。

| 症状 | 原因 | 直し方 |
|---|---|---|
| リセット直後の I / LRA / 最大 M / 最大 S にリセット前の音が混入し、相対ゲートの基準まで押し上げて I が前回値に張り付く (再生開始のたびに起きる) | `reset_integrated` は「今鳴っている音」の表示を切らさないため直近 3 秒のサブブロックを残すが、その窓の値をそのまま max / ヒストグラムへ入れていた | **リセット地点をまたぐ窓は確定させない** (`elapsed_blocks >= 窓長` を条件に足す) |
| 範囲の先頭 6 サンプルにピークがあると True Peak が Sample Peak より 30dB 低く出る (BS.1770 の TP >= サンプルピークに違反、上限チェックも素通り) | FIR の充填中は補間出力を捨てていた | 充填ガードは残したまま (EBU Tech 3341 #15〜#19 の過渡による過大読みを防ぐため)、**素のサンプル値 `\|x\|` を常に下限**に置く |
| 5 分を超える範囲でグラフが最大 Momentary の山を落とし、数値と最大 26dB 食い違う | `fill_curve` が列ごとに 1 ブロックの瞬時値を点サンプルしていた (1 列 ≒ 0.78 秒 に約 37 ブロック) | 列内は**最大値で畳む** (widget 側 `to_columns` と同じ規約に揃える) |
| グラフの縦軸ラベルが曲線の塗りに覆われて読めない | ラベルをグリッドと一緒に塗りの**下**へ描いていた | ラベルは塗りの**後**に描き、暗いバッキングチップでコントラストを保証 |
| `common/src/scale.rs` が `WIRE_SOURCES` 未登録 = protocol fingerprint の穴 (既存) | `Song.scale_changes` の型定義がそこにある | `WIRE_SOURCES` に追加 |

## 7. 検証

- `common::loudness` — BS.1770-5 の係数一致 / -18 dBFS ステレオ = -18 LUFS /
  Tech 3342 の LRA (既存テストをそのまま移設)。
- `common::loudness_report` — 定常正弦の Integrated 一致、曲線が左から埋まる、
  最大値の発生位置、クリップ数、NaN 混入からの復帰、目標との差。
- `daw-ui-core::loudness_graph` — 折れ線が値の無い列で切れる、列への畳み込み、
  未走査区間を埋め戻さない、表示レンジの端。
- `daw_gui/tests/loudness_analysis.rs` — 既定範囲 → ハンドシェイク → **拍で** 解析コマンド、
  走査中は再生も編集も通らない、完了で Realtime 復帰と確定、編集で stale、
  中止で途中値を残さない、範囲プリセット。
- `daw_gui/tests/loudness_analysis_smoke.rs` (+ `tests/scripts/loudness_analysis_smoke.js`) —
  **プロセス横断の end-to-end**。`daw_gui --script` が空 song を engine へ届け、
  `daw.analyzeLoudnessJson(8, 24)` で解析させ、拍→サンプル換算 (16 拍 @120BPM = 8.00 秒)
  と完了通知の往復を pixel でなく数値で固定する。プラグイン / 音源ファイル不要。
- headless: `daw.analyzeLoudnessJson(startBeat, endBeat, timeoutMs)` は実プロジェクトでも
  使える (`daw.exportWavRange` と同じ流儀)。`start >= end` で全曲。
- protocol を変えたので **`make build` でワークスペース全体を再ビルド**すること
  (子 exe の fingerprint 不一致で Hello が落ちる)。
