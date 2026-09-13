//! handler::device_event — [`DeviceEvent`] の唯一の dispatcher。
//!
//! 各 arm の本体は domain ごとの handler (`devices` / `device_relocate` / `parallel` /
//! `automation_lanes` / `modulation`) のメソッドで、ここは振り分けるだけ。

use crate::event_device::DeviceEvent;
use crate::state::AppData;

impl AppData {
    /// [`AppEvent::Device`](crate::event::AppEvent::Device) の本体。
    pub(crate) fn handle_device_event(&mut self, ev: DeviceEvent) {
        match ev {
            DeviceEvent::ToggleSlotGui { device_id } => {
                self.toggle_slot_gui(device_id);
            }
            DeviceEvent::SetVideoFxParam { device_id, param_id, value_real } => {
                self.set_video_fx_param(device_id, param_id, value_real);
            }
            DeviceEvent::SetPluginParam { device_id, param_id, value_real } => {
                self.set_plugin_param(device_id, param_id, value_real);
            }
            DeviceEvent::RemoveDevices { device_ids } => {
                self.remove_devices(device_ids);
            }
            DeviceEvent::RelocateDevices(req) => {
                self.relocate_devices(req);
            }
            DeviceEvent::SelectDevice { device_id, modifier } => {
                self.apply_select_device(device_id, modifier);
            }
            DeviceEvent::ReloadDevice { device_id } => {
                self.reload_device(device_id);
            }
            DeviceEvent::ExplodeParallelOut { device_id } => {
                self.explode_parallel_out(device_id);
            }
            DeviceEvent::SetParallelOutputRoute { device_id, port, dest } => {
                self.set_parallel_output_route(device_id, port, dest);
            }
            DeviceEvent::SetSidechainSource { device_id, port, source } => {
                self.set_sidechain_source(device_id, port, source);
            }
            DeviceEvent::SetPluginSendAllKeys { device_id, enabled } => {
                self.set_plugin_send_all_keys(device_id, enabled);
            }
            DeviceEvent::SetDevicesBypassed { device_ids, bypassed } => {
                self.set_devices_bypassed(&device_ids, bypassed);
            }
            DeviceEvent::SetAuxInputTapPoint { device_id, port, tap_point } => {
                self.set_aux_input_tap_point(device_id, port, tap_point);
            }
            // ---- r.md #110 Parallel ----
            DeviceEvent::AddParallel { chain, at } => self.add_parallel(chain, at),
            DeviceEvent::GroupDevices { device_ids } => self.group_devices(device_ids),
            DeviceEvent::UngroupParallel { parallel_id } => self.ungroup_parallel(parallel_id),
            DeviceEvent::AddParallelChain { parallel_id } => self.add_parallel_chain(parallel_id),
            DeviceEvent::DuplicateParallelChain { chain_id } => self.duplicate_parallel_chain(chain_id),
            DeviceEvent::RenameParallelChain { chain_id, name } => self.rename_parallel_chain(chain_id, name),
            DeviceEvent::RenameParallel { parallel_id, name } => self.rename_parallel(parallel_id, name),
            DeviceEvent::SetParallelChainColor { chain_id, color } => {
                self.set_parallel_chain_color(chain_id, color);
            }
            DeviceEvent::SetParallelColor { parallel_id, color } => self.set_parallel_color(parallel_id, color),
            DeviceEvent::SetChainMixer { chain_id, edit } => self.set_chain_mixer(chain_id, edit),
            DeviceEvent::SetParallelMixer { parallel_id, edit } => self.set_parallel_mixer(parallel_id, edit),
            DeviceEvent::SetParallelSplit { parallel_id, split } => self.set_parallel_split(parallel_id, split),
            DeviceEvent::ToggleParallelNodeCollapsed { id } => self.toggle_parallel_node_collapsed(id),
        }
    }
}
