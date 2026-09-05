//! 下部パネル「Sampler」タブ (`docs/plan_global_sampler.md` §3.3)。
//!
//! 常にリング全体を表示する (右端 = 今、Q8 でスクロール / ズーム無し)。
//! - ヘッダ: 録音源 / 長さ (秒) / 一時停止 / 試聴 / 選択長。
//! - 本体: 波形オーバービュー + 再生していた区間の小節線 + 秒目盛 + 選択範囲。
//! - 左ドラッグで範囲選択。選択の上で押してドラッグすると daw-ui の drag payload
//!   ([`SAMPLER_DRAG_KIND`]) で持ち出し、アレンジ / セルが受ける
//!   (`arrangement_view::take_capture_drops`)。
//!
//! 時間軸の写像は [`RingAxis`] 1 本 (描画 / 当たり判定 / drop すべて同じ)。

use std::sync::Arc;

use daw_ui_core::{DragKind, Edit, Ui};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::event_sampler::SamplerEvent;
use crate::state::midi_capture::{MIDI_CAPTURE_DRAG_KIND, MidiCaptureDragPayload};
use crate::state::sampler::{
    BUCKET_FRAMES, RingAxis, SAMPLER_DRAG_KIND, SamplerDragPayload, segment_spans,
};

pub(crate) const HEADER_H: f32 = 30.0;
const PAD: f32 = 6.0;
const FONT: f32 = 12.0;
/// 秒目盛の帯 (本体の下端)。
const RULER_H: f32 = 14.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let p = &app.theme.core;
    ui.panel("sampler_bg", area, p.panel, 0.0);
    let header = Rect { x: area.x, y: area.y, w: area.w, h: HEADER_H };
    let body = Rect {
        x: area.x + PAD,
        y: area.y + HEADER_H,
        w: (area.w - PAD * 2.0).max(1.0),
        h: (area.h - HEADER_H - PAD).max(1.0),
    };
    draw_header(app, ui, header);

    let st = &app.sampler;
    let axis = RingAxis {
        x: body.x,
        w: body.w,
        write_frames: st.write_frames,
        capacity: st.capacity(),
    };
    let wave = Rect { x: body.x, y: body.y, w: body.w, h: (body.h - RULER_H).max(1.0) };
    ui.push_rect(RectCommand {
        rect: body,
        fill: p.inset_bg,
        border: p.border,
        border_width: 1.0,
        radius: [3.0; 4],
        clip_rect: None,
    });
    if st.ring.is_none() {
        ui.label_at("sampler_none", "音声エンジンに接続していません", body.x + 8.0, body.y + 8.0, FONT, p.text_dim);
        return;
    }
    let sr = st.sample_rate();
    draw_bar_lines(app, ui, wave, |frame| axis.frame_to_x(frame), &sampler_bar_source(app, sr));
    draw_seconds_ruler(app, ui, body, RULER_H, |secs_ago| {
        axis.frame_to_x(st.write_frames.saturating_sub((secs_ago * f64::from(sr)) as u64))
    }, st.capacity() as f64 / f64::from(sr.max(1)));
    draw_waveform(app, ui, wave, &axis);

    // ---- 選択 / 持ち出し ----
    let sel_rect = st.selection.map(|(s, e)| Rect {
        x: axis.frame_to_x(s),
        y: wave.y,
        w: (axis.frame_to_x(e) - axis.frame_to_x(s)).max(1.0),
        h: wave.h,
    });
    if let Some(r) = sel_rect {
        ui.push_rect(RectCommand {
            rect: r,
            fill: p.accent_wash,
            border: p.accent,
            border_width: 1.0,
            radius: [0.0; 4],
            clip_rect: Some(wave),
        });
    }
    let pointer = ui.pointer();
    let press_in_sel = pointer.primary_just_pressed
        && pointer.pos.is_some_and(|(px, py)| sel_rect.is_some_and(|r| r.contains(px, py)));
    if press_in_sel {
        if let (Some(_), Some((s, e))) = (ui.take_primary_press_in_rect(wave), st.selection) {
            ui.begin_drag(SAMPLER_DRAG_KIND, SamplerDragPayload { start_frame: s, end_frame: e });
        }
    } else if let Some(d) = ui.take_drag_in_rect("sampler_select", wave) {
        let a = axis.x_to_frame(d.anchor.0);
        let b = axis.x_to_frame(d.current.0);
        let sel = if d.kind == DragKind::Released && (d.current.0 - d.anchor.0).abs() < 2.0 {
            None
        } else {
            Some((a.min(b), a.max(b)))
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::SetSelection(sel)));
        }));
    }
}

