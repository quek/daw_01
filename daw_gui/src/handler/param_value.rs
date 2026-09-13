//! handler::param_value — パラメーターの**現在値** (plain 単位)・**持ち主** (lane / routing を置く
//! store) と、 それを使う「最後に触ったパラメーターのオートメーションレーンを足す」 (`A` キー)。
//!
//! r.md #129 (`docs/plan_rack_native_devices.md` §7.5 / §7.7) で口を 1 本ずつにした:
//! - 現在値は [`AppData::target_plain_value`] — レーン既定値・録音点・`A` キーが共有する。
//! - 持ち主は [`param_owner`] — `A` キー・last touched・録音レーン・変調 routing が共有する。
//! - last touched の記録は [`AppData::note_touched_target`]。
use common::model::{AutomationTarget, Song};

use crate::app_types::*;
use crate::state::*;

/// `target` の lane / routing を置く store の持ち主 (track id か `MASTER_TRACK_ID`)。
///
/// 置き場の規則は `Song::bound_owner_track` が持ち (device / chain / Parallel / 変調は束縛先の
/// 所属、song-wide param は master)、ここは target だけでは決まらない住所 (Volume / Pan / Mute /
/// SendGain / Image / Text / Group) を `fallback_owner` (= 呼び出し側の track) で埋めるだけ。
/// `None` = id で束縛する住所なのに束縛先が居ない (削除された)。
///
/// view から渡る track id は、同じフレームで device を他トラックへ運んだ後だと古いことがあるので、
/// id で束縛する住所は **必ずここで** 実行時の Song から引き直す。
pub(crate) fn param_owner(song: &Song, target: &AutomationTarget, fallback_owner: u32) -> Option<u32> {
    use AutomationTarget as T;
    match song.bound_owner_track(target) {
        Some(owner) => Some(owner),
        None if target.bound_node_id().is_some() || matches!(target, T::ModSourceParam { .. } | T::ModRoutingDepth { .. }) => {
            None
        }
        None => Some(fallback_owner),
    }
}

/// 最後に触ったパラメーターの lane / routing を置く store の持ち主。束縛先が居ない / 住所がその種類に無い /
/// 持ち主の store (トラック) が消えたなら `None` (= 「対象が削除された」)。
///
/// 解決の規則は enforce と同じ `Song::param_target_resolves` なので、`Some` の持ち主へ積んだレーンは同じ編集の
/// 中で消されない。`A` キーと、消えた対象を指す session 状態の掃除 (`reconcile_song_refs`) が共有する
/// (録音レーン / 値保持レーン / 変調を積む口も、作るときは同じ述語で判定する)。
pub(crate) fn touched_param_owner(song: &Song, touched: &TouchedParam) -> Option<u32> {
    let owner = param_owner(song, &touched.target, touched.track_id)?;
    (song.param_stores(owner).is_some() && song.param_target_resolves(&touched.target, owner)).then_some(owner)
}

