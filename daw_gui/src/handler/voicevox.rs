//! handler::voicevox — VOICEVOX 歌唱/トーク status + vocal metadata + lipsync
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::model::Clip;
use common::plugin_format::PluginFormat;
use common::protocol::{PluginCommand, VocalSynthFailure};

/// 2 つの口 ImageEvent 列が「実質同一」か (source_id は厳密一致、 時間/幾何は
/// 1e-6 許容)。 idempotency 判定で exact `==` を使うと、 load 時に
/// `(clip.start + ev.start) - r0` が float 非結合性で元の `ev.start` と bit 単位で
/// 一致せず、 無変更のはずのプロジェクトが開いただけで dirty 化してしまう
/// (r.md #9 違反)。 rebuild が書く field (source_id / 時間 / rect) のみ比較すれば
/// 足りる (他 field は生成時に常に `ImageEvent::default()`)。
fn mouth_events_equivalent(
    a: &[common::model::ImageEvent],
    b: &[common::model::ImageEvent],
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.source_id == y.source_id
                && (x.event_start_in_clip_beats - y.event_start_in_clip_beats).abs() <= 1e-6
                && (x.event_length_beats - y.event_length_beats).abs() <= 1e-6
                && (x.x - y.x).abs() <= 1e-6
                && (x.y - y.y).abs() <= 1e-6
                && (x.w - y.w).abs() <= 1e-6
                && (x.h - y.h).abs() <= 1e-6
        })
}

/// `spans` (song-absolute の口画像区間) の全体の広がり `(min start, max end)`。
/// 空なら `None`。
fn open_span_extent(spans: &[(f64, f64, u32)]) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &(s, e, _) in spans {
        lo = lo.min(s);
        hi = hi.max(e);
    }
    (hi > lo).then_some((lo, hi))
}

/// 口 track (`mouth_track_id`) の `auto_lipsync` clip 群を、 与えられた非重複な
/// 口画像区間 `open` (song-absolute、 start 昇順) と立ち絵 body 範囲から、
/// **閉じ口で隙間を埋めた単一の連続 auto_lipsync clip** に再構築する。
///
/// - r.md #17: 常に「高々 1 本」に畳むので `auto_lipsync` clip が重なり得ない。
/// - r.md #18: `fill_mouth_timeline` が歌/セリフの無い区間 (open の隙間 + 立ち絵
///   範囲の余白) を閉じ口で埋めるので、 立ち絵が映っている間は口が消えない。
///
/// 目標形が現状 (ちょうど 1 本の同一 clip) と一致するなら **何もせず `false`**
/// を返す (idempotent → load 時の collapse や無変更再生成で '*' を付けない)。
/// 実際に clip 集合が変わったら `true`。 `mouth_map` 未設定なら生成不能なので、
/// 残っている auto clip を掃除するだけ (あれば `true`)。
fn rebuild_mouth_clip(
    song: &mut common::model::Song,
    mouth_track_id: u32,
    open: Vec<(f64, f64, u32)>,
) -> bool {
    let Some(m_idx) = song.tracks.iter().position(|t| t.id == mouth_track_id) else {
        return false;
    };
    // 閉じ口 image。 mouth_map 未設定 or Closed 未割当なら 0 (= 埋めない)。
    let closed_id = song.tracks[m_idx]
        .mouth_map
        .as_ref()
        .map_or(0, |m| m.resolve(common::model::MouthShape::Closed));

    // 充填範囲: 立ち絵 body が映る範囲 (閉じ口を敷く) ∪ open 区間の広がり。
    // 閉じ口が未割当 (closed_id == 0) のときは body へ広げても埋められないので open のみ。
    let body = if closed_id != 0 {
        common::lipsync::tachie_body_range(song, mouth_track_id)
    } else {
        None
    };
    let fill_range = match (body, open_span_extent(&open)) {
        (Some(b), Some(o)) => Some((b.0.min(o.0), b.1.max(o.1))),
        (Some(b), None) => Some(b),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    };

    // 目標イベント列 (clip-local) を組む。
    let res = song.video_resolution;
    let target: Option<(f64, f64, Vec<common::model::ImageEvent>)> =
        fill_range.and_then(|(r0, r1)| {
            let filled = common::lipsync::fill_mouth_timeline(&open, (r0, r1), closed_id);
            if filled.is_empty() {
                return None;
            }
            let events: Vec<common::model::ImageEvent> = filled
                .iter()
                .map(|&(s, e, img)| {
                    let mut ev = common::model::ImageEvent {
                        source_id: img,
                        event_start_in_clip_beats: s - r0,
                        event_length_beats: e - s,
                        ..common::model::ImageEvent::default()
                    };
                    // build_mouth_events は rect を全画面 default で返すので、 素材寸法から
                    // aspect-fit rect を計算して上書き (立ち絵の他の子レイヤーと収まりを揃える)。
                    if let Some(src) = song.media.image_sources.get(&img) {
                        let (x, y, w, h) = aspect_fit_pip_rect(res, (src.width, src.height));
                        ev.x = x;
                        ev.y = y;
                        ev.w = w;
                        ev.h = h;
                    }
                    ev
                })
                .collect();
            Some((r0, r1 - r0, events))
        });

    // idempotency: 既に「ちょうど 1 本の auto clip」で目標と一致するなら触らない。
    let auto_positions: Vec<usize> = song.tracks[m_idx]
        .clips
        .iter()
        .enumerate()
        .filter(|(_, c)| c.auto_lipsync)
        .map(|(i, _)| i)
        .collect();
    match &target {
        None => {
            // 目標 = clip 無し。 既存 auto があれば削除、 無ければ no-op。
            if auto_positions.is_empty() {
                return false;
            }
            song.tracks[m_idx].clips.retain(|c| !c.auto_lipsync);
            song.gc_clip_contents();
            true
        }
        Some((new_start, new_len, new_events)) => {
            if auto_positions.len() == 1 {
                let c = &song.tracks[m_idx].clips[auto_positions[0]];
                // 許容付き比較 (r.md #9: load 時の float 再構成差で無変更を dirty 化しない)。
                let same_geom = (c.start_beat - new_start).abs() <= 1e-6
                    && (c.length_beats - new_len).abs() <= 1e-6;
                let same_events = song
                    .clip_contents
                    .get(&c.content_id)
                    .and_then(|cc| cc.image_events())
                    .is_some_and(|ev| mouth_events_equivalent(ev, new_events));
                if same_geom && same_events {
                    return false;
                }
            }
            // 置換: 既存 auto を全削除 → 単一 clip を追加。
            song.tracks[m_idx].clips.retain(|c| !c.auto_lipsync);
            let content_id = song.alloc_content(
                common::model::ClipContent::Image(common::model::ImageContent {
                    events: new_events.clone(),
                }),
                "口パク".to_string(),
            );
            let m = &mut song.tracks[m_idx];
            m.place_clip(Clip {
                id: 0,
                start_beat: *new_start,
                length_beats: *new_len,
                content_id,
                color: None,
                auto_lipsync: true,
                // 生成した配置ルールの世代を焼き込む (r.md #39)。load 時に
                // 古い世代を見つけたら一度だけ再生成する。
                lipsync_gen: common::lipsync::PLACEMENT_GEN,
                ..Default::default()
            });
            song.gc_clip_contents();
            true
        }
    }
}

