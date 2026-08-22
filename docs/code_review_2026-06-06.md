<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# daw_01 全体コードレビュー (2026-06-06)

手法: 20 ユニット (66,584 行) をレンズ別 (RT安全性/FFI/perf/correctness) に並列レビュー →
各所見を別エージェントが敵対的に検証 (偽陽性を実コード参照で排除) → 集約。
41 エージェント / 676 tool uses。**confirmed 83 / uncertain 8 / rejected 13**。

---

## 1. エグゼクティブサマリ

コードベース全体は堅牢な 3 プロセス分離アーキテクチャと明確な RT 制約方針を持つが、レビューで最も重大かつ反復的に現れたテーマは **(A) RT オーディオパス上でのヒープ確保・ロギング・tokio send の混入** と **(B) 永続化/IPC/clipboard 等の信頼境界での値域未検証** の 2 つである。特に深刻なのは、曲レベル automation (`song_lanes`) が GC/正規化パスから完全に漏れており **テンポオートメーションが保存時に消失する** データロスバグ (model.rs:842, 683) と、ミキサーの live setter が `is_dirty` を立てず **fader/mute/solo 編集が黙って破棄される** バグ (app.rs:14052 ほか) で、いずれもユーザーの作業成果を失わせる。CLAP の in_events が time 昇順にソートされず spec 違反 (clap_plugin.rs:1260) になっている点、VOICEVOX synth thread が永久失敗 job で join 無限ブロックする点 (voicevox.rs:188) も実害が大きい。横断的には「RT パスの `tracing` 呼び出し」が 8 ファイル以上、「外部入力の値域/サイズ未検証」が IPC・project load・plugin scan・clipboard・WAV import の全境界に散在している。

## 2. High 重大度

1. **`common/src/model.rs:842-860` — gc_clip_contents が song_lanes を live 集合に含めず曲レベル automation が保存前に削除される** [Correctness]
   `gc_clip_contents` は `tracks[].clips` と `tracks[].automation_lanes[].clips` のみから live 集合を構築し、`song_lanes[].clips` (SongTempo/TimeSig master lane) を走査しない。テンポオートメーションを描いて保存すると content_id が live 判定されず `clip_contents` / `clip_content_names` から retain 除外され、次回ロードでカーブが全消失する。`clip_content_refcount` も同様に song_lanes を歩かず共有判定が壊れる。
   **修正**: `gc_clip_contents` / `clip_content_refcount` の両方に `song_lanes[].clips` の二重ループを追加。round-trip 回帰テストを追加。

2. **`common/src/model.rs:683-784` — ensure_clip_contents が song_lanes のクリップを移行対象にせず content_id sentinel が再採番されない** [Correctness]
   `tracks[].clips` と `tracks[].automation_lanes[].clips` のみ走査するため、曲レベル automation clip が `content_id==0` のまま残り `clip_contents[0]` にエントリが作られず、automation eval / GUI lookup が常に空フォールバックする。
   **修正**: `ensure_clip_contents` 末尾に song_lanes への content_id 再採番 + `or_insert_with(Automation default)` ループを追加。max content_id 走査にも song_lanes を含める。

3. **`daw_gui/src/app.rs:14052-14088, 14194-14271` — ミキサー live setter (volume/pan/mute/solo/send) が is_dirty を立てず silent data loss** [Correctness]
   `set_track_volume`/`set_track_pan`/`toggle_track_mute`/`toggle_track_solo`/`set_send_gain`/`set_send_enabled` は persisted フィールドを書き換えるが realtime IPC 直送のみで `sync_song_to_plugin_host` を通らず `is_dirty` を立てない。fader 等だけ触って閉じると保存確認モーダルが出ず破棄され、autosave も走らない。`SetSongBpmFromScrub` が明示的に防いでいるのと同型の漏れ。
   **修正**: 各 setter 末尾で値が変化したときに `self.is_dirty = true;` を立てる。