impl AppData {
    /// `A` キー shortcut の handler。`last_touched_param` の lane を
    /// その持ち主の store に追加 (or 既存があれば visible = true で復活)。
    /// 仕様: `docs/plan_automation.md` §7.3。
    pub(crate) fn add_automation_from_last_touched(&mut self) {
        let Some(touched) = self.cur.peph.last_touched_param.clone() else {
            self.ui_ephemeral.status_message =
                "No parameter touched yet — drag any knob first".into();
            return;
        };
        // r.md #129 (§7.7): 持ち主は target の束縛先が決める (device を他トラックへ運んだ後でも、
        // master fx chain の device でも正しい store に積む)。置き場の分岐は `param_stores` 1 か所。
        // 束縛先が解決しない (消えた / 種類が違う) なら積まない — 積むと enforce が同じ編集の中で消し、
        // 中身の無い undo step と `*` だけが残る。
        let song = self.cur.song_doc.song();
        let Some(owner) = touched_param_owner(song, &touched) else {
            self.cur.peph.last_touched_param = None;
            self.ui_ephemeral.status_message = "Last-touched parameter was removed".into();
            return;
        };
        let existing = song
            .param_stores(owner)
            .and_then(|(lanes, _)| lanes.iter().find(|l| l.target == touched.target).map(|l| l.id));
        if let Some(lane_id) = existing {
            // 既存 lane を visible / enabled = true に戻して expand。
            self.cur.view
                .hidden_automation_lanes
                .remove(&common::model::AutomationLaneKey { track: owner, lane: lane_id });
            self.edit_song_checked(|song| {
                if let Some(lane) = song.automation_lane_by_key_mut(owner, lane_id)
                    && !lane.enabled
                {
                    lane.enabled = true;
                    true
                } else {
                    false
                }
            });
            self.expand_automation_of(owner);
            self.ui_ephemeral.status_message = format!(
                "Automation lane '{}' は既に存在します",
                touched.display_name
            );
            return;
        }
        // 新規 lane を作成。default_value は target の現在値。native の値は `params` にあるので、
        // PluginParam 式の隠しレーンは作らない。
        let default_value = self.lane_default_for_target(&touched);
        let lane = common::model::AutomationLane::new(touched.target.clone(), default_value);
        if !self.edit_song_checked(|song| song.push_lane(owner, lane).is_some()) {
            return;
        }
        self.expand_automation_of(owner);
        self.ui_ephemeral.status_message = format!(
            "Added automation lane: {}",
            touched.display_name
        );
    }

    /// `owner` (track id か `MASTER_TRACK_ID`) のオートメーション行を開く。
    pub(crate) fn expand_automation_of(&mut self, owner: u32) {
        if owner == common::model::MASTER_TRACK_ID {
            self.cur.view.master_row_automation_expanded = true;
        } else {
            self.cur.view.expanded_automation_tracks.insert(owner);
        }
    }

    /// 「最後に触ったパラメーター」を記録する唯一の口 (`A` キー / MIDI Learn の的)。
    /// 値を触る経路 (native の値編集 / 単体 native の bypass / Limiter / 変調のツマミと深さ) は
    /// **必ずここを呼ぶ** (呼ばないとその param だけ `A` でレーンを作れない)。
    ///
    /// 持ち主は [`param_owner`]、名前は `automation_target_label`。id で束縛する住所の束縛先が
    /// 居なければ記録しない。
    pub(crate) fn note_touched_target(&mut self, target: AutomationTarget, fallback_owner: u32) {
        let Some(track_id) = param_owner(self.cur.song_doc.song(), &target, fallback_owner) else {
            return;
        };
        let display_name = self.automation_target_label(&target);
        self.cur.peph.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
    }

    /// `target` の現在値 (plain)。レーン既定値 ([`Self::lane_default_for_target`])・録音点
    /// (`record_automation_points_for_tick`)・`A` キーが共有する**唯一の口**。
    ///
    /// `owner` は target だけでは決まらない住所 (Volume / Pan / Mute / SendGain / Image / Text /
    /// Group) の track。id で束縛する住所は **track を引く前に** id で解決するので `owner` を見ない
    /// (master fx chain の Chain / Parallel / native も同じ口で値が出る)。`None` = 値の出所が無い
    /// (束縛先が居ない / plugin がまだ値を報告していない / 表示 clip が無い)。
    pub(crate) fn target_plain_value(&self, owner: u32, target: &AutomationTarget) -> Option<f64> {
        use AutomationTarget as T;
        let song = self.cur.song_doc.song();
        match target {
            T::NativeParam { device_id, param } => song.native_by_id(*device_id)?.param(*param).map(f64::from),
            T::MasterLimiter(p) => Some(f64::from(song.master_limiter.param(*p))),
            T::TrackBuiltin(param) => track_builtin_value(song, owner, param),
            // plugin の値は Song に無い。GUI の cache (`PluginParamValueChanged` / ノブ操作で更新) を引く。
            T::PluginParam { device_id, param_id, .. } => {
                song.plugin_by_id(*device_id)?;
                self.cur.pipc
                    .plugin_param_values
                    .get(&DeviceParamKey { device_id: *device_id, param_id: *param_id })
                    .copied()
            }
            T::SongTempo => Some(f64::from(song.bpm)),
            T::SongTimeSigNumerator => Some(f64::from(song.time_sig.0)),
            // Image / Text PiP: 同 track の first event (セル込み) の field 値 (`docs/plan_image_automation.md`
            // §4)。drag が event の field を更新 → ここで読み直す → 録音が点を打つ。
            T::ImageBuiltin(field) => first_image_event(song, owner).map(|ev| image_builtin_value(ev, *field)),
            T::TextBuiltin(field) => first_text_event(song, owner).map(|ev| text_builtin_value(ev, *field)),
            // group は表示 clip を持たないので track の group_transform (無ければ恒等) を読む。
            T::GroupTransform(p) => song
                .track_by_id(owner)
                .map(|t| f64::from(group_transform_field(&t.group_transform.unwrap_or_default(), *p))),
            // r.md #89: モジュレーター自身のツマミ / 変調 1 本の深さ。
            T::ModSourceParam { source_id, param } => self.mod_param_plain(*source_id, *param),
            T::ModRoutingDepth { routing_id } => {
                song.all_mod_routings().find(|r| r.id == *routing_id).map(|r| f64::from(r.depth))
            }
        }
    }

