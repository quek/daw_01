//! handler::media — audio/video/image/text/MIDI の import
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use std::path::{Path, PathBuf};
use common::model::{AudioContent, AudioEvent, Clip, ClipContent};
use crate::import_audio;

impl AppData {
    /// Import one or more audio files into the song (Phase 1 PR3).
    /// Synchronous — blocks the UI until decode completes (Phase 2
    /// will move this to a background thread; spec §7.4). Each file:
    ///
    /// 1. Hash + copy into `<project_dir>/samples/` (or import_cache
    ///    fallback for unsaved projects, §13 Q2).
    /// 2. Decode (WAV-only in Phase 1).
    /// 3. Allocate `AudioSourceId`, register on `Song.audio_sources`.
    /// 4. Stash decoded buffer in `audio_source_cache`.
    /// 5. Build a single `AudioEvent` covering the whole source and
    ///    wrap it in a fresh `ClipContent::Audio` content. Place a
    ///    `Clip` on the cursor track at the playhead. Phase 2 / PR4
    ///    refines drop-coordinate → (track, beat) resolution.
    ///
    /// Failures (unsupported format, oversize, decode error) surface
    /// in `status_message`; partial progress (= some files succeeded)
    /// is preserved.
    /// File menu → "Import Audio..." 経路。 `rfd` の native file picker
    /// (multi-select、 audio filter) を開いて、 選択された path を
    /// `action_import_audio` に転送するだけのラッパ。 dialog をキャンセル
    /// した場合は no-op。 起点が違うだけで採番 / dedup / コピー / decode
    /// は drag&drop と完全に同じ pipeline。 filter は `common::audio_decode`
    /// の対応拡張子 SSoT (WAV/AIFF/FLAC/MP3/OGG/M4A、 r.md #19)。
    pub(crate) fn action_open_import_audio_dialog(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("Audio", common::audio_decode::SUPPORTED_AUDIO_EXTENSIONS)
            .set_title("Import Audio");
        self.spawn_file_dialog(
            dialog,
            FileDialogMode::PickFiles,
            FileDialogKind::ImportAudio,
        );
    }

    /// PR-D 段階 3: Audio Editor の context menu "Add From Source..."。
    /// `rfd` で 1 ファイル選択 → `AddAudioEventFromFile` に転送 (= 内部
    /// で `import_audio::import_one` 経由で decode + AudioSource 採番)。
    /// `position_in_clip_beats` は呼び出し側 (= context menu 発火位置 =
    /// 直前 event の右端) で決定。 `handle_event` 経由なので auto Undo
    /// snapshot が積まれる。
    pub fn action_open_audio_event_dialog(
        &mut self,
        target: ClipKey,
        position_in_clip_beats: f64,
    ) {
        let dialog = rfd::FileDialog::new()
            .add_filter("Audio", common::audio_decode::SUPPORTED_AUDIO_EXTENSIONS)
            .set_title("Add Audio Event");
        self.spawn_file_dialog(
            dialog,
            FileDialogMode::PickFile,
            FileDialogKind::AddAudioEvent {
                clip: target,
                position_in_clip_beats,
            },
        );
    }

