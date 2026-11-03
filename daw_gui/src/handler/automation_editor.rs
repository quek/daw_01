//! handler::automation_editor — Automation Editor (下部パネルの Piano Roll タブを置き換える
//! オートメーションクリップの編集面) の開閉と表示状態。
//!
//! 点の編集そのものはレーンと同じ AppEvent (`AddAutomationPoint` 等) を通る。ハンドラは
//! `AutomationLane::clip_by_id` でアレンジのクリップとセルの両方を引くので、ここには無い。
//!
//! 対象は安定 id (`AutomationClipKey`) で持ち、クリップが消えたときの貼り替えはしない。
//! 読む側が [`AppData::automation_editor_target`] で実在を確かめる。

use common::model::{AudioEditorViewState, AutomationClipKey};

use crate::app_types::{AutomationPointKeyRef, EditSurface, MIN_AUDIO_EDITOR_VIEW_LEN_BEATS};
use crate::state::AppData;

/// 保存された表示状態をクリップの長さに収めた `(開始拍, 長さ)`。どちらも窓ローカル拍。
/// 表示状態が無ければ全体表示。
#[must_use]
pub fn clamp_view(view: Option<AudioEditorViewState>, clip_len: f64) -> (f64, f64) {
    let total = clip_len.max(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS);
    let Some(v) = view else {
        return (0.0, total);
    };
    let len = v.len_beats.clamp(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total);
    (v.start_beat.clamp(0.0, total - len), len)
}

impl AppData {
    /// いま開いている Automation Editor の対象。クリップが消えていれば `None`
    /// (トラック削除 / undo / セル削除のどの経路でも、読む側で閉じたことになる)。
    #[must_use]
    pub fn automation_editor_target(&self) -> Option<AutomationClipKey> {
        let key = self.cur.peph.automation_editor_clip?;
        self.cur.song_doc.song().automation_lane_by_key(key.track, key.lane)?.clip_by_id(key.clip)?;
        Some(key)
    }

    /// Automation Editor を開く。Audio Editor は閉じる (同じタブを取り合うので排他)。
    pub fn open_automation_editor(&mut self, key: AutomationClipKey) {
        let exists = self
            .cur.song_doc.song()
            .automation_lane_by_key(key.track, key.lane)
            .is_some_and(|l| l.clip_by_id(key.clip).is_some());
        if !exists {
            return;
        }
        if self.cur.peph.audio_editor_clip.is_some() {
            self.close_audio_editor();
        }
        self.cur.view.bottom_panel = Some(1);
        if self.cur.peph.automation_editor_clip != Some(key) {
            self.cur.peph.automation_editor_clip = Some(key);
            self.drop_undisplayed_automation_points();
        }
    }

    /// Automation Editor を閉じる。表示状態は `automation_editor_views` に残す。
    pub fn close_automation_editor(&mut self) {
        if self.cur.peph.automation_editor_clip.take().is_none() {
            return;
        }
        self.cur.peph.automation_editor_hover_beat = None;
        self.drop_undisplayed_automation_points();
    }

    /// この点はいまどこかに描かれているか: アレンジのクリップならレーンに、それ以外
    /// (セル) は Automation Editor がタブに見えているときだけ。実在しない点は `false`。
    #[must_use]
    pub fn automation_point_is_displayed(&self, k: AutomationPointKeyRef) -> bool {
        let in_arrangement = self
            .cur.song_doc.song()
            .automation_lane_by_key(k.track_id, k.lane_id)
            .is_some_and(|l| l.clip_index_by_id(k.clip_id).is_some());
        in_arrangement || self.automation_editor_owns_point(k)
    }

    /// 選択中の点が全部見えているか。[`Self::edit_surface`] の「点の面が生きているか」で、
    /// 見えない点 (タブを隠したセル / 消えたクリップ) を Delete / Cut が消さないための門番。
    #[must_use]
    pub fn selected_automation_points_displayed(&self) -> bool {
        let points = &self.cur.selection.selected_automation_points;
        !points.is_empty() && points.iter().all(|k| self.automation_point_is_displayed(*k))
    }

    /// どこにも描かれなくなった点を選択から外す。アレンジのクリップの点はレーンに見えて
    /// いるので残る (セルの点だけが落ちる)。
    fn drop_undisplayed_automation_points(&mut self) {
        let mut points = std::mem::take(&mut self.cur.selection.selected_automation_points);
        points.retain(|k| self.automation_point_is_displayed(*k));
        self.cur.selection.selected_automation_points = points;
        if self.cur.selection.selected_automation_points.is_empty()
            && self.cur.selection.last_edit_select == Some(EditSurface::AutomationPoints)
        {
            self.cur.selection.last_edit_select = None;
        }
        // 数値入力中の点も同じ (描かれなくなれば入力欄を出す場所が無い)。
        if let Some(k) = self.cur.peph.editing_automation_point
            && !self.automation_point_is_displayed(k)
        {
            self.cur.peph.editing_automation_point = None;
        }
    }

    /// 下部パネルのタブ 1 を素のピアノロールにする (編集面を閉じてタブを選ぶ)。
    pub fn show_piano_roll(&mut self) {
        if self.cur.peph.audio_editor_clip.is_some() {
            self.close_audio_editor();
        }
        self.close_automation_editor();
        self.cur.view.bottom_panel = Some(1);
    }