/// 1 clip の notes を builtin VOICEVOX 向けの [`NoteMetadata`] へ変換する。
///
/// `note_id` は **安定 id** (アーキ不変条件 1): `(clip.id, note.id)` から
/// [`common::plugin_metadata::sing_note_id`] で決定論的に導出する。daw_audio の
/// sequencer が **同じ関数**で同じ値を作るので、「クリップ先頭に 1 音足すと以降の
/// 全 note_id がずれる」が起きない。
///
/// `start_beat` は content-local → song-absolute (r.md #44: note は content 原点基準)。
/// `clip_id` は `note_id` の導出元であり、合成進捗のクリップ帰属にも使う (r.md #75)。
/// `speaker_id` は per-clip 歌唱声 (`0` = builtin 側で `DEFAULT_SINGER_ID` へフォールバック)。
fn collect_sing_metadata(
    song: &common::model::Song,
    clip: &Clip,
) -> Vec<common::plugin_metadata::NoteMetadata> {
    let notes: &[common::model::Note] = song
        .clip_contents
        .get(&clip.content_id)
        .and_then(|c| c.notes())
        .unwrap_or(&[]);
    notes
        .iter()
        .map(|n| common::plugin_metadata::NoteMetadata {
            note_id: common::plugin_metadata::sing_note_id(clip.id, n.id),
            start_beat: clip.content_to_song_beat(n.start_beat),
            duration_beats: n.duration_beats,
            pitch: n.pitch,
            velocity: n.velocity,
            lyric: n.lyric.clone().unwrap_or_default(),
            clip_id: clip.id,
            speaker_id: clip.speaker_id,
        })
        .collect()
}

/// (talk) 1 clip の `ClipContent::Text` を [`TalkMetadata`] へ変換する
/// (`docs/plan_voicevox_talk.md` §3.2)。
///
/// `event_id` は `talk_event_id(clip.id, event_index)` で決定論的に導出する
/// (sequencer の talk-trigger と同式)。**空テキストは両側で skip** して event_id の
/// 対応を保つ。声は per-clip (`clip.speaker_id` を talk style として解釈)、
/// スケールは `clip.talk`。`clip_id` は合成進捗のクリップ帰属 (r.md #75) —
/// これが無いと Text クリップにスピナーが一切点かない。
fn collect_talk_metadata(
    song: &common::model::Song,
    clip: &Clip,
) -> Vec<common::plugin_metadata::TalkMetadata> {
    let Some(events) = song
        .clip_contents
        .get(&clip.content_id)
        .and_then(|c| c.text_events())
    else {
        return Vec::new();
    };
    let scales = clip.talk.unwrap_or_default();
    events
        .iter()
        .enumerate()
        .filter(|(_, ev)| !ev.text.is_empty())
        .map(|(event_index, ev)| common::plugin_metadata::TalkMetadata {
            event_id: common::plugin_metadata::talk_event_id(clip.id, event_index as u32),
            start_beat: clip.content_to_song_beat(ev.event_start_in_clip_beats),
            text: ev.text.clone(),
            speaker_id: clip.speaker_id,
            speed_scale: scales.speed_scale,
            pitch_scale: scales.pitch_scale,
            intonation_scale: scales.intonation_scale,
            volume_scale: scales.volume_scale,
            clip_id: clip.id,
        })
        .collect()
}

impl AppData {
    // ===== VOICEVOX 生成状態の可視化 ==========================

    /// `VoicevoxSynthStatus` IPC handler。per-plugin の busy + 失敗種別 + 進捗を更新する。
    /// `Unreachable` の立上りで `failing_since` を記録 (継続中は維持) して 5s 後に
    /// 「engine 未接続」へ、`Rejected` は `rejected` に理由を積んで即時「合成できない歌詞」
    /// 表示へ。成功 (`None`) で両方クリア。idle かつ失敗なしの entry は掃除して
    /// `voicevox_any_generating` 等の判定を軽く保つ (= 進捗もクリアされる)。
    pub(crate) fn apply_voicevox_synth_status(
        &mut self,
        device_id: u64,
        progress: common::protocol::VocalSynthProgress,
    ) {
        let now = std::time::Instant::now();
        let failure = progress.failure.clone();
        let entry = self
            .voicevox
            .voicevox_synth_status
            .entry(device_id)
            .or_default();
        entry.progress = progress;
        match failure {
            VocalSynthFailure::None => {
                entry.failing_since = None;
                entry.rejected = None;
            }
            VocalSynthFailure::Unreachable => {
                entry.failing_since.get_or_insert(now);
                entry.rejected = None;
            }
            VocalSynthFailure::Rejected { detail } => {
                entry.failing_since = None;
                entry.rejected = Some(detail);
            }
        }
        if !entry.progress.busy && entry.failing_since.is_none() && entry.rejected.is_none() {
            self.voicevox.voicevox_synth_status.remove(&device_id);
        }
    }

    /// engine には到達できているが歌詞等を拒否した「内容エラー」の理由 (あれば最初の 1 件)。
    /// overlay が「合成できない歌詞があります」表示に使う。
    pub fn voicevox_rejected_detail(&self) -> Option<&str> {
        self.voicevox
            .voicevox_synth_status
            .values()
            .find_map(|s| s.rejected.as_deref())
    }

    /// track が持つ builtin VOICEVOX device の安定 device id (= load 済なら)。
    /// `sync_vocal_metadata` の lookup と同じ (device 実在 → `loaded_devices` で
    /// load 確認)。
    pub fn voicevox_plugin_id_for_track(&self, track: &common::model::Track) -> Option<u64> {
        track
            .devices
            .iter()
            .find(|d| {
                d.format == PluginFormat::Builtin
                    && d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
            })
            .map(|d| d.id)
            .filter(|id| self.ipc.loaded_devices.contains_key(id))
    }

