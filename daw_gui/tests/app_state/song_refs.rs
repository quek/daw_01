//! Song の id を指す session 状態 (device 選択 / SC Listen / MIDI Learn 待ち / last touched / Par / SC パネル) が、
//! 指す先が消えた・作り直されたときに別の対象へ付かない (`docs/plan_rack_native_devices.md` §7.7 / §7.9 /
//! §10.14 / §12.2 / Q18)。
//!
//! 掃除は Song を変える AppData の口 (`edit_song` 系と undo / redo) の後の `reconcile_song_refs` 1 本が持つので、
//! ここでは「経路ごとに掃除を書いていなかった」経路 (グループ解除 / 末尾トラック削除) を名指しで通す。

use common::model::{
    AutomationTarget, BindingTarget, ChainRef, CompParam, MidiBindInput, MidiBinding, NativeKind, NativeParamId,
    RackPanelKey,
};
use common::protocol::AudioCommand;
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent};
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_native::NativeEdit;

use super::support::{build_app, drain};

fn dev(app: &mut AppData, ev: DeviceEvent) {
    app.handle_event(AppEvent::Device(ev));
}

fn add_track(app: &mut AppData) -> u32 {
    let before: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|id| !before.contains(id)).expect("新トラック")
}

fn builtin_comp(app: &AppData, owner: u32) -> u64 {
    app.cur.song_doc.song().builtin_native(owner, NativeKind::Comp).expect("組み込み Comp").id
}

/// `track` に内蔵 device を足して id を返す。
fn add_native(app: &mut AppData, track: u32, kind: NativeKind, open_panel: bool) -> u64 {
    dev(app, DeviceEvent::AddNative { chain: ChainRef::Track(track), kind, open_panel });
    app.cur.selection.selected_device_ids[0]
}

fn listen_cmds(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<Option<u64>> {
    drain(rx)
        .into_iter()
        .filter_map(|c| match c {
            AudioCommand::SetScListen { device_id, .. } => Some(device_id),
            _ => None,
        })
        .collect()
}

fn thr() -> NativeParamId {
    NativeParamId::Comp(CompParam::Threshold)
}

// ---- 指摘 2: トラックが消える全経路で Listen / 選択が掃除される --------------------------------

/// グループ解除で group ごと消えた Comp の Listen は解除され (engine にも送る)、undo で group が戻っても
/// Listen は勝手に戻らない。group の device を指す選択も落ちる。
#[test]
fn ungrouping_clears_listen_and_selection_of_the_removed_group() {
    let (mut app, mut audio, _p, _d) = build_app();
    let g = app.cur.song_doc.song().tracks[0].id;
    let child = add_track(&mut app);
    app.handle_event(AppEvent::SetTrackParent { track_ids: vec![child], parent_id: Some(g), anchor_after: Some(g) });
    let comp = builtin_comp(&app, g);
    dev(&mut app, DeviceEvent::SetScListen { device_id: Some(comp) });
    app.set_device_selection(vec![comp]);
    app.cur.selection.device_anchor = Some(comp);
    let _ = drain(&mut audio);

    app.handle_event(AppEvent::UngroupTracks { track_ids: vec![g] });
    assert!(app.cur.song_doc.song().track_by_id(g).is_none(), "group は消える");
    assert_eq!(app.cur.peph.sc_listen_device, None);
    assert_eq!(listen_cmds(&mut audio), vec![None], "engine にも解除を送る");
    assert!(app.cur.selection.selected_device_ids.is_empty());
    assert_eq!(app.cur.selection.device_anchor, None);

    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().native_by_id(comp).is_some(), "undo で group の Comp が戻る");
    assert_eq!(app.cur.peph.sc_listen_device, None, "Listen は勝手に戻らない");
}

