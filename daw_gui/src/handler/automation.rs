//! handler::automation — instrument track 追加 + automation lane/point/clip の中核操作
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::model::{AudioEvent, InstrumentSource};

impl AppData {
    pub(crate) fn action_add_instrument_track(&mut self) {
        let index = self.song_doc.song().tracks.len() + 1;
        // 挿入位置は「選択中で最上段の track の直上」 (純ロジックは
        // add_track_insert_index)。 選択が無いときだけ従来どおり末尾。
        let insert_at = add_track_insert_index(&self.song_doc.song().tracks, &self.selection.selected_track_ids);
        // 新 track は挿入位置の基準 track (= 最上段の選択) と同じグループ
        // 階層に入れる (parent_group_id を継承)。基準が無い (= 選択無しで末尾挿入、
        // insert_at == tracks.len()) ときだけ master 直下 (None)。基準がグループ (子持ち)
        // でも「同じ階層 = 兄弟」になる (parent_group_id 継承がそのまま兄弟化する)。
        let parent_group_id = self.song_doc.song().tracks.get(insert_at).and_then(|t| t.parent_group_id);
        let Some(id) = self.edit_song(|song| {
            let id = song.alloc_track_id();
            let track = track_with(|t| {
                t.id = id;
                t.name = format!("Track {index}");
                t.source = InstrumentSource::None;
                t.clips = Vec::new();
                t.parent_group_id = parent_group_id;
            });
            song.tracks.insert(insert_at, track);
            id
        }) else {
            return;
        };
        // 追加直後はこの新 track を唯一の選択 + カーソルにする (次の操作の対象)。
        self.selection.selected_track_ids = vec![id];
        self.resize_track_peak_display();
        tracing::info!(insert_at, ?parent_group_id, "added instrument track");
    }

    // ----------------------------------------------------------------
    // Arranger セクション (曲のパート) の編集ハンドラ。gui_01 M14 Phase 127 の
    // 帯操作 emit を受けて適用する。undo は全て push_undo_snapshot (Song 丸ごと clone) で
    // Ctrl+Z 復帰可能。
    //
    // 現状は **帯 (Section エントリ) の作成 / 移動 / リサイズ / 複製** まで。「帯を動かすと
    // 範囲内の全 clip + automation + tempo + 拍子 + key も一緒に動く」破壊的フルスコープ
    // リフロー (境界での clip 分割 + ripple、`docs/plan_arranger_track.md` §3) は次段で
    // 実装する (gui_01 の lane 描画 landing と並行)。
    // ----------------------------------------------------------------

    /// 新規セクションを作る。`start` / `len` は widget で snap 済。名前は Intro/Aメロ/サビ…
    /// を巡回、色はパレットから採番。`normalize_sections` で昇順・非重複を保つ。
    pub(crate) fn apply_create_section(&mut self, start: f64, len: f64) {
        // len が正のときだけ作成 (NaN / 非正は無視)。
        if len > 0.0 {
            self.edit_song(|song| {
                let id = song.alloc_section_id();
                let n = song.sections.len();
                song.sections.push(common::model::Section {
                    id,
                    name: section_default_name(n),
                    color: section_default_color(n),
                    start_beat: start.max(0.0),
                    len_beats: len,
                });
                song.normalize_sections();
                tracing::info!(id, start, len, "created arranger section");
            });
        }
    }

    /// セクション帯を `next_start` へ**破壊的に移動**する (`Song::move_section`: 範囲内の
    /// 全トラック clip + automation + tempo/拍子/key を一緒に動かし、 前後を ripple)。 clip
    /// 位置が変わるので `flush_song_sync`。 移動が起きなければ undo snapshot を破棄。
    /// （境界をまたぐ clip の分割は `Song::move_section` の次段。）
    pub(crate) fn apply_move_section(&mut self, id: u32, next_start: f64) {
        self.edit_song_checked(|song| song.move_section(id, next_start));
    }

    /// セクション帯をリサイズする (被覆範囲の再定義、内容は動かさない)。
    pub(crate) fn apply_resize_section(&mut self, id: u32, next_start: f64, next_len: f64) {
        if next_len > 0.0 {
            self.edit_song(|song| {
                if let Some(s) = song.sections.iter_mut().find(|s| s.id == id) {
                    s.start_beat = next_start.max(0.0);
                    s.len_beats = next_len;
                }
            });
            self.edit_song(|song| song.normalize_sections());
        }
    }

    /// セクション帯を `dest_start` へ**複製挿入**する (`Song::duplicate_section`: 範囲内 content を
    /// linked コピーし、 dest 以降を ripple で空けて落とす)。 clip が増えるので
    /// `flush_song_sync`。 複製が起きなければ undo snapshot を破棄。
    pub(crate) fn apply_duplicate_section(&mut self, id: u32, dest_start: f64) {
        self.edit_song_checked(|song| song.duplicate_section(id, dest_start).is_some());
    }

    /// 「このセクションをループ」: 帯の範囲を既存ループ領域に設定する (ループの SSoT を駆動、
    /// 二重化しない)。
    pub(crate) fn apply_loop_section(&mut self, id: u32) {
        if let Some(s) = self.song_doc.song().sections.iter().find(|s| s.id == id) {
            let (start, end) = (s.start_beat, s.end_beat());
            self.handle_event(AppEvent::SetLoopRange { start, end });
        }
    }

    /// 「帯のみ削除」: セクション帯だけ消し、 内容は温存する (Studio One Backspace 相当)。
    pub(crate) fn apply_delete_section_band(&mut self, id: u32) {
        self.edit_song_checked(|song| song.delete_section(id));
    }

    /// 「範囲ごと削除」: セクションの時間範囲と内容を消して詰める (破壊的、 Delete Range 相当)。
    /// clip が変わるので plugin host へ sync。
    pub(crate) fn apply_delete_section_range(&mut self, id: u32) {
        self.edit_song_checked(|song| song.delete_section_range(id));
    }

    /// gui_01 の `SelectSection { id, modifier }` を解決してセクション選択集合を
    /// 更新する (`SelectModifier` は track header click と同 idiom、 末尾 = anchor)。 section を
    /// 選んだ時点で clip / note / track 等の他面選択をクリアし、 キーボード Delete が曖昧に
    /// ならないようにする (section は `edit_surface` の最低優先なので、 他選択が残っていると
    /// Delete がそちらを向く)。
    pub(crate) fn apply_select_section(
        &mut self,
        id: u32,
        modifier: crate::widgets::arrangement::SelectModifier,
    ) {
        // r.md #35: 選択遷移は全選択面共通の `SelectModifier::resolve` に統一
        // (`docs/plan_selection_modifiers.md` §4.2)。 アンカーは
        // `SelectionState.section_anchor` が所有する — 旧実装は「選択集合の末尾」 を
        // 基点にしていたが、 RangeFromAnchor が集合ごと書き換えるので Shift+click を
        // 繰り返すと基点が歩いて範囲を伸縮できなかった。
        let prev = self.selection.selected_section_ids.clone();
        let anchor = self.selection.section_anchor;
        // 帯は開始拍順に並べて 1 次元の範囲を取る。
        let ordered: Vec<u32> = {
            let mut v: Vec<&common::model::Section> = self.song_doc.song().sections.iter().collect();
            v.sort_by(|a, b| {
                a.start_beat
                    .partial_cmp(&b.start_beat)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.into_iter().map(|s| s.id).collect()
        };
        self.selection.selected_section_ids = modifier.resolve(&prev, id, || {
            crate::widgets::select_modifier::range_ordered(&ordered, anchor?, id)
        });
        if modifier.updates_anchor() {
            self.selection.section_anchor = Some(id);
        }
        // section を選んだら他面の選択を消す (Delete の曖昧さ回避、 §doc 参照)。
        self.selection.selected_clips.clear();
        self.selection.selected_clip = None;
        self.selection.selected_notes.clear();
        self.selection.selected_automation_clips.clear();
        self.selection.selected_automation_points.clear();
        self.selection.selected_track_ids.clear();
    }

    /// 選択中のセクション帯を削除する (帯のみ・内容温存、 キーボード Delete から)。
    pub(crate) fn apply_delete_selected_sections(&mut self) {
        if self.selection.selected_section_ids.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.selection.selected_section_ids);
        self.edit_song_checked(move |song| {
            let mut removed = false;
            for id in ids {
                removed |= song.delete_section(id);
            }
            removed
        });
    }

    // ----------------------------------------------------------------
    // gui_01 #028 (M14 Phase 63n-2) — automation lane / point handlers
    // ----------------------------------------------------------------

    pub(crate) fn set_lane_enabled(&mut self, track_id: u32, lane_id: u32, enabled: bool) {
        self.edit_song_checked(|song| {
            if let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) {
                lane.enabled = enabled;
                true
            } else {
                false
            }
        });
    }

