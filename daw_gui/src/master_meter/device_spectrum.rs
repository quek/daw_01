//! EQ Par の背後に出す device ごとのスペクトラム (r.md #129 Q14、
//! `docs/plan_rack_native_devices.md` §11.2) のテレメトリポーラ側。
//!
//! daw_audio が `common::device_scope_bridge` の slot へ書いた「その device を通った後の音」を
//! 読み、`(project, device_id)` ごとに [`SpectrumAnalyzer`] を回す。解析の作りと表示の弾道は
//! マスターのスペクトラムと同じ (同じ設定を渡す)。見出しから消えた device の解析器は捨てる。

use std::sync::Arc;

use common::device_scope_bridge::{DeviceScopeBridgeHandle, DeviceScopeReader, MAX_DEVICE_SCOPES};
use common::protocol::ProjectKey;

use super::settings::MeterSettings;
use super::spectrum::SpectrumAnalyzer;

/// ポーラスレッドが 1 本だけ持つ読み手 (単一 reader 前提のカーソル)。
pub struct DeviceSpectrumPoller {
    reader: DeviceScopeReader,
    buf: Vec<[f32; 2]>,
    /// slot ごとの解析器 (見出しが変わったら作り直す)。
    analyzers: [Option<((ProjectKey, u64), SpectrumAnalyzer)>; MAX_DEVICE_SCOPES],
    /// 直前の tick で何か送ったか (空になった 1 回だけ空の tick を送り、表示を消す)。
    sent_nonempty: bool,
}

impl Default for DeviceSpectrumPoller {
    fn default() -> Self {
        Self {
            reader: DeviceScopeReader::default(),
            buf: Vec::new(),
            analyzers: std::array::from_fn(|_| None),
            sent_nonempty: false,
        }
    }
}

impl DeviceSpectrumPoller {
    /// 全 slot を読み進め、`project` の device の表示値 (`SPECTRUM_BANDS` 帯の dB) を返す。
    /// 送る必要が無い tick (前回も今回も空) は `None`。
    pub fn tick(
        &mut self,
        h: &DeviceScopeBridgeHandle,
        project: ProjectKey,
        settings: &MeterSettings,
    ) -> Option<Vec<(u64, Arc<[f32]>)>> {
        let sr = h.sample_rate();
        let mut out = Vec::new();
        for k in 0..MAX_DEVICE_SCOPES {
            self.buf.clear();
            let Some((slot_project, device_id, _)) = self.reader.read(h, k, &mut self.buf) else {
                self.analyzers[k] = None;
                continue;
            };
            if sr == 0 {
                continue;
            }
            let key = (slot_project, device_id);
            let analyzer = match &mut self.analyzers[k] {
                Some((held, a)) if *held == key => {
                    a.apply(sr, settings);
                    a
                }
                slot => &mut slot.insert((key, SpectrumAnalyzer::new(sr, settings))).1,
            };
            analyzer.process(&self.buf);
            if slot_project == project {
                out.push((device_id, Arc::from(analyzer.display_db())));
            }
        }
        if out.is_empty() && !std::mem::replace(&mut self.sent_nonempty, false) {
            return None;
        }
        self.sent_nonempty = !out.is_empty();
        Some(out)
    }
}
