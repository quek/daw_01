<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# FIXME #90: VOICEVOX wav / 口パク 生成状態の可視化

ユーザー要望: 「voicevox で wav やロパク(口パク)の生成状態を可視化したい。プログレスバーを
表示するのがいいか。いまだと反映されているかどうかが分からなくて不便」。

## 確定要件 (grill-me 2026-06-27)

- **理想**: ユーザーが「いま聞こえる音」「見える口パク」が最新編集を反映しているか
  (= まだ再生成中か、確定したか) を常に一目で判別できる。
- **表示形態 = クリップ上 + 全体オーバーレイ の両方** (最も情報量が多い理想形)。
  - アレンジ上の各歌唱/読み上げクリップ (と口パククリップ) に「再生成中」スピナーを出す。
    印が消えた = そのクリップは最新、が直接の合図。
  - 画面端に非ブロッキングの全体インジケータ (「VOICEVOX 合成中… (残り N)」「口パク生成中…」)。
- **engine 未起動/起動途中** = 合成は裏で 1.5s ごと無限リトライ → 何もしないとスピナーが
  永遠に回る。**一定時間 (5s) 進まなければ「⚠ VOICEVOX エンジンに接続できません
  (エンジンを起動してください)」を別表示** に切り替える。engine boot 中 (数秒) は「合成中」に見える。
- **完了時 = スピナーが静かに消えるだけ**。「✓反映しました」等のフラッシュは出さない
  (連続編集で debounce ごとに点滅してノイズになるため)。
- 偽の % は出さない。HTTP は中間進捗を返さないので indeterminate (回転スピナー) + 残件数のみ。

## 進捗 % が作れない理由 (一次情報)

VOICEVOX 合成 (`/frame_synthesis` 歌唱, `/audio_query`+`/synthesis` talk) も口パク
phoneme query (`/sing_frame_audio_query`) も **1 回の blocking HTTP**。中間進捗イベントが
無いので、声/トラック単位の「処理中/完了」(indeterminate) + 件数が最も正直な粒度。
WAV 合成は FIXME #36 で音量一貫性のため「声(speaker)単位まとめ合成」を採用しており、
clip 別合成はしない設計 → WAV 側の最小粒度はトラック単位、口パク側は口トラック単位。

## 生成は 2 系統 (調査済み)

| | WAV 合成 (歌唱/読み上げ) | 口パク生成 |
|---|---|---|
| 場所 | plugin host プロセス: builtin VOICEVOX plugin の synth thread | daw_gui の背景スレッド |
| トリガ | `sync_vocal_metadata()` → `SetBuiltinPluginNoteMetadata` IPC | `regenerate_lipsync_for_track()` (400ms debounce) |
| 既存状態 | `synth_queued_gen`/`synth_done_gen` (queued>done = 処理中)。bounce のみ `VocalSynthReady` 報告 | `lipsync_gen` + `LipsyncGenerated`。in-flight フラグ無し |
| engine 健全性 | **両系統で 1 engine 共有** → WAV 合成の `failing` を engine-health の SSoT にする (口パクはソースが必ず歌唱/読み上げトラック=WAV 合成も同 engine で走る) |

## アーキテクチャ (SSoT)

### WAV 合成状態の報告 (plugin host → GUI)

builtin VOICEVOX plugin (`daw_plugin_host/src/builtin/voicevox.rs`) の synth thread が
状態遷移時に reporter callback を呼ぶ (event-driven、poll でなく、reinit でも生きる、
`PluginEvent` 非依存)。

- 新フィールド `status_reporter: Arc<ArcSwapOption<VoicevoxStatusReporter>>`、
  `type VoicevoxStatusReporter = Box<dyn Fn(bool /*busy*/, bool /*failing*/) + Send + Sync>`。
- synth processing thread が `last_reported: Option<(bool,bool)>` を持ち、**変化時のみ** reporter を呼ぶ:
  - 非空 job を synth 開始直前 → `(true, false)`。
  - HTTP Err (どれか失敗) → `(true, true)` 後 1.5s backoff。
  - 成功 store / 空 job 確定で coalesce slot に次 job が無い → `(false, false)`。次 job 有りなら維持。
- `busy` は本質的に `queued_gen > done_gen` と等価だが、`failing` は新規。done_gen は成功でのみ
  進むので、処理中/retry 中は queued>done のまま = busy。
- trait `LoadedPlugin` に `fn set_voicevox_status_reporter(&mut self, _: VoicevoxStatusReporter) {}`
  (default no-op)。VoicevoxBuiltin だけ override し ArcSwapOption に store。