/// ヘッダ行: 録音源 / 長さ / 一時停止 / 試聴 / 選択長。
fn draw_header(app: &AppData, ui: &mut Ui<'_, AppData>, header: Rect) {
    let p = &app.theme.core;
    let y = header.y + (HEADER_H - 22.0) / 2.0;
    let mut x = header.x + PAD;

    // 録音源 (Master / 各 track × 3 tap)。
    let song = app.song_doc.song();
    let mut items: Vec<String> = vec!["Master".to_string()];
    let mut sources: Vec<common::protocol::SamplerSource> = vec![common::protocol::SamplerSource::Master];
    for t in &song.tracks {
        for tp in [
            common::model::TapPoint::PreFx,
            common::model::TapPoint::PostFx,
            common::model::TapPoint::PostFader,
        ] {
            items.push(format!("{} · {}", t.name, crate::handler::sampler::tap_point_label(tp)));
            sources.push(common::protocol::SamplerSource::Track(common::model::AudioTap {
                source_track: t.id,
                tap_point: tp,
            }));
        }
    }
    let selected = sources.iter().position(|s| *s == app.sampler.source).unwrap_or(0);
    let item_refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let dd = Rect { x, y, w: 220.0, h: 22.0 };
    if let Some(i) = ui.dropdown_with_font("sampler_source", dd, &item_refs, selected, FONT)
        && let Some(src) = sources.get(i).copied()
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::SetSource(src)));
        }));
    }
    x += dd.w + PAD * 2.0;

    x = seconds_field(app, ui, x, y, "sampler_secs");
    x = pause_and_preview(
        ui,
        p,
        x,
        y,
        ("sampler_pause", app.sampler.paused, AppEvent::Sampler(SamplerEvent::TogglePaused)),
        ("sampler_preview", app.sampler.preview_until.is_some(), app.sampler.selection.is_some(), AppEvent::Sampler(SamplerEvent::TogglePreview)),
    );

    if let Some((s, e)) = app.sampler.selection {
        let secs = (e - s) as f64 / f64::from(app.sampler.sample_rate().max(1));
        ui.label_at("sampler_sel_len", &format!("選択 {secs:.2} s — アレンジ / セルへドラッグ"), x, y + 5.0, FONT, p.text_dim);
    } else {
        ui.label_at("sampler_hint", "ドラッグで範囲を選ぶ", x, y + 5.0, FONT, p.text_dim);
    }
}

