// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! S3b-1: AppData state group (MediaState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::sync::{Arc, Mutex};

use crate::app::AssetDecodeStaging;
use crate::audio_source_cache::AudioSourceCache;

pub struct MediaState {
    /// Decoded sample buffers for `Song.audio_sources`, keyed by
    /// `AudioSourceId`. Filled lazily on import (Phase 1 PR3). The
    /// audio engine maintains its own independent cache — file-backed
    /// sources are decoded twice (once per process) to keep IPC lean
    /// (`docs/plan_audio_clip.md` §6.1 / §8.3).
    pub audio_source_cache: AudioSourceCache,
    /// Video thumbnail RGBA8 staging area, keyed by `VideoSourceId`.
    /// Populated by `action_import_video` (P3.4); drained by the
    /// runner (P3.5) which calls `Renderer::create_texture` +
    /// `upload_texture_rgba` and inserts the resulting `TextureHandle`
    /// into [`Self::video_texture_cache`]. After a successful upload
    /// the entry here is dropped (= the texture lives in GPU memory).
    /// `(width, height, rgba)`; rgba length is `width * height * 4`.
    pub video_thumbnail_rgba:
        std::collections::HashMap<common::model::VideoSourceId, (u32, u32, std::sync::Arc<Vec<u8>>)>,
    /// `VideoSourceId`s queued for GPU texture upload. The runner
    /// drains this each frame.
    pub pending_thumbnail_uploads: Vec<common::model::VideoSourceId>,
    /// v13 (`docs/plan_image_overlay.md` §P2): Image BGRA8 staging
    /// area, keyed by `ImageSourceId`. Populated by
    /// `action_import_image`; drained by the runner (P3) which calls
    /// `Renderer::create_texture_bgra` + `upload_texture_bgra` and
    /// inserts the resulting `TextureHandle` into
    /// [`Self::image_texture_cache`]. After upload the entry here is
    /// dropped (= the texture lives in GPU memory). `(width, height,
    /// bgra)`; bgra length is `width * height * 4`.
    pub image_source_bgra: std::collections::HashMap<
        common::model::ImageSourceId,
        (u32, u32, std::sync::Arc<Vec<u8>>),
    >,
    /// v13: `ImageSourceId`s queued for GPU texture upload. Drained by
    /// the runner each frame.
    pub pending_image_uploads: Vec<common::model::ImageSourceId>,
    /// プロジェクトロード時の audio / image background decode の staging。
    /// `Some` の間は streaming load 進行中 (= 再生 gate + 進捗 overlay 表示)。
    /// `begin_asset_decode` で `Some`、 全件取り込みで `None`。
    pub asset_decode: Option<Arc<Mutex<AssetDecodeStaging>>>,
    /// 進捗 overlay 用 (done, total)。 draw で Mutex を取らずに済むよう
    /// `on_asset_decode_tick` がプレーン値で更新する。 `None` = 非表示。
    pub load_progress: Option<(usize, usize)>,
    /// 進捗 overlay のラベル (ロード / 走査で文言が違う)。`load_progress` が
    /// `Some` のときだけ使われる。
    pub load_progress_label: &'static str,
}
