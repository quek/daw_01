//! オーディオデバイスの設定・列挙・今開いているデバイスの情報 (r.md #156、設計は `docs/plan_audio_io.md`)。
//!
//! - [`AudioDeviceConfig`]: **何を開きたいか**。所有者は daw_gui (`app_config.json` に永続)。daw_audio は
//!   起動時の handshake (`AudioCommand::OpenDevice`) で受け取るだけで、覚えない。
//! - [`AudioDeviceList`]: この PC にあるデバイスの一覧 (daw_audio が列挙して返す)。
//! - [`AudioDeviceInfo`]: **実際に開けたもの**。設定どおりに開けなければ OS 標準の既定デバイスへ戻し、
//!   理由を [`AudioDeviceInfo::fallback_reason`] に入れる (保存した設定は変えない)。
//!
//! デバイスの住所は表示名ではなく安定 id ([`AudioDeviceRef`])。ch は **デバイスの ch 番号** (0 始まり) で
//! 指し、一覧の並び順では指さない (アーキ不変条件 1)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// ドライバーの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Encode, Decode, Serialize, Deserialize)]
pub enum AudioDriverKind {
    /// OS 標準 (Windows は WASAPI 共有モード)。設定画面の「Windows 標準」。
    #[default]
    System,
    /// ASIO (ドライバーの COM interface を直接呼ぶ)。
    Asio,
    /// 合成のデバイス (実機を開かない検査用)。設定画面には出さない。デバイス id は [`synthetic`]。
    Synthetic,
}

/// 合成のデバイス ([`AudioDriverKind::Synthetic`]) の id ([`AudioDeviceConfig::output`] の `id` に入れる)。
pub mod synthetic {
    /// 入力 ch `c` の、入力の通し番号 `i` のフレームが `(c, i)` から決まる値 (`ramp_sample`)。
    pub const RAMP: &str = "ramp";
    /// 入力 ch 0 / 1 = デバイスへ出した出力 L / R (`mute_output` で消す前) を「入力レイテンシ + 出力レイテンシ」
    /// フレーム遅らせたもの (他の入力 ch は無音)。申告レイテンシは入力 [`LOOPBACK_INPUT_LATENCY`]、出力
    /// [`LOOPBACK_OUTPUT_LATENCY`] + バッファ 1 枚ぶん (実機と同じく、出力レイテンシはバッファを含む)。
    pub const LOOPBACK: &str = "loopback";
    /// `LOOPBACK` の入力レイテンシ (frames)。
    pub const LOOPBACK_INPUT_LATENCY: u32 = 64;
    /// `LOOPBACK` の出力レイテンシのうち、バッファ 1 枚ぶんを除いた分 (frames)。
    pub const LOOPBACK_OUTPUT_LATENCY: u32 = 128;
    /// 合成のデバイスの入力 ch 数。
    pub const INPUT_CHANNELS: u16 = 4;
    /// `RAMP` ののこぎり波の周期 (フレーム)。
    pub const RAMP_PERIOD: u64 = 1000;

    /// `RAMP` の入力 ch `ch` の、通し番号 `frame` のフレームの値。ch ごとに振幅が違う
    /// (`0.5 / (ch + 1)`)、周期 [`RAMP_PERIOD`] フレームののこぎり波 (`-1..1` × 振幅)。
    #[must_use]
    pub fn ramp_sample(ch: u16, frame: u64) -> f32 {
        let phase = (frame % RAMP_PERIOD) as f32 / RAMP_PERIOD as f32;
        (phase * 2.0 - 1.0) * (0.5 / f32::from(ch + 1))
    }
}

/// デバイスの住所: 安定 id と、見つからないとき用の表示名。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode, Serialize, Deserialize)]
pub struct AudioDeviceRef {
    /// 安定 id。ASIO はドライバーの CLSID (`{XXXXXXXX-XXXX-...}`)、OS 標準は OS のデバイス id
    /// (WASAPI の endpoint id)、合成は [`synthetic`] の定数。
    pub id: String,
    /// 選んだときの表示名。id のデバイスが見つからないときに「○○ が見つかりません」と出すためだけに持つ
    /// (照合には使わない)。
    pub name: String,
}

