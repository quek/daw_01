//! handler::glue — 選択範囲の Glue (結合)
//!
//! **audio は焼き込み** (選択範囲を 1 本の WAV へ offline render して 1 clip / 1 event に
//! 置換)、それ以外の kind は非破壊 merge。設計正本は
//! [`docs/plan_glue_bake.md`](../../../docs/plan_glue_bake.md)。
use crate::state::*;
use crate::app_types::*;
use common::model::{
    AudioContent, Clip, ClipContent, ClipKey, LaneRef, MidiContent, Note, Song, TimeSelection,
};
use std::collections::BTreeMap;

/// `J` (Glue) の焼き込み 1 トラック分 (`docs/plan_glue_bake.md` §4)。
/// 完了通知で `frames` が埋まり、全 job が揃った時点でまとめて song へ適用する。
#[derive(Debug, Clone)]
pub struct GlueBakeJob {
    /// 焼き込み対象のトラック (安定 id)。IPC echo の照合にも使う。
    pub track_id: u32,
    pub out_path: std::path::PathBuf,
    pub source_path: common::model::AudioSourcePath,
    /// 新 content の名前 (= 範囲の最も早いクリップの名前)。
    pub name: String,
    /// render 済み frames (完了通知で埋まる)。
    pub frames: u64,
}

/// `J` (Glue) の焼き込み待ち。
///
/// render 中に選択が動いても適用先は動かさないので、実行時の選択範囲を退避して持つ。
/// トラックは **1 本ずつ順に** 焼く (engine の offline render は同時 1 本)。
#[derive(Debug, Clone)]
pub struct PendingGlueBake {
    /// `J` を押した時点の選択範囲 (= 結合範囲)。
    pub sel: TimeSelection,
    pub jobs: Vec<GlueBakeJob>,
    /// 実行中 job の index。
    pub current: usize,
}

/// Glue の対象種別 (= 1 トラック内で 1 つに畳める content variant)。
/// 混在は結合できない (`Automation` は `Track.clips` に居ないので候補に無い)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GlueKind {
    Midi,
    Audio,
    Video,
    Image,
    Text,
}

/// r.md #44 の audio 版と対になる video 版 (source 軸が micro 秒)。
/// clip の内容窓 `[win_start, win_end)` (content-local 拍) で event を切り出す。
///
/// Glue は複数 clip を **1 つの新しい content へ焼き込む**破壊的操作なので、
/// 「鳴っている範囲」をそのまま新 event として作り直す必要がある (窓は clip 側に
/// 残せない)。頭を落とす分は現在の長さ比で source を進める線形近似。
fn crop_video_event(
    ev: &common::model::VideoEvent,
    win_start: f64,
    win_end: f64,
) -> Option<common::model::VideoEvent> {
    let e0 = ev.event_start_in_clip_beats;
    let e1 = e0 + ev.event_length_beats;
    let c0 = e0.max(win_start);
    let c1 = e1.min(win_end);
    if c1 <= c0 {
        return None;
    }
    let mut out = ev.clone();
    let span = ev.source_end_micros.saturating_sub(ev.source_start_micros);
    if c0 > e0 && ev.event_length_beats > 1e-9 && span > 0 {
        let frac = (c0 - e0) / ev.event_length_beats;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let advance = (span as f64 * frac).max(0.0) as u64;
        out.source_start_micros = ev
            .source_start_micros
            .saturating_add(advance)
            .min(ev.source_end_micros);
        out.fade_in_beats = (ev.fade_in_beats - (c0 - e0)).max(0.0);
    }
    if c1 < e1 && ev.event_length_beats > 1e-9 && span > 0 {
        let kept = (c1 - c0) / ev.event_length_beats;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let keep_micros = (span as f64 * kept).max(0.0) as u64;
        out.source_end_micros = out
            .source_start_micros
            .saturating_add(keep_micros)
            .min(ev.source_end_micros);
        out.fade_out_beats = (ev.fade_out_beats - (e1 - c1)).max(0.0);
    }
    out.event_start_in_clip_beats = c0;
    out.event_length_beats = c1 - c0;
    Some(out)
}

