// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `PROTOCOL_FINGERPRINT` の焼き込み (`docs/plan_arch_refactor.md` §3)。
//!
//! wire (named pipe の bincode / shmem の repr(C) レイアウト) を渡る型を定義
//! する source file 群の content hash (FNV-1a 64bit) を計算し、
//! `DAW_PROTOCOL_FINGERPRINT` 環境変数として `protocol.rs` へ渡す。
//! Hello handshake で照合することで「ビルド世代の混在」(= 片方だけ rebuild
//! した exe が decode 失敗 → 無音 → respawn loop) を接続時に明示検出する。
//!
//! content hash なので、これらのファイルに変更が無い再ビルドでは値が変わらず
//! 誤検知しない。逆にコメント 1 字の変更でも値は変わるが、それは「安全側に
//! 倒れて rebuild を促す」だけで運用上の害はない。

use std::fs;

/// wire を渡る型を定義するファイル群。ここに列挙し忘れると「protocol を
/// 変えたのに fingerprint が変わらない」型の穴になるので、protocol.rs から
/// 参照される型を新しいファイルへ切り出したら必ず追加すること。
const WIRE_SOURCES: &[&str] = &[
    "src/wire.rs",
    "src/protocol.rs",
    "src/model.rs",
    "src/model/track.rs",
    "src/model/modulation.rs",
    "src/model/content.rs",
    "src/model/automation.rs",
    "src/loudness_report.rs",
    // Song.scale_changes が持つ型 (Scale / KeySignature 等) の定義。model.rs から
    // 参照され `LoadSong` の wire に載るのに未登録だった (r.md #54 レビューで発覚)。
    "src/scale.rs",
    "src/plugin_metadata.rs",
    "src/plugin_format.rs",
    "src/port_config.rs",
    "src/process_data.rs",
    "src/audio_bridge.rs",
    "src/metrics_bridge.rs",
    "src/scope_bridge.rs",
    "src/worker_bridge.rs",
    "src/plugin_ref.rs",
];

fn main() {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for path in WIRE_SOURCES {
        println!("cargo:rerun-if-changed={path}");
        let bytes = fs::read(path).unwrap_or_else(|e| panic!("build.rs: read {path}: {e}"));
        for b in path.as_bytes().iter().chain(bytes.iter()) {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
        }
    }
    println!("cargo:rustc-env=DAW_PROTOCOL_FINGERPRINT={hash:016x}");
}
