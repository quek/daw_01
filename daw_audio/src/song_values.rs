//! 値のみの `Song` 更新 (mixer / 内蔵 device / send / record-arm / bpm / 拍子 / 移調)。
//!
//! どれも `Song` の 1 フィールドを書き換えて **値のみ bundle** で publish するだけで、
//! routing schedule の再 compile を伴わない (`docs/plan_arch_refactor.md` §5 D)。
//! RT 側は snapshot から live-read する。
//!
//! `recv_loop` の巨大 match がこの 9 コマンドだけで 50 行以上を使っていたので
//! 1 本にまとめた。`daw_audio/src/main.rs` は実コード 1,569 行の baseline 天井
//! ちょうどで **1 行も太れない**ので、機能を足す前にここへ逃がしている (不変条件 9)。
//!
//! **宛先は全部安定 id** (`Track::id` / `Send::id`) — positional index は使わない
//! (アーキ不変条件 1)。

use common::model::{MAX_TRACK_GAIN, Song};
use common::protocol::AudioCommand;

/// `cmd` が値のみ更新なら `song` に適用して `true`。それ以外は `false`。
///
/// クランプの範囲は GUI 側と同じ SSoT を使う (`MAX_TRACK_GAIN` = +6 dB、r.md #11)。
/// IPC は信頼境界なので、範囲外の値はここで必ず潰す。
pub fn apply(cmd: &AudioCommand, song: &mut Song) -> bool {
    match *cmd {
        AudioCommand::SetTrackVolume { track, volume, .. } => {
            with_track(song, track, |t| t.volume = volume.clamp(0.0, MAX_TRACK_GAIN));
        }
        AudioCommand::SetTrackPan { track, pan, .. } => {
            with_track(song, track, |t| t.pan = pan.clamp(-1.0, 1.0));
        }
        AudioCommand::SetTrackMuted { track, muted, .. } => {
            with_track(song, track, |t| t.muted = muted);
        }
        AudioCommand::SetTrackSolo { track, solo, .. } => {
            with_track(song, track, |t| t.solo = solo);
        }
        // r.md #129: 内蔵 device の値。IPC は信頼境界なので `replace_values` がフィールド単位で
        // 丸めてから載せる (種類違いの値は捨てる)。device id は song 全体で一意なので track で
        // 引かない (master fx chain の device にも同じ口で届く)。
        AudioCommand::SetNativeDevice { device_id, bypassed, params, .. } => {
            if let Some(d) = song.native_by_id_mut(device_id) {
                d.replace_values(bypassed, params);
            }
        }
        // master のフェーダー後 Limiter。**シーリングが壊れると出力が丸ごと消える**ので境界で丸める。
        AudioCommand::SetMasterLimiter { mut limiter, .. } => {
            limiter.sanitize();
            song.master_limiter = limiter;
        }
        AudioCommand::SetTrackArmed { track, armed, .. } => {
            with_track(song, track, |t| t.armed = armed);
        }
        AudioCommand::SetSendGain { track, send_id, gain, .. } => {
            with_send(song, track, send_id, |s| {
                // track / master と同じ +6 dB 上限を共有 (r.md #11 sibling)。
                s.gain = gain.clamp(0.0, MAX_TRACK_GAIN);
            });
        }
        AudioCommand::SetSendEnabled { track, send_id, enabled, .. } => {
            with_send(song, track, send_id, |s| s.enabled = enabled);
        }
        // r.md #110 Parallel chain の mixer (gain / pan / mute / solo)。宛先は安定
        // `ParallelChain::id` (track は所有者の確認にだけ使う)。
        AudioCommand::SetChainGain { chain_id, gain, .. } => {
            with_chain(song, chain_id, |c| c.gain = gain.clamp(0.0, MAX_TRACK_GAIN));
        }
        AudioCommand::SetChainPan { chain_id, pan, .. } => {
            with_chain(song, chain_id, |c| c.pan = pan.clamp(-1.0, 1.0));
        }
        AudioCommand::SetChainMuted { chain_id, muted, .. } => {
            with_chain(song, chain_id, |c| c.muted = muted);
        }
        AudioCommand::SetChainSolo { chain_id, solo, .. } => {
            with_chain(song, chain_id, |c| c.solo = solo);
        }
        AudioCommand::SetParallelOutGain { parallel_id, gain, .. } => {
            with_parallel(song, parallel_id, |r| r.out_gain = gain.clamp(0.0, MAX_TRACK_GAIN));
        }
        AudioCommand::SetParallelGainMatch { parallel_id, on, .. } => {
            with_parallel(song, parallel_id, |r| r.gain_match = on);
        }
        AudioCommand::SetParallelSplitFreq { parallel_id, edge, hz, .. } => {
            with_parallel(song, parallel_id, |r| {
                r.set_split_freq(edge, hz);
            });
        }
        // r.md #114 Selector: アクティブ chain / クロスフェード時間 (規則は model の setter が SSoT)。
        AudioCommand::SetParallelActiveChain { parallel_id, chain_id, .. } => {
            with_parallel(song, parallel_id, |r| {
                r.set_active_chain(chain_id);
            });
        }
        AudioCommand::SetParallelSelectorFade { parallel_id, fade_ms, .. } => {
            with_parallel(song, parallel_id, |r| {
                r.set_selector_fade(fade_ms);
            });
        }
        AudioCommand::SetSongBpm { bpm, .. } => song.bpm = bpm.clamp(1.0, 400.0),
        AudioCommand::SetSongTimeSigNumerator { num, .. } => song.time_sig.0 = num.clamp(1, 32),
        AudioCommand::SetSongTranspose { semitones, .. } => {
            let max = common::transpose::TRANSPOSE_MAX_SEMITONES;
            song.transpose = semitones.clamp(-max, max);
        }
        _ => return false,
    }
    true
}