/// 非 audio kind の結合素材 (窓で切り出して combined-local 拍へ寄せ終えたもの)。
#[derive(Default)]
struct Fragments {
    midi_notes: Vec<Note>,
    video_events: Vec<common::model::VideoEvent>,
    image_events: Vec<common::model::ImageEvent>,
    text_events: Vec<common::model::TextEvent>,
}

/// clip 1 つの kind。`Track.clips` から辿れない variant は `None`。
fn clip_glue_kind(song: &Song, key: ClipKey) -> Option<GlueKind> {
    let clip = song.clip_by_key(key)?;
    match song.clip_contents.get(&clip.content_id)? {
        ClipContent::Midi(_) => Some(GlueKind::Midi),
        ClipContent::Audio(_) => Some(GlueKind::Audio),
        ClipContent::Video(_) => Some(GlueKind::Video),
        ClipContent::Image(_) => Some(GlueKind::Image),
        ClipContent::Text(_) => Some(GlueKind::Text),
        // Automation clips don't live in `Track.clips` so a stale link here is
        // unreachable, but be defensive and treat it as "結合できない"。
        ClipContent::Automation(_) => None,
    }
}

/// refs の kind を 1 つに畳む。混在 / 未知が混ざれば `None` (= そのトラックは skip)。
fn glue_kind_of(song: &Song, refs: &[ClipKey]) -> Option<GlueKind> {
    let mut kind: Option<GlueKind> = None;
    for r in refs {
        let this = clip_glue_kind(song, *r)?;
        match kind {
            None => kind = Some(this),
            Some(prev) if prev != this => return None,
            _ => {}
        }
    }
    kind
}

/// 非 audio kind の素材集め。
///
/// r.md #44: clip は content への「窓」なので、
/// (a) content-local 拍 → combined-local 拍の換算は clip 開始ではなく **content 原点**
///     (`content_origin_beat`) 基準、
/// (b) 窓の外の note / event は **鳴っていない**ので glue にも含めない。
/// これで左端 trim 済み clip を glue しても、隠れていた中身が復活しない。
fn collect_fragments(song: &Song, refs: &[ClipKey], combined_start: f64) -> Fragments {
    let mut frags = Fragments::default();
    for r in refs {
        let Some(clip) = song.clip_by_key(*r) else {
            continue;
        };
        let Some(content) = song.clip_contents.get(&clip.content_id) else {
            continue;
        };
        let (win_start, win_end) = clip.content_window();
        let shift = clip.content_origin_beat() - combined_start;
        match content {
            ClipContent::Midi(midi) => {
                for note in &midi.notes {
                    // sequencer と同じ gate: 発音開始が窓内の note だけ、
                    // 長さは窓末尾で clamp (= 実際に鳴っている姿)。
                    if note.start_beat < win_start || note.start_beat >= win_end {
                        continue;
                    }
                    let dur = note.duration_beats.min(win_end - note.start_beat).max(0.0);
                    frags.midi_notes.push(Note {
                        start_beat: note.start_beat + shift,
                        duration_beats: dur,
                        ..note.clone()
                    });
                }
            }
            ClipContent::Video(video) => {
                for ev in &video.events {
                    let Some(mut cropped) = crop_video_event(ev, win_start, win_end) else {
                        continue;
                    };
                    cropped.event_start_in_clip_beats += shift;
                    frags.video_events.push(cropped);
                }
            }
            // Image / Text は時間軸 source を持たないので、窓との交差で表示区間を切るだけ。
            ClipContent::Image(image) => {
                for ev in &image.events {
                    let e0 = ev.event_start_in_clip_beats.max(win_start);
                    let e1 = (ev.event_start_in_clip_beats + ev.event_length_beats).min(win_end);
                    if e1 <= e0 {
                        continue;
                    }
                    frags.image_events.push(common::model::ImageEvent {
                        event_start_in_clip_beats: e0 + shift,
                        event_length_beats: e1 - e0,
                        ..ev.clone()
                    });
                }
            }
            ClipContent::Text(text) => {
                for ev in &text.events {
                    let e0 = ev.event_start_in_clip_beats.max(win_start);
                    let e1 = (ev.event_start_in_clip_beats + ev.event_length_beats).min(win_end);
                    if e1 <= e0 {
                        continue;
                    }
                    frags.text_events.push(common::model::TextEvent {
                        event_start_in_clip_beats: e0 + shift,
                        event_length_beats: e1 - e0,
                        ..ev.clone()
                    });
                }
            }
            // Audio は焼き込み (`place_baked_glue_clip`) が担うのでここには来ない。
            ClipContent::Audio(_) | ClipContent::Automation(_) => {}
        }
    }
    frags
}

