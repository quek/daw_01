# daw_01 全体コードレビュー (2026-06-06)

手法: 20 ユニット (66,584 行) をレンズ別 (RT安全性/FFI/perf/correctness) に並列レビュー →
各所見を別エージェントが敵対的に検証 (偽陽性を実コード参照で排除) → 集約。
41 エージェント / 676 tool uses。**confirmed 83 / uncertain 8 / rejected 13**。

---

## 0. 解決状況 (2026-08-22 再検証)

> **この文書は 2026-06-06 時点のスナップショットです。以降の全体アーキテクチャ改修
> (`docs/plan_arch_refactor.md`) と個別修正で、下記のとおり大半が解決済みです。
> 行番号は当時のもので現在とはずれています — 各項目に付けた「現在」行を参照してください。**

| 区分 | 件数 | ✅ 解決済み | △ 部分的 | ⚠️ 未対応 | ❓ 未再検証 |
|---|---|---|---|---|---|
| §2 High | 12 | **12** | 0 | **0** | 0 |
| §3 Mid | 23 | 21 | 2 | **0** | 0 |
| §4 Low (抜粋 ~19 / 全 ~50) | ~50 | 6 | 0 | **0** | ~44 |
| §7 要追加調査 (uncertain) | 8 | 4 | 0 | **0** | 4 |

- **再検証で見つかった未対応 (⚠️) は 0 件です。** High 12 件と Mid 23 件は全数を現コードで
  1 件ずつ確認しました。
- △ 2 件はどちらも **描画パフォーマンス** の項目で、指摘された病的パターン (N² 走査 /
  `clone` + `format!` の多段 collect) は解消済み、immediate-mode 由来の毎フレーム `Vec`
  構築だけが残っています。**correctness / 安全性の問題ではありません。**
- ❓ は「悪い」ではなく **今回の再検証で個別に追えていない** という意味です。§4 Low は
  抜粋のみが名指しで、残りは元レポートにも項目名が無いため機械的に追えません。
- 再検証者: Claude (2026-08-22)。判定根拠は各項目の「現在」行に file:line で残してあります。
  再確認したい人はそこから辿れます。

**なぜ大半が解決しているか**: このレビュー直後に `docs/plan_arch_refactor.md` の全体改修を
実施し、(a) RT スレッドを「無限待ち・確保・解放をしない」不変条件で作り直し
(`RtBundle` + rtrb ring)、(b) 信頼境界の値域検証を `Song::sanitize_ranges` /
`normalize_after_load` / `clipboard.rs` の 3 つの入口に集約し、(c) positional index
addressing を安定 id に置換しました。本レビューの横断テーマ (A)〜(F) が、そのまま
改修の設計入力になっています。

---

## 1. エグゼクティブサマリ

コードベース全体は堅牢な 3 プロセス分離アーキテクチャと明確な RT 制約方針を持つが、レビューで最も重大かつ反復的に現れたテーマは **(A) RT オーディオパス上でのヒープ確保・ロギング・tokio send の混入** と **(B) 永続化/IPC/clipboard 等の信頼境界での値域未検証** の 2 つである。特に深刻なのは、曲レベル automation (`song_lanes`) が GC/正規化パスから完全に漏れており **テンポオートメーションが保存時に消失する** データロスバグ (model.rs:842, 683) と、ミキサーの live setter が `is_dirty` を立てず **fader/mute/solo 編集が黙って破棄される** バグ (app.rs:14052 ほか) で、いずれもユーザーの作業成果を失わせる。CLAP の in_events が time 昇順にソートされず spec 違反 (clap_plugin.rs:1260) になっている点、VOICEVOX synth thread が永久失敗 job で join 無限ブロックする点 (voicevox.rs:188) も実害が大きい。横断的には「RT パスの `tracing` 呼び出し」が 8 ファイル以上、「外部入力の値域/サイズ未検証」が IPC・project load・plugin scan・clipboard・WAV import の全境界に散在している。

## 2. High 重大度 — 12 件すべて ✅ 解決済み