/// 選択から消えた id を落とすだけでは、直近の編集面 (last-wins タグ) を device 面へ戻さない。device を
/// 2 つ選んだあと別の面を操作し、undo で片方が消えても、次の Delete は device 面を向かない。
#[test]
fn pruning_device_selection_keeps_the_last_edit_surface() {
    use daw_gui::app::EditSurface;
    let (mut app, _a, _p, _d) = build_app();
    let t0 = app.cur.song_doc.song().tracks[0].id;
    let eq = app.cur.song_doc.song().builtin_native(t0, NativeKind::Eq).expect("eq").id;
    let c2 = add_native(&mut app, t0, NativeKind::Comp, false);
    app.set_device_selection(vec![eq, c2]);
    app.cur.selection.last_edit_select = Some(EditSurface::TimeRange);

    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().native_by_id(c2).is_none());
    assert_eq!(app.cur.selection.selected_device_ids, vec![eq], "消えた id だけ落ちる");
    assert_eq!(app.cur.selection.last_edit_select, Some(EditSurface::TimeRange), "編集面は変えない");

    // 全部消えたら device 面のタグは降ろす。
    app.handle_event(AppEvent::Redo);
    app.set_device_selection(vec![c2]);
    assert_eq!(app.cur.selection.last_edit_select, Some(EditSurface::Devices));
    app.handle_event(AppEvent::Undo);
    assert!(app.cur.selection.selected_device_ids.is_empty());
    assert_eq!(app.cur.selection.last_edit_select, None);
}

/// 末尾トラックの削除でも同じ。
#[test]
fn removing_the_last_track_clears_listen() {
    let (mut app, mut audio, _p, _d) = build_app();
    let last = add_track(&mut app);
    let comp = builtin_comp(&app, last);
    dev(&mut app, DeviceEvent::SetScListen { device_id: Some(comp) });
    let _ = drain(&mut audio);

    app.handle_event(AppEvent::RemoveLastTrack);
    assert!(app.cur.song_doc.song().track_by_id(last).is_none());
    assert_eq!(app.cur.peph.sc_listen_device, None);
    assert_eq!(listen_cmds(&mut audio), vec![None]);
}

/// 口パクのソース (出力先 binding を持つトラック) を外す経路はどれも、ソースを失った口 track の生成物を片付ける
/// (残すと歌が無いのに口だけ動き、再生成の経路からは二度と片付かない)。選択トラック削除だけでなく
/// グループ解除と末尾トラック削除も同じ後始末を通る。
#[test]
fn every_track_removal_path_reaps_orphan_lipsync_clips() {
    use common::model::{Clip, ClipContent, ImageContent};
    let auto_clips = |app: &AppData, mouth: u32| {
        app.cur.song_doc.song().track_by_id(mouth).expect("mouth").clips.iter().filter(|c| c.auto_lipsync).count()
    };
    let bind = |app: &mut AppData, source: u32, mouth: u32| {
        app.edit_song(|song| {
            let content = song.alloc_content(ClipContent::Image(ImageContent { events: Vec::new() }), String::new());
            song.track_by_id_mut(source).expect("source").lipsync_target_track = Some(mouth);
            song.track_by_id_mut(mouth).expect("mouth").place_clip(Clip {
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: content,
                auto_lipsync: true,
                ..Default::default()
            });
        });
        assert_eq!(auto_clips(app, mouth), 1);
    };

    // 末尾トラック削除。
    let (mut app, _a, _p, _d) = build_app();
    let mouth = app.cur.song_doc.song().tracks[0].id;
    let source = add_track(&mut app);
    bind(&mut app, source, mouth);
    app.handle_event(AppEvent::RemoveLastTrack);
    assert!(app.cur.song_doc.song().track_by_id(source).is_none());
    assert_eq!(auto_clips(&app, mouth), 0, "末尾トラック削除");

    // グループ解除 (group 自身がソース)。
    let (mut app, _a, _p, _d) = build_app();
    let mouth = app.cur.song_doc.song().tracks[0].id;
    let group = add_track(&mut app);
    let child = add_track(&mut app);
    app.handle_event(AppEvent::SetTrackParent { track_ids: vec![child], parent_id: Some(group), anchor_after: Some(group) });
    bind(&mut app, group, mouth);
    app.handle_event(AppEvent::UngroupTracks { track_ids: vec![group] });
    assert!(app.cur.song_doc.song().track_by_id(group).is_none());
    assert_eq!(auto_clips(&app, mouth), 0, "グループ解除");
}