/// 非 audio kind の結合 content を組む。
fn merged_content(kind: GlueKind, frags: Fragments) -> ClipContent {
    match kind {
        GlueKind::Video => ClipContent::Video(common::model::VideoContent {
            events: frags.video_events,
        }),
        GlueKind::Image => ClipContent::Image(common::model::ImageContent {
            events: frags.image_events,
        }),
        GlueKind::Text => ClipContent::Text(common::model::TextContent {
            events: frags.text_events,
        }),
        GlueKind::Midi | GlueKind::Audio => {
            let mut notes = frags.midi_notes;
            notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
            // glue で別 clip 由来の同一ピッチ note が時間的に重なり得るので、
            // 全 note を勝者として重なりを解消する。
            let all: Vec<u32> = (0..notes.len() as u32).collect();
            resolve_note_overlaps(&mut notes, &all);
            // v29: merged content では note id を振り直す (per-content unique が不変条件)。
            for (i, n) in notes.iter_mut().enumerate() {
                n.id = i as u32 + 1;
            }
            ClipContent::Midi(MidiContent {
                next_note_id: notes.len() as u32 + 1,
                notes,
            })
        }
    }
}

impl AppData {
    /// 選択範囲 × レーンを結合する (`J`)。
    ///
    /// audio のトラックは**焼き込み** — offline render の完了を待ってから
    /// [`Self::apply_glue`] が 1 回の編集で適用する (= 1 undo step、失敗時は無変更)。
    /// audio が 1 つも無い選択は render を挟まず同期で終わる。
    pub(crate) fn action_glue_selected_clips(&mut self) {
        let Some(sel) = self.selection.time.clone() else {
            self.ui_ephemeral.status_message = "Glue: 範囲を選択してください".to_string();
            return;
        };
        if self.ipc.pending_glue_bake.is_some() || self.ipc.pending_clip_fx_bounce.is_some() {
            self.ui_ephemeral.status_message =
                "Glue: 焼き込み中です。 完了をお待ちください".into();
            return;
        }
        let refs_by_track = self.glue_refs_by_track(&sel, false);
        if refs_by_track.is_empty() {
            tracing::warn!("Glue: 範囲内にクリップが無い");
            self.ui_ephemeral.status_message = "Glue: 範囲の中にクリップがありません".into();
            return;
        }
        let audio_tracks: Vec<u32> = refs_by_track
            .iter()
            .filter(|(_, refs)| glue_kind_of(self.song_doc.song(), refs) == Some(GlueKind::Audio))
            .map(|(id, _)| *id)
            .collect();
        if audio_tracks.is_empty() {
            self.apply_glue(&sel, &BTreeMap::new());
            return;
        }
        self.start_glue_bake(sel, &audio_tracks);
    }

