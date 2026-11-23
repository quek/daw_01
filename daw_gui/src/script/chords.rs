//! r.md #157: コードトラックの script API (`docs/plan_chord_track.md`)。
//!
//! 編集は GUI と同じ `AppEvent::Chord` / `ChordFollow` / `ChordTool` を `AppData::handle_event` に通す
//! (undo / dirty / 子プロセス sync は handler の唯一の口が持つ)。イベントは保存形 (serde) のまま JSON で
//! 渡すので、variant を足せばここを触らずに script から撃てる。

use boa_engine::value::TryFromJs;
use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};
use common::chord_name::parse_chord;
use serde::Deserialize;
use serde_json::Value;

use super::{arg_to_string, js_native, with_app};
use crate::app::AppEvent;
use crate::event_chord::{ChordFollowEvent, ChordToolEvent, ChordTrackEvent};

/// `daw.chordEvent(json)` の受け口。`{"Chord": ..}` / `{"ChordFollow": ..}` / `{"ChordTool": ..}` の
/// 1 キーのオブジェクト (中身は各 enum の externally tagged 形)。
#[derive(Deserialize)]
enum ScriptChordEvent {
    Chord(ChordTrackEvent),
    ChordFollow(ChordFollowEvent),
    ChordTool(ChordToolEvent),
}

/// `daw.chordEvent(json)` — コードトラックのイベントを 1 つ撃つ。例:
///
/// - `{"Chord": {"Place": {"start": 0, "len": 4, "chord": {"root": 9, "kind": "Minor7"}}}}`
/// - `{"Chord": {"Move": {"ids": [1, 2], "delta": 4}}}`
/// - `{"ChordFollow": {"SetTracks": {"track_ids": [1], "mode": "Narrow"}}}`
///
/// コードの JSON は `daw.parseChord("Am7")` で作れる。
pub(super) fn daw_chord_event(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_to_string(args, 0, ctx)?;
    let ev: ScriptChordEvent =
        serde_json::from_str(&json).map_err(|e| js_native(format!("chordEvent: parse: {e}")))?;
    with_app(move |app, _side, _io| {
        app.handle_event(match ev {
            ScriptChordEvent::Chord(e) => AppEvent::Chord(e),
            ScriptChordEvent::ChordFollow(e) => AppEvent::ChordFollow(e),
            ScriptChordEvent::ChordTool(e) => AppEvent::ChordTool(e),
        });
    });
    Ok(JsValue::undefined())
}

/// `daw.chordsJson()` → `[{id, start_beat, len_beats, chord, name}]` の JSON 文字列 (位置順)。
/// `name` は GUI の帯と同じ表示名 (`AppData::chord_label`)。
pub(super) fn daw_chords_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_app(move |app, _side, _io| {
        let song = app.cur.song_doc.song();
        let rows: Vec<Value> = song
            .chords
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "start_beat": c.start_beat,
                    "len_beats": c.len_beats,
                    "chord": c.chord,
                    "name": app.chord_label(&c.chord, c.start_beat),
                })
            })
            .collect();
        serde_json::to_string(&rows)
    })
    .map_err(|e| js_native(format!("chordsJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.parseChord(text)` → コードの JSON 文字列 (`{"root": 9, "kind": "Minor7"}`)。読めなければ `null`。
pub(super) fn daw_parse_chord(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg_to_string(args, 0, ctx)?;
    let Some(chord) = parse_chord(&text) else { return Ok(JsValue::null()) };
    let json = serde_json::to_string(&chord).map_err(|e| js_native(format!("parseChord: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.soundingKey(trackId, clipId, noteId)` → クリップのノートが追従で鳴らす鍵盤 (書いた音の空間、移調は
/// 含まない)。ピアノロールの薄い枠と「鳴る音で確定」と同じ読み口 (`ClipSoundingKeys`、自分の和音とクリップの記録
/// 込み)。鳴らさなければ / クリップかノートが無ければ `null`。engine を通さずに追従の評価だけを確かめる口。
pub(super) fn daw_sounding_key(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let clip_id = u32::try_from_js(args.get_or_undefined(1), ctx)?;
    let note_id = u32::try_from_js(args.get_or_undefined(2), ctx)?;
    let key = with_app(move |app, _side, _io| {
        let keys = app.clip_sounding_keys(track_id, clip_id)?;
        let song = app.cur.song_doc.song();
        let clip = song.track_by_id(track_id)?.clip_by_id(clip_id)?;
        let note = song.clip_notes(clip).iter().find(|n| n.id == note_id)?;
        keys.key(note)
    });
    Ok(key.map_or(JsValue::null(), |k| JsValue::from(i32::from(k))))
}