1. ✅ **`common/src/model.rs:842-860` — gc_clip_contents が song_lanes を live 集合に含めず曲レベル automation が保存前に削除される** [Correctness]
   `gc_clip_contents` は `tracks[].clips` と `tracks[].automation_lanes[].clips` のみから live 集合を構築し、`song_lanes[].clips` (SongTempo/TimeSig master lane) を走査しない。テンポオートメーションを描いて保存すると content_id が live 判定されず `clip_contents` / `clip_content_names` から retain 除外され、次回ロードでカーブが全消失する。`clip_content_refcount` も同様に song_lanes を歩かず共有判定が壊れる。
   **修正**: `gc_clip_contents` / `clip_content_refcount` の両方に `song_lanes[].clips` の二重ループを追加。round-trip 回帰テストを追加。
   **現在**: `common/src/model.rs:2369-2373` が `song_lanes` を走査 (「Without walking them, a tempo-automation curve's `content_id` is judged dead」と経緯コメントあり)。`clip_content_refcount` も `common/src/model.rs:2277` で `song_lanes` を含む。回帰テスト `gc_clip_contents_keeps_song_lane_references` (`common/src/model/tests.rs:1674`)。

2. ✅ **`common/src/model.rs:683-784` — ensure_clip_contents が song_lanes のクリップを移行対象にせず content_id sentinel が再採番されない** [Correctness]
   `tracks[].clips` と `tracks[].automation_lanes[].clips` のみ走査するため、曲レベル automation clip が `content_id==0` のまま残り `clip_contents[0]` にエントリが作られず、automation eval / GUI lookup が常に空フォールバックする。
   **修正**: `ensure_clip_contents` 末尾に song_lanes への content_id 再採番 + `or_insert_with(Automation default)` ループを追加。max content_id 走査にも song_lanes を含める。
   **現在**: `common/src/model.rs:2183` (max 走査) と `:2240-2247` (sentinel 再採番 + insert) が `song_lanes` を含む。回帰テスト `ensure_clip_contents_reassigns_song_lane_sentinel_ids` (`common/src/model/tests.rs:1708`)。

3. ✅ **`daw_gui/src/app.rs:14052-14088, 14194-14271` — ミキサー live setter (volume/pan/mute/solo/send) が is_dirty を立てず silent data loss** [Correctness]
   `set_track_volume`/`set_track_pan`/`toggle_track_mute`/`toggle_track_solo`/`set_send_gain`/`set_send_enabled` は persisted フィールドを書き換えるが realtime IPC 直送のみで `sync_song_to_plugin_host` を通らず `is_dirty` を立てない。fader 等だけ触って閉じると保存確認モーダルが出ず破棄され、autosave も走らない。`SetSongBpmFromScrub` が明示的に防いでいるのと同型の漏れ。
   **修正**: 各 setter 末尾で値が変化したときに `self.is_dirty = true;` を立てる。
   **現在**: 6 setter すべてが `edit_song_checked` / `edit_song` チョークポイント経由 (`daw_gui/src/handler/mixer.rs:301` volume / `:334` pan / `:485` send gain / `:535` send enabled / `:566` mute / `:579` solo)。dirty はチョークポイントの epoch bump が担うので個別フラグ立てが不要になった (アーキテクチャ不変条件 5)。

4. ✅ **`daw_plugin_host/src/clap_plugin.rs:1260-1277` — in_events が time 昇順にソートされていない (CLAP spec 違反)** [Correctness]
   `EventListView` は note 列と param 列を別々にソートして単純連結するため、グローバルには time 昇順でない (例: `[note@200, param@50]`)。CLAP の `clap_input_events` は time 単調性を host 契約として要求しており、二分探索/sample-accurate 補間する plugin が誤動作・取りこぼしを起こす。automation + 演奏混在の典型ケースで踏む。
   **修正**: note と param を 1 本の time 昇順 stable merge 列にする。merge 用 index バッファは activate 時に pre-allocate して RT 安全を維持。
   **現在**: `EventOrderRef { time, stream, idx }` による単一 merge 列 (`daw_plugin_host/src/clap_plugin.rs:1590-1606`)。`event_order` は load 時に `Vec::with_capacity(512)` で事前確保 (`:700`)、`process()` 冒頭で `clear()` → 3 stream を push → `sort_unstable_by_key` (`:282-307`) なので RT で再確保しない。同時刻は Note < Param < ParamMod の複合キーで安定。

5. ✅ **`daw_plugin_host/src/builtin/voicevox.rs:188-253` — synth thread が永久失敗 job で shutdown を検知できず join() 無限ブロック** [Concurrency]
   engine 未起動時、synth job が coalesce slot に戻され retry し続けるため `strong_count<=1` の終了チェックに永久到達しない。deactivate/Drop → `stop_synth_thread()` → `join()` で plugin-main thread が無期限ブロックし、plugin/track 削除・アプリ終了がフリーズ。reqwest client が timeout 未設定 (voicevox.rs:557) でハングリスクが二重。
   **修正**: processing thread に AtomicBool shutdown flag を持たせループ先頭でチェック。reqwest に `.timeout(5s)` を付与。
   **現在**: `synth_shutdown: Arc<AtomicBool>` (`daw_plugin_host/src/builtin/voicevox.rs:434`) をループ先頭・retry backoff の sleep 中・受信待ちの各所でチェック (`:525` `:530` `:548` `:571` `:595` `:643`)、`join()` の直前に `store(true)` (`:764-768`)。HTTP は `SYNTH_HTTP_TIMEOUT_SECS` 付き client (`daw_plugin_host/src/builtin/voicevox_synth.rs:220-221`, `:324-325`)。
   **残る枝葉 (別件)**: GUI 側の口パク用 `query_phonemes` だけ `reqwest::blocking::Client::new()` で timeout 無し (`daw_gui/src/voicevox_client.rs:87`)。本項目が指す synth thread とは別経路 (GUI の background thread) だが、同じハング risk のきょうだい。

6. ✅ **`common/src/voicevox.rs:289 (および 354, 408)` — エラー body の byte slice が UTF-8 char 境界で panic** [Correctness]
   `&body[..body.len().min(200)]` は byte 単位 index。日本語 (FastAPI の `{"detail":...}`) で 200 byte 目が multibyte の途中だと `not a char boundary` で panic。HTTP 失敗時のエラーメッセージ生成という、まさに不正レスポンス処理経路で発火する。
   **修正**: `body.chars().take(200).collect::<String>()` に変更。3 箇所すべて修正。
   **現在**: 推奨どおり `body.chars().take(200).collect()` (`daw_gui/src/voicevox_client.rs:99`, `:152`)。arch-refactor S5-2 で reqwest 依存部が `common/src/voicevox.rs` から `daw_gui/src/voicevox_client.rs` へ移設されたので、旧ファイルには該当コードが無い。byte slice は 1 箇所も残っていない。

7. ✅ **`common/src/voicevox.rs:672-684` — find_sample_rate_field の fallback 空文字列が body.replace で JSON 全体を破壊** [Correctness]
   key 不在時に `String::new()` を返すが、Rust の `str::replace("", ...)` は「全文字間にマッチ」するため、`body.replace("", "\"outputSamplingRate\":48000")` が JSON を完全破壊する。意図 (no-op) と真逆。VOICEVOX がレスポンス形式変更で key を省くと顕在化。
   **修正**: `Option<String>` 返しにし `if let Some` でガード。理想は serde_json で `Value` パースして直接書き換え。
   **現在**: 推奨どおり `fn find_sample_rate_field(json: &str) -> Option<String>` (`daw_plugin_host/src/builtin/voicevox_synth.rs:409`)、caller は `if let Some(field) = find_sample_rate_field(&body)` でガード (`:105`)。

8. ✅ **`daw_audio/src/engine.rs:1936-1957` — has_soloed_contributor が再生コールバック内でヒープ確保 (vec! + push)** [RT-safety]
   `let mut frontier: Vec<u32> = vec![track_id];` + BFS `frontier.push()` が、solo 状態で send/group があると毎バッファ実行される。再生中に solo した瞬間からアロケータを呼ぶ。
   **修正**: BFS バッファを事前確保 (MAX_TRACKS=32 → 固定長 `[u32;64]` + visited bitset)。理想は solo 解決を compile_schedule で edit-time に焼き込む。
   **現在**: `has_soloed_contributor` (BFS + `vec!`) は削除。solo 解決は `Song::ancestor_soloed` の **確保なしの親チェーン走査** (`common/src/model.rs:1975-1990`、`hops > tracks.len()` で循環親も打ち切る) になり、RT 側は `daw_audio/src/graph/execute.rs:430` でそれを読むだけ。同系の `track_has_children` / `track_receives_send` も "RT-safe scan, no alloc" と明記されている。

9. ✅ **`daw_audio/src/graph/compile.rs:388-445` — compile_schedule が audio callback スレッドで tracing::info!/warn! を呼ぶ** [RT-safety]
   doc は edit-time 専用と言うが実呼び出し元は `engine.rs:585 refresh_schedule` (CPAL data callback = RT スレッド)。編集着地ブロックで tracing が global subscriber ロック + String alloc を伴い、当該ブロックでドロップアウト。
   **修正**: PR3 設計通り compile を別スレッドへ移し `ArcSwap<Schedule>` で publish。暫定でも診断 tracing 群を RT 経路から除去。
   **現在**: 推奨 (PR3) どおり実施済み。`daw_audio/src/graph/compile.rs` に `tracing::` は **0 件**。`compile_schedule` の呼び出し元は `daw_audio/src/export.rs:537` (export thread) とテストのみで、RT からは呼ばれない。RT へは `RtBundle` を rtrb の forward ring で配送し、superseded bundle は recycle ring で off-thread drop する (`daw_audio/src/engine.rs:1-19` module doc。「RT で alloc / free / 最終 refcount drop が起きない」)。

10. ✅ **`daw_plugin_host/src/process_server.rs:476-501` — RT dispatch ループ内で tokio UnboundedSender::send が heap alloc** [RT-safety]
    `run_worker` (TIME_CRITICAL + MMCSS の audio dispatch スレッド) で plugin GUI 発の param event を tokio unbounded mpsc に send。block 境界を跨ぐ send で内部 linked-list が alloc。再生中に knob をドラッグすると per-buffer で発火。コメント自身が alloc を認めている。
    **修正**: lock-free SPSC ring (rtrb 等) か固定長 ring + Atomic index に置換し worker 側は「書くだけ」に。plugin-main で drain。
    **現在**: 推奨どおり **固定長 lock-free SPSC ring** `ParamEventRing` を新設 (`daw_plugin_host/src/process_server.rs:196-250`、容量定数は `:165`)。RT worker は `param_ring.push(...)` するだけ (`:760`, `:768`)、専用 drain thread が `ring.pop()` で吸い出して tokio channel へ中継する (`:276-308`)。

11. ✅ **`daw_plugin_host/src/vst3_plugin.rs:995-1000` — process() の RT パス内で tracing::warn! (param プール overflow 時)** [RT-safety]
    1 buffer に 64 超の distinct param で `tracing::warn!` を呼ぶ。automation 多用プロジェクトで到達。`%self.name` の String Display capture も format alloc を生む。
    **修正**: overflow フラグを AtomicBool に立て、`process()` 外 (plugin-main poll / stop_processing) で一度だけログ。
    **現在**: 推奨どおり。RT 側は `param_pool_overflowed.store(true, Relaxed)` のみ (`daw_plugin_host/src/vst3_plugin.rs:117` 宣言 / `:210` store)、ログは `process()` の外で `swap(false)` の edge 検出により 1 度だけ (`:1379-1383`)。`process()` 内に `tracing::` は 0 件。

12. ✅ **`daw_gui/src/import_video.rs:660` — extract_audio_to_wav: channels==0 で整数除算 panic** [FFI-safety]
    `cur_len/4/channels as u64` の channels は WMF が返す外部値。0 でゼロ除算 panic。`WavSpec{channels:0}` を WavWriter に渡す経路にもなる。
    **修正**: sample_rate/channels を読んだ直後に `if channels==0 || sample_rate==0 { return Ok(None) }` で入口ガード。
    **現在**: 推奨どおり入口ガード `if sample_rate == 0 || channels == 0 {` (`daw_gui/src/import_video.rs:233`、`extract_audio_to_wav` は `:197`)。

## 3. Mid 重大度 — 21 件 ✅ / 2 件 △

- ✅ **`daw_gui/src/main.rs:169-189, 257-278` — incoming bridge が stale audio_tx を握り続け、audio respawn 後の OpenPluginShmem/ClosePluginShmem が握りつぶされる** [Concurrency]: respawn で `AppData.audio_tx` だけ差し替わり bridge thread の sender は更新されず、再起動後ロードした plugin の音が出ない。SSoT 違反。→ audio 向け sender の所有者を AppData に一本化。
  **現在**: 一本化済み。送信は live な `self.ipc.audio_tx` から (`daw_gui/src/handler/devices.rs:89` Open / `:413` Close、どちらもコード内に「SSoT (code review 2026-06-06): incoming bridge の stale clone ではなく…」と本レビューを名指しした経緯コメントあり)。respawn 時の差し替えは `daw_gui/src/handler/automation_lanes.rs:981` (None 化) / `:1092` (新 sender)。
- ✅ **`daw_audio/src/audio_worker.rs:336-353` — AudioWorkerPool::shutdown が到達不能でワーカスレッド/イベントハンドルがリーク** [Concurrency]: `shutdown(self)` は Arc 内から move-out できず Drop も無いため、旧プール解体時に worker が `WaitForSingleObject(INFINITE)` で永久ブロック、HANDLE も CloseHandle されずリーク。→ `impl Drop` を追加。
  **現在**: `impl Drop for AudioWorkerPool` (`daw_audio/src/audio_worker.rs:386`)。
- ✅ **`daw_plugin_host/src/process_server.rs:467-469` — drain_out_param_*_into の Vec::append が out 容量超過で再確保** [RT-safety]: collected 側に上限 clamp が無く 64 超 gesture で out が RT 上で再確保。→ MAX_EVENTS clamp。
  **現在**: **収集側**で clamp する形に解決 (append 側より上流で、より確実)。`collect_out_note_try_push` の各 arm が `if out.len() < out.capacity() { out.push(..) }` で事前確保容量を超える push を破棄する (`daw_plugin_host/src/clap_plugin.rs:1443-1446`, `:1462`, `:1481-1484`)。容量は load 時に確定 (`:701-704`、notes 256 / touches 64 / values 256 / releases 64)。
- ✅ **`daw_plugin_host/src/clap_plugin.rs:1104-1112, 1136-1143` — audio thread の try_push callback 内で tracing::warn!** [RT-safety]: malformed gesture を毎ブロック emit する plugin で RT 違反。→ 黙って破棄 or AtomicU64 bump。
  **現在**: 推奨どおり「黙って破棄」。`collect_out_note_try_push` (`daw_plugin_host/src/clap_plugin.rs:1413-1500`) に `tracing::` は 0 件。併せて **FFI 安全性が強化** され、各 arm が deref 前に `header.size < size_of::<..>()` を検証している (malformed event による境界外読みの防御)。
- ✅ **`daw_plugin_host/src/vst3_events.rs:133-140` — Vst3OutEventList::addEvent が無上限 push で RT realloc** [RT-safety]: capacity 64 で上限チェック無し。→ `len()>=CAP` で drop。
  **現在**: 推奨どおり。`const OUT_EVENT_CAP: usize = 4096` (`daw_plugin_host/src/vst3_events.rs:90`)、`if events.len() >= OUT_EVENT_CAP { return kResultOk; }` で超過を破棄 (`:147-149`)。
- ✅ **`daw_plugin_host/src/builtin/voicevox.rs:350-369` — audio thread の process() 内に tracing::debug! が残存** [RT-safety]: `RUST_LOG=debug` で stdout Mutex + I/O が audio callback に。→ 3 箇所削除。
  **現在**: `daw_plugin_host/src/builtin/voicevox.rs` の `process()` 内に `tracing::` は 0 件。
- ✅ **`daw_plugin_host/src/vst3_plugin.rs:477` — aux_input_bus_channels が channelCount(i32) を負値チェック無しで u32 化** [FFI-safety]: 負値→巨大 u32→OOM。→ `.max(0)` + MAX_AUX clamp。
  **現在**: 推奨どおり `(info.channelCount.max(0) as u32).min(MAX_CHANNELS as u32)` + バス数を `MAX_AUX_IN` で打ち切り (`daw_plugin_host/src/vst3_plugin.rs:821-831`)。
- ✅ **`daw_plugin_host/src/main.rs:995-1001 (MoveSlot)` — plugin_lookup / registry の slot を更新せず bookkeeping が陳腐化** [SSoT]: 後続 RemoveSlotPlugin が誤 plugin_id を削除、automation が誤 slot に帰属。→ move 後に全 map を現順序から再構築。
  **現在**: **問題ごと構造的に消滅**。arch-refactor で device の addressing を positional slot から安定 `PluginInstance::id` に変えたため、並び替えで貼り替える bookkeeping 自体が不要になり `ReorderChain` / `MoveSlot` IPC message は削除された (`common/src/protocol.rs:12`「旧 `ReorderChain` message は不要になり削除」、`daw_gui/src/handler/devices.rs:753`)。回帰テストは `daw_gui/tests/app_state/group_track_lifecycle.rs:358`。
- ✅ **`daw_audio/src/sequencer.rs:175-202` / `:183` — collect_events_for_buffer / active_notes の push が上限なしで RT 再確保** [RT-safety]: 高 BPM/多 clip・同時発音 64 超で容量超過。→ push 前 clamp。
  **現在**: 推奨どおり push 前 clamp。`const ACTIVE_NOTES_CAP: usize = MAX_EVENTS` (`daw_audio/src/sequencer.rs:17`)、`if out.len() >= MAX_EVENTS || active_notes.len() >= ACTIVE_NOTES_CAP { .. }` (`:197`)、他の push 経路も `:225` / `:284` で clamp。
- ✅ **`daw_audio/src/engine.rs:1664-1688` — SidechainTap ハンドラの tracing::trace! が RT で毎バッファ発火しうる** [RT-safety]: 対象 plugin 未ロード遷移状態で毎バッファ。→ 削除 or edge 化。
  **現在**: `daw_audio/src/engine.rs` の SidechainTap ハンドラに `tracing::` は 0 件。
- ✅ **`daw_audio/src/graph/compile.rs:41-453` — compile_schedule の大量ヒープ確保が audio callback スレッドで実行** [RT-safety]: #9 と同根。→ PR3 で off-thread 化。
  **現在**: §2 #9 と同じく解決 (compile は RT から呼ばれず、`RtBundle` を ring で配送)。
- ✅ **`common/src/plugin_db.rs:362-379` — read_feature_list が外部 .clap の feature 配列を上限なしポインタ walk** [FFI-safety]: NULL 終端を欠く malformed plugin で境界外読み。→ `for _ in 0..256` 上限。
  **現在**: 推奨どおり `const MAX_FEATURES: usize = 256` で打ち切り、上限到達時のみ警告ログ。**2 コピーとも修正済み** (`daw_plugin_host/src/clap_plugin.rs:1801` / `daw_plugin_host/src/plugin_scan.rs:220`)。関数は `common/src/plugin_db.rs` から daw_plugin_host へ移設された。
- ✅ **`common/src/project.rs:53-101` — load() がデシリアライズ値域を一切検証しない (bpm/time_sig/length_beats)** [Correctness]: 破損/手編集 .daw の bpm<=0/NaN, denominator=0 が下流に流れる。→ `Song::sanitize_ranges()` を SSoT 化。
  **現在**: 推奨どおり `Song::sanitize_ranges()` を SSoT 化 (`common/src/model.rs:1422-1442`)。bpm は `is_finite` 判定 + `clamp(1.0, 1000.0)`、time_sig 分子 `clamp(1,32)` / 分母は 1|2|4|8|16 のホワイトリスト、`length_beats` / `video_framerate` / `video_resolution` も検証。`normalize_after_load()` (`:1473`) から呼ばれ、それを `common/src/project.rs:860` が load 経路で必ず通す。横断テーマ (D) の NaN 素通りもここで塞がっている。
- ✅ **`common/src/project.rs:84-100` — load() が sort 不変条件 (scale_changes / automation points) を再確立しない** [Correctness]: 順序が崩れた .daw で `scale_at`/`evaluate_clip` が黙って誤値。→ load で再ソート。
  **現在**: `normalize_after_load()` が `ensure_scale_changes_sorted()` と `ensure_automation_points_sorted()` を呼ぶ (`common/src/model.rs:1483-1484`)。
- ✅ **`common/src/model.rs:250-290` — gc/ensure video/image source が save/load パスに未接続で doc 契約と乖離** [SSoT]: orphan source がディスクに残る。→ normalize ヘルパに集約し project.rs から呼ぶ。
  **現在**: 推奨どおり 2 つの単一入口に集約。`normalize_for_save()` が `gc_clip_contents` / `gc_audio_sources` / `gc_video_sources` / `gc_image_sources` を呼び (`common/src/model.rs:1448-1453`)、`common/src/project.rs:77` の save 経路が通す。load 側は `normalize_after_load()` が `ensure_video_source_ids` / `ensure_image_source_ids` を通す。
- ✅ **`daw_gui/src/app.rs:2340-2381, 7677-7789` — ノート/オートメーション貼り付けが undo 不能 (push_undo_snapshot 漏れ)** [Correctness]: Ctrl+Z で戻せない。→ paste 冒頭で snapshot。
  **現在**: 両 paste が `edit_song` チョークポイント経由になり、undo snapshot はチョークポイントが無条件で積む (アーキテクチャ不変条件 5。手動 `push_undo_snapshot` は廃止)。`paste_notes_at` → `daw_gui/src/handler/notes.rs:78`、`paste_points_at` → `daw_gui/src/handler/automation.rs:777`。
- ✅ **`daw_gui/src/app.rs:2340-2377` — clipboard JSON の Note を値域検証せず貼り付け** [Error-handling]: pitch>127/負 duration/NaN start が clamp 無しで下流へ。→ clamp/skip、is_finite で弾く。
  **現在**: `daw_gui/src/clipboard.rs` に sanitize 層を新設。Note は `is_finite` で弾き `pitch.min(127)` / `velocity.clamp(1,127)` (`:163-179`)、audio event は `gain_db.clamp(-60,24)` / `pan.clamp(-1,1)` / semitone clamp / fade 非負 (`:185-224`)、automation point は `time_beat` 非負 + `value_norm` 0..=1 (`:232-241`)。
- ✅ **`daw_gui/src/view/arrangement_view.rs:1762-1789` — automation clip の share-group 判定が毎フレーム O(全clip) の N²** [Performance]: → 構築済み refcount map を渡し O(1)。
  **現在**: 推奨どおり。`refcount_by_content: HashMap` を 1 フレームに 1 度だけ batch 構築し (`daw_gui/src/widgets/arrangement/view_build.rs:48`, `:68-81`)、判定は `is_shared` closure で O(1) (`:92-93`)。
- △ **`daw_gui/src/view/arrangement_view.rs:168-321` — tracks: Vec を毎フレーム全track×全clip ぶん alloc** [Performance]: → data_generation 変化時のみ再構築し 1 frame キャッシュ。
  **現在 (部分的)**: arrangement は widget 化され (`daw_gui/src/widgets/arrangement/`)、**描画** は `heavy()` + `cached(viewport_key)` で粗粒度キャッシュされる (`run.rs:1883`, `:2089`)。指摘された N² の depth / refcount 再計算も batch 化済み (`view_build.rs:48`)。ただし view 構造体の構築 (`view_build.rs:47 build()`) 自体は immediate-mode の設計どおり毎フレーム走り `Vec` を確保する。**correctness の問題ではなく、残っているのは確保コストのみ。**
- ✅ **`daw_gui/src/view/audio_editor.rs:395-396` — 波形 event ごとに planes_borrowed Vec を毎フレーム確保** [Performance]: → SmallVec / 固定長。
  **現在**: 推奨どおり固定長スタック配列 `[&[f32]; MAX_WAVEFORM_CHANNELS]` を使い、稀な >2ch source のときだけ `Vec` にフォールバックする (`daw_gui/src/view/audio_editor.rs:484-493`。チャンネルを黙って捨てないための fallback)。
- △ **`daw_gui/src/view/track_inspector.rs:1386-1407` — Vocal speaker dropdown のラベル列を毎フレーム全構築 (clone + format! + 2段 collect)** [Performance]: → 1 フレームキャッシュ。
  **現在 (部分的)**: 指摘された `clone` + `format!` + 2 段 collect は解消し、借用 `&str` の 1 段 collect になった (`daw_gui/src/view/track_inspector/mod.rs:2025-2026`)。`Vec<&str>` の確保自体は毎フレーム残る。**文字列の複製と整形は無くなっているので、元指摘のコストの大半は消えている。**
- ✅ **`daw_gui/src/text_compose.rs:161-225` / `image_compose.rs:151-193` / `group_compose.rs:142-154` — overlay の lane 解決が per-frame で線形 find / O(tracks³) 走査** [Performance]: → lane 索引を 1 度作る、隣接を 1 pass 構築。
  **現在**: 3 ファイルすべて推奨どおり索引化。`text_compose.rs:209` が `lane_index: HashMap<TextBuiltinParam, &AutomationLane>` を 1 度構築、`image_compose.rs:101-104` が `ImageLaneIndex::build(track)` を「`track.automation_lanes` の single pass」で構築 (コメントに「instead of re-`find`ing」と経緯あり)、`group_compose.rs:211` が `by_id: HashMap<u32, &Track>` を 1 度構築。
- ✅ **`daw_gui/src/video_playback_worker.rs:262-294` — ring 先読みが低フレームレート project で slot ごとに full seek を誘発しうる** [Performance]: fps<=10 で毎 slot keyframe 再 walk。→ budget を動的化。
  **現在**: **問題ごと構造的に消滅**。per-slot GPU テクスチャの先読み ring は Media Foundation 撤去と同時に削除され (`docs/plan_video_decode_unify.md`)、libav の 1-frame-latest BGRA sink が「GUI が今表示している中心フレームだけ」をデコードする形になった (`daw_gui/src/video_playback_worker.rs:31-35`, `:214-225`)。`DecodedRing` は 1 slot スナップショットの互換ラッパとして残るだけで、slot ごとの seek walk が起きない。

## 4. Low 重大度 (抜粋) — 6 件 ✅ / 残り ❓ 未再検証

engine.rs:595 (atomic 多重 load), automation.rs:84 (不要 clone), main.rs:141 (NaN 未検証 LoadSong), export.rs:85 (overflow → saturating_add), process_server.rs:405/430 (frames 未再検証 / panic で pool ハング), clap_plugin.rs:366 (param count を with_capacity 信用), plugin_db.rs:324 (allocation bomb), vst3_scan.rs:298 (tuid_to_hex 重複定義 SSoT), win_sem.rs:16-60 (dead API; 削除前 smoke test 要), wire.rs:14 (read/write サイズ上限非対称), model.rs:385 (alloc_track_id overflow), automation.rs:338 / timing.rs:27 (NaN sanitize 漏れ), voicevox_cache.rs:29 (cache 無制限成長), app.rs:13432 (automation 録音途中の is_dirty 窓), track_inspector.rs:1355 (cursor None → track 0 誤対象), libav_decoder.rs:195 (decode walk 1024 上限が黒落ち退行リスク), libav_encoder.rs:98 (framerate as i32 overflow), import_video.rs:452 (DEFAULT_STRIDE 無視) ほか計 ~50 件。

**再検証したもの (6 件、すべて ✅ 解決済み)**:

- ✅ **wire.rs:14 read/write サイズ上限非対称** → `MAX_MESSAGE_BYTES = 16 MiB` を **書き込み側 (`common/src/wire.rs:14`) と読み込み側 (`:46`) の両方**で検査。非対称は解消。境界テストもある (`:108`)。
- ✅ **model.rs:385 alloc_track_id overflow** → `clamp(1, MASTER_TRACK_ID - 1)` + `saturating_add` で、sentinel (`0` / `u32::MAX`) を絶対に配らず wrap もしない (`common/src/model.rs:787-793`)。
- ✅ **export.rs:85 overflow → saturating_add** → `saturating_add` 化 (`daw_audio/src/export.rs:321`, `:438-440`)。
- ✅ **voicevox_cache.rs:29 cache 無制限成長** → `MAX_CACHE_BYTES = 1 GiB` + mtime 最古から削除する `prune()` (`daw_plugin_host/src/builtin/voicevox_cache.rs:35`, `:145-168`)。書き込みのたびに `prune()` (`:120`)。
- ✅ **libav_encoder.rs:98 framerate as i32 overflow** → キャスト前に `if framerate <= 0.0 || framerate > 1000.0 { return Err(..) }` で範囲を確定 (`daw_gui/src/libav_encoder.rs:94-99`。「`framerate` is bounded to (0, 1000] above」とコメントあり)。
- ✅ **plugin_db.rs:324 allocation bomb** の同系: 外部 plugin の feature 配列上限は §3 の `MAX_FEATURES = 256` で解決。plugin DB キャッシュの load も空 id エントリを drop する (`common/src/plugin_db.rs:176-186`)。

**❓ 未再検証 (約 44 件)**: 上記以外の Low 項目。元レポートが「ほか計 ~50 件」と抜粋のみで、
名指しされていない項目は文書からは追えません。名指しの残り
(engine.rs:595 / automation.rs:84 / main.rs:141 / process_server.rs:405,430 /
clap_plugin.rs:366 / vst3_scan.rs:298 / win_sem.rs:16-60 / automation.rs:338 /
timing.rs:27 / app.rs:13432 / track_inspector.rs:1355 / libav_decoder.rs:195 /
import_video.rs:452) も今回は個別に追っていません。
**いずれも元レポートで Low = 「実害が小さい / 到達確率が低い」と判定されたものです。**

## 5. 横断的テーマ

- ✅ **(A) RT オーディオパス上のロギング (最頻・最重要)**: `tracing` が RT スレッドに多数残存 (engine/compile/process_server/clap_plugin/vst3_plugin/voicevox)。subscriber ロック + String format + I/O を伴い CLAUDE.md「ログ出力を呼ばない」に反する。**統一方針: RT パスの全 tracing を撤去し、必要なら Atomic フラグ + 非 RT スレッドで一度だけログする「edge 検出 + 非 RT flush」に揃える。**
  **現在**: 方針どおり適用済み。compile.rs / builtin voicevox の `process()` / clap try_push / SidechainTap から `tracing` は消え、vst3 の param pool overflow は `AtomicBool` + `swap(false)` の edge 検出で非 RT 側から 1 度だけログする形になった (§2 #9 #11、§3 の該当 4 件)。
- ✅ **(B) RT パスの無上限 push / append によるヒープ再確保**: sequencer/process_server/vst3_events/clap_plugin/engine。**統一方針: 全 push 経路に固定 CAP クランプ、または lock-free 固定長 ring。**
  **現在**: 方針どおり。sequencer は `MAX_EVENTS` / `ACTIVE_NOTES_CAP`、vst3_events は `OUT_EVENT_CAP`、clap の out collector は `capacity()` clamp、process_server は固定長 SPSC ring (`ParamEventRing`)。engine の BFS は `ancestor_soloed` の確保なし走査に置換。
- ✅ **(C) 信頼境界での値域・サイズ未検証**: 永続化・IPC・plugin scan・clipboard・WAV/video import の全境界で bpm=0/NaN, denominator=0, channels=0, count=u32::MAX 等が 0 除算/無限ループ/OOM/panic を起こす。**統一方針: 各境界で 1 度だけ sanitize する SSoT ヘルパを入口に集約。**
  **現在**: 方針どおり入口に集約。永続化 = `Song::sanitize_ranges` / `normalize_after_load`、clipboard = `daw_gui/src/clipboard.rs` の sanitize 群、plugin scan = `MAX_FEATURES` / `MAX_AUX_IN` / 空 id drop、IPC = `wire.rs` の双方向 `MAX_MESSAGE_BYTES`、video/WAV import = `import_video.rs:233` / `render_video.rs:205` の channels/sample_rate 0 ガード。
- ✅ **(D) NaN 伝播**: `bpm<=0.0` 系ガードが `NaN<=0.0==false` で素通り。**統一方針: `!is_finite() || <=0` で書く。**
  **現在**: 方針どおり。`sanitize_ranges` が全数値フィールドを `is_finite` で判定 (`common/src/model.rs:1422-1442`)、plugin transport は `TransportBlock::derive` の `sanitize_pos()` が `as i64` の前に非有限を潰す (`daw_plugin_host/src/process_scaffold.rs:145-152`。CLAP / VST3 で共有なので両フォーマットが乖離しない)。
- ✅ **(E) SSoT 違反**: audio_tx の 2 経路分裂、MoveSlot の lookup 乖離、tuid_to_hex 重複定義、refcount 再計算、gc 未接続。
  **現在**: audio_tx = `self.ipc.audio_tx` 一本化、MoveSlot = 安定 id 化で IPC ごと削除、refcount = 1 フレーム batch、gc = `normalize_for_save` / `normalize_after_load` の 2 入口に集約。`tuid_to_hex` の重複は今回未再検証 (§4 の ❓)。
- △ **(F) GUI 描画スレッドの per-frame アロケーション**: cache 無しの毎フレーム Vec/Arc::from/format!/String clone。**統一方針: data_generation 変化時のみ再構築する 1-frame キャッシュ + Arc<str> + lane 索引化。**
  **現在 (部分的)**: lane 索引化 (3 compose ファイル) と波形 planes の固定長化、refcount の batch 化、描画の `heavy()/cached(viewport_key)` 化は完了。immediate-mode 由来の view 構造体 (`Vec`) 構築は毎フレーム残る (§3 の △ 2 件)。

## 6. 推奨着手順 (High 上位) — 全 10 件 ✅ 着手・解決済み

1. ✅ **model.rs:842 / 683 — song_lanes の GC/ensure 漏れ** (テンポ automation がロード時に消失する確定データロス。両方まとめて修正 + round-trip テスト。最優先) → §2 #1 #2
2. ✅ **app.rs:14052 ほか — ミキサー live setter の is_dirty 漏れ** (fader/mute/solo/send が silent data loss。1 行 × 6 箇所で即修正可) → §2 #3
3. ✅ **voicevox.rs:289/354/408 — UTF-8 char 境界 panic** (`chars().take()` で 3 箇所即修正) → §2 #6
4. ✅ **voicevox.rs:672 — find_sample_rate_field の空文字列 JSON 破壊** (`Option` 返し) → §2 #7
5. ✅ **voicevox.rs:188 — synth thread の join 無限ブロック** (AtomicBool flag + reqwest timeout) → §2 #5
6. ✅ **import_video.rs:660 — channels==0 ゼロ除算 panic** (入口ガード 1 行) → §2 #12
7. ✅ **clap_plugin.rs:1260 — in_events の time ソート (CLAP spec 違反)** → §2 #4
8. ✅ **engine.rs:1936 — has_soloed_contributor の RT ヒープ確保** (固定長バッファ化) → §2 #8
9. ✅ **process_server.rs:476 + compile.rs:388 + vst3_plugin.rs:995 — RT パスの tracing/tokio send 群** (横断テーマ A/B を一括対処) → §2 #9 #10 #11
10. ✅ **main.rs:169 — audio respawn 後の OpenPluginShmem 握りつぶし** (audio_tx の SSoT 化) → §3 の 1 件目

## 7. 要追加調査 (uncertain) — 4 件 ✅ / 4 件 ❓ 未再検証

修正前に実機/コンテキスト確認を推奨:
- ❓ `compile.rs:289-291` aux_in_port の `as u8` 範囲未検証 (compile-time、実害低) — **未再検証**
- ✅ `clap_plugin.rs:1193-1212` transport f64→i64 が NaN で i64::MIN/MAX (→ is_finite チェック)
  → 解決済み。`as i64` の前に `TransportBlock::derive` が `sanitize_pos()` を全 f64 位置に適用する (`daw_plugin_host/src/process_scaffold.rs:145-152`)。CLAP (`clap_plugin.rs:1534-1538`) と VST3 が同じ block を共有するので、片方だけ直り忘れることが構造的に起きない。
- ✅ `plugin_db.rs:85-94` load_from_file 値域未検証 + ensure_builtins の暗黙規約依存
  → キャッシュ load は空 `id` のエントリを警告付きで drop する (`common/src/plugin_db.rs:176-186`)。
- ❓ `model.rs:165-300` Song/AudioSource/VideoSource serde 全体の値域検証の置き場所 (model vs project.rs) の設計判断 — **設計判断としては決着済み** (`Song::sanitize_ranges` = model 側に置き、`project::load` が `normalize_after_load` 経由で必ず呼ぶ)。ただし **全フィールドを網羅しているかは未再検証**。
- ❓ `app.rs:5551-5557` project load 後の bpm/time_sig 再検証 (project.rs sanitize と重複領域) — **未再検証**
- ❓ `bootstrap.rs:100-141` respawn の pipe 再作成競合 (致命度は要実測) — **未再検証**
- ✅ `video_playback_worker.rs:262-294` ring 先読みの full seek (fps 分布次第)
  → per-slot 先読み ring ごと削除され、1-frame-latest sink で中心フレームのみデコードする形になった (§3 の該当項目)。
- ✅ `render_video.rs:123-140` export WAV channels==0 で無限ループ / sample_rate==0 で time_base 分母 0 (到達確率不明だが帰結が重い)
  → 入口ガード `if spec.channels == 0 || spec.sample_rate == 0 {` (`daw_gui/src/render_video.rs:205`)。
