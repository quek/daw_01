//! handler::native_edit — 内蔵 device (`Device::Native`) と master Limiter の値編集・追加
//! (`docs/plan_rack_native_devices.md` §10.2)。
//!
//! 値の変化を engine へ運ぶ **値 IPC の唯一の口** は [`AppData::send_native_value`] と
//! `SetMasterLimiter` の送信 1 か所。自動 ON の規則は `NativeEdit::apply` が持ち、ここは
//! Song 編集 → 値 IPC → last touched の順に繋ぐだけ。

use common::model::{
    AutomationTarget, ChainRef, Device, MASTER_TRACK_ID, NativeDevice, NativeKind, NativeParamId,
    RackPanelKey,
};
use common::protocol::AudioCommand;

use crate::event_native::{MasterLimiterEdit, NativeEdit};
use crate::state::AppData;

impl AppData {
    /// 組み込み・追加分共通の値編集 (`DeviceEvent::NativeEdit`)。
    pub(crate) fn apply_native_edit(&mut self, device_id: u64, edit: &NativeEdit) {
        let Some(owner) = self.cur.song_doc.song().device_owner_track(device_id) else {
            return;
        };
        let changed = self.edit_song_checked(|song| {
            song.native_by_id_mut(device_id).is_some_and(|d| edit.apply(d))
        });
        if changed {
            self.send_native_value(device_id);
        }
        // A キー / MIDI Learn の的は「On 以外で最後に触った param」。
        if let NativeEdit::Params(params) = edit
            && let Some(&(param, _)) = params.iter().rev().find(|(p, _)| !matches!(p, NativeParamId::On(_)))
        {
            self.note_touched_target(AutomationTarget::NativeParam { device_id, param }, owner);
        }
    }

    /// master のフェーダー後 Limiter (`DeviceEvent::MasterLimiterEdit`)。先読み遅延の有無が
    /// 変わる On の切り替えも、同じフレームの LoadSong が焼くので値 IPC は On / Ceiling 共通。
    pub(crate) fn apply_master_limiter_edit(&mut self, edit: MasterLimiterEdit) {
        let changed = self.edit_song_checked(|song| edit.apply(&mut song.master_limiter));
        if changed {
            let limiter = self.cur.song_doc.song().master_limiter;
            self.send_audio(AudioCommand::SetMasterLimiter { project: self.pk(), limiter });
        }
        self.note_touched_target(AutomationTarget::MasterLimiter(edit.param()), MASTER_TRACK_ID);
    }

    /// `SetNativeDevice` を送る唯一の口 (値だけ。構造と配線は LoadSong が運ぶ)。
    pub(crate) fn send_native_value(&self, device_id: u64) {
        let Some(dev) = self.cur.song_doc.song().native_by_id(device_id) else {
            return;
        };
        self.send_audio(AudioCommand::SetNativeDevice {
            project: self.pk(),
            device_id,
            bypassed: dev.bypassed,
            params: dev.params,
        });
    }

    /// picker の内蔵 4 種 (`DeviceEvent::AddNative`)。chain の既定位置 (Q6) へ追加分として挿す。
    /// 位置・id・番号はすべて closure の中で **実行時の Song** から求める。挿す chain は呼び出し側が決める
    /// (picker は `select_plugin_from_db` が、トラックが無ければ足してから渡す)。無い chain なら何もしない。
    pub(crate) fn add_native(&mut self, chain: ChainRef, kind: NativeKind, open_panel: bool) {
        let mut created = None;
        self.edit_song_checked(|song| {
            let Some(owner) = song.chain_owner_track(chain) else {
                return false;
            };
            let Some(at) = song.default_insert_index(chain) else {
                return false;
            };
            let id = song.alloc_device_id();
            let ordinal = song.next_native_ordinal(owner, kind);
            let inserted = song.insert_device(chain, at, Device::Native(NativeDevice::new_added(kind, id, ordinal)));
            if inserted {
                created = Some(id);
            }
            inserted
        });
        let Some(id) = created else {
            return;
        };
        if open_panel {
            self.cur.view.open_rack_panels.insert(RackPanelKey::Device(id));
        }
        self.set_device_selection(vec![id]);
        self.cur.selection.device_anchor = Some(id);
    }
}
