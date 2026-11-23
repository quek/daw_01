//! r.md #156: トラックの「入力元」と「モニター」 (インスペクタの「入力」の帯の中身、r.md #158)。
//!
//! 対象は [`AppData::input_target_track_ids`] (複数選択ならその全部、値が割れていれば選択なしで表示)。
//! 録れない入力は選択肢に出さず (r.md #158 仕様 14、[`crate::handler::audio_rec::recordable_inputs`])、
//! 書き込みも録れるトラックにだけ。モニターは音声入力にだけ効くので、対象が全部 MIDI の間は行ごと出さない
//! (空白も残さない — 行を足すと下の帯が動くのは、押した dropdown より下だけ)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use common::model::{MonitorMode, TrackInput};

use super::form;
use super::layout::FORM_ROW_PITCH;
use super::vocab;
use crate::app::{AppData, AppEvent};
use crate::event_audio_rec::AudioRecEvent;
use crate::handler::audio_rec::recordable_inputs;

/// 全部同じ値ならその値 (割れていれば `None`)。
fn fold<T: PartialEq + Copy>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let first = values.next()?;
    values.all(|v| v == first).then_some(first)
}

/// 入力の帯を出すか (対象のどれかが何かを録れる)。
pub(super) fn input_band_shown(app: &AppData) -> bool {
    !app.input_target_track_ids().is_empty()
}

pub(super) fn draw_input_section(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, pad: f32, mut y: f32) -> f32 {
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
    let kinds = app.recordable_inputs_of(&track_ids);
    let c = form::cols(area.x + pad, area.w - pad * 2.0);

    // ---- 入力元 ----
    form::label(ui, app, "inspector_input_label", vocab::FIELD_INPUT_SOURCE, &c, y);
    let choices = app.audio_input_choices(&inputs, kinds);
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    // 値が割れているときは選択なし (dropdown は範囲外 index で空表示になる)。
    let selected = input.and_then(|i| choices.iter().position(|c| c.input == i)).unwrap_or(usize::MAX);
    if let Some(i) = ui.dropdown("inspector_track_input", c.value(y), &labels, selected)
        && let Some(choice) = choices.get(i)
    {
        // 録れるトラックにだけ書く (グループのバスに MIDI を、など意味の無い入力を付けない)。
        let input = choice.input;
        let ids: Vec<u32> =
            tracks().filter(|t| recordable_inputs(song, t).allows(input)).map(|t| t.id).collect();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackInput { track_ids: ids, input }));
        }));
    }
    y += FORM_ROW_PITCH;

    // ---- モニター (オフ / 自動 / オン)。音声入力のときだけ ----
    if any_audio {
        form::label(ui, app, "inspector_monitor_label", vocab::FIELD_MONITOR, &c, y);
        let labels = MonitorMode::ALL.map(MonitorMode::label);
        let selected = monitor.and_then(|m| MonitorMode::ALL.iter().position(|c| *c == m)).unwrap_or(usize::MAX);
        if let Some(i) = ui.dropdown("inspector_track_monitor", c.value(y), &labels, selected)
            && let Some(&monitor) = MonitorMode::ALL.get(i)
        {
            let ids = track_ids;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackMonitor { track_ids: ids, monitor }));
            }));
        }
        y += FORM_ROW_PITCH;
    }
    y
}
