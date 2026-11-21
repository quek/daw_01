//! r.md #156: オーディオ入力を録っている間、録っているトラックのレーンに「録音中の区間」の帯を出す。
//!
//! 帯は録音が始まった拍からプレイヘッドまで伸びる (`RecordingState::audio_rec_region`、Tick ごとに
//! `AppData::audio_rec_tick` が伸ばす)。録り終えてから engine の書き終わりが届いてクリップが置かれる
//! までの間も、帯はそのまま残る。行の縦位置は他の overlay と同じ `f.rows` 1 本から読む。
//!
//! レーンには既存のクリップ (どんな色でもありうる) があるので、録音の赤の半透明の面に加えて、明暗 2 本の枠
//! (選択枠と同じ `push_selection_ring`) を重ねる — 赤いクリップや明るいクリップの上でも区間の端が読める。

use super::super::launcher::layout::row_screen_top;
use super::super::*;

/// 帯の面の不透明度 (下のクリップと波形が透けて見える程度)。
const FILL_ALPHA: f32 = 0.35;

pub(super) fn draw(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    let Some(region) = app.cur.recording.audio_rec_region.as_ref() else { return };
    let Some((start, end)) = region.span else { return };
    let x_of = |beat: f64| f.lanes.x + ((beat - f.view.start_beat) / f.beat_per_px) as f32;
    // 録り始めの 1 フレームでも見えるよう、幅は 1px を下回らせない。
    let (x0, x1) = (x_of(start), x_of(end).max(x_of(start) + 1.0));
    if x1 < f.lanes.x || x0 > f.lanes.x + f.lanes.w {
        return;
    }
    let fill = f.style.armed_button.on_color.with_alpha(FILL_ALPHA);
    let recording = |key: ArrangementRowKey| matches!(key, ArrangementRowKey::Track(id) if region.track_ids.contains(&id));
    ui.heavy(("arrangement_rec_region", &f.id), |hctx| {
        hctx.with_clip_rect(f.lanes, |hctx| {
            for row in f.rows.iter().filter(|r| recording(r.key)) {
                let top = row_screen_top(f, row);
                if top + row.height < f.lanes.y || top > f.lanes.y + f.lanes.h {
                    continue;
                }
                let r = Rect { x: x0, y: top, w: x1 - x0, h: row.height };
                push_filled_rect(hctx, r, fill);
                push_selection_ring(hctx, r, f.style, 0.0, None);
            }
        });
    });
}
