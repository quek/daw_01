//! r.md #156: トラックの「入力元」と「モニター」 (インスペクタの「入力」の帯の中身、r.md #158)。
//!
//! 対象は [`AppData::input_targets`] (複数選択ならその全部、値が割れていれば選択なしで表示)。帯を出すか・帯の
//! 対象・中身が、`draw_bands` が 1 フレームに 1 回だけ作ったその結果を共有する。録れない入力は選択肢に出さず
//! (r.md #158 仕様 14、`common::model::InputRules`)、書き込みも録れるトラックにだけ。モニターは音声入力にだけ
//! 効くので、対象が全部 MIDI の間は行ごと出さない (空白も残さない — 行を足すと下の帯が動くのは、押した
//! dropdown より下だけ)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use common::model::{MonitorMode, TrackInput};

use super::form;
use super::layout::FORM_ROW_PITCH;
use super::vocab;
use crate::app::{AppData, AppEvent};
use crate::event_audio_rec::AudioRecEvent;
use crate::handler::audio_rec::{InputTarget, RecordableInputs};

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
    targets: &[InputTarget<'_>],
) -> f32 {
    if targets.is_empty() {
        return y;
    }
    let inputs: Vec<TrackInput> = targets.iter().map(|t| t.track.input).collect();
    let input = fold(inputs.iter().copied());
    let monitor = fold(targets.iter().map(|t| t.track.monitor));
    let any_audio = inputs.iter().any(|i| !i.is_midi());
    let kinds = targets.iter().fold(RecordableInputs::default(), |acc, t| acc.union(t.kinds));
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
        let ids: Vec<u32> = targets.iter().filter(|t| t.kinds.allows(input)).map(|t| t.track.id).collect();
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
            let ids: Vec<u32> = targets.iter().map(|t| t.track.id).collect();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AudioRec(AudioRecEvent::SetTrackMonitor { track_ids: ids, monitor }));
            }));
        }
        y += FORM_ROW_PITCH;
    }
    y
}