    pub(crate) fn action_import_audio(
        &mut self,
        paths: Vec<PathBuf>,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        let project_dir: Option<PathBuf> = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // 配置先 track の決定 (r.md #31):
        //  - Track(idx): drop が乗った既存 track (範囲外は末尾 track に clamp)。
        //  - NewTrackBottom: track の無い下の余白 drop → 一番下に新規 track を
        //    1 本作って積む (下記ループ内で lazily 生成)。song が空でも作れる。
        //  - NoHint: File menu / dialog (位置情報なし) → cursor track fallback (無ければ 0)。
        // `fixed_dest` = 既存 track を指す確定 index (Track/NoHint)。NewTrackBottom は
        // None で、初回ファイルの edit_song 内で一番下に track を新設する。既存 track を
        // 要する Track/NoHint で track が 0 本なら取り込めない (NewTrackBottom は除く)。
        let n_tracks = self.song_doc.song().tracks.len();
        let fixed_dest: Option<usize> = match target {
            // どちらも「一番下に新しいトラックを作る」。違うのは置き場所だけ
            // (アレンジのレーン / ランチャーのセル) で、それは `cell_index` が決める。
            ImportTrackTarget::NewTrackBottom | ImportTrackTarget::LauncherNewTrack { .. } => None,
            ImportTrackTarget::Track(i) => {
                if n_tracks == 0 {
                    self.ui_ephemeral.status_message =
                        "Audio import: 配置先のトラックが無いため取り込めません".to_string();
                    return;
                }
                Some((i as usize).min(n_tracks - 1))
            }
            // セルへの drop は **安定 id** で来る。解決できない (消えた) なら
            // 一番下に新規トラックを作る側へ倒す。
            ImportTrackTarget::LauncherCell { track_id, .. } => {
                self.song_doc.song().track_index_of(track_id)
            }
            ImportTrackTarget::NoHint => {
                if n_tracks == 0 {
                    self.ui_ephemeral.status_message =
                        "Audio import: 配置先のトラックが無いため取り込めません".to_string();
                    return;
                }
                Some(self.cursor_track_index().unwrap_or(0).min(n_tracks - 1))
            }
        };
        // NewTrackBottom で新設する track 名は最初のファイル名 (stem) を使う。
        let new_track_name = paths
            .first()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Audio")
            .to_string();

        // drag&drop の drop 位置 (`target_beat`) を最優先、 無ければ playhead。
        let start_beat_seed: f64 =
            target_beat.unwrap_or(self.transport.playhead_beat.unwrap_or(0.0) as f64);
        let bpm = self.song_doc.song().bpm;
        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut next_start_beat = start_beat_seed.max(0.0);
        // NewTrackBottom: 最初の成功ファイルで作った bottom track の index を覚え、
        // 2 ファイル目以降は同じ track に順送りで積む (track + clip1 が 1 undo step)。
        let mut bottom_idx: Option<usize> = None;
        // セルへの drop で 2 ファイル目以降を右の列へ送る量 (arrangement の
        // `next_start_beat` と同じ役割)。
        let mut cell_offset = 0usize;

        for path in paths {
            let imported = match import_audio::import_one(&path, project_dir.as_deref()) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };

            let length_beats =
                frames_to_beats(imported.buffer.frames, imported.buffer.sample_rate, bpm);

            let prev_bottom = bottom_idx;
            let track_name = new_track_name.clone();
            // 落とし先の列 (表示順 index)。`song` を要らないのでループ側で解く
            // (edit_song の閉包の中に入れるとネストが 1 段深くなる)。
            let cell_index = import_cell_index(target, cell_offset);
            let Some((source_id, dest_idx)) = self.edit_song(|song| {
                let source_id = song.alloc_audio_source_id();
                song.media.audio_sources.insert(source_id, imported.source);

                // r.md #87: セルへ落としたファイルは小節にフィットさせる
                // (`cell_place_len` の doc)。
                let (place_len, stretch_mode) =
                    cell_place_len(cell_index.is_some(), length_beats, song.time_sig);

                // v29: 新規 content の単一 event なので id=1 / allocator は 2 から。
                let event = AudioEvent {
                    id: 1,
                    source_id,
                    event_start_in_clip_beats: 0.0,
                    // 素材全体 (`source_*_frames`) を `place_len` 拍に写す。
                    event_length_beats: place_len,
                    source_start_frames: 0,
                    source_end_frames: imported.buffer.frames,
                    stretch_mode,
                    ..AudioEvent::default()
                };
                let content_id = song.alloc_content(
                    ClipContent::Audio(AudioContent {
                        events: vec![event],
                        next_event_id: 2,
                    }),
                    imported.display_name.clone(),
                );

                // 配置先 track index を確定。 NewTrackBottom は初回だけ一番下に
                // 空 track を push してその index を使う (以降は prev_bottom を再利用)。
                let dest_idx = match fixed_dest {
                    Some(idx) => idx,
                    None => match prev_bottom {
                        Some(idx) => idx,
                        None => {
                            let track_id = song.alloc_track_id();
                            song.tracks.push(track_with(|t| {
                                t.id = track_id;
                                t.name = track_name;
                            }));
                            song.tracks.len() - 1
                        }
                    },
                };
                // r.md #87: ランチャーのセルへ落としたときは **arrangement ではなく
                // その行のセル**に置く。列は表示順 index で来るので、ここで
                // `ensure_scene_at` が実体化する (load 時に列を補わない規約のまま)。
                let cell_scene = cell_index.map(|i| song.ensure_scene_at(i));
                place_audio_clip(
                    &mut song.tracks[dest_idx],
                    cell_scene,
                    next_start_beat,
                    place_len,
                    content_id,
                );
                (source_id, dest_idx)
            }) else {
                return;
            };
            if fixed_dest.is_none() {
                bottom_idx = Some(dest_idx);
            }
            self.media.audio_source_cache.insert(source_id, imported.buffer.clone());
            next_start_beat += length_beats;
            cell_offset += 1;
            imported_ok += 1;
        }


