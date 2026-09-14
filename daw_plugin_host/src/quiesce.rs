//! plugin-main が plugin の audio half に触れる前の、worker との排他 (registry から外す → 処理中の `process()` を
//! 排出する)。契約は `process_server` の module doc と [`crate::plugin_instance`] の `AudioHalf`。
//!
//! 待ちは有界 ([`crate::process_server::WorkerPool::quiesce`])。上限までに `process()` から抜けない plugin には触れず、
//! worker が使っているものを壊さずに手放す。pool を閉じても止まらない worker が残れば、その plugin を別の worker が
//! 並行に呼ばないように plugin_host を終えて daw_gui の respawn に任せる ([`PluginHost::wedged`])。

use common::protocol::{DeviceAddr, InstanceToken, PluginEvent};

use crate::process_server::{PluginEntry, registry_insert, registry_remove};
use crate::{InstanceRecord, PluginHost, editor_keys, editor_window};

impl PluginHost {
    /// registry から `device` の entry を外し、worker の in-flight
    /// dispatch を排出する。戻り値 = 外した entry (republish 用)。
    /// entry が未 publish なら quiesce も不要 (worker は触れない)。
    /// `Err` = その plugin の `process()` が上限までに抜けない。entry は戻してあり、呼び出し側は plugin に触れない。
    pub(crate) fn detach_and_quiesce(&self, device: DeviceAddr) -> Result<Option<PluginEntry>, ()> {
        let Some(token) = self.instances.get(&device).map(|rec| rec.token) else {
            return Ok(None);
        };
        let saved = registry_remove(&self.registry, token);
        if let Some(entry) = saved.as_ref()
            && !self.quiesce(&[token]).is_empty()
        {
            registry_insert(&self.registry, token, entry.clone());
            return Err(());
        }
        Ok(saved)
    }

    /// [`Self::detach_and_quiesce`] で外した entry を同じ instance の token で戻す。
    /// (device が消えていれば捨てる = 戻し先が無い)
    pub(crate) fn republish(&self, device: DeviceAddr, entry: PluginEntry) {
        if let Some(rec) = self.instances.get(&device) {
            registry_insert(&self.registry, rec.token, entry);
        }
    }

    /// worker の in-flight dispatch のうち `tokens` の plugin に触れているものを排出する。戻り値 = 上限までに
    /// `process()` から抜けなかった token — その plugin には触れない ([`crate::process_server::WorkerPool::quiesce`])。
    pub(crate) fn quiesce(&self, tokens: &[InstanceToken]) -> Vec<InstanceToken> {
        let mut stuck = self.worker_pool.as_ref().map_or_else(Vec::new, |pool| pool.quiesce(tokens));
        for token in tokens {
            if self.wedged.contains(token) && !stuck.contains(token) {
                stuck.push(*token);
            }
        }
        if !stuck.is_empty() {
            tracing::error!(?stuck, "plugin の process() が抜けないので、その plugin には触れずに続ける");
        }
        stuck
    }

    /// worker pool を閉じる。`false` = 止まらない worker が残った ([`PluginHost::wedged`])。
    pub(crate) fn close_worker_pool(&mut self) -> bool {
        if let Some(pool) = self.worker_pool.take() {
            self.wedged.extend(pool.shutdown());
        }
        if !self.wedged.is_empty() {
            tracing::error!(wedged = ?self.wedged, "plugin の process() から戻らない worker が残ったので plugin_host を終える");
        }
        self.wedged.is_empty()
    }

    /// `process()` から抜けない plugin の record を、worker が使っている audio half / shmem / 計測枠を壊さずに
    /// 手放す (plugin_host が立て直されるまで残る)。daw_gui からは消えたように見せる。
    pub(crate) fn abandon_stuck_device(&mut self, device: DeviceAddr, rec: InstanceRecord, emit_unloaded: bool) {
        editor_keys::with_router(|r| r.unregister_editor(device));
        if let Some(editor) = rec.editor.as_ref() {
            // 窓は壊さない (gui_destroy を通らずに plugin の子窓を消すことになる)。隠すだけ。
            editor.hide();
            if self.instances.values().all(|r| r.editor.is_none()) {
                editor_window::clear_windows_active();
            }
            self.emit(PluginEvent::SlotGuiClosed { device });
        }
        if emit_unloaded {
            self.emit(PluginEvent::SlotPluginUnloaded { device });
        }
        std::mem::forget(rec);
    }
}