// ---- 指摘 3: undo で消えた id を新しい device が引き継がない --------------------------------------

/// undo で消えた device の Par / SC パネルの開閉 (session / 表示状態) は、次に作った device に付かない
/// (Q18: 新しい device は閉じた状態から始まる)。redo で戻れば元の device は開いた状態で出る (§12.2)。
///
/// 塞ぐのは SongDoc の履歴ジャンプが id allocator を巻き戻さない修正 (X1)。
#[test]
fn a_device_created_after_undo_does_not_inherit_open_panels() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = app.cur.song_doc.song().tracks[0].id;
    let comp = add_native(&mut app, t0, NativeKind::Comp, true);
    app.cur.peph.open_sidechain_panel = Some(comp);
    assert!(app.rack_panel_open(RackPanelKey::Device(comp)));

    app.handle_event(AppEvent::Undo);
    app.handle_event(AppEvent::Redo);
    assert!(app.rack_panel_open(RackPanelKey::Device(comp)), "redo で戻れば開いた状態");

    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().native_by_id(comp).is_none());
    let fresh = add_native(&mut app, t0, NativeKind::Comp, false);
    assert!(!app.rack_panel_open(RackPanelKey::Device(fresh)), "新しい device の Par は閉じている");
    assert_ne!(app.cur.peph.open_sidechain_panel, Some(fresh), "SC パネルも付かない");
}

// ---- 指摘 4: 束縛先が消えた MIDI Learn / A キー ------------------------------------------------

/// Learn 待ちの的の device を消すと Learn は取り消され (status)、その後の CC は binding を積まず、
/// undo も `*` も増えない。
#[test]
fn deleting_the_learn_target_cancels_midi_learn() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = app.cur.song_doc.song().tracks[0].id;
    let c2 = add_native(&mut app, t0, NativeKind::Comp, false);
    dev(&mut app, DeviceEvent::NativeEdit { device_id: c2, edit: NativeEdit::param(thr(), -20.0) });
    let learn = app.midi_learn_binding_target(None).expect("learn");
    assert_eq!(learn, BindingTarget::NativeParam { device_id: c2, param: thr() });
    app.handle_event(AppEvent::StartMidiLearn(learn));

    dev(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![c2] });
    assert_eq!(app.cur.recording.midi_learn_target, None, "Learn は取り消される");
    assert!(app.ui_ephemeral.status_message.contains("取り消し"), "{}", app.ui_ephemeral.status_message);
    assert!(app.cur.peph.last_touched_param.is_none(), "last touched も外れる");

    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    app.handle_event(AppEvent::MidiControlChange { channel: 0, controller: 21, value: 64 });
    assert!(app.cur.song_doc.song().midi_bindings.is_empty());
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
}

