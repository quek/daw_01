//! r.md #156: オーディオ入力を録っている間、録っているトラックのレーンに「録音中の区間」の帯と、その中の波形を出す。
//!
//! 帯は録音が始まった拍からプレイヘッドまで伸びる (`RecordingState::audio_rec_region`、Tick ごとに
//! `AppData::audio_rec_tick` が伸ばす)。録り終えてから engine の書き終わりが届いてクリップが置かれる
//! までの間も、帯はそのまま残る。行の縦位置は他の overlay と同じ `f.rows` 1 本から読む。
//!
//! 波形は engine が録りながら送ってくる山 (`LiveTakeWave`) を、いま録っている区間の頭から並べる。山が届くのは
//! プレイヘッドより少し遅れるので、帯の右端の手前までしか描かれない。録音中のテンポの変化は均して置く
//! (録り終えて置かれるクリップは正確な位置になる)。
//!
//! レーンには既存のクリップ (どんな色でもありうる) があるので、録音の赤の半透明の面に加えて、明暗 2 本の枠
//! (選択枠と同じ `push_selection_ring`) を重ねる — 赤いクリップや明るいクリップの上でも区間の端が読める。
//! 波形も同じ明暗の組で描く (明るい本体の上下に暗い縁)。

use common::audio_take::TAKE_PEAK_FRAMES;
use common::automation::beats_to_samples;

use super::super::launcher::layout::row_screen_top;
use super::super::*;

/// 帯の面の不透明度 (下のクリップと波形が透けて見える程度)。
const FILL_ALPHA: f32 = 0.35;
/// 波形の振れ幅 (行の高さに対する割合)。
const WAVE_HEIGHT: f32 = 0.9;

/// 波形の横の位置決め: 帯の左端が WAV の `frame0` で、右へ 1px ごとに `frames_per_px` 進む。
#[derive(Clone, Copy)]
struct WaveMap {
    frame0: f64,
    frames_per_px: f64,
}

pub(super) fn draw(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    let Some(region) = app.cur.recording.audio_rec_region.as_ref() else { return };
    let Some((start, end)) = region.span else { return };
    let wave = &region.wave;
    // いま録っている区間が届いていれば、帯の頭はその拍 (Tick の刻みより正確)。折り返した直後は、まだ前の周の
    // 区間のままのことがある — 頭が帯の終わりより先なら Tick の値を使う。
    let segment = wave.segment.filter(|s| s.start_beat <= end);
    let start = segment.map_or(start, |s| s.start_beat);
    let x_of = |beat: f64| f.lanes.x + ((beat - f.view.start_beat) / f.beat_per_px) as f32;
    // 録り始めの 1 フレームでも見えるよう、幅は 1px を下回らせない。
    let (x0, x1) = (x_of(start), x_of(end).max(x_of(start) + 1.0));
    if x1 < f.lanes.x || x0 > f.lanes.x + f.lanes.w {
        return;
    }
    let map = segment.map(|s| {
        let song = app.cur.song_doc.song();
        let sr = wave.sample_rate;
        let frames = beats_to_samples(song, sr, end).saturating_sub(beats_to_samples(song, sr, s.start_beat));
        WaveMap { frame0: s.file_frame as f64, frames_per_px: frames as f64 / f64::from(x1 - x0) }
    });
    let fill = f.style.armed_button.on_color.with_alpha(FILL_ALPHA);
    let recording_track = |key: ArrangementRowKey| match key {
        ArrangementRowKey::Track(id) if region.track_ids.contains(&id) => Some(id),
        _ => None,
    };
    ui.heavy(("arrangement_rec_region", &f.id), |hctx| {
        hctx.with_clip_rect(f.lanes, |hctx| {
            for row in f.rows.iter() {
                let Some(track_id) = recording_track(row.key) else { continue };
                let top = row_screen_top(f, row);
                if top + row.height < f.lanes.y || top > f.lanes.y + f.lanes.h {
                    continue;
                }
                let r = Rect { x: x0, y: top, w: x1 - x0, h: row.height };
                push_filled_rect(hctx, r, fill);
                let peaks = wave.peaks.iter().find(|(id, _)| *id == track_id).map(|(_, p)| p.as_slice());
                if let (Some(map), Some(peaks)) = (map, peaks) {
                    push_wave(hctx, f, r, map, peaks);
                }
                push_selection_ring(hctx, r, f.style, 0.0, None);
            }
        });
    });
}

/// 帯 `r` の中に、画面の 1px ごとに 1 本の縦線で波形を描く (見えている範囲だけ)。
fn push_wave(hctx: &mut HeavyCtx<'_, '_, AppData>, f: &ArrangementFrame<'_>, r: Rect, map: WaveMap, peaks: &[u8]) {
    let bucket = f64::from(TAKE_PEAK_FRAMES);
    // 山が帯の終わりより先まである = 区間が前の周のまま (折り返した直後)。新しい区間が届くまで描かない。
    if peaks.len() as f64 * bucket > map.frame0 + map.frames_per_px * f64::from(r.w) + bucket {
        return;
    }
    let (light, dark) = (f.style.clip_selected_border, f.style.clip_selected_border_inner);
    let mid = r.y + r.h * 0.5;
    let half = r.h * 0.5 * WAVE_HEIGHT;
    let right = (r.x + r.w).min(f.lanes.x + f.lanes.w);
    let mut x = r.x.max(f.lanes.x).floor();
    while x < right {
        let from = map.frame0 + f64::from((x - r.x).max(0.0)) * map.frames_per_px;
        let k0 = (from / bucket) as usize;
        if k0 >= peaks.len() {
            break;
        }
        let k1 = (((from + map.frames_per_px) / bucket).ceil() as usize).clamp(k0 + 1, peaks.len());
        let peak = peaks[k0..k1].iter().copied().max().unwrap_or(0);
        let h = (f32::from(peak) / 255.0 * half).max(0.5);
        push_filled_rect(hctx, Rect { x, y: mid - h - 1.0, w: 1.0, h: h * 2.0 + 2.0 }, dark);
        push_filled_rect(hctx, Rect { x, y: mid - h, w: 1.0, h: h * 2.0 }, light);
        x += 1.0;
    }
}