    /// 選択範囲 × レーンに掛かるクリップをトラック別 (開始拍順) に集める。
    /// `fully_inside` なら範囲に**完全に入る**ものだけ (= 境界 split 済み前提)。
    fn glue_refs_by_track(
        &self,
        sel: &TimeSelection,
        fully_inside: bool,
    ) -> BTreeMap<u32, Vec<ClipKey>> {
        let song = self.song_doc.song();
        let mut out: BTreeMap<u32, Vec<(f64, ClipKey)>> = BTreeMap::new();
        for lane in &sel.lanes {
            let LaneRef::Track(track_id) = lane else {
                continue;
            };
            let Some(track) = song.track_by_id(*track_id) else {
                continue;
            };
            for clip in &track.clips {
                let hit = if fully_inside {
                    sel.contains_span(clip.start_beat, clip.length_beats)
                } else {
                    sel.intersects(clip.start_beat, clip.length_beats)
                };
                if hit {
                    out.entry(*track_id).or_default().push((
                        clip.start_beat,
                        ClipKey { track_id: *track_id, clip_id: clip.id },
                    ));
                }
            }
        }
        out.into_iter()
            .map(|(track_id, mut v)| {
                v.sort_by(|a, b| a.0.total_cmp(&b.0));
                (track_id, v.into_iter().map(|(_, k)| k).collect())
            })
            .collect()
    }

    /// audio トラックの焼き込みキューを組んで 1 本目の render を撃つ。
    fn start_glue_bake(&mut self, sel: TimeSelection, tracks: &[u32]) {
        let refs_by_track = self.glue_refs_by_track(&sel, false);
        let mut jobs: Vec<GlueBakeJob> = Vec::with_capacity(tracks.len());
        for &track_id in tracks {
            // 新 content の名前は範囲の最も早いクリップから借りる (従来の merge と同じ)。
            let name = refs_by_track
                .get(&track_id)
                .and_then(|refs| refs.first().copied())
                .and_then(|r| self.song_doc.song().clip_by_key(r))
                .map(|c| self.song_doc.song().content_name(c.content_id).to_string())
                .unwrap_or_default();
            let Some((out_path, source_path)) = self.bounce_output_path(&name, "_glue") else {
                return;
            };
            jobs.push(GlueBakeJob {
                track_id,
                out_path,
                source_path,
                name,
                frames: 0,
            });
        }
        if jobs.is_empty() {
            return;
        }
        self.ui_ephemeral.status_message =
            format!("Glue: {} トラックを焼き込み中...", jobs.len());
        self.ipc.pending_glue_bake = Some(PendingGlueBake { sel, jobs, current: 0 });
        if !self.send_glue_bake(0) {
            self.abort_glue_bake("Glue: 焼き込みを開始できませんでした".into());
        }
    }

