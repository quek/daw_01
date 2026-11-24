//! handler::plugin_dirty — r.md #160: plugin の中だけが変わったプロジェクトを「未保存」として扱う。
//!
//! plugin の state は Song の外 (plugin の中) にあり、保存のときに `RequestAllStates` の往復で取り直す。
//! だから plugin の窓の中だけを触った変更は Song の dirty (`state_id`) に現れず、そのまま閉じると確認なしで
//! 失われていた。ここが 2 段で拾う:
//!
//! - **A. 報告された変更** — plugin host がユーザーの変更と判定した報告 (`PluginEvent::PluginStateChanged`。
//!   判定の正本は `daw_plugin_host::user_edit`) で [`SongDoc::note_plugin_edit`] を進める。`*`・保存確認・
//!   OS のセッション終了のブロック・autosave は全部 [`SongDoc::is_dirty`] を読むので、それだけで揃う。
//!   保存が降ろすのは **`RequestAllStates` を送った時点まで** (`ProjectIpc::state_request_plugin_edits`) —
//!   往復の間に来た報告は応答の state に入っている保証が無いので降ろさない。
//! - **B. 報告しない変更** — 閉じる / 終了の前に state を取り直し、基準 ([`PluginStateBaseline`]) と比べる。
//!   違えば A と同じ印を立てて、いつもの保存確認に合流する。基準は「保存した blob」ではなく
//!   **plugin が返した state** の hash (state を渡して取り直すと bytes が変わる plugin で、開くたびに
//!   聞かれないように)。取るたびに中身が変わる plugin は毎回聞かれる (失うより聞く)。
//!
//! autosave は live の Song を書くので、報告された変更が Song の blob にまだ入っていなければ、先に取り直す
//! ([`AppData::autosave_needs_plugin_state`])。

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use common::model::{AutomationTarget, Song};
use common::protocol::SlotState;

use crate::app_types::{DirtyGuardAction, PendingStateRequest};
use crate::state::AppData;

/// plugin が返した state の指紋 (`device_id` → hash)。blob そのものは持たない (正本は plugin と Song)。
/// hash はこのプロセスの中でしか比べない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginStateDigest(HashMap<u64, u64>);

impl PluginStateDigest {
    /// 取得に失敗した device (`SlotState::error`) は入れない (比べる相手が無い)。
    pub(crate) fn of(states: &[SlotState]) -> Self {
        let hash = |s: &SlotState| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.data.hash(&mut h);
            s.ara_archive.hash(&mut h);
            h.finish()
        };
        Self(states.iter().filter(|s| s.error.is_none()).map(|s| (s.device_id, hash(s))).collect())
    }

    /// 両方にある device のうち、指紋が違うものがあるか (`skip` の device は比べない)。片方にしか無い device は
    /// 数えない — device の増減は Song の編集で、そちらの dirty が見ている。
    fn differs_from(&self, baseline: &Self, skip: &HashSet<u64>) -> bool {
        self.0.iter().any(|(id, hash)| !skip.contains(id) && baseline.0.get(id).is_some_and(|b| b != hash))
    }

    /// `other` にしか無い device を足す (既にある device の指紋は変えない)。
    fn add_missing(&mut self, other: Self) {
        for (id, hash) in other.0 {
            self.0.entry(id).or_insert(hash);
        }
    }
}

/// 保存が plugin から取り直してファイルに書いた state。
pub(crate) struct SavedPluginState {
    /// その `RequestAllStates` を送った時点の `SongDoc::plugin_edits` (ここまでがファイルに入った)。
    pub through_edits: u64,
    pub digest: PluginStateDigest,
}

/// host が値を書き続ける plugin (param にオートメーションのクリップか変調が掛かっている)。state が再生位置で
/// 動くので、閉じるときの比較から外す — 比べると「再生しただけで保存を聞かれる」。ユーザーが窓で触った分は
/// 報告 (module doc の A) が拾う。
fn host_driven_devices(song: &Song) -> HashSet<u64> {
    let device = |target: &AutomationTarget| match target {
        AutomationTarget::PluginParam { device_id, .. } => Some(*device_id),
        _ => None,
    };
    let stores = std::iter::once((song.song_lanes.as_slice(), song.song_mod_routings.as_slice()))
        .chain(song.tracks.iter().map(|t| (t.automation_lanes.as_slice(), t.mod_routings.as_slice())));
    let mut driven = HashSet::new();
    for (lanes, routings) in stores {
        let automated = lanes.iter().filter(|l| !l.clips.is_empty() || !l.session_clips.is_empty());
        driven.extend(automated.filter_map(|l| device(&l.target)));
        driven.extend(routings.iter().filter_map(|r| device(&r.target)));
    }
    driven
}

