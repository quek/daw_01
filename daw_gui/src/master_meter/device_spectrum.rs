//! EQ Par の背後に出す device ごとのスペクトラム (r.md #129 Q14、
//! `docs/plan_rack_native_devices.md` §11.2) のテレメトリポーラ側。
//!
//! daw_audio が `common::device_scope_bridge` の slot へ書いた「その device を通った後の音」を
//! 読み、`(project, device_id)` ごとに [`SpectrumAnalyzer`] を回す。解析の作りと表示の弾道は
//! マスターのスペクトラムと同じ (同じ設定を渡す)。見出しから消えた device の解析器は捨てる。
//!
//! ## 音が流れている間だけ送る (r.md #49)
//!
//! engine は停止中も見出しの slot へ書き続ける (Par を開いている限り無音のフレームが流れる)。
//! 毎 tick 送ると GUI は窓がアクティブな限り 30fps で描き直し続けるので、マスターメーターの
//! `visual_digest` と同じ作法を取る:
//! - 表示解像度 ([`DISPLAY_STEP_DB`]) で量子化したダイジェストを slot ごとに持ち、表示が前回送ったものと
//!   同じ tick は **送らない** ([`DeviceSpectrumPoller::tick`] が `None`)。
//! - 無音が続いて表示も動かなくなった slot は解析そのものを休む (FFT を実時間で回し続けない)。
//! - 送る tick にはダイジェストを添え、GUI は tick の再描画判定の指紋に混ぜる。

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use common::device_scope_bridge::{DeviceScopeBridgeHandle, DeviceScopeReader, MAX_DEVICE_SCOPES};
use common::protocol::ProjectKey;

use super::SETTLE_TICKS;
use super::settings::MeterSettings;
use super::spectrum::{DISPLAY_STEP_DB, SpectrumAnalyzer};

/// 送る 1 tick ぶん (アクティブなタブの project の device だけ)。
pub struct DeviceSpectra {
    /// `(device id, SPECTRUM_BANDS 帯の表示値 dB)`。
    pub spectra: Vec<(u64, Arc<[f32]>)>,
    /// 表示解像度で量子化した中身のダイジェスト (project と device の組を含む)。表示が変わらなければ同じ値。
    pub visual_digest: u64,
}

/// slot 1 つぶんの解析状態。
struct Slot {
    key: (ProjectKey, u64),
    analyzer: SpectrumAnalyzer,
    /// 直近の表示値のダイジェスト。
    digest: u64,
    /// 無音かつ表示が変わらなかった連続 tick 数 (`SETTLE_TICKS` で解析を休む)。
    quiet_ticks: u32,
}

/// ポーラスレッドが 1 本だけ持つ読み手 (単一 reader 前提のカーソル)。
pub struct DeviceSpectrumPoller {
    reader: DeviceScopeReader,
    buf: Vec<[f32; 2]>,
    /// slot ごとの解析器 (見出しが変わったら作り直す)。
    slots: [Option<Slot>; MAX_DEVICE_SCOPES],
    /// 休んでいる解析器に設定の変更を反映させるための前回の設定。
    settings: Option<MeterSettings>,
    /// 直前に送った tick のダイジェスト (`None` = まだ何も送っていない)。
    sent: Option<u64>,
}

impl Default for DeviceSpectrumPoller {
    fn default() -> Self {
        Self {
            reader: DeviceScopeReader::default(),
            buf: Vec::new(),
            slots: std::array::from_fn(|_| None),
            settings: None,
            sent: None,
        }
    }
}

