//! 値のみの `Song` 更新 (mixer strip / send / record-arm / bpm / 拍子)。
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
        AudioCommand::SetTrackVolume { track, volume } => {
            with_track(song, track, |t| t.volume = volume.clamp(0.0, MAX_TRACK_GAIN));
        }
        AudioCommand::SetTrackPan { track, pan } => {
            with_track(song, track, |t| t.pan = pan.clamp(-1.0, 1.0));
        }
        AudioCommand::SetTrackMuted { track, muted } => {
            with_track(song, track, |t| t.muted = muted);
        }
        AudioCommand::SetTrackSolo { track, solo } => {
            with_track(song, track, |t| t.solo = solo);
        }
        // 内蔵チャンネルストリップ (docs/plan_channel_strip.md)。IPC は信頼境界
        // なので、各パラメータを可動範囲へ丸めてから載せる。丸め方の SSoT は
        // `common::model::channel_strip` の `ParamRange`。
        AudioCommand::SetTrackStrip { track, strip } => {
            with_track(song, track, |t| t.strip = sanitize_strip(strip));
        }
        AudioCommand::SetTrackArmed { track, armed } => {
            with_track(song, track, |t| t.armed = armed);
        }
        AudioCommand::SetSendGain { track, send_id, gain } => {
            with_send(song, track, send_id, |s| {
                // track / master と同じ +6 dB 上限を共有 (r.md #11 sibling)。
                s.gain = gain.clamp(0.0, MAX_TRACK_GAIN);
            });
        }
        AudioCommand::SetSendEnabled { track, send_id, enabled } => {
            with_send(song, track, send_id, |s| s.enabled = enabled);
        }
        AudioCommand::SetSongBpm { bpm } => song.bpm = bpm.clamp(1.0, 400.0),
        AudioCommand::SetSongTimeSigNumerator { num } => song.time_sig.0 = num.clamp(1, 32),
        _ => return false,
    }
    true
}

/// IPC 境界のクランプ: 各パラメータを可動範囲へ丸める。
///
/// 範囲外の周波数や負の時定数がそのまま係数計算に入ると、フィルタが発散して
/// **NaN が master バスまで伝播する** (一度混ざると停止するまで無音)。
fn sanitize_strip(mut strip: common::model::ChannelStrip) -> common::model::ChannelStrip {
    use common::model::{CompParam, EqBand, EqParam};
    for band in EqBand::ALL {
        for param in [EqParam::Freq, EqParam::Gain, EqParam::Q] {
            let v = strip.eq.param(band, param);
            strip.eq.set_param(band, param, if v.is_finite() { v } else { 0.0 });
        }
    }
    for param in [
        CompParam::Threshold,
        CompParam::Ratio,
        CompParam::Attack,
        CompParam::Release,
        CompParam::Makeup,
        CompParam::ScFreq,
    ] {
        let v = strip.comp.param(param);
        strip.comp.set_param(param, if v.is_finite() { v } else { 0.0 });
    }
    strip
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

        assert!(apply(&AudioCommand::SetTrackVolume { track: 3, volume: 99.0 }, &mut song));
        assert_eq!(song.tracks[0].volume, MAX_TRACK_GAIN);
        assert!(apply(&AudioCommand::SetTrackPan { track: 3, pan: -9.0 }, &mut song));
        assert_eq!(song.tracks[0].pan, -1.0);
        assert!(apply(
            &AudioCommand::SetSendGain { track: 3, send_id: 1, gain: -5.0 },
            &mut song
        ));
        assert_eq!(song.tracks[0].sends[0].gain, 0.0);
        assert!(apply(&AudioCommand::SetSongBpm { bpm: 0.0 }, &mut song));
        assert_eq!(song.bpm, 1.0);
        assert!(apply(&AudioCommand::SetSongTimeSigNumerator { num: 0 }, &mut song));
        assert_eq!(song.time_sig.0, 1);

        // 存在しない id は何も壊さない。
        assert!(apply(&AudioCommand::SetTrackMuted { track: 99, muted: true }, &mut song));
        assert!(!song.tracks[0].muted);
        // 値のみ更新でないコマンドは扱わない。
        assert!(!apply(&AudioCommand::Play, &mut song));
    }


    /// IPC は信頼境界。壊れた値 (NaN / 範囲外) が係数計算へ入るとフィルタが発散し、
    /// **NaN が master まで伝播して停止するまで無音**になる。ここで必ず潰す。
    #[test]
    fn ストリップの値は_ipc_境界で丸められる() {
        use common::model::{ChannelStrip, EqBand, EqParam};

        let mut song = Song::default();
        song.tracks.push(Track { id: 7, ..Track::default() });

        let mut strip = ChannelStrip::default();
        strip.eq.hmf.freq_hz = f32::NAN;
        strip.eq.hmf.gain_db = 999.0;
        strip.comp.attack_ms = -5.0;
        strip.comp.sc_freq_hz = 1.0; // 20Hz 未満は OFF へ

        assert!(apply(&AudioCommand::SetTrackStrip { track: 7, strip }, &mut song));
        let got = song.tracks[0].strip;
        assert!(got.eq.param(EqBand::Hmf, EqParam::Freq).is_finite());
        assert!((got.eq.param(EqBand::Hmf, EqParam::Gain) - 15.0).abs() < 1e-6);
        assert!((got.comp.attack_ms - 0.1).abs() < 1e-6, "{}", got.comp.attack_ms);
        assert_eq!(got.comp.sc_freq_hz, 0.0);
    }
}
