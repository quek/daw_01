//! r.md #110 (`docs/plan_parallel.md` §7): Parallel の GUI 側 — Group / Ungroup / chain 追加・削除 /
//! Parallel 内 chain への運搬 / chain rows の flatten / sidechain の chain source。

use common::model::{ChainRef, Device, TapPoint, TapSource};
use common::protocol::PluginEvent;
use daw_gui::app::{AppData, AppEvent, ChainRowKind, RelocateDevices};

use super::support::{build_app, fake_plugin_loaded, select_track_single};

/// track 0 に [synth, bitcrush, delay] を picker 経由で載せて load 完了まで進める。
fn setup_chain(app: &mut AppData) -> (u32, [u64; 3]) {
    let track_id = app.cur.song_doc.song().tracks[0].id;
    select_track_single(app, 0);
    let mut ids = [0u64; 3];
    for (i, pid) in ["test.synth", "test.bitcrush", "test.delay"].iter().enumerate() {
        app.handle_event(AppEvent::OpenPluginPicker { chain: None });
        app.handle_event(AppEvent::SelectPluginFromDb {
            id: (*pid).into(),
            keep_open: false,
            open_gui: false,
        });
        ids[i] = fake_plugin_loaded(app, track_id, i as u32, pid);
    }
    (track_id, ids)
}

fn flush_states(app: &mut AppData) {
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
}

fn plugin_ids_in_order(app: &AppData) -> Vec<String> {
    app.cur.song_doc.song().tracks[0]
        .plugins()
        .map(|p| p.plugin_id.clone())
        .collect()
}

#[test]
fn group_wraps_selected_devices_into_a_parallel_and_ungroup_restores_them() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let (_track_id, [synth, bitcrush, delay]) = setup_chain(&mut app);

    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush, delay] });
    let song = app.cur.song_doc.song();
    let devices = &song.tracks[0].devices;
    assert_eq!(devices.len(), 2, "synth + Parallel");
    assert_eq!(devices[0].id(), synth);
    let parallel = devices[1].as_parallel().expect("2 つ目は Parallel");
    assert_eq!(parallel.chains.len(), 1);
    assert_eq!(
        parallel.chains[0].devices.iter().map(Device::id).collect::<Vec<_>>(),
        vec![bitcrush, delay],
        "選んだ device が chain 1 本に順序どおり入る"
    );
    assert_ne!(parallel.id, 0);
    assert_ne!(parallel.chains[0].id, 0);
    // 信号順は変わらない。
    assert_eq!(plugin_ids_in_order(&app), vec!["test.synth", "test.bitcrush", "test.delay"]);

    let parallel_id = parallel.id;
    app.handle_event(AppEvent::UngroupParallel { parallel_id });
    let devices = &app.cur.song_doc.song().tracks[0].devices;
    assert_eq!(
        devices.iter().map(Device::id).collect::<Vec<_>>(),
        vec![synth, bitcrush, delay],
        "Ungroup で直列に戻る (id は据え置き)"
    );
}