/// 「長さ (秒)」の数値欄。Sampler / MIDI Capture 両タブで同じ SSoT
/// (`UiPrefs::sampler_seconds`) を編集する。戻りは次の x。
pub(crate) fn seconds_field(app: &AppData, ui: &mut Ui<'_, AppData>, x: f32, y: f32, id: &'static str) -> f32 {
    use daw_ui_core::{ScrubableNumberFormat, ScrubableNumberStyle};
    let p = &app.theme.core;
    ui.label_at((id, "label"), "長さ", x, y + 5.0, FONT, p.text_dim);
    let field = Rect { x: x + 32.0, y, w: 64.0, h: 22.0 };
    let pending = std::cell::Cell::new(None::<f64>);
    let resp = ui.scrubable_number_at(
        id,
        field,
        f64::from(app.ui_prefs.sampler_seconds),
        f64::from(common::sampler_ring::DEFAULT_SECONDS),
        ScrubableNumberFormat::Integer,
        &ScrubableNumberStyle {
            font_size: FONT,
            sensitivity: 0.5,
            range: Some((1.0, f64::from(common::sampler_ring::MAX_SECONDS))),
            ..ScrubableNumberStyle::from_palette(p)
        },
        |v| {
            pending.set(Some(v));
            Edit::mutate(|_: &mut AppData| {})
        },
        None,
        None,
    );
    let active = resp.dragging || resp.editing_text;
    if let Some(v) = pending.get() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::SetSeconds { seconds: v.round() as u32, commit: !active }));
        }));
    }
    // ドラッグ / 入力の立ち下がりで確定 (設定画面の VOICEVOX 塊長と同じ edge 検出)。
    if active != app.ui_ephemeral.sampler_secs_editing {
        let live = resp.displayed_value.round() as u32;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.sampler_secs_editing = active;
            if !active {
                app.handle_event(AppEvent::Sampler(SamplerEvent::SetSeconds { seconds: live, commit: true }));
            }
        }));
    }
    ui.label_at((id, "unit"), "秒", field.x + field.w + 4.0, y + 5.0, FONT, p.text_dim);
    field.x + field.w + 4.0 + 16.0 + PAD * 2.0
}