    /// 開いているクリップの `(開始拍, 長さ, クリップ長)`。
    #[must_use]
    pub fn automation_editor_view(&self, key: AutomationClipKey) -> Option<(f64, f64, f64)> {
        let clip_len = self
            .cur.song_doc.song()
            .automation_lane_by_key(key.track, key.lane)?
            .clip_by_id(key.clip)?
            .length_beats;
        let (start, len) = clamp_view(self.cur.view.automation_editor_views.get(&key).copied(), clip_len);
        Some((start, len, clip_len))
    }

    pub fn set_automation_editor_view(&mut self, key: AutomationClipKey, start_beat: f64, len_beats: f64) {
        self.cur.view.automation_editor_views.insert(key, AudioEditorViewState { start_beat, len_beats });
    }

    /// 全体表示へ戻す (`daw.fit_view`)。
    pub fn fit_automation_editor(&mut self) {
        if let Some(key) = self.automation_editor_target() {
            self.cur.view.automation_editor_views.remove(&key);
        }
    }

    /// 開いているクリップの点を全部選ぶ (`Ctrl+A`)。窓の外の点は見えないので選ばない
    /// (選ぶと、ドラッグで窓の端へ潰れ、Delete で黙って消える)。
    pub fn select_all_automation_editor_points(&mut self) {
        let Some(key) = self.automation_editor_target() else {
            return;
        };
        let song = self.cur.song_doc.song();
        let clip = song
            .automation_lane_by_key(key.track, key.lane)
            .and_then(|l| l.clip_by_id(key.clip));
        let points = clip
            .and_then(|c| song.clip_contents.get(&c.content_id))
            .and_then(common::model::ClipContent::automation_points)
            .unwrap_or(&[]);
        let (from, to) = clip.map_or((0.0, 0.0), common::model::AutomationClip::content_window);
        #[allow(clippy::cast_possible_truncation)]
        let next: Vec<AutomationPointKeyRef> = points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.time_beat >= from && p.time_beat <= to)
            .map(|(i, _)| AutomationPointKeyRef {
                track_id: key.track,
                lane_id: key.lane,
                clip_id: key.clip,
                point_idx: i as u32,
            })
            .collect();
        self.handle_event(crate::event::AppEvent::SelectAutomationPoints {
            prev: self.cur.selection.selected_automation_points.clone(),
            next,
        });
    }

    /// 開いているクリップの再生位置 (窓ローカル拍にする前の、クリップの座標系の拍)。
    /// アレンジのクリップは song の拍。セルはセル内の位相で、鳴っていなければ `None`
    /// ([`Self::editor_playhead_beat`] のレーン行版)。
    #[must_use]
    pub fn automation_editor_playhead_beat(&self, key: AutomationClipKey) -> Option<f64> {
        let lane = self.cur.song_doc.song().automation_lane_by_key(key.track, key.lane)?;
        let Some(cell) = lane.session_clips.iter().find(|c| c.clip.id == key.clip) else {
            return self.cur.transport.playhead_beat.map(f64::from);
        };
        let snap = self.launcher_running_row(key.track, key.lane)?;
        if snap.state != common::audio_bridge::LAUNCHER_STATE_PLAYING || snap.playing_clip_id != key.clip {
            return None;
        }
        let playhead = f64::from(self.cur.transport.playhead_beat?);
        let phase = crate::launcher_time::cell_phase(
            snap.launch_beat,
            playhead,
            cell.clip.length_beats,
            cell.launch.looping,
        )?;
        Some(cell.clip.start_beat + phase)
    }

    /// 開いているクリップがセルか (ruler の seek / `f` の振り分けに使う)。
    #[must_use]
    pub fn automation_editor_is_cell(&self, key: AutomationClipKey) -> bool {
        self.cur
            .song_doc
            .song()
            .automation_lane_by_key(key.track, key.lane)
            .is_some_and(|l| l.session_clips.iter().any(|c| c.clip.id == key.clip))
    }

    /// この点の数値入力欄を Automation Editor が出すか (下部パネルに見えているときだけ)。
    /// そうでなければレーン側 (`arrangement_view`) が出す。
    #[must_use]
    pub fn automation_editor_owns_point(&self, k: AutomationPointKeyRef) -> bool {
        self.cur.view.bottom_panel == Some(1)
            && self.automation_editor_target()
                == Some(AutomationClipKey { track: k.track_id, lane: k.lane_id, clip: k.clip_id })
    }

    /// ポインタ下のクリップと拍 (窓ローカル)。Automation Editor の本体の上にいるときだけ `Some`。
    #[must_use]
    pub fn automation_editor_hover(&self) -> Option<(AutomationClipKey, f64)> {
        if self.cur.view.bottom_panel != Some(1) {
            return None;
        }
        Some((self.automation_editor_target()?, self.cur.peph.automation_editor_hover_beat?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 表示状態が無ければ全体表示() {
        assert_eq!(clamp_view(None, 8.0), (0.0, 8.0));
    }

    /// クリップが縮んでも、表示がクリップの外へ飛び出さない。
    #[test]
    fn 表示はクリップの長さに収まる() {
        let v = |start_beat, len_beats| Some(AudioEditorViewState { start_beat, len_beats });
        assert_eq!(clamp_view(v(2.0, 4.0), 8.0), (2.0, 4.0));
        assert_eq!(clamp_view(v(6.0, 4.0), 8.0), (4.0, 4.0), "右端で止まる");
        assert_eq!(clamp_view(v(-3.0, 4.0), 8.0), (0.0, 4.0), "左端で止まる");
        assert_eq!(clamp_view(v(1.0, 32.0), 8.0), (0.0, 8.0), "クリップより長くは見せない");
    }
}
