//! GUI からのランチャー操作 (`AudioCommand` の launcher 系) を受ける口。
//!
//! `recv_loop` (IPC スレッド) から呼ばれる。ここで **audio thread が読む形**
//! (`EngineCommand` / `SharedState` の atomic) へ落とすので、RT 側は
//! 「積まれた `LaunchRequest` を drain する」だけで済む。
//!
//! `main.rs` は実コード 1,569 行の baseline 天井ちょうどなので、
//! **launcher の分岐を `recv_loop` の中へ書かない** (不変条件 9)。

use common::protocol::AudioCommand;

use super::quantize;
use super::runtime::LaunchRequest;
use super::RowKey;
use crate::engine::{EngineCommand, EngineShared};

/// この `AudioCommand` が launcher 系なら処理して `true`。
///
/// `SetGlobalLaunchQuantize` だけは `SharedState` の atomic に直接載せる
/// (毎 buffer 1 回読むスカラーなので、キューを通す意味が無い)。それ以外は
/// 発火の判断に `Song` が要るので audio thread へ渡す。
pub fn dispatch(
    cmd: AudioCommand,
    engine_shared: &EngineShared,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
) -> bool {
    let req = match cmd {
        AudioCommand::SetGlobalLaunchQuantize(q) => {
            engine_shared.global_launch_quantize.store(
                quantize::encode(q),
                std::sync::atomic::Ordering::Release,
            );
            tracing::info!(?q, "received SetGlobalLaunchQuantize");
            return true;
        }
        AudioCommand::LaunchCell { track_id, lane_id, clip_id, pressed } => {
            LaunchRequest::Cell { key: RowKey::lane(track_id, lane_id), clip_id, pressed }
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
    use common::model::LaunchQuantize;

    #[test]
    fn 量子化設定は_atomic_へ_それ以外は_audio_thread_へ() {
        let shared = EngineShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(dispatch(
            AudioCommand::SetGlobalLaunchQuantize(LaunchQuantize::Note {
                div: 8,
                triplet: false
            }),
            &shared,
            &tx
        ));
        assert!(rx.try_recv().is_err(), "量子化設定はキューを通さない");
        assert_eq!(
            quantize::decode(
                shared.global_launch_quantize.load(std::sync::atomic::Ordering::Acquire)
            ),
            LaunchQuantize::Note { div: 8, triplet: false }
        );

        // 行の宛先は安定 id で運ばれる (lane_id = 0 がトラック行)。
        assert!(dispatch(
            AudioCommand::LaunchCell { track_id: 3, lane_id: 0, clip_id: 9, pressed: true },
            &shared,
            &tx
        ));
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
        assert!(!dispatch(AudioCommand::Play, &shared, &tx));
    }
}
