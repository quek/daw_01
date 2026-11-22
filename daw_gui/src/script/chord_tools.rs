//! r.md #157 §3.4: コードの試聴 / 検出 / 録音の script API (`docs/plan_chord_track.md`)。
//!
//! 設定と検出の発注は `daw.chordEvent({"ChordTool": ..})` (GUI と同じ `ChordToolEvent`) で撃つ。ここは
//! それで足りない口 (試聴を鳴らす / 状態を読む) だけを持つ。

use boa_engine::{Context, JsResult, JsString, JsValue};
use common::model::Chord;

use super::{arg_to_string, js_native, with_app};

/// `daw.auditionChord(chordJson)` — コードを試聴トラックで鳴らす (帯のクリックと同じ `AppData::audition_chord`)。
pub(super) fn daw_audition_chord(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_to_string(args, 0, ctx)?;
    let chord: Chord = serde_json::from_str(&json).map_err(|e| js_native(format!("auditionChord: parse: {e}")))?;
    with_app(move |app, _side, _io| app.audition_chord(&chord.sanitized()));
    Ok(JsValue::undefined())
}

/// `daw.chordToolsJson()` → `{view, record_armed, detecting, audition, audio_chords}` の JSON 文字列。
/// `audition` は鳴らしている試聴音の鍵盤 (送った鍵盤)、`audio_chords` は音源ごとの
/// `[start_frame, end_frame, 表示名]` の列。
pub(super) fn daw_chord_tools_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_app(move |app, _side, _io| {
        let tools = &app.cur.chord_tools;
        let song = app.cur.song_doc.song();
        let audio: serde_json::Map<String, serde_json::Value> = song
            .audio_chords
            .iter()
            .map(|(id, spans)| {
                let rows: Vec<serde_json::Value> = spans
                    .iter()
                    .map(|s| {
                        let name = common::chord_name::format_chord(
                            &s.chord,
                            common::chord_name::ChordNotation::Regular,
                            None,
                        );
                        serde_json::json!([s.start_frame, s.end_frame, name])
                    })
                    .collect();
                (id.to_string(), serde_json::Value::Array(rows))
            })
            .collect();
        serde_json::to_string(&serde_json::json!({
            "view": tools.view,
            "record_armed": tools.record_armed,
            "detecting": tools.detect_jobs.len(),
            "audition": tools.audition.notes.iter().filter_map(|n| n.key).collect::<Vec<u8>>(),
            "audio_chords": audio,
        }))
    })
    .map_err(|e| js_native(format!("chordToolsJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}