#[test]
fn chain_rows_put_each_open_chains_devices_under_its_row() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let (track_id, [synth, bitcrush, delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();
    app.handle_event(AppEvent::AddParallelChain { parallel_id });
    let (chain_a, chain_b) = {
        let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
        (r.chains[0].id, r.chains[1].id)
    };
    // 既定は全 chain 展開: 各 chain の中身はその chain 行の直下、`+ chain` は一番下。
    let rows = app.chain_rows();
    let kinds: Vec<String> = rows
        .iter()
        .map(|r| match &r.kind {
            ChainRowKind::Plugin(e) => format!("P{}", e.device_id),
            ChainRowKind::ParallelBegin { parallel_id, .. } => format!("RB{parallel_id}"),
            ChainRowKind::SplitParams { parallel_id, .. } => format!("S{parallel_id}"),
            ChainRowKind::Chain { chain_id, open, .. } => {
                format!("C{chain_id}{}", if *open { "*" } else { "" })
            }
            ChainRowKind::AddChain { .. } => "+c".into(),
            ChainRowKind::AddPlugin { chain } => match chain {
                ChainRef::Track(_) => "+pT".into(),
                ChainRef::Chain(c) => format!("+p{c}"),
            },
            ChainRowKind::ParallelEnd { .. } => "RE".into(),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            format!("P{synth}"),
            format!("RB{parallel_id}"),
            format!("C{chain_a}*"),
            format!("P{bitcrush}"),
            format!("+p{chain_a}"),
            format!("C{chain_b}*"),
            format!("+p{chain_b}"),
            "+c".to_string(),
            "RE".to_string(),
            format!("P{delay}"),
            "+pT".to_string(),
        ],
    );
    // A を閉じると bitcrush の行が消え、B は開いたまま。 もう一度で戻る。
    app.handle_event(AppEvent::ToggleParallelNodeCollapsed { id: chain_a });
    let rows = app.chain_rows();
    assert!(!rows.iter().any(|r| matches!(&r.kind, ChainRowKind::Plugin(e) if e.device_id == bitcrush)));
    assert!(rows.iter().any(|r| matches!(&r.kind, ChainRowKind::AddPlugin { chain: ChainRef::Chain(c) } if *c == chain_b)));
    app.handle_event(AppEvent::ToggleParallelNodeCollapsed { id: chain_a });
    assert_eq!(app.chain_rows().iter().filter(|r| matches!(r.kind, ChainRowKind::Plugin(_))).count(), 3);
    // Parallel ごと畳むと開始行 1 本だけ (chain 行も終了行も無い)。
    app.handle_event(AppEvent::ToggleParallelNodeCollapsed { id: parallel_id });
    let rows = app.chain_rows();
    assert!(rows.iter().any(|r| matches!(&r.kind, ChainRowKind::ParallelBegin { open: false, .. })));
    assert!(!rows.iter().any(|r| matches!(r.kind, ChainRowKind::Chain { .. } | ChainRowKind::ParallelEnd { .. })));
    app.handle_event(AppEvent::ToggleParallelNodeCollapsed { id: parallel_id });
    // 自動色: Parallel と chain、兄弟 chain、入れ子の Parallel / chain がそれぞれ別の色。
    let (rc, ca, cb) = {
        let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
        (r.color.expect("parallel color"), r.chains[0].color.expect("chain color"), r.chains[1].color.expect("chain color"))
    };
    assert!(rc != ca && rc != cb && ca != cb, "{rc:?} {ca:?} {cb:?}");
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let inner = app.cur.song_doc.song().chain_by_id(chain_a).unwrap().1.devices[0].as_parallel().unwrap().clone();
    let (irc, ica) = (inner.color.unwrap(), inner.chains[0].color.unwrap());
    assert!(![rc, ca].contains(&irc) && ![rc, ca, irc].contains(&ica), "入れ子は外側と別の色: {irc:?} {ica:?}");
    let _ = track_id;
}

#[test]
fn relocate_moves_a_device_into_a_parallel_chain_and_back() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let (track_id, [synth, bitcrush, delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();
    let chain_id = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].id;

    // delay を chain の中 (bitcrush の後ろ) へ。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![delay],
        dest: ChainRef::Chain(chain_id),
        dest_index: 1,
        copy: false,
    }));
    flush_states(&mut app);
    let song = app.cur.song_doc.song();
    assert_eq!(song.tracks[0].devices.len(), 2, "top-level は synth + Parallel");
    let chain = song.chain_by_id(chain_id).unwrap().1;
    assert_eq!(chain.devices.iter().map(Device::id).collect::<Vec<_>>(), vec![bitcrush, delay]);
    assert_eq!(song.find_device(delay), Some((ChainRef::Chain(chain_id), 1)));

    // Parallel ごと top-level 先頭へ (synth の前)。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![parallel_id],
        dest: ChainRef::Track(track_id),
        dest_index: 0,
        copy: false,
    }));
    flush_states(&mut app);
    let ids: Vec<u64> = app.cur.song_doc.song().tracks[0].devices.iter().map(Device::id).collect();
    assert_eq!(ids, vec![parallel_id, synth]);
    assert_eq!(plugin_ids_in_order(&app), vec!["test.bitcrush", "test.delay", "test.synth"]);

    // Parallel を自分の中の chain へは落とせない (循環)。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![parallel_id],
        dest: ChainRef::Chain(chain_id),
        dest_index: 0,
        copy: false,
    }));
    flush_states(&mut app);
    let ids: Vec<u64> = app.cur.song_doc.song().tracks[0].devices.iter().map(Device::id).collect();
    assert_eq!(ids, vec![parallel_id, synth], "自分の中への移動は無視される");
}