    /// `index` 番目の job を engine へ投げる (対象トラックだけを残した song を積んでから
    /// 範囲を offline render)。
    fn send_glue_bake(&mut self, index: usize) -> bool {
        let Some(pending) = self.ipc.pending_glue_bake.as_ref() else {
            return false;
        };
        let (start_beat, end_beat) = (pending.sel.start_beat.max(0.0), pending.sel.end_beat);
        let Some(job) = pending.jobs.get(index) else {
            return false;
        };
        let (track_id, path) = (job.track_id, job.out_path.clone());
        if end_beat <= start_beat {
            return false;
        }
        // pre_fx = true: insert FX / フェーダー / pan を外した「素材の素の音」だけを焼く
        // (`docs/plan_glue_bake.md` §3)。
        let Some(isolated) = self.isolated_track_song(track_id, true) else {
            return false;
        };
        if let Some(p) = self.ipc.pending_glue_bake.as_mut() {
            p.current = index;
        }
        self.send_audio(common::protocol::AudioCommand::SetMasterGain(isolated.master_gain));
        self.send_audio(common::protocol::AudioCommand::LoadSong(isolated));
        self.send_plugin(common::protocol::PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_audio(common::protocol::AudioCommand::BounceClipFxOnline {
            path,
            source_track: track_id,
            // Glue の単位は範囲 × トラックなので「元 clip」は 1 つに定まらない。
            // echo は path + track で照合する (`handle_glue_bake_complete`)。
            source_clip: 0,
            start_beat,
            end_beat,
            // 素材だけの render なので曲頭から積み上げる意味が無い (= 範囲頭から cold)。
            warm: false,
        });
        true
    }

    /// `BounceClipFxComplete` が Glue の焼き込み宛だったら処理して `true`。
    /// (bounce の完了処理と同じ通知を共有するので、先にこちらで引き取る。)
    pub(crate) fn handle_glue_bake_complete(
        &mut self,
        path: &std::path::Path,
        source_track: u32,
        error: Option<&str>,
        frames: u64,
    ) -> bool {
        let Some(pending) = self.ipc.pending_glue_bake.as_mut() else {
            return false;
        };
        let index = pending.current;
        let Some(job) = pending.jobs.get_mut(index) else {
            return false;
        };
        if job.track_id != source_track || job.out_path != path {
            // 別 render の完了 (respawn 後の残骸等)。進行中の Glue は壊さない。
            return false;
        }
        if let Some(err) = error {
            self.abort_glue_bake(format!("Glue: 焼き込みに失敗しました ({err})"));
            return true;
        }
        if frames == 0 {
            self.abort_glue_bake("Glue: 焼き込み結果が空です".into());
            return true;
        }
        job.frames = frames;
        let next = index + 1;
        if next < pending.jobs.len() {
            if !self.send_glue_bake(next) {
                self.abort_glue_bake("Glue: 焼き込みを継続できませんでした".into());
            }
            return true;
        }
        // 全トラック完了 → engine を live へ戻してから 1 回の編集で適用する。
        self.send_plugin(common::protocol::PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Realtime,
        ));
        let Some(done) = self.ipc.pending_glue_bake.take() else {
            return true;
        };
        let baked: BTreeMap<u32, GlueBakeJob> =
            done.jobs.into_iter().map(|j| (j.track_id, j)).collect();
        self.apply_glue(&done.sel, &baked);
        // 適用**後**に engine の song を戻す (= 焼き上がった song をそのまま届ける)。
        // 先に戻すと isolated → 旧 song → 次フレームの epoch flush で新 song、と
        // LoadSong が 2 度走り、その間 engine は結合前の song で鳴る。
        self.restore_engine_song_after_bounce();
        true
    }

