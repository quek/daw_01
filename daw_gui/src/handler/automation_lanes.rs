//! handler::automation_lanes — image/group/text automation lane + inspector param + plugin param 編集 + child-disconnect
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;

impl AppData {
    /// `A` キー shortcut の handler。`last_touched_param` の lane を
    /// 該当 track に追加 (or 既存があれば visible = true で復活)。
    /// 仕様: `docs/plan_automation.md` §7.3。
    /// Inspector の image event「📈」 ボタンから呼ばれる。 選択中 image
    /// clip の track に `AutomationTarget::ImageBuiltin(field)` lane を
    /// 追加する (= `docs/plan_image_automation.md` §4.1)。 既存 lane が
    /// 同 target で見つかれば visible / enabled を `true` に戻して
    /// 終わり (= 削除復活 UX)、 無ければ新規作成。 default_value は
    /// 同 track 上の first image event の field 値、 image event が
    /// 解決できなければ image field 共通 default (0 for x/y、 1 for w/h
    /// /opacity)。
    pub(crate) fn add_image_automation_lane(
        &mut self,
        field: common::model::ImageBuiltinParam,
    ) {
        use common::model::{AutomationLane, AutomationTarget, ClipContent, ImageBuiltinParam};
        let Some(target_clip) = self.selected_clip_ref() else {
            self.ui_ephemeral.status_message =
                "Image Automation: 画像 clip を選択してください".into();
            return;
        };
        let track_id_opt = self
            .song_doc.song()
            .track_by_id(target_clip.track_id)
            .map(|t| t.id);
        let Some(track_id) = track_id_opt else {
            return;
        };
        let target = AutomationTarget::ImageBuiltin(field);

        // 既存 lane を find。 あれば visible / enabled を true に。
        let found_existing = self.edit_song_checked(|song| {
            if let Some(track) = song.track_by_id_mut(track_id)
                && let Some(lane) = track
                    .automation_lanes
                    .iter_mut()
                    .find(|l| l.target == target)
            {
                lane.visible = true;
                lane.enabled = true;
                true
            } else {
                false
            }
        });
        if found_existing {
            self.ui_prefs.expanded_automation_tracks.insert(track_id);
            self.ui_ephemeral.status_message = format!(
                "Image Automation lane '{}' は既に存在します",
                automation_target_display_name(&target)
            );
            return;
        }

        // default_value: 同 track 上の first image event の field 値。
        // r.md #87: 走査は `all_clips` (= clips + session_clips) — content を探す
        // 走査なので、素材がランチャーのセルにしか無い track で常識値へ落ちない。
        // image event が無ければ field ごとの常識値。 clamp 範囲は field
        // 種別で異なる (x/y/w/h/opacity = [0,1]、 rotation = [-π, π])。
        let default_value: f64 = {
            let Some(track) = self.song_doc.song().track_by_id(track_id) else {
                return;
            };
            let event = track.all_clips().find_map(|c| {
                self.song_doc.song().clip_contents.get(&c.content_id).and_then(|content| {
                    match content {
                        ClipContent::Image(img) => img.events.first(),
                        _ => None,
                    }
                })
            });
            let v = match event {
                Some(ev) => match field {
                    ImageBuiltinParam::X => ev.x,
                    ImageBuiltinParam::Y => ev.y,
                    ImageBuiltinParam::W => ev.w,
                    ImageBuiltinParam::H => ev.h,
                    ImageBuiltinParam::Opacity => ev.opacity,
                    ImageBuiltinParam::Rotation => ev.rotation_radians,
                },
                None => match field {
                    ImageBuiltinParam::X | ImageBuiltinParam::Y => 0.0,
                    ImageBuiltinParam::W
                    | ImageBuiltinParam::H
                    | ImageBuiltinParam::Opacity => 1.0,
                    ImageBuiltinParam::Rotation => 0.0,
                },
            };
            let v = f64::from(v);
            match field {
                ImageBuiltinParam::Rotation => {
                    v.clamp(-std::f64::consts::PI, std::f64::consts::PI)
                }
                _ => v.clamp(0.0, 1.0),
            }
        };

        let __applied = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(track_id) else {
                return false;
            };
            let lane_id = track.alloc_lane_id();
            let new_lane =
                AutomationLane { id: lane_id, ..AutomationLane::new(target.clone(), default_value) };
            track.automation_lanes.push(new_lane);
            true
        });
        if !__applied {
            return;
        }
        self.ui_prefs.expanded_automation_tracks.insert(track_id);
        self.ui_ephemeral.status_message = format!(
            "Added image automation lane: {}",
            automation_target_display_name(&target)
        );
    }

    /// PiP drag 中に image lane gesture が `active_param_gestures` に
    /// 残っているか。 record_automation_points_for_tick の起動条件で
    /// 「停止中でも image drag 中なら record を回す」 ために使う。
    pub(crate) fn image_pip_drag_active(&self) -> bool {
        self.recording.active_param_gestures
            .iter()
            .any(|(_, t)| matches!(t, common::model::AutomationTarget::ImageBuiltin(_)))
    }

    /// preview drag begin で呼ばれる。 選択中 image clip の track 上で
    /// `AutomationTarget::ImageBuiltin(_)` lane を持つ全 field を
    /// `active_param_gestures` に登録。 record_automation_points_for_tick
    /// が再生中に 1/64 beat throttle で point を打つ pipeline に乗る
    /// (`docs/plan_image_automation.md` §5)。
    ///
    /// 停止中の drag は ImageEvent.field を直接編集する経路で UI を
    /// 動かすが、 lane が override しているので preview は変化しない (=
    /// default 値だけが変わる)。 「停止中の drag で keyframe を打つ」
    /// UX は別途 follow-up (`docs/plan_image_automation.md` §8 未確定
    /// 事項)。
    pub(crate) fn begin_image_pip_drag_recording(&mut self) {
        use common::model::{AutomationTarget, ImageBuiltinParam};
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let Some(track) = self.song_doc.song().track_by_id(target_clip.track_id) else {
            return;
        };
        let track_id = track.id;
        // lane が存在する field を全て active_param_gestures に。 record
        // path が curve insert を行う。
        let fields = [
            ImageBuiltinParam::X,
            ImageBuiltinParam::Y,
            ImageBuiltinParam::W,
            ImageBuiltinParam::H,
            ImageBuiltinParam::Opacity,
        ];
        let mut seeded: Vec<AutomationTarget> = Vec::new();
        for field in fields {
            let target = AutomationTarget::ImageBuiltin(field);
            let has_lane = track
                .automation_lanes
                .iter()
                .any(|l| l.enabled && l.target == target);
            if has_lane {
                self.recording.active_param_gestures.insert((track_id, target.clone()));
                if matches!(
                    self.recording.recording_mode,
                    common::model::RecordingMode::Latch
                        | common::model::RecordingMode::Write
                ) && self.transport.is_playing
                {
                    self.recording.latched_param_gestures.insert((track_id, target.clone()));
                }
                seeded.push(target);
            }
        }
        if !seeded.is_empty() {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// preview drag end で呼ばれる。 begin で seed した全 ImageBuiltin
    /// gesture を `active_param_gestures` から remove。 Touch mode では
    /// `recording_last_beat` からも消す (= 連続録音停止)。 Latch / Write
    /// では latched は stop まで残す (= 既存 ParamGestureEnd と同 idiom)。
    pub(crate) fn end_image_pip_drag_recording(&mut self) {
        use common::model::AutomationTarget;
        // image lane gesture だけを掃除 (audio / plugin gesture は残す)。
        let to_remove: Vec<(u32, AutomationTarget)> = self
            .recording.active_param_gestures
            .iter()
            .filter(|(_, t)| matches!(t, AutomationTarget::ImageBuiltin(_)))
            .cloned()
            .collect();
        let any = !to_remove.is_empty();
        for key in to_remove {
            self.recording.active_param_gestures.remove(&key);
            if self.recording.recording_mode == common::model::RecordingMode::Touch {
                self.recording.recording_last_beat.remove(&key);
            }
        }
        if any {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// docs/plan_text_overlay.md §4 P6: 選択中 text clip の track 上で
    /// `TextBuiltin(_)` lane を持つ全 field を `active_param_gestures` に
    /// 登録 (= image PiP drag と同 idiom)。 lane が無い field は drag が
    /// TextEvent.field を直接書くだけ (= lane override 無し時の単純経路)。
    pub(crate) fn begin_text_pip_drag_recording(&mut self) {
        use common::model::{AutomationTarget, TextBuiltinParam};
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let Some(track) = self.song_doc.song().track_by_id(target_clip.track_id) else {
            return;
        };
        let track_id = track.id;
        let fields = [
            TextBuiltinParam::X,
            TextBuiltinParam::Y,
            TextBuiltinParam::W,
            TextBuiltinParam::H,
            TextBuiltinParam::Rotation,
        ];
        let mut seeded = false;
        for field in fields {
            let target = AutomationTarget::TextBuiltin(field);
            let has_lane = track
                .automation_lanes
                .iter()
                .any(|l| l.enabled && l.target == target);
            if has_lane {
                self.recording.active_param_gestures.insert((track_id, target.clone()));
                if matches!(
                    self.recording.recording_mode,
                    common::model::RecordingMode::Latch
                        | common::model::RecordingMode::Write
                ) && self.transport.is_playing
                {
                    self.recording.latched_param_gestures.insert((track_id, target));
                }
                seeded = true;
            }
        }
        if seeded {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// docs/plan_text_overlay.md §4 P6: text PiP drag end で seed した
    /// `TextBuiltin(_)` gesture を `active_param_gestures` から remove
    /// (= image PiP drag end と同 idiom)。
    pub(crate) fn end_text_pip_drag_recording(&mut self) {
        use common::model::AutomationTarget;
        let to_remove: Vec<(u32, AutomationTarget)> = self
            .recording.active_param_gestures
            .iter()
            .filter(|(_, t)| matches!(t, AutomationTarget::TextBuiltin(_)))
            .cloned()
            .collect();
        let any = !to_remove.is_empty();
        for key in to_remove {
            self.recording.active_param_gestures.remove(&key);
            if self.recording.recording_mode == common::model::RecordingMode::Touch {
                self.recording.recording_last_beat.remove(&key);
            }
        }
        if any {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// text PiP drag が active (= `TextBuiltin(_)` lane gesture を保持)
    /// なら true。 停止中の drag-while-stopped auto-keyframe を image と
    /// 同様に許可するため、 `record_automation_points_for_tick` の gate
    /// で `image_pip_drag_active() || text_pip_drag_active()` の OR で
    /// 使う。
    pub(crate) fn text_pip_drag_active(&self) -> bool {
        self.recording.active_param_gestures
            .iter()
            .any(|(_, t)| matches!(t, common::model::AutomationTarget::TextBuiltin(_)))
    }

    /// 選択中 image clip の track から `ImageBuiltin(field)` lane を
    /// 削除 (= override 解除)。 lane が見つからない場合は no-op + status
    /// 表示。 削除後は ImageEvent.field がふたたび effective。
    pub(crate) fn remove_image_automation_lane(
        &mut self,
        field: common::model::ImageBuiltinParam,
    ) {
        use common::model::AutomationTarget;
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let target = AutomationTarget::ImageBuiltin(field);
        let removed = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(target_clip.track_id) else {
                return false;
            };
            let before = track.automation_lanes.len();
            track.automation_lanes.retain(|l| l.target != target);
            before != track.automation_lanes.len()
        });
        if !removed {
            self.ui_ephemeral.status_message = format!(
                "Image Automation: {} lane が見つかりません",
                automation_target_display_name(&target)
            );
            return;
        }
        self.ui_ephemeral.status_message = format!(
            "Image Automation lane '{}' を削除しました",
            automation_target_display_name(&target)
        );
    }

    // ---- 立ち絵 group transform (`docs/plan_tachie_group_transform.md` §5.5) --

    /// 選択中（cursor）group track に `GroupTransform(param)` lane を追加。
    /// 既存があれば visible / enabled 復活のみ。default_value は現
    /// `group_transform` の field 値（plain）。`add_image_automation_lane` と同型。
    pub(crate) fn add_group_automation_lane(&mut self, param: common::model::GroupTransformParam) {
        use common::model::{AutomationLane, AutomationTarget};
        let Some(track_id) = self.cursor_track_id() else {
            self.ui_ephemeral.status_message =
                "Group Transform: group track を選択してください".into();
            return;
        };
        let target = AutomationTarget::GroupTransform(param);
        let found_existing = self.edit_song_checked(|song| {
            if let Some(track) = song.track_by_id_mut(track_id)
                && let Some(lane) =
                    track.automation_lanes.iter_mut().find(|l| l.target == target)
            {
                lane.visible = true;
                lane.enabled = true;
                true
            } else {
                false
            }
        });
        if found_existing {
            self.ui_prefs.expanded_automation_tracks.insert(track_id);
            self.ui_ephemeral.status_message = format!(
                "Group Automation lane '{}' は既に存在します",
                automation_target_display_name(&target)
            );
            return;
        }
        let gt = self
            .song_doc.song()
            .track_by_id(track_id)
            .and_then(|t| t.group_transform)
            .unwrap_or_default();
        let default_value = f64::from(group_transform_field(&gt, param));
        let __applied = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(track_id) else {
                return false;
            };
            let lane_id = track.alloc_lane_id();
            track.automation_lanes.push(AutomationLane {
                id: lane_id,
                ..AutomationLane::new(target.clone(), default_value)
            });
            true
        });
        if !__applied {
            return;
        }
        self.ui_prefs.expanded_automation_tracks.insert(track_id);
        self.ui_ephemeral.status_message = format!(
            "Added group automation lane: {}",
            automation_target_display_name(&target)
        );
    }

    /// 選択中 group track から `GroupTransform(param)` lane を削除。
    pub(crate) fn remove_group_automation_lane(
        &mut self,
        param: common::model::GroupTransformParam,
    ) {
        use common::model::AutomationTarget;
        let Some(track_id) = self.cursor_track_id() else {
            return;
        };
        let target = AutomationTarget::GroupTransform(param);
        let removed = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(track_id) else {
                return false;
            };
            let before = track.automation_lanes.len();
            track.automation_lanes.retain(|l| l.target != target);
            before != track.automation_lanes.len()
        });
        if !removed {
            self.ui_ephemeral.status_message = format!(
                "Group Automation: {} lane が見つかりません",
                automation_target_display_name(&target)
            );
            return;
        }
        self.ui_ephemeral.status_message = format!(
            "Group Automation lane '{}' を削除しました",
            automation_target_display_name(&target)
        );
    }

    /// `Track.group_transform`（無ければ default を Some 化）の該当 field を
    /// 設定。`last_touched_param` も更新（touch+A 用）。純 visual なので audio
    /// へは送らない。
    pub(crate) fn set_group_transform_field(
        &mut self,
        track_id: u32,
        param: common::model::GroupTransformParam,
        value: f32,
    ) {
        use common::model::GroupTransformParam as G;
        let __applied = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(track_id) else {
                return false;
            };
            let gt = track.group_transform.get_or_insert_with(Default::default);
            match param {
                G::X => gt.x = value,
                G::Y => gt.y = value,
                G::Rotation => gt.rotation_radians = value,
                G::ScaleX => gt.scale_x = value,
                G::ScaleY => gt.scale_y = value,
                G::AnchorX => gt.anchor_x = value,
                G::AnchorY => gt.anchor_y = value,
                G::Opacity => gt.opacity = value,
            }
            true
        });
        if !__applied {
            return;
        }
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::GroupTransform(param),
            display_name: format!("Group {}", group_param_label(param)),
            touched_at: std::time::Instant::now(),
        });
    }

    /// group track が「visual group」か（§5.6 派生判定）。subtree に image /
    /// video / text 表示 clip を持つ track が 1 つでもある、または既に
    /// `group_transform` データを持つなら true。inspector / 合成の gate。
    pub fn group_has_visual_content(&self, group_track_id: u32) -> bool {
        crate::group_compose::group_has_visual_content(self.song_doc.song(), group_track_id)
    }

    /// group inspector 用 summary。cursor track が visual group なら、各 param に
    /// `GroupTransform(param)` lane があるか（=「A」 トグル点灯）を返す。
    pub fn inspector_group_transform_summary(
        &self,
    ) -> Option<GroupTransformInspectorSummary> {
        // Transform もチェーン行の "GUI" ボタンでトグル開閉する（他 FX と統一、
        // 出っぱなしにしない）。開いている device が cursor track の Transform 配置 device の
        // ときだけ Group Transform セクションを出す。
        let open_device = self.ui_ephemeral.open_video_fx_params?;
        // r.md #71 (プラグインのコピー / 移動): パネルは device_id で開いたまま
        // にして、 **描画側で** 「いま表示しているチェーンの device か」 を gate する
        // (device を別トラックへ移してもパネルが自然に追従する)。
        let (open_track, open_idx) = find_device_by_id(self.song_doc.song(), open_device)?;
        if self.cursor_track_id() != Some(open_track) {
            return None;
        }
        let track = self.song_doc.song().track_by_id(open_track)?;
        if track.devices.get(open_idx as usize).map(|d| d.plugin_id.as_str())
            != Some(common::video_fx::TRANSFORM_ID)
        {
            return None;
        }
        let mut automated = [false; 8];
        for param in GROUP_PARAMS {
            automated[group_param_index(param)] = track.automation_lanes.iter().any(
                |l| matches!(l.target, common::model::AutomationTarget::GroupTransform(p) if p == param),
            );
        }
        Some(GroupTransformInspectorSummary {
            track_id: track.id,
            automated,
            transform: track.group_transform.unwrap_or_default(),
        })
    }

    /// 開いている映像 FX param パネル（`open_video_fx_params`）が cursor
    /// track と一致するとき、その device の def + 各 param の現在実値を返す。inspector が
    /// scrubable_number 行に展開する（Group Transform セクションと同 idiom）。
    pub fn inspector_video_fx_params(&self) -> Option<VideoFxParamsInspector> {
        let device_id = self.ui_ephemeral.open_video_fx_params?;
        let (track_id, device_index) = find_device_by_id(self.song_doc.song(), device_id)?;
        if self.cursor_track_id() != Some(track_id) {
            return None;
        }
        let def = self
            .song_doc.song()
            .fx_chain_by_track_id(track_id)?
            .get(device_index as usize)
            .and_then(|d| common::video_fx::def_by_id(&d.plugin_id))?;
        if def.params.is_empty() {
            return None; // Transform 等は専用セクションで編集。
        }
        let empty: &[common::model::AutomationLane] = &[];
        let lanes: &[common::model::AutomationLane] =
            if track_id == common::model::MASTER_TRACK_ID {
                &self.song_doc.song().song_lanes
            } else {
                self.song_doc.song()
                    .track_by_id(track_id)
                    .map_or(empty, |t| t.automation_lanes.as_slice())
            };
        let values: Vec<f32> = def
            .params
            .iter()
            .map(|p| {
                let target = common::model::AutomationTarget::PluginParam {
                    device_id,
                    param_id: p.id,
                    legacy_device_index: None,
                };
                // base = lane default_value、無ければ manifest default（実レンジ表示）。
                let norm = lanes
                    .iter()
                    .find(|l| l.target == target)
                    .map_or_else(|| p.kind.default_norm(), |l| l.default_value);
                p.kind.norm_to_real(norm)
            })
            .collect();
        Some(VideoFxParamsInspector { track_id, device_id, def, values })
    }

    /// 内蔵映像 FX param を 1 つ編集（パネルの scrubable から）。値の SSoT は
    /// `PluginParam` lane の `default_value`（0..=1 norm、`video_fx` モジュール doc）。lane が
    /// 無ければ値保持用（`visible=false`・curve 無し）を作る。master は `song_lanes`。
    pub(crate) fn set_video_fx_param(&mut self, device_id: u64, param_id: u32, value_real: f32) {
        use common::model::{AutomationLane, AutomationTarget};
        // lane の所有者 (track / master) は device_id から毎回引き直す
        // (r.md #71 プラグインのコピー / 移動: cursor track に依存しない)。
        let song = self.song_doc.song();
        let Some((track_id, device_index)) = find_device_by_id(song, device_id) else {
            return;
        };
        // def_by_id は &'static を返すので self.song_doc.song() の借用はここで終わる。
        let Some(def) = device_at(song, track_id, device_index)
            .and_then(|d| common::video_fx::def_by_id(&d.plugin_id))
        else {
            return;
        };
        let Some(param) = def.param(param_id) else {
            return;
        };
        let display_name = format!("{} {}", def.name, param.name);
        let norm = param.kind.real_to_norm(value_real);
        let target = AutomationTarget::PluginParam {
            device_id,
            param_id,
            legacy_device_index: None,
        };
        self.edit_song(|song| {
            if track_id == common::model::MASTER_TRACK_ID {
                if let Some(lane) = song.song_lanes.iter_mut().find(|l| l.target == target) {
                    lane.default_value = norm;
                } else {
                    let id = song.alloc_song_lane_id();
                    let mut lane = AutomationLane::new(target.clone(), norm);
                    lane.id = id;
                    lane.visible = false;
                    song.song_lanes.push(lane);
                }
            } else if let Some(track) = song.track_by_id_mut(track_id) {
                if let Some(lane) = track.automation_lanes.iter_mut().find(|l| l.target == target) {
                    lane.default_value = norm;
                } else {
                    let id = track.alloc_lane_id();
                    let mut lane = AutomationLane::new(target.clone(), norm);
                    lane.id = id;
                    lane.visible = false;
                    track.automation_lanes.push(lane);
                }
            }
        });
        // 「A」キー (last_touched_param) で automation lane を可視化/curve 化できる。
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
    }

    /// 汎用 plugin の「Par」インライン param パネルの read snapshot。
    /// `open_plugin_params` が cursor track の device を指し、 host から param 一覧が
    /// 届いているときに、 lane default_value を実レンジ化した編集可能な param 行を返す。
    /// VOICEVOX / 字幕 builtin は host param を持たず、 専用セクション (Clip Voice /
    /// Talk / Text Event) が `*_param_panel_open()` gate で Par パネルとして描画される
    /// ので、 ここでは `None` (= 汎用パネルは出さない)。
    pub fn inspector_plugin_params(&self) -> Option<PluginParamsInspector> {
        let device_id = self.ui_ephemeral.open_plugin_params?;
        let (track_id, device_index) = find_device_by_id(self.song_doc.song(), device_id)?;
        if self.cursor_track_id() != Some(track_id) {
            return None;
        }
        let device = device_at(self.song_doc.song(), track_id, device_index)?;
        let plugin_name = resolve_plugin_name(&self.ipc.plugin_db, &device.plugin_id);

        // param 行: lane default_value (無ければ info.default_value を正規化) を
        // 実レンジへ。 HIDDEN は出さない。
        let empty: &[common::model::AutomationLane] = &[];
        let lanes: &[common::model::AutomationLane] =
            if track_id == common::model::MASTER_TRACK_ID {
                &self.song_doc.song().song_lanes
            } else {
                self.song_doc.song()
                    .track_by_id(track_id)
                    .map_or(empty, |t| t.automation_lanes.as_slice())
            };
        let params: Vec<PluginParamRow> = self
            .ipc.plugin_params
            .get(&device_id)
            .map(|infos| {
                infos
                    .iter()
                    .filter(|p| {
                        p.flags & common::protocol::plugin_param_flags::HIDDEN == 0
                    })
                    .map(|p| {
                        let span = p.max_value - p.min_value;
                        let target = common::model::AutomationTarget::PluginParam {
                            device_id,
                            param_id: p.id,
                            legacy_device_index: None,
                        };
                        let norm = lanes.iter().find(|l| l.target == target).map_or_else(
                            || {
                                if span.abs() < f64::EPSILON {
                                    0.0
                                } else {
                                    ((p.default_value - p.min_value) / span).clamp(0.0, 1.0)
                                }
                            },
                            |l| l.default_value,
                        );
                        PluginParamRow {
                            id: p.id,
                            name: p.name.clone(),
                            value_real: p.min_value + norm * span,
                            default_real: p.default_value,
                            min: p.min_value,
                            max: p.max_value,
                            stepped: p.flags
                                & common::protocol::plugin_param_flags::STEPPED
                                != 0,
                            readonly: p.flags
                                & common::protocol::plugin_param_flags::READONLY
                                != 0,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        // param が 1 つも無い device (VOICEVOX / 字幕 / Silence) は汎用パネルを出さない
        // (= 専用セクションが Par パネルを担う)。
        if params.is_empty() {
            return None;
        }
        Some(PluginParamsInspector {
            track_id,
            device_id,
            plugin_name,
            params,
        })
    }

    /// 「Par」パネルが開いている device の plugin_id (cursor track 上)。
    /// VOICEVOX / 字幕 など専用セクションを持つ builtin の Par 開閉判定に使う。
    pub(crate) fn open_param_panel_plugin_id(&self) -> Option<&str> {
        let device_id = self.ui_ephemeral.open_plugin_params?;
        let (track_id, index) = find_device_by_id(self.song_doc.song(), device_id)?;
        if self.cursor_track_id() != Some(track_id) {
            return None;
        }
        device_at(self.song_doc.song(), track_id, index).map(|d| d.plugin_id.as_str())
    }

    /// VOICEVOX builtin の「Par」パネルが開いているか (= Clip Voice /
    /// Talk セクションを Par パネルとして描画する gate)。
    pub fn voicevox_param_panel_open(&self) -> bool {
        self.open_param_panel_plugin_id() == Some(common::plugin_db::BUILTIN_ID_VOICEVOX)
    }

    /// 字幕 builtin の「Par」パネルが開いているか (= Text Event
    /// セクションを Par パネルとして描画する gate)。
    pub fn subtitle_param_panel_open(&self) -> bool {
        self.open_param_panel_plugin_id() == Some(common::plugin_db::SUBTITLE_ID)
    }

    /// 汎用 plugin param を 1 つ編集 (「⚙」パネルの scrubable から)。 値の
    /// SSoT は `PluginParam` lane の `default_value` (0..=1 norm)。 実レンジ↔norm は
    /// host が送った `PluginParamInfo` の min/max。 lane が無ければ値保持用
    /// (`visible=false`) を作る。 master は `song_lanes`。 音への反映 (host push) は
    /// scrub 終端で inspector が `flush_song_sync` を呼ぶ (RT 安全)。
    /// r.md #71 (プラグインのコピー / 移動): 旧 `set_plugin_param_on_track` との
    /// 2 本立てを 1 本に畳んだ。 両方が `device_id` を取った瞬間、 lane の
    /// 所有者 (track / master) の解決が `find_device_by_id` に移り、 wrapper 側の
    /// 存在理由 (cursor track の解決) が消えて中身まで同一になったため。
    pub(crate) fn set_plugin_param(&mut self, device_id: u64, param_id: u32, value_real: f64) {
        use common::model::{AutomationLane, AutomationTarget};
        // device が消えていれば何もしない (削除済み device への stale binding /
        // stale event は正常系なので tracing は出さない)。
        let Some((track_id, _)) = find_device_by_id(self.song_doc.song(), device_id) else {
            return;
        };
        let Some(info) = self
            .ipc.plugin_params
            .get(&device_id)
            .and_then(|v| v.iter().find(|p| p.id == param_id))
            .cloned()
        else {
            return;
        };
        let span = info.max_value - info.min_value;
        let norm = if span.abs() < f64::EPSILON {
            0.0
        } else {
            ((value_real - info.min_value) / span).clamp(0.0, 1.0)
        };
        let target = AutomationTarget::PluginParam {
            device_id,
            param_id,
            legacy_device_index: None,
        };
        self.edit_song(|song| {
            if track_id == common::model::MASTER_TRACK_ID {
                if let Some(lane) = song.song_lanes.iter_mut().find(|l| l.target == target) {
                    lane.default_value = norm;
                } else {
                    let id = song.alloc_song_lane_id();
                    let mut lane = AutomationLane::new(target.clone(), norm);
                    lane.id = id;
                    lane.visible = false;
                    song.song_lanes.push(lane);
                }
            } else if let Some(track) = song.track_by_id_mut(track_id) {
                if let Some(lane) = track.automation_lanes.iter_mut().find(|l| l.target == target) {
                    lane.default_value = norm;
                } else {
                    let id = track.alloc_lane_id();
                    let mut lane = AutomationLane::new(target.clone(), norm);
                    lane.id = id;
                    lane.visible = false;
                    track.automation_lanes.push(lane);
                }
            }
        });
        // 表示名は `automation_target_label` 1 本に寄せる (r.md #72 / #78)。
        // かつてここだけ `format!("{module} {name}")` を手組みしていたため、
        // 同じ param が経路によって別名で出ていた。
        let display_name = self.automation_target_label(&target);
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
    }

    /// docs/plan_text_overlay.md §4 P8: 選択中 text clip の track に
    /// `TextBuiltin(field)` lane を追加。 既存 lane があれば visible /
    /// enabled を再有効化のみ。 default_value は `lane_default_for_target`
    /// 経由で TextEvent の現値 (= image lane と同 idiom、 23 field 分の
    /// match は `lane_default_for_target` 内に集約済)。
    pub(crate) fn add_text_automation_lane(
        &mut self,
        field: common::model::TextBuiltinParam,
    ) {
        use common::model::{AutomationLane, AutomationTarget};
        let Some(target_clip) = self.selected_clip_ref() else {
            self.ui_ephemeral.status_message =
                "Text Automation: text clip を選択してください".into();
            return;
        };
        let track_id_opt = self
            .song_doc.song()
            .track_by_id(target_clip.track_id)
            .map(|t| t.id);
        let Some(track_id) = track_id_opt else {
            return;
        };
        let target = AutomationTarget::TextBuiltin(field);

        // 既存 lane があれば visible / enabled だけを true に。
        let found_existing = self.edit_song_checked(|song| {
            if let Some(track) = song.track_by_id_mut(track_id)
                && let Some(lane) = track
                    .automation_lanes
                    .iter_mut()
                    .find(|l| l.target == target)
            {
                lane.visible = true;
                lane.enabled = true;
                true
            } else {
                false
            }
        });
        if found_existing {
            self.ui_prefs.expanded_automation_tracks.insert(track_id);
            self.ui_ephemeral.status_message = format!(
                "Text Automation lane '{}' は既に存在します",
                automation_target_display_name(&target)
            );
            return;
        }

        // 23 field 分の現値解決は `lane_default_for_target` が TextBuiltin
        // を扱う (TextEvent 無し時の常識値も同関数内)。 caller は track_id
        // + target を流すだけ。
        let touched = TouchedParam {
            track_id,
            target: target.clone(),
            display_name: automation_target_display_name(&target).to_string(),
            touched_at: std::time::Instant::now(),
        };
        let default_value = self.lane_default_for_target(&touched);

        let __applied = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(track_id) else {
                return false;
            };
            let lane_id = track.alloc_lane_id();
            track.automation_lanes.push(AutomationLane {
                id: lane_id,
                ..AutomationLane::new(target.clone(), default_value)
            });
            true
        });
        if !__applied {
            return;
        }
        self.ui_prefs.expanded_automation_tracks.insert(track_id);
        self.ui_ephemeral.status_message = format!(
            "Added text automation lane: {}",
            automation_target_display_name(&target)
        );
    }

    /// 選択中 text clip の track から `TextBuiltin(field)` lane を削除
    /// (= override 解除、 TextEvent.field が ふたたび effective)。 lane
    /// が無ければ no-op + status 表示。
    pub(crate) fn remove_text_automation_lane(
        &mut self,
        field: common::model::TextBuiltinParam,
    ) {
        use common::model::AutomationTarget;
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let target = AutomationTarget::TextBuiltin(field);
        let removed = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(target_clip.track_id) else {
                return false;
            };
            let before = track.automation_lanes.len();
            track.automation_lanes.retain(|l| l.target != target);
            before != track.automation_lanes.len()
        });
        if !removed {
            self.ui_ephemeral.status_message = format!(
                "Text Automation: {} lane が見つかりません",
                automation_target_display_name(&target)
            );
            return;
        }
        self.ui_ephemeral.status_message = format!(
            "Text Automation lane '{}' を削除しました",
            automation_target_display_name(&target)
        );
    }

    /// 子プロセス (daw_audio / daw_plugin_host) の pipe loop が break
    /// したときに呼ばれる。 `ChildSupervisor.respawn(kind)` で新 child を
    /// spawn + handshake + Session/OpenWorkerPool 再送し、 成功なら
    /// `audio_tx` / `plugin_tx` を新 sender に差し替え、 SetProjectDir +
    /// LoadSong + restore_plugin_from_song で state restore。 失敗時は
    /// tx を None のまま status_message でユーザーに通知 (= sync_song
    /// _to_plugin_host を呼ぶと send が捨てられるが panic しない)。
    ///
    /// `is_playing` は false に戻す (= ユーザーが Play を押し直す前提)。
    /// audio engine が再起動した直後の playhead は 0 で、 旧 playhead に
    /// 自動 seek すると意図しない位置から再生になるので、 user に明示
    /// 操作してもらう方が安全。
    pub(crate) fn handle_child_disconnected(&mut self, kind: common::protocol::ChildKind) {
        use common::protocol::ChildKind;
        // (r.md #61) 終了シーケンス中の切断は crash ではなく **こちらが頼んだ
        // 結果**。ここで respawn すると「終了しようとしているのに子が生き返る」。
        //
        // ガードは呼び出し側 3 箇所 (`AudioEvent::ChildDisconnected` /
        // `PluginEvent::ChildDisconnected` / `WorkerPoolStalled` からの合成) では
        // なく、この入口 1 箇所に置く (SSoT — 呼び出し側に撒くと必ず漏れる)。
        if self.suppress_child_respawn(kind) {
            return;
        }
        let was_playing = self.transport.is_playing;
        // r.md #51: 通常 `is_playing` の writer は `on_tick` の観測だけだが、
        // 子プロセスが落ちた以上 Tick はもう来ない (= 観測が永久に止まる) ので、
        // ここだけは「走っていない」ことを直接書き込む。 録音セッションも同時に
        // 閉じないと Rec が点灯したまま、凍ったプレイヘッドへノートが積み上がる。
        self.transport.is_playing = false;
        self.transport.preroll_remaining = 0;
        self.transport.pending_play = false;
        self.transport.pending_play_record = None;
        self.close_recording_session();
        self.silence_monitor_notes();
        self.recording.active_param_gestures.clear();
        self.recording.latched_param_gestures.clear();
        // r.md #54: 走査中の解析も畳む。子が落ちた以上 `LoudnessAnalysisComplete` は
        // 永遠に来ないので、放置すると背景を暗転したまま watchdog の 60 秒まで
        // 操作不能になる (書き出しの `abort_audio_export` と同じ理由・同じ位置)。
        // plugin_host の切断でも畳む — `PluginsReinitDone` 待ちで固まるため。
        self.abort_loudness_analysis(
            "子プロセスが切断されたためラウドネス解析を中止しました".into(),
        );
        // 音声 render 中の crash で export を中止したか。respawn 成功時の status に
        // 「書き出しを中止しました」を併記して、中止の事実が上書きで消えないようにする。
        let mut export_aborted = false;
        match kind {
            ChildKind::Audio => {
                self.ipc.audio_tx = None;
                // 音声 render 中の crash なら ExportWavComplete が永遠に来ない。
                // export を強制終了して overlay / 入力 gate を解除する（解除しないと
                // GUI が永久ロックする）。AudioRender 中でなければ no-op。中止した
                // ことは下の respawn status に併記する（respawn 成功 status に
                // 上書きされて「書き出しが中止された」事実が消えないように）。
                export_aborted = self.abort_audio_export(
                    "音声エンジンがクラッシュしたため書き出しを中止しました".into(),
                );
                tracing::warn!("daw_audio child disconnected");
            }
            ChildKind::PluginHost => {
                self.ipc.plugin_tx = None;
                self.ipc.pending_plugin_loads.clear();
                self.ipc.loaded_devices.clear();
                // host が消えた時点で **全** device が未ロードなので、「一部だけ
                // 未ロード」を示す失敗 entry は誤情報になる (respawn すれば
                // restore_plugin_from_song が全 device を load し直し、crash-loop で
                // 諦めた場合は status_message がその旨を伝える)。
                self.ipc.failed_plugin_loads.clear();
                // plugin state 取得待ちの round-trip はもう完了しない
                // (host 消滅で AllStatesReceived が来ない)。 stale な queue / 保留ガードを
                // 破棄して GUI の恒久ロックを防ぐ。 hang watchdog (`abort_state_roundtrip`)
                // と同じ脱出処理に一本化する。
                self.abort_state_roundtrip();
                tracing::warn!("daw_plugin_host child disconnected");
            }
        }
        // 進行中の bounce / 書き出しを畳む (脱出口は handler::export が持つ)。
        export_aborted |= self.abort_inflight_renders_on_disconnect();

        // 中止した書き出しがあれば status に併記する suffix。
        let export_suffix = if export_aborted {
            " — 書き出しを中止しました"
        } else {
            ""
        };

        // crash-loop ガード: 短時間に同 kind が閾値以上切断したら自動 respawn を
        // 止める。落ちるプラグインを抱えたプロジェクト (例: state 復元後に
        // restartComponent を連発して host を落とす VST3) で respawn→reload→再 crash
        // の無限ループに陥り、 GUI が固まるのを防ぐ。
        const CRASH_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);
        const CRASH_LIMIT: usize = 3;
        let now = std::time::Instant::now();
        self.ipc.child_disconnect_log
            .retain(|(_, t)| now.duration_since(*t) < CRASH_WINDOW);
        self.ipc.child_disconnect_log.push((kind, now));
        let recent = self
            .ipc.child_disconnect_log
            .iter()
            .filter(|(k, _)| *k == kind)
            .count();
        if recent >= CRASH_LIMIT {
            self.ui_ephemeral.status_message = format!(
                "{}が繰り返しクラッシュしています — 自動再起動を停止しました。\
                 プロジェクトのプラグインを確認してください{}{}",
                kind.as_str(),
                if was_playing { " (再生停止)" } else { "" },
                export_suffix
            );
            tracing::error!(
                ?kind,
                recent,
                "child crash-loop detected; giving up auto-respawn to keep the UI responsive"
            );
            return;
        }

        // (r.md #61) **入口のガードをもう一度評価する**。この関数の途中で
        // `abort_state_roundtrip` が「保留していた終了意図を聞き直す」経路
        // (project.rs) を通り、clean な project なら `begin_shutdown` まで
        // **同期的に**走り切ることがある。入口のガードは「この関数の実行中に
        // phase は変わらない」を前提にしていたが、そこが崩れる。
        //
        // 見逃すと: 終了シーケンスが始まった直後に新しい daw_plugin_host を
        // spawn してプラグインを全部ロードし直し、それを 5 秒後に
        // `kill_remaining` が TerminateProcess する — r.md #61 で消したはずの
        // 「deactivate / destroy を通らない強制 kill」がそのまま再現する。
        if self.suppress_child_respawn(kind) {
            return;
        }
        // supervisor 経由で respawn を試みる。 supervisor が None
        // (= script / test 経路) なら通知だけで終わる。
        let Some(supervisor) = self.ipc.supervisor.clone() else {
            self.ui_ephemeral.status_message = format!(
                "{}が切断されました{}{} — supervisor 無効",
                kind.as_str(),
                if was_playing { " (再生停止)" } else { "" },
                export_suffix
            );
            return;
        };
        // v29: pipe の型分割に伴い respawn も kind 別 API。 どちらも
        // worker pool を新世代で mint するので、 生き残っている側の子にも
        // `CloseWorkerPool` + 新世代 `OpenWorkerPool` を送って pair を
        // 揃える (旧世代の poisoned pair / stale auto-reset signal を
        // 名前空間ごと捨てる — `common::plugin_ref` の contract)。
        let respawn_result: anyhow::Result<()> = match kind {
            ChildKind::Audio => supervisor.respawn_audio().map(|r| {
                self.ipc.audio_tx = Some(r.tx);
                // (v29 §3) 新 Hello の device_sample_rate を採用した session
                // に合わせて、 GUI 側の拍↔sample 変換の複製も更新する。
                if self.ipc.sample_rate != r.sample_rate {
                    tracing::info!(
                        old = self.ipc.sample_rate,
                        new = r.sample_rate,
                        "audio respawn changed the session sample rate"
                    );
                    self.ipc.sample_rate = r.sample_rate;
                }
                self.send_plugin(common::protocol::PluginCommand::CloseWorkerPool);
                self.send_plugin(r.pool.to_plugin_cmd());
            }),
            ChildKind::PluginHost => supervisor.respawn_plugin().map(|r| {
                self.ipc.plugin_tx = Some(r.tx);
                self.send_audio(common::protocol::AudioCommand::CloseWorkerPool);
                self.send_audio(r.pool.to_audio_cmd());
            }),
        };
        match respawn_result {
            Ok(()) => {
                // state restore: project_dir + LoadSong (= sync_song_to
                // _plugin_host 経路)、 plugin slots は restore_plugin_from
                // _song で SetSlotPlugin 再送。
                let song_snapshot = self.song_doc.song().clone();
                self.restore_plugin_from_song(&song_snapshot);
                // ループ (ON/OFF + 範囲) は `Song` に載らない session state なので
                // LoadSong では戻らない。 新しい audio プロセスは既定 (OFF / 範囲
                // 未設定) で立ち上がるため、 明示的に送り直して GUI 表示と engine の
                // 実挙動を揃える。
                if matches!(kind, ChildKind::Audio) {
                    self.set_loop_region(self.transport.loop_region);
                }
                self.ui_ephemeral.status_message = format!(
                    "{}を再起動しました{}{}",
                    kind.as_str(),
                    if was_playing { " (再生は手動で再開してください)" } else { "" },
                    export_suffix
                );
                tracing::info!(?kind, "child respawn + state restore completed");
            }
            Err(e) => {
                // (v29 §3) ビルド世代不一致は respawn を繰り返しても直らない。
                // ユーザー向けに「make build」 を明示し、 これ以上の自動
                // respawn には入らない (この handler は respawn 失敗時に
                // 再試行しないので、 メッセージ差し替えだけで終端する)。
                if e.is::<crate::bootstrap::FingerprintMismatch>() {
                    self.ui_ephemeral.status_message = format!(
                        "{} のビルドが古く protocol が一致しません — \
                         make build を実行してください (daw_audio.exe / daw_plugin_host.exe)",
                        kind.as_str()
                    );
                    tracing::error!(error = %e, ?kind, "child respawn refused: protocol fingerprint mismatch");
                } else {
                    self.ui_ephemeral.status_message = format!(
                        "{}の再起動に失敗しました: {}{} — アプリ再起動が必要です",
                        kind.as_str(),
                        e,
                        export_suffix
                    );
                    tracing::error!(error = %e, ?kind, "child respawn failed");
                }
            }
        }
    }

    pub(crate) fn add_automation_from_last_touched(&mut self) {
        let Some(touched) = self.ui_ephemeral.last_touched_param.clone() else {
            self.ui_ephemeral.status_message =
                "No parameter touched yet — drag any knob first".into();
            return;
        };
        // Phase 5 Step 5.1 (gui_01 #034): song-level target は master row の
        // `song_lanes` に追加 (= track 紐付け無し)。 TrackBuiltin / PluginParam
        // は従来通り該当 track の automation_lanes に追加。
        // r.md #8 再監査: master fx (`MASTER_TRACK_ID`) の PluginParam も master row の
        // `song_lanes` に置く (master は Track ではないので track_by_id で引けない)。
        let is_song_level = matches!(
            touched.target,
            common::model::AutomationTarget::SongTempo
                | common::model::AutomationTarget::SongTimeSigNumerator
        ) || touched.track_id == common::model::MASTER_TRACK_ID;
        // song-level でない場合のみ touched track が削除済か検査。
        if !is_song_level && self.song_doc.song().track_by_id(touched.track_id).is_none() {
            self.ui_ephemeral.last_touched_param = None;
            self.ui_ephemeral.status_message =
                "Last-touched parameter's track was removed".into();
            return;
        }
        // 既存 lane を find (target 一致)。 master か track かで lookup 経路が分岐。
        let existing_lane_id: Option<u32> = if is_song_level {
            self.song_doc.song()
                .song_lanes
                .iter()
                .find(|l| l.target == touched.target)
                .map(|l| l.id)
        } else {
            self.song_doc.song()
                .track_by_id(touched.track_id)
                .and_then(|t| {
                    t.automation_lanes
                        .iter()
                        .find(|l| l.target == touched.target)
                        .map(|l| l.id)
                })
        };
        if let Some(lane_id) = existing_lane_id {
            // 既存 lane を visible / enabled = true に戻して expand。
            let lookup_track_id = if is_song_level {
                common::model::MASTER_TRACK_ID
            } else {
                touched.track_id
            };
            self.edit_song(|song| {
                if let Some(lane) =
                    song.automation_lane_by_key_mut(lookup_track_id, lane_id)
                {
                    lane.visible = true;
                    lane.enabled = true;
                }
            });
            if is_song_level {
                self.ui_prefs.master_row_automation_expanded = true;
            } else {
                self.ui_prefs.expanded_automation_tracks.insert(touched.track_id);
            }
            self.ui_ephemeral.status_message = format!(
                "Automation lane '{}' は既に存在します",
                touched.display_name
            );
            return;
        }
        // 新規 lane を作成。default_value は target に応じて現在値を引く。
        let default_value = self.lane_default_for_target(&touched);
        if is_song_level {
            self.edit_song(|song| {
                let lane_id = song.alloc_song_lane_id();
                let new_lane = common::model::AutomationLane {
                    id: lane_id,
                    ..common::model::AutomationLane::new(touched.target.clone(), default_value)
                };
                song.song_lanes.push(new_lane);
            });
            self.ui_prefs.master_row_automation_expanded = true;
        } else {
            let __applied = self.edit_song_checked(|song| {
                let Some(track) = song.track_by_id_mut(touched.track_id) else {
                    return false;
                };
                let lane_id = track.alloc_lane_id();
                let new_lane = common::model::AutomationLane {
                    id: lane_id,
                    ..common::model::AutomationLane::new(touched.target.clone(), default_value)
                };
                track.automation_lanes.push(new_lane);
                true
            });
            if !__applied {
                return;
            }
            self.ui_prefs.expanded_automation_tracks.insert(touched.track_id);
        }
        self.ui_ephemeral.status_message = format!(
            "Added automation lane: {}",
            touched.display_name
        );
    }

    /// `AddAutomationFromLastTouched` の補助。target の現在値を plain
    /// 単位で取得 (lane.default_value 初期化用)。 track-builtin は track の strip 値、
    /// send gain は `track.sends[idx].gain`、 plugin param は `current_plain_value`
    /// の cache (A6 r.md #8)、 song-level は `song.bpm` / `song.time_sig.0`。
    pub(crate) fn lane_default_for_target(&self, touched: &TouchedParam) -> f64 {
        use common::model::{AutomationTarget, TrackBuiltinParam};
        match &touched.target {
            AutomationTarget::TrackBuiltin(param) => {
                let Some(track) = self.song_doc.song().track_by_id(touched.track_id) else {
                    return 0.0;
                };
                match param {
                    TrackBuiltinParam::Volume => f64::from(track.volume),
                    TrackBuiltinParam::Pan => f64::from(track.pan),
                    TrackBuiltinParam::Mute => {
                        if track.muted {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    // A6 (r.md #8): send gain の現在値は model にある。
                    // v29: 安定 send id 一致で引く。
                    TrackBuiltinParam::SendGain { send_id, .. } => track
                        .sends
                        .iter()
                        .find(|s| s.id == *send_id)
                        .map_or(0.0, |s| f64::from(s.gain)),
                }
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
                .song_doc
                .song()
                .all_mod_routings()
                .find(|r| r.id == *routing_id)
                .map_or(0.0, |r| f64::from(r.depth)),
            AutomationTarget::SongTempo => f64::from(self.song_doc.song().bpm),
            AutomationTarget::SongTimeSigNumerator => f64::from(self.song_doc.song().time_sig.0),
            // Image PiP default: 同 track の最初の image clip の first
            // event 値を初期値に使う。 1 つも image clip が無い (= lane
            // を空 image track で先行追加するケース) は 0.0 fallback。
            AutomationTarget::ImageBuiltin(field) => {
                use common::model::{ClipContent, ImageBuiltinParam};
                let Some(track) = self.song_doc.song().track_by_id(touched.track_id) else {
                    return 0.0;
                };
                let event = track.all_clips().find_map(|c| {
                    self.song_doc.song()
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
            // Text default: 同 track の first text event (セル込み) の field 値。
            // text clip が無い (= lane を空 track で先行追加) は field
            // ごとの常識値 (色 RGBA は (1,1,1,1) や (0,0,0,1) 等)。
            AutomationTarget::TextBuiltin(field) => {
                use common::model::{ClipContent, TextBuiltinParam as T};
                let Some(track) = self.song_doc.song().track_by_id(touched.track_id) else {
                    return 0.0;
                };
                let event = track.all_clips().find_map(|c| {
                    self.song_doc.song()
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
            // Group transform default: 同 track の group_transform (無ければ
            // GroupTransform::default) の該当 field。 group は表示 clip を持たない
            // ので image/text のような clip 探索は不要。
            AutomationTarget::GroupTransform(param) => {
                use common::model::GroupTransformParam as G;
                let gt = self
                    .song_doc.song()
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

    /// **アレンジの** automation クリップ (`lane.clips`) を消す。
    ///
    /// ランチャーのレーン行のセル (`lane.session_clips`) はここでは消えない —
    /// セルを消す口は [`AppData::delete_launcher_cells`] 1 本
    /// ([`EditSurface::LauncherCells`](crate::app::EditSurface::LauncherCells) の
    /// 削除経路) で、`normalize_session` もそちらが通す。
    pub(crate) fn delete_automation_clips(&mut self, keys: &[common::model::AutomationClipKey]) {
        if keys.is_empty() {
            return;
        }
        self.edit_song(|song| {
            for k in keys {
                let Some(lane) = song.automation_lane_by_key_mut(k.track, k.lane) else {
                    continue;
                };
                if let Some(idx) = lane.clip_index_by_id(k.clip) {
                    lane.clips.remove(idx);
                }
            }
        });
        // 選択中だった clip があれば selection からも除く。
        self.selection.selected_automation_clips
            .retain(|sel| !keys.iter().any(|k| k == sel));
    }

}