/// 閉じるときの比較 (module doc の B) の基準。
#[derive(Debug, Default, PartialEq, Eq)]
pub enum PluginStateBaseline {
    /// 無い (plugin を 1 つも読み込んでいない / 取れなかった)。比べない。
    #[default]
    None,
    /// plugin を読み込んだ。読み込みが全部確定したら取る ([`AppData::request_plugin_state_baseline_if_due`])。
    /// `known` = 既に基準を持っている device の分 (後から足した plugin の分だけを足すために残す)。
    Due { known: PluginStateDigest },
    /// 取り寄せ中 (`PendingStateRequest::Baseline` が待ち行列にある)。
    Requested { known: PluginStateDigest },
    Known(PluginStateDigest),
}

impl PluginStateBaseline {
    fn take_known(&mut self) -> PluginStateDigest {
        match std::mem::take(self) {
            Self::None => PluginStateDigest::default(),
            Self::Due { known } | Self::Requested { known } | Self::Known(known) => known,
        }
    }
}

impl AppData {
    // ---- A. 報告された変更 ----------------------------------------------------

    /// `PluginEvent::PluginStateChanged`: ユーザーが plugin の窓の中で中身を変えた。削除済みの device 宛て
    /// (遅れて届いた報告) は捨てる。
    pub(crate) fn on_plugin_state_changed(&mut self, device_id: u64) {
        if self.cur.song_doc.plugin_by_id(device_id).is_some() {
            self.cur.song_doc.note_plugin_edit();
        }
    }

    // ---- B. 基準 --------------------------------------------------------------

    /// plugin を host に載せた (`SlotPluginLoaded`)。その device の基準はまだ無い。
    pub(crate) fn mark_plugin_state_baseline_due(&mut self) {
        let baseline = &mut self.cur.pipc.plugin_state_baseline;
        if !matches!(baseline, PluginStateBaseline::Requested { .. }) {
            *baseline = PluginStateBaseline::Due { known: baseline.take_known() };
        }
    }

    /// 基準を取る段になっていれば取り寄せる。読み込みが全部確定してから (`handle_event` の一番外側の終わりで
    /// `resume_after_plugin_loads` と並べて呼ぶ)。書き出し / 解析の間は応答が捨てられるので、終わるまで待つ。
    pub(crate) fn request_plugin_state_baseline_if_due(&mut self) {
        if !matches!(self.cur.pipc.plugin_state_baseline, PluginStateBaseline::Due { .. })
            || self.shutdown.is_shutting_down()
            || self.plugin_loads_pending()
            || self.export_or_analysis_busy()
            || self.ipc.plugin_tx.is_none()
        {
            return;
        }
        let known = self.cur.pipc.plugin_state_baseline.take_known();
        if !self.song_has_plugin() {
            // 読み込んだ plugin を全部消した後。
            self.cur.pipc.plugin_state_baseline = PluginStateBaseline::Known(known);
            return;
        }
        self.cur.pipc.plugin_state_baseline = PluginStateBaseline::Requested { known };
        self.enqueue_state_request(PendingStateRequest::Baseline);
    }

    /// `PendingStateRequest::Baseline` の応答。基準の無い device の分だけ足す — 既に基準のある device を
    /// 上書きすると、その間の報告の無い変更が基準に溶けて見えなくなる。
    pub(crate) fn finish_plugin_state_baseline(&mut self, states: &[SlotState]) {
        let mut known = self.cur.pipc.plugin_state_baseline.take_known();
        known.add_missing(PluginStateDigest::of(states));
        self.cur.pipc.plugin_state_baseline = PluginStateBaseline::Known(known);
    }

    /// 保存が plugin から取った state をファイルに書いた。以後はそれが基準。
    pub(crate) fn rebase_plugin_state_on_save(&mut self, saved: PluginStateDigest) {
        self.cur.pipc.plugin_state_baseline = PluginStateBaseline::Known(saved);
        // 終了の途中の保存 (「保存して終了」) の後で、同じ state をもう一度比べない。
        self.cur.pipc.unreported_check_passed = true;
    }

    // ---- B. 閉じるときの比較 --------------------------------------------------

    /// 閉じる / 終了の前に、報告の無い変更を比べる必要があるか。未保存なら確認モーダルがどのみち出るので
    /// 比べない。比べる相手 (基準のある plugin) が無い / host が居ない / 書き出し中 (応答が捨てられる) は
    /// 比べられないので比べない。
    #[must_use]
    pub(crate) fn needs_unreported_check(&self) -> bool {
        if self.cur.pipc.unreported_check_passed
            || self.cur.song_doc.is_dirty()
            || self.ipc.plugin_tx.is_none()
            || self.export_or_analysis_busy()
        {
            return false;
        }
        let PluginStateBaseline::Known(baseline) = &self.cur.pipc.plugin_state_baseline else {
            return false;
        };
        self.cur.song_doc.song().all_plugins().any(|p| baseline.0.contains_key(&p.id))
    }

    /// state を取り直す。`action` は応答 ([`Self::finish_unreported_check`]) の後で
    /// `request_guarded_action` がやり直す (待ち行列が空になったときの既存の再評価に乗る)。
    pub(crate) fn begin_unreported_check(&mut self, action: DirtyGuardAction) {
        self.cur.pipc.guard_pending_action = Some(action);
        self.enqueue_state_request(PendingStateRequest::UnreportedCheck);
    }

