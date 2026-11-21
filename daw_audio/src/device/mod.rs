//! オーディオデバイスの層 (r.md #156、設計は `docs/plan_audio_io.md`)。
//!
//! - [`control`]: 制御スレッド。デバイスを開く・止める・起こす・開き直すのは全部そこ (ASIO は作ったスレッドに
//!   縛られる)。
//! - backend 3 種 ([`system`] = OS 標準 / [`asio_device`] = ASIO / [`synthetic`] = 合成): 自分の形式の buffer を
//!   f32 に直して [`callback`] (engine の入口。全 backend 共通) を呼ぶだけ。
//! - [`input`]: RT でデバイスの入力 ch の音を読む口 (録音の取り出しと、描画の中のモニター)。
//! - [`monitor`] / [`convert`]: 入力を別スレッドへ渡す ring、sample 形式の変換。
//! - [`enumerate`]: この PC のデバイスの列挙 (開かない)。

#[cfg(all(windows, target_pointer_width = "64"))]
mod asio_device;
pub mod callback;
pub mod control;
pub mod convert;
pub mod enumerate;
pub mod input;
pub mod monitor;
mod synthetic;
mod system;

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use common::audio_device::{AudioDeviceConfig, AudioDeviceInfo, AudioDriverKind, AudioInputDevice};
use common::metrics_bridge::MetricsBridgeHandle;

use callback::CallbackSlot;

/// metrics 面の置き場。デバイスは session (shmem の名前) が届く前に開くので、開いた時点では空で、
/// engine を置くときに入る。RT からは `get` で読む (ロックしない)。
pub type MetricsCell = Arc<OnceLock<Arc<MetricsBridgeHandle>>>;

/// backend がデバイスを開くときに借りるもの。
pub struct OpenCtx {
    pub slot: CallbackSlot,
    pub metrics: MetricsCell,
    /// ASIO の `sysRef` に渡す窓 (制御スレッドの隠し窓の HWND)。
    pub sys_window: *mut c_void,
}

/// 開いているデバイス。**制御スレッドの上でだけ触る**。drop でデバイスを閉じる。
pub enum Backend {
    System(system::SystemDevice),
    #[cfg(all(windows, target_pointer_width = "64"))]
    Asio(asio_device::AsioDevice),
    Synthetic(synthetic::SyntheticDevice),
}

/// [`Backend::poll`] が見つけた変化。
pub enum Change {
    None,
    /// `AudioDeviceInfo` の中身 (レイテンシなど) が変わった。stream はそのまま。
    Info,
    /// ドライバーが開き直しを求めた (理由)。
    Reopen(String),
    /// デバイスが使えなくなった (理由)。
    Lost(String),
}

impl Backend {
    /// コールバックを動かす / 止める (アイドル park と終了)。
    pub fn set_running(&mut self, run: bool) -> Result<(), String> {
        match self {
            Self::System(d) => d.set_running(run),
            #[cfg(all(windows, target_pointer_width = "64"))]
            Self::Asio(d) => d.set_running(run),
            Self::Synthetic(d) => {
                d.set_running(run);
                Ok(())
            }
        }
    }

    /// デバイス側の変化を拾う (制御スレッドが周期的に呼ぶ)。`info` は今のデバイス情報で、変わった分を直す。
    pub fn poll(&mut self, info: &mut AudioDeviceInfo, metrics: Option<&MetricsBridgeHandle>) -> Change {
        match self {
            Self::System(d) => d.poll(info),
            #[cfg(all(windows, target_pointer_width = "64"))]
            Self::Asio(d) => d.poll(info, metrics),
            Self::Synthetic(d) => {
                let _ = metrics;
                d.poll()
            }
        }
    }

    /// ドライバーのコントロールパネルを開く (ASIO だけ)。
    pub fn control_panel(&self) -> Result<(), String> {
        match self {
            #[cfg(all(windows, target_pointer_width = "64"))]
            Self::Asio(d) => d.control_panel(),
            _ => Err("このデバイスにコントロールパネルはありません".to_string()),
        }
    }
}

/// 設定のデバイスを開く (まだ鳴らさない)。**必ず何かを開いて返す**: 設定どおりに開けなければ OS 標準の既定の
/// 出力へ戻し、それも無ければ音の出ない合成デバイスで engine だけ動かす。戻した理由は
/// `AudioDeviceInfo::fallback_reason` に入れる (保存された設定は daw_gui のもので、ここでは変えない)。
pub fn open(config: &AudioDeviceConfig, ctx: &OpenCtx) -> (Backend, AudioDeviceInfo) {
    let reason = match open_configured(config, ctx) {
        Ok(opened) => return opened,
        Err(reason) => reason,
    };
    tracing::warn!(%reason, "audio device did not open as configured; falling back");
    let is_plain_default = config.driver == AudioDriverKind::System
        && config.system_output.is_none()
        && config.system_input == AudioInputDevice::None;
    let reason = if is_plain_default {
        reason
    } else {
        match system::open(&AudioDeviceConfig::default(), ctx) {
            Ok((device, mut info)) => {
                info.fallback_reason = Some(reason);
                return (Backend::System(device), info);
            }
            Err(e) => format!("{reason} / OS 標準の既定デバイスも開けません: {e}"),
        }
    };
    let null = AudioDeviceConfig { synthetic_device: Some(synthetic::NULL.to_string()), ..Default::default() };
    match synthetic::open(&null, ctx) {
        Ok((device, mut info)) => {
            info.fallback_reason = Some(reason);
            (Backend::Synthetic(device), info)
        }
        // スレッドも作れない。デバイスなしで返す (engine は描かないが、プロセスは生きて理由を伝える)。
        Err(e) => {
            tracing::error!(error = %e, "no audio device at all");
            (
                Backend::Synthetic(synthetic::SyntheticDevice::dead()),
                AudioDeviceInfo { fallback_reason: Some(reason), ..Default::default() },
            )
        }
    }
}

fn open_configured(config: &AudioDeviceConfig, ctx: &OpenCtx) -> Result<(Backend, AudioDeviceInfo), String> {
    match config.driver {
        AudioDriverKind::System => system::open(config, ctx).map(|(d, info)| (Backend::System(d), info)),
        #[cfg(all(windows, target_pointer_width = "64"))]
        AudioDriverKind::Asio => asio_device::open(config, ctx).map(|(d, info)| (Backend::Asio(d), info)),
        #[cfg(not(all(windows, target_pointer_width = "64")))]
        AudioDriverKind::Asio => Err("このビルドは ASIO を使えません".to_string()),
        AudioDriverKind::Synthetic => synthetic::open(config, ctx).map(|(d, info)| (Backend::Synthetic(d), info)),
    }
}