impl DeviceSpectrumPoller {
    /// 全 slot を読み進め、`project` の device の表示値を返す。表示が前回送ったものと変わらない tick
    /// (無音で落ち切った / まだ何も無い) は `None` — 送らない (module doc)。見出しが空になったときは
    /// 空の tick を 1 回だけ返して表示を消す。
    pub fn tick(
        &mut self,
        h: &DeviceScopeBridgeHandle,
        project: ProjectKey,
        settings: &MeterSettings,
    ) -> Option<DeviceSpectra> {
        let sr = h.sample_rate();
        // 設定が変わったら休んでいる解析器も起こす (レンジが変われば落ち切った表示値も変わる)。
        let settings_changed = self.settings.replace(*settings).is_some_and(|prev| prev != *settings);
        let mut digest = std::collections::hash_map::DefaultHasher::new();
        project.0.hash(&mut digest);
        let mut any = false;
        for k in 0..MAX_DEVICE_SCOPES {
            self.buf.clear();
            let Some((slot_project, device_id, _)) = self.reader.read(h, k, &mut self.buf) else {
                self.slots[k] = None;
                continue;
            };
            if sr == 0 {
                continue;
            }
            let key = (slot_project, device_id);
            let slot = match &mut self.slots[k] {
                Some(slot) if slot.key == key => {
                    slot.analyzer.apply(sr, settings);
                    slot
                }
                empty => empty.insert(Slot { key, analyzer: SpectrumAnalyzer::new(sr, settings), digest: 0, quiet_ticks: 0 }),
            };
            if settings_changed {
                slot.quiet_ticks = 0;
            }
            // 読めたフレームが無い tick (見出しの張り直し直後 / engine が休んでいる) は表示を据え置き、
            // 休む判定にも数えない — 数えると、減衰の途中で engine が休んだとき表示がそこで凍る。
            let quiet = self.buf.iter().all(|f| f[0] == 0.0 && f[1] == 0.0);
            if !self.buf.is_empty() && (!quiet || slot.quiet_ticks < SETTLE_TICKS) {
                slot.analyzer.process(&self.buf);
                let now = display_digest(slot.analyzer.display_db());
                slot.quiet_ticks = if quiet && now == slot.digest { slot.quiet_ticks.saturating_add(1) } else { 0 };
                slot.digest = now;
            }
            if slot_project == project {
                any = true;
                device_id.hash(&mut digest);
                slot.digest.hash(&mut digest);
            }
        }
        let visual_digest = digest.finish();
        // 送ったことが無いのに空 (消す表示が無い) / 前回と同じ表示 は送らない。
        if (!any && self.sent.is_none()) || self.sent == Some(visual_digest) {
            return None;
        }
        self.sent = Some(visual_digest);
        let spectra = self
            .slots
            .iter()
            .flatten()
            .filter(|s| s.key.0 == project)
            .map(|s| (s.key.1, Arc::from(s.analyzer.display_db())))
            .collect();
        Some(DeviceSpectra { spectra, visual_digest })
    }
}

/// 表示値を表示解像度で量子化したダイジェスト (無音で落ち切れば必ず収束する)。
fn display_digest(bands: &[f32]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in bands {
        let q = if v.is_finite() { (v / DISPLAY_STEP_DB).round() as i64 } else { i64::MIN };
        q.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> DeviceScopeBridgeHandle {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let i = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        DeviceScopeBridgeHandle::create(&format!("daw_01_device_spectrum_test_{}_{i}", std::process::id()))
            .expect("shmem")
    }

    /// r.md #49: 見出しの slot に無音が流れ続ける (停止中に Par を開いている) 間は、表示が落ち切ったら
    /// 送らなくなる。音が来たら送り、音が止んで表示が落ち切ればまた止まる。見出しが消えたら空を 1 回だけ送る。
    #[test]
    fn ticks_are_sent_only_while_the_display_changes() {
        let h = handle();
        h.set_sample_rate(48_000);
        let project = ProjectKey(1);
        let settings = MeterSettings::default();
        let mut poller = DeviceSpectrumPoller::default();
        let silence = vec![0.0_f32; 1_600];
        let mut run = |h: &DeviceScopeBridgeHandle, l: &[f32]| {
            h.write_block(0, l, l);
            poller.tick(h, project, &settings).map(|t| t.spectra.len())
        };

        h.set_slot(0, project, 7);
        assert_eq!(run(&h, &silence), Some(1), "見出しが付いた最初の tick は送る");
        let quiet_sends = (0..30).filter(|_| run(&h, &silence).is_some()).count();
        assert!(quiet_sends <= 1, "無音の間は表示が変わらないので送らない (送った回数 {quiet_sends})");
        assert_eq!(run(&h, &silence), None);

        let tone: Vec<f32> = (0..1_600).map(|n| (n as f32 * 0.05).sin() * 0.5).collect();
        assert_eq!(run(&h, &tone), Some(1), "音が来たら送る");
        let settled = (0..300).map(|_| run(&h, &silence)).skip_while(Option::is_some).take(20).all(|t| t.is_none());
        assert!(settled, "音が止んで表示が落ち切ったら送らなくなる");

        h.set_slot(0, ProjectKey(0), 0);
        assert_eq!(poller.tick(&h, project, &settings).map(|t| t.spectra.len()), Some(0), "空を 1 回だけ送る");
        assert!(poller.tick(&h, project, &settings).is_none());
    }
}
