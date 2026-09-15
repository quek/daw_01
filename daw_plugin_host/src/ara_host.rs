//! plugin-main の ARA document の command (組む / 消す / クリップボードの写し) — document ごとの編集
//! (`ara::session`) と、document をまたぐ状態の置き場 (`ara::states`) をつなぐ。
//!
//! document を組むとき、作る modification の始め方は **ほかの document を見て** 決める (別のトラックへ写した
//! クリップの Melodyne の編集は写した元の document にある)。 ほかの document は読むだけで、編集するのは組む
//! document だけ。

use common::ara_ids::AraArchiveEntry;
use common::protocol::{AraClipSpec, DeviceAddr, ProjectKey};

use crate::PluginHost;
use crate::ara::session::{AraSession, StartTable};

impl PluginHost {
    /// `SetupAraDocument`: `device` の document を `clips` に合わせる。 作る modification の始め方をほかの document と
    /// 取っておいた状態から解決し、destroy した modification の状態を取っておく。 `setup_ara` は内部で
    /// deactivate → activate するので quiesce 契約。
    pub(crate) fn setup_ara_document(
        &mut self,
        device: DeviceAddr,
        clips: &[AraClipSpec],
        bpm: f64,
        time_sig: (u16, u16),
        archive: Option<&[u8]>,
        archive_ids: &[AraArchiveEntry],
    ) {
        let Ok(saved) = self.detach_and_quiesce(device) else { return };
        let published = saved.is_some();
        let starts = self.resolve_ara_starts(device, clips, if archive.is_some() { archive_ids } else { &[] });
        let archive = archive.map(|bytes| crate::ara::SavedArchive { bytes, ids: archive_ids });
        let edit = crate::ara::AraEdit { clips, bpm, time_sig, archive, starts: &starts };
        let Some(rec) = self.instances.get_mut(&device) else {
            tracing::warn!(?device, "SetupAraDocument: no plugin for ?device");
            return;
        };
        if published {
            rec.plugin.stop_processing();
        }
        match rec.plugin.setup_ara(edit) {
            Some(retired) => {
                tracing::info!(?device, n = clips.len(), restored = starts.len(), "ARA document set up");
                if let Some(format) = rec.plugin.ara_session().map(|s| s.archive_format().to_owned()) {
                    self.ara_states.retire(device.project, &format, retired);
                }
            }
            None => tracing::warn!(?device, "SetupAraDocument: plugin is not ARA-capable, ignoring"),
        }
        if published
            && let Some(rec) = self.instances.get_mut(&device)
            && let Err(e) = rec.plugin.start_processing()
        {
            tracing::error!(error = ?e, ?device, "SetupAraDocument: start_processing failed");
        }
        if let Some(entry) = saved {
            self.republish(device, entry);
        }
    }

    /// `ClearAraDocument`: 消す document の状態を取っておいてから session を畳む (quiesce 契約)。
    pub(crate) fn clear_ara_document(&mut self, device: DeviceAddr) {
        let Ok(saved) = self.detach_and_quiesce(device) else { return };
        let published = saved.is_some();
        self.keep_ara_document(device);
        if let Some(rec) = self.instances.get_mut(&device) {
            if published {
                rec.plugin.stop_processing();
            }
            rec.plugin.clear_ara();
            if published && let Err(e) = rec.plugin.start_processing() {
                tracing::error!(error = ?e, ?device, "ClearAraDocument: start_processing failed");
            }
            tracing::info!(?device, "ARA document cleared");
        }
        if let Some(entry) = saved {
            self.republish(device, entry);
        }
    }

    /// 畳む直前の `device` の document の状態を取っておく (ARA session が無ければ何もしない)。
    pub(crate) fn keep_ara_document(&mut self, device: DeviceAddr) {
        if let Some(session) = self.instances.get(&device).and_then(|rec| rec.plugin.ara_session()) {
            self.ara_states.keep_document(device.project, session);
        }
    }

    /// `SnapshotAraClipboard`: クリップボードへ写した modification の今の状態を取っておく。
    pub(crate) fn snapshot_ara_clipboard(&mut self, project: ProjectKey, project_id: u64, modifications: &[String]) {
        let sessions = ara_sessions(&self.instances);
        self.ara_states.snapshot_clipboard(&sessions, project, project_id, modifications);
    }

    /// `device` の document を `clips` に合わせる編集で作る modification の始め方。
    fn resolve_ara_starts(&self, device: DeviceAddr, clips: &[AraClipSpec], saved: &[AraArchiveEntry]) -> StartTable {
        let Some(session) = self.instances.get(&device).and_then(|rec| rec.plugin.ara_session()) else {
            return StartTable::new();
        };
        let sessions = ara_sessions(&self.instances);
        self.ara_states.resolve_starts(device, session, &sessions, clips, saved)
    }
}

/// plug-in host に居る ARA session (device 順 = 引き当ての順序を決める)。
fn ara_sessions(instances: &std::collections::HashMap<DeviceAddr, crate::InstanceRecord>) -> Vec<(DeviceAddr, &AraSession)> {
    let mut sessions: Vec<(DeviceAddr, &AraSession)> =
        instances.iter().filter_map(|(addr, rec)| rec.plugin.ara_session().map(|s| (*addr, s))).collect();
    sessions.sort_unstable_by_key(|(addr, _)| *addr);
    sessions
}