    /// 新しいレーンの `default_value` (plain)。`target_plain_value` が値を出せない住所は
    /// 種類ごとの常識値 ([`fallback_plain`]: model の既定値)。
    pub(crate) fn lane_default_for_target(&self, touched: &TouchedParam) -> f64 {
        self.target_plain_value(touched.track_id, &touched.target)
            .unwrap_or_else(|| fallback_plain(&touched.target))
    }
}

/// `TrackBuiltin` の現在値。Volume / Pan / Mute / SendGain は `owner` の track、Chain / Parallel は
/// id (master fx chain の中でも引ける)。
fn track_builtin_value(song: &Song, owner: u32, param: &common::model::TrackBuiltinParam) -> Option<f64> {
    use common::model::TrackBuiltinParam as B;
    match param {
        B::Volume => song.track_by_id(owner).map(|t| f64::from(t.volume)),
        B::Pan => song.track_by_id(owner).map(|t| f64::from(t.pan)),
        B::Mute => song.track_by_id(owner).map(|t| f64::from(u8::from(t.muted))),
        // A6 (r.md #8) / v29: send は安定 send id で引く。
        B::SendGain { send_id, .. } => {
            song.track_by_id(owner)?.sends.iter().find(|s| s.id == *send_id).map(|s| f64::from(s.gain))
        }
        // r.md #110: Parallel chain の gain / pan (安定 chain id で引く)。
        B::ChainGain { chain_id } => song.chain_by_id(*chain_id).map(|(_, c)| f64::from(c.gain)),
        B::ChainPan { chain_id } => song.chain_by_id(*chain_id).map(|(_, c)| f64::from(c.pan)),
        B::ParallelOutGain { parallel_id } => song.parallel_by_id(*parallel_id).map(|r| f64::from(r.out_gain)),
        // r.md #112: 分割が off の Parallel は既定の境界周波数を出す。
        B::ParallelSplitFreq { parallel_id, edge } => song
            .parallel_by_id(*parallel_id)?
            .split
            .freq(*edge)
            .or_else(|| common::model::Split::DEFAULT_FREQUENCY3.freq(*edge))
            .map(f64::from),
        // r.md #114: アクティブ chain の中央の位置 (Selector でなければ中央 0.5)。
        B::ParallelSelect { parallel_id } => song.parallel_by_id(*parallel_id).map(|r| f64::from(r.select_pos())),
    }
}

/// `track_id` の最初の image event (arrangement と launcher のどちらの clip でも)。
fn first_image_event(song: &Song, track_id: u32) -> Option<&common::model::ImageEvent> {
    song.track_by_id(track_id)?.all_clips().find_map(|c| match song.clip_contents.get(&c.content_id) {
        Some(common::model::ClipContent::Image(img)) => img.events.first(),
        _ => None,
    })
}

/// `track_id` の最初の text event (arrangement と launcher のどちらの clip でも)。
fn first_text_event(song: &Song, track_id: u32) -> Option<&common::model::TextEvent> {
    song.track_by_id(track_id)?.all_clips().find_map(|c| match song.clip_contents.get(&c.content_id) {
        Some(common::model::ClipContent::Text(t)) => t.events.first(),
        _ => None,
    })
}

