//! Audio Editor (Phase 2 PR6 minimal viewer)。
//!
//! audio clip ダブルクリックで bottom_panel の Piano Roll タブの代わりに
//! 表示される波形 view。 `docs/plan_audio_clip.md` §3.10 の Audio Editor。
//!
//! Phase 2 PR6 はミニマル版 (read-only viewer):
//! - clip 内の first event の波形を全幅で描画 (gui_01 `Ui::waveform`)
//! - clip 名 + length / source 情報のヘッダ
//! - 「閉じる」 ボタン (`AppEvent::CloseAudioEditor`)
//! - Esc shortcut で閉じる (= `view/shortcuts.rs` で wire)
//!
//! event 単位の trim / 移動 / 追加 / 削除、 dB handle、 fade 角 drag 等の
//! 編集機能は後続 PR (PR8 以降) で。 編集はそれまで Inspector 経由
//! (Phase 2 PR1-3)。

use daw_ui_core::{
    ChannelLayout, Edit, SampleSlices, Ui, WaveformRenderMode, WaveformSource,
    WaveformStyle, WaveformView,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const BG: Color = Color { r: 0.10, g: 0.11, b: 0.13, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.62, g: 0.65, b: 0.70, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("audio_editor_bg", area, BG, 0.0);

    // 開いている clip を解決。 audio_editor_clip がセットされていない、
    // または範囲外 / 非 audio なら placeholder を出して return (= clip
    // が削除された / Undo で消えた場合の防御)。
    let target = match app.audio_editor_clip {
        Some(t) => t,
        None => {
            ui.label_at(
                "audio_editor_empty",
                "(Audio Editor: 表示する clip が選択されていません)",
                area.x + 12.0,
                area.y + 18.0,
                12.0,
                TEXT_DIM,
            );
            return;
        }
    };
    let Some(track) = app.song.tracks.get(target.track as usize) else {
        return;
    };
    let Some(clip) = track.clips.get(target.clip as usize) else {
        return;
    };
    let Some(common::model::ClipContent::Audio(audio)) =
        app.song.clip_contents.get(&clip.content_id)
    else {
        return;
    };

    // ----- Header (clip 名 + length + close button) -------------------
    let pad = 12.0;
    let header_h = 24.0;
    ui.label_at(
        "audio_editor_title",
        &format!(
            "Audio Editor — {} ({:.2} beats)",
            clip.name,
            clip.length_beats
        ),
        area.x + pad,
        area.y + 6.0,
        14.0,
        TEXT,
    );
    let close_w = 60.0;
    let close_rect = Rect {
        x: area.x + area.w - pad - close_w,
        y: area.y + 4.0,
        w: close_w,
        h: header_h,
    };
    ui.button_at("audio_editor_close", "Close", close_rect, || {
        Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseAudioEditor))
    });

    // ----- Waveform area --------------------------------------------
    let wf_area = Rect {
        x: area.x + pad,
        y: area.y + header_h + 12.0,
        w: (area.w - pad * 2.0).max(0.0),
        h: (area.h - header_h - 24.0).max(0.0),
    };
    if wf_area.w <= 0.0 || wf_area.h <= 0.0 {
        return;
    }

    // 1 clip = 1 event 前提 (Phase 2 PR6 minimal)、 first event を表示。
    // multi-event は後続 PR で event ごとに rect を分割描画する。
    let Some(event) = audio.events.first() else {
        ui.label_at(
            "audio_editor_no_event",
            "(空の audio content — event がありません)",
            wf_area.x + 4.0,
            wf_area.y + 8.0,
            11.0,
            TEXT_DIM,
        );
        return;
    };
    let Some(buffer) = app.audio_source_cache.get(event.source_id) else {
        ui.label_at(
            "audio_editor_no_buffer",
            "(decode 待ち / missing source)",
            wf_area.x + 4.0,
            wf_area.y + 8.0,
            11.0,
            TEXT_DIM,
        );
        return;
    };

    // SampleSlices::Planar は &[&[f32]] を要求。 GUI 描画 path なので
    // 毎フレーム alloc は許容 (RT path ではない)。
    let planes_borrowed: Vec<&[f32]> = buffer.samples.iter().map(Vec::as_slice).collect();
    let event_len = event
        .source_end_frames
        .saturating_sub(event.source_start_frames)
        .max(1);

    let source = WaveformSource {
        samples: SampleSlices::Planar(&planes_borrowed),
        valid_len: buffer.frames as usize,
        generation: event.source_id as u64,
        sample_rate: buffer.sample_rate,
    };
    let view = WaveformView {
        start_sample: event.source_start_frames,
        len_samples: event_len,
        vertical_gain: 1.0,
    };
    let style = WaveformStyle {
        fg: Color::rgba(0.55, 0.85, 0.95, 0.95),
        fg_clipped: Color::rgb(0.95, 0.45, 0.40),
        fill: None,
        baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.18)),
        // multi-channel のとき左右に振り分けて描く (mono は overlay と
        // 同等)。 arrangement の mini 波形は Overlay にしてあるが、
        // Audio Editor では縦軸が広いので Stacked にして source の
        // channel 構造が見えるようにする。
        channel_layout: ChannelLayout::Stack,
        render_mode: WaveformRenderMode::Auto,
        line_width_px: 1.0,
    };
    let _ = ui.waveform(
        ("audio_editor_wf", clip.id),
        wf_area,
        source,
        view,
        style,
    );

    // Source meta (debug-ish ヘルパー: source ファイル / sample_rate /
    // channels)。 Phase 4+ で source メタ情報を Inspector 側にも出す
    // (`docs/plan_audio_clip.md` §3.9 Source 行)。
    if let Some(audio_source) = app.song.audio_sources.get(&event.source_id) {
        let meta = format!(
            "{}  {} Hz · {} ch · {} frames",
            match &audio_source.path {
                common::model::AudioSourcePath::ProjectRelative(p) => p.display().to_string(),
                common::model::AudioSourcePath::Absolute(p) => p.display().to_string(),
                common::model::AudioSourcePath::Generated { id } => format!("Generated #{id}"),
            },
            audio_source.sample_rate,
            audio_source.channels,
            audio_source.frames,
        );
        ui.label_at(
            "audio_editor_meta",
            &meta,
            area.x + pad,
            area.y + area.h - 18.0,
            10.0,
            TEXT_DIM,
        );
    }
}