/// 適用の口でも確かめる: 解決しない的 (種類違い) の Learn は bind せず、同じ CC の既存 binding を消さず、
/// undo も `*` も増やさない。同じ binding を Learn し直しても undo は増えない。
#[test]
fn learning_an_unresolvable_target_binds_nothing() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = app.cur.song_doc.song().tracks[0].id;
    let comp = builtin_comp(&app, t0);
    let eq = app.cur.song_doc.song().builtin_native(t0, NativeKind::Eq).expect("eq").id;
    let live = BindingTarget::NativeParam { device_id: comp, param: thr() };
    app.handle_event(AppEvent::StartMidiLearn(live));
    app.handle_event(AppEvent::MidiControlChange { channel: 0, controller: 21, value: 0 });
    let bound = |app: &AppData| app.cur.song_doc.song().midi_bindings.clone();
    assert_eq!(
        bound(&app),
        vec![MidiBinding { channel: 0, input: MidiBindInput::cc(21), legacy_controller: None, target: live }]
    );

    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    app.handle_event(AppEvent::StartMidiLearn(live));
    app.handle_event(AppEvent::MidiControlChange { channel: 0, controller: 21, value: 0 });
    assert_eq!(app.cur.song_doc.undo_depth(), depth, "同じ binding の Learn し直しは変化なし");

    // EQ の id に Comp の住所 (= id が別の種類に再利用された的)。
    app.cur.recording.midi_learn_target = Some(BindingTarget::NativeParam { device_id: eq, param: thr() });
    app.handle_event(AppEvent::MidiControlChange { channel: 0, controller: 21, value: 0 });
    assert_eq!(bound(&app).len(), 1, "既存の binding は消えない");
    assert_eq!(bound(&app)[0].target, live);
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
    assert!(app.ui_ephemeral.status_message.contains("bind しませんでした"), "{}", app.ui_ephemeral.status_message);
}

/// A キー: 束縛先が種類違いの last touched はレーンを積まず、「削除された」として last touched を外す
/// (中身の無い undo step / `*` / 成功 status を出さない)。触った device を消すと last touched も外れる。
#[test]
fn a_key_on_an_unresolvable_target_adds_nothing() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = app.cur.song_doc.song().tracks[0].id;
    let eq = app.cur.song_doc.song().builtin_native(t0, NativeKind::Eq).expect("eq").id;
    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    app.handle_event(AppEvent::TouchParam {
        track_id: t0,
        target: AutomationTarget::NativeParam { device_id: eq, param: thr() },
        display_name: "Comp: Thr".into(),
    });
    app.handle_event(AppEvent::AddAutomationFromLastTouched);
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
    assert!(app.cur.peph.last_touched_param.is_none());
    assert!(app.ui_ephemeral.status_message.contains("removed"), "{}", app.ui_ephemeral.status_message);

    let c2 = add_native(&mut app, t0, NativeKind::Comp, false);
    dev(&mut app, DeviceEvent::NativeEdit { device_id: c2, edit: NativeEdit::param(thr(), -20.0) });
    assert!(app.cur.peph.last_touched_param.is_some());
    dev(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![c2] });
    assert!(app.cur.peph.last_touched_param.is_none(), "消えた device の last touched は外れる");
}

// ---- 束縛先を解決してから積む: 変調 routing / 値保持レーン -----------------------------------------

const FX_PARAM: u32 = 7;

/// 先頭トラックに plugin を 1 個置き、host の param 一覧を届けた app。戻り値 = (トラック, device)。
fn app_with_plugin() -> (AppData, u32, u64) {
    use common::model::{Device, PluginInstance};
    use common::protocol::{PluginEvent, PluginParamInfo};
    let (mut app, _a, _p, _d) = build_app();
    let t0 = app.cur.song_doc.song().tracks[0].id;
    let device_id = app
        .edit_song(|song| {
            let id = song.alloc_device_id();
            let plugin = PluginInstance { id, ..PluginInstance::new("test.delay".into(), common::plugin_format::PluginFormat::Clap) };
            song.track_by_id_mut(t0).expect("t0").devices.insert(0, Device::Plugin(plugin));
            id
        })
        .expect("plugin");
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginParamList {
        device: app.dev(device_id),
        params: vec![PluginParamInfo {
            id: FX_PARAM,
            name: "Dry/Wet".into(),
            module: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.5,
            flags: 0,
        }],
        has_embedded_gui: true,
    }));
    let visible: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.apply_select_tracks(t0, daw_gui::widgets::select_modifier::SelectModifier::Single, &visible);
    (app, t0, device_id)
}

