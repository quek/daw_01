//! 読み上げ (VOICEVOX の Text クリップ、`docs/plan_voicevox_talk.md` §4)。r.md #158 で VOICEVOX の Par から
//! クリップの帯へ移した。
//!
//! VOICEVOX を持つトラックの Text クリップを 1 つ選んでいるとき、小見出し「読み上げ」の下に
//! 「ここから読む」・字幕デバイスを足すボタン (無いとき)・話者 (キャラ → talk のスタイル)・
//! 読み上げのスケール 4 つ (話速 / 音高 / 抑揚 / 音量) を出す。セリフ (本文) は帯の先頭の 1 本 (`mod.rs`)。
//! 声は `Clip::speaker_id` を talk のスタイルとして流用 (`SetClipVoice` で焼き込み)、スケールは `Clip::talk`。

use daw_ui_core::{Edit, ScrubableNumberFormat, Ui};

use common::model::ClipKey;

use super::super::form::{self, FormCols};
use super::super::layout::FORM_ROW_PITCH;
use super::super::{band, push_scrub_bracket, scrub_style, toggle_audio_style, vocab};
use super::voice_picker::{VoicePick, draw_voice_pick};
use crate::app::{AppData, AppEvent, InspectorScrubField, TalkParamKind};

/// 小見出し「読み上げ」以下。VOICEVOX を持つトラックの Text クリップでなければ何も描かない。
pub(super) fn draw_talk(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, clip: ClipKey, y: f32) -> f32 {
    let song = app.cur.song_doc.song();
    let Some(track) = song.track_by_id(clip.track_id).filter(|t| t.is_voicevox_vocal()) else {
        return y;
    };
    let Some(cl) = track.clip_by_id(clip.clip_id) else {
        return y;
    };
    let mut y = band::sub_heading(app, ui, "inspector_talk_head", c.label_x, w, y + 2.0, vocab::SUB_TALK, None);

    // r.md #132 残件: 「ここから読む」。点灯 = このクリップの窓の頭から読み上げる、消灯 = 前の片の読み上げの
    // 続き (字幕だけを出す)。押すと普通の読み上げ / 続きの片を入れ替える (1 undo step)。窓で始まる字幕が
    // 無いクリップには出さない。
    if let Some(reads) = app.clip_text_reads(clip) {
        form::label(ui, app, "inspector_talk_reads_label", vocab::FIELD_READ_START, c, y);
        ui.toggle_button_at(
            "inspector_talk_reads_here",
            vocab::TOGGLE_READ_HERE,
            c.value(y),
            reads,
            &toggle_audio_style(&app.theme),
            move |_| Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::SetClipTextReads { clip, reads: !reads })),
        );
        y += FORM_ROW_PITCH;
    }

    // 字幕デバイスが無い = 画面に出ない。ワンクリックで足す (Q10)。
    if !track.has_subtitle_device() {
        form::label(ui, app, "inspector_talk_subtitle_label", vocab::SUB_SUBTITLE, c, y);
        if ui.button_at_clicked("inspector_talk_add_subtitle", vocab::ADD_SUBTITLE_DEVICE, c.value(y)) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::SelectPluginFromDb {
                    id: common::plugin_db::SUBTITLE_ID.to_string(),
                    keep_open: false,
                    open_gui: false,
                });
            }));
        }
        y += FORM_ROW_PITCH;
    }

    // 話者 (キャラ → talk のスタイル)。
    let pick = VoicePick {
        id: "inspector_talk_voice",
        list: &app.voicevox.talk_speakers,
        current: (cl.singer_name.as_str(), cl.style_name.as_str()),
        speaker: cl.speaker_id,
        clip,
        refetch: || AppEvent::RefetchSpeakers,
    };
    y = draw_voice_pick(app, ui, c, y, &pick);

    // 読み上げのスケール 4 つ。
    let talk = cl.talk.unwrap_or_default();
    let scales = [
        (vocab::FIELD_TALK_SPEED, TalkParamKind::Speed, talk.speed_scale, 1.0),
        (vocab::FIELD_TALK_PITCH, TalkParamKind::Pitch, talk.pitch_scale, 0.0),
        (vocab::FIELD_TALK_INTONATION, TalkParamKind::Intonation, talk.intonation_scale, 1.0),
        (vocab::FIELD_TALK_VOLUME, TalkParamKind::Volume, talk.volume_scale, 1.0),
    ];
    for (label, kind, value, default) in scales {
        form::label(ui, app, ("inspector_talk_scale_label", label), label, c, y);
        let resp = ui.scrubable_number_at(
            ("inspector_talk_scale", label),
            c.value(y),
            f64::from(value),
            default,
            ScrubableNumberFormat::Decimal(2),
            &scrub_style(&app.theme),
            move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipTalkParam { clip, param: kind, value: v as f32 });
                })
            },
            None,
            None,
        );
        // 他の数値欄と同じく 1 ドラッグ = 1 undo step に束ねる。
        push_scrub_bracket(ui, app, InspectorScrubField::Talk { kind }, resp.dragging || resp.editing_text);
        y += FORM_ROW_PITCH;
    }
    y
}
