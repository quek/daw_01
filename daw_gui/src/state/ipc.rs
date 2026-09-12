//! S3b-1: AppData state group (IpcState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::sync::{Arc, Mutex};

use common::plugin_db::PluginDatabase;
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::UnboundedSender;

use crate::dispatcher::BackgroundDispatcher;

/// `(device_id, param_id)` の複合キー。 生タプルにしないのは、 positional
/// キーと見分けが付かなくなる (arch-lint / 読み手の双方) ため。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceParamKey {
    pub device_id: u64,
    pub param_id: u32,
}

pub struct IpcState {
    // PDC の入力となる「plugin が報告した latency」 は daw_gui では **持たない**。
    // plugin_host からの `PluginEvent::PluginLatencyChanged` を
    // `AudioCommand::SetDeviceLatency` としてそのまま engine へ中継し、
    // track / master の合計は `compile_schedule` が device chain から導出する
    // (r.md #9: 集計値を `Song` に書き戻すと保存され、 開き直しで「開いただけで `*`」)。

    // -------- Resource monitor (r.md #3) --------
    /// 集計済みリソース指標 (DSP load / system CPU / fps / xrun / mem)。 poller
    /// (DSP/xrun/buffer) / sysinfo スレッド (CPU/mem) / runner (fps) が別々に
    /// 更新し、 status bar 常駐メーターと詳細パネルが読む。
    pub metrics: common::metrics_bridge::ResourceMetrics,
    /// MetricsBridge ハンドル (per-plugin CPU の直接読み出し用)。 GUI mode のみ
    /// `Some`、 script / test は `None`。 詳細パネルが Song の各 device (かつ
    /// `loaded_devices` に居るもの) について `plugin_dsp_us` を直接読む。
    pub metrics_bridge: Option<Arc<common::metrics_bridge::MetricsBridgeHandle>>,

    // -------- Plugin database / picker --------
    pub plugin_db: Option<Arc<PluginDatabase>>,

    // -------- IPC senders --------
    pub audio_tx: Option<UnboundedSender<AudioCommand>>,
    pub plugin_tx: Option<UnboundedSender<PluginCommand>>,
    /// 子プロセス自動再起動 supervisor (`bootstrap::ChildSupervisor`)。
    /// production (GUI mode) では `Some`、 script / test 経路では `None`。
    /// `ChildDisconnected` event 受信時に `respawn(kind)` で新 child を
    /// spawn + handshake + Session/OpenWorkerPool 再送し、 新 tx で
    /// `audio_tx` / `plugin_tx` を差し替える。
    pub supervisor: Option<Arc<crate::bootstrap::ChildSupervisor>>,
    /// (A1 r.md #8) オーディオセッションの実サンプルレート (= daw_audio が報告した
    /// デバイス実レート、 `bootstrap.sample_rate`)。 拍↔sample 変換 (seek / export
    /// range / clip 尺) はこの値を使い、 engine と一致させる。 session-only。
    pub sample_rate: u32,
    /// 直近の child 切断時刻 (kind 別)。短時間に閾値以上切断したら crash-loop と
    /// 判断して自動 respawn を止める (= 落ちるプラグインを抱えたプロジェクトで
    /// respawn→reload→再 crash の無限ループに陥り GUI が固まるのを防ぐ)。session-only。
    pub child_disconnect_log: Vec<(common::protocol::ChildKind, std::time::Instant)>,


    // -------- Plugin load tracking (A7 race-condition fix) -----------
    /// v29: `SetSlotPlugin` の要求世代 counter (AppData-wide 単調増加 =
    /// per-device 単調増加を含意)。 送信ごとに bump して
    /// `pending_plugin_loads` へ記録する。
    pub(crate) next_plugin_load_generation: u64,

    // -------- Background workers --------
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    pub is_rescanning: bool,

    /// 背景スレッド (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX
    /// 合成 / plugin DB rescan) からメインスレッドへ `AppEvent` を送るための
    /// dispatcher。 production は `WinitDispatcher` (winit `EventLoopProxy`
    /// ラップ)、 test は `RecordingDispatcher` (Mutex<Vec> に蓄積)。
    pub event_proxy: Arc<dyn BackgroundDispatcher>,
}