4. **`daw_plugin_host/src/clap_plugin.rs:1260-1277` — in_events が time 昇順にソートされていない (CLAP spec 違反)** [Correctness]
   `EventListView` は note 列と param 列を別々にソートして単純連結するため、グローバルには time 昇順でない (例: `[note@200, param@50]`)。CLAP の `clap_input_events` は time 単調性を host 契約として要求しており、二分探索/sample-accurate 補間する plugin が誤動作・取りこぼしを起こす。automation + 演奏混在の典型ケースで踏む。
   **修正**: note と param を 1 本の time 昇順 stable merge 列にする。merge 用 index バッファは activate 時に pre-allocate して RT 安全を維持。

5. **`daw_plugin_host/src/builtin/voicevox.rs:188-253` — synth thread が永久失敗 job で shutdown を検知できず join() 無限ブロック** [Concurrency]
   engine 未起動時、synth job が coalesce slot に戻され retry し続けるため `strong_count<=1` の終了チェックに永久到達しない。deactivate/Drop → `stop_synth_thread()` → `join()` で plugin-main thread が無期限ブロックし、plugin/track 削除・アプリ終了がフリーズ。reqwest client が timeout 未設定 (voicevox.rs:557) でハングリスクが二重。
   **修正**: processing thread に AtomicBool shutdown flag を持たせループ先頭でチェック。reqwest に `.timeout(5s)` を付与。

6. **`common/src/voicevox.rs:289 (および 354, 408)` — エラー body の byte slice が UTF-8 char 境界で panic** [Correctness]
   `&body[..body.len().min(200)]` は byte 単位 index。日本語 (FastAPI の `{"detail":...}`) で 200 byte 目が multibyte の途中だと `not a char boundary` で panic。HTTP 失敗時のエラーメッセージ生成という、まさに不正レスポンス処理経路で発火する。
   **修正**: `body.chars().take(200).collect::<String>()` に変更。3 箇所すべて修正。

7. **`common/src/voicevox.rs:672-684` — find_sample_rate_field の fallback 空文字列が body.replace で JSON 全体を破壊** [Correctness]
   key 不在時に `String::new()` を返すが、Rust の `str::replace("", ...)` は「全文字間にマッチ」するため、`body.replace("", "\"outputSamplingRate\":48000")` が JSON を完全破壊する。意図 (no-op) と真逆。VOICEVOX がレスポンス形式変更で key を省くと顕在化。
   **修正**: `Option<String>` 返しにし `if let Some` でガード。理想は serde_json で `Value` パースして直接書き換え。

8. **`daw_audio/src/engine.rs:1936-1957` — has_soloed_contributor が再生コールバック内でヒープ確保 (vec! + push)** [RT-safety]
   `let mut frontier: Vec<u32> = vec![track_id];` + BFS `frontier.push()` が、solo 状態で send/group があると毎バッファ実行される。再生中に solo した瞬間からアロケータを呼ぶ。
   **修正**: BFS バッファを事前確保 (MAX_TRACKS=32 → 固定長 `[u32;64]` + visited bitset)。理想は solo 解決を compile_schedule で edit-time に焼き込む。

9. **`daw_audio/src/graph/compile.rs:388-445` — compile_schedule が audio callback スレッドで tracing::info!/warn! を呼ぶ** [RT-safety]
   doc は edit-time 専用と言うが実呼び出し元は `engine.rs:585 refresh_schedule` (CPAL data callback = RT スレッド)。編集着地ブロックで tracing が global subscriber ロック + String alloc を伴い、当該ブロックでドロップアウト。
   **修正**: PR3 設計通り compile を別スレッドへ移し `ArcSwap<Schedule>` で publish。暫定でも診断 tracing 群を RT 経路から除去。

10. **`daw_plugin_host/src/process_server.rs:476-501` — RT dispatch ループ内で tokio UnboundedSender::send が heap alloc** [RT-safety]
    `run_worker` (TIME_CRITICAL + MMCSS の audio dispatch スレッド) で plugin GUI 発の param event を tokio unbounded mpsc に send。block 境界を跨ぐ send で内部 linked-list が alloc。再生中に knob をドラッグすると per-buffer で発火。コメント自身が alloc を認めている。
    **修正**: lock-free SPSC ring (rtrb 等) か固定長 ring + Atomic index に置換し worker 側は「書くだけ」に。plugin-main で drain。

