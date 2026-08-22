// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! handler::transport — 再生 / 停止 / loop / panic / seek
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use common::protocol::{AudioCommand};

/// [`AppData::play`] の結果。 録音開始が「本当に走り出したか」で分岐する
/// (r.md #51) ため、要求が通ったかどうかを型で返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayOutcome {
    /// engine へ `Play` を送った (実際に走り出したかは `on_tick` が観測する)。
    Started,
    /// 読み込み待ちで queue した。 完了時に `pending_play` が再発火する。
    Queued,
    /// 開始できない (書き出し中)。 status_message は `play()` が出している。
    Refused,
}

impl AppData {
    // -------- Playback -----------------------------------------------------

    /// 再生を開始する **唯一の口**。 録音開始 (`start_recording`) もここを通る
    /// ので、書き出し中の拒否・読み込み待ちの queue・停止ホーム
    /// (`playback_origin_beat`) の捕捉が録音でも同じように効く。
    pub(crate) fn play(&mut self) -> PlayOutcome {
        self.start_transport(None)
    }

    /// [`Self::play`] の本体。 `record` が `Some(preroll_samples)` なら
    /// 「録音として走り出す」 (`0` = count-in 無し) で、録音の開始だけがこれを使う。
    ///
    /// 録音の開始を `play()` の外から送らないのが要点 — 別々に送ると、
    /// `StartRecording` が届く前の 1 バッファだけ曲が進んでから count-in / 録音に
    /// 入る、という取りこぼしが生まれる。ここなら送信順が保証される。
    pub(crate) fn start_transport(&mut self, record: Option<u64>) -> PlayOutcome {
        // export 中は再生を禁止する。音声 freewheel フェーズの realtime play は
        // offline render と競合し、書き出される音声を壊しうる（映像フェーズは
        // 独立だが、 混乱を避けて export 全体で一律に止める）。標準 WAV export も
        // `export_stage` が立つので同じ gate で止まる（旧構造では WAV export 中に
        // 再生できてしまい render を壊しえた）。
        // r.md #54: ラウドネス解析も同じ freewheel 経路なので同じ gate で止める
        // (解析中はオーディオ出力が無音化され、プラグインは走査スレッドが占有する
        // ので、そもそも音は出せない)。
        if self.offline_render_busy() {
            self.ui_ephemeral.status_message = if self.loudness.phase.is_busy() {
                "ラウドネス解析中は再生できません".into()
            } else {
                "書き出し中は再生できません".into()
            };
            return PlayOutcome::Refused;
        }
        // プロジェクトロードの asset decode 中は音声がまだ揃って
        // いないので再生を gate して queue する (audio 完了で on_asset_decode_tick
        // が flush)。 r.md #42: gate の根拠は **音が揃っていないこと** なので、
        // 画像 / 動画サムネイルの decode 中 (= GPU 再初期化後の再読込を含む) は
        // 待たせない。
        if self.audio_decode_pending() {
            self.transport.pending_play = true;
            self.transport.pending_play_record = record;
            self.ui_ephemeral.status_message = "プロジェクト読込中...".into();
            return PlayOutcome::Queued;
        }
        // A7: if any plugin is still in the SetSlotPlugin →
        // SlotPluginLoaded round-trip (its `OpenPluginShmem` may not
        // have reached the audio engine yet), queue the Play so every
        // track starts on the same buffer once registration completes.
        // Without this the just-loaded tracks render silent for the
        // first few buffers / first loop.
        if !self.ipc.pending_plugin_loads.is_empty() {
            self.transport.pending_play = true;
            self.transport.pending_play_record = record;
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.ipc.pending_plugin_loads.len()
            );
            return PlayOutcome::Queued;
        }
        // ensure-synced (docs/plan_arch_refactor.md §7.5): 同 frame 内に編集があって
        // まだ frame flush 前なら、 最新 song を Play の前に engine へ届ける。 epoch
        // 未変化なら no-op なので、 定常状態 (= 既に frame flush 済) では LoadSong を
        // 再送せず、 大量 WAV の compile_audio_schedule 同期遅延を踏まない。
        self.flush_song_sync();
        // Pro Tools 流の「Stop で開始位置に戻る」 用に、 実際の再生
        // 開始時の playhead を保存。 ruler クリック等で playhead を
        // 移動してから play した場合は、 その位置が origin になる。
        self.transport.playback_origin_beat = Some(self.transport.playhead_beat.unwrap_or(0.0));
        if let Some(preroll_samples) = record {
            // 録音の開始は Play より先に届ける必要がある (engine は届いた順に
            // 消費するので、逆順だと 1 バッファぶん曲が進んでから count-in / 録音に
            // 入る)。 count-in 無し (`0`) でも必ず送る — engine はこれで
            // 「録音中」を知り、曲末 auto-stop の抑止と `recording_live` の
            // publish を始める。
            self.send_audio(AudioCommand::StartRecording { preroll_samples });
        }
        self.send_audio(AudioCommand::Play);
        // r.md #50: 走り出すたびに積算ラウドネス一式をリセットする
        // (Cubase の "Reset on Start" 相当)。曲を頭から通せば「この曲の
        // ラウドネス」がそのまま出る、という grill-me の決定。
        //
        // r.md #51 で再生も録音もこの 1 箇所を通るようになったので、
        // 録音開始のためにもう 1 箇所リセットを置く必要は無い。
        self.reset_master_loudness();
        // `is_playing` はここでは書かない。engine が走り出したことを `on_tick` が
        // 観測して立てる (r.md #51 — 状態の所有者は engine)。
        PlayOutcome::Started
    }

    /// 読み込み待ちで queue しておいた再生要求を発火する (プラグインロード完了 /
    /// asset decode 完了の 3 経路から呼ぶ唯一の口)。 queue 時に「録音だったか /
    /// count-in が何拍か」を復元するので、録音開始が queue されても録音のまま再開する。
    pub(crate) fn fire_pending_play(&mut self) {
        self.transport.pending_play = false;
        let record = self.transport.pending_play_record.take();
        self.start_transport(record);
    }

    /// プレイヘッドを `beat` に置き、「停止で戻るホーム」 (`playback_origin_beat`)
    /// も同位置へ更新し、audio engine へ SeekTo を送る。 ruler click (arrangement /
    /// piano_roll / audio_editor) と `f` キーから共通で呼ぶ唯一の seek 経路 (= 「停止 =
    /// 最後に意図的に置いた位置に戻る」 の SSoT)。 再生中でも home を更新するので、
    /// 再生中に置き直して停止すると新しい位置へ戻る。 `beat` は呼び出し側で snap 済を渡す。
    pub(crate) fn seek_playhead_to(&mut self, beat: f64) {
        let beat = beat.max(0.0);
        // r.md #10: 明示 seek は Home の 2 段トグルをリセットする (= 次の Home は
        // まず最新クリップ位置へ)。 ここは ruler click / `f` / End / Home 自身が
        // 通る唯一の seek 経路。 goto_timeline_home はこの後で flag を再設定する。
        // (再生中の playhead poll は playhead_beat を直接書くのでここを通らず、
        // flag に触れない = 再生中もトグルが壊れない。)
        self.ui_ephemeral.home_toggle_at_first = false;
        self.transport.playhead_beat = Some(beat as f32);
        self.transport.playback_origin_beat = Some(beat as f32);
        // ensure-synced: 換算は song のテンポカーブを使う。 直前の BPM 編集が
        // 未 flush だと engine の再生グリッドが旧 tempo のままで seek 位置がずれる。
        // epoch 未変化なら no-op。
        self.flush_song_sync();
        // r.md #54: 定数 BPM の線形換算をやめ、engine の sample↔beat 対応と同じ
        // `beats_to_samples` (SongTempo カーブの積分) を通す。定数換算のままだと
        // テンポオートメーションのある曲で「クリックした小節と実際に鳴り始める
        // 位置」がずれ、ラウドネス解析の「最大値の位置へ飛ぶ」も外れる。
        let samples = common::automation::beats_to_samples(
            self.song_doc.song(),
            self.ipc.sample_rate,
            beat,
        );
        self.send_audio(AudioCommand::SeekTo { samples });
    }

    /// `f` キーの実体。 snap 済 song-absolute beat へプレイヘッドを置き
    /// (`seek_playhead_to`: home も更新 + SeekTo)、停止中は `play()` を呼んでその位置から
    /// 再生開始する (play() の export / asset / plugin ゲートと playback_origin_beat capture を
    /// 継承するため body を再実装しない)。 再生中は `play()`/`stop()` を呼ばずシームレスに
    /// 継続する (home は `seek_playhead_to` が更新済なので Stop はこの位置へ戻る)。
    pub(crate) fn action_play_from_cursor(&mut self, beat: f64) {
        self.seek_playhead_to(beat);
        if !self.transport.is_playing {
            self.play();
        }
    }

    /// `Home` キー (r.md #10): 位置導出のトグル。 プレイヘッドが最後 (時間的に
    /// 最新) のクリップ開始位置に「まだ居ない」 なら そこへ、 「既に居る」 なら
    /// 1.1.1 (song 先頭 = beat 0) へ移動する (= 2 度押しで先頭)。 clip が無ければ
    /// 先頭。 transient な押下回数 state を持たず、 現在位置だけで分岐するので
    /// 無効化するものが無い (SSoT)。 `seek_playhead_to` 経由なので停止中/再生中の
    /// どちらでも効き、 停止ホーム (`playback_origin_beat`) も追従する。
    pub(crate) fn goto_timeline_home(&mut self) {
        // 先頭 (時間的に最初) のクリップの頭。 clip が無ければ None。
        let first = common::timing::content_bounds_beats(self.song_doc.song()).map(|(lo, _)| lo);
        // トグルは **live playhead 位置でなく直前の Home 結果**
        // (`home_toggle_at_first`) で判定する。 位置導出だと再生中に playhead が
        // 毎フレーム進んで 2 度押しが成立せず、 長尺では f32/f64 の丸め差が EPS を
        // 超えて先頭へ戻れない (レビュー指摘)。 flag は明示 seek / 停止でのみ
        // リセットされ、 再生中の playhead poll では触らないので、 再生中でも
        // 確実にトグルする。 clip が無ければ常に 1.1.1。
        let go_to_start = self.ui_ephemeral.home_toggle_at_first || first.is_none();
        let target = if go_to_start { 0.0 } else { first.unwrap_or(0.0) };
        // `seek_playhead_to` が flag を false に戻すので、 設定はその後に行う。
        self.seek_playhead_to(target);
        self.ui_ephemeral.home_toggle_at_first = !go_to_start;
        // アレンジを横スクロールして移動先を可視化 (Home は左端寄せ)。
        self.reveal_beat_in_arrange(target, true);
    }

    /// `End` キー (r.md #10): プレイヘッドを content 終端 (最後のクリップの直後 =
    /// 全クリップの `max(start + length)`) へ移動する。 clip が無ければ先頭
    /// (beat 0)。 `seek_playhead_to` 経由なので停止中/再生中どちらでも効く。
    pub(crate) fn goto_timeline_end(&mut self) {
        let target = common::timing::content_bounds_beats(self.song_doc.song())
            .map(|(_, hi)| hi)
            .unwrap_or(0.0);
        self.seek_playhead_to(target);
        // アレンジを横スクロールして終端を可視化 (End は右端寄せ)。
        self.reveal_beat_in_arrange(target, false);
    }

    /// Home/End のシーク後、 アレンジビューを横スクロールして目標拍を可視にする
    /// (r.md #10 user 要望「移動に合わせてスクロールも」)。 `at_start=true` (Home) は
    /// 目標を左端近くに、 `false` (End) は右端近くに置く。 canvas 幅が未確定 (0、
    /// 初回描画前) なら左端寄せにフォールバック。 `arrange_scroll_beat` は tick の
    /// 再生追従と同じ「左端拍」なので、 再生中に follow が ON なら次 tick が上書き
    /// する (= 新しい playhead を追い続ける、 意図どおり)。
    fn reveal_beat_in_arrange(&mut self, beat: f64, at_start: bool) {
        // 端に貼り付けず少し余白を残す。
        const MARGIN_BEATS: f32 = 1.0;
        let beat = beat.max(0.0) as f32;
        let lanes_w = self.ui_ephemeral.last_arrange_lanes_size.0;
        let visible = lanes_w / self.ui_prefs.arrange_zoom_x.max(1.0); // lanes_w 0 → 0
        let scroll = if at_start || visible <= 0.0 {
            beat - MARGIN_BEATS
        } else {
            // End: 目標を右端の少し内側に置き、 手前の content を見せる。
            beat - visible + MARGIN_BEATS
        };
        self.ui_prefs.arrange_scroll_beat = scroll.max(0.0);
    }

    /// A7: register a `device_id` we are about to send `SetSlotPlugin`
    /// for, and — if playback is currently running — pause it until the
    /// last `SlotPluginLoaded` arrives. Without the pause, plugins loaded
    /// while playing render silent until the audio engine's
    /// `OpenPluginShmem` register catches up (typically several buffers
    /// or a loop wrap behind).
    ///
    /// v29: 要求 generation を採番して返す (呼び出し側は `SetSlotPlugin`
    /// に載せる)。 応答 (`SlotPluginLoaded` / `SlotPluginLoadFailed`) は
    /// この generation と一致するものだけ受理される (stale 応答 race guard)。
    pub(crate) fn track_pending_load(&mut self, device_id: u64) -> u64 {
        // r.md #51: **録音中は止めない**。 テイクを切らないことの方が、
        // 足したプラグインが数バッファ遅れて鳴り出すことより重い
        // (REAPER も走行中のトラック arm 追加を明示的に許可している)。
        // 止めてしまうと録音セッションが閉じ、録り直しになる。
        let recording = self.recording.requested;
        if self.ipc.pending_plugin_loads.is_empty() && self.transport.is_playing && !recording {
            self.send_audio(AudioCommand::Stop);
            self.transport.pending_play = true;
        }
        self.ipc.next_plugin_load_generation = self.ipc.next_plugin_load_generation.wrapping_add(1).max(1);
        let generation = self.ipc.next_plugin_load_generation;
        self.ipc.pending_plugin_loads.insert(device_id, generation);
        // 新しい load 要求が in-flight になった時点で、 直近の失敗理由は
        // 「現在の状態」 ではなくなる (インスペクタの「未ロード」表示もここで
        // 消える)。 SetSlotPlugin を送る全経路がこの関数を通るので、 ここが
        // 失敗 entry を落とす唯一の口。
        self.ipc.failed_plugin_loads.remove(&device_id);
        if self.transport.pending_play {
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.ipc.pending_plugin_loads.len()
            );
        }
        generation
    }

    /// 停止を **要求する** 唯一の口。 実際に止まったことの反映
    /// (プレイヘッドを開始位置へ戻す / 録音セッションを閉じる) は、engine が
    /// 止まったのを観測した [`Self::on_transport_stopped`] が行う。
    ///
    /// 録音セッションだけはここでも即座に閉じる。ユーザーが明示的に止めた以上、
    /// 観測が届くまでの数十 ms に鍵盤を叩いたぶんが録音に混ざってはいけない。
    /// クローズは冪等なので二重に呼ばれても害はない。
    pub(crate) fn stop(&mut self) {
        self.send_audio(AudioCommand::Stop);
        self.close_recording_session();
    }

    /// engine が止まったことを観測したときの後始末 (r.md #51)。
    ///
    /// 手動停止・曲末の auto-stop・書き出し・パニック・子プロセスの crash が
    /// **すべてここへ収束する**。「どんな止まり方でも再生を押した位置へ戻る」
    /// (r.md #50 の停止ホーム契約) を 1 箇所で保証するための合流点。
    pub(crate) fn on_transport_stopped(&mut self) {
        // Pro Tools 流: 停止時に playhead を「再生開始位置」 (= 直前の
        // play() 呼び出し時点の playhead) に戻す。 GUI 側 playhead_beat
        // の即時上書きと、 audio engine への SeekTo IPC を 1 セットで
        // 実行する。 後者を送らないと on_tick が直近サンプル位置を返し
        // て GUI 側の戻し操作を打ち消す。 origin が None (= まだ一度も
        // play していない) なら playhead は触らない。
        if let Some(origin) = self.transport.playback_origin_beat.take() {
            self.transport.playhead_beat = Some(origin);
            // ensure-synced: 換算は song のテンポカーブ依存 (seek_playhead_to と同旨)。
            // epoch 未変化なら no-op。
            self.flush_song_sync();
            // r.md #54: `seek_playhead_to` と同じ `beats_to_samples` (SongTempo
            // カーブの積分) を通す。ここだけ定数 BPM のままだと、テンポカーブの
            // ある曲で「停止で戻る位置」と「クリックで飛ぶ位置」が食い違う。
            let samples = common::automation::beats_to_samples(
                self.song_doc.song(),
                self.ipc.sample_rate,
                f64::from(origin),
            );
            self.send_audio(AudioCommand::SeekTo { samples });
        }
        // 録音は transport に乗るモードなので、止まったら必ず閉じる
        // (旧実装は stop() が録音フラグに触れず、停止後も Rec が点灯したまま
        // 凍ったプレイヘッドへノートが積み上がっていた)。
        self.close_recording_session();
        // Phase 4 Step C: recording session を transport stop でクローズ。
        // Latch / Write の latched set + per-param 直近 record 位置を全て
        // clear。 これで次の Play 時には latched / last_beat が空からスタート、
        // touching しない limit 何も record されない (Touch / Latch / Write 共通)。
        self.recording.latched_param_gestures.clear();
        self.recording.recording_last_beat.clear();
        // Phase 4 Step C-2: audio thread の recording bypass を解除 +
        // 最新 song を送る (= curve eval に戻る瞬間に正しい point sequence
        // が反映される)。 currently_recording_lanes は !is_playing なので
        // 必ず empty に解決する。
        self.sync_recording_lanes_with_audio();
    }

    /// パニック — 鳴っている全ての音を即座に止める。
    ///
    /// 1. 再生中なら [`Self::stop`] で transport を止める（sequencer note-off を
    ///    flush、audio clip / metronome を停止、playhead を開始位置へ戻す）。
    /// 2. 全 plugin を `ReinitAllPlugins`（deactivate→activate）で再初期化し、
    ///    note-off を無視する音源（VCV Rack 2 の hold voice）/ reverb tail /
    ///    鍵盤プレビューの stuck note / 自己発振まで確実に黙らせる。WAV 書き出し
    ///    開始時のクリーンリセットと同じ機構をそのまま流用する（ユーザー要望）。
    ///
    /// 書き出し中（offline render / 映像）は freewheel を壊さないよう no-op。
    /// reinit は fire-and-forget — 返信の `PluginsReinitDone` は pending_export が
    /// 無いので handler 側で無視される。
    ///
    /// クリック対策: `ReinitAllPlugins` は全 plugin を audio engine の mix から
    /// 一瞬で外すので、 master がフル音量のまま外すと段差クリック（「ビープ」）に
    /// なる。 そこで:
    /// 1. まず engine に `Panic` を送って master を declick フェードアウト →
    ///    **ミュート保持** させる。
    /// 2. reinit を [`PANIC_REINIT_DELAY`] だけ遅延させ、 plugin の detach が
    ///    master ミュート後に起きるようにする（`on_tick` が遅延 reinit を発火）。
    /// 3. reinit 完了通知 `PluginsReinitDone` を受けたら engine に `PanicRelease`
    ///    を送り、 master をフェードインで戻す（`panic_release_pending`）。
    ///
    /// ミュート解除を固定タイマーでなく**実際の reinit 完了**に結びつけることで、
    /// GUI メインスレッド stall や巨大 reinit でも、 plugin が mix に残ったまま
    /// master が戻る（クリック / reverb tail 復活）ことを防ぐ。engine 側にも
    /// plugin-host hang 用の安全 auto-release がある。
    pub(crate) fn panic(&mut self) {
        if self.transport.pending_video_export.is_some() || self.transport.export_stage.is_some() {
            return;
        }
        // r.md #51: `is_playing` で条件を付けない。 これは観測値なので、押した
        // 瞬間にはまだ true になっていない (= 録音を始めた直後のパニック) ことが
        // あり、そこで Stop を送らないと「全ての音を停止しました」が嘘になる。
        // stop() は要求 + 録音セッションのクローズだけで冪等。
        self.stop();
        // モニターで鳴らしている held は reinit が黙らせるので、こちらは
        // 台帳だけ畳む (残すと次の note-off で存在しない音を止めにいく)。
        self.recording.monitor_notes.clear();
        self.send_audio(AudioCommand::Panic);
        self.transport.panic_reinit_due = Some(std::time::Instant::now());
        self.transport.panic_release_pending = true;
        self.ui_ephemeral.status_message = "パニック: 全ての音を停止しました".into();
    }

    /// 再生ループ (ON/OFF + 範囲) を更新する **唯一の口**。 session state を書き、
    /// 同じ値を `AudioCommand::SetLoop` で engine へ送る (ON/OFF と範囲を別経路に
    /// しない = SSoT)。 ループは `Song` に属さないので `edit_song()` を通さず、
    /// 従って undo にも dirty (`*`) にも影響しない — ズーム / スクロールと同じ
    /// 「聴き方の都合」 (`common::model::LoopRegion`)。 保存は `ViewState` 経由。
    pub(crate) fn set_loop_region(&mut self, region: common::model::LoopRegion) {
        let mut region = region;
        region.sanitize();
        if !region.has_range() {
            // 範囲は「未定義」 の正規形 (0/0) に畳む。 engine 側の
            // `effective_loop_bounds` はこれを見て曲全体へフォールバックする。
            region.start_beat = 0.0;
            region.end_beat = 0.0;
        }
        self.transport.loop_region = region;
        self.send_audio(AudioCommand::SetLoop(region));
    }

    pub(crate) fn toggle_loop(&mut self) {
        let region = common::model::LoopRegion {
            enabled: !self.transport.loop_region.enabled,
            ..self.transport.loop_region
        };
        self.set_loop_region(region);
    }

    pub(crate) fn set_loop_range(&mut self, start: f64, end: f64) {
        let region = common::model::LoopRegion {
            start_beat: start,
            end_beat: end,
            ..self.transport.loop_region
        };
        self.set_loop_region(region);
    }

    /// `R` キー: 選択素材の bounding range (= 最小 `start_beat` 〜 最大
    /// `start_beat + length_beats`) を loop 範囲に設定し、 loop ON + 再生開始。
    /// 既に loop ON かつ現在の loop 範囲が同じ bounding range と一致するなら
    /// loop を OFF にする (再生は維持)。
    ///
    /// 対象面 (`automation`) は root の `edit_surface` arbiter が解決した結果。
    /// その面の選択素材の bounding span を `arrange_selection_beat_span` で取る。
    /// 解決できなければ no-op。
    pub(crate) fn loop_selected_clip_toggle(&mut self, automation: bool) {
        let Some((start, end)) = self.arrange_selection_beat_span(automation) else {
            return;
        };

        const EPS: f64 = 1e-9;
        let current = self.transport.loop_region;
        let same_range = (current.start_beat - start).abs() < EPS
            && (current.end_beat - end).abs() < EPS;

        if current.enabled && same_range {
            self.set_loop_region(common::model::LoopRegion { enabled: false, ..current });
            return;
        }

        self.set_loop_region(common::model::LoopRegion {
            enabled: true,
            start_beat: start,
            end_beat: end,
        });
        if !self.transport.is_playing {
            self.play();
        }
    }

}