        self.ui_ephemeral.status_message = match (imported_ok, errors.is_empty()) {
            (0, false) => format!("Audio import 失敗: {}", errors.join(" / ")),
            (n, true) => format!("Audio import 完了: {n} ファイル"),
            (n, false) => format!(
                "Audio import: {n} ファイル成功、 {} 件エラー: {}",
                errors.len(),
                errors.join(" / ")
            ),
        };
    }

    /// Video import (docs/plan_video.md P2). For each path:
    ///   1. `import_one_video` does the WMF metadata read + audio
    ///      extract + decode (on the GUI thread; typical phone-video
    ///      imports finish in 1-3s).
    ///   2. Allocate `AudioSourceId` for the extracted audio (if any),
    ///      register it on `Song.audio_sources`, cache the decoded
    ///      buffer.
    ///   3. Allocate `VideoSourceId`, link it to the audio id, register
    ///      on `Song.video_sources`.
    ///   4. Build one `VideoEvent` covering the whole source and wrap
    ///      it in a fresh `ClipContent::Video`.
    ///   5. Append a new `TrackKind::Video` track and (when audio is
    ///      present) a paired `TrackKind::Audio` track. Each carries a
    ///      single clip starting at the playhead.
    ///
    /// Subsequent imports stack at the end of the timeline by bumping
    /// `next_start_beat`. Failures are collected per path and surfaced
    /// in `status_message` along with the success count.
    #[cfg(windows)]
    pub(crate) fn action_import_video(&mut self, paths: Vec<PathBuf>, target_beat: Option<f64>) {
        use common::model::{
            AudioContent, AudioEvent, ClipContent, VideoContent, VideoEvent,
        };

        if paths.is_empty() {
            return;
        }
        let project_dir: Option<PathBuf> = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // drag&drop の drop 位置 (`target_beat`) を最優先、 無ければ playhead。
        let start_beat_seed: f64 =
            target_beat.unwrap_or(self.transport.playhead_beat.unwrap_or(0.0) as f64);
        let bpm = self.song_doc.song().bpm;
        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut next_start_beat = start_beat_seed.max(0.0);

        for path in paths {
            let imported = match crate::import_video::import_one_video(
                &path,
                project_dir.as_deref(),
            ) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let display_name = imported.display_name.clone();
            let duration_micros = imported.video_source.duration_micros;
            let video_length_beats = micros_to_beats(duration_micros, bpm);

            // 1) Register paired audio (if present) before video so we
            //    have the AudioSourceId for the video back-link.
            let audio_source_id = match &imported.audio {
                Some(audio) => {
                    let Some(id) = self.edit_song(|song| {
                        let id = song.alloc_audio_source_id();
                        song.media.audio_sources.insert(id, audio.source.clone());
                        id
                    }) else {
                        return;
                    };
                    self.media.audio_source_cache.insert(id, audio.buffer.clone());
                    Some(id)
                }
                None => None,
            };

            // 2) Register video source with the audio back-link.
            let mut vs = imported.video_source;
            vs.audio_source_id = audio_source_id;
            let Some(video_source_id) = self.edit_song(|song| {
                let id = song.alloc_video_source_id();
                song.media.video_sources.insert(id, vs);
                id
            }) else {
                return;
            };

            // 2b) Stash the thumbnail RGBA (if extracted) and queue
            //     a GPU upload. The runner picks this up next frame
            //     (P3.5) and writes the resulting TextureHandle into
            //     `video_texture_cache` for the arrangement view to
            //     read.
            if let Some(thumb) = imported.thumbnail {
                self.media.video_thumbnail_rgba.insert(
                    video_source_id,
                    (thumb.width, thumb.height, std::sync::Arc::new(thumb.rgba)),
                );
                self.media.pending_thumbnail_uploads.push(video_source_id);
            }

            // 3) Video clip content + auto track.
            self.edit_song(|song| {
                let v_content_id = song.alloc_content(
                    ClipContent::Video(VideoContent {
                        events: vec![VideoEvent {
                            source_id: video_source_id,
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: video_length_beats,
                            source_start_micros: 0,
                            source_end_micros: duration_micros,
                            ..VideoEvent::default()
                        }],
                    }),
                    display_name.clone(),
                );
                let video_track_id = song.alloc_track_id();
                let mut video_track = track_with(|t| {
                    t.id = video_track_id;
                    t.name = format!("{display_name} (Video)");
                });
                let v_clip_id = video_track.alloc_clip_id();
                video_track.clips.push(Clip {
                    id: v_clip_id,
                    start_beat: next_start_beat,
                    length_beats: video_length_beats,
                    content_id: v_content_id,
                    color: None,
                    auto_lipsync: false,
                    ..Default::default()
                });
                song.tracks.push(video_track);
            });

            // 4) Paired audio clip + audio track (only when audio is
            //    present in the source).
            if let (Some(audio), Some(audio_src_id)) =
                (imported.audio, audio_source_id)
            {
                let audio_length_beats = frames_to_beats(
                    audio.buffer.frames,
                    audio.buffer.sample_rate,
                    bpm,
                );
                self.edit_song(|song| {
                    let a_content_id = song.alloc_content(
                        ClipContent::Audio(AudioContent {
                            events: vec![AudioEvent {
                                // v29: 新規 content の単一 event = id 1。
                                id: 1,
                                source_id: audio_src_id,
                                event_start_in_clip_beats: 0.0,
                                event_length_beats: audio_length_beats,
                                source_start_frames: 0,
                                source_end_frames: audio.buffer.frames,
                                ..AudioEvent::default()
                            }],
                            next_event_id: 2,
                        }),
                        format!("{display_name} (audio)"),
                    );
                    let audio_track_id = song.alloc_track_id();
                    let mut audio_track = track_with(|t| {
                        t.id = audio_track_id;
                        t.name = format!("{display_name} (Audio)");
                    });
                    let a_clip_id = audio_track.alloc_clip_id();
                    audio_track.clips.push(Clip {
                        id: a_clip_id,
                        start_beat: next_start_beat,
                        length_beats: audio_length_beats,
                        content_id: a_content_id,
                        color: None,
                        auto_lipsync: false,
                        ..Default::default()
                    });
                    song.tracks.push(audio_track);
                });
            }

            next_start_beat += video_length_beats;
            imported_ok += 1;
        }


        self.ui_ephemeral.status_message = match (imported_ok, errors.is_empty()) {
            (0, false) => format!("Video import 失敗: {}", errors.join(" / ")),
            (n, true) => format!("Video import 完了: {n} ファイル (V + A track 追加)"),
            (n, false) => format!(
                "Video import: {n} ファイル成功、 {} 件エラー: {}",
                errors.len(),
                errors.join(" / ")
            ),
        };
    }

    /// File menu → "Import Video..." 経路。 `rfd` の native file picker
    /// (multi-select、 mp4/mov/mkv/webm filter) を開いて、 選択された
    /// path を `action_import_video` に転送する。
    #[cfg(windows)]
    pub(crate) fn action_open_import_video_dialog(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "mov", "mkv", "webm", "m4v", "avi"])
            .set_title("Import Video...");
        self.spawn_file_dialog(
            dialog,
            FileDialogMode::PickFiles,
            FileDialogKind::ImportVideo,
        );
    }

    /// v13 (`docs/plan_image_overlay.md` §P2): import one or more
    /// image files as PiP overlay clips. Each successful import:
    ///
    /// 1. Allocates an `ImageSourceId` and registers an `ImageSource`
    ///    in `Song.image_sources` (path / dimensions / format).
    /// 2. Stages the BGRA8 bytes in `image_source_bgra` and queues a
    ///    GPU texture upload via `pending_image_uploads` — the runner
    ///    picks this up next frame (P3) and writes the resulting
    ///    `TextureHandle` into `image_texture_cache`.
    /// 3. Creates a Video-kind Track + an Image clip occupying the
    ///    project length (= so the user immediately sees the image
    ///    on top of any active video). PiP rect defaults to full-
    ///    screen; the user shrinks/positions it via the P5 drag
    ///    handle UI or the P4 inspector.
    ///
    /// Errors are accumulated; partial-success is permitted (= the
    /// status bar summarizes how many files succeeded / failed).
    pub(crate) fn action_import_image(
        &mut self,
        paths: Vec<PathBuf>,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        let project_dir = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

        // drop が既存 track を指していれば (`Track(idx)`) その track に画像 clip を
        // 貼り付ける (= ドロップしたトラックに追加)。 track の無い下の余白 drop
        // (`NewTrackBottom`) や dialog 経由 (`NoHint`) は一番下に新規 track を作って
        // 貼る (r.md #31: 以前は arrangement 先頭 index 0 への insert だった)。
        // r.md #87: セルへの drop は安定 id なので、まず id → index を解く。
        let cell_track_idx = match target {
            ImportTrackTarget::LauncherCell { track_id, .. } => {
                self.song_doc.song().track_index_of(track_id)
            }
            _ => None,
        };
        let dest_track_idx: Option<usize> = cell_track_idx
            .or_else(|| resolve_media_drop_target(target, self.song_doc.song().tracks.len()));

        // drag&drop の drop 位置 (`target_beat`) を最優先。 無いとき (dialog 経由)
        // は従来挙動: 既存 track に貼るときは playhead を seed に順送り配置
        // (複数枚を重ねない)、 新規 track 経路は各画像が自分の track を持つので
        // beat 0 始まり。
        let mut next_start_beat = match target_beat {
            Some(b) => b.max(0.0),
            None if dest_track_idx.is_some() => {
                (self.transport.playhead_beat.unwrap_or(0.0) as f64).max(0.0)
            }
            None => 0.0_f64,
        };
        let image_clip_length_beats = (self.song_doc.song().length_beats * 0.5).max(8.0);

        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        // セルへの drop で 2 枚目以降を右の列へ送る量 (audio import と同じ役割)。
        let mut cell_offset = 0usize;
        for path in &paths {
            let imported = match crate::import_image::import_one_image(
                path,
                project_dir.as_deref(),
            ) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };

            // 1) Register ImageSource + stage BGRA for GPU upload.
            // bgra length matches width * height * 4 by construction of
            // `import_one_image`; re-fetch dims from the source we just
            // inserted so the staging matches.
            let cell_idx = import_cell_index(target, cell_offset);
            let Some((image_source_id, src_w, src_h)) = self.edit_song(|song| {
                let image_source_id = song.alloc_image_source_id();
                song.media.image_sources.insert(image_source_id, imported.source);
                let src = &song.media.image_sources[&image_source_id];
                (image_source_id, src.width, src.height)
            }) else {
                return;
            };
            self.media.image_source_bgra.insert(
                image_source_id,
                (src_w, src_h, std::sync::Arc::new(imported.bgra)),
            );
            self.media.pending_image_uploads.push(image_source_id);

            // 2) Build the Image clip content. Single ImageEvent
            // covering the whole clip。 デフォルト PiP rect は
            // `Song.video_resolution` と画像 aspect で「アスペクト比
            // 維持の中央配置」 を計算する (= 縦長画像を 16:9 preview に
            // 入れると上下に余白、 横長画像なら左右に余白)。 ユーザーが
            // 後から inspector / preview drag で自由に拡縮 / 配置できる。
            let display_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("image")
                .to_string();
            let (def_x, def_y, def_w, def_h) = aspect_fit_pip_rect(
                self.song_doc.song().video_resolution,
                (src_w, src_h),
            );
            self.edit_song(|song| {
                let i_content_id = song.alloc_content(
                    common::model::ClipContent::Image(common::model::ImageContent {
                        events: vec![common::model::ImageEvent {
                            source_id: image_source_id,
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: image_clip_length_beats,
                            x: def_x,
                            y: def_y,
                            w: def_w,
                            h: def_h,
                            ..common::model::ImageEvent::default()
                        }],
                    }),
                    display_name.clone(),
                );

                // 3) 配置先 track を決める。
                //    - 既存 track (drop 先): その index にそのまま貼る。
                //    - 新規 track: arrangement の一番下 (末尾 push) に track を作って
                //      貼る (r.md #31: ドロップ位置どおり一番下へ。以前は先頭 index 0 に
                //      insert して既存 video layer の手前に合成していた — composite は
                //      top-wins なので一番下 = 奥になる、plan_video §4 P7)。
                let place_idx = match dest_track_idx {
                    Some(idx) => idx,
                    None => {
                        let image_track_id = song.alloc_track_id();
                        song.tracks.push(track_with(|t| {
                            t.id = image_track_id;
                            t.name = format!("{display_name} (Image)");
                        }));
                        song.tracks.len() - 1
                    }
                };
                // r.md #87: セルへ落としたときは **アレンジではなくその行のセル**へ。
                place_new_clip(
                    song,
                    place_idx,
                    cell_idx,
                    next_start_beat,
                    image_clip_length_beats,
                    i_content_id,
                );
            });
            // 既存 track に複数枚貼るときだけ順送り。 新規 track 経路は各画像が
            // 自分の track を持つので beat 0 固定 (従来挙動)。
            if dest_track_idx.is_some() {
                next_start_beat += image_clip_length_beats;
            }
            cell_offset += 1;
            imported_ok += 1;
        }

        // No `flush_song_sync` — image clips have no
        // audio engine implications, the daw_audio process never
        // sees them.

        // `paths.is_empty()` early-returns above, so we know
        // imported_ok + errors.len() >= 1 — the (0, true) "nothing
        // happened" case is unreachable here.
        self.ui_ephemeral.status_message = match (imported_ok, errors.is_empty()) {
            (0, false) => format!("Image import 失敗: {}", errors.join(" / ")),
            (n, true) => format!("Image import 完了: {n} ファイル"),
            (n, false) => format!(
                "Image import: {n} ファイル成功、 {} 件エラー: {}",
                errors.len(),
                errors.join(" / ")
            ),
        };
    }

    /// docs/plan_text_clip_creation.md: 空きレーン右クリック → "Text クリップ" 経路。
    /// `track_id` の track の `start_beat` 位置に `ClipContent::Text` clip を 1 個追加する。
    /// clip は default 体裁 ("Title" / 64px / 中央横帯) の単一 `TextEvent` を持つ。
    /// 「text トラック」 は存在せず (v16 で全 track 統一済み)、 text は他 clip と同じく
    /// 任意の track 上にタイムラインで生成する。 content / styles は inspector、 PiP rect は
    /// preview drag で編集。 clip 長は他 clip 生成 (`create_clip`) と同じ `DEFAULT_CLIP_LENGTH`。
    pub(crate) fn add_text_clip_to_track(&mut self, track_id: u32, start_beat: f64) {
        let Some(track_idx) = self.song_doc.song().tracks.iter().position(|t| t.id == track_id) else {
            return;
        };
        let start_beat = start_beat.max(0.0);
        let length_beats = DEFAULT_CLIP_LENGTH;

        let Some(new_clip_idx) = self.edit_song(|song| {
            let content_id = song.alloc_content(
                common::model::ClipContent::Text(common::model::TextContent {
                    events: vec![common::model::TextEvent {
                        text: "Title".into(),
                        event_length_beats: length_beats,
                        ..common::model::TextEvent::default()
                    }],
                }),
                // デフォルトでクリップ名は無し。 表示名は clip_display_label が
                // TextEvent.text ("Title") から導出する (= 名前 == 本文)。
                String::new(),
            );

            let track = &mut song.tracks[track_idx];
            let clip_id = track.alloc_clip_id();
            let new_clip_idx = track.clips.len() as u32;
            track.clips.push(common::model::Clip {
                id: clip_id,
                start_beat,
                length_beats,
                content_id,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            });
            new_clip_idx
        }) else {
            return;
        };

        // create_clip と同様、 生成直後の clip を選択して inspector に出す。
        let r = ClipKey {
            track_id: track_idx as u32,
            clip_id: new_clip_idx,
        };
        self.set_single_clip_selection(r);
        self.selection.selected_notes.clear();
        self.select_track(track_idx as u32);

        self.ui_ephemeral.status_message = "Text clip 追加".into();
    }

    /// File menu → "Import Image..." 経路。 `rfd` の native file picker
    /// (multi-select、 png/jpg/jpeg/webp/bmp/gif filter) を開いて、 選択
    /// された path を `action_import_image` に転送する。 OS-neutral
    /// (= image crate のみ、 cfg(windows) 不要)。
    pub(crate) fn action_open_import_image_dialog(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter(
                "Image",
                &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "tga", "gif"],
            )
            .set_title("Import Image...");
        self.spawn_file_dialog(
            dialog,
            FileDialogMode::PickFiles,
            FileDialogKind::ImportImage,
        );
    }

    /// File menu → "Import MIDI..." 経路 (r.md #66)。`rfd` の native picker を
    /// 開いて選択 path を `action_import_midi` に転送する。起点が違うだけで
    /// 解析 / track 生成 / 配置は drag&drop と完全に同じ pipeline。
    pub(crate) fn action_open_import_midi_dialog(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("MIDI", crate::midi_import::SUPPORTED_MIDI_EXTENSIONS)
            .set_title("Import MIDI...");
        self.spawn_file_dialog(dialog, FileDialogMode::PickFiles, FileDialogKind::ImportMidi);
    }

    /// MIDI (SMF) ファイルの取り込み (r.md #66、設計正本
    /// [`docs/plan_midi_import.md`](../../../docs/plan_midi_import.md))。
    ///
    /// 1. 全ファイルを解析 (`midi_import::parse_midi_bytes`)。SMF track 1 本 =
    ///    daw_01 track 1 本、1 SMF track に複数 channel が混在すれば channel 分割。
    /// 2. 曲にクリップが 1 つも無いときだけ、SMF のテンポ / 拍子を採用する
    ///    (既存クリップがある曲で BPM を変えると audio / video の実時間位置が
    ///    全部ずれるため。ユーザー確定事項)。
    /// 3. `target` が既存 track を指していれば 1 本目をその track に載せ、2 本目
    ///    以降はその直下へ挿入 (`parent_group_id` はアンカー track から継承)。
    ///    それ以外は全部一番下に追加 (r.md #31 の統一規則)。
    /// 4. clip は「content の窓」として作る: content-local 拍は SMF tick 0 起点の
    ///    まま保ち、`content_offset_beats` で音の始まる小節まで窓を進める。
    ///
    /// 解析 / ファイル I/O は `edit_song` の外で済ませ、Song 変更は 1 回の
    /// `edit_song` に閉じる (= 1 undo step、dirty / 子プロセス sync は chokepoint 任せ)。
    pub(crate) fn action_import_midi(
        &mut self,
        paths: Vec<PathBuf>,
        target: ImportTrackTarget,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        // SMPTE timing (division 負値) の SMF は tick が「秒の細分」なので、
        // 取り込み先プロジェクトのテンポで拍に直す。metrical のファイルでは使われない。
        let tempo_map = common::tempo_map::TempoMap::from_song(self.song_doc.song());
        let seconds_to_beat = |s: f64| tempo_map.seconds_to_beat(s);

        let mut parsed_files: Vec<(PathBuf, String, crate::midi_import::ParsedMidi)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for path in &paths {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("MIDI")
                .to_string();
            match crate::midi_import::read_and_parse(path, &seconds_to_beat) {
                Ok(p) => parsed_files.push((path.clone(), stem, p)),
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }
        if parsed_files.is_empty() {
            self.ui_ephemeral.status_message =
                format!("MIDI import 失敗: {}", errors.join(" / "));
            return;
        }

        // drop 位置 (無ければ playhead) が SMF tick 0 の置き場所。
        let drop_beat = target_beat
            .unwrap_or(self.transport.playhead_beat.unwrap_or(0.0) as f64)
            .max(0.0);
        let song_is_empty = song_has_no_clips(self.song_doc.song());
        let anchor = resolve_media_drop_target(target, self.song_doc.song().tracks.len());

        // ---- 採用するテンポ / 拍子を先に確定する (空の曲のときだけ) ----
        let adopted_time_sig = if song_is_empty {
            parsed_files[0].2.time_sig
        } else {
            None
        };
        let adopted_bpm: Option<f32> = if song_is_empty {
            parsed_files[0].2.tempo.first().map(|&(_, bpm)| bpm)
        } else {
            None
        };
        // SMPTE (絶対時刻) のファイルは「秒 → 拍」を**取り込み前の**テンポで解いている。
        // テンポを採用すると換算に使った BPM と再生 BPM が食い違い、SMPTE 経路が
        // 守ろうとした実時間位置がずれる。採用後のテンポで解き直す。
        if let Some(bpm) = adopted_bpm {
            let adopted_conv = |s: f64| s * f64::from(bpm) / 60.0;
            for (path, _, parsed) in parsed_files.iter_mut() {
                if !parsed.is_smpte {
                    continue;
                }
                match crate::midi_import::read_and_parse(path, &adopted_conv) {
                    Ok(again) => *parsed = again,
                    Err(e) => tracing::warn!(error = %e, path = %path.display(),
                        "SMPTE MIDI の再解析に失敗 (取り込み前のテンポのまま配置する)"),
                }
            }
        }
        // テンポカーブを作るのは metrical のファイルだけ。SMPTE は絶対時刻が正本で
        // tempo meta は再生タイミングの正本ではないので、曲頭 BPM だけ採用する。
        let tempo_curve: &[(f64, f32)] = if adopted_bpm.is_some() && !parsed_files[0].2.is_smpte {
            &parsed_files[0].2.tempo
        } else {
            &[]
        };
        // テンポ clip が覆うべき範囲 (= 取り込む素材の終端、content-local 拍)。
        let content_end = parsed_files
            .iter()
            .map(|(_, _, p)| p.end_beat())
            .fold(0.0_f64, f64::max);

        let mut created_tracks = 0usize;
        let mut placed_clips = 0usize;
        let mut notes_total = 0usize;
        let mut dropped_events = 0usize;
        let mut tempo_adopted = false;

        let applied = self.edit_song(|song| {
            let mut song_end = 0.0_f64;
            // ---- テンポ / 拍子 ----
            if let Some(ts) = adopted_time_sig {
                song.time_sig = ts;
            }
            // 小節長 (拍) — 拍子採用後の値で計算する。
            let bar_beats =
                f64::from(song.time_sig.0).max(1.0) * 4.0 / f64::from(song.time_sig.1).max(1.0);
            if let Some(bpm) = adopted_bpm {
                song.bpm = bpm;
                tempo_adopted = true;
                if tempo_curve.len() > 1 {
                    song_end = song_end.max(install_song_tempo_lane(
                        song,
                        tempo_curve,
                        drop_beat,
                        content_end,
                        bar_beats,
                    ));
                }
            }

            // ---- 配置 ----
            // 1 本目だけ既存 track に載せ (anchor)、以降は insert_at に順次挿入する。
            let mut anchor = anchor;
            let mut insert_at = anchor.map(|i| i + 1);
            let parent_group_id = anchor
                .and_then(|i| song.tracks.get(i))
                .and_then(|t| t.parent_group_id);
            for (_, stem, parsed) in &parsed_files {
                dropped_events += parsed.dropped_events;
                for ptrack in &parsed.tracks {
                    let (first_beat, last_beat) = ptrack.span_beats();
                    // clip は content の窓: 音の始まる小節から、終わる小節まで。
                    let win_start = (first_beat / bar_beats).floor() * bar_beats;
                    let win_end = (((last_beat - 1e-9) / bar_beats).ceil() * bar_beats)
                        .max(win_start + bar_beats);
                    let name = midi_track_name(ptrack, stem);
                    // clip 名は歌詞が無いときだけ入れる。明示名はクリップ表示で歌詞
                    // より優先されるので、歌詞入り (.kar) では歌詞を隠してしまう
                    // (`widgets/arrangement/view_build.rs` の表示優先順位)。
                    let content_name = if ptrack.has_lyrics() {
                        String::new()
                    } else {
                        name.clone()
                    };
                    let content_id = song.alloc_content(
                        ClipContent::Midi(common::model::MidiContent {
                            next_note_id: ptrack.notes.len() as u32 + 1,
                            notes: ptrack.notes.clone(),
                        }),
                        content_name,
                    );
                    let dest_idx = match anchor.take() {
                        Some(idx) => idx,
                        None => {
                            let track_id = song.alloc_track_id();
                            let track = track_with(|t| {
                                t.id = track_id;
                                t.name = name;
                                t.parent_group_id = parent_group_id;
                            });
                            created_tracks += 1;
                            match insert_at {
                                Some(at) => {
                                    let at = at.min(song.tracks.len());
                                    song.tracks.insert(at, track);
                                    insert_at = Some(at + 1);
                                    at
                                }
                                None => {
                                    song.tracks.push(track);
                                    song.tracks.len() - 1
                                }
                            }
                        }
                    };
                    // r.md #87: セルへ落としたら **アレンジではなくセル**へ置く。
                    // SMF が複数トラックを作る場合も同じ列に 1 つずつ並べる。
                    let start_beat = drop_beat + win_start;
                    let length_beats = win_end - win_start;
                    place_new_midi_clip(
                        song,
                        dest_idx,
                        import_cell_index(target, 0),
                        (start_beat, length_beats, win_start),
                        content_id,
                    );
                    song_end = song_end.max(start_beat + length_beats);
                    placed_clips += 1;
                    notes_total += ptrack.notes.len();
                }
            }
            // 取り込みが曲の長さを超えたら伸ばす (伸ばさないと「全曲」書き出しが
            // 既定 64 拍で切れる)。縮めることはしない。
            song.length_beats = song.length_beats.max(song_end);
        });
        if applied.is_none() {
            // 書き出し中は編集が拒否される。
            return;
        }
        self.resize_track_peak_display();

        let mut msg = format!(
            "MIDI import 完了: {placed_clips} トラック / {notes_total} ノート"
        );
        if tempo_adopted {
            msg.push_str("、テンポを取り込み");
        }
        if dropped_events > 0 {
            msg.push_str(&format!(
                "、CC / ピッチベンド等 {dropped_events} イベントは非対応のため破棄"
            ));
        }
        if created_tracks > 0 {
            msg.push_str("、新規トラックには音源が入っていません");
        }
        if !errors.is_empty() {
            msg.push_str(&format!(" ({} 件エラー: {})", errors.len(), errors.join(" / ")));
        }
        self.ui_ephemeral.status_message = msg;
    }
}