11. **`daw_plugin_host/src/vst3_plugin.rs:995-1000` — process() の RT パス内で tracing::warn! (param プール overflow 時)** [RT-safety]
    1 buffer に 64 超の distinct param で `tracing::warn!` を呼ぶ。automation 多用プロジェクトで到達。`%self.name` の String Display capture も format alloc を生む。
    **修正**: overflow フラグを AtomicBool に立て、`process()` 外 (plugin-main poll / stop_processing) で一度だけログ。

12. **`daw_gui/src/import_video.rs:660` — extract_audio_to_wav: channels==0 で整数除算 panic** [FFI-safety]
    `cur_len/4/channels as u64` の channels は WMF が返す外部値。0 でゼロ除算 panic。`WavSpec{channels:0}` を WavWriter に渡す経路にもなる。
    **修正**: sample_rate/channels を読んだ直後に `if channels==0 || sample_rate==0 { return Ok(None) }` で入口ガード。

## 3. Mid 重大度

- **`daw_gui/src/main.rs:169-189, 257-278` — incoming bridge が stale audio_tx を握り続け、audio respawn 後の OpenPluginShmem/ClosePluginShmem が握りつぶされる** [Concurrency]: respawn で `AppData.audio_tx` だけ差し替わり bridge thread の sender は更新されず、再起動後ロードした plugin の音が出ない。SSoT 違反。→ audio 向け sender の所有者を AppData に一本化。
- **`daw_audio/src/audio_worker.rs:336-353` — AudioWorkerPool::shutdown が到達不能でワーカスレッド/イベントハンドルがリーク** [Concurrency]: `shutdown(self)` は Arc 内から move-out できず Drop も無いため、旧プール解体時に worker が `WaitForSingleObject(INFINITE)` で永久ブロック、HANDLE も CloseHandle されずリーク。→ `impl Drop` を追加。
- **`daw_plugin_host/src/process_server.rs:467-469` — drain_out_param_*_into の Vec::append が out 容量超過で再確保** [RT-safety]: collected 側に上限 clamp が無く 64 超 gesture で out が RT 上で再確保。→ MAX_EVENTS clamp。
- **`daw_plugin_host/src/clap_plugin.rs:1104-1112, 1136-1143` — audio thread の try_push callback 内で tracing::warn!** [RT-safety]: malformed gesture を毎ブロック emit する plugin で RT 違反。→ 黙って破棄 or AtomicU64 bump。
- **`daw_plugin_host/src/vst3_events.rs:133-140` — Vst3OutEventList::addEvent が無上限 push で RT realloc** [RT-safety]: capacity 64 で上限チェック無し。→ `len()>=CAP` で drop。
- **`daw_plugin_host/src/builtin/voicevox.rs:350-369` — audio thread の process() 内に tracing::debug! が残存** [RT-safety]: `RUST_LOG=debug` で stdout Mutex + I/O が audio callback に。→ 3 箇所削除。
- **`daw_plugin_host/src/vst3_plugin.rs:477` — aux_input_bus_channels が channelCount(i32) を負値チェック無しで u32 化** [FFI-safety]: 負値→巨大 u32→OOM。→ `.max(0)` + MAX_AUX clamp。
- **`daw_plugin_host/src/main.rs:995-1001 (MoveSlot)` — plugin_lookup / registry の slot を更新せず bookkeeping が陳腐化** [SSoT]: 後続 RemoveSlotPlugin が誤 plugin_id を削除、automation が誤 slot に帰属。→ move 後に全 map を現順序から再構築。
- **`daw_audio/src/sequencer.rs:175-202` / `:183` — collect_events_for_buffer / active_notes の push が上限なしで RT 再確保** [RT-safety]: 高 BPM/多 clip・同時発音 64 超で容量超過。→ push 前 clamp。
- **`daw_audio/src/engine.rs:1664-1688` — SidechainTap ハンドラの tracing::trace! が RT で毎バッファ発火しうる** [RT-safety]: 対象 plugin 未ロード遷移状態で毎バッファ。→ 削除 or edge 化。
- **`daw_audio/src/graph/compile.rs:41-453` — compile_schedule の大量ヒープ確保が audio callback スレッドで実行** [RT-safety]: #9 と同根。→ PR3 で off-thread 化。
- **`common/src/plugin_db.rs:362-379` — read_feature_list が外部 .clap の feature 配列を上限なしポインタ walk** [FFI-safety]: NULL 終端を欠く malformed plugin で境界外読み。→ `for _ in 0..256` 上限。
- **`common/src/project.rs:53-101` — load() がデシリアライズ値域を一切検証しない (bpm/time_sig/length_beats)** [Correctness]: 破損/手編集 .daw の bpm<=0/NaN, denominator=0 が下流に流れる。→ `Song::sanitize_ranges()` を SSoT 化。
- **`common/src/project.rs:84-100` — load() が sort 不変条件 (scale_changes / automation points) を再確立しない** [Correctness]: 順序が崩れた .daw で `scale_at`/`evaluate_clip` が黙って誤値。→ load で再ソート。
- **`common/src/model.rs:250-290` — gc/ensure video/image source が save/load パスに未接続で doc 契約と乖離** [SSoT]: orphan source がディスクに残る。→ normalize ヘルパに集約し project.rs から呼ぶ。
- **`daw_gui/src/app.rs:2340-2381, 7677-7789` — ノート/オートメーション貼り付けが undo 不能 (push_undo_snapshot 漏れ)** [Correctness]: Ctrl+Z で戻せない。→ paste 冒頭で snapshot。
- **`daw_gui/src/app.rs:2340-2377` — clipboard JSON の Note を値域検証せず貼り付け** [Error-handling]: pitch>127/負 duration/NaN start が clamp 無しで下流へ。→ clamp/skip、is_finite で弾く。
- **`daw_gui/src/view/arrangement_view.rs:1762-1789` — automation clip の share-group 判定が毎フレーム O(全clip) の N²** [Performance]: → 構築済み refcount map を渡し O(1)。
- **`daw_gui/src/view/arrangement_view.rs:168-321` — tracks: Vec を毎フレーム全track×全clip ぶん alloc** [Performance]: → data_generation 変化時のみ再構築し 1 frame キャッシュ。
- **`daw_gui/src/view/audio_editor.rs:395-396` — 波形 event ごとに planes_borrowed Vec を毎フレーム確保** [Performance]: → SmallVec / 固定長。
- **`daw_gui/src/view/track_inspector.rs:1386-1407` — Vocal speaker dropdown のラベル列を毎フレーム全構築 (clone + format! + 2段 collect)** [Performance]: → 1 フレームキャッシュ。
- **`daw_gui/src/text_compose.rs:161-225` / `image_compose.rs:151-193` / `group_compose.rs:142-154` — overlay の lane 解決が per-frame で線形 find / O(tracks³) 走査** [Performance]: → lane 索引を 1 度作る、隣接を 1 pass 構築。
- **`daw_gui/src/video_playback_worker.rs:262-294` — ring 先読みが低フレームレート project で slot ごとに full seek を誘発しうる** [Performance]: fps<=10 で毎 slot keyframe 再 walk。→ budget を動的化。