#[test]
fn removing_a_chain_unloads_its_plugins_and_keeps_the_parallel() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let (_track_id, [_synth, bitcrush, delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush, delay] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();
    app.handle_event(AppEvent::AddParallelChain { parallel_id });
    let chain_a = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].id;
    let _ = super::support::drain(&mut plugin_rx);

    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![chain_a] });
    flush_states(&mut app);
    let song = app.cur.song_doc.song();
    let parallel = song.parallel_by_id(parallel_id).expect("Parallel 自体は残る");
    assert_eq!(parallel.chains.len(), 1, "chain A が消えて B だけ");
    assert!(song.plugin_by_id(bitcrush).is_none() && song.plugin_by_id(delay).is_none());
    let msgs = super::support::drain(&mut plugin_rx);
    let removed: Vec<u64> = msgs
        .iter()
        .filter_map(|m| match m {
            common::protocol::PluginCommand::RemoveSlotPlugin { device: common::protocol::DeviceAddr { device_id, .. } } => Some(*device_id),
            _ => None,
        })
        .collect();
    assert!(removed.contains(&bitcrush) && removed.contains(&delay), "中の plugin は host からも外す: {msgs:?}");
}

#[test]
fn sidechain_source_can_be_a_chain_of_the_same_track() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let (_track_id, [_synth, bitcrush, delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();
    let chain_a = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap().chains[0].id;
    // 候補に 「Parallel / Chain 1」 が出る。
    let choices = app.sidechain_source_choices();
    assert!(
        choices.iter().any(|c| c.source == Some(TapSource::Chain(chain_a))),
        "chain が source 候補に並ぶ: {choices:?}"
    );
    app.handle_event(AppEvent::SetSidechainSource {
        device_id: delay,
        port: 0,
        source: Some(TapSource::Chain(chain_a)),
    });
    app.handle_event(AppEvent::SetAuxInputTapPoint { device_id: delay, port: 0, tap_point: TapPoint::PostFx });
    let p = app.cur.song_doc.song().plugin_by_id(delay).unwrap();
    let tap = p.aux_inputs[0].unwrap().tap;
    assert_eq!(tap.source, TapSource::Chain(chain_a));
    assert_eq!(tap.tap_point, TapPoint::PostFx);
    // 旧 JSON 互換: track source は `source_track`、chain source は `source_chain` で保存される。
    let json = serde_json::to_string(&tap).unwrap();
    assert!(json.contains("\"source_chain\""), "{json}");
}

/// ヘッダ行の出力 trim / Match: Song が書き換わり、値のみの IPC が engine へ飛ぶ
/// (chain mixer と同じ経路、再 compile は要らない)。
#[test]
fn parallel_out_gain_and_gain_match_update_song_and_send_value_only_commands() {
    use common::protocol::AudioCommand;
    use daw_gui::handler::parallel::ParallelMixerEdit;
    let (mut app, mut audio_rx, _plugin_rx, _proxy) = build_app();
    let (track_id, [_synth, bitcrush, _delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();
    let _ = super::support::drain(&mut audio_rx);

    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::OutGain(0.5) });
    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::GainMatch(true) });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!((r.out_gain, r.gain_match), (0.5, true));
    let cmds = super::support::drain(&mut audio_rx);
    assert!(
        cmds.iter().any(|c| matches!(c, AudioCommand::SetParallelOutGain { project: _, track, parallel_id: p, gain }
            if *track == track_id && *p == parallel_id && *gain == 0.5)),
        "{cmds:?}"
    );
    assert!(cmds.iter().any(|c| matches!(c, AudioCommand::SetParallelGainMatch { project: _, parallel_id: p, on: true, .. } if *p == parallel_id)));
    assert!(!cmds.iter().any(|c| matches!(c, AudioCommand::LoadSong { .. })), "値のみ更新は再 compile しない");
    // 同じ値をもう一度 → 何も送らない。
    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::GainMatch(true) });
    assert!(super::support::drain(&mut audio_rx).is_empty());
}