/// 曲にクリップが 1 つも無いか (= MIDI import がテンポ / 拍子を取り込んでよいか)。
/// track の clip、track automation lane の clip、song automation lane の clip を
/// すべて見る (「クリップが 1 つも無い曲」というユーザー向けの定義そのまま)。
pub(crate) fn song_has_no_clips(song: &common::model::Song) -> bool {
    song.tracks.iter().all(|t| {
        t.clips.is_empty() && t.automation_lanes.iter().all(|l| l.clips.is_empty())
    }) && song.song_lanes.iter().all(|l| l.clips.is_empty())
}

/// import した track の表示名。SMF の TrackName meta を優先し、無ければファイル名。
/// 1 つの SMF track を channel で割った場合は ch 番号を付けて区別する。
pub(crate) fn midi_track_name(track: &crate::midi_import::ParsedTrack, stem: &str) -> String {
    let base = track.name.clone().unwrap_or_else(|| stem.to_string());
    if track.channel_split {
        format!("{base} ch{}", u16::from(track.channel) + 1)
    } else {
        base
    }
}

/// SMF の tempo breakpoint を `SongTempo` automation lane として組み、作った
/// automation clip の終端 (song 拍) を返す (`midi_export.rs` の階段近似の逆写像)。
/// SMF は step tempo なので [`AutomationCurve::Hold`] を使う。`origin_beat` は
/// SMF tick 0 を置いた song 拍、`content_end_beat` は取り込む素材の終端 (content-local)。
///
/// **clip は最後の breakpoint を厳密に内側に含む長さにする**。automation clip の
/// 範囲は半開区間 `[start, start + length)` (`common/src/automation.rs` の
/// `clip_covering`) なので、clip 長を「最後の breakpoint の拍」ぴったりにすると
/// 最後のテンポ変化が評価されず、しかも clip の外は `lane.default_value` (= 曲頭
/// BPM) に戻るため、テンポ変化が 2 点だけの普通のファイルでテンポマップが丸ごと
/// 無効になる。
fn install_song_tempo_lane(
    song: &mut common::model::Song,
    tempo: &[(f64, f32)],
    origin_beat: f64,
    content_end_beat: f64,
    bar_beats: f64,
) -> f64 {
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget,
    };
    let Some(&(first_beat, first_bpm)) = tempo.first() else {
        return 0.0;
    };
    let start_beat = origin_beat + first_beat;
    // 最後の breakpoint より必ず先へ伸ばし、素材の終端まで覆う
    // (半開区間なので「最後の point = clip 終端」だとその変化が効かない)。
    let last_point_beat = origin_beat + tempo.last().map_or(first_beat, |&(b, _)| b);
    let bar = bar_beats.max(1e-6);
    let end_beat = (origin_beat + content_end_beat).max(last_point_beat + bar);
    let points: Vec<AutomationPoint> = tempo
        .iter()
        .enumerate()
        .map(|(i, &(beat, bpm))| AutomationPoint {
            id: i as u32 + 1,
            time_beat: (origin_beat + beat - start_beat).max(0.0),
            value: f64::from(bpm),
            curve: AutomationCurve::Hold,
        })
        .collect();
    let content_id = song.alloc_content(
        ClipContent::Automation(AutomationContent {
            next_point_id: points.len() as u32 + 1,
            points,
        }),
        String::new(),
    );
    // 既に SongTempo lane があればそこへ clip を足す (lane は target ごとに 1 本)。
    let lane = if let Some(idx) = song
        .song_lanes
        .iter()
        .position(|l| l.target == AutomationTarget::SongTempo)
    {
        &mut song.song_lanes[idx]
    } else {
        let lane_id = song.alloc_song_lane_id();
        let mut lane = AutomationLane::new(AutomationTarget::SongTempo, f64::from(first_bpm));
        lane.id = lane_id;
        song.song_lanes.push(lane);
        song.song_lanes.last_mut().expect("just pushed")
    };
    // clip の外 (= 取り込み位置より前) で使われる値。新規 lane / 再利用 lane で
    // 挙動が割れないよう、どちらでも曲頭 BPM に揃える。
    lane.default_value = f64::from(first_bpm);
    let clip_id = lane.alloc_clip_id();
    lane.clips.push(AutomationClip {
        id: clip_id,
        name: "Tempo".to_string(),
        start_beat,
        length_beats: (end_beat - start_beat).max(bar),
        content_id,
        content_offset_beats: 0.0,
    });
    end_beat
}