/// OS 標準 ([`AudioDriverKind::System`]) の入力デバイスの選び方。ASIO の入力はドライバーが持つので使わない。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Encode, Decode, Serialize, Deserialize)]
pub enum AudioInputDevice {
    /// 入力を開かない (既定 — 選ぶまでマイクを開かない)。
    #[default]
    None,
    /// OS の既定の入力デバイス。
    Default,
    Device(AudioDeviceRef),
}

/// デバイスの ch 番号の範囲 (`first` から `count` 本)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Encode, Decode, Serialize, Deserialize)]
pub struct ChannelRange {
    pub first: u16,
    pub count: u16,
}

impl ChannelRange {
    #[must_use]
    pub fn contains(self, ch: u16) -> bool {
        ch >= self.first && u32::from(ch) < u32::from(self.first) + u32::from(self.count)
    }

    /// `available` 本の ch を持つデバイスに収まる範囲へ切り詰める。
    #[must_use]
    pub fn clamped(self, available: u16) -> Self {
        let first = self.first.min(available);
        Self { first, count: self.count.min(available - first) }
    }
}

/// 何を開きたいか。どの field も「未指定 (`None`) = デバイスの既定」。
///
/// OS 標準と ASIO のデバイスの選択は別々に覚える (種類を行き来しても、それぞれで最後に選んだデバイスへ戻る)。
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioDeviceConfig {
    pub driver: AudioDriverKind,
    /// OS 標準の出力デバイス。`None` = OS の既定の出力デバイス。
    pub system_output: Option<AudioDeviceRef>,
    /// OS 標準の入力デバイス。
    pub system_input: AudioInputDevice,
    /// ASIO のドライバー (入出力とも)。`None` = 未選択 (ASIO を選んでも開けないので OS 標準へ戻る)。
    pub asio_driver: Option<AudioDeviceRef>,
    /// 合成のデバイス ([`synthetic`] の id)。`None` = [`synthetic::RAMP`]。
    pub synthetic_device: Option<String>,
    /// サンプルレート (Hz)。`None` = デバイスの今のレート。
    pub sample_rate: Option<u32>,
    /// バッファサイズ (frames)。`None` = デバイスの推奨値。
    pub buffer_frames: Option<u32>,
    /// 出力するステレオ組の先頭の ch 番号 (0 = ch 0 + 1、2 = ch 2 + 3)。`None` = 0。
    pub output_first_channel: Option<u16>,
    /// 使う入力 ch。`None` = デバイスの全入力 ch。`count == 0` = 入力を使わない。
    pub input_channels: Option<ChannelRange>,
}

impl AudioDeviceConfig {
    /// デバイスが替わる変更の後に、そのデバイスに依る項目 (バッファサイズ・ch) を「デバイスの既定」へ戻す。
    /// サンプルレートは残す (替えた先でも同じレートを望むのが普通。開けなければ既定へ落ちる)。
    fn reset_device_dependent(&mut self) {
        self.buffer_frames = None;
        self.output_first_channel = None;
        self.input_channels = None;
    }

    /// ドライバーの種類を替える。
    pub fn set_driver(&mut self, driver: AudioDriverKind) {
        if self.driver != driver {
            self.driver = driver;
            self.reset_device_dependent();
        }
    }

    /// OS 標準の出力デバイスを替える (`None` = OS の既定)。
    pub fn set_system_output(&mut self, device: Option<AudioDeviceRef>) {
        if self.system_output.as_ref().map(|d| &d.id) != device.as_ref().map(|d| &d.id) {
            self.reset_device_dependent();
        }
        self.system_output = device;
    }

    /// OS 標準の入力デバイスを替える。
    pub fn set_system_input(&mut self, device: AudioInputDevice) {
        if self.system_input != device {
            self.input_channels = None;
        }
        self.system_input = device;
    }