/// 一時停止 / 試聴の 2 トグル。戻りは次の x。
pub(crate) fn pause_and_preview(
    ui: &mut Ui<'_, AppData>,
    p: &daw_ui_core::theme::Palette,
    x: f32,
    y: f32,
    pause: (&'static str, bool, AppEvent),
    preview: (&'static str, bool, bool, AppEvent),
) -> f32 {
    use daw_ui_core::ToggleButtonStyle;
    let style = ToggleButtonStyle { font_size: FONT, radius: 3.0, ..ToggleButtonStyle::from_palette(p) };
    let (pid, paused, pev) = pause;
    let r = Rect { x, y, w: 72.0, h: 22.0 };
    ui.toggle_button_at(pid, "一時停止", r, paused, &style, move |_| {
        Edit::mutate(move |app: &mut AppData| app.handle_event(pev.clone()))
    });
    let (vid, previewing, enabled, vev) = preview;
    let r2 = Rect { x: r.x + r.w + PAD, y, w: 56.0, h: 22.0 };
    if enabled {
        ui.toggle_button_at(vid, "試聴", r2, previewing, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| app.handle_event(vev.clone()))
        });
    } else {
        ui.push_rect(RectCommand {
            rect: r2,
            fill: p.control,
            border: p.border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
        ui.label_at((vid, "disabled"), "試聴", r2.x + 14.0, y + 5.0, FONT, p.text_faint);
    }
    r2.x + r2.w + PAD * 2.0
}

/// 波形オーバービュー: 1 px 列ごとにそこへ落ちるバケツの min/max を縦線で描く。
fn draw_waveform(app: &AppData, ui: &mut Ui<'_, AppData>, wave: Rect, axis: &RingAxis) {
    let p = &app.theme.core;
    let st = &app.sampler;
    if axis.capacity == 0 || wave.w < 1.0 {
        return;
    }
    let ink = p.waveform_for(p.inset_bg, daw_ui_core::theme::WaveformInk::Normal);
    let mid = wave.y + wave.h / 2.0;
    let half = wave.h / 2.0 - 1.0;
    let cols = wave.w.floor() as usize;
    let frames_per_px = axis.capacity as f64 / wave.w as f64;
    let oldest = axis.oldest();
    let mut segs: Vec<LineSegment> = Vec::with_capacity(cols);
    for c in 0..cols {
        let f0 = oldest + (c as f64 * frames_per_px) as u64;
        let f1 = oldest + ((c + 1) as f64 * frames_per_px) as u64;
        let (b0, b1) = (f0 / BUCKET_FRAMES, (f1.max(f0 + 1) - 1) / BUCKET_FRAMES + 1);
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        for b in b0..b1 {
            let (mn, mx) = st.overview.get(b);
            lo = lo.min(mn);
            hi = hi.max(mx);
        }
        let x = wave.x + c as f32 + 0.5;
        segs.push(LineSegment {
            a: [x, mid - hi.clamp(-1.0, 1.0) * half],
            b: [x, mid - lo.clamp(-1.0, 1.0) * half],
            color: ink,
        });
    }
    ui.push_lines(LineBatch { segments: Arc::from(segs), line_width_px: 1.0, clip_rect: Some(wave) });
    // 一時停止中は右端に帯を出す (波形が流れないことの明示)。
    if st.paused {
        let r = Rect { x: wave.x + wave.w - 90.0, y: wave.y + 4.0, w: 84.0, h: 18.0 };
        ui.push_rect(RectCommand { rect: r, fill: app.theme.daw.record.with_alpha(0.85), border: Color::TRANSPARENT, border_width: 0.0, radius: [3.0; 4], clip_rect: None });
        ui.label_at("sampler_paused_badge", "一時停止中", r.x + 10.0, r.y + 3.0, FONT, p.ink_for(app.theme.daw.record));
    }
}

/// 小節線の供給元: 「x 座標 → 曲位置 (samples)」を区間ごとに解く。
pub(crate) struct BarSource {
    /// `(区間の始点 frame/ns, 終点, その始点の曲位置 samples, bpm)`。
    pub spans: Vec<(u64, u64, u64)>,
    /// 区間内の単位 (frame なら sample_rate、ns なら 1e9) あたりの samples。
    pub samples_per_unit: f64,
}

/// Sampler タブ用: セグメントをリング frame 座標のまま使う。
fn sampler_bar_source(app: &AppData, _sr: u32) -> BarSource {
    let st = &app.sampler;
    let spans = segment_spans(&st.segments, st.write_frames)
        .into_iter()
        .filter_map(|(s, e, seg)| seg.playhead_samples.map(|ph| (s, e, ph)))
        .collect();
    BarSource { spans, samples_per_unit: 1.0 }
}

/// 再生していた区間に小節線 + 小節番号を重ねる。`to_x` は区間の単位 (frame / ns) → x。
pub(crate) fn draw_bar_lines(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    wave: Rect,
    to_x: impl Fn(u64) -> f32,
    src: &BarSource,
) {
    let p = &app.theme.core;
    let song = app.song_doc.song();
    let sr = app.ipc.sample_rate.max(1);
    let tempo = common::tempo_map::TempoMap::from_song(song);
    let bar = common::model::beats_per_bar(song.time_sig).max(1e-6);
    let mut lines: Vec<LineSegment> = Vec::new();
    let mut labels: Vec<(f32, String)> = Vec::new();
    for &(start, end, ph0) in &src.spans {
        // 区間の薄い帯 (= 再生していた)。
        let (x0, x1) = (to_x(start), to_x(end));
        ui.push_rect(RectCommand {
            rect: Rect { x: x0, y: wave.y, w: (x1 - x0).max(0.0), h: wave.h },
            fill: p.accent.with_alpha(0.06),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(wave),
        });
        let len_samples = ((end - start) as f64 * src.samples_per_unit) as u64;
        let beat0 = tempo.samples_to_beat(ph0, sr);
        let beat1 = tempo.samples_to_beat(ph0 + len_samples, sr);
        let first_bar = (beat0 / bar).ceil() as i64;
        let last_bar = (beat1 / bar).floor() as i64;
        // 詰まりすぎたら間引く (ラベルは 40px 以上空いたときだけ)。
        let mut last_label_x = f32::MIN;
        for k in first_bar..=last_bar {
            let beat = k as f64 * bar;
            let s = tempo.beat_to_samples(beat, sr);
            if s < ph0 {
                continue;
            }
            let unit = ((s - ph0) as f64 / src.samples_per_unit) as u64;
            let x = to_x(start + unit);
            if x < wave.x || x > wave.x + wave.w {
                continue;
            }
            lines.push(LineSegment { a: [x, wave.y], b: [x, wave.y + wave.h], color: p.border_hover });
            if x - last_label_x >= 40.0 {
                labels.push((x, format!("{}", k + 1)));
                last_label_x = x;
            }
        }
    }
    if !lines.is_empty() {
        ui.push_lines(LineBatch { segments: Arc::from(lines), line_width_px: 1.0, clip_rect: Some(wave) });
    }
    for (i, (x, text)) in labels.into_iter().enumerate() {
        ui.label_at(("sampler_bar", i), &text, x + 2.0, wave.y + 2.0, 10.0, p.text_dim);
    }
}

/// 下端の秒目盛 (「-10s」…「0」)。`to_x(secs_ago)`。
pub(crate) fn draw_seconds_ruler(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    body: Rect,
    ruler_h: f32,
    to_x: impl Fn(f64) -> f32,
    span_secs: f64,
) {
    let p = &app.theme.core;
    let ruler = Rect { x: body.x, y: body.y + body.h - ruler_h, w: body.w, h: ruler_h };
    ui.push_rect(RectCommand { rect: ruler, fill: p.header, border: Color::TRANSPARENT, border_width: 0.0, radius: [0.0; 4], clip_rect: None });
    if span_secs <= 0.0 || body.w < 1.0 {
        return;
    }
    // 目盛間隔: 60px 以上空く最小の「きりのいい秒数」。
    let px_per_sec = body.w as f64 / span_secs;
    let step = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0]
        .into_iter()
        .find(|s| s * px_per_sec >= 60.0)
        .unwrap_or(600.0);
    let mut lines = Vec::new();
    let mut k = 0.0;
    let mut i = 0;
    while k <= span_secs {
        let x = to_x(k);
        lines.push(LineSegment { a: [x, ruler.y], b: [x, ruler.y + 4.0], color: p.text_dim });
        let text = if k == 0.0 { "0".to_string() } else { format!("-{k:.0}s") };
        let tw = ui.measure_text(&text, 10.0);
        ui.label_at(("sampler_ruler", i), &text, (x - tw - 2.0).max(ruler.x), ruler.y + 2.0, 10.0, p.text_dim);
        k += step;
        i += 1;
    }
    ui.push_lines(LineBatch { segments: Arc::from(lines), line_width_px: 1.0, clip_rect: Some(ruler) });
}

/// 運搬中の範囲を示すチップ (root が最後に描く = 常に最前面)。背景に依存しない
/// 暗いチップ + 明るい文字でコントラストを保証する (device drag と同じ idiom)。
pub fn draw_drag_chip(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let label = if let Some(p) = ui.drag_payload::<SamplerDragPayload>(SAMPLER_DRAG_KIND) {
        let secs = p.end_frame.saturating_sub(p.start_frame) as f64 / f64::from(app.sampler.sample_rate().max(1));
        format!("Sampler {secs:.2} s")
    } else if let Some(p) = ui.drag_payload::<MidiCaptureDragPayload>(MIDI_CAPTURE_DRAG_KIND) {
        let secs = p.end_ns.saturating_sub(p.start_ns) as f64 / 1e9;
        format!("MIDI {secs:.2} s")
    } else {
        return;
    };
    let Some((px, py)) = ui.pointer().pos else { return };
    let core = &app.theme.core;
    let chip = Rect { x: px + 12.0, y: py + 12.0, w: 130.0, h: 22.0 };
    ui.panel_with_border("capture_drag_chip", chip, core.panel_raised, core.accent, 1.0, 3.0);
    ui.label_at("capture_drag_label", &label, chip.x + 8.0, chip.y + 5.0, 11.0, core.text);
}