## 4. Low 重大度 (抜粋)

engine.rs:595 (atomic 多重 load), automation.rs:84 (不要 clone), main.rs:141 (NaN 未検証 LoadSong), export.rs:85 (overflow → saturating_add), process_server.rs:405/430 (frames 未再検証 / panic で pool ハング), clap_plugin.rs:366 (param count を with_capacity 信用), plugin_db.rs:324 (allocation bomb), vst3_scan.rs:298 (tuid_to_hex 重複定義 SSoT), win_sem.rs:16-60 (dead API; 削除前 smoke test 要), wire.rs:14 (read/write サイズ上限非対称), model.rs:385 (alloc_track_id overflow), automation.rs:338 / timing.rs:27 (NaN sanitize 漏れ), voicevox_cache.rs:29 (cache 無制限成長), app.rs:13432 (automation 録音途中の is_dirty 窓), track_inspector.rs:1355 (cursor None → track 0 誤対象), libav_decoder.rs:195 (decode walk 1024 上限が黒落ち退行リスク), libav_encoder.rs:98 (framerate as i32 overflow), import_video.rs:452 (DEFAULT_STRIDE 無視) ほか計 ~50 件。

## 5. 横断的テーマ

- **(A) RT オーディオパス上のロギング (最頻・最重要)**: `tracing` が RT スレッドに多数残存 (engine/compile/process_server/clap_plugin/vst3_plugin/voicevox)。subscriber ロック + String format + I/O を伴い CLAUDE.md「ログ出力を呼ばない」に反する。**統一方針: RT パスの全 tracing を撤去し、必要なら Atomic フラグ + 非 RT スレッドで一度だけログする「edge 検出 + 非 RT flush」に揃える。**
- **(B) RT パスの無上限 push / append によるヒープ再確保**: sequencer/process_server/vst3_events/clap_plugin/engine。**統一方針: 全 push 経路に固定 CAP クランプ、または lock-free 固定長 ring。**
- **(C) 信頼境界での値域・サイズ未検証**: 永続化・IPC・plugin scan・clipboard・WAV/video import の全境界で bpm=0/NaN, denominator=0, channels=0, count=u32::MAX 等が 0 除算/無限ループ/OOM/panic を起こす。**統一方針: 各境界で 1 度だけ sanitize する SSoT ヘルパを入口に集約。**
- **(D) NaN 伝播**: `bpm<=0.0` 系ガードが `NaN<=0.0==false` で素通り。**統一方針: `!is_finite() || <=0` で書く。**
- **(E) SSoT 違反**: audio_tx の 2 経路分裂、MoveSlot の lookup 乖離、tuid_to_hex 重複定義、refcount 再計算、gc 未接続。
- **(F) GUI 描画スレッドの per-frame アロケーション**: cache 無しの毎フレーム Vec/Arc::from/format!/String clone。**統一方針: data_generation 変化時のみ再構築する 1-frame キャッシュ + Arc<str> + lane 索引化。**