    /// track の歌唱/読み上げ WAV 合成が進行中か (= 所属 builtin VOICEVOX が busy)。
    pub fn track_wav_synthesizing(&self, track_id: u32) -> bool {
        let Some(track) = self.song_doc.song().tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pid) = self.voicevox_plugin_id_for_track(track) else {
            return false;
        };
        self.voicevox.voicevox_synth_status.get(&pid).is_some_and(|s| s.progress.busy)
    }

    /// このクリップに **未完了フレーズが掛かっているか** (= クリップ上スピナーの点灯条件)。
    /// 「トラックが busy」ではないので、1 ノート直しただけで同トラックの全クリップが
    /// 回ることがなくなる (r.md #75)。
    pub fn clip_wav_synthesizing(&self, track_id: u32, clip_id: u32) -> bool {
        let Some(track) = self.song_doc.song().tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pid) = self.voicevox_plugin_id_for_track(track) else {
            return false;
        };
        self.voicevox
            .voicevox_synth_status
            .get(&pid)
            .is_some_and(|s| s.progress.pending_clips.binary_search(&clip_id).is_ok())
    }

    /// 曲中の builtin VOICEVOX device の安定 id (load 済のものだけ、track 順)。
    /// 書き出し前の合成完了ゲートが「誰を待つか」を決めるのに使う。
    pub(crate) fn all_vocal_synth_device_ids(&self) -> Vec<u64> {
        self.song_doc
            .song()
            .tracks
            .iter()
            .filter(|t| t.is_voicevox_vocal())
            .filter_map(|t| self.voicevox_plugin_id_for_track(t))
            .collect()
    }

    /// 出力先 (口 track) が口パク再生成中か。
    pub fn lipsync_target_generating(&self, track_id: u32) -> bool {
        self.voicevox.lipsync_inflight.contains(&track_id)
    }

    /// いずれかの VOICEVOX 生成 (WAV 合成 / 口パク) が進行中か。
    pub fn voicevox_any_generating(&self) -> bool {
        !self.voicevox.lipsync_inflight.is_empty()
            || self.voicevox.voicevox_synth_status.values().any(|s| s.progress.busy)
    }

    /// 合成待ちのフレーズ総数 (= 全体オーバーレイの「残り N フレーズ」)。
    /// r.md #75 で合成の最小単位がフレーズになったので、旧
    /// `voicevox_synth_busy_count` (= busy な track 数) は意味を失った。
    pub fn voicevox_pending_phrase_count(&self) -> u32 {
        self.voicevox
            .voicevox_synth_status
            .values()
            .map(|s| s.progress.pending)
            .sum()
    }

    /// r.md #75: 合成の塊の長さ (秒) を設定する。
    ///
    /// ドラッグ中 (`commit = false`) は表示値だけ動かし、確定 (release / 数値入力 /
    /// ダブルクリックリセット) でだけ保存と再合成を行う。**これは合成結果を変える入力**
    /// なので、確定時は差分キャッシュを落として全 device へ流し直す (= 曲全体の再合成)。
    /// 落とさないと再送されず、フレーズ WAV のキャッシュキーに混ぜてある以上
    /// 「設定が効かない」ように見える。
    pub(crate) fn set_voicevox_chunk_secs(&mut self, secs: f32, commit: bool) {
        let next = secs.clamp(
            common::voicevox_phrase::MIN_CHUNK_SECS,
            common::voicevox_phrase::MAX_CHUNK_SECS,
        );
        if (next - self.ui_prefs.voicevox_chunk_secs).abs() >= 1e-3 {
            self.ui_prefs.voicevox_chunk_secs = next;
        }
        if !commit {
            return;
        }
        self.persist_app_config();
        self.voicevox.voicevox_metadata_sent.clear();
        self.sync_vocal_metadata();
    }

    /// 別プロジェクトへ切り替えるときに捨てる VOICEVOX の in-flight / 送信記憶。
    ///
    /// device_id は project ごとに再割当されるので、stale entry が誤 hit すると
    /// 新 project の vocal device に seed 合成が飛ばず無音になる。書き出しの
    /// 合成完了ゲートも同様に畳む (前の曲の device を待ち続けない)。
    pub(crate) fn reset_voicevox_sync_state(&mut self) {
        self.voicevox.voicevox_metadata_sent.clear();
        self.voicevox.priority_sent.clear();
        self.ipc.pending_vocal_synth_export.clear();
    }

    /// アプリ設定の「合成の塊の長さ」(秒) を有効範囲へクランプして返す。
    /// SSoT は `app_config.json` の `voicevox_chunk_secs`。
    pub(crate) fn voicevox_chunk_secs(&self) -> f32 {
        self.ui_prefs.voicevox_chunk_secs.clamp(
            common::voicevox_phrase::MIN_CHUNK_SECS,
            common::voicevox_phrase::MAX_CHUNK_SECS,
        )
    }

    /// engine 未接続警告を出すべきか (= busy のまま failing が閾値以上継続)。
    /// engine boot (数秒) の間は failing でも警告せず「合成中」に見せ、閾値超過で切り替える。
    pub fn voicevox_engine_unreachable(&self, now: std::time::Instant) -> bool {
        self.voicevox.voicevox_synth_status.values().any(|s| {
            s.progress.busy
                && s.failing_since
                    .is_some_and(|t| now.duration_since(t) >= VOICEVOX_ENGINE_WARNING)
        })
    }

    /// スピナーを回し続ける (= 連続再描画を要求する) べきか。engine 未接続が
    /// 確定したら static 警告にして再描画を止める (CPU spin させない)。
    pub fn voicevox_animating(&self, now: std::time::Instant) -> bool {
        self.voicevox_any_generating() && !self.voicevox_engine_unreachable(now)
    }

    /// PR-V3: track.source = Vocal で instrument に builtin VOICEVOX が
    /// load されている全 track の clip notes を `NoteMetadata` 配列に
    /// 変換し、 plugin host に `SetBuiltinPluginNoteMetadata` で送る。
    /// plugin_id 未確定 (= load 完了通知前) の track はスキップ、
    /// `SlotPluginLoadedFromChild` 受信時に再呼び出しされる。
    ///
    /// PR-V4 follow-up: vocal track が 1 つでも存在するなら VOICEVOX
    /// engine を lazy spawn する。 旧 `begin_vocal_synth` 内にあった
    /// 起動 logic を移植 (= localhost:50021 が起動済でなければ自動で
    /// spawn、 builtin plugin の HTTP synth を成功させる前提)。
    pub fn sync_vocal_metadata(&mut self) {
        let bpm = self.song_doc.song().bpm;
        // r.md #75: 塊 (= `/sing_frame_audio_query` 1 回) の長さ。アプリ設定が SSoT。
        let chunk_secs = self.voicevox_chunk_secs();
        let has_vocal_track = self.song_doc.song().tracks.iter().any(|t| t.is_voicevox_vocal());
        if has_vocal_track {
            self.ensure_voicevox_engine();
        }
        for track in &self.song_doc.song().tracks {
            if !track.is_voicevox_vocal() {
                continue;
            }
            // builtin VOICEVOX を chain 内に持つ device の安定 id
            // (`loaded_devices` に居ること = load 完了確認込み)。
            let Some(host_plugin_id) = self.voicevox_plugin_id_for_track(track) else {
                continue;
            };

            let song = self.song_doc.song();
            // 全 clip の notes / TextEvent を metadata 配列へ flatten する
            // (組み立ての規則は下の 2 つの純粋関数が持つ)。
            let entries: Vec<common::plugin_metadata::NoteMetadata> = track
                .clips
                .iter()
                .flat_map(|clip| collect_sing_metadata(song, clip))
                .collect();
            let talk: Vec<common::plugin_metadata::TalkMetadata> = track
                .clips
                .iter()
                .flat_map(|clip| collect_talk_metadata(song, clip))
                .collect();
            // r.md #27: この device の歌唱/読み上げ入力 (bpm/notes/talk) が前回送信から
            // 変わっていなければ再送しない (= builtin plugin が不要な再合成を走らせない)。
            // `sync_vocal_metadata` は epoch bump のたびに呼ばれるので、Transform 等の
            // 非 vocal 編集ではここが cache hit → send skip になる。`sync_ara_documents`
            // の差分キャッシュと同 idiom。
            // r.md #75: `chunk_secs` は **合成結果を変える入力**なので比較に含める
            // (含めないと設定つまみを動かしても再送されない)。**playhead はここに
            // 入れない** — 入れるとトランスポートのたびに再合成になる (順序ヒントは
            // 専用の `SetVocalSynthPriority`)。
            if self
                .voicevox
                .voicevox_metadata_sent
                .get(&host_plugin_id)
                .is_some_and(|(b, c, e, t)| {
                    *b == bpm && *c == chunk_secs && *e == entries && *t == talk
                })
            {
                continue;
            }
            self.voicevox.voicevox_metadata_sent.insert(
                host_plugin_id,
                (bpm, chunk_secs, entries.clone(), talk.clone()),
            );
            self.send_plugin(PluginCommand::SetBuiltinPluginNoteMetadata {
                device_id: host_plugin_id,
                bpm,
                chunk_secs,
                entries,
                talk,
            });
        }
    }

    /// 合成中の builtin VOICEVOX へ再生ヘッド位置を送る (r.md #75)。前回送信から
    /// **1 拍以上動いたときだけ**送るので、トランスポート中でも IPC は数 Hz 以下に収まる。
    /// busy な device が 1 つも無ければ何もしない。
    ///
    /// **再合成はトリガしない** (`SetVocalSynthPriority` は順序ヒント専用)。停止 / seek でも
    /// `playhead_beat` が動くので、同じ経路で届く。
    pub(crate) fn send_vocal_synth_priority_if_moved(&mut self) {
        if self.voicevox.voicevox_synth_status.is_empty() {
            return;
        }
        let Some(playhead) = self.transport.playhead_beat.map(f64::from) else {
            return;
        };
        let busy: Vec<u64> = self
            .voicevox
            .voicevox_synth_status
            .iter()
            .filter(|(_, s)| s.progress.busy)
            .map(|(id, _)| *id)
            .collect();
        for device_id in busy {
            let moved = self
                .voicevox
                .priority_sent
                .get(&device_id)
                .is_none_or(|prev| (playhead - prev).abs() >= 1.0);
            if !moved {
                continue;
            }
            self.voicevox.priority_sent.insert(device_id, playhead);
            self.send_plugin(PluginCommand::SetVocalSynthPriority {
                device_id,
                playhead_beats: playhead,
            });
        }
    }

    /// `target` (口 track) の口パク出力を決定する入力すべての 64-bit fingerprint。
    /// `regenerate_lipsync_for_track` が phoneme query / `build_mouth_events` に渡す
    /// 入力と **厳密に一致** させる (= ここに含めた値が変わったときだけ再生成が要る):
    ///
    /// - song の `bpm`
    /// - `target` の `mouth_map` (7 slot の `ImageSourceId`)
    /// - `target` を出力先にする全ソーストラックの **並び順 (priority)** と、各 clip の
    ///   `start_beat` / `length_beats`、および
    ///     - sing clip: notes の `start_beat` / `duration_beats` / `pitch` / `lyric`
    ///       (= `build_sing_query` が読むフィールド。`velocity` / `muted` は phoneme へ
    ///       影響しないので **含めない**)
    ///     - talk clip: 先頭の非空 `TextEvent` の `text` / `event_start_in_clip_beats`、
    ///       および clip の `speaker_id` / `talk.speed_scale` (= `query_talk_phonemes` が
    ///       phoneme 長に使う値。pitch / intonation / volume は無関係なので含めない)
    ///
    /// track 名 / 色 / mute / volume / plugin 等の **非入力** は含めないので、それらの
    /// 編集では fingerprint が不変 → `LipsyncDebounceFired` が再生成をスキップする。
    /// 走査順は下の `regenerate_lipsync_for_track` の snap 収集ループと一致させること。
    pub(crate) fn lipsync_input_fingerprint(song: &common::model::Song, target_id: u32) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        song.bpm.to_bits().hash(&mut h);
        if let Some(t) = song.tracks.iter().find(|t| t.id == target_id)
            && let Some(m) = &t.mouth_map
        {
            [m.a, m.i, m.u, m.e, m.o, m.n, m.closed].hash(&mut h);
        }
        // r.md #18: 閉じ口の充填範囲は立ち絵 body clip の範囲に、 各 event の
        // aspect-fit rect は video 解像度に依存する。 これらを fingerprint に含めて、
        // body の resize/move や解像度変更で口パクが再生成されるようにする (かつ
        // load 時の再構成と同一入力になり dirty-on-open を防ぐ、 code review)。
        if let Some((lo, hi)) = common::lipsync::tachie_body_range(song, target_id) {
            lo.to_bits().hash(&mut h);
            hi.to_bits().hash(&mut h);
        }
        song.video_resolution.0.hash(&mut h);
        song.video_resolution.1.hash(&mut h);
        for (idx, src) in song.tracks.iter().enumerate() {
            if src.lipsync_target_track != Some(target_id) {
                continue;
            }
            (idx as u32).hash(&mut h); // priority (= トラック並び順)
            // snap を生成する clip (notes 有り / 非空 text 有り) だけが出力に効く。
            // `regenerate_lipsync_for_track` の収集と同条件で、その clip の位置と
            // 内容のみをハッシュする (= 非対象 clip の移動では fingerprint 不変)。
            for clip in &src.clips {
                let content = song.clip_contents.get(&clip.content_id);
                if let Some(notes) = content
                    .and_then(|c| c.notes())
                    .filter(|n| common::voicevox::sing_base_beat(n).is_some())
                {
                    // sing: clip 位置 + build_sing_query が読む note フィールドのみ。
                    // r.md #44: 窓 (offset) も配置に効くので fingerprint に含める
                    // (含めないと端 trim しても口パクが再生成されない)。
                    clip.start_beat.to_bits().hash(&mut h);
                    clip.length_beats.to_bits().hash(&mut h);
                    clip.content_offset_beats.to_bits().hash(&mut h);
                    for n in notes {
                        n.start_beat.to_bits().hash(&mut h);
                        n.duration_beats.to_bits().hash(&mut h);
                        n.pitch.hash(&mut h);
                        n.lyric.hash(&mut h);
                    }
                } else if let Some(ev) = content
                    .and_then(|c| c.text_events())
                    .and_then(|events| events.iter().find(|e| !e.text.is_empty()))
                {
                    // talk: clip 位置 + 先頭の非空 TextEvent + 声 + 話速のみ。
                    clip.start_beat.to_bits().hash(&mut h);
                    clip.length_beats.to_bits().hash(&mut h);
                    clip.content_offset_beats.to_bits().hash(&mut h);
                    ev.text.hash(&mut h);
                    ev.event_start_in_clip_beats.to_bits().hash(&mut h);
                    clip.speaker_id.hash(&mut h);
                    clip.talk
                        .unwrap_or_default()
                        .speed_scale
                        .to_bits()
                        .hash(&mut h);
                }
            }
        }
        h.finish()
    }

    /// 全 target (口) track の現在の入力 fingerprint を記録する。load 直後に呼び、
    /// 保存済み口パク clip を生成した入力をベースライン化することで、開いた直後の
    /// 非入力編集 (track rename 等) で口パクが再生成されないようにする。
    pub(crate) fn seed_lipsync_fingerprints(&mut self) {
        self.voicevox.lipsync_fingerprints.clear();
        let mut targets: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter_map(|t| t.lipsync_target_track)
            .collect();
        targets.sort_unstable();
        targets.dedup();
        for target in targets {
            let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target);
            self.voicevox.lipsync_fingerprints.insert(target, fp);
        }
    }

    /// load / recovery 時に呼ぶ (r.md #17/#18): 既存プロジェクトの口 track を
    /// 「高々 1 本の連続 auto_lipsync clip・隙間は閉じ口」 という現行の不変条件へ
    /// **決定論的に畳み直す** (VOICEVOX 再合成は不要 — 既存 image event を並べ直す
    /// だけ)。 旧バージョンで生成した重なり合う複数 auto clip (#17) や、 隙間で口が
    /// 消える clip (#18) がここで正規化される。 既に目標形なら `rebuild_mouth_clip`
    /// が no-op を返し `normalize_song_checked` が epoch を進めないので、 正しく
    /// 保存済みのプロジェクトを開いても '*' は付かない (r.md #9 の contract を守る)。
    /// `seed_lipsync_fingerprints` の後に呼ぶ (baseline 確定後なので、 実際に畳んだ
    /// legacy プロジェクトだけが dirty = 要再保存になる)。
    pub(crate) fn normalize_lipsync_clips_on_load(&mut self) {
        let mut mouth_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter_map(|t| t.lipsync_target_track)
            .collect();
        mouth_ids.sort_unstable();
        mouth_ids.dedup();
        for mouth_id in mouth_ids {
            self.normalize_song_checked(|song| {
                let Some(m_idx) = song.tracks.iter().position(|t| t.id == mouth_id) else {
                    return false;
                };
                let closed_id = song.tracks[m_idx]
                    .mouth_map
                    .as_ref()
                    .map_or(0, |m| m.resolve(common::model::MouthShape::Closed));
                // 既存 auto clip の **開き口** event を song-absolute span へ展開
                // (priority = clip 順、 旧 per-clip 生成で重なっていても上位優先で畳む)。
                // 閉じ口は fill が敷き直すので merge に載せない (live 経路と対称、 code review)。
                let mut spans: Vec<(f64, f64, u32, u32)> = Vec::new();
                for (i, clip) in song.tracks[m_idx].clips.iter().enumerate() {
                    if !clip.auto_lipsync {
                        continue;
                    }
                    let Some(events) = song
                        .clip_contents
                        .get(&clip.content_id)
                        .and_then(|c| c.image_events())
                    else {
                        continue;
                    };
                    // clip は content への「窓」なので、 窓の外に隠れている event は
                    // 見えていない = 存在しないものとして畳む。 clamp を忘れると、
                    // 端を trim した口 clip を開くたびに隠れ event が復活して目標形が
                    // 現状と食い違い、 **開くだけで毎回 '*'** が付く (r.md #9)。
                    let win_lo = clip.start_beat;
                    let win_hi = clip.start_beat + clip.length_beats;
                    for ev in events {
                        if closed_id != 0 && ev.source_id == closed_id {
                            continue;
                        }
                        let s = clip
                            .content_to_song_beat(ev.event_start_in_clip_beats)
                            .max(win_lo);
                        let e = (clip.content_to_song_beat(ev.event_start_in_clip_beats)
                            + ev.event_length_beats)
                            .min(win_hi);
                        if e > s {
                            spans.push((s, e, ev.source_id, i as u32));
                        }
                    }
                }
                // 開き口が無くても、 既存 auto clip があれば rebuild を通して閉じ口だけの
                // 単一 clip へ畳む (旧 clip が全部閉じ口だった場合も 1 本に正規化)。
                let has_auto = song.tracks[m_idx].clips.iter().any(|c| c.auto_lipsync);
                if spans.is_empty() && !has_auto {
                    return false;
                }
                // 重なりを解決した非重複区間 (既存 open/closed 両方を含む)。 これを
                // rebuild に渡すと、 残った隙間 (旧 clip 間) と立ち絵余白が閉じ口で
                // 埋められ、 全体が 1 本の連続 clip に畳まれる。
                let spans = merge_lipsync_events_by_priority(spans);
                rebuild_mouth_clip(song, mouth_id, spans)
            });
        }
    }

    /// load 時に呼ぶ (r.md #39): 保存済みの `auto_lipsync` clip が **古い配置ルール**
    /// ([`common::lipsync::PLACEMENT_GEN`]) で作られていたら、そのソース vocal track の
    /// 口パクを一度だけ再生成する。
    ///
    /// 口パク event は project に永続化される派生データで、通常の再生成トリガは
    /// `lipsync_input_fingerprint` の差分だけ。配置ルールを変えても入力は変わらないので、
    /// この経路が無いと旧タイミング (talk なら音声より ~100ms 遅れ) が温存され、
    /// 「直したのに変わらない」になる。合成 WAV 側の `CACHE_SCHEMA_VERSION` と対の仕組み。
    ///
    /// r.md #9 の dirty-on-open contract とは「**実際に古い世代のときだけ** dirty になる」
    /// 形で両立する (現行世代の clip しか無い project では何もしない)。engine 未起動等で
    /// phoneme query が失敗した場合は既存 clip が温存され、世代も古いままなので次回 open で
    /// 再試行される。
    pub(crate) fn regenerate_outdated_lipsync_on_load(&mut self) {
        let vocal_ids =
            common::lipsync::vocal_tracks_with_outdated_lipsync(self.song_doc.song());
        for vid in vocal_ids {
            tracing::info!(
                vocal_track_id = vid,
                placement_gen = common::lipsync::PLACEMENT_GEN,
                "口パク配置ルールが更新されているため再生成する (r.md #39)"
            );
            self.regenerate_lipsync_for_track(vid);
        }
    }

    /// 口パク (lip-sync) を再生成する (docs/plan_pakupaku.md §7)。`vocal_track_id`
    /// の各 clip の notes を snapshot し、背景スレッドで `query_phonemes`
    /// (`sing_frame_audio_query` のみ) を叩いて結果を `AppEvent::LipsyncGenerated`
    /// で main thread へ返す。binding (`lipsync_target_track`) 未設定 / 口 track の
    /// `mouth_map` 未設定 / notes を持つ clip 無し のときは no-op。歌唱のみ (Q6)。
    pub fn regenerate_lipsync_for_track(&mut self, vocal_track_id: u32) {
        let Some(target_id) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == vocal_track_id)
            .and_then(|t| t.lipsync_target_track)
        else {
            return;
        };
        // この target の現在の入力 fingerprint をベースラインとして記録する。
        // `LipsyncDebounceFired` はこの値と現在値を比べ、変化した target だけ
        // 再生成する (= rename 等の非入力編集では再生成しない)。直接呼び出し
        // (binding / mouth_map 変更) はここで必ず最新値へ更新されるので、直後の
        // debounce 発火は fingerprint 一致で二重再生成にならない。
        let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target_id);
        self.voicevox.lipsync_fingerprints.insert(target_id, fp);
        // 口 track が存在し mouth_map が設定済みか (= 生成する意味があるか)。
        let configured = self.song_doc.song().tracks.iter().any(|t| {
            t.id == target_id && t.mouth_map.as_ref().is_some_and(|m| m.is_configured())
        });
        if !configured {
            return;
        }
        let bpm = self.song_doc.song().bpm;
        // (talk) target 中心: 出力先が `target_id` の **全ソーストラック** をまとめて
        // 再生成する (`docs/plan_voicevox_talk.md`)。トラック並び順 index を priority に
        // し、apply 側で重なりを上位優先で解決する。各 clip は notes (歌唱) があれば sing、
        // 無く Text なら talk として扱う。
        let mut snaps: Vec<(f64, f64, f64, u32, Vec<common::model::Note>)> = Vec::new();
        let mut talk_snaps: Vec<(f64, f64, f64, u32, String, u32, common::model::TalkParams)> =
            Vec::new();
        for (idx, src) in self.song_doc.song().tracks.iter().enumerate() {
            if src.lipsync_target_track != Some(target_id) {
                continue;
            }
            let priority = idx as u32;
            for clip in &src.clips {
                // sing: notes を持つ clip。phoneme 列 frame 0 は「基準ノートの
                // `REST_FRAMES` 手前」に来る (= 合成 wav 先頭と同じ位置、r.md #39)。
                if let Some(notes) = self
                    .song_doc.song()
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.notes())
                    && let Some(base) = common::voicevox::sing_base_beat(notes)
                {
                    // r.md #44: phoneme は content-local 起点なので原点基準で置き、
                    // 長さの上限は窓の末尾 (content-local) にする。
                    snaps.push((
                        clip.content_origin_beat(),
                        clip.content_offset_beats + clip.length_beats,
                        common::voicevox::sing_head_beat(base, bpm),
                        priority,
                        notes.to_vec(),
                    ));
                    continue;
                }
                // talk: Text clip の先頭の非空 TextEvent。phoneme 列 frame 0 = wav 先頭 =
                // 「発話開始の pre-silence 分手前」(現行 pre-silence は 0 なので event 開始)。
                if let Some(events) = self
                    .song_doc.song()
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.text_events())
                    && let Some(ev) = events.iter().find(|e| !e.text.is_empty())
                {
                    let scales = clip.talk.unwrap_or_default();
                    let pre_beats = common::voicevox::frames_to_beats(
                        f64::from(common::voicevox::talk_pre_silence_frames(
                            scales.speed_scale,
                        )),
                        bpm,
                    );
                    talk_snaps.push((
                        clip.content_origin_beat(),
                        clip.content_offset_beats + clip.length_beats,
                        ev.event_start_in_clip_beats - pre_beats,
                        priority,
                        ev.text.clone(),
                        clip.speaker_id,
                        scales,
                    ));
                }
            }
        }
        if snaps.is_empty() && talk_snaps.is_empty() {
            // r.md #18: 開き口ソースが 1 つも無くても、 立ち絵が映っている間は口を
            // 消さない。 HTTP は不要 (phoneme が無い) ので、 立ち絵範囲を閉じ口だけで
            // 埋めた単一 clip を同期適用する (body / closed 未設定なら rebuild が
            // 既存 auto clip を掃除するだけ)。
            self.normalize_song_checked(|song| rebuild_mouth_clip(song, target_id, Vec::new()));
            return;
        }
        self.ensure_voicevox_engine();
        // 出力先 (口 track) を in-flight に登録 = クリップ上スピナー +
        // 全体オーバーレイ「口パク生成中」を点灯。完了イベントで必ず外す。
        self.voicevox.lipsync_inflight.insert(target_id);
        // spawn 時点の世代を snapshot し、 結果と一緒に返す。 HTTP が遅延して
        // いる間に project が切り替わる (reset_saved_baseline が gen を bump) と
        // handler 側で破棄される。
        let generation = self.voicevox.lipsync_gen;
        let target_track_id = target_id;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            let mut clips = Vec::with_capacity(snaps.len() + talk_snaps.len());
            for (clip_start_beat, clip_len_beats, first_phoneme_local_beat, priority, notes) in snaps {
                match crate::voicevox_client::query_phonemes(&notes, bpm) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_phoneme_local_beat,
                        priority,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "lip-sync phoneme query failed");
                    }
                }
            }
            // (talk) 読み上げ phoneme を `query_talk_phonemes` で取り、同じ
            // `LipsyncClipResult` に詰める (= apply 経路は歌唱と共通)。
            for (clip_start_beat, clip_len_beats, first_phoneme_local_beat, priority, text, speaker_id, scales) in
                talk_snaps
            {
                match crate::voicevox_client::query_talk_phonemes(&text, speaker_id, &scales) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_phoneme_local_beat,
                        priority,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "talk lip-sync phoneme query failed");
                    }
                }
            }
            // 成功/失敗/空に関わらず **必ず** 送る。handler が
            // `lipsync_inflight` から target を外してスピナーを止める。clips が空なら
            // (= 全 HTTP 失敗) handler 側で「既存 clip を温存」して反映だけスキップする。
            proxy.send(AppEvent::LipsyncGenerated {
                vocal_track_id,
                target_track_id,
                bpm,
                clips,
                generation,
            });
        });
    }

    /// `AppEvent::LipsyncGenerated` handler。口 track の自動生成 clip
    /// (`auto_lipsync == true`) を全て差し替える。派生データなので Undo
    /// snapshot は積まない (user 編集側が Undo 対象、手編集は保持しない = Q8)。
    pub(crate) fn apply_lipsync_generated(
        &mut self,
        vocal_track_id: u32,
        bpm: f32,
        results: Vec<LipsyncClipResult>,
    ) {
        // 口パクは派生データの自動再生成なので undo 履歴に入れない
        // (normalize_song)。 epoch は進む = dirty + frame flush の再 sync は走る。
        // normalize_song 経由で export_lock を同期する: export (freewheel /
        // video render) 中に口パク再生成が完了しても render 中の LoadSong を
        // 送らない (H2)。 `_checked` 版で、 生成結果が既存 clip と同一なら epoch を
        // 進めない (= 再生成トリガのたびに無変更で '*' が付くのを防ぐ)。
        self.normalize_song_checked(|song| {
            // HTTP 中に song が変わっている可能性があるため id ベースで再解決。
            let Some(target_id) = song
                .tracks
                .iter()
                .find(|t| t.id == vocal_track_id)
                .and_then(|t| t.lipsync_target_track)
            else {
                return false;
            };
            let Some(mouth_map) = song.track_by_id(target_id).and_then(|t| t.mouth_map.clone())
            else {
                return false;
            };
            let closed_id = mouth_map.resolve(common::model::MouthShape::Closed);
            // (talk) 全ソースの **開き口** event を song-absolute (start, end, image_id,
            // priority) に展開する。複数ソース (歌唱 Vox / 読み上げ Talk) が同じ口 track を
            // 共有しても、次の merge で重なりが上位 (priority 小 = 上のトラック) 優先で解決
            // される (`docs/plan_voicevox_talk.md`)。 閉じ口 padding は merge に載せない
            // ——上位ソースの閉じ口が下位ソースの開き口を潰すのを防ぐため。 隙間の閉じ口は
            // rebuild 内の fill が立ち絵範囲で一括して敷き直す (code review)。
            let mut spans: Vec<(f64, f64, u32, u32)> = Vec::new();
            for r in &results {
                let events = common::lipsync::build_mouth_events(
                    &r.phonemes,
                    &mouth_map,
                    bpm,
                    r.first_phoneme_local_beat,
                    r.clip_len_beats,
                );
                for ev in events {
                    if closed_id != 0 && ev.source_id == closed_id {
                        continue; // 閉じ口は fill が敷くので merge から除外
                    }
                    let s = r.clip_start_beat + ev.event_start_in_clip_beats;
                    let e = s + ev.event_length_beats;
                    if e > s {
                        spans.push((s, e, ev.source_id, r.priority));
                    }
                }
            }
            // 上位優先で重なりを解決した非重複な開き口区間。 これを立ち絵範囲まで
            // 閉じ口で埋めて **1 本の連続 auto_lipsync clip** に畳む (r.md #17/#18)。
            let open = merge_lipsync_events_by_priority(spans);
            rebuild_mouth_clip(song, target_id, open)
        });
    }

    /// `SetLipsyncTarget` handler。vocal track の出力先 binding を更新し、
    /// 設定時は口パクを再生成する (snapshot は `is_undoable` 経由で handler
    /// 前に取得済み = binding 変更を undo 可能)。
    pub(crate) fn set_lipsync_target(&mut self, track_id: u32, target: Option<u32>) {
        // 変更前の出力先を控える (retarget / unbind で旧 track の掃除に使う)。
        let old_target = self
            .song_doc
            .song()
            .track_by_id(track_id)
            .and_then(|t| t.lipsync_target_track);
        let applied = self.edit_song(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            if t.lipsync_target_track == target {
                return false; // 無変更 → no-op (dirty 化しない)
            }
            t.lipsync_target_track = target;
            true
        }) == Some(true);
        if !applied {
            return;
        }
        // r.md #17 (retarget leak): 出力先を変更/解除したら、 旧口 track に残った
        // 生成済み auto clip を掃除する — ただし他の vocal がまだ旧 track を出力先に
        // していなければ (共有していれば温存)。 applied なので old != target が確定。
        if let Some(old) = old_target
            && !self
                .song_doc
                .song()
                .tracks
                .iter()
                .any(|t| t.lipsync_target_track == Some(old))
        {
            self.normalize_song_checked(|song| {
                let mut removed = false;
                if let Some(m) = song.tracks.iter_mut().find(|t| t.id == old) {
                    let before = m.clips.len();
                    m.clips.retain(|c| !c.auto_lipsync);
                    removed = m.clips.len() != before;
                }
                if removed {
                    song.gc_clip_contents();
                }
                removed
            });
        }
        if target.is_some() {
            self.regenerate_lipsync_for_track(track_id);
        }
    }

    /// `SetMouthMapSlot` handler。口 track の `mouth_map` の 1 slot を更新し、
    /// この口 track を出力先にしている vocal track の口パクを再生成する。
    pub(crate) fn set_mouth_map_slot(
        &mut self,
        track_id: u32,
        shape: common::model::MouthShape,
        source_id: common::model::ImageSourceId,
    ) {
        let applied = self.edit_song(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            let map = t
                .mouth_map
                .get_or_insert_with(common::model::MouthMap::default);
            match shape {
                common::model::MouthShape::A => map.a = source_id,
                common::model::MouthShape::I => map.i = source_id,
                common::model::MouthShape::U => map.u = source_id,
                common::model::MouthShape::E => map.e = source_id,
                common::model::MouthShape::O => map.o = source_id,
                common::model::MouthShape::N => map.n = source_id,
                common::model::MouthShape::Closed => map.closed = source_id,
            }
            true
        }) == Some(true);
        if !applied {
            return;
        }
        // この口 track を出力先にしている vocal track を再生成する。
        let vocal_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|v| v.lipsync_target_track == Some(track_id))
            .map(|v| v.id)
            .collect();
        for vid in vocal_ids {
            self.regenerate_lipsync_for_track(vid);
        }
    }

    /// song 変更時に呼ぶ (= `flush_song_sync` から)。binding を持つ
    /// vocal track があれば debounce timer を立て、quiet period (400ms) 後に
    /// `LipsyncDebounceFired` を送る。rapid 編集 (歌詞タイプ等) は世代カウンタで
    /// coalesce され、最後の 1 回だけ再生成される。
    /// 進行中 (debounce 待ち) の口パク自動再生成を無効化する。 generation
    /// counter を bump するだけで、 既にスケジュール済みの `LipsyncDebounceFired`
    /// は世代不一致になり handler 側で no-op になる (新しい timer は spawn しない)。
    /// `reset_saved_baseline` (= load / new / recovery) から呼び、 開いた直後の
    /// spurious dirty を防ぐ。
    pub(crate) fn cancel_pending_lipsync_regen(&mut self) {
        self.voicevox.lipsync_gen = self.voicevox.lipsync_gen.wrapping_add(1);
    }

    pub(crate) fn mark_lipsync_dirty(&mut self) {
        if !self
            .song_doc.song()
            .tracks
            .iter()
            .any(|t| t.lipsync_target_track.is_some())
        {
            return;
        }
        self.voicevox.lipsync_gen = self.voicevox.lipsync_gen.wrapping_add(1);
        let generation = self.voicevox.lipsync_gen;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            proxy.send(AppEvent::LipsyncDebounceFired(generation));
        });
    }

}