/// 取り込んだオーディオを配置する。**列 (`cell_scene`) が `Some` ならランチャーの
/// セル、`None` ならアレンジのクリップ**。セルは「撃った瞬間」が原点なので開始拍を
/// 持たない (`SessionClip::clip` の契約)。同じ列に既にセルがあれば置き換える
/// (ドロップは上書きの意味)。
fn place_audio_clip(
    track: &mut common::model::Track,
    cell_scene: Option<u32>,
    start_beat: f64,
    length_beats: f64,
    content_id: common::model::ContentId,
) {
    let id = track.alloc_clip_id();
    place_imported_clip(
        track,
        cell_scene,
        Clip {
            id,
            start_beat,
            length_beats,
            content_id,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        },
    );
}

/// 取り込んだ 1 クリップを `track_idx` の行へ置く (アレンジ / セル共通)。
/// `cell_index` が `Some` ならその表示順の列を実体化してセルにする。
fn place_new_clip(
    song: &mut common::model::Song,
    track_idx: usize,
    cell_index: Option<usize>,
    start_beat: f64,
    length_beats: f64,
    content_id: common::model::ContentId,
) {
    let cell_scene = cell_index.map(|i| song.ensure_scene_at(i));
    let Some(track) = song.tracks.get_mut(track_idx) else {
        return;
    };
    let id = track.alloc_clip_id();
    place_imported_clip(
        track,
        cell_scene,
        Clip {
            id,
            start_beat,
            length_beats,
            content_id,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        },
    );
}