/// 安定 `Track::id` で引いて適用する (見つからなければ何もしない)。
fn with_track(song: &mut Song, track_id: u32, f: impl FnOnce(&mut common::model::Track)) {
    if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) {
        f(t);
    }
}

/// 安定 `Track::id` + `Send::id` の 2 段で引いて適用する。
fn with_send(
    song: &mut Song,
    track_id: u32,
    send_id: u32,
    f: impl FnOnce(&mut common::model::Send),
) {
    if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id)
        && let Some(s) = t.sends.iter_mut().find(|s| s.id == send_id)
    {
        f(s);
    }
}

/// r.md #110: `parallel_id` の Parallel に `f` を当てる (dangling は no-op)。
fn with_parallel(song: &mut Song, parallel_id: u64, f: impl FnOnce(&mut common::model::Parallel)) {
    if let Some(r) = song.parallel_by_id_mut(parallel_id) {
        f(r);
    }
}

/// r.md #110: `chain_id` の Parallel chain に `f` を当てる (dangling は no-op)。
fn with_chain(song: &mut Song, chain_id: u64, f: impl FnOnce(&mut common::model::ParallelChain)) {
    if let Some(c) = song.chain_by_id_mut(chain_id) {
        f(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Send, SendMode, Track};

    #[test]
    fn 値は_ipc_境界でクランプされる() {
        let mut song = Song::default();
        song.tracks.push(Track {
            id: 3,
            sends: vec![Send {
                id: 1,
                dest_track_id: 4,
                gain: 1.0,
                enabled: true,
                mode: SendMode::PostFader,
            }],
            ..Track::default()
        });

        assert!(apply(&AudioCommand::SetTrackVolume { project: common::protocol::ProjectKey(1), track: 3, volume: 99.0 }, &mut song));
        assert_eq!(song.tracks[0].volume, MAX_TRACK_GAIN);
        assert!(apply(&AudioCommand::SetTrackPan { project: common::protocol::ProjectKey(1), track: 3, pan: -9.0 }, &mut song));
        assert_eq!(song.tracks[0].pan, -1.0);
        assert!(apply(
            &AudioCommand::SetSendGain { project: common::protocol::ProjectKey(1), track: 3, send_id: 1, gain: -5.0 },
            &mut song
        ));
        assert_eq!(song.tracks[0].sends[0].gain, 0.0);
        assert!(apply(&AudioCommand::SetSongBpm { project: common::protocol::ProjectKey(1), bpm: 0.0 }, &mut song));
        assert_eq!(song.bpm, 1.0);
        assert!(apply(&AudioCommand::SetSongTimeSigNumerator { project: common::protocol::ProjectKey(1), num: 0 }, &mut song));
        assert_eq!(song.time_sig.0, 1);
        assert!(apply(&AudioCommand::SetSongTranspose { project: common::protocol::ProjectKey(1), semitones: -100 }, &mut song));
        assert_eq!(song.transpose, -24);

        // 存在しない id は何も壊さない。
        assert!(apply(&AudioCommand::SetTrackMuted { project: common::protocol::ProjectKey(1), track: 99, muted: true }, &mut song));
        assert!(!song.tracks[0].muted);
        // 値のみ更新でないコマンドは扱わない。
        assert!(!apply(&AudioCommand::Play { project: common::protocol::ProjectKey(1) }, &mut song));
    }


    /// IPC は信頼境界。壊れた値 (NaN / 範囲外) が係数計算へ入るとフィルタが発散し、
    /// **NaN が master まで伝播して停止するまで無音**になる。ここで必ず潰す。
    /// 種類違いの値は捨て、master fx chain の device にも届く。
    #[test]
    fn 内蔵デバイスの値は_ipc_境界で丸められる() {
        use common::model::{
            CompSettings, Device, EqSettings, MasterLimiterSettings, NativeDevice, NativeKind, NativeParams,
        };
        let pk = common::protocol::ProjectKey(1);
        let mut song = Song::default();
        song.tracks.push(Track {
            id: 7,
            devices: vec![
                Device::Native(NativeDevice::new_builtin(NativeKind::Comp, 11)),
                Device::Native(NativeDevice::new_builtin(NativeKind::Eq, 12)),
            ],
            ..Track::default()
        });
        song.master_fx_chain = vec![Device::Native(NativeDevice::new_builtin(NativeKind::ToneEq, 21))];

        let mut eq = EqSettings::default();
        eq.hmf.freq_hz = f32::NAN;
        eq.hmf.gain_db = 999.0;
        let cmd = AudioCommand::SetNativeDevice { project: pk, device_id: 12, bypassed: false, params: NativeParams::Eq(eq) };
        assert!(apply(&cmd, &mut song));
        let got = *song.native_by_id(12).unwrap();
        let NativeParams::Eq(e) = got.params else { panic!("eq") };
        assert!(e.hmf.freq_hz.is_finite() && (e.hmf.gain_db - 15.0).abs() < 1e-6, "{e:?}");
        assert!(!got.bypassed);

        let comp = CompSettings { attack_ms: -5.0, sc_freq_hz: 1.0, ..CompSettings::default() };
        let wrong_kind = AudioCommand::SetNativeDevice { project: pk, device_id: 12, bypassed: true, params: NativeParams::Comp(comp) };
        assert!(apply(&wrong_kind, &mut song));
        assert_eq!(*song.native_by_id(12).unwrap(), got, "種類違いの値は何もしない");
        let cmd = AudioCommand::SetNativeDevice { project: pk, device_id: 11, bypassed: false, params: NativeParams::Comp(comp) };
        assert!(apply(&cmd, &mut song));
        let NativeParams::Comp(c) = song.native_by_id(11).unwrap().params else { panic!("comp") };
        assert!((c.attack_ms - 0.1).abs() < 1e-6 && c.sc_freq_hz == 0.0, "{c:?}");

        let tone = NativeParams::default_of(NativeKind::ToneEq);
        assert!(apply(&AudioCommand::SetNativeDevice { project: pk, device_id: 21, bypassed: false, params: tone }, &mut song));
        assert!(!song.native_by_id(21).unwrap().bypassed, "master fx chain の device にも届く");

        let limiter = MasterLimiterSettings { on: true, ceiling_db: f32::NAN };
        assert!(apply(&AudioCommand::SetMasterLimiter { project: pk, limiter }, &mut song));
        assert!(song.master_limiter.on && song.master_limiter.ceiling_db == -1.0);
    }
}
