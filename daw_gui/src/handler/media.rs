//! handler::media — audio/video/image/text の import
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
    /// (multi-select、 WAV filter) を開いて、 選択された path を
    /// `action_import_audio` に転送するだけのラッパ。 dialog をキャンセル
    /// した場合は no-op。 起点が違うだけで採番 / dedup / コピー / decode
    /// は drag&drop と完全に同じ pipeline。
    pub(crate) fn action_open_import_audio_dialog(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
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
        target: ClipRef,
        position_in_clip_beats: f64,
    ) {
        let dialog = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
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
        target_track_idx: Option<u32>,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        let project_dir: Option<PathBuf> = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // 引数 `target_track_idx` (= drag&drop の drop 位置から arrangement
        // view が計算) を最優先、 None なら cursor_track_index にフォール
        // バック (= File menu / dialog 経由)、 さらに無いときは 0。 範囲外
        // (= track 数を超える) 値は最後の track に clamp。
        let n_tracks = self.song_doc.song().tracks.len();
        let target_track_idx: usize = target_track_idx
            .map(|i| (i as usize).min(n_tracks.saturating_sub(1)))
            .or_else(|| self.cursor_track_index())
            .unwrap_or(0);
        // drag&drop の drop 位置 (`target_beat`) を最優先、 無ければ playhead。
        let start_beat_seed: f64 =
            target_beat.unwrap_or(self.transport.playhead_beat.unwrap_or(0.0) as f64);
        if self.song_doc.song().tracks.is_empty() {
            self.ui_ephemeral.status_message =
                "Audio import: 配置先のトラックが無いため取り込めません".to_string();
            return;
        }

        let bpm = self.song_doc.song().bpm;
        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut next_start_beat = start_beat_seed.max(0.0);

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

            let Some(source_id) = self.edit_song(|song| {
                let source_id = song.alloc_audio_source_id();
                song.media.audio_sources.insert(source_id, imported.source);

                // v29: 新規 content の単一 event なので id=1 / allocator は 2 から。
                let event = AudioEvent {
                    id: 1,
                    source_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: length_beats,
                    source_start_frames: 0,
                    source_end_frames: imported.buffer.frames,
                    ..AudioEvent::default()
                };
                let content_id = song.alloc_content(
                    ClipContent::Audio(AudioContent {
                        events: vec![event],
                        next_event_id: 2,
                    }),
                    imported.display_name.clone(),
                );

                let track = &mut song.tracks[target_track_idx];
                let new_clip_id = track.alloc_clip_id();
                track.clips.push(Clip {
                    id: new_clip_id,
                    start_beat: next_start_beat,
                    length_beats,
                    content_id,
                    color: None,
                    auto_lipsync: false,
                    ..Default::default()
                });
                source_id
            }) else {
                return;
            };
            self.media.audio_source_cache.insert(source_id, imported.buffer.clone());
            next_start_beat += length_beats;
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
        target_track_idx: Option<u32>,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        let project_dir = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

        // drop 位置から計算した track index が既存 track を指していれば、その
        // track に画像 clip を貼り付ける (= ドロップしたトラックに追加)。 track の
        // 無い下の領域 (= 範囲外 index) や dialog 経由 (None) は従来どおり
        // arrangement 先頭 (index 0) に新規 track を作って貼る。
        let dest_track_idx: Option<usize> =
            resolve_image_drop_target(target_track_idx, self.song_doc.song().tracks.len());

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
                //    - 新規 track: arrangement 先頭 (index 0) に Video 用 track を
                //      作って挿入 → 既存 video layer の上に合成される
                //      (multi-track composite top-wins, plan_video §4 P7)。
                let place_idx = match dest_track_idx {
                    Some(idx) => idx,
                    None => {
                        let image_track_id = song.alloc_track_id();
                        song.tracks.insert(
                            0,
                            track_with(|t| {
                                t.id = image_track_id;
                                t.name = format!("{display_name} (Image)");
                            }),
                        );
                        0
                    }
                };
                let track = &mut song.tracks[place_idx];
                let i_clip_id = track.alloc_clip_id();
                track.clips.push(Clip {
                    id: i_clip_id,
                    start_beat: next_start_beat,
                    length_beats: image_clip_length_beats,
                    content_id: i_content_id,
                    color: None,
                    auto_lipsync: false,
                    ..Default::default()
                });
            });
            // 既存 track に複数枚貼るときだけ順送り。 新規 track 経路は各画像が
            // 自分の track を持つので beat 0 固定 (従来挙動)。
            if dest_track_idx.is_some() {
                next_start_beat += image_clip_length_beats;
            }
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
        let r = ClipRef {
            track: track_idx as u32,
            clip: new_clip_idx,
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

}
