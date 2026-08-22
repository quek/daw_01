// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

// 「曲を開いて再生すると先頭 track しか鳴らない」の end-to-end 検証用スクリプト。
//
// GUI の File→Open と同じ経路 (loadSongFile) で実プロジェクトを開き、そのまま
// 再生する。daw_audio の debug ビルドは 1 秒ごとに `engine heartbeat` を
// `track_peaks=[(peak_l, peak_r, effective_mute), ...]` 付きで吐くので、
// ログを見れば **どのトラックが実際に音を出したか** が判る。
//
//   cargo run -p daw_gui --features script -- \
//     --script daw_gui/tests/scripts/open_and_play_peaks.js \
//     --arg song=<path to .daw>
//
// 期待: 全トラックの peak が非ゼロ。RtBundle の coalescing が topology
// bundle の schedule を捨てていた頃は、起動時 default song (1 track) の
// schedule が残るため track 0 以外の peak が 0 のままだった。
const song = daw.scriptArgs.song;
if (!song) {
  throw new Error("--arg song=<path to .daw> is required");
}
// 実機と同じ条件を作る: CPAL stream が定常運転 (10ms 周期の callback) に
// 入ってから曲を開く。stream 開始直後は callback が前詰めで連続発火するため、
// LoadSong と直後の OpenPluginShmem が別 callback に分かれてしまい、
// 本来の「同一 buffer 周期で coalescing される」条件を再現できない。
daw.sleepMs(2000);
daw.loadSongFile(song);
// plugin (VST3 / builtin) の load と audio source の decode が終わるまで待つ。
daw.sleepMs(4000);
daw.play();
// 全トラックのクリップが 1 度は鳴る長さを流す (曲の後半にしか出ない
// トラックがあるので、頭から通しで再生する)。
daw.sleepMs(Number(daw.scriptArgs.playMs || 25000));
daw.stop();