    pub(crate) fn set_lane_visible(&mut self, track_id: u32, lane_id: u32, visible: bool) {
        self.edit_song_checked(|song| {
            if let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) {
                lane.visible = visible;
                true
            } else {
                false
            }
        });
    }

    /// Lane header default slider drag (release / live preview)。
    /// `next_norm` は normalized 0..=1、target に応じて plain 単位に
    /// 逆変換してから格納する。同時に last-touched param も更新する
    /// (lane default knob を回した後 `A` を押すと同 lane が visible
    /// 復活する閉ループ)。
    pub(crate) fn set_lane_default(&mut self, track_id: u32, lane_id: u32, next_norm: f32) {
        let Some(Some(target)) = self.edit_song(|song| {
            let lane = song.automation_lane_by_key_mut(track_id, lane_id)?;
            let target = lane.target.clone();
            lane.default_value = common::automation::norm_to_plain(&target, next_norm);
            Some(target)
        }) else {
            return;
        };
        let display_name = self.automation_target_label(track_id, &target);
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
    }

    /// gui_01 #030 (M14 Phase 63n-5): lane 高さ drag。`next_px` は
    /// widget 側で min/max に clamp 済なのでそのまま反映。
    pub(crate) fn set_lane_height(&mut self, track_id: u32, lane_id: u32, next_px: u16) {
        // ユーザーが明示的に lane を resize した = `Z` 縦ズームの一時拡大 (session
        // override) を破棄して model 高さに制御を戻す。
        self.ui_prefs.automation_lane_row_overrides
            .remove(&common::model::AutomationLaneKey { track: track_id, lane: lane_id });
        self.edit_song_checked(|song| {
            if let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) {
                lane.height_px = next_px;
                true
            } else {
                false
            }
        });
    }

    pub(crate) fn delete_lane(&mut self, track_id: u32, lane_id: u32) {
        // gui_01 #034 (Phase 63n-10): master row sentinel 対応。 song_lanes
        // の方にあれば該当 idx を探して remove、 通常 track なら従来通り。
        self.edit_song_checked(|song| {
            if track_id == common::model::MASTER_TRACK_ID {
                if let Some(idx) = song.song_lanes.iter().position(|l| l.id == lane_id) {
                    song.song_lanes.remove(idx);
                    return true;
                }
                false
            } else if let Some(track) = song.track_by_id_mut(track_id)
                && let Some(idx) = track.lane_index_by_id(lane_id)
            {
                track.automation_lanes.remove(idx);
                // 共有先のなくなった clip_contents は次の save / GC で
                // 自動回収。
                true
            } else {
                false
            }
        });
    }

    /// dblclick on lane body → 1 point 追加。clip-local `time_beat`
    /// 昇順を保つよう挿入位置を二分探索で決める。
    pub(crate) fn add_automation_point(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        time_beat: f64,
        value_norm: f32,
    ) {
        self.edit_song_checked(|song| {
            let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
                return false;
            };
            let target = lane.target.clone();
            let Some(clip) = lane.clip_by_id(clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let plain = common::automation::norm_to_plain(&target, value_norm);
            let entry = song.clip_contents.entry(content_id).or_insert_with(|| {
                common::model::ClipContent::Automation(
                    common::model::AutomationContent::default(),
                )
            });
            let content = match entry {
                common::model::ClipContent::Automation(a) => a,
                _ => {
                    tracing::warn!(
                        content_id,
                        "AddAutomationPoint: content variant is not Automation, skipping"
                    );
                    return false;
                }
            };
            // v29: ドキュメントに入る新規要素は必ず allocator で安定 id を採番。
            let new_point = common::model::AutomationPoint {
                id: content.alloc_point_id(),
                time_beat,
                value: plain,
                curve: common::model::AutomationCurve::Linear,
            };
            let points = &mut content.points;
            let insert_at = points.partition_point(|p| p.time_beat <= time_beat);
            points.insert(insert_at, new_point);
            true
        });
    }

    pub(crate) fn move_automation_points(&mut self, deltas: &[MoveAutomationPointEntry]) {
        if deltas.is_empty() {
            return;
        }
        // 各 delta の lane.target を引いて plain 化、同 clip 内の point
        // を一括更新後に sort で昇順を保つ。同一 clip 複数 point は
        // group して 1 度の sort で済ませる。
        self.edit_song(|song| {
            let mut touched: std::collections::HashSet<common::model::ContentId> =
                std::collections::HashSet::new();
            for delta in deltas {
                let Some(lane) =
                    song.automation_lane_by_key_mut(delta.key.track_id, delta.key.lane_id)
                else {
                    continue;
                };
                let target = lane.target.clone();
                let Some(clip) = lane.clip_by_id(delta.key.clip_id) else {
                    continue;
                };
                let content_id = clip.content_id;
                let plain = common::automation::norm_to_plain(&target, delta.next_value_norm);
                let Some(entry) = song.clip_contents.get_mut(&content_id) else {
                    continue;
                };
                let common::model::ClipContent::Automation(a) = entry else {
                    continue;
                };
                if let Some(p) = a.points.get_mut(delta.key.point_idx as usize) {
                    p.time_beat = delta.next_time_beat;
                    p.value = plain;
                    touched.insert(content_id);
                }
            }
            for cid in touched {
                if let Some(common::model::ClipContent::Automation(a)) =
                    song.clip_contents.get_mut(&cid)
                {
                    a.points.sort_by(|p1, p2| {
                        p1.time_beat
                            .partial_cmp(&p2.time_beat)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
            }
        });
    }

    /// point の現在値 (plain 単位) を読む。inline 数値入力の prefill /
    /// 編集開始可否判定に使う。master row (`MASTER_TRACK_ID`) lane も
    /// `automation_lane_by_key` が解決する。
    pub(crate) fn automation_point_value(&self, key: &AutomationPointKeyRef) -> Option<f64> {
        let lane = self
            .song_doc.song()
            .automation_lane_by_key(key.track_id, key.lane_id)?;
        let clip = lane.clip_by_id(key.clip_id)?;
        let content = self.song_doc.song().clip_contents.get(&clip.content_id)?;
        let pts = content.automation_points()?;
        pts.get(key.point_idx as usize).map(|p| p.value)
    }

    /// point の値を **plain 単位の絶対値**で上書き (inline 数値入力の確定)。
    /// `value` は呼び出し側 (`arrangement_view`) で表示単位レンジに clamp +
    /// `from_display` 済の plain。時間 (`time_beat`) は変えないので sort 順は不変。
    pub(crate) fn set_automation_point_value(&mut self, key: &AutomationPointKeyRef, value: f64) {
        self.edit_song_checked(|song| {
            let Some(lane) = song.automation_lane_by_key(key.track_id, key.lane_id) else {
                return false;
            };
            let Some(clip) = lane.clip_by_id(key.clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Automation(a)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            if let Some(p) = a.points.get_mut(key.point_idx as usize) {
                p.value = value;
            }
            true
        });
    }

    pub(crate) fn delete_automation_points(&mut self, points: &[AutomationPointKeyRef]) {
        if points.is_empty() {
            return;
        }
        // 同じ content_id でまとめて、index 降順で削除 (前から消すと
        // 後の index がずれるため)。
        let mut by_content: std::collections::HashMap<
            common::model::ContentId,
            Vec<u32>,
        > = std::collections::HashMap::new();
        for k in points {
            let Some(lane) = self.song_doc.song().automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            by_content.entry(clip.content_id).or_default().push(k.point_idx);
        }
        self.edit_song(move |song| {
            for (cid, mut indices) in by_content {
                indices.sort_unstable_by(|a, b| b.cmp(a));
                indices.dedup();
                if let Some(common::model::ClipContent::Automation(a)) =
                    song.clip_contents.get_mut(&cid)
                {
                    for idx in indices {
                        if (idx as usize) < a.points.len() {
                            a.points.remove(idx as usize);
                        }
                    }
                }
            }
        });
        // (review) point_idx は positional なので削除で全 index がずれる。 残すと
        // 次の Del / Cut が詰め後の別の点を破壊する (`delete_selected_notes` の
        // `mem::take` と同じ後始末)。 inline 編集中の点も同様に無効化する。
        self.selection.selected_automation_points.clear();
        self.ui_ephemeral.editing_automation_point = None;
    }

    pub(crate) fn set_automation_curve_type(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        next: common::model::AutomationCurve,
    ) {
        self.edit_song_checked(|song| {
            let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
                return false;
            };
            let Some(clip) = lane.clip_by_id(clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Automation(a)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            if let Some(p) = a.points.get_mut(point_idx as usize) {
                p.curve = next;
                true
            } else {
                false
            }
        });
    }

    /// gui_01 #033 Phase 63n-9: Bezier curve handle drag release で 1 件
    /// 発火される `SetAutomationCurveBezierTension` の handler。 既存
    /// curve type が `Bezier` でない場合は no-op (= race / 仕様外発火)。
    /// `next` は widget で `-1.0..=1.0` clamp 済だが、 defensive で再 clamp。
    pub(crate) fn set_automation_curve_bezier_tension(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        next: f32,
    ) {
        self.edit_song_checked(|song| {
            let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
                return false;
            };
            let Some(clip) = lane.clip_by_id(clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Automation(a)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            if let Some(p) = a.points.get_mut(point_idx as usize)
                && matches!(p.curve, common::model::AutomationCurve::Bezier { .. })
            {
                p.curve = common::model::AutomationCurve::Bezier {
                    tension: next.clamp(-1.0, 1.0),
                };
                true
            } else {
                false
            }
        });
    }

    /// gui_01 #033 Phase 63n-9: Exponential curve handle drag release で
    /// 発火される `SetAutomationCurveExponentialBend` の handler。 既存
    /// curve type が `Exponential` でない場合は no-op。
    pub(crate) fn set_automation_curve_exponential_bend(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        next: f32,
    ) {
        self.edit_song_checked(|song| {
            let Some(lane) = song.automation_lane_by_key_mut(track_id, lane_id) else {
                return false;
            };
            let Some(clip) = lane.clip_by_id(clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Automation(a)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            if let Some(p) = a.points.get_mut(point_idx as usize)
                && matches!(p.curve, common::model::AutomationCurve::Exponential { .. })
            {
                p.curve = common::model::AutomationCurve::Exponential {
                    bend: next.clamp(-1.0, 1.0),
                };
                true
            } else {
                false
            }
        });
    }

    /// Phase 3: `selected_automation_points` を grid (`1/div` beat) に snap。
    /// piano roll の [`Self::quantize_selected_notes`] と同 idiom。 sort
    /// invariant を維持するため snap 後に各 clip 内 point 列を sort し直し、
    /// `selected_automation_points` も新 idx で再構築する。 selection 再
    /// 構築は `(snapped_time, value)` で lookup する (point に stable id
    /// が無いので、 同 frame 内の値ペアで identify)。 同 clip 内に snap
    /// 結果が同位置になる point が複数いれば最初の一致を採用。
    pub(crate) fn quantize_selected_automation_points(&mut self, div: u8) {
        if self.selection.selected_automation_points.is_empty() {
            return;
        }
        let div = div.max(1) as f64;
        let snap = |b: f64| ((b * div).round() / div).max(0.0);
        let selected = self.selection.selected_automation_points.clone();

        // `content_id` ごとに、 quantize 対象 idx 群と、 selection lookup 用の
        // `(snapped_time, value)` ペア群を集める。 ペアは selection の現順序
        // を維持するため Vec で持つ。
        #[derive(Clone, Copy)]
        struct Owner {
            track_id: u32,
            lane_id: u32,
            clip_id: u32,
        }
        struct ContentBuckets {
            owner: Owner,
            idxs: Vec<u32>,
            lookups: Vec<(f64, f64)>,
        }
        let mut by_content: std::collections::HashMap<
            common::model::ContentId,
            ContentBuckets,
        > = std::collections::HashMap::new();
        for k in &selected {
            let Some(lane) = self.song_doc.song().automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Automation(a)) =
                self.song_doc.song().clip_contents.get(&content_id)
            else {
                continue;
            };
            let Some(p) = a.points.get(k.point_idx as usize) else {
                continue;
            };
            let entry = by_content.entry(content_id).or_insert_with(|| ContentBuckets {
                owner: Owner {
                    track_id: k.track_id,
                    lane_id: k.lane_id,
                    clip_id: k.clip_id,
                },
                idxs: Vec::new(),
                lookups: Vec::new(),
            });
            entry.idxs.push(k.point_idx);
            entry.lookups.push((snap(p.time_beat), p.value));
        }

        let cap = selected.len();
        let Some(new_selection) = self.edit_song(move |song| {
            let mut new_selection: Vec<AutomationPointKeyRef> = Vec::with_capacity(cap);
            for (content_id, bucket) in by_content {
                let ContentBuckets {
                    owner,
                    idxs,
                    lookups,
                } = bucket;
                let Some(common::model::ClipContent::Automation(a)) =
                    song.clip_contents.get_mut(&content_id)
                else {
                    continue;
                };
                // snap 対象 point の time_beat を書き換え。 重複 idx は HashSet
                // で除去せず、 set_mut が冪等なのでそのまま再代入。
                for idx in &idxs {
                    if let Some(p) = a.points.get_mut(*idx as usize) {
                        p.time_beat = snap(p.time_beat);
                    }
                }
                a.points.sort_by(|p1, p2| {
                    p1.time_beat
                        .partial_cmp(&p2.time_beat)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                // 新 idx を `(snapped_time, value)` で lookup。
                for (st, sv) in &lookups {
                    if let Some(new_idx) = a.points.iter().position(|p| {
                        (p.time_beat - st).abs() < 1e-9 && (p.value - sv).abs() < 1e-9
                    }) {
                        new_selection.push(AutomationPointKeyRef {
                            track_id: owner.track_id,
                            lane_id: owner.lane_id,
                            clip_id: owner.clip_id,
                            point_idx: new_idx as u32,
                        });
                    }
                }
            }
            new_selection
        }) else {
            return;
        };

        self.selection.selected_automation_points = new_selection;
    }

    /// Phase 3: 選択中 automation point を JSON 化して OS clipboard に
    /// 出せるよう text を返す。 [`Self::copy_notes_clip`] と同
    /// idiom。 point の `value` は target ごとに値域が違う (Volume:
    /// 0..=2.0、 Pan: -1..=1 等) ので、 lane の `target` を引いて
    /// **normalized 0..=1** で serialize する。 paste 側でも target を
    /// 引いて plain に戻す (= target が違う lane に貼っても curve の
    /// shape を保てる、 Bitwig 流)。
    ///
    /// 戻り値は `(json, count)`。 何も copy できない (選択無し / lookup
    /// 失敗) 場合は `None`。
    pub fn copy_points_clip(&self) -> Option<(String, usize)> {
        if self.selection.selected_automation_points.is_empty() {
            return None;
        }
        let mut copied: Vec<crate::clipboard::CopiedPoint> =
            Vec::with_capacity(self.selection.selected_automation_points.len());
        for k in &self.selection.selected_automation_points {
            let Some(lane) = self.song_doc.song().automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            let Some(common::model::ClipContent::Automation(a)) =
                self.song_doc.song().clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            let Some(p) = a.points.get(k.point_idx as usize) else {
                continue;
            };
            let value_norm = common::automation::plain_to_norm(&lane.target, p.value);
            copied.push(crate::clipboard::CopiedPoint {
                time_beat: p.time_beat,
                value_norm,
                curve: p.curve,
            });
        }
        if copied.is_empty() {
            return None;
        }
        // earliest time_beat を anchor として 0.0 にシフト (Note と同じ)。
        let earliest = copied
            .iter()
            .map(|p| p.time_beat)
            .fold(f64::INFINITY, f64::min);
        if earliest.is_finite() {
            for p in &mut copied {
                p.time_beat -= earliest;
            }
        }
        let count = copied.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            crate::clipboard::ClipboardPayload::AutomationPoints(copied),
        )
        .to_json()?;
        Some((json, count))
    }

    /// `CopiedPoint` 群を「マウス下の automation lane」の `song_beat`
    /// (song-absolute 拍) を含む automation clip に貼る。clip が無い (レーンの空き)
    /// なら no-op + status。`song_beat - clip.start` を clip-local anchor とし、各 point の
    /// 相対 `time_beat` を加算。value は lane.target に応じ norm→plain 復元して sort 維持
    /// insert。貼った点群を新選択にする。戻り値は挿入数。
    pub fn paste_points_at(
        &mut self,
        points_in: Vec<crate::clipboard::CopiedPoint>,
        lane_key: common::model::AutomationLaneKey,
        song_beat: f64,
    ) -> usize {
        if points_in.is_empty() {
            return 0;
        }
        let Some(lane) = self.song_doc.song().automation_lane_by_key(lane_key.track, lane_key.lane) else {
            return 0;
        };
        let target = lane.target.clone();
        let Some(clip) = lane
            .clips
            .iter()
            .find(|c| song_beat >= c.start_beat && song_beat < c.start_beat + c.length_beats)
        else {
            self.ui_ephemeral.status_message =
                "貼り付け先の automation clip がありません (レーンの空き)".to_string();
            return 0;
        };
        let dest_key = common::model::AutomationClipKey {
            track: lane_key.track,
            lane: lane_key.lane,
            clip: clip.id,
        };
        let content_id = clip.content_id;
        let anchor = (song_beat - clip.start_beat).max(0.0);

        // dest content が automation でない壊れたモデルなら undo を触る前に bail。
        if let Some(c) = self.song_doc.song().clip_contents.get(&content_id)
            && !matches!(c, common::model::ClipContent::Automation(_))
        {
            self.ui_ephemeral.status_message =
                "貼り付け先 clip が automation でない (型不整合)".to_string();
            return 0;
        }

        let count = points_in.len();
        let Some(Some(new_indices)) = self.edit_song(move |song| {
            let entry = song
                .clip_contents
                .entry(content_id)
                .or_insert_with(|| {
                    common::model::ClipContent::Automation(
                        common::model::AutomationContent::default(),
                    )
                });
            let content = match entry {
                common::model::ClipContent::Automation(a) => a,
                _ => return None,
            };

            // 挿入後の新 idx は sort のたび変動するので、 全 point を挿入し
            // 終えてから「挿入した値ペア」 で再 lookup する。
            let mut inserted_pairs: Vec<(f64, f64)> = Vec::with_capacity(points_in.len());
            for src in &points_in {
                let plain = common::automation::norm_to_plain(&target, src.value_norm);
                let t = (src.time_beat + anchor).max(0.0);
                // v29: 新規 point は allocator で安定 id を採番。
                let new_point = common::model::AutomationPoint {
                    id: content.alloc_point_id(),
                    time_beat: t,
                    value: plain,
                    curve: src.curve,
                };
                let insert_at = content.points.partition_point(|p| p.time_beat <= t);
                content.points.insert(insert_at, new_point);
                inserted_pairs.push((t, plain));
            }
            let points = &mut content.points;

            Some(
                inserted_pairs
                    .iter()
                    .filter_map(|(t, v)| {
                        points
                            .iter()
                            .position(|p| {
                                (p.time_beat - t).abs() < 1e-9 && (p.value - v).abs() < 1e-9
                            })
                            .map(|i| i as u32)
                    })
                    .collect::<Vec<u32>>(),
            )
        }) else {
            return 0;
        };

        self.selection.selected_automation_points = new_indices
            .into_iter()
            .map(|i| AutomationPointKeyRef {
                track_id: dest_key.track,
                lane_id: dest_key.lane,
                clip_id: dest_key.clip,
                point_idx: i,
            })
            .collect();
        self.selection.last_edit_select = Some(EditSelectSurface::AutomationPoints);
        count
    }

    // -------- audio event clipboard --------

    /// オーディオエディタで選択中のイベントを clipboard envelope
    /// (`ClipboardPayload::AudioEvents`) JSON に。最早 start を 0 とした相対に正規化。
    pub fn copy_events_clip(&self) -> Option<(String, usize)> {
        let r = self.ui_ephemeral.audio_editor_clip?;
        if self.selection.audio_editor_selected_events.is_empty() {
            return None;
        }
        let track = self.song_doc.song().tracks.get(r.track as usize)?;
        let clip = track.clips.get(r.clip as usize)?;
        let content = self.song_doc.song().clip_contents.get(&clip.content_id)?;
        let events = content.audio_events()?;
        let mut copied: Vec<AudioEvent> = self
            .selection.audio_editor_selected_events
            .iter()
            .filter_map(|i| events.get(*i).cloned())
            .collect();
        if copied.is_empty() {
            return None;
        }
        let earliest = copied
            .iter()
            .map(|e| e.event_start_in_clip_beats)
            .fold(f64::INFINITY, f64::min);
        if earliest.is_finite() {
            for e in &mut copied {
                e.event_start_in_clip_beats -= earliest;
            }
        }
        let count = copied.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            crate::clipboard::ClipboardPayload::AudioEvents(copied),
        )
        .to_json()?;
        Some((json, count))
    }

    /// イベント群を「編集中オーディオクリップ (`audio_editor_clip`)」の `at_beat`
    /// (clip-local 拍) に貼る。`events` は最早=0 正規化済み相対。値域は呼び出し側で
    /// sanitize 済み。clip 長を必要なら拡張し、貼ったイベント群を新選択にする。戻り値は挿入数。
    pub fn paste_events_at(&mut self, mut events: Vec<AudioEvent>, at_beat: f64) -> usize {
        if events.is_empty() {
            return 0;
        }
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            self.ui_ephemeral.status_message = "貼り付け先のオーディオクリップがありません".to_string();
            return 0;
        };
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return 0;
        };
        if !matches!(
            self.song_doc.song().clip_contents.get(&content_id),
            Some(common::model::ClipContent::Audio(_))
        ) {
            self.ui_ephemeral.status_message = "貼り付け先 clip が audio でない".to_string();
            return 0;
        }
        let anchor = at_beat.max(0.0);
        let count = events.len();
        let Some(Some(new_indices)) = self.edit_song(move |song| {
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return None;
            };
            let mut new_indices = Vec::with_capacity(events.len());
            let mut max_end = 0.0f64;
            for e in &mut events {
                e.event_start_in_clip_beats += anchor;
                // clipboard の AudioEvent は元 content の id を持つ。 貼り付け先 content
                // で per-content 一意 id 不変条件 (invariant #1) を守るため再採番する
                // (paste_points_at と同 idiom、 M4 sibling)。
                e.id = audio.alloc_event_id();
                max_end = max_end.max(e.event_start_in_clip_beats + e.event_length_beats);
                new_indices.push(audio.events.len());
                audio.events.push(e.clone());
            }
            // clip 長が足りなければ拡張 (add_audio_event_from_file と同 idiom)。
            if let Some(track) = song.tracks.get_mut(target.track as usize)
                && let Some(clip) = track.clips.get_mut(target.clip as usize)
                && max_end > clip.length_beats
            {
                clip.length_beats = max_end;
            }
            Some(new_indices)
        }) else {
            return 0;
        };
        self.selection.audio_editor_selected_events = new_indices;
        if self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        count
    }

    // -------- clip clipboard --------

    /// 選択中クリップ群を clipboard envelope (`ClipboardPayload::Clips`) JSON に。
    /// 最上段トラックを `track_offset` 0、最早 start を `start_beat` 0 とした相対で
    /// 正規化。content payload と name を inline 同梱 (別プロジェクト独立復元用)、
    /// `content_id` も保持 (同一プロジェクトのリンク共有用)。
    pub fn copy_clips_clip(&self) -> Option<(String, usize)> {
        let refs = self.selected_clip_refs();
        if refs.is_empty() {
            return None;
        }
        let mut resolved: Vec<(usize, common::model::Clip)> = Vec::new();
        for r in &refs {
            if let Some(t) = self.song_doc.song().tracks.get(r.track as usize)
                && let Some(c) = t.clips.get(r.clip as usize)
            {
                resolved.push((r.track as usize, c.clone()));
            }
        }
        if resolved.is_empty() {
            return None;
        }
        let min_track = resolved.iter().map(|(ti, _)| *ti).min().unwrap_or(0);
        let earliest = resolved
            .iter()
            .map(|(_, c)| c.start_beat)
            .fold(f64::INFINITY, f64::min);
        let base = if earliest.is_finite() { earliest } else { 0.0 };
        let mut clips = Vec::with_capacity(resolved.len());
        for (ti, c) in &resolved {
            let content = self
                .song_doc.song()
                .clip_contents
                .get(&c.content_id)
                .cloned()
                .unwrap_or_default();
            let name = self.song_doc.song().clip_content_names.get(&c.content_id).cloned();
            clips.push(crate::clipboard::ClipCopy {
                track_offset: (*ti as i64) - (min_track as i64),
                start_beat: c.start_beat - base,
                length_beats: c.length_beats,
                color: c.color,
                auto_lipsync: c.auto_lipsync,
                // clip-level mute も clipboard へ。
                muted: c.muted,
                content_id: c.content_id,
                content,
                name,
                // per-clip 声を clipboard へ。
                speaker_id: c.speaker_id,
                singer_name: c.singer_name.clone(),
                style_name: c.style_name.clone(),
                // (talk) per-clip 読み上げスケールも clipboard へ。
                talk: c.talk,
            });
        }
        let count = clips.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            crate::clipboard::ClipboardPayload::Clips(clips),
        )
        .to_json()?;
        Some((json, count))
    }

    /// クリップ群を「マウス下トラック (`anchor_track`)」を基準に `at_beat` (song-absolute,
    /// snap 済) へ貼る。`track_offset` で相対トラック、`start_beat` で相対拍を復元。
    /// content は同一プロジェクト (`src_pid == project_id`) かつ content が現存すれば流用
    /// (リンク共有)、そうでなければ inline payload から新 content_id 採番 (独立)。
    /// 貼ったクリップ群を新選択にする。戻り値は挿入数。
    pub fn paste_clips_at(
        &mut self,
        clips: Vec<crate::clipboard::ClipCopy>,
        src_pid: u64,
        anchor_track: u32,
        at_beat: f64,
    ) -> usize {
        if clips.is_empty() {
            return 0;
        }
        let Some(anchor_idx) = self.song_doc.song().track_index_by_id(anchor_track) else {
            self.ui_ephemeral.status_message = "貼り付け先のトラックがありません".to_string();
            return 0;
        };
        let same_project = src_pid == self.song_doc.song().project_id;
        // 貼り付け対象 (target_idx が範囲内) が 1 件も無ければ undo を積まず return
        // (= spurious な no-op undo step を作らない、paste_notes_at と同方針)。
        let any_valid = clips.iter().any(|cc| {
            let ti = anchor_idx as i64 + cc.track_offset;
            ti >= 0 && (ti as usize) < self.song_doc.song().tracks.len()
        });
        if !any_valid {
            self.ui_ephemeral.status_message = "貼り付け先のトラックがありません".to_string();
            return 0;
        }
        // content remap: 同一 source content_id は 1 度だけ採番して dedup する
        // (linked クリップ群を複数貼っても貼り付け後もリンクを保つ)。同一プロジェクト
        // かつ content 現存なら流用 (リンク共有)、それ以外は inline payload から独立採番。
        let Some(new_refs) = self.edit_song(move |song| {
            let mut content_remap: std::collections::HashMap<
                common::model::ContentId,
                common::model::ContentId,
            > = std::collections::HashMap::new();
            let mut new_refs: Vec<ClipRef> = Vec::new();
            for cc in &clips {
                let target_idx = anchor_idx as i64 + cc.track_offset;
                if target_idx < 0 || target_idx as usize >= song.tracks.len() {
                    continue;
                }
                let target_idx = target_idx as usize;
                let content_id = if let Some(&new) = content_remap.get(&cc.content_id) {
                    new
                } else {
                    let resolved =
                        if same_project && song.clip_contents.contains_key(&cc.content_id) {
                            cc.content_id
                        } else {
                            song.alloc_content(
                                cc.content.clone(),
                                cc.name.clone().unwrap_or_default(),
                            )
                        };
                    content_remap.insert(cc.content_id, resolved);
                    resolved
                };
                let Some(to_track) = song.tracks.get_mut(target_idx) else {
                    continue;
                };
                let new_clip_id = to_track.alloc_clip_id();
                let new_idx = to_track.clips.len() as u32;
                to_track.clips.push(common::model::Clip {
                    id: new_clip_id,
                    start_beat: (at_beat + cc.start_beat).max(0.0),
                    length_beats: cc.length_beats,
                    content_id,
                    color: cc.color,
                    auto_lipsync: cc.auto_lipsync,
                    // clipboard は配置世代を運ばないので「世代不明」= 0。
                    // auto_lipsync clip なら次の load / 再生成で作り直される。
                    lipsync_gen: 0,
                    // clipboard の clip-level mute を paste 先 clip へ引き継ぐ。
                    muted: cc.muted,
                    // clipboard の per-clip 声を paste 先 clip へ引き継ぐ。
                    speaker_id: cc.speaker_id,
                    singer_name: cc.singer_name.clone(),
                    style_name: cc.style_name.clone(),
                    // (talk) per-clip 読み上げスケールも引き継ぐ。
                    talk: cc.talk,
                });
                new_refs.push(ClipRef {
                    track: target_idx as u32,
                    clip: new_idx,
                });
            }
            new_refs
        }) else {
            return 0;
        };
        let pasted = new_refs.len();
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selection.selected_notes.clear();
        }
        pasted
    }

    // -------- automation clip clipboard (オートメーションクリップの copy / cut / paste) --------

    /// 選択中 automation clip 群を clipboard envelope (`ClipboardPayload::AutomationClips`)
    /// JSON に。 最早 clip start を `start_beat` 0 とした相対で正規化、 curve 点は lane の
    /// `target` を引いて **clip-local 時間 + normalized 値** (`CopiedPoint`) で serialize する
    /// (= automation point copy と同じ idiom、 target が違う lane に貼っても shape を温存)。
    /// `source_content_id` を保持して linked group を paste 後も保つ。 戻り値は `(json, count)`、
    /// 選択無し / 解決失敗なら `None`。
    pub fn copy_automation_clips_clip(&self) -> Option<(String, usize)> {
        if self.selection.selected_automation_clips.is_empty() {
            return None;
        }
        let mut resolved = Vec::new();
        for k in &self.selection.selected_automation_clips {
            let Some(lane) = self.song_doc.song().automation_lane_by_key(k.track, k.lane) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip) else {
                continue;
            };
            resolved.push((lane.target.clone(), clip.clone()));
        }
        if resolved.is_empty() {
            return None;
        }
        let earliest = resolved
            .iter()
            .map(|(_, c)| c.start_beat)
            .fold(f64::INFINITY, f64::min);
        let base = if earliest.is_finite() { earliest } else { 0.0 };
        let mut out = Vec::with_capacity(resolved.len());
        for (target, clip) in &resolved {
            let points: Vec<crate::clipboard::CopiedPoint> =
                match self.song_doc.song().clip_contents.get(&clip.content_id) {
                    Some(common::model::ClipContent::Automation(a)) => a
                        .points
                        .iter()
                        .map(|p| crate::clipboard::CopiedPoint {
                            time_beat: p.time_beat,
                            value_norm: common::automation::plain_to_norm(target, p.value),
                            curve: p.curve,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
            let name = self.song_doc.song().clip_content_names.get(&clip.content_id).cloned();
            out.push(crate::clipboard::AutomationClipCopy {
                start_beat: clip.start_beat - base,
                length_beats: clip.length_beats,
                source_content_id: clip.content_id,
                points,
                name,
            });
        }
        let count = out.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            self.song_doc.song().project_id,
            crate::clipboard::ClipboardPayload::AutomationClips(out),
        )
        .to_json()?;
        Some((json, count))
    }

    /// `AutomationClipCopy` 群を「マウス下の automation lane」(`lane_key`) の `at_beat`
    /// (song-absolute, snap 済) を基準に貼る。 各 clip の相対 `start_beat` を加算して
    /// `start_beat` 昇順 insert、 curve は paste 先 lane の `target` で norm→plain 復元する
    /// (= 独立 content を新規採番。 同一 `source_content_id` を共有していた clip 群は同じ
    /// 新 content を指して内部リンクを保つ)。 貼った clip 群を新選択にする。 戻り値は挿入数。
    pub fn paste_automation_clips_at(
        &mut self,
        clips: Vec<crate::clipboard::AutomationClipCopy>,
        lane_key: common::model::AutomationLaneKey,
        at_beat: f64,
    ) -> usize {
        if clips.is_empty() {
            return 0;
        }
        let Some(lane) = self.song_doc.song().automation_lane_by_key(lane_key.track, lane_key.lane) else {
            self.ui_ephemeral.status_message = "貼り付け先の automation lane がありません".to_string();
            return 0;
        };
        let target = lane.target.clone();
        let Some(new_keys) = self.edit_song(move |song| {
            // 同一 source content_id は 1 度だけ採番して dedup (= linked group を paste 後も保つ)。
            let mut content_remap: std::collections::HashMap<
                common::model::ContentId,
                common::model::ContentId,
            > = std::collections::HashMap::new();
            let mut new_keys: Vec<common::model::AutomationClipKey> = Vec::new();
            for cc in &clips {
                let content_id = if let Some(&id) = content_remap.get(&cc.source_content_id) {
                    id
                } else {
                    // v29: 新規 content の点にも安定 id を採番する (1 始まりの
                    // 連番 = per-content allocator と同じ)。
                    let mut points: Vec<common::model::AutomationPoint> = cc
                        .points
                        .iter()
                        .enumerate()
                        .map(|(i, p)| common::model::AutomationPoint {
                            id: i as u32 + 1,
                            time_beat: p.time_beat.max(0.0),
                            value: common::automation::norm_to_plain(&target, p.value_norm),
                            curve: p.curve,
                        })
                        .collect();
                    points.sort_by(|a, b| a.time_beat.total_cmp(&b.time_beat));
                    let content = common::model::ClipContent::Automation(
                        common::model::AutomationContent {
                            next_point_id: points.len() as u32 + 1,
                            points,
                        },
                    );
                    let id = song.alloc_content(content, cc.name.clone().unwrap_or_default());
                    content_remap.insert(cc.source_content_id, id);
                    id
                };
                let Some(lane) =
                    song.automation_lane_by_key_mut(lane_key.track, lane_key.lane)
                else {
                    continue;
                };
                let new_id = lane.alloc_clip_id();
                let start_beat = (at_beat + cc.start_beat).max(0.0);
                let new_clip = common::model::AutomationClip {
                    id: new_id,
                    name: String::new(),
                    start_beat,
                    length_beats: cc.length_beats,
                    content_id,
                };
                let pos = lane.clips.partition_point(|c| c.start_beat < start_beat);
                lane.clips.insert(pos, new_clip);
                new_keys.push(common::model::AutomationClipKey {
                    track: lane_key.track,
                    lane: lane_key.lane,
                    clip: new_id,
                });
            }
            new_keys
        }) else {
            return 0;
        };
        let pasted = new_keys.len();
        if pasted > 0 {
            self.selection.selected_automation_clips = new_keys;
            // 貼ったばかりの clip を直後の copy/cut/delete 対象にする: 競合する点選択を
            // 解除し (paste_clips_at が selected_notes を clear するのと同じ)、 last-wins も
            // clip 側に倒す。
            self.selection.selected_automation_points.clear();
            self.selection.last_edit_select = Some(EditSelectSurface::AutomationClips);
        }
        pasted
    }

    /// 修飾なし drag release。source lane から取り出して target lane へ
    /// `start_beat` 昇順 insert。lane 跨ぎ可、target 不一致でも accept
    /// (curve は normalized なので意味温存、`docs/plan_automation.md`
    /// §5.4)。
    pub(crate) fn move_automation_clips(&mut self, deltas: &[MoveAutomationClipEntry]) {
        if deltas.is_empty() {
            return;
        }
        self.edit_song(|song| {
            for d in deltas {
                let mut taken: Option<common::model::AutomationClip> = None;
                if let Some(source_lane) =
                    song.automation_lane_by_key_mut(d.from.track, d.from.lane)
                    && let Some(idx) = source_lane.clip_index_by_id(d.from.clip)
                {
                    taken = Some(source_lane.clips.remove(idx));
                }
                let Some(mut clip) = taken else { continue };
                clip.start_beat = d.next_start_beat;
                if let Some(target_lane) =
                    song.automation_lane_by_key_mut(d.to_lane.track, d.to_lane.lane)
                {
                    let start = clip.start_beat;
                    let pos = target_lane
                        .clips
                        .partition_point(|c| c.start_beat < start);
                    target_lane.clips.insert(pos, clip);
                }
            }
        });
    }

    /// Ctrl+drag release。source は残置、同じ `ContentId` を持つ新 clip
    /// を `to_lane` に追加 (linked: curve を共有)。target が source と
    /// 同じ lane でも問題なく動く。
    pub(crate) fn clone_automation_clips_linked(&mut self, deltas: &[MoveAutomationClipEntry]) {
        if deltas.is_empty() {
            return;
        }
        self.edit_song(|song| {
            for d in deltas {
                let template = {
                    let Some(source_lane) =
                        song.automation_lane_by_key(d.from.track, d.from.lane)
                    else {
                        continue;
                    };
                    let Some(source_clip) = source_lane.clip_by_id(d.from.clip) else {
                        continue;
                    };
                    (
                        source_clip.content_id,
                        source_clip.name.clone(),
                        source_clip.length_beats,
                    )
                };
                let Some(target_lane) =
                    song.automation_lane_by_key_mut(d.to_lane.track, d.to_lane.lane)
                else {
                    continue;
                };
                let new_id = target_lane.alloc_clip_id();
                let new_clip = common::model::AutomationClip {
                    id: new_id,
                    name: template.1,
                    start_beat: d.next_start_beat,
                    length_beats: template.2,
                    content_id: template.0,
                };
                let start = new_clip.start_beat;
                let pos = target_lane
                    .clips
                    .partition_point(|c| c.start_beat < start);
                target_lane.clips.insert(pos, new_clip);
            }
        });
    }

    /// Ctrl+Shift+drag release。source は残置、content を deep clone (新
    /// `ContentId` 採番) して独立 clip を追加。共有グループには入らない。
    pub(crate) fn clone_automation_clips_independent(
        &mut self,
        deltas: &[MoveAutomationClipEntry],
    ) {
        if deltas.is_empty() {
            return;
        }
        self.edit_song(|song| {
            for d in deltas {
                let template = {
                    let Some(source_lane) =
                        song.automation_lane_by_key(d.from.track, d.from.lane)
                    else {
                        continue;
                    };
                    let Some(source_clip) = source_lane.clip_by_id(d.from.clip) else {
                        continue;
                    };
                    (
                        source_clip.content_id,
                        source_clip.name.clone(),
                        source_clip.length_beats,
                    )
                };
                // Content を deep clone (`ClipContent` enum 全体の clone なので
                // Midi/Audio/Automation いずれも対応)。content が無い場合は空
                // Automation で作成。
                let cloned_content = song
                    .clip_contents
                    .get(&template.0)
                    .cloned()
                    .unwrap_or_else(|| {
                        common::model::ClipContent::Automation(
                            common::model::AutomationContent::default(),
                        )
                    });
                let new_content_id = song.alloc_content_id();
                song.clip_contents.insert(new_content_id, cloned_content);
                let Some(target_lane) =
                    song.automation_lane_by_key_mut(d.to_lane.track, d.to_lane.lane)
                else {
                    continue;
                };
                let new_id = target_lane.alloc_clip_id();
                let new_clip = common::model::AutomationClip {
                    id: new_id,
                    name: template.1,
                    start_beat: d.next_start_beat,
                    length_beats: template.2,
                    content_id: new_content_id,
                };
                let start = new_clip.start_beat;
                let pos = target_lane
                    .clips
                    .partition_point(|c| c.start_beat < start);
                target_lane.clips.insert(pos, new_clip);
            }
        });
    }

    /// 選択 automation clip 群の bounding span (= MIDI `clip_block_span` の lane
    /// 版)。 解決できない stale key は無視、 有効 clip が無ければ `None`。
    pub(crate) fn automation_block_span(&self, sources: &[common::model::AutomationClipKey]) -> Option<f64> {
        let mut min_start = f64::MAX;
        let mut max_end = f64::MIN;
        for &src in sources {
            let Some(clip) = self
                .song_doc.song()
                .automation_lane_by_key(src.track, src.lane)
                .and_then(|lane| lane.clip_by_id(src.clip))
            else {
                continue;
            };
            min_start = min_start.min(clip.start_beat);
            max_end = max_end.max(clip.start_beat + clip.length_beats);
        }
        (max_end >= min_start).then_some(max_end - min_start)
    }

    /// `source` の共有コピーを `new_start_beat` に 1 つ生成し新 key を返す
    /// (選択・sync は呼び出し側)。 `content_id` を流用し linked group に追加。
    pub(crate) fn duplicate_one_automation_clip_shared_at(
        &mut self,
        source: common::model::AutomationClipKey,
        new_start_beat: f64,
    ) -> Option<common::model::AutomationClipKey> {
        let (content_id, name, length) = {
            let lane = self.song_doc.song().automation_lane_by_key(source.track, source.lane)?;
            let src_clip = lane.clip_by_id(source.clip)?;
            (src_clip.content_id, src_clip.name.clone(), src_clip.length_beats)
        };
        self.edit_song(move |song| {
            let lane = song.automation_lane_by_key_mut(source.track, source.lane)?;
            let new_id = lane.alloc_clip_id();
            let new_clip = common::model::AutomationClip {
                id: new_id,
                name,
                start_beat: new_start_beat,
                length_beats: length,
                content_id,
            };
            let pos = lane.clips.partition_point(|c| c.start_beat < new_start_beat);
            lane.clips.insert(pos, new_clip);
            Some(common::model::AutomationClipKey {
                track: source.track,
                lane: source.lane,
                clip: new_id,
            })
        })?
    }

    /// `source` の独立コピー (content deep clone + 新 ContentId) を
    /// `new_start_beat` に生成し新 key を返す。
    pub(crate) fn duplicate_one_automation_clip_unique_at(
        &mut self,
        source: common::model::AutomationClipKey,
        new_start_beat: f64,
    ) -> Option<common::model::AutomationClipKey> {
        let (src_content_id, name, length) = {
            let lane = self.song_doc.song().automation_lane_by_key(source.track, source.lane)?;
            let src_clip = lane.clip_by_id(source.clip)?;
            (src_clip.content_id, src_clip.name.clone(), src_clip.length_beats)
        };
        let cloned_content = self
            .song_doc.song()
            .clip_contents
            .get(&src_content_id)
            .cloned()
            .unwrap_or_else(|| {
                common::model::ClipContent::Automation(
                    common::model::AutomationContent::default(),
                )
            });
        self.edit_song(move |song| {
            let new_content_id = song.alloc_content_id();
            song.clip_contents.insert(new_content_id, cloned_content);
            let lane = song.automation_lane_by_key_mut(source.track, source.lane)?;
            let new_id = lane.alloc_clip_id();
            let new_clip = common::model::AutomationClip {
                id: new_id,
                name,
                start_beat: new_start_beat,
                length_beats: length,
                content_id: new_content_id,
            };
            let pos = lane.clips.partition_point(|c| c.start_beat < new_start_beat);
            lane.clips.insert(pos, new_clip);
            Some(common::model::AutomationClipKey {
                track: source.track,
                lane: source.lane,
                clip: new_id,
            })
        })?
    }

    /// 選択 automation clip 群をまとめて共有複製 (D shortcut)。 選択
    /// ブロック span だけ後ろにずらして複製し、 複製群を選択にする (連打で後方連鎖)。
    pub(crate) fn duplicate_automation_clips_shared(&mut self, sources: &[common::model::AutomationClipKey]) {
        let Some(offset) = self.automation_block_span(sources) else {
            return;
        };
        let mut new_keys = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song_doc.song()
                .automation_lane_by_key(src.track, src.lane)
                .and_then(|lane| lane.clip_by_id(src.clip))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(k) = self.duplicate_one_automation_clip_shared_at(src, new_start) {
                new_keys.push(k);
            }
        }
        if !new_keys.is_empty() {
            self.selection.selected_automation_clips = new_keys;
            self.selection.last_edit_select = Some(EditSelectSurface::AutomationClips);
        }
    }

    /// 選択 automation clip 群をまとめて独立複製 (Alt+D shortcut)。
    pub(crate) fn duplicate_automation_clips_unique(&mut self, sources: &[common::model::AutomationClipKey]) {
        let Some(offset) = self.automation_block_span(sources) else {
            return;
        };
        let mut new_keys = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song_doc.song()
                .automation_lane_by_key(src.track, src.lane)
                .and_then(|lane| lane.clip_by_id(src.clip))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(k) = self.duplicate_one_automation_clip_unique_at(src, new_start) {
                new_keys.push(k);
            }
        }
        if !new_keys.is_empty() {
            self.selection.selected_automation_clips = new_keys;
            self.selection.last_edit_select = Some(EditSelectSurface::AutomationClips);
        }
    }

    pub(crate) fn resize_automation_clips(&mut self, deltas: &[ResizeAutomationClipEntry]) {
        if deltas.is_empty() {
            return;
        }
        self.edit_song(|song| {
            for d in deltas {
                let Some(lane) = song.automation_lane_by_key_mut(d.key.track, d.key.lane)
                else {
                    continue;
                };
                if let Some(clip) = lane.clip_by_id_mut(d.key.clip) {
                    clip.start_beat = d.next_start;
                    clip.length_beats = d.next_len;
                }
            }
        });
    }

    /// `refcount >= 2` の共有 automation clip を独立化 (content + 共有名を
    /// `fork_content` で deep clone、当該 clip だけ新 `ContentId` を指す)。
    ///
    /// r.md #14 の sibling: 右クリックした clip が現在の automation clip 選択に
    /// 含まれるなら **選択した全 automation clip** を対象にする (含まれないなら
    /// 単体)。 各 clip を per-clip fork するので選択内で linked だった clip も全て
    /// 独立になる。 1 回の `edit_song` で 1 undo step、 既に全て独立なら
    /// `edit_song_checked` の no-op 検出で dirty 化しない。
    pub(crate) fn make_automation_clip_unique(&mut self, key: common::model::AutomationClipKey) {
        let selected = self.selection.selected_automation_clips.clone();
        let targets = if selected.contains(&key) {
            selected
        } else {
            vec![key]
        };
        // status message 用: 編集前に「独立化される (= 共有中の)」clip 数を数える
        // (逐次 fork 回数だと共有群の最後の 1 つを取りこぼす。 clips.rs と同方針)。
        let made_unique = targets
            .iter()
            .filter(|k| {
                self.song_doc
                    .song()
                    .automation_lane_by_key(k.track, k.lane)
                    .and_then(|lane| lane.clip_by_id(k.clip))
                    .is_some_and(|c| self.song_doc.song().clip_content_refcount(c.content_id) >= 2)
            })
            .count();
        self.edit_song_checked(|song| {
            let mut changed = false;
            for k in &targets {
                let content_id = {
                    let Some(lane) = song.automation_lane_by_key(k.track, k.lane) else {
                        continue;
                    };
                    let Some(clip) = lane.clip_by_id(k.clip) else {
                        continue;
                    };
                    clip.content_id
                };
                // 他 clip と共有していなければ既に独立 → fork 不要。
                if song.clip_content_refcount(content_id) <= 1 {
                    continue;
                }
                let new_content_id = song.fork_content(content_id);
                if let Some(lane) = song.automation_lane_by_key_mut(k.track, k.lane)
                    && let Some(clip) = lane.clip_by_id_mut(k.clip)
                {
                    clip.content_id = new_content_id;
                    changed = true;
                }
            }
            changed
        });
        self.ui_ephemeral.status_message = match made_unique {
            0 => "すでに独立 clip です".into(),
            1 => "Clip を独立化しました".into(),
            n => format!("{n} 個のクリップを独立化しました"),
        };
    }

    /// gui_01 #029 (M14 Phase 63n-4): lane body 空き領域 dblclick で
    /// automation clip を新規作成。`docs/plan_automation.md` §5.5。
    /// 初期 `points` は **空** (= `lane.default_value` 引きずり)、
    /// user が dblclick で point を追加していく Bitwig 流。
    pub(crate) fn create_automation_clip(
        &mut self,
        lane_key: common::model::AutomationLaneKey,
        start_beat: f64,
        len_beats: f64,
    ) {
        // 新 ContentId を先に採番 + 空 Automation content を登録。
        let Some(new_content_id) = self.edit_song(|song| {
            let new_content_id = song.alloc_content_id();
            song.clip_contents.insert(
                new_content_id,
                common::model::ClipContent::Automation(
                    common::model::AutomationContent::default(),
                ),
            );
            new_content_id
        }) else {
            return;
        };
        // B6 (r.md #8): clip 名に実 param 名を使うため、 mut borrow を取る前に
        // immutable borrow で target を取り出し track-aware label を解決する。
        let Some(target) = self
            .song_doc.song()
            .automation_lane_by_key(lane_key.track, lane_key.lane)
            .map(|l| l.target.clone())
        else {
            return;
        };
        let display = self.automation_target_label(lane_key.track, &target);
        self.edit_song_checked(|song| {
            let Some(lane) = song
                .automation_lane_by_key_mut(lane_key.track, lane_key.lane)
            else {
                return false;
            };
            let clip_id = lane.alloc_clip_id();
            let new_clip = common::model::AutomationClip {
                id: clip_id,
                name: format!("{display} curve"),
                start_beat,
                length_beats: len_beats,
                content_id: new_content_id,
            };
            let pos = lane.clips.partition_point(|c| c.start_beat < start_beat);
            lane.clips.insert(pos, new_clip);
            true
        });
    }

}
