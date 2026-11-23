//! r.md #156: トラックの「入力」と「モニター」 (インスペクタの「入力」の帯の中身、r.md #158)。
//!
//! 高さは常に同じ — 下の帯を動かさない。対象は
//! [`AppData::input_target_track_ids`] (複数選択ならその全部、値が割れていれば選択なしで表示)。
//! モニターはオーディオ入力のトラックだけに効くので、対象が全部 MIDI の間は出さない。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use common::model::{MonitorMode, TrackInput};

use crate::app::{AppData, AppEvent};
use crate::event_audio_rec::AudioRecEvent;

const ROW_H: f32 = 22.0;
const ROW_GAP: f32 = 4.0;
const LABEL_H: f32 = 16.0;

/// 全部同じ値ならその値 (割れていれば `None`)。
fn fold<T: PartialEq + Copy>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let first = values.next()?;
    values.all(|v| v == first).then_some(first)
}

pub(super) fn draw_input_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let track_ids = app.input_target_track_ids();
    if track_ids.is_empty() {
        return y;
    }
    let song = app.cur.song_doc.song();
    let tracks = || track_ids.iter().filter_map(|id| song.track_by_id(*id));
    let inputs: Vec<TrackInput> = tracks().map(|t| t.input).collect();
    let input = fold(inputs.iter().copied());
    let monitor = fold(tracks().map(|t| t.monitor));
    let any_audio = inputs.iter().any(|i| !i.is_midi());

    let p = &app.theme.core;
    // 入力 : モニター = 6 : 4。入力の欄の幅は MIDI でも変えない (選んだ瞬間に欄が伸び縮みしない)。
    let row_w = (area.w - pad * 2.0).max(0.0);
    let input_w = (row_w - ROW_GAP) * 0.6;
    let monitor_w = (row_w - ROW_GAP) * 0.4;
    let right_x = area.x + pad + input_w + ROW_GAP;

    ui.label_at("inspector_input_label", "入力", area.x + pad, y, 11.0, p.text_dim);
    if any_audio {
        ui.label_at("inspector_monitor_label", "モニター", right_x, y, 11.0, p.text_dim);
    }
    y += LABEL_H;

    // ---- 入力 ----
    let choices = app.audio_input_choices(&inputs);
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    // 値が割れているときは選択なし (dropdown は範囲外 index で空表示になる)。
    let selected = input.and_then(|i| choices.iter().position(|c| c.input == i)).unwrap_or(usize::MAX);
    if let Some(i) =
        ui.dropdown("inspector_track_input", Rect { x: area.x + pad, y, w: input_w, h: ROW_H }, &labels, selected)
        && let Some(choice) = choices.get(i)
    {
        let (ids, input) = (track_ids.clone(), choice.input);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackInput { track_ids: ids, input }));
        }));
    }

    // ---- モニター (オフ / 自動 / オン) ----
    if any_audio {
        let labels = MonitorMode::ALL.map(MonitorMode::label);
        let selected = monitor.and_then(|m| MonitorMode::ALL.iter().position(|c| *c == m)).unwrap_or(usize::MAX);
        if let Some(i) =
            ui.dropdown("inspector_track_monitor", Rect { x: right_x, y, w: monitor_w, h: ROW_H }, &labels, selected)
            && let Some(&monitor) = MonitorMode::ALL.get(i)
        {
            let ids = track_ids;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackMonitor { track_ids: ids, monitor }));
            }));
        }
    }
    y + ROW_H
}