/// [`place_new_clip`] の MIDI 版 (窓の開始 `content_offset_beats` を持つ)。
/// `span` = `(start_beat, length_beats, content_offset_beats)`。
fn place_new_midi_clip(
    song: &mut common::model::Song,
    track_idx: usize,
    cell_index: Option<usize>,
    span: (f64, f64, f64),
    content_id: common::model::ContentId,
) {
    let cell_scene = cell_index.map(|i| song.ensure_scene_at(i));
    let Some(track) = song.tracks.get_mut(track_idx) else {
        return;
    };
    let id = track.alloc_clip_id();
    place_imported_clip(
        track,
        cell_scene,
        Clip {
            id,
            start_beat: span.0,
            length_beats: span.1,
            content_id,
            content_offset_beats: span.2,
            ..Default::default()
        },
    );
}

/// セルへ落とすときの長さと伸縮モード。
///
/// **セルの長さ = ループ長**なので、素材の実長 (端数拍) のままだとループが小節から
/// ずれて曲と合わない。いちばん近い小節数へ丸め、その長さへ time-stretch する
/// (`source_*_frames` は動かさないので中身は全部鳴る)。端数が無ければ `Raw` の
/// まま無加工。アレンジのレーンへ落としたときは実長のまま (従来どおり)。
#[must_use]
fn cell_place_len(
    into_cell: bool,
    natural_beats: f64,
    time_sig: (u8, u8),
) -> (f64, common::model::StretchMode) {
    if !into_cell {
        return (natural_beats, common::model::StretchMode::Raw);
    }
    let fit = fit_to_bars(natural_beats, time_sig);
    let mode = if (fit - natural_beats).abs() > 1e-6 {
        common::model::StretchMode::Stretch
    } else {
        common::model::StretchMode::Raw
    };
    (fit, mode)
}

