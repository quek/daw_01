//! handler::device_mix — device の Mix (原音との混ぜ具合) と Δ (差分試聴)。r.md #151。
//!
//! Mix は「作った中身」なので Song に書く (`Device::set_mix`、undo / 保存の対象。値のみ IPC)。
//! Δ は **聴き方の都合** なので Song に書かない (`ProjectEphemeral.delta_devices`、SC Listen と同じ扱い:
//! 保存も undo もされず、書き出しにも入らない)。

use common::model::MAX_DELTA_DEVICES;
use common::protocol::AudioCommand;

use crate::state::AppData;

impl AppData {
    /// `DeviceEvent::SetDeviceMix`。Mix を持たない device ([`common::model::Device::has_mix`]) には効かない。
    pub(crate) fn set_device_mix(&mut self, device_id: u64, mix: f32) {
        let changed = self.edit_song_checked(move |song| {
            song.device_by_id_mut(device_id).is_some_and(|d| d.has_mix() && d.set_mix(mix))
        });
        if changed && let Some(d) = self.cur.song_doc.song().device_by_id(device_id) {
            let mix = d.mix();
            self.send_audio(AudioCommand::SetDeviceMix { project: self.pk(), device_id, mix });
        }
    }

    /// `DeviceEvent::ToggleDeviceDelta`。複数 device で同時に ON にできる ([`MAX_DELTA_DEVICES`] まで。
    /// 超えるぶんは受けない)。Mix を持たない / 居ない device は受けない。
    pub(crate) fn toggle_device_delta(&mut self, device_id: u64) {
        let mut ids = self.cur.peph.delta_devices.clone();
        if let Some(i) = ids.iter().position(|&id| id == device_id) {
            ids.remove(i);
        } else {
            let has_mix = self.cur.song_doc.song().device_by_id(device_id).is_some_and(common::model::Device::has_mix);
            if !has_mix || ids.len() >= MAX_DELTA_DEVICES {
                return;
            }
            ids.push(device_id);
        }
        self.set_delta_devices(ids);
    }

    /// `peph.delta_devices` の書き込みと `SetDeltaDevices` の送信の唯一の口。
    /// respawn 後の再送 (`restore_tabs_after_respawn`) も同じ値でここを通す。
    pub(crate) fn set_delta_devices(&mut self, device_ids: Vec<u64>) {
        self.cur.peph.delta_devices.clone_from(&device_ids);
        self.send_audio(AudioCommand::SetDeltaDevices { project: self.pk(), device_ids });
    }

    /// Song に居なくなった device の Δ を解除する (Song を変える全ての口の後始末 `reconcile_song_refs` から)。
    pub(crate) fn prune_delta_devices(&mut self) {
        let song = self.cur.song_doc.song();
        let kept: Vec<u64> =
            self.cur.peph.delta_devices.iter().copied().filter(|&id| song.device_by_id(id).is_some()).collect();
        if kept.len() != self.cur.peph.delta_devices.len() {
            self.set_delta_devices(kept);
        }
    }
}
