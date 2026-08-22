//! テスト fixture を作る `ffmpeg` CLI の解決と、そこで使うエンコーダの SSoT。
//!
//! **同梱している pin 済みビルドだけを使う**。以前は 3 つの test モジュール
//! (`import_video` / `render_video` / `video_playback`) が同じ `locate_ffmpeg` を
//! 各自に持ち、いずれも **PATH の ffmpeg** を探して fixture を `-c:v libx264` で
//! 符号化していた。つまりテストは「開発機に入っている GPL 有効ビルド」に暗黙依存し、
//! 頒布する `third_party/ffmpeg` (BtbN `win64-lgpl-shared`、`--disable-libx264`) では
//! 1 件も通らない状態だった。NOTICE は「同梱 FFmpeg は GPL-only な外部ライブラリを
//! 一切使わない」と書いているので、主張と実態が食い違っていた (公開前監査で発覚)。
//!
//! PATH へのフォールバックは **意図的に持たない**。ここで探すビルドを 1 つに固定すると
//! 「テストが通った = 頒布するビルドで完結している」が構造的に成立する。
//! 見つからないときはテスト側が SKIP する (`make fetch-ffmpeg` で取得できる)。

use std::path::{Path, PathBuf};

/// fixture の符号化に使う H.264 エンコーダ。
///
/// pin したビルドの configure は `--disable-libx264 --disable-libx265` かつ
/// `--enable-libopenh264` なので、H.264 を出せるソフトウェアエンコーダはこれ
/// (`ffmpeg -encoders` で確認できる)。コンテナ / コーデックは mp4 / H.264 のままなので、
/// デコード側を検証するというテストの意図は変わらない。
///
/// ハードウェアエンコーダ (`h264_nvenc` / `h264_qsv` / `h264_amf` / `h264_mf`) は
/// GPU / ドライバの有無で結果が変わるので fixture には使わない。
pub const H264_ENCODER: &str = "libopenh264";

/// 同梱の pin 済み `ffmpeg` 実行ファイル。未取得なら `None` (呼び出し側は SKIP)。
#[must_use]
pub fn locate_ffmpeg() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    // CARGO_MANIFEST_DIR = <repo>/daw_gui。third_party は repo 直下。
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("third_party")
        .join("ffmpeg")
        .join("bin")
        .join(exe);
    pinned.is_file().then_some(pinned)
}

/// `locate_ffmpeg` の SKIP 理由 (テストごとに文面を書き分けない)。
#[must_use]
pub fn skip_reason(test_name: &str) -> String {
    format!(
        "{test_name}: 同梱の third_party/ffmpeg が無いので SKIP \
         (`make fetch-ffmpeg` で取得できる)"
    )
}