/// 素材の長さ (拍) を **いちばん近い小節数** に丸める (最低 1 小節)。
///
/// ランチャーのセルはこの長さでループするので、端数拍のままだと曲の小節と
/// ずれ続ける。丸めた長さへ time-stretch して「撃てばそのまま合う」状態にする。
#[must_use]
fn fit_to_bars(natural_beats: f64, time_sig: (u8, u8)) -> f64 {
    let bar = common::model::beats_per_bar(time_sig);
    if !natural_beats.is_finite() || natural_beats <= 0.0 || bar <= 0.0 {
        return bar.max(1.0);
    }
    let bars = (natural_beats / bar).round().max(1.0);
    bars * bar
}

/// 取り込んだクリップを行へ置く **唯一の口**。
///
/// `cell_scene` が `Some` ならランチャーのセルとして置く (`start_beat` は捨てて
/// 0 — セルは「撃った瞬間」を原点にする契約)。置き換えは `put_session_clip` を
/// 通すので、**鳴っているセルの上に落としても主導権が新しい id へ移り、音が
/// 止まらない**。取り込み経路 (オーディオ / 画像 / MIDI) は全部ここを通すこと —
/// 経路ごとに `clips.push` を手写しすると、その経路だけセルに落ちない。
pub(crate) fn place_imported_clip(
    track: &mut common::model::Track,
    cell_scene: Option<u32>,
    clip: Clip,
) {
    match cell_scene {
        Some(scene_id) => track.put_session_clip(common::model::SessionClip {
            scene_id,
            clip: Clip { start_beat: 0.0, ..clip },
            launch: common::model::LaunchSettings::default(),
        }),
        None => track.clips.push(clip),
    }
}

/// 取り込み先の列 (表示順 index)。ランチャーのセル / 新規トラック行への drop の
/// ときだけ `Some`。`offset` は同じ drop 内の 2 件目以降を右の列へ送る量。
pub(crate) fn import_cell_index(target: ImportTrackTarget, offset: usize) -> Option<usize> {
    match target {
        ImportTrackTarget::LauncherCell { scene_index, .. }
        | ImportTrackTarget::LauncherNewTrack { scene_index } => {
            Some(scene_index as usize + offset)
        }
        _ => None,
    }
}