fn image_builtin_value(ev: &common::model::ImageEvent, field: common::model::ImageBuiltinParam) -> f64 {
    use common::model::ImageBuiltinParam as I;
    f64::from(match field {
        I::X => ev.x,
        I::Y => ev.y,
        I::W => ev.w,
        I::H => ev.h,
        I::Opacity => ev.opacity,
        I::Rotation => ev.rotation_radians,
    })
}

fn text_builtin_value(ev: &common::model::TextEvent, field: common::model::TextBuiltinParam) -> f64 {
    use common::model::TextBuiltinParam as T;
    f64::from(match field {
        T::X => ev.x,
        T::Y => ev.y,
        T::W => ev.w,
        T::H => ev.h,
        T::Opacity => ev.opacity,
        T::Rotation => ev.rotation_radians,
        T::FontSize => ev.font_size_px,
        T::FillR => ev.fill_color[0],
        T::FillG => ev.fill_color[1],
        T::FillB => ev.fill_color[2],
        T::FillA => ev.fill_color[3],
        T::OutlineR => ev.outline_color[0],
        T::OutlineG => ev.outline_color[1],
        T::OutlineB => ev.outline_color[2],
        T::OutlineA => ev.outline_color[3],
        T::OutlineWidth => ev.outline_width_px,
        T::ShadowR => ev.shadow_color[0],
        T::ShadowG => ev.shadow_color[1],
        T::ShadowB => ev.shadow_color[2],
        T::ShadowA => ev.shadow_color[3],
        T::ShadowOffsetX => ev.shadow_offset_px.0,
        T::ShadowOffsetY => ev.shadow_offset_px.1,
        T::ShadowBlur => ev.shadow_blur_px,
    })
}