#[cfg(test)]
mod rebuild_mouth_clip_tests {
    //! r.md #17/#18: `rebuild_mouth_clip` が口 track を「高々 1 本の連続
    //! auto_lipsync clip・隙間は閉じ口」に畳むことの回帰テスト。
    use super::rebuild_mouth_clip;
    use common::model::{Clip, ClipContent, ImageContent, MouthMap, Song, Track};

    fn image_content(song: &mut Song) -> u32 {
        let cid = song.alloc_content_id();
        song.clip_contents
            .insert(cid, ClipContent::Image(ImageContent { events: vec![] }));
        cid
    }

    /// group G(1) + body 立ち絵 track(2, [0,8)) + 口 track(3, closed=99)。
    fn tachie_song(closed: u32) -> Song {
        let mut song = Song::default();
        let body_cid = image_content(&mut song);
        song.tracks = vec![
            Track { id: 1, ..Default::default() },
            Track {
                id: 2,
                parent_group_id: Some(1),
                clips: vec![Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: body_cid,
                    ..Default::default()
                }],
                ..Default::default()
            },
            Track {
                id: 3,
                parent_group_id: Some(1),
                mouth_map: Some(MouthMap { closed, ..Default::default() }),
                ..Default::default()
            },
        ];
        song
    }

    /// 口 track 上の (start, end, source_id) を返す (auto clip の events)。
    fn mouth_triples(song: &Song) -> Vec<(f64, f64, u32)> {
        let m = song.track_by_id(3).unwrap();
        assert_eq!(m.clips.len(), 1, "auto_lipsync clip はちょうど 1 本 (r.md #17)");
        let clip = &m.clips[0];
        assert!(clip.auto_lipsync);
        song.clip_contents
            .get(&clip.content_id)
            .unwrap()
            .image_events()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    clip.content_to_song_beat(e.event_start_in_clip_beats),
                    clip.content_to_song_beat(e.event_start_in_clip_beats) + e.event_length_beats,
                    e.source_id,
                )
            })
            .collect()
    }

    #[test]
    fn single_open_span_is_closed_filled_over_tachie_range() {
        // 開き口 [2,4) img5 のみ。 立ち絵 [0,8) の残りは閉じ口(99)で埋まり、
        // 全体が 1 本の連続 clip [0,8) になる (r.md #18 option1)。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        assert_eq!(
            mouth_triples(&song),
            vec![(0.0, 2.0, 99), (2.0, 4.0, 5), (4.0, 8.0, 99)],
        );
    }

    #[test]
    fn rebuild_stamps_the_current_placement_generation() {
        // r.md #39: 再生成した clip には現行の配置ルール世代を焼き込む。これで
        // 「古い世代を見つけたら load 時に一度だけ作り直す」検出が終端する
        // (焼き込みを忘れると毎回 open のたびに再生成 = '*' が付き続ける)。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        let clip = &song.track_by_id(3).unwrap().clips[0];
        assert!(clip.auto_lipsync);
        assert_eq!(clip.lipsync_gen, common::lipsync::PLACEMENT_GEN);
        // 世代が現行なので、もう再生成対象にならない。
        assert!(common::lipsync::vocal_tracks_with_outdated_lipsync(&song).is_empty());
    }

    #[test]
    fn rebuild_is_idempotent() {
        // 同じ入力での再構築は clip を作り直さず false を返す (load collapse /
        // 無変更再生成で '*' を付けない = r.md #9 の contract)。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        let before = mouth_triples(&song);
        assert!(
            !rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]),
            "同一入力の再構築は no-op"
        );
        assert_eq!(mouth_triples(&song), before);
    }

    #[test]
    fn closed_unassigned_leaves_gaps_and_no_tachie_extend() {
        // 閉じ口 未割当 (closed=0) → 隙間を埋めず、 立ち絵範囲へも広げない。
        // clip は open span [2,4) だけ (従来どおり隙間は口なし)。
        let mut song = tachie_song(0);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        assert_eq!(mouth_triples(&song), vec![(2.0, 4.0, 5)]);
    }

    #[test]
    fn overlapping_legacy_span_collapses_to_single_clip() {
        // 旧 per-clip 生成を模し、 複数 auto clip が既にある状態から呼んでも
        // 1 本に畳まれる (呼び出し側が merge 済みの非重複 span を渡す前提)。
        let mut song = tachie_song(99);
        // 既存の重複 auto clip を 2 本仕込む。
        let c1 = image_content(&mut song);
        let c2 = image_content(&mut song);
        let m_idx = song.tracks.iter().position(|t| t.id == 3).unwrap();
        song.tracks[m_idx].clips = vec![
            Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: c1, auto_lipsync: true, ..Default::default() },
            Clip { id: 2, start_beat: 2.0, length_beats: 4.0, content_id: c2, auto_lipsync: true, ..Default::default() },
        ];
        // 非重複 open span を渡して再構築 → 1 本に。
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(1.0, 3.0, 5)]));
        assert_eq!(song.track_by_id(3).unwrap().clips.len(), 1);
        assert_eq!(
            mouth_triples(&song),
            vec![(0.0, 1.0, 99), (1.0, 3.0, 5), (3.0, 8.0, 99)],
        );
    }

    #[test]
    fn empty_open_fills_whole_body_with_closed() {
        // r.md #18: 開き口が 1 つも無くても、 立ち絵範囲を閉じ口 1 本で覆う。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, Vec::new()));
        assert_eq!(mouth_triples(&song), vec![(0.0, 8.0, 99)]);
        // 同じ状態なら no-op。
        assert!(!rebuild_mouth_clip(&mut song, 3, Vec::new()));
    }

    #[test]
    fn idempotent_after_load_reconstruction_with_fractional_beats() {
        // r.md #9 回帰: body が非整数 beat 始まり + fractional な open だと、 load の
        // span 再構成 `clip.start + ev.start` が float 非結合性で元の値と bit 単位で
        // ズレる。 exact `==` だと無変更でも rebuild → dirty 化していたが、 許容比較
        // (mouth_events_equivalent) で「無変更」と判定して epoch を進めない。
        let mut song = Song::default();
        let body_cid = image_content(&mut song);
        song.tracks = vec![
            Track { id: 1, ..Default::default() },
            Track {
                id: 2,
                parent_group_id: Some(1),
                clips: vec![Clip {
                    id: 1,
                    start_beat: 16.5,
                    length_beats: 8.0,
                    content_id: body_cid,
                    ..Default::default()
                }],
                ..Default::default()
            },
            Track {
                id: 3,
                parent_group_id: Some(1),
                mouth_map: Some(MouthMap { closed: 99, a: 5, ..Default::default() }),
                ..Default::default()
            },
        ];
        // fractional な open 区間で最初の生成。
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(16.5 + 2.3333333, 16.5 + 3.1666667, 5)]));
        // load 相当: mouth clip の **開き口** event を (clip.start + ev.start) で再構成
        // (= normalize_lipsync_clips_on_load と同じ、 closed は除外)。
        let recon: Vec<(f64, f64, u32)> = {
            let m = song.track_by_id(3).unwrap();
            let clip = &m.clips[0];
            song.clip_contents
                .get(&clip.content_id)
                .unwrap()
                .image_events()
                .unwrap()
                .iter()
                .filter(|e| e.source_id != 99)
                .map(|e| {
                    let s = clip.content_to_song_beat(e.event_start_in_clip_beats);
                    (s, s + e.event_length_beats, e.source_id)
                })
                .collect()
        };
        assert!(
            !rebuild_mouth_clip(&mut song, 3, recon),
            "load 再構成後の rebuild は no-op でなければならない (r.md #9)"
        );
    }
}