/// ◉ で待ち受け中のモジュレーターを undo で消すと待ち受けは外れ、プラグイン窓のツマミを触っても routing を
/// 積まない (空の undo step / `*` / 「割り当てました」を出さない)。消えたモジュレーターの id を掴んだまま
/// 触っても (適用の口の判定)、Song は変わらない。
#[test]
fn undoing_the_armed_mod_source_disarms_and_touch_adds_nothing() {
    use common::protocol::PluginEvent;
    let (mut app, t0, device_id) = app_with_plugin();
    app.handle_event(AppEvent::AddModSource { kind: daw_gui::app::ModSourceKindTag::Lfo });
    let source = app.cur.song_doc.song().mod_sources.last().expect("source").id;
    app.handle_event(AppEvent::SetArmedModSource(Some(source)));

    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().mod_sources.iter().all(|m| m.id != source), "undo でモジュレーターが消える");
    assert_eq!(app.cur.peph.armed_mod_source, None, "待ち受けは外れる");

    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    let touch = |app: &mut AppData| {
        app.handle_event(AppEvent::Plugin(PluginEvent::PluginParamTouched {
            device: app.dev(device_id),
            param_id: FX_PARAM,
            display_name: format!("Param {FX_PARAM}"),
        }));
    };
    touch(&mut app);
    // 消えた id を掴んだまま触る (掃除より前に届いた待ち受け)。
    app.cur.peph.armed_mod_source = Some(source);
    touch(&mut app);
    assert!(app.cur.song_doc.song().track_by_id(t0).expect("t0").mod_routings.is_empty());
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
    assert!(!app.ui_ephemeral.status_message.contains("割り当てました"), "{}", app.ui_ephemeral.status_message);
}

/// 変調 routing の編集は、解決しない target を積まず、変化の無い編集で undo も `*` も積まない
/// (同じ routing の追加 / 無い routing の解除 / 同じ極性 / 同じ深さ)。
#[test]
fn mod_routing_edits_without_effect_add_no_undo_step() {
    let (mut app, t0, device_id) = app_with_plugin();
    app.handle_event(AppEvent::AddModSource { kind: daw_gui::app::ModSourceKindTag::Lfo });
    let source_id = app.cur.song_doc.song().mod_sources.last().expect("source").id;
    let target = AutomationTarget::PluginParam { device_id, param_id: FX_PARAM, legacy_device_index: None };
    app.handle_event(AppEvent::AddModRouting { track_id: t0, target: target.clone(), source_id });
    assert_eq!(app.cur.song_doc.song().track_by_id(t0).expect("t0").mod_routings.len(), 1);

    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    let eq = app.cur.song_doc.song().builtin_native(t0, NativeKind::Eq).expect("eq").id;
    let wrong_kind = AutomationTarget::NativeParam { device_id: eq, param: thr() };
    app.handle_event(AppEvent::AddModRouting { track_id: t0, target: target.clone(), source_id });
    app.handle_event(AppEvent::AddModRouting { track_id: t0, target: wrong_kind.clone(), source_id });
    app.handle_event(AppEvent::RemoveModRouting { track_id: t0, target: wrong_kind, source_id });
    app.handle_event(AppEvent::SetModRoutingPolarity { track_id: t0, target: target.clone(), source_id, bipolar: false });
    app.handle_event(AppEvent::SetModRoutingDepth { track_id: t0, target, source_id, depth: 1.0 });
    assert_eq!(app.cur.song_doc.song().track_by_id(t0).expect("t0").mod_routings.len(), 1, "種類違いは積まない");
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
}

/// plugin の Par の値 (値保持レーンの既定値) を同じ値で書き直しても undo も `*` も積まない。
#[test]
fn rewriting_a_plugin_param_with_the_same_value_adds_no_undo_step() {
    let (mut app, _t0, device_id) = app_with_plugin();
    dev(&mut app, DeviceEvent::SetPluginParam { device_id, param_id: FX_PARAM, value_real: 0.25 });
    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    dev(&mut app, DeviceEvent::SetPluginParam { device_id, param_id: FX_PARAM, value_real: 0.25 });
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
    dev(&mut app, DeviceEvent::SetPluginParam { device_id, param_id: FX_PARAM, value_real: 0.75 });
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "値が変われば 1 step");
}
