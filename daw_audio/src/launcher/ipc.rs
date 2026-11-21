//! GUI からのランチャー操作 (`AudioCommand` の launcher 系) を受ける口。
//!
//! `recv_loop` (IPC スレッド) から呼ばれる。ここで **audio thread が読む形**
//! ([`LaunchRequest`]) へ落とすので、RT 側は「積まれた `LaunchRequest` を drain する」
//! だけで済む。
//!
//! `main.rs` は実コード 1,569 行の baseline 天井ちょうどなので、
//! **launcher の分岐を `recv_loop` の中へ書かない** (不変条件 9)。

use common::protocol::AudioCommand;

use super::runtime::LaunchRequest;
use super::RowKey;

/// launcher 系の `AudioCommand` を audio thread 用の操作へ落とす。launcher 系でなければ `None`。
///
/// 発火の判断には `Song` が要る (どのセルがどの列に居るか) ので、
/// すべて audio thread のキューへ渡す。**グローバルローンチ量子化はここを通らない** —
/// `Song.global_launch_quantize` が SSoT で、`LoadSong` に載って届く
/// (値の経路を 2 本持つと、どちらが効いたか分からなくなる)。
#[must_use]
pub fn request(cmd: &AudioCommand) -> Option<LaunchRequest> {
    Some(match *cmd {
        AudioCommand::LaunchCell { track_id, lane_id, clip_id, pressed, immediate, .. } => {
            LaunchRequest::Cell { key: RowKey::lane(track_id, lane_id), clip_id, pressed, immediate }
        }
        AudioCommand::LaunchCellFrom { track_id, lane_id, clip_id, phase_beats, .. } => {
            LaunchRequest::CellFrom { key: RowKey::lane(track_id, lane_id), clip_id, phase_beats }
        }
        AudioCommand::RephaseLauncherRows { phase_beats, .. } => LaunchRequest::RephaseRunning { phase_beats },
        AudioCommand::LaunchScene { scene_id, pressed, immediate, .. } => {
            LaunchRequest::Scene { scene_id, pressed, immediate }
        }
        AudioCommand::StopRow { track_id, lane_id, immediate, .. } => {
            LaunchRequest::StopRow { key: RowKey::lane(track_id, lane_id), immediate }
        }
        AudioCommand::StopAllRows { immediate, .. } => LaunchRequest::StopAll { immediate },
        AudioCommand::SwitchRowToArranger { track_id, lane_id, .. } => {
            LaunchRequest::RowToArranger { key: RowKey::lane(track_id, lane_id) }
        }
        AudioCommand::SwitchAllToArranger { .. } => LaunchRequest::AllToArranger,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::protocol::ProjectKey;

    #[test]
    fn 発火系の_command_は_audio_thread_用の操作になる() {
        // 行の宛先は安定 id で運ばれる (lane_id = 0 がトラック行)。
        let cmd = AudioCommand::LaunchCell {
            project: ProjectKey(4),
            track_id: 3,
            lane_id: 0,
            clip_id: 9,
            pressed: true,
            immediate: false,
        };
        assert_eq!(
            request(&cmd),
            Some(LaunchRequest::Cell { key: RowKey::track(3), clip_id: 9, pressed: true, immediate: false })
        );

        // launcher 以外は素通り (recv_loop の他の arm が処理する)。
        assert_eq!(request(&AudioCommand::Play { project: ProjectKey(4) }), None);
    }
}
