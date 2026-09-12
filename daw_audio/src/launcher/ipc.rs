//! GUI からのランチャー操作 (`AudioCommand` の launcher 系) を受ける口。
//!
//! `recv_loop` (IPC スレッド) から呼ばれる。ここで **audio thread が読む形**
//! (`EngineCommand` / `SharedState` の atomic) へ落とすので、RT 側は
//! 「積まれた `LaunchRequest` を drain する」だけで済む。
//!
//! `main.rs` は実コード 1,569 行の baseline 天井ちょうどなので、
//! **launcher の分岐を `recv_loop` の中へ書かない** (不変条件 9)。

use common::protocol::{AudioCommand, ProjectKey};

use super::runtime::LaunchRequest;
use super::RowKey;
use crate::engine::EngineCommand;

/// この `AudioCommand` が launcher 系なら処理して `true`。
///
/// 発火の判断には `Song` が要る (どのセルがどの列に居るか) ので、
/// すべて audio thread のキューへ渡す。**グローバルローンチ量子化はここを通らない** —
/// `Song.global_launch_quantize` が SSoT で、`LoadSong` に載って届く
/// (値の経路を 2 本持つと、どちらが効いたか分からなくなる)。
pub fn dispatch(
    cmd: AudioCommand,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
) -> bool {
    let (project, req): (ProjectKey, LaunchRequest) = match cmd {
        AudioCommand::LaunchCell { project, track_id, lane_id, clip_id, pressed, immediate } => {
            (project, LaunchRequest::Cell { key: RowKey::lane(track_id, lane_id), clip_id, pressed, immediate })
        }
        AudioCommand::LaunchCellFrom { project, track_id, lane_id, clip_id, phase_beats } => {
            (project, LaunchRequest::CellFrom { key: RowKey::lane(track_id, lane_id), clip_id, phase_beats })
        }
        AudioCommand::RephaseLauncherRows { project, phase_beats } => {
            (project, LaunchRequest::RephaseRunning { phase_beats })
        }
        AudioCommand::LaunchScene { project, scene_id, pressed, immediate } => {
            (project, LaunchRequest::Scene { scene_id, pressed, immediate })
        }
        AudioCommand::StopRow { project, track_id, lane_id, immediate } => {
            (project, LaunchRequest::StopRow { key: RowKey::lane(track_id, lane_id), immediate })
        }
        AudioCommand::StopAllRows { project, immediate } => (project, LaunchRequest::StopAll { immediate }),
        AudioCommand::SwitchRowToArranger { project, track_id, lane_id } => {
            (project, LaunchRequest::RowToArranger { key: RowKey::lane(track_id, lane_id) })
        }
        AudioCommand::SwitchAllToArranger { project } => (project, LaunchRequest::AllToArranger),
        _ => return false,
    };
    // 送れなかった (= audio thread が居ない) ときは黙って捨てる — 起動直後 /
    // 終了処理中で、そもそも鳴らす相手が居ない。
    let _ = cmd_tx.send(EngineCommand::Launch { project, req });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 発火系の_command_は_audio_thread_へ渡る() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // 行の宛先は安定 id で運ばれる (lane_id = 0 がトラック行)。
        assert!(dispatch(AudioCommand::LaunchCell { project: ProjectKey(4), track_id: 3, lane_id: 0, clip_id: 9, pressed: true, immediate: false }, &tx));
        let got = rx.try_recv().expect("audio thread へ渡る");
        match got {
            EngineCommand::Launch { project, req: LaunchRequest::Cell { key, clip_id, pressed, .. } } => {
                assert_eq!(project, ProjectKey(4));
                assert_eq!(key, RowKey::track(3));
                assert_eq!(clip_id, 9);
                assert!(pressed);
            }
            other => panic!("{other:?} が来た"),
        }

        // launcher 以外は素通り (recv_loop の他の arm が処理する)。
        assert!(!dispatch(AudioCommand::Play { project: ProjectKey(4) }, &tx));
    }
}
