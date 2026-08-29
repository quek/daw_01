//! (talk) Text クリップの読み上げ話者 + 読み上げスケール 4 つ。
//!
//! `device_panel/mod.rs` が順に呼ぶセクションの 1 つ。 contract は
//! `chain_sections.rs` / `modulation_rack.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。
use super::super::*;

pub(super) fn draw_talk(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    // (talk) Text Clip 読み上げ編集 (`docs/plan_voicevox_talk.md` §4)。選択中 clip が
    // VOICEVOX デバイス付きトラック上の Text clip のとき、talk 話者 (キャラ→talk style)
    // + 読み上げスケール 4 つ (話速/音高/抑揚/音量) を編集する。声は `Clip::speaker_id`
    // を talk style として流用 (SetClipVoice で焼き込み)。スケールは `Clip::talk`。
    // 対象外は **早期 return** で抜ける。 `if let` チェーンで囲むと本文 270 行が
    // まるごと 1 段深くなり、 内側の widget コールバックが nesting budget を割る。
    if !app.voicevox_param_panel_open() {
        return y;
    }
    let Some(r) = app.selected_clip_ref() else { return y };
    let Some(track) = app.song_doc.song().track_by_id(r.track_id) else { return y };
    if !track.is_voicevox_vocal() {
        return y;
    }
    let Some(clip) = track.clip_by_id(r.clip_id) else { return y };
    if !app
        .song_doc
        .song()
        .clip_contents
        .get(&clip.content_id)
        .is_some_and(|c| matches!(c, common::model::ClipContent::Text(_)))
    {
        return y;
    }

    let clip_key = common::model::ClipKey {
        track_id: track.id,
        clip_id: clip.id,
    };
    let cur_speaker = clip.speaker_id;
    let has_subtitle = track.has_subtitle_device();
    let talk = clip.talk.unwrap_or_default();
    // 現在の talk 声名: clip 焼き込み名 → speaker_id 逆引き → 空 (取得中表示)。
    let (cur_char, cur_style) = if !clip.singer_name.is_empty() {
        (clip.singer_name.clone(), clip.style_name.clone())
    } else {
        app.voicevox.talk_speakers
            .iter()
            .find_map(|s| {
                s.styles
                    .iter()
                    .find(|st| st.id == cur_speaker)
                    .map(|st| (s.name.clone(), st.name.clone()))
            })
            .unwrap_or_default()
    };

    ui.label_at(
        "inspector_talk_label",
        "読み上げ (Talk)",
        area.x + pad,
        y,
        12.0,
        p.text,
    );
    y += 18.0;

    // 字幕デバイス未挿入 = 画面非表示。ワンクリック追加ヘルパ (Q10)。
    if !has_subtitle {
        let warn_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 22.0,
        };
        if ui.button_at_clicked(
            "inspector_talk_add_subtitle",
            "+ 字幕デバイス (画面に表示)",
            warn_rect,
        ) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::SelectPluginFromDb {
                    id: common::plugin_db::SUBTITLE_ID.to_string(),
                    keep_open: false,
                    open_gui: false,
                });
            }));
        }
        y += 26.0;
    }

    // (talk) 本文 (セリフ) 入力。字幕 device 時は overlay「Text Event」節が本文入力を
    // 持つので、ここは字幕 device 無し (= 喋るが映さない talk-only) のときだけ出し、
    // 二重入力を避ける。編集 buffer / events は overlay と共用 (同時表示しないので競合せず)。
    if !has_subtitle {
        if app.ui_ephemeral.clip_edit_buffer_target != Some(r) {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ResyncClipTextEditBuffers(r));
            }));
        }
        ui.label_at(
            "inspector_talk_text_label",
            "セリフ",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        let resp = ui.text_input_at(
            "inspector_talk_text_input",
            Rect {
                x: area.x + pad + 48.0,
                y,
                w: area.w - pad * 2.0 - 48.0,
                h: 22.0,
            },
            &app.ui_ephemeral.clip_text_content_edit_text,
            &TextInputStyle::default(),
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipTextContentEditChanged(s))
                })
            },
        );
        // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
        if resp.committed || resp.blurred {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipTextContentEdit)
            }));
        }
        y += 26.0;
    }

    // talk 話者 picker (キャラ → talk style)。
    if app.voicevox.talk_speakers.is_empty() {
        let txt = if cur_char.is_empty() {
            "(talk 声一覧 取得中…)".to_string()
        } else {
            format!("{cur_char} - {cur_style}  (一覧取得中…)")
        };
        ui.label_at(
            "inspector_talk_voice_current",
            &txt,
            area.x + pad + 4.0,
            y + 6.0,
            11.0,
            p.text,
        );
        y += 26.0;
    } else {
        let char_labels: Vec<&str> =
            app.voicevox.talk_speakers.iter().map(|s| s.name.as_str()).collect();
        let cur_char_idx = app
            .voicevox.talk_speakers
            .iter()
            .position(|s| s.name == cur_char)
            .or_else(|| {
                app.voicevox.talk_speakers
                    .iter()
                    .position(|s| s.styles.iter().any(|st| st.id == cur_speaker))
            })
            .unwrap_or(0);
        let char_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };
        let picked_char =
            ui.dropdown("inspector_talk_char", char_rect, &char_labels, cur_char_idx);
        y += 28.0;

        let char_idx = picked_char
            .unwrap_or(cur_char_idx)
            .min(app.voicevox.talk_speakers.len() - 1);
        let speaker = &app.voicevox.talk_speakers[char_idx];
        let style_labels: Vec<&str> =
            speaker.styles.iter().map(|st| st.name.as_str()).collect();
        let cur_style_idx = speaker
            .styles
            .iter()
            .position(|st| st.id == cur_speaker)
            .unwrap_or(0);
        let style_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };
        let picked_style = ui.dropdown(
            "inspector_talk_style",
            style_rect,
            &style_labels,
            cur_style_idx,
        );
        y += 28.0;

        let chosen: Option<(u32, String, String)> = if let Some(pc) = picked_char {
            app.voicevox.talk_speakers.get(pc).and_then(|s| {
                s.styles
                    .first()
                    .map(|st| (st.id, s.name.clone(), st.name.clone()))
            })
        } else if let Some(ps) = picked_style {
            speaker
                .styles
                .get(ps)
                .map(|st| (st.id, speaker.name.clone(), st.name.clone()))
        } else {
            None
        };
        if let Some((sid, sn, stn)) = chosen {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipVoice {
                    clip: clip_key,
                    speaker_id: sid,
                    singer_name: sn.clone(),
                    style_name: stn.clone(),
                });
            }));
        }

        let refetch_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 22.0,
        };
        if ui.button_at_clicked(
            "inspector_talk_refetch",
            "talk 声一覧を再取得",
            refetch_rect,
        ) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::RefetchSpeakers);
            }));
        }
        y += 28.0;
    }

    // 読み上げスケール 4 つ。VOICEVOX talk の話速/音高/抑揚/音量。
    let scales = [
        ("話速", TalkParamKind::Speed, f64::from(talk.speed_scale), 1.0),
        ("音高", TalkParamKind::Pitch, f64::from(talk.pitch_scale), 0.0),
        (
            "抑揚",
            TalkParamKind::Intonation,
            f64::from(talk.intonation_scale),
            1.0,
        ),
        ("音量", TalkParamKind::Volume, f64::from(talk.volume_scale), 1.0),
    ];
    for (label, kind, val, default) in scales {
        ui.label_at(
            ("inspector_talk_scale_label", label),
            label,
            area.x + pad,
            y + 4.0,
            11.0,
            p.text,
        );
        let input_rect = Rect {
            x: area.x + pad + 48.0,
            y,
            w: area.w - pad * 2.0 - 48.0,
            h: 20.0,
        };
        let resp = ui.scrubable_number_at(
            ("inspector_talk_scale", label),
            input_rect,
            val,
            default,
            ScrubableNumberFormat::Decimal(2),
            &scrub_style(&app.theme),
            move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipTalkParam {
                        clip: clip_key,
                        param: kind,
                        value: v as f32,
                    });
                })
            },
            None,
            None,
        );
        // 他の inspector 数値 field (`scrub_field`) と同じ Begin/End bracket
        // で 1 drag = 1 undo step にする — これが無いと talk 4 項目だけ
        // Ctrl+Z で戻せない (review)。
        let scrub_key = crate::app::InspectorScrubField::Talk(kind);
        let active = resp.dragging || resp.editing_text;
        let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
        if active && !was_active {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                app.handle_event(AppEvent::BeginInspectorScrub);
            }));
        } else if !active && was_active {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.inspector_scrub_active = None;
                app.handle_event(AppEvent::EndInspectorScrub);
            }));
        }
        y += 24.0;
    }
    y
}
