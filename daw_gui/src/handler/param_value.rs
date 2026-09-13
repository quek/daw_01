//! handler::param_value — パラメーターの**現在値** (plain 単位) と、 それを初期値に使う
//! 「最後に触ったパラメーターのオートメーションレーンを足す」 (`A` キー)。
//!
//! `handler/automation_lanes.rs` から機械分割した `impl AppData` メソッド群 (挙動は元と
//! 同一、 サイズ budget = 不変条件 9)。
use crate::app_types::*;
use crate::state::*;

/// id (device / chain / Parallel / 変調ソース / 変調) で束縛する住所か。`bound_owner_track` が
/// `None` を返したとき、束縛先が居ない (= 削除された) のか、呼び出し側の track が持ち主の
/// 住所なのかを分ける。
fn target_is_id_bound(target: &common::model::AutomationTarget) -> bool {
    use common::model::AutomationTarget as T;
    target.bound_node_id().is_some() || matches!(target, T::ModSourceParam { .. } | T::ModRoutingDepth { .. })
}

impl AppData {
    /// `A` キー shortcut の handler。`last_touched_param` の lane を
    /// 該当 track に追加 (or 既存があれば visible = true で復活)。
    /// 仕様: `docs/plan_automation.md` §7.3。
    pub(crate) fn add_automation_from_last_touched(&mut self) {
        let Some(touched) = self.cur.peph.last_touched_param.clone() else {
            self.ui_ephemeral.status_message =
                "No parameter touched yet — drag any knob first".into();
            return;
        };
        // r.md #129 (§7.7): 持ち主は target の束縛先が決める (device を他トラックへ運んだ後でも、
        // master fx chain の device でも正しい store に積む)。置き場の分岐は `param_stores` 1 か所。
        let song = self.cur.song_doc.song();
        let owner = match song.bound_owner_track(&touched.target) {
            Some(owner) => owner,
            None if target_is_id_bound(&touched.target) => {
                self.cur.peph.last_touched_param = None;
                self.ui_ephemeral.status_message = "Last-touched parameter was removed".into();
                return;
            }
            None => touched.track_id,
        };
        let Some((lanes, _)) = song.param_stores(owner) else {
            self.cur.peph.last_touched_param = None;
            self.ui_ephemeral.status_message =
                "Last-touched parameter's track was removed".into();
            return;
        };
        if let Some(lane_id) = lanes.iter().find(|l| l.target == touched.target).map(|l| l.id) {
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
        // 新規 lane を作成。default_value は target に応じて現在値を引く。native の値は
        // `params` にあるので、PluginParam 式の隠しレーンは作らない。
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
    fn expand_automation_of(&mut self, owner: u32) {
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
    /// 持ち主は `bound_owner_track` (id で束縛する住所の store の持ち主)、target だけでは
    /// 決まらない住所 (Volume / Pan など) は `fallback_owner`。id で束縛する住所の束縛先が
    /// 居なければ記録しない。名前は `automation_target_label`。
    pub(crate) fn note_touched_target(&mut self, target: common::model::AutomationTarget, fallback_owner: u32) {
        let track_id = match self.cur.song_doc.song().bound_owner_track(&target) {
            Some(owner) => owner,
            None if target_is_id_bound(&target) => return,
            None => fallback_owner,
        };
        let display_name = self.automation_target_label(&target);
        self.cur.peph.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
    }

    /// track-builtin target の現在値 (plain)。`lane_default_for_target` から
    /// 切り出したのは、内側の match がネスト段数の予算 (不変条件 9) を
    /// 押し上げるため — 値の取り出しはこの 1 段で完結する。
    fn track_builtin_plain_value(
        &self,
        track_id: u32,
        param: &common::model::TrackBuiltinParam,
    ) -> f64 {
        use common::model::TrackBuiltinParam as P;
        let Some(track) = self.cur.song_doc.song().track_by_id(track_id) else {
            return 0.0;
        };
        match param {
            P::Volume => f64::from(track.volume),
            P::Pan => f64::from(track.pan),
            P::Mute => f64::from(u8::from(track.muted)),
            // A6 (r.md #8): send gain の現在値は model にある。
            // v29: 安定 send id 一致で引く。
            P::SendGain { send_id, .. } => track
                .sends
                .iter()
                .find(|s| s.id == *send_id)
                .map_or(0.0, |s| f64::from(s.gain)),
            // r.md #110: Parallel chain の gain / pan (安定 chain id で引く)。
            P::ChainGain { chain_id } => self
                .cur.song_doc
                .song()
                .chain_by_id(*chain_id)
                .map_or(1.0, |(_, c)| f64::from(c.gain)),
            P::ChainPan { chain_id } => self
                .cur.song_doc
                .song()
                .chain_by_id(*chain_id)
                .map_or(0.0, |(_, c)| f64::from(c.pan)),
            P::ParallelOutGain { parallel_id } => self
                .cur.song_doc
                .song()
                .parallel_by_id(*parallel_id)
                .map_or(1.0, |r| f64::from(r.out_gain)),
            // r.md #112: 分割が off の Parallel (dangling lane) は既定値を出す。
            P::ParallelSplitFreq { parallel_id, edge } => self
                .cur.song_doc
                .song()
                .parallel_by_id(*parallel_id)
                .and_then(|r| r.split.freq(*edge))
                .or_else(|| common::model::Split::DEFAULT_FREQUENCY3.freq(*edge))
                .map_or(0.0, f64::from),
            // r.md #114: アクティブ chain の中央の位置 (Selector でなければ中央 0.5)。
            P::ParallelSelect { parallel_id } => self
                .cur.song_doc
                .song()
                .parallel_by_id(*parallel_id)
                .map_or(0.5, |r| f64::from(r.select_pos())),
        }
    }

    /// `AddAutomationFromLastTouched` の補助。target の現在値を plain
    /// 単位で取得 (lane.default_value 初期化用)。 track-builtin は track の strip 値、
    /// send gain は `track.sends[idx].gain`、 plugin param は `current_plain_value`
    /// の cache (A6 r.md #8)、 song-level は `song.bpm` / `song.time_sig.0`。
    pub(crate) fn lane_default_for_target(&self, touched: &TouchedParam) -> f64 {
        use common::model::AutomationTarget;
        match &touched.target {
            AutomationTarget::TrackBuiltin(param) => {
                self.track_builtin_plain_value(touched.track_id, param)
            }
            // A6 (r.md #8): plugin param は GUI の現在値 cache を引く
            // (`current_plain_value` が `plugin_param_values` から解決)。
            AutomationTarget::PluginParam { .. } => self
                .current_plain_value(touched.track_id, &touched.target)
                .unwrap_or(0.0),
            // r.md #89: モジュレーター自身のツマミ / 変調 1 本の深さ。値の SSoT は
            // `common::mod_graph::param_plain` (ラックのツマミもここを引く)。
            AutomationTarget::ModSourceParam { source_id, param } => {
                self.mod_param_plain_value(*source_id, *param)
            }
            AutomationTarget::ModRoutingDepth { routing_id } => self
                .cur.song_doc
                .song()
                .all_mod_routings()
                .find(|r| r.id == *routing_id)
                .map_or(0.0, |r| f64::from(r.depth)),
            // r.md #129: 内蔵 device は id で引く (`NativeDevice::param` が住所 ↔ 値の SSoT)。
            // 束縛先が居なければ住所の既定値。
            AutomationTarget::NativeParam { device_id, param } => f64::from(
                self.cur
                    .song_doc
                    .song()
                    .native_by_id(*device_id)
                    .and_then(|d| d.param(*param))
                    .or_else(|| param.default_plain())
                    .unwrap_or(0.0),
            ),
            AutomationTarget::MasterLimiter(param) => {
                f64::from(self.cur.song_doc.song().master_limiter.param(*param))
            }
            AutomationTarget::SongTempo => f64::from(self.cur.song_doc.song().bpm),
            AutomationTarget::SongTimeSigNumerator => f64::from(self.cur.song_doc.song().time_sig.0),
            AutomationTarget::ImageBuiltin(field) => self.image_field_value(touched.track_id, *field),
            AutomationTarget::TextBuiltin(field) => self.text_field_value(touched.track_id, *field),
            // Group transform default: 同 track の group_transform (無ければ
            // GroupTransform::default) の該当 field。 group は表示 clip を持たない
            // ので image/text のような clip 探索は不要。
            AutomationTarget::GroupTransform(param) => {
                use common::model::GroupTransformParam as G;
                let gt = self
                    .cur.song_doc.song()
                    .track_by_id(touched.track_id)
                    .and_then(|t| t.group_transform)
                    .unwrap_or_default();
                f64::from(match param {
                    G::X => gt.x,
                    G::Y => gt.y,
                    G::Rotation => gt.rotation_radians,
                    G::ScaleX => gt.scale_x,
                    G::ScaleY => gt.scale_y,
                    G::AnchorX => gt.anchor_x,
                    G::AnchorY => gt.anchor_y,
                    G::Opacity => gt.opacity,
                })
            }
        }
    }

    /// Image PiP default: 同 track の最初の image clip の first
    /// event 値を初期値に使う。 1 つも image clip が無い (= lane
    /// を空 image track で先行追加するケース) は 0.0 fallback。
    /// `lane_default_for_target` から切り出したのは、 clip 探索の closure がネスト段数の
    /// 予算 (不変条件 9) を押し上げるため。
    fn image_field_value(&self, track_id: u32, field: common::model::ImageBuiltinParam) -> f64 {
        use common::model::{ClipContent, ImageBuiltinParam};
        let Some(track) = self.cur.song_doc.song().track_by_id(track_id) else {
            return 0.0;
        };
        let event = track.all_clips().find_map(|c| {
            self.cur.song_doc.song()
                .clip_contents
                .get(&c.content_id)
                .and_then(|content| match content {
                    ClipContent::Image(img) => img.events.first(),
                    _ => None,
                })
        });
        let Some(ev) = event else { return 0.0 };
        f64::from(match field {
            ImageBuiltinParam::X => ev.x,
            ImageBuiltinParam::Y => ev.y,
            ImageBuiltinParam::W => ev.w,
            ImageBuiltinParam::H => ev.h,
            ImageBuiltinParam::Opacity => ev.opacity,
            ImageBuiltinParam::Rotation => ev.rotation_radians,
        })
    }

    /// Text default: 同 track の first text event (セル込み) の field 値。
    /// text clip が無い (= lane を空 track で先行追加) は field
    /// ごとの常識値 (色 RGBA は (1,1,1,1) や (0,0,0,1) 等)。
    /// 切り出した理由は [`Self::image_field_value`] と同じ。
    fn text_field_value(&self, track_id: u32, field: common::model::TextBuiltinParam) -> f64 {
        use common::model::{ClipContent, TextBuiltinParam as T};
        let Some(track) = self.cur.song_doc.song().track_by_id(track_id) else {
            return 0.0;
        };
        let event = track.all_clips().find_map(|c| {
            self.cur.song_doc.song()
                .clip_contents
                .get(&c.content_id)
                .and_then(|content| match content {
                    ClipContent::Text(t) => t.events.first(),
                    _ => None,
                })
        });
        let Some(ev) = event else {
            // text clip 無し → default 値 (= TextEvent::default
            // の常識値と整合させる)。
            return match field {
                T::X => 0.0,
                T::Y => 0.4,
                T::W => 1.0,
                T::H => 0.2,
                T::Opacity => 1.0,
                T::Rotation => 0.0,
                T::FontSize => 64.0,
                T::FillR | T::FillG | T::FillB | T::FillA => 1.0,
                T::OutlineR | T::OutlineG | T::OutlineB => 0.0,
                T::OutlineA => 1.0,
                T::OutlineWidth => 0.0,
                T::ShadowR | T::ShadowG | T::ShadowB => 0.0,
                T::ShadowA => 0.5,
                T::ShadowOffsetX | T::ShadowOffsetY => 0.0,
                T::ShadowBlur => 0.0,
            };
        };
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
}
