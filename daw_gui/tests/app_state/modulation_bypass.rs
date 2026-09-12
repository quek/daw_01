//! r.md #115: 変調のバイパス (Q キー) — モジュレーター全体 (`ModSource::enabled`) と
//! 1 本ごと (`ModRouting::enabled`) の 2 段。
//!
//! 固定する契約:
//! 1. どちらの切替も Song の編集 (undo / dirty に乗る) で、 同値なら何もしない。
//! 2. ラックの行 (`mod_source_routings`) は「自分が有効か」 と「実際に効いているか
//!    (= ソースも有効)」 を別に持つ — ソースをバイパスしても routing 側のフラグは据え置き
//!    (戻せば元どおり効く)。
//! 3. GUI 側の live 合成 (`inspector_mod_data`) もバイパスを飛ばす (engine と同じ
//!    `modulation_offset_norm_with` を通る)。

use common::model::{AutomationTarget, ModSource, ModSourceKind, Track, TrackBuiltinParam};

use daw_gui::app::{AppData, AppEvent};

use super::support::build_app;

const TRACK_A: u32 = 100;

fn app_with_source() -> (AppData, u32) {
    let (mut app, _audio_rx, _plugin_rx, _disp) = build_app();
    let sid = app
        .edit_song(|song| {
            song.tracks.clear();
            song.tracks.push(Track { id: TRACK_A, name: "Lead".into(), ..Track::default() });
            let id = song.alloc_mod_source_id();
            song.mod_sources.push(ModSource {
                id,
                owner_track_id: TRACK_A,
                color: [0.3, 0.7, 1.0],
                kind: ModSourceKind::default(),
                enabled: true,
            });
            id
        })
        .expect("edit_song");
    (app, sid)
}

fn connect(app: &mut AppData, target: &AutomationTarget, source_id: u32) -> u32 {
    app.handle_event(AppEvent::AddModRouting { track_id: TRACK_A, target: target.clone(), source_id });
    app.cur.song_doc
        .song()
        .all_mod_routings()
        .find(|r| r.source_id == source_id && &r.target == target)
        .map(|r| r.id)
        .expect("繋いだ変調が引ける")
}

#[test]
fn routing_and_source_bypass_are_independent_and_undoable() {
    let (mut app, sid) = app_with_source();
    let vol = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
    let pan = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan);
    let r_vol = connect(&mut app, &vol, sid);
    let r_pan = connect(&mut app, &pan, sid);
    app.cur.song_doc.mark_saved();

    // 1 本だけバイパス: その行だけ effective が落ちる。
    app.handle_event(AppEvent::SetModRoutingEnabled { routing_id: r_vol, enabled: false });
    assert!(app.cur.song_doc.is_dirty(), "バイパスは曲の編集");
    let rows = app.mod_source_routings(sid);
    let flags: Vec<(u32, bool, bool)> = rows.iter().map(|r| (r.id, r.enabled, r.effective)).collect();
    assert_eq!(flags, vec![(r_vol, false, false), (r_pan, true, true)]);

    // 同値は no-op (undo step を積まない)。
    let before = app.cur.song_doc.undo_depth();
    app.handle_event(AppEvent::SetModRoutingEnabled { routing_id: r_vol, enabled: false });
    assert_eq!(app.cur.song_doc.undo_depth(), before);

    // ソースをバイパス: 全行 effective が落ちるが、 routing 側のフラグは据え置き。
    app.handle_event(AppEvent::SetModSourceEnabled { id: sid, enabled: false });
    assert!(!app.cur.song_doc.song().mod_sources[0].enabled);
    assert!(app.mod_source_display().iter().all(|s| !s.enabled));
    let rows = app.mod_source_routings(sid);
    let flags: Vec<(u32, bool, bool)> = rows.iter().map(|r| (r.id, r.enabled, r.effective)).collect();
    assert_eq!(flags, vec![(r_vol, false, false), (r_pan, true, false)]);

    // 戻すと routing 側のフラグどおりに効く。
    app.handle_event(AppEvent::SetModSourceEnabled { id: sid, enabled: true });
    let rows = app.mod_source_routings(sid);
    let flags: Vec<(u32, bool, bool)> = rows.iter().map(|r| (r.id, r.enabled, r.effective)).collect();
    assert_eq!(flags, vec![(r_vol, false, false), (r_pan, true, true)]);

    // 無い id は no-op。
    app.handle_event(AppEvent::SetModRoutingEnabled { routing_id: 9_999, enabled: false });
    app.handle_event(AppEvent::SetModSourceEnabled { id: 9_999, enabled: false });

    // undo で 1 段ずつ戻る (最後の有効化 → ソースのバイパス → routing のバイパス)。
    app.handle_event(AppEvent::Undo);
    assert!(!app.cur.song_doc.song().mod_sources[0].enabled);
    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().mod_sources[0].enabled);
    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().mod_routing_by_id(r_vol).unwrap().enabled);
}
