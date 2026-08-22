// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #31: ファイルを「トラック一覧の下の空きスペース」にドロップしたときは、
//! 一番上ではなく **一番下** に新規トラックを作ってクリップを貼る
//! (`ImportTrackTarget::NewTrackBottom`)。
//!
//! headless: 実 D&D の OS イベント (winit `DroppedFile`) は駆動できないので、
//! arrangement view が drop 位置から解決する `ImportTrackTarget` を直接
//! `AppData::handle_event` に流し、下流 (track 生成 + clip 配置) を検証する。
//! import は同期 decode なので assert は dispatch 直後に確定する。
//! import_cache のグローバル汚染を避けるため file_path を temp project に向ける。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, ImportTrackTarget};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

use hound::{SampleFormat, WavSpec, WavWriter};
use image::{ImageBuffer, Rgba};
use tempfile::TempDir;

/// headless な `AppData` + import 先の temp project。 TempDir は返して生かして
/// おく (drop すると samples/ の書き込み先が消える)。 既定 Song は "Track 1" を
/// 1 本 (clip 0 個) 持つ。
fn build_app() -> (
    AppData,
    TempDir,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher;
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    let proj = TempDir::new().unwrap();
    // project_dir = file_path.parent() を temp にして import 物を temp/samples へ。
    app.song_doc.file_path = Some(proj.path().join("proj.daw"));
    (app, proj, audio_rx, plugin_rx)
}

fn write_wav(dir: &Path, name: &str, frames: usize) -> PathBuf {
    let path = dir.join(name);
    let spec = WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(&path, spec).unwrap();
    for i in 0..frames {
        w.write_sample((i % 100) as i16).unwrap();
    }
    w.finalize().unwrap();
    path
}

fn write_png(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_fn(8, 8, |x, y| Rgba([(x * 32) as u8, (y * 32) as u8, 128, 255]));
    img.save(&path).unwrap();
    path
}

/// 空きスペース drop (audio) → 一番下に新規 track、既存 (先頭) track は無変化。
#[test]
fn audio_drop_in_empty_space_creates_track_at_bottom() {
    let (mut app, src, _a, _p) = build_app();
    let wav = write_wav(src.path(), "kick.wav", 4800);
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportAudio {
        paths: vec![wav],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(4.0),
    });

    let tracks = &app.song_doc.song().tracks;
    assert_eq!(tracks.len(), before + 1, "一番下に新規 track が 1 本増える");
    let bottom = tracks.last().unwrap();
    assert_eq!(bottom.clips.len(), 1, "末尾 track に audio clip が 1 個");
    assert!(
        (bottom.clips[0].start_beat - 4.0).abs() < 1e-6,
        "clip は drop beat (4.0) に置かれる"
    );
    assert!(
        tracks[0].clips.is_empty(),
        "既存の先頭 track は変化なし (一番上に貼っていない)"
    );
}

/// 複数ファイルの空きスペース drop (audio) → 新規 track は **1 本**、その 1 本に
/// 全 clip を順送りで積む (singular「トラックを追加」)。
#[test]
fn audio_multi_drop_stacks_on_single_bottom_track() {
    let (mut app, src, _a, _p) = build_app();
    let w1 = write_wav(src.path(), "a.wav", 4800);
    let w2 = write_wav(src.path(), "b.wav", 2400);
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportAudio {
        paths: vec![w1, w2],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });

    let tracks = &app.song_doc.song().tracks;
    assert_eq!(tracks.len(), before + 1, "複数ファイルでも新規 track は 1 本だけ");
    assert_eq!(
        tracks.last().unwrap().clips.len(),
        2,
        "2 clip が同じ末尾 track に積まれる"
    );
}

/// 既存 track への drop (audio) → その track に貼るだけ。新規 track は作らない。
#[test]
fn audio_drop_on_existing_track_adds_there() {
    let (mut app, src, _a, _p) = build_app();
    let wav = write_wav(src.path(), "kick.wav", 4800);
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportAudio {
        paths: vec![wav],
        target: ImportTrackTarget::Track(0),
        target_beat: Some(2.0),
    });

    let tracks = &app.song_doc.song().tracks;
    assert_eq!(tracks.len(), before, "既存 track drop は track を増やさない");
    assert_eq!(tracks[0].clips.len(), 1, "指定した既存 track に clip が付く");
}

/// 空きスペース drop (image) → 一番下 (末尾 push) に新規 track。#31 の核心:
/// 以前の `insert(0)` (一番上) ではなく末尾に入り、先頭 track は無変化。
#[test]
fn image_drop_in_empty_space_creates_track_at_bottom_not_top() {
    let (mut app, src, _a, _p) = build_app();
    let png = write_png(src.path(), "pic.png");
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportImage {
        paths: vec![png],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(1.0),
    });

    let tracks = &app.song_doc.song().tracks;
    assert_eq!(tracks.len(), before + 1, "新規 image track が 1 本増える");
    assert_eq!(
        tracks.last().unwrap().clips.len(),
        1,
        "末尾 (= 一番下) track に image clip が入る"
    );
    assert!(
        tracks[0].clips.is_empty(),
        "先頭 track は無変化 (index 0 への top-insert をしていない)"
    );
}
