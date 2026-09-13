//! handler::sc_listen — SC Listen (`docs/plan_rack_native_devices.md` §10.14、K25)。
//!
//! Listen は **聴き方の都合** なので Song に書かない (`ProjectEphemeral.sc_listen_device`、
//! Option 1 個なので「プロジェクト内で同時に 1 つ」が型で保証される)。Song を編集するのは
//! bypass 中の Comp を押したときの有効化だけ (undo 1 step「デバイスを有効化」)。

use common::model::NativeKind;
use common::protocol::AudioCommand;

use crate::state::AppData;

impl AppData {
    /// `DeviceEvent::SetScListen`。`Some(id)` は Comp のときだけ受け、bypass 中なら先に有効化する。
    pub(crate) fn request_sc_listen(&mut self, device_id: Option<u64>) {
        let Some(id) = device_id else {
            self.set_sc_listen(None);
            return;
        };
        let Some(dev) = self.cur.song_doc.song().native_by_id(id) else {
            return;
        };
        if dev.kind() != NativeKind::Comp {
            return;
        }
        if dev.bypassed {
            self.set_devices_bypassed(&[id], false);
        }
        self.set_sc_listen(Some(id));
    }

    /// `peph.sc_listen_device` の書き込みと `SetScListen` の送信の唯一の口 (自動 ON はしない)。
    /// respawn 後の再送 (`restore_tabs_after_respawn`) も同じ値でここを通す。
    pub(crate) fn set_sc_listen(&mut self, device_id: Option<u64>) {
        self.cur.peph.sc_listen_device = device_id;
        self.send_audio(AudioCommand::SetScListen { project: self.pk(), device_id });
    }

    /// Song に居なくなった device を Listen していたら解除する (削除 / 切り取り / Parallel 解除 /
    /// トラック削除 / undo-redo の後始末 `prune_device_session_refs` から)。
    pub(crate) fn prune_sc_listen(&mut self) {
        if let Some(id) = self.cur.peph.sc_listen_device
            && self.cur.song_doc.song().native_by_id(id).is_none()
        {
            self.set_sc_listen(None);
        }
    }
}