/// r.md #112: 帯域分割 on で chain が 3 本に補われ、 既定名の chain は帯域名に付け替わる
/// (ユーザーが付けた名前は据え置き)。 Split の param 行がヘッダ直下に出る。 off に戻しても chain は
/// 残り、 既定名は `Chain N` に戻る。
#[test]
fn enabling_frequency_split_pads_chains_to_three_and_shows_the_split_row() {
    use common::model::Split;
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let (_track_id, [_synth, bitcrush, _delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();

    app.handle_event(AppEvent::SetParallelSplit { parallel_id, split: Split::DEFAULT_FREQUENCY3 });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.split, Split::Frequency3 { low_hz: 200.0, high_hz: 2_000.0 });
    assert_eq!(
        r.chains.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["Low", "Mid", "High"],
        "既定名 `Chain 1` は帯域名へ、 足りない帯域ぶんは帯域名で補う"
    );
    assert_eq!(r.chains[0].devices.len(), 1, "既存 chain の中身はそのまま");
    assert!(r.chains.iter().all(|c| c.id != 0), "補った chain も採番済み");
    let kinds: Vec<String> = app
        .chain_rows()
        .iter()
        .map(|r| match &r.kind {
            ChainRowKind::ParallelBegin { .. } => "RB".to_string(),
            ChainRowKind::SplitParams { split: Split::Frequency3 { .. }, .. } => "SPLIT".to_string(),
            ChainRowKind::Chain { name, .. } => format!("C:{name}"),
            _ => "-".to_string(),
        })
        .filter(|k| k != "-")
        .collect();
    assert_eq!(kinds, vec!["RB", "SPLIT", "C:Low", "C:Mid", "C:High"], "param 行はヘッダ直下");

    // ユーザーが付けた名前は切替で触らない。
    let mid_id = r.chains[1].id;
    app.handle_event(AppEvent::RenameParallelChain { chain_id: mid_id, name: "Comp".into() });
    app.handle_event(AppEvent::SetParallelSplit { parallel_id, split: Split::None });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.split, Split::None);
    assert_eq!(
        r.chains.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["Chain 1", "Comp", "Chain 3"],
        "off に戻しても chain は消さず、 既定名だけ `Chain N` に戻る"
    );
    assert!(!app.chain_rows().iter().any(|r| matches!(r.kind, ChainRowKind::SplitParams { .. })));
}

/// r.md #112: クロスオーバーは値のみ IPC (再 compile なし)。 交差させると相手側が押される。
#[test]
fn split_frequency_edits_keep_order_and_send_value_only_commands() {
    use common::model::{Split, SplitEdge};
    use common::protocol::AudioCommand;
    use daw_gui::handler::parallel::ParallelMixerEdit;
    let (mut app, mut audio_rx, _plugin_rx, _proxy) = build_app();
    let (track_id, [_synth, bitcrush, _delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();
    app.handle_event(AppEvent::SetParallelSplit { parallel_id, split: Split::DEFAULT_FREQUENCY3 });
    let _ = super::support::drain(&mut audio_rx);

    app.handle_event(AppEvent::SetParallelMixer {
        parallel_id,
        edit: ParallelMixerEdit::SplitFreq { edge: SplitEdge::LowMid, hz: 3_000.0 },
    });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.split, Split::Frequency3 { low_hz: 3_000.0, high_hz: 3_000.0 }, "Low|Mid が Mid|High を押し上げる");
    let cmds = super::support::drain(&mut audio_rx);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            AudioCommand::SetParallelSplitFreq { project: _, track, parallel_id: p, edge: SplitEdge::LowMid, hz }
                if *track == track_id && *p == parallel_id && *hz == 3_000.0
        )),
        "{cmds:?}"
    );
    assert!(!cmds.iter().any(|c| matches!(c, AudioCommand::LoadSong { .. })), "値のみ更新は再 compile しない");

    // 値域外は端へ。 同じ値をもう一度 → 何も送らない。
    app.handle_event(AppEvent::SetParallelMixer {
        parallel_id,
        edit: ParallelMixerEdit::SplitFreq { edge: SplitEdge::MidHigh, hz: 99_999.0 },
    });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.split.freq(SplitEdge::MidHigh), Some(20_000.0));
    let _ = super::support::drain(&mut audio_rx);
    app.handle_event(AppEvent::SetParallelMixer {
        parallel_id,
        edit: ParallelMixerEdit::SplitFreq { edge: SplitEdge::MidHigh, hz: 20_000.0 },
    });
    assert!(super::support::drain(&mut audio_rx).is_empty());
}

