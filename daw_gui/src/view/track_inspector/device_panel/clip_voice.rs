//! VOICEVOX 歌唱クリップの per-clip 声 (キャラ ▼ → スタイル ▼)。
//!
//! `device_panel/mod.rs` が順に呼ぶセクションの 1 つ。 contract は
//! `chain_sections.rs` / `modulation_rack.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。
use super::super::*;

pub(super) fn draw_clip_voice(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    // Clip Voice 編集: 選択中の clip が vocal track 上の MIDI clip の
    // とき、 キャラ ▼ → スタイル ▼ の 2 段 dropdown で per-clip 声を選ぶ。
    // 声は per-clip (`Clip::speaker_id`) が SSoT、 SetClipVoice で焼き込む。
    if app.voicevox_param_panel_open()
        && let Some(r) = app.selected_clip_ref()
        && let Some(track) = app.song_doc.song().tracks.get(r.track as usize)
        && track.is_voicevox_vocal()
        && let Some(clip) = track.clips.get(r.clip as usize)
        && app
            .song_doc.song()
            .clip_contents
            .get(&clip.content_id)
            .is_none_or(|c| matches!(c, common::model::ClipContent::Midi(_)))
    {
        let clip_key = common::model::ClipKey {
            track_id: track.id,
            clip_id: clip.id,
        };
        let cur_speaker = clip.speaker_id;
        // 現在の声の表示名: clip 焼き込み名 → speaker_id 逆引き → アプリ既定。
        let (cur_singer, cur_style) = if !clip.singer_name.is_empty() {
            (clip.singer_name.clone(), clip.style_name.clone())
        } else if let Some(found) = app.voicevox.singers.iter().find_map(|s| {
            s.styles
                .iter()
                .find(|st| st.id == cur_speaker)
                .map(|st| (s.name.clone(), st.name.clone()))
        }) {
            found
        } else {
            (
                common::voicevox::DEFAULT_SINGER_NAME.to_string(),
                common::voicevox::DEFAULT_STYLE_NAME.to_string(),
            )
        };

        ui.label_at(
            "inspector_clip_voice_label",
            "Clip Voice",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        if app.voicevox.singers.is_empty() {
            // engine 未起動 / 一覧未取得: 焼き込み声名 + 取得中。 声名は常に出せる。
            let txt = format!("{cur_singer} - {cur_style}  (一覧取得中…)");
            ui.label_at(
                "inspector_clip_voice_current",
                &txt,
                area.x + pad + 4.0,
                y + 6.0,
                11.0,
                p.text,
            );
            y += 26.0;
        } else {
            // 上段: キャラ dropdown。
            let char_labels: Vec<&str> =
                app.voicevox.singers.iter().map(|s| s.name.as_str()).collect();
            let cur_char_idx = app
                .voicevox.singers
                .iter()
                .position(|s| s.name == cur_singer)
                .unwrap_or(0);
            let char_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_char = ui.dropdown(
                "inspector_clip_voice_char",
                char_rect,
                &char_labels,
                cur_char_idx,
            );
            y += 28.0;

            // 下段: スタイル dropdown (= 上段で選んだ or 現在のキャラの styles)。
            let char_idx = picked_char.unwrap_or(cur_char_idx).min(app.voicevox.singers.len() - 1);
            let singer = &app.voicevox.singers[char_idx];
            let style_labels: Vec<&str> =
                singer.styles.iter().map(|st| st.name.as_str()).collect();
            let cur_style_idx = singer
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
                "inspector_clip_voice_style",
                style_rect,
                &style_labels,
                cur_style_idx,
            );
            y += 28.0;

            // 確定値: キャラを変えたらそのキャラの先頭 style、 style を変えたら
            // その style を採用 (= (speaker_id, singer_name, style_name))。
            let chosen: Option<(u32, String, String)> = if let Some(pc) = picked_char {
                app.voicevox.singers.get(pc).and_then(|s| {
                    s.styles
                        .first()
                        .map(|st| (st.id, s.name.clone(), st.name.clone()))
                })
            } else if let Some(ps) = picked_style {
                singer
                    .styles
                    .get(ps)
                    .map(|st| (st.id, singer.name.clone(), st.name.clone()))
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

            // 再取得ボタン (新規キャラ導入時に押す)。
            let refetch_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_clip_voice_refetch",
                "声一覧を再取得",
                refetch_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::RefetchSingers);
                }));
            }
            y += 28.0;
        }
    }
    y
}
