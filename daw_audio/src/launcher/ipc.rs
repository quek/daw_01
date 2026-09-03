//! GUI からのランチャー操作 (`AudioCommand` の launcher 系) を受ける口。
//!
//! `recv_loop` (IPC スレッド) から呼ばれる。ここで **audio thread が読む形**
//! (`EngineCommand` / `SharedState` の atomic) へ落とすので、RT 側は
//! 「積まれた `LaunchRequest` を drain する」だけで済む。
//!
//! `main.rs` は実コード 1,569 行の baseline 天井ちょうどなので、
//! **launcher の分岐を `recv_loop` の中へ書かない** (不変条件 9)。

use common::protocol::AudioCommand;

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
    let req = match cmd {
        AudioCommand::LaunchCell { track_id, lane_id, clip_id, pressed } => {
            LaunchRequest::Cell { key: RowKey::lane(track_id, lane_id), clip_id, pressed }
        }
        AudioCommand::LaunchCellFrom { track_id, lane_id, clip_id, phase_beats } => {
            LaunchRequest::CellFrom { key: RowKey::lane(track_id, lane_id), clip_id, phase_beats }
        }
        AudioCommand::RephaseLauncherRows { phase_beats } => {
            LaunchRequest::RephaseRunning { phase_beats }
        }
        AudioCommand::LaunchScene { scene_id, pressed } => {
            LaunchRequest::Scene { scene_id, pressed }
        }
        AudioCommand::StopRow { track_id, lane_id } => {
            LaunchRequest::StopRow { key: RowKey::lane(track_id, lane_id) }
        }
        AudioCommand::StopAllRows => LaunchRequest::StopAll,
        AudioCommand::SwitchRowToArranger { track_id, lane_id } => {
            LaunchRequest::RowToArranger { key: RowKey::lane(track_id, lane_id) }
        }
        AudioCommand::SwitchAllToArranger => LaunchRequest::AllToArranger,
        _ => return false,
    };
    // 送れなかった (= audio thread が居ない) ときは黙って捨てる — 起動直後 /
    // 終了処理中で、そもそも鳴らす相手が居ない。
    let _ = cmd_tx.send(EngineCommand::Launch(req));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 発火系の_command_は_audio_thread_へ渡る() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // 行の宛先は安定 id で運ばれる (lane_id = 0 がトラック行)。
        assert!(dispatch(AudioCommand::LaunchCell { track_id: 3, lane_id: 0, clip_id: 9, pressed: true }, &tx));
        let got = rx.try_recv().expect("audio thread へ渡る");
        match got {
            EngineCommand::Launch(LaunchRequest::Cell { key, clip_id, pressed }) => {
                assert_eq!(key, RowKey::track(3));
                assert_eq!(clip_id, 9);
                assert!(pressed);
            }
            other => panic!("{other:?} が来た"),
        }

        // launcher 以外は素通り (recv_loop の他の arm が処理する)。
        assert!(!dispatch(AudioCommand::Play, &tx));
    }
}