## 6. 推奨着手順 (High 上位)

1. **model.rs:842 / 683 — song_lanes の GC/ensure 漏れ** (テンポ automation がロード時に消失する確定データロス。両方まとめて修正 + round-trip テスト。最優先)
2. **app.rs:14052 ほか — ミキサー live setter の is_dirty 漏れ** (fader/mute/solo/send が silent data loss。1 行 × 6 箇所で即修正可)
3. **voicevox.rs:289/354/408 — UTF-8 char 境界 panic** (`chars().take()` で 3 箇所即修正)
4. **voicevox.rs:672 — find_sample_rate_field の空文字列 JSON 破壊** (`Option` 返し)
5. **voicevox.rs:188 — synth thread の join 無限ブロック** (AtomicBool flag + reqwest timeout)
6. **import_video.rs:660 — channels==0 ゼロ除算 panic** (入口ガード 1 行)
7. **clap_plugin.rs:1260 — in_events の time ソート (CLAP spec 違反)**
8. **engine.rs:1936 — has_soloed_contributor の RT ヒープ確保** (固定長バッファ化)
9. **process_server.rs:476 + compile.rs:388 + vst3_plugin.rs:995 — RT パスの tracing/tokio send 群** (横断テーマ A/B を一括対処)
10. **main.rs:169 — audio respawn 後の OpenPluginShmem 握りつぶし** (audio_tx の SSoT 化)

## 7. 要追加調査 (uncertain)

修正前に実機/コンテキスト確認を推奨:
- `compile.rs:289-291` aux_in_port の `as u8` 範囲未検証 (compile-time、実害低)
- `clap_plugin.rs:1193-1212` transport f64→i64 が NaN で i64::MIN/MAX (→ is_finite チェック)
- `plugin_db.rs:85-94` load_from_file 値域未検証 + ensure_builtins の暗黙規約依存
- `model.rs:165-300` Song/AudioSource/VideoSource serde 全体の値域検証の置き場所 (model vs project.rs) の設計判断
- `app.rs:5551-5557` project load 後の bpm/time_sig 再検証 (project.rs sanitize と重複領域)
- `bootstrap.rs:100-141` respawn の pipe 再作成競合 (致命度は要実測)
- `video_playback_worker.rs:262-294` ring 先読みの full seek (fps 分布次第)
- `render_video.rs:123-140` export WAV channels==0 で無限ループ / sample_rate==0 で time_base 分母 0 (到達確率不明だが帰結が重い)