/// 現在値の出所が無い住所のレーン既定値 = **model の既定値** (新しく作ったときにその param が
/// 持つ値)。表 (数値) を複製せず、各 model の `Default` / `default_plain` を引く。
fn fallback_plain(target: &AutomationTarget) -> f64 {
    use AutomationTarget as T;
    use common::model::TrackBuiltinParam as B;
    match target {
        T::NativeParam { param, .. } => param.default_plain().map_or(0.0, f64::from),
        T::MasterLimiter(p) => f64::from(p.default_plain()),
        // gain は unity (0 にすると無音のレーンができる)、pan / mute は中立。
        T::TrackBuiltin(B::Volume | B::SendGain { .. } | B::ChainGain { .. } | B::ParallelOutGain { .. }) => 1.0,
        T::TrackBuiltin(B::Pan | B::ChainPan { .. } | B::Mute) => 0.0,
        T::TrackBuiltin(B::ParallelSplitFreq { edge, .. }) => {
            common::model::Split::DEFAULT_FREQUENCY3.freq(*edge).map_or(0.0, f64::from)
        }
        // Selector でない Parallel の位置 (`Parallel::select_pos` と同じ中央)。
        T::TrackBuiltin(B::ParallelSelect { .. }) => 0.5,
        T::SongTempo => f64::from(Song::default().bpm),
        T::SongTimeSigNumerator => f64::from(Song::default().time_sig.0),
        T::ImageBuiltin(field) => image_builtin_value(&common::model::ImageEvent::default(), *field),
        T::TextBuiltin(field) => text_builtin_value(&common::model::TextEvent::default(), *field),
        T::GroupTransform(p) => f64::from(group_transform_field(&common::model::GroupTransform::default(), *p)),
        T::PluginParam { .. } | T::ModSourceParam { .. } | T::ModRoutingDepth { .. } => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use common::model::{
        AutomationTarget, BindingTarget, BusCompParam, ChainRef, CompParam, Device, MASTER_TRACK_ID, NativeKind,
        NativeParamId, Parallel, RecordingMode, TrackBuiltinParam,
    };
    use common::protocol::AudioCommand;

    use crate::app_types::ModSourceKindTag;
    use crate::device_addr::{InsertAt, RelocateDevices};
    use crate::event::AppEvent;
    use crate::event_device::DeviceEvent;
    use crate::event_native::NativeEdit;
    use crate::state::{AppData, ParamSurface};

    fn thr() -> NativeParamId {
        NativeParamId::Comp(CompParam::Threshold)
    }

    fn device(app: &mut AppData, ev: DeviceEvent) {
        app.handle_event(AppEvent::Device(ev));
    }

    /// 先頭トラックと、追加した 2 本目のトラック。
    fn two_tracks(app: &mut AppData) -> (u32, u32) {
        let t1 = app.cur.song_doc.song().tracks[0].id;
        app.handle_event(AppEvent::AddInstrumentTrack);
        let t2 = app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|&id| id != t1).expect("2 本目");
        (t1, t2)
    }

    /// `track` に追加分の Comp ("Comp 2") を足してその id を返す。
    fn add_comp(app: &mut AppData, track: u32) -> u64 {
        device(app, DeviceEvent::AddNative { chain: ChainRef::Track(track), kind: NativeKind::Comp, open_panel: false });
        app.cur.song_doc.song().track_by_id(track).expect("track").devices.iter()
            .filter_map(Device::as_native)
            .find(|n| !n.builtin && n.kind() == NativeKind::Comp)
            .expect("追加分の Comp")
            .id
    }

    fn move_device(app: &mut AppData, id: u64, dest: u32) {
        device(
            app,
            DeviceEvent::RelocateDevices(RelocateDevices {
                device_ids: vec![id],
                dest: ChainRef::Track(dest),
                dest_index: InsertAt::Default,
                copy: false,
            }),
        );
        assert_eq!(app.cur.song_doc.song().device_owner_track(id), Some(dest), "運べている");
    }

    fn lane_default(app: &AppData, owner: u32, target: &AutomationTarget) -> Option<f64> {
        let (lanes, _) = app.cur.song_doc.song().param_stores(owner)?;
        lanes.iter().find(|l| l.target == *target).map(|l| l.default_value)
    }

    /// A-1 (T19): last touched のラベルは番号つきの device 名、A キーのレーンは target の持ち主の
    /// store (他トラックへ運んだ後は運び先、master の device は song 側) に現在値で積まれる。
    #[test]
    fn a_key_puts_native_lanes_in_the_owner_store_with_the_current_value() {
        let mut app = crate::test_support::headless_app();
        let (t1, t2) = two_tracks(&mut app);
        let c2 = add_comp(&mut app, t1);
        device(&mut app, DeviceEvent::NativeEdit { device_id: c2, edit: NativeEdit::param(thr(), -20.0) });
        assert_eq!(app.cur.peph.last_touched_param.as_ref().expect("touched").display_name, "Comp 2: Thr");

        move_device(&mut app, c2, t2);
        app.add_automation_from_last_touched();
        let target = AutomationTarget::NativeParam { device_id: c2, param: thr() };
        assert_eq!(lane_default(&app, t2, &target), Some(-20.0), "運び先の store に現在値で");
        assert_eq!(lane_default(&app, t1, &target), None, "触ったときのトラックには積まない");

        let bus = app.cur.song_doc.song().builtin_native(MASTER_TRACK_ID, NativeKind::BusComp).expect("bus").id;
        let bus_thr = NativeParamId::BusComp(BusCompParam::Threshold);
        device(&mut app, DeviceEvent::NativeEdit { device_id: bus, edit: NativeEdit::param(bus_thr, -12.0) });
        app.add_automation_from_last_touched();
        let bus_target = AutomationTarget::NativeParam { device_id: bus, param: bus_thr };
        assert_eq!(lane_default(&app, MASTER_TRACK_ID, &bus_target), Some(-12.0), "master の device は song_lanes");
    }

    /// A-2 (T20 / §18-S): Touch で再生中の gesture は、内蔵 device / SendGain / ChainGain でも点を打つ。
    #[test]
    fn touch_recording_writes_points_for_native_send_and_chain_targets() {
        let mut app = crate::test_support::headless_app();
        let (t1, t2) = two_tracks(&mut app);
        let comp = app.cur.song_doc.song().builtin_native(t1, NativeKind::Comp).expect("comp").id;
        app.handle_event(AppEvent::AddSend { src_track_id: t1, dest_track_id: t2 });
        let send_id = app.cur.song_doc.song().track_by_id(t1).expect("t1").sends[0].id;
        let chain_id = app
            .edit_song(|song| {
                let mut parallel = Parallel::new();
                parallel.id = song.alloc_device_id();
                parallel.chains[0].id = song.alloc_device_id();
                let chain_id = parallel.chains[0].id;
                song.insert_device(ChainRef::Track(t1), 0, Device::Parallel(parallel));
                chain_id
            })
            .expect("Parallel を挿す");
        let targets = [
            AutomationTarget::NativeParam { device_id: comp, param: thr() },
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id, legacy_send_idx: None }),
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id }),
        ];

        app.cur.recording.recording_mode = RecordingMode::Touch;
        app.cur.transport.is_playing = true;
        for target in &targets {
            app.handle_event(AppEvent::ParamGestureBegin { surface: ParamSurface::Rack, track_id: t1, target: target.clone() });
        }
        assert_eq!(app.record_automation_points_for_tick(1.0), targets.len(), "3 target とも 1 点ずつ");
        let song = app.cur.song_doc.song();
        let (lanes, _) = song.param_stores(t1).expect("t1");
        for target in &targets {
            let lane = lanes.iter().find(|l| l.target == *target).unwrap_or_else(|| panic!("{target:?} のレーン"));
            let points = song.clip_contents.get(&lane.clips[0].content_id).and_then(|c| c.automation_points());
            assert_eq!(points.map(<[_]>::len), Some(1), "{target:?}");
        }
    }

    /// 録音で作るレーン (`ensure_recording_lane_clip`) も束縛先が解決するときだけ積む: 種類違いの住所ではレーン /
    /// content / undo / `*` を増やさない (積むと enforce が同じ編集の中で消し、空の undo step と孤児の content が
    /// 残る)。いまの呼び出し元 (録音の tick) は値が引けない住所を先に飛ばすが、口そのものが規則を持つ。
    #[test]
    fn recording_lane_is_not_created_for_unresolvable_targets() {
        let mut app = crate::test_support::headless_app();
        let t1 = app.cur.song_doc.song().tracks[0].id;
        let eq = app.cur.song_doc.song().builtin_native(t1, NativeKind::Eq).expect("eq").id;
        let target = AutomationTarget::NativeParam { device_id: eq, param: thr() };
        app.cur.song_doc.mark_saved();
        let depth = app.cur.song_doc.undo_depth();
        let contents = app.cur.song_doc.song().clip_contents.len();

        assert_eq!(app.ensure_recording_lane_clip(t1, &target, 1.0), None);
        let song = app.cur.song_doc.song();
        assert!(song.param_stores(t1).expect("t1").0.iter().all(|l| l.target != target));
        assert_eq!(song.clip_contents.len(), contents, "content も採番しない");
        assert_eq!(app.cur.song_doc.undo_depth(), depth);
        assert!(!app.cur.song_doc.is_dirty());
    }

    /// A-3 (T26 / §18-T): master fx chain の Parallel の chain gain で A を押すと、既定値は chain の現在値
    /// (旧実装は track を先に引いて master で 0 = 無音のレーンを作っていた)。
    #[test]
    fn a_key_on_a_master_chain_gain_uses_the_chain_value() {
        let mut app = crate::test_support::headless_app();
        let chain_id = app
            .edit_song(|song| {
                let mut parallel = Parallel::new();
                parallel.id = song.alloc_device_id();
                parallel.chains[0].id = song.alloc_device_id();
                parallel.chains[0].gain = 0.5;
                let chain_id = parallel.chains[0].id;
                let at = song.default_insert_index(ChainRef::Track(MASTER_TRACK_ID)).expect("master");
                song.insert_device(ChainRef::Track(MASTER_TRACK_ID), at, Device::Parallel(parallel));
                chain_id
            })
            .expect("Parallel を挿す");
        let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id });
        app.handle_event(AppEvent::ParamGestureBegin {
            surface: ParamSurface::Rack,
            track_id: MASTER_TRACK_ID,
            target: target.clone(),
        });
        app.handle_event(AppEvent::ParamGestureEnd { surface: ParamSurface::Rack, track_id: MASTER_TRACK_ID, target: target.clone() });
        app.add_automation_from_last_touched();
        assert_eq!(lane_default(&app, MASTER_TRACK_ID, &target), Some(0.5));
    }

    /// A-4 (T27 / §18-V): device を他トラックへ運んだ直後に、view が古い track id で出した変調の
    /// 追加 / 深さ変更は、運び先の store に効く。
    #[test]
    fn mod_routing_edits_with_a_stale_track_id_land_in_the_owner_store() {
        let mut app = crate::test_support::headless_app();
        let (t1, t2) = two_tracks(&mut app);
        let c2 = add_comp(&mut app, t1);
        app.handle_event(AppEvent::AddModSource { kind: ModSourceKindTag::Lfo });
        let source_id = app.cur.song_doc.song().mod_sources.last().expect("source").id;
        move_device(&mut app, c2, t2);

        let target = AutomationTarget::NativeParam { device_id: c2, param: thr() };
        app.handle_event(AppEvent::AddModRouting { track_id: t1, target: target.clone(), source_id });
        app.handle_event(AppEvent::SetModRoutingDepth { track_id: t1, target: target.clone(), source_id, depth: 0.25 });
        let song = app.cur.song_doc.song();
        let depth_in = |owner: u32| {
            song.param_stores(owner)
                .expect("store")
                .1
                .iter()
                .find(|r| r.target == target && r.source_id == source_id)
                .map(|r| r.depth)
        };
        assert_eq!(depth_in(t2), Some(0.25), "運び先に積まれ、深さも運び先で変わる");
        assert_eq!(depth_in(t1), None);
    }

    /// A-5 (§7.9 / §18-X): 内蔵 device の param を触って Learn すると NativeParam に bind され、CC は
    /// ノブと同じ口 (値域 / 自動 ON / 値 IPC) を通る。device を消すと binding も消える。
    #[test]
    fn midi_learn_binds_native_params_and_drives_them_like_the_knob() {
        let mut app = crate::test_support::headless_app();
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::unbounded_channel();
        app.ipc.audio_tx = Some(audio_tx);
        let t1 = app.cur.song_doc.song().tracks[0].id;
        let c2 = add_comp(&mut app, t1);
        device(&mut app, DeviceEvent::NativeEdit { device_id: c2, edit: NativeEdit::param(thr(), -20.0) });

        let learn = app.midi_learn_binding_target(None);
        assert_eq!(learn, Some(BindingTarget::NativeParam { device_id: c2, param: thr() }), "Volume に落ちない");
        app.handle_event(AppEvent::StartMidiLearn(learn.expect("learn")));
        app.handle_event(AppEvent::MidiControlChange { channel: 0, controller: 21, value: 0 });
        assert!(app.cur.recording.midi_learn_target.is_none(), "CC 1 通で Learn が終わる");

        device(&mut app, DeviceEvent::SetDevicesBypassed { device_ids: vec![c2], bypassed: true });
        while audio_rx.try_recv().is_ok() {}
        app.handle_event(AppEvent::MidiControlChange { channel: 0, controller: 21, value: 127 });
        let dev = *app.cur.song_doc.song().native_by_id(c2).expect("c2");
        let top = thr().range().display_range().1;
        assert!((f64::from(dev.param(thr()).expect("thr")) - top).abs() < 1e-3, "CC127 は上限");
        assert!(!dev.bypassed, "ノブと同じく触れば ON");
        let mut sent = 0;
        while let Ok(cmd) = audio_rx.try_recv() {
            sent += usize::from(matches!(cmd, AudioCommand::SetNativeDevice { device_id, .. } if device_id == c2));
        }
        assert_eq!(sent, 1, "値 IPC は 1 通");

        device(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![c2] });
        assert!(
            !app.cur.song_doc.song().midi_bindings.iter().any(|b| matches!(b.target, BindingTarget::NativeParam { device_id, .. } if device_id == c2)),
            "device を消すと binding も消える"
        );
    }
}