- plugin host `main.rs`: 新 plugin install + plugin_id 割当後、`voicevox_synth_progress().is_some()`
  なら `set_voicevox_status_reporter` で reporter を仕込む。reporter は `plugin_id` + `evt_tx` を
  capture し `PluginEvent::VoicevoxSynthStatus { plugin_id, busy, failing }` を送る。
- `PluginEvent::VoicevoxSynthStatus` → `ChildToMain::VoicevoxSynthStatus { plugin_id, busy, failing }`
  (`From` impl)。protocol enum に variant 追加 (末尾、bincode derive 済) → **`cargo build --workspace`**。

### 口パク in-flight 追跡 (daw_gui 内、SSoT = daw_gui)

- `regenerate_lipsync_for_track`: spawn 前に解決済 `target_id` を `lipsync_inflight: HashSet<u32>` に insert。
  spawn する thread には `target_id` も渡す。
- thread は **常に** 完了イベントを送る (成功/失敗/空)。`AppEvent::LipsyncGenerated` に
  `target_track_id: u32` を足し、成功時のみ clips 非空。handler は generation/staleness に関わらず
  まず `lipsync_inflight` から target を除去し、その後 generation 一致 & 非空なら従来どおり適用。

### GUI 状態 + ヘルパ (`AppData`)

- `voicevox_synth_status: HashMap<u32 plugin_id, VocalSynthStatus { busy: bool, failing_since: Option<Instant> }>`。
  `VoicevoxSynthStatus` event で更新 (failing 立上りで failing_since=now、!failing で None)。
- `lipsync_inflight: HashSet<u32 target_track_id>`。
- `anim_epoch: Instant` (スピナー位相用、construction で設定)。
- helper:
  - `voicevox_plugin_id_for_track(track) -> Option<u32>` (sync_vocal_metadata の lookup を抽出)。
  - `track_is_synthesizing(track_id) -> bool` (= 所属 builtin VOICEVOX plugin が busy)。
  - `lipsync_target_is_generating(track_id) -> bool` (= lipsync_inflight.contains)。
  - `voicevox_busy_track_count() -> usize` (残り N)。
  - `voicevox_engine_warning(now) -> bool` (busy && failing_since 経過 >= 5s が 1 つでも)。
  - `voicevox_animating(now) -> bool` (busy 系が在り && !engine_warning。warning 後は static で再描画停止)。

### アニメーション (連続再描画)

`runner.rs::render_frame` は現状 `state.app.is_playing` を返す (再生中のみ連続再描画)。
これを `is_playing || voicevox_animating(Instant::now())` に拡張。生成中だけ連続再描画し
スピナーを時間ベースで回す。engine warning 確定後は animating=false → static 表示で再描画停止
(CPU spin させない)。GUI スレッドなので `Instant::now()` は問題なし (RT 制約は audio thread のみ)。

### 描画

- 新 `daw_gui/src/view/voicevox_overlay.rs`: 上端中央 (load_overlay と被るときは下にオフセット) の
  非ブロッキング overlay。engine warning は amber、それ以外は spinner + 「合成中… (残り N)」/「口パク生成中…」。
  共通 `draw_spinner(ui, id, cx, cy, r, phase, color)` (8 点の回転 dot、font 非依存) を提供。
- `arrangement_view.rs`: clip overlay ループ (resp.clip_rects、`ui` で毎フレーム、cache 外) に、
  busy な歌唱/読み上げトラックの vocal/talk clip と、生成中の口トラックの `auto_lipsync` clip に対し、
  クリップ右上角に小スピナーを描く。
- `root.rs` で voicevox_overlay::draw を呼ぶ。`mod.rs` に追加。

## テスト (高レイヤ優先)

1. protocol `VoicevoxSynthStatus` bincode roundtrip (既存 protocol test と同型)。
2. `AppEvent::VoicevoxSynthStatus` handler: map 更新 / failing_since 設定。
3. `voicevox_engine_warning` 純関数: failing_since と now (Instant + Duration) で閾値判定。
4. `AppEvent::LipsyncGenerated` handler: target を lipsync_inflight から除去 (成功/空/stale 全ケース)。
5. spinner 位相純関数 `spinner_head(elapsed, period, n)`。

自明な mapping (`voicevox_plugin_id_for_track` 等) はテストしない (`feedback_no_tests_for_simple_cases`)。
plugin host synth thread の報告は background+HTTP で unit 困難 → 遷移ロジックは単純化し実機で検証。