    /// ASIO のドライバーを替える。
    pub fn set_asio_driver(&mut self, driver: Option<AudioDeviceRef>) {
        if self.asio_driver.as_ref().map(|d| &d.id) != driver.as_ref().map(|d| &d.id) {
            self.reset_device_dependent();
        }
        self.asio_driver = driver;
    }
}

/// 一覧の 1 行。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct AudioDeviceEntry {
    pub device: AudioDeviceRef,
    /// OS の既定のデバイスか (ASIO では常に false)。
    pub is_default: bool,
}

/// この PC にあるデバイス (`AudioCommand::ListAudioDevices` の返事)。合成のデバイスは載せない。
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct AudioDeviceList {
    pub system_outputs: Vec<AudioDeviceEntry>,
    pub system_inputs: Vec<AudioDeviceEntry>,
    pub asio_drivers: Vec<AudioDeviceEntry>,
}

/// デバイスの ch 1 本。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct AudioChannelInfo {
    /// デバイスの ch 番号 (0 始まり)。
    pub index: u16,
    /// ドライバーが付けた名前 (無ければ空)。
    pub name: String,
}

/// 実際に開けたデバイス。
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct AudioDeviceInfo {
    /// 実際に開いた種類 (戻したときは [`AudioDriverKind::System`])。
    pub driver: AudioDriverKind,
    /// 開いた出力デバイス (ASIO はドライバー) の表示名。
    pub output_name: String,
    /// 開いた入力デバイスの表示名 (入力を開いていなければ `None`。ASIO は出力と同じ名前)。
    pub input_name: Option<String>,
    /// 実際のサンプルレート (Hz)。engine 全体 (session) のレートになる。
    pub sample_rate: u32,
    /// 実際のバッファサイズ (frames = コールバック 1 回のフレーム数。OS 標準は目安)。
    pub buffer_frames: u32,
    /// 選べるサンプルレート (昇順)。空 = ホストからは選べない。
    pub sample_rates: Vec<u32>,
    /// 選べるバッファサイズ (昇順)。空 = ホストからは選べない (ドライバーのコントロールパネルで変える)。
    pub buffer_sizes: Vec<u32>,
    /// デバイスの出力 ch の全部 (ch 番号順)。
    pub output_channels: Vec<AudioChannelInfo>,
    /// デバイスの入力 ch の全部 (ch 番号順)。入力を開いていなければ空。
    pub input_channels: Vec<AudioChannelInfo>,
    /// 実際に出力している組の先頭の ch 番号。
    pub output_first_channel: u16,
    /// 実際に使っている入力 ch (engine に届く ch)。
    pub active_inputs: ChannelRange,
    /// 入力レイテンシ (frames): コールバックに届いたブロックの先頭のフレームが、入力端子に来てから経った時間。
    /// ASIO はドライバーの申告値 (バッファぶんを含む)。OS 標準は推定値 (出力コールバックへ合わせ直す ring の
    /// 遅れを含む)。
    pub input_latency_frames: u32,
    /// 出力レイテンシ (frames): コールバックで渡したブロックの先頭のフレームが、出力端子から出るまでの時間。
    /// ASIO はドライバーの申告値 (バッファぶんを含む)。OS 標準は推定値。先読みの遅れは含まない。
    pub output_latency_frames: u32,
    /// ドライバーのコントロールパネルを開けるか (ASIO だけ)。
    pub has_control_panel: bool,
    /// 設定どおりに開けなかったときの理由 (人が読む 1 行)。出力が開けなければ OS 標準の既定デバイスへ戻す
    /// (それも無ければ、音の出ない合成のデバイスで engine だけ動かす)。OS 標準で入力だけ開けなかったときは、
    /// 出力は設定どおりで入力なしになる。
    pub fallback_reason: Option<String>,
}

impl AudioDeviceInfo {
    /// 入力 ch 番号 `ch` が engine に届いているか。
    #[must_use]
    pub fn input_is_active(&self, ch: u16) -> bool {
        self.active_inputs.contains(ch)
    }
}
