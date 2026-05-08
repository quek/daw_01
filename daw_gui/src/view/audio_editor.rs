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
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect};

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

    // ----- Multi-event 対応 (Phase 2 PR-D 段階 1) ---------------------
    // events.iter().enumerate() で各 event ごとに rect を分割描画。
    // event の rect = wf_area を `(event_start / clip.length_beats)` 〜
    // `((event_start + event_length) / clip.length_beats)` で割り当てる。
    // 選択中 event (= `audio_editor_selected_event`、 None なら 0 を
    // default) は border highlight。 Click 検出 / drag は段階 2 以降。
    if audio.events.is_empty() {
        ui.label_at(
            "audio_editor_no_event",
            "(空の audio content — event がありません)",
            wf_area.x + 4.0,
            wf_area.y + 8.0,
            11.0,
            TEXT_DIM,
        );
        return;
    }
    let selected_idx = app.audio_editor_selected_event.unwrap_or(0);
    let clip_len_beats = clip.length_beats.max(1e-6); // 0 div 防御

    for (idx, event) in audio.events.iter().enumerate() {
        let Some(buffer) = app.audio_source_cache.get(event.source_id) else {
            // 当該 event は decode 待ち / missing source → 透けて見える
            // 範囲だけマーカー描画 (= 他 event は描く)。
            continue;
        };

        // event rect (clip 内の time range を wf_area 全幅にマップ)
        let evt_x_start = (event.event_start_in_clip_beats / clip_len_beats) as f32;
        let evt_x_end = ((event.event_start_in_clip_beats + event.event_length_beats)
            / clip_len_beats) as f32;
        let event_rect = Rect {
            x: wf_area.x + evt_x_start.clamp(0.0, 1.0) * wf_area.w,
            y: wf_area.y,
            w: ((evt_x_end - evt_x_start) * wf_area.w).max(2.0),
            h: wf_area.h,
        };

        let planes_borrowed: Vec<&[f32]> =
            buffer.samples.iter().map(Vec::as_slice).collect();
        let event_len_frames = event
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
            len_samples: event_len_frames,
            vertical_gain: 1.0,
        };
        let is_selected = idx == selected_idx;
        let fg = if is_selected {
            Color::rgba(0.65, 0.95, 1.0, 0.95)
        } else {
            Color::rgba(0.45, 0.70, 0.85, 0.85)
        };
        let style = WaveformStyle {
            fg,
            fg_clipped: Color::rgb(0.95, 0.45, 0.40),
            fill: None,
            baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.15)),
            channel_layout: ChannelLayout::Stack,
            render_mode: WaveformRenderMode::Auto,
            line_width_px: 1.0,
        };
        let _ = ui.waveform(
            ("audio_editor_wf", clip.id, idx),
            event_rect,
            source,
            view,
            style,
        );

        // Selection border (= 選択中のみ視認できる枠)。 1 px 太い線で
        // 上下左右を marker。 push_rect で半透明帯にしても良いが、
        // border の方が波形を遮らない。
        if is_selected {
            let border_color = Color::rgba(0.95, 0.78, 0.31, 0.85);
            ui.push_lines(LineBatch {
                segments: std::sync::Arc::from(vec![
                    // top
                    LineSegment {
                        a: [event_rect.x, event_rect.y],
                        b: [event_rect.x + event_rect.w, event_rect.y],
                        color: border_color,
                    },
                    // bottom
                    LineSegment {
                        a: [event_rect.x, event_rect.y + event_rect.h],
                        b: [event_rect.x + event_rect.w, event_rect.y + event_rect.h],
                        color: border_color,
                    },
                    // left
                    LineSegment {
                        a: [event_rect.x, event_rect.y],
                        b: [event_rect.x, event_rect.y + event_rect.h],
                        color: border_color,
                    },
                    // right
                    LineSegment {
                        a: [event_rect.x + event_rect.w, event_rect.y],
                        b: [event_rect.x + event_rect.w, event_rect.y + event_rect.h],
                        color: border_color,
                    },
                ]),
                line_width_px: 1.5,
                clip_rect: Some(wf_area),
            });
        }
    }

    // ----- Playhead 線 (Phase 2 PR7) -----
    // 再生中 / Stop 後 (= playhead_beat is Some) かつ playhead が現
    // clip の範囲内なら、 wf_area 上に縦線を重ねる。 視覚的に「曲全体の
    // どこを再生しているか」 が Audio Editor 内でも分かる。 view は
    // event.event_start_in_clip_beats から event_length_beats まで
    // (= 1 event = clip 全体) を全幅マッピングしているので、
    // x = wf_area.x + (in_clip_beats / clip.length_beats) * wf_area.w。
    if let Some(ph_beat) = app.playhead_beat {
        let ph_beat = ph_beat as f64;
        let clip_start = clip.start_beat;
        let clip_end = clip_start + clip.length_beats;
        if ph_beat >= clip_start && ph_beat < clip_end && clip.length_beats > 0.0 {
            let in_clip = ph_beat - clip_start;
            let x = wf_area.x + (in_clip / clip.length_beats) as f32 * wf_area.w;
            let color = Color::rgba(1.0, 0.55, 0.20, 0.9);
            ui.push_lines(LineBatch {
                segments: std::sync::Arc::from(vec![LineSegment {
                    a: [x, wf_area.y],
                    b: [x, wf_area.y + wf_area.h],
                    color,
                }]),
                line_width_px: 1.5,
                clip_rect: Some(wf_area),
            });
        }
    }

    // Source meta (= 選択中 event の source 情報)。 Phase 4+ で
    // Inspector の Source 行 (`docs/plan_audio_clip.md` §3.9) として
    // 出す予定だが、 当面は Audio Editor 内 footer に出して視認性を
    // 確保する。 multi-event のときは選択中 event の source を参照。
    let footer_event = audio.events.get(selected_idx).or(audio.events.first());
    if let Some(footer_event) = footer_event
        && let Some(audio_source) = app.song.audio_sources.get(&footer_event.source_id)
    {
        let meta = format!(
            "{}  {} Hz · {} ch · {} frames  ({} events)",
            match &audio_source.path {
                common::model::AudioSourcePath::ProjectRelative(p) => p.display().to_string(),
                common::model::AudioSourcePath::Absolute(p) => p.display().to_string(),
                common::model::AudioSourcePath::Generated { id } => format!("Generated #{id}"),
            },
            audio_source.sample_rate,
            audio_source.channels,
            audio_source.frames,
            audio.events.len(),
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