    /// 焼き込みを中断する。**何も変更せず**、出力ファイルを消して engine を戻す
    /// (全か無かの原則、`docs/plan_glue_bake.md` §4)。
    pub(crate) fn abort_glue_bake(&mut self, message: String) {
        if let Some(pending) = self.ipc.pending_glue_bake.take() {
            for job in &pending.jobs {
                let _ = std::fs::remove_file(&job.out_path);
            }
        }
        self.send_plugin(common::protocol::PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Realtime,
        ));
        self.restore_engine_song_after_bounce();
        tracing::warn!(%message, "Glue bake aborted");
        self.ui_ephemeral.status_message = message;
    }

    /// 結合を song へ適用する。`baked` に居るトラックは焼いた WAV へ置換、
    /// それ以外は非破壊 merge。**1 gesture = 1 undo step**。
    fn apply_glue(&mut self, sel: &TimeSelection, baked: &BTreeMap<u32, GlueBakeJob>) {
        if !sel.start_beat.is_finite() || !sel.end_beat.is_finite() {
            return;
        }
        // **結合できるトラックだけを切る。** 境界 split は破壊的なので、混在で結合を
        // 断るトラックや render できなかったトラックまで切ると、「何も結合していないのに
        // クリップだけ切り刻まれて残る」 (= 全か無かの原則を破る、
        // `docs/plan_glue_bake.md` §4)。
        let (a, b) = (sel.start_beat, sel.end_beat);
        let mut tracks: Vec<u32> = Vec::new();
        for (track_id, refs) in self.glue_refs_by_track(sel, false) {
            match glue_kind_of(self.song_doc.song(), &refs) {
                // audio は焼けたトラックだけ (render 失敗は無変更)。
                Some(GlueKind::Audio) if baked.contains_key(&track_id) => tracks.push(track_id),
                Some(GlueKind::Audio) | None => {}
                Some(_) => tracks.push(track_id),
            }
        }
        // `J` は 1 回の操作なので 1 undo step に束ねる。 境界の分割 (1 回) と
        // トラックごとの結合 (N 回) が別々の step になると、1 回の `J` を戻すのに
        // N+1 回 Undo が要る。**非同期の完了から呼ばれる**ので、進行中のドラッグの
        // bracket は横取りせず退避して戻す。
        let gesture = self.song_doc.enter_own_gesture();
        // **範囲の境界でクリップを割ってから集める。** はみ出した部分は元のクリップと
        // して残り、範囲の中身だけが 1 クリップへ焼き込まれる (Live の `Ctrl+E`
        // "Split Clip at Selection" と同じ切り出し、`docs/plan_range_selection.md` §7.1)。
        let split_tracks = tracks.clone();
        self.edit_song(move |song| {
            for track_id in &split_tracks {
                crate::handler::range_ops::split_track_at(song, *track_id, a);
                crate::handler::range_ops::split_track_at(song, *track_id, b);
            }
        });

        let mut new_refs: Vec<ClipKey> = Vec::new();
        let mut decoded: Vec<(common::model::AudioSourceId, std::path::PathBuf)> = Vec::new();
        let mut glued_count = 0usize;
        let mut had_mixed_kind = false;
        for (track_id, refs) in self.glue_refs_by_track(sel, true) {
            if refs.is_empty() {
                continue;
            }
            // 混在は**そのトラックだけ**飛ばす (旧実装は 1 トラックの混在で以降の全
            // トラックを skip しつつ、先に結合済みのトラックの編集は残していた)。
            // ここに来る前に「切ったトラック」だけへ絞ってあるので、混在トラックは
            // 切られてもいない。
            let Some(kind) = glue_kind_of(self.song_doc.song(), &refs) else {
                had_mixed_kind = true;
                continue;
            };
            if !tracks.contains(&track_id) {
                continue;
            }
            let placed = if kind == GlueKind::Audio {
                let Some(job) = baked.get(&track_id) else {
                    // render できなかったトラックには触れない。
                    continue;
                };
                self.place_baked_glue_clip(sel, track_id, &refs, job, &mut decoded)
            } else {
                self.place_merged_glue_clip(sel, track_id, &refs, kind)
            };
            if let Some(key) = placed {
                new_refs.push(key);
                glued_count += 1;
            }
        }
        self.song_doc.leave_own_gesture(gesture);

        // 焼いた WAV を即再生できるよう decode cache へ (失敗しても保存/再読込で回復)。
        for (source_id, path) in decoded {
            match crate::import_audio::decode_audio(&path) {
                Ok(buffer) => {
                    self.media.audio_source_cache.insert(source_id, std::sync::Arc::new(buffer));
                }
                Err(e) => tracing::warn!(
                    error = %e, path = %path.display(),
                    "Glue: 焼き込み WAV の decode に失敗 (次の保存/読込で回復)"
                ),
            }
        }

        if glued_count == 0 {
            self.ui_ephemeral.status_message = if had_mixed_kind {
                "Glue: 種類が混在しているため結合できません".into()
            } else {
                "Glue: 範囲の中にクリップがありません".into()
            };
            return;
        }
        tracing::info!(glued_count, ?new_refs, "Glue completed");
        self.select_new_clips(&new_refs);
        self.ui_ephemeral.status_message = if had_mixed_kind {
            format!("Glue: {glued_count} 箇所を結合しました (種類が混在したトラックは除外)")
        } else {
            format!("Glue: {glued_count} 箇所を結合しました")
        };
    }

    /// 焼いた WAV を指す 1 clip / 1 event を置く (元 clip は消す)。
    fn place_baked_glue_clip(
        &mut self,
        sel: &TimeSelection,
        track_id: u32,
        refs: &[ClipKey],
        job: &GlueBakeJob,
        decoded: &mut Vec<(common::model::AudioSourceId, std::path::PathBuf)>,
    ) -> Option<ClipKey> {
        let source = common::model::AudioSource {
            path: job.source_path.clone(),
            sample_rate: self.ipc.sample_rate,
            channels: 2,
            frames: job.frames,
            original_bpm: Some(self.song_doc.song().bpm),
            root_key: None,
        };
        let (start, len) = (sel.start_beat, sel.len_beats());
        let name = job.name.clone();
        // 焼き込み結果は範囲ちょうどを覆う 1 event (窓は先頭から)。tail の切り落としと
        // Raw 固定の理由は `baked_audio_event` (bounce と共通の SSoT) が持つ。
        let mut event = self.baked_audio_event(0, (sel.start_beat, sel.end_beat), 0.0, job.frames);
        let clip_ids: Vec<u32> = refs.iter().map(|r| r.clip_id).collect();
        let placed = self.edit_song(move |song| {
            let source_id = song.alloc_audio_source_id();
            song.media.audio_sources.insert(source_id, source);
            event.source_id = source_id;
            let content_id = song.alloc_content(
                ClipContent::Audio(AudioContent { next_event_id: 2, events: vec![event] }),
                name,
            );
            let track = song.track_by_id_mut(track_id)?;
            // 住所が id なので「後ろから消して index の詰まりを避ける」儀式は要らない。
            for id in &clip_ids {
                track.remove_clip_by_id(*id);
            }
            let clip_id = track.place_clip(Clip {
                start_beat: start,
                length_beats: len,
                content_id,
                // mute は焼いた音に反映済み (鳴っていた音がそのまま WAV になっている)。
                ..Clip::default()
            });
            Some((source_id, ClipKey { track_id, clip_id }))
        })?;
        let (source_id, key) = placed?;
        decoded.push((source_id, job.out_path.clone()));
        Some(key)
    }

    /// 非 audio kind の非破壊 merge (元 clip の中身を 1 content へ寄せる)。
    fn place_merged_glue_clip(
        &mut self,
        sel: &TimeSelection,
        track_id: u32,
        refs: &[ClipKey],
        kind: GlueKind,
    ) -> Option<ClipKey> {
        // **結合範囲 = 選択範囲そのもの** (`docs/plan_range_selection.md` §7.1)。
        // 範囲が中身より広ければ、前後の空白は content 内の「何も無い区間」として
        // 自然に表現される (`content_offset_beats` を負にする必要は無い)。
        let (start, len) = (sel.start_beat, sel.len_beats());
        let song = self.song_doc.song();
        let name = refs
            .first()
            .and_then(|r| song.clip_by_key(*r))
            .map(|c| song.content_name(c.content_id).to_string())
            .unwrap_or_default();
        let new_content = merged_content(kind, collect_fragments(song, refs, start));
        // merged clip は最初 (= 最も早い) の source clip の声 / mute を採用
        // (複数声混在時のポリシー)。 source 削除前に capture する。
        let meta = refs.first().and_then(|r| song.clip_by_key(*r)).map(|c| {
            (c.speaker_id, c.singer_name.clone(), c.style_name.clone(), c.talk, c.muted)
        });
        let (speaker_id, singer_name, style_name, talk, muted) =
            meta.unwrap_or((0, String::new(), String::new(), None, false));
        let clip_ids: Vec<u32> = refs.iter().map(|r| r.clip_id).collect();
        self.edit_song(move |song| {
            let content_id = song.alloc_content(new_content, name);
            let track = song.track_by_id_mut(track_id)?;
            for id in &clip_ids {
                track.remove_clip_by_id(*id);
            }
            let clip_id = track.place_clip(Clip {
                start_beat: start,
                length_beats: len,
                content_id,
                muted,
                speaker_id,
                singer_name,
                style_name,
                talk,
                ..Clip::default()
            });
            Some(ClipKey { track_id, clip_id })
        })?
    }
}