    /// `PendingStateRequest::UnreportedCheck` の応答。基準と違えば未保存にする (続く再評価が確認モーダルを出す)。
    /// 同じなら、この閉じる / 終了の間はもう比べない。
    pub(crate) fn finish_unreported_check(&mut self, states: &[SlotState]) {
        let PluginStateBaseline::Known(baseline) = &self.cur.pipc.plugin_state_baseline else {
            return;
        };
        let skip = host_driven_devices(self.cur.song_doc.song());
        if PluginStateDigest::of(states).differs_from(baseline, &skip) {
            tracing::info!("plugin state differs from the saved baseline; treating the project as unsaved");
            self.cur.song_doc.note_plugin_edit();
        } else {
            self.cur.pipc.unreported_check_passed = true;
        }
    }

    /// 閉じる / 終了の一連の確認を新しく始める: 前回の「比べて同じだった」を全タブで忘れる。
    pub(crate) fn reset_unreported_checks(&mut self) {
        self.for_each_tab(|app| app.cur.pipc.unreported_check_passed = false);
    }

    /// state の往復を破棄した (host が応答しない / 居なくなった) ときの、ここが積んだ要求の後始末。
    /// `dropped` = 破棄した待ち行列。
    ///
    /// - 比較 (`UnreportedCheck`): 確かめられなかったので **変わったものとして扱う** (未保存にする)。終了は
    ///   確認モーダルに戻り、「保存せず終了」で抜けられる。黙って閉じて失うより聞く。
    /// - 基準 (`Baseline`): 取り直さない (応答しない host に同じ要求を積み続けない)。足りない分は比べない。
    /// - autosave: 取り直せなかった state のまま書く (何も残さないより、Song の編集だけでも残す)。
    pub(crate) fn settle_aborted_plugin_state_requests(&mut self, dropped: &[PendingStateRequest]) {
        for req in dropped {
            match req {
                PendingStateRequest::UnreportedCheck => self.cur.song_doc.note_plugin_edit(),
                PendingStateRequest::Baseline => {
                    let known = self.cur.pipc.plugin_state_baseline.take_known();
                    self.cur.pipc.plugin_state_baseline = PluginStateBaseline::Known(known);
                }
                PendingStateRequest::Autosave => self.write_autosave(),
                _ => {}
            }
        }
    }

    // ---- autosave -------------------------------------------------------------

    /// autosave の前に plugin の state を取り直す必要があるか: 報告された変更が live の Song の blob に
    /// まだ入っていない。host が居なければ取り直せないので、今ある blob のまま書く。
    #[must_use]
    pub(crate) fn autosave_needs_plugin_state(&self) -> bool {
        self.cur.song_doc.plugin_state_blob_is_stale()
            && self.ipc.plugin_tx.is_some()
            && !self.export_or_analysis_busy()
            && self.song_has_plugin()
    }

    /// 取り直しを積む (既に積んであれば何もしない)。応答で [`Self::write_autosave`] が書く。
    pub(crate) fn request_plugin_state_for_autosave(&mut self) {
        let queued = self.cur.pipc.pending_state_queue.iter().any(|r| matches!(r, PendingStateRequest::Autosave));
        if !queued {
            self.enqueue_state_request(PendingStateRequest::Autosave);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(device_id: u64, data: &[u8]) -> SlotState {
        SlotState { device_id, data: Some(data.to_vec()), ara_archive: None, error: None }
    }

    /// 比べるのは両方にある device だけ。取得に失敗した device は比べない。
    #[test]
    fn digest_compares_only_devices_present_on_both_sides() {
        let none = HashSet::new();
        let baseline = PluginStateDigest::of(&[state(1, b"a"), state(2, b"b")]);
        assert!(!PluginStateDigest::of(&[state(1, b"a"), state(2, b"b")]).differs_from(&baseline, &none));
        let changed = PluginStateDigest::of(&[state(1, b"a"), state(2, b"changed")]);
        assert!(changed.differs_from(&baseline, &none));
        assert!(!changed.differs_from(&baseline, &HashSet::from([2])), "host が値を書き続ける device");
        assert!(!PluginStateDigest::of(&[state(1, b"a"), state(3, b"new")]).differs_from(&baseline, &none), "増えた device");
        let failed = SlotState { error: Some("save failed".into()), data: None, ..state(2, b"") };
        assert!(!PluginStateDigest::of(&[state(1, b"a"), failed]).differs_from(&baseline, &none), "取れなかった device");
    }

    /// 後から足した plugin の基準を取るとき、既にある device の基準は動かさない。
    #[test]
    fn a_later_baseline_does_not_overwrite_known_devices() {
        let mut known = PluginStateDigest::of(&[state(1, b"at load")]);
        known.add_missing(PluginStateDigest::of(&[state(1, b"touched since"), state(2, b"new")]));
        let none = HashSet::new();
        assert!(PluginStateDigest::of(&[state(1, b"touched since")]).differs_from(&known, &none));
        assert!(!PluginStateDigest::of(&[state(2, b"new")]).differs_from(&known, &none));
    }
}