/// r.md #112: Mid/Side は chain を 2 本に補い (Mid / Side)、 param 行は出ない。
#[test]
fn enabling_mid_side_split_pads_chains_to_two_without_a_params_row() {
    use common::model::Split;
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let (_track_id, [_synth, bitcrush, _delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();

    app.handle_event(AppEvent::SetParallelSplit { parallel_id, split: Split::MidSide });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.split, Split::MidSide);
    assert_eq!(r.chains.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["Mid", "Side"]);
    assert!(!app.chain_rows().iter().any(|r| matches!(r.kind, ChainRowKind::SplitParams { .. })));
}

/// r.md #114: Selector は chain を 2 本 (A/B) に補い、 アクティブ chain は先頭の実 id に解決される。
/// `Active` の切替は値のみ IPC (再 compile なし) で、 chain 行はアクティブ以外が `inactive`。
/// アクティブ chain を消すと先頭へ落ちる (補償コード無し)。
#[test]
fn selector_pads_to_two_chains_and_switches_the_active_chain_value_only() {
    use common::model::Split;
    use common::protocol::AudioCommand;
    use daw_gui::handler::parallel::ParallelMixerEdit;
    let (mut app, mut audio_rx, _plugin_rx, _proxy) = build_app();
    let (track_id, [_synth, bitcrush, _delay]) = setup_chain(&mut app);
    app.handle_event(AppEvent::GroupDevices { device_ids: vec![bitcrush] });
    let parallel_id = app.cur.song_doc.song().tracks[0].devices[1].id();

    app.handle_event(AppEvent::SetParallelSplit { parallel_id, split: Split::DEFAULT_SELECTOR });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    let [a, b] = [r.chains[0].id, r.chains[1].id];
    assert_eq!(r.split, Split::Selector { active_chain: a, fade_ms: Split::DEFAULT_SELECTOR_FADE_MS }, "先頭 chain の実 id");
    assert_eq!(r.chains.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["Chain 1", "Chain 2"]);
    let inactive = |app: &daw_gui::app::AppData| -> Vec<bool> {
        app.chain_rows()
            .iter()
            .filter_map(|r| match &r.kind {
                ChainRowKind::Chain { inactive, .. } => Some(*inactive),
                _ => None,
            })
            .collect()
    };
    assert!(app.chain_rows().iter().any(|r| matches!(r.kind, ChainRowKind::SplitParams { .. })), "Active / Fade の param 行");
    assert_eq!(inactive(&app), vec![false, true]);
    let _ = super::support::drain(&mut audio_rx);

    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::ActiveChain(b) });
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.active_chain_index(), Some(1));
    assert_eq!(inactive(&app), vec![true, false]);
    let cmds = super::support::drain(&mut audio_rx);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            AudioCommand::SetParallelActiveChain { project: _, track, parallel_id: p, chain_id }
                if *track == track_id && *p == parallel_id && *chain_id == b
        )),
        "{cmds:?}"
    );
    assert!(!cmds.iter().any(|c| matches!(c, AudioCommand::LoadSong { .. })), "値のみ更新は再 compile しない");

    // fade も値のみ。 無い chain / 同じ chain は何も送らない。
    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::SelectorFade(120.0) });
    assert_eq!(app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap().split.selector_fade_ms(), Some(120.0));
    let cmds = super::support::drain(&mut audio_rx);
    assert!(cmds.iter().any(|c| matches!(c, AudioCommand::SetParallelSelectorFade { project: _, fade_ms, .. } if *fade_ms == 120.0)));
    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::ActiveChain(b) });
    app.handle_event(AppEvent::SetParallelMixer { parallel_id, edit: ParallelMixerEdit::ActiveChain(9_999) });
    assert!(super::support::drain(&mut audio_rx).is_empty());

    // アクティブ chain (b) を消す → 先頭 (a) がアクティブ。
    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![b] });
    flush_states(&mut app);
    let r = app.cur.song_doc.song().parallel_by_id(parallel_id).unwrap();
    assert_eq!(r.chains.len(), 1);
    assert_eq!(r.active_chain_index(), Some(0));
    assert_eq!(inactive(&app), vec![false]);
}
