//! 読み上げ (VOICEVOX の Text クリップ、`docs/plan_voicevox_talk.md` §4)。r.md #158 で VOICEVOX の Par から
//! クリップの帯へ移した。
//!
//! VOICEVOX を持つトラックの Text クリップを選んでいるとき、小見出し「読み上げ」の下に
//! 「ここから読む」・字幕デバイスを足すボタン (無いとき)・話者 (キャラ → talk のスタイル)・
//! 読み上げのスケール 4 つ (話速 / 音高 / 抑揚 / 音量) を出す。セリフ (本文) は帯の先頭の 1 本 (`mod.rs`)。
//! 声は `Clip::speaker_id` を talk のスタイルとして流用 (`SetClipVoice` で焼き込み)、スケールは `Clip::talk`。
//! 話者とスケールは選んでいる読み上げクリップ全部へ書く (値が割れていれば「—」/ 空の dropdown)。宛先の絞り込みは
//! `AppData::is_voice_target` の 1 本で、編集が起きたときに解決する。
//! 「ここから読む」と字幕デバイスは代表クリップ (とそのトラック) のもの。

use daw_ui_core::{Edit, ScrubableNumberFormat, Ui};

use common::model::{ChainRef, TalkParams};

use super::super::form;
use super::super::layout::FORM_ROW_PITCH;
use super::super::{band, push_scrub_bracket, scrub_style, toggle_audio_style, vocab};
use super::ClipFields;
use super::voice_picker::{VoicePick, draw_voice_pick, same_voice};
use crate::app::{AppData, AppEvent, InspectorScrubField, TalkParamKind};
use crate::event_device::DeviceEvent;
use crate::handler::inspector_selection::VoiceKind;

/// (欄名, 編集する項目, 値の取り出し, dblclick で戻す値)。
type Scale = (&'static str, TalkParamKind, fn(&TalkParams) -> f32, f64);
const SCALES: [Scale; 4] = [
    (vocab::FIELD_TALK_SPEED, TalkParamKind::Speed, |t| t.speed_scale, 1.0),
    (vocab::FIELD_TALK_PITCH, TalkParamKind::Pitch, |t| t.pitch_scale, 0.0),
    (vocab::FIELD_TALK_INTONATION, TalkParamKind::Intonation, |t| t.intonation_scale, 1.0),
    (vocab::FIELD_TALK_VOLUME, TalkParamKind::Volume, |t| t.volume_scale, 1.0),
];

/// 選んでいる読み上げクリップ全部を 1 回だけ歩いて畳んだもの: 話者が揃っているか、スケール 4 つ (割れていれば `None`)。
fn fold_talks(app: &AppData, f: &ClipFields<'_, '_>) -> (bool, [Option<f32>; 4]) {
    let rep = f.rep.clip;
    let mut same = true;
    let mut first: Option<TalkParams> = None;
    let mut mixed = [false; 4];
    for c in f.sel.items.iter().filter(|c| app.is_voice_target(c.track, c.clip, VoiceKind::Talk)) {
        same &= same_voice(c.clip, rep);
        let t = c.clip.talk.unwrap_or_default();
        match &first {
            None => first = Some(t),
            Some(a) => {
                for (i, (_, _, get, _)) in SCALES.iter().enumerate() {
                    mixed[i] |= (get(a) - get(&t)).abs() >= 1e-6;
                }
            }
        }
    }
    let values = std::array::from_fn(|i| first.as_ref().filter(|_| !mixed[i]).map(|t| (SCALES[i].2)(t)));
    (same, values)
}

/// 小見出し「読み上げ」以下。代表クリップが VOICEVOX を持つトラックの Text クリップでなければ何も描かない。
pub(super) fn draw_talk(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, y: f32) -> f32 {
    let (rep, c) = (f.rep, &f.c);
    if !app.is_voice_target(rep.track, rep.clip, VoiceKind::Talk) {
        return y;
    }
    let mut y = band::sub_heading(app, ui, "inspector_talk_head", c.label_x, f.w, y + 2.0, vocab::SUB_TALK, None);

    // r.md #132 残件: 「ここから読む」。点灯 = このクリップの窓の頭から読み上げる、消灯 = 前の片の読み上げの
    // 続き (字幕だけを出す)。押すと普通の読み上げ / 続きの片を入れ替える (1 undo step)。窓で始まる字幕が
    // 無いクリップには出さない。クリップの窓ごとの値なので、代表クリップを 1 つだけ選んでいるときだけ。
    if let Some(reads) = app.clip_text_reads(rep.key).filter(|_| f.sel.single()) {
        form::label(ui, app, "inspector_talk_reads_label", vocab::FIELD_READ_START, c, y);
        let clip = rep.key;
        ui.toggle_button_at(
            "inspector_talk_reads_here",
            vocab::TOGGLE_READ_HERE,
            c.value(y),
            reads,
            &toggle_audio_style(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::SetClipTextReads { clip, reads: !reads }))
            },
        );
        y += FORM_ROW_PITCH;
    }

    // 字幕デバイスが無い = 画面に出ない。ワンクリックで足す (Q10)。足す先は代表クリップの居るトラック
    // (カーソルのトラックではない — クリップを選んだまま別のトラックのヘッダを押していても取り違えない)。
    if !rep.track.has_subtitle_device() {
        form::label(ui, app, "inspector_talk_subtitle_label", vocab::SUB_SUBTITLE, c, y);
        if ui.button_at_clicked("inspector_talk_add_subtitle", vocab::ADD_SUBTITLE_DEVICE, c.value(y)) {
            let chain = ChainRef::Track(rep.key.track_id);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::AddPlugin {
                    chain,
                    id: common::plugin_db::SUBTITLE_ID.to_string(),
                    open_gui: false,
                }));
            }));
        }
        y += FORM_ROW_PITCH;
    }

    // 話者 (キャラ → talk のスタイル)。選んでいる読み上げクリップで割れていれば空。
    let (same, scales) = fold_talks(app, f);
    let cl = rep.clip;
    let pick = VoicePick {
        id: "inspector_talk_voice",
        list: &app.voicevox.talk_speakers,
        current: same.then_some(((cl.singer_name.as_str(), cl.style_name.as_str()), cl.speaker_id)),
        targets: |app| app.inspector_voice_targets(VoiceKind::Talk),
        refetch: || AppEvent::RefetchSpeakers,
    };
    y = draw_voice_pick(app, ui, c, y, &pick);

    // 読み上げのスケール 4 つ。値は全部を畳んだもの (割れていれば「—」、触ると全部その値に揃う)。
    for ((label, kind, _, default), value) in SCALES.into_iter().zip(scales) {
        form::label(ui, app, ("inspector_talk_scale_label", label), label, c, y);
        let resp = ui.scrubable_number_at(
            ("inspector_talk_scale", label),
            c.value(y),
            value.map_or(default, f64::from),
            default,
            ScrubableNumberFormat::Decimal(2),
            &scrub_style(&app.theme),
            move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    let clips = app.inspector_voice_targets(VoiceKind::Talk);
                    app.handle_event(AppEvent::SetClipTalkParam { clips, param: kind, value: v as f32 });
                })
            },
            value.is_none().then_some("\u{2014}"),
            None,
        );
        // 他の数値欄と同じく 1 ドラッグ = 1 undo step に束ねる。
        push_scrub_bracket(ui, app, InspectorScrubField::Talk { kind }, resp.dragging || resp.editing_text);
        y += FORM_ROW_PITCH;
    }
    y
}
