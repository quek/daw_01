<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: 動画デコードの libav 一本化 — Media Foundation 全撤去

## 背景 / 動機

2026-07-06、`daw_gui` が動画再生中に **combase.dll のアクセス違反 (COM を NULL/解放済み
ポインタ経由で呼んだ)** で落ちた。WER ダンプ (`daw_gui.exe.6084.dmp`) と落ちたスレッドの
モジュール単位スタック走査 (ntdll + combase + daw_gui.exe のみ、MF/ドライバフレーム皆無) から、
**プレビューの Media Foundation デコード経路が真因**と確定 (詳細な調査手順は memory
`reference_windows_minidump_forensics`)。

トリガはプロジェクト `scratch/20260512` の 1080p 動画で、Media Foundation が HW デコードできず
(`CopyDecodedFrame 0x80004005 = E_FAIL`) 毎回 libav へフォールバックしている点。

### なぜ patch でなく破壊 + 再構築か (大原則)

「MF 破棄経路にレース対策 (Flush/Shutdown) を足す」は **影響範囲を狭く patch する妥協案**で、
CLAUDE.md 冒頭の大原則に反する。理想から問い直すと、誤りは **MF という第二のデコーダを
抱えていること自体**:

- **Windows 専用** (プロジェクトは Linux 配慮を掲げる)。
- **フォーマット制限**でユーザーの実素材を毎回デコードできず libav に落ちる = 固有価値ゼロ。
- **COM / D3D11 → 共有ハンドル → wgpu(Vulkan) import** という別 API 間の無駄なバウンス。
  keyed-mutex / NT handle 寿命 / cross-apartment COM 解放レースの温床 (今回のクラッシュを含め
  検出されたバグは 5 種)。
- **SSoT 違反**: 「MF で試す → 失敗検出 → 切替」の二重経路 + 判定ヒューリスティックが常時稼働。

**export は既に libav 一本** (`libav_decoder.rs` → rsmpeg SW decode → swscale → BGRA8 → wgpu)。
preview だけが MF に残っている。理想 = **デコードを libav 一本に統一し、MF をどこにも残さない**。

## 決定: 単一 libav エンジン + BGRA シンク

**最終形 = preview と export が共有する単一 libav (rsmpeg) デコードエンジン。既定は SW デコード、
BGRA8 出力、wgpu の BGRA テクスチャへアップロード。** これは妥協ではなく、shipped stack 上での
理想である (下記の通り「より高度」な経路は bindings で到達不能、または preview 解像度で無益)。

### GPU 受け渡しの三択と選定 (一次情報で確定)

wgpu-hal 29.x / rsmpeg 0.17 (ffmpeg 7.1) を実ソースで精査した結論:

1. **zero-copy Vulkan VkImage import** — **却下 (到達不能)**。
   - wgpu 側は準備済 (`wgpu_hal::vulkan::Device::texture_from_raw` が存在、`TextureMemory::External`)。
   - **決定的ブロッカー: rsmpeg 0.17 が Vulkan hwcontext を一切 expose していない。**
     `rusty_ffmpeg` の bindgen whitelist は `hwcontext_vulkan.h` をコメントアウトしており
     (`build.rs:113`)、`AVVulkanDeviceContext` / `AVVulkanFramesContext` / `AVVkFrame` が FFI に
     存在しない → ffmpeg にデバイスを渡すことも、デコードされた `VkImage` を受け取ることも不可。
     `rusty_ffmpeg` を fork するか `#[repr(C)]` を手書き (ffmpeg point release で layout 崩壊) が必要。
   - さらに wgpu-hal 29 は **`VkSamplerYcbcrConversion` を有効化していない** (`adapter.rs:418` で
     コメントアウト) → HW ycbcr サンプリング不可。NV12/P010 を 2 plane view にして BT.709 を
     WGSL で手計算する必要。external-semaphore/fence 拡張も皆無 = cross-API 同期を生実装する必要。
   - 1080p では帯域上の利得ゼロ、かつ NVIDIA の Vulkan decode にはドライバハングの既知報告。
     **明示的に却下する。**

2. **HW-decode + `av_hwframe_transfer_data` (GPU→CPU readback) + BGRA upload** — **却下 (readback trap)**。
   rsmpeg 0.17 で到達できる唯一の HW 出力経路だが、高価な PCIe download を残したまま HW 複雑性を
   足すだけ (4K で数 ms/frame、しばしば純 SW ≧)。

3. **SW decode + BGRA** — **採用**。backend 非依存 (wgpu が Vulkan でも DX12 でも同一、shared-device も
   hal barrier も per-driver validation も無い)。export 経路で実証済み。1080p H.264 SW decode は
   **実時間の 8〜20 倍**の余裕。

### SW 既定の正当性

- ユーザーの実素材 **10-bit H.264 High10 は Ampere で HW デコード不可** (NVDEC app note)。どのみち SW。
- SW は全フォーマットの universal fallback (ドライバ癖に対する安全網も兼ねる)。
- HW opt-in は **構造だけ用意して speculative には積まない** (KISS)。`avcodec_get_hw_config` で HW
  デコーダの有無は分かるが、readback trap のため初期実装では配線しない。将来 4K で実測ニーズが
  出たら per-codec opt-in を同じ BGRA シンクの背後に足せる形にしておく。

### 将来の in-place 最適化 (今は採らない)

NV12/P010 の plane-upload + BT.709-in-WGSL は upload 帯域を約 2.6× 削減 (12bpp vs 32bpp) し CPU の
RGB 変換も省ける。**同じテクスチャシンクの背後の最適化**として、4K preview の upload が実測ボトルに
なった時のみ導入。1080p では domain-agnostic renderer に YUV shader と 2-plane texture kind を
足す価値が無いので初期採用しない。

## 削除リスト (MF / D3D11 表面)

| ファイル | 削除 | 保持 |
|---|---|---|
| `daw_gui/src/video_playback.rs` | `ReaderEntry` / `SharedSlot` / `SharedPool` / `D3D11WmfState` / `try_init_d3d11` / `create_reader_for_source` / `seek_reader` / `read_sample_only` / `sample_to_frame` / `write_to_shared_pool_slot` / `create_shared_pool` / `SendHandle` / `DecodedFrame::Shared` / 全 `MediaFoundation`・`Dxgi`・`Direct3D11` import | 純 timeline ロジック (`active_sources_at` / `active_source_at` / `ActiveVideoFrame` / fade/alpha) / `bgra_to_rgba` |
| `daw_gui/src/video_playback_worker.rs` | MTA COM apartment init (`CoInitializeEx` / `ensure_mf_startup`) | worker スレッド構造 (Condvar coalescing / mpsc / shutdown / lookahead) |
| `ui/crates/renderer/src/device.rs` | `create_texture_from_d3d11_shared_handle` / `import_d3d11_shared_handle_dx12` / `import_d3d11_shared_handle_vulkan` / `RendererError::{WrongBackend, VulkanImportFailed}` / `vulkan_external_memory_supported` 判定 | `create_texture_bgra` / `upload_texture_bgra` / `TextureKind::Bgra` (generic = invariant #8 維持) |
| `daw_gui/src/render_video.rs` | 残存 `MFStartup` 呼び出し + テストの WMF 再読込 | libav encoder 本体 (既に libav) |
| `daw_gui/src/import_video.rs` | `IMFSourceReader` による metadata/thumbnail/audio 抽出、`ensure_mf_startup` | 公開 API シグネチャ (`extract_metadata` / `extract_thumbnail` / 音声抽出) — 中身を libav へ |

**`ensure_mf_startup` は probe/thumbnail/audio-demux/playback が共有している。全部を libav へ移して
初めて MF を完全撤去できる** (片方だけ残すと MFStartup が生き残り「削除は名目だけ」になる)。

## モジュール構成 (as-built)

MF 中核を削った後は各ファイルが god-file budget (3,000 行) に十分収まるため、`video/`
ディレクトリへの再編は行わず **既存ファイルを in-place で slim 化** した (churn / risk 最小化。
アーキテクチャの理想 = libav 一本・MF 撤去は達成済みで、ディレクトリ分割は cosmetic なため):

- `video_playback.rs` (1,946 → ~700 行) — song → active-frames の純 timeline クエリ + `DecodedFrame`
  (BGRA struct) + 単一 libav エンジンを owner する薄い `VideoPlaybackEngine` + `bgra_to_rgba`。
- `libav_decoder.rs` — **単一 libav エンジン** (`LibavVideoDecoder` / `SourceDecoder` / `DecodedBgra`)。
  preview (`new_preview` = 960px 縮小) と export (`new` = native) の**唯一のデコーダ**。デコードの SSoT。
- `video_playback_worker.rs` — preview 用背景デコードスレッド (mpsc、COM apartment 不要)。ring は
  BGRA では 1 スロットに退化 (center フレームのみ decode。`DecodedRing`/`RingSlot` の器は drain 側
  互換のため維持)。
- `import_video.rs` — libav による probe (avformat) / thumbnail (単一 libav エンジン再利用) /
  音声抽出 (avcodec decode + swresample → Float32 → hound WAV)。

### 保持シーム (再構築で維持する契約)

- `active_sources_at(song, playhead) -> Vec<ActiveVideoFrame>` (純モデルクエリ) → `worker.request` →
  worker が frame をデコード → `preview.upload_frame` → composite が texture を sample。
- **`DecodedFrame` は BGRA 単一 payload に collapse** (`Shared` variant 削除)。lookahead ring は
  BGRA では「1-frame-latest」なので `PREVIEW_RING_SIZE` の per-slot 独立テクスチャ機構は不要化 →
  per-source 単一テクスチャに簡素化 (KISS)。
- `preview.upload_frame` / `upload_texture_bgra` はそのまま (BGRA アップロードは既存経路)。

## 性能チェックポイント (実装前の唯一の de-risk)

敵対的検証が指摘した唯一のリスク: **crossfade / multi-track = 同一 playhead で N 本同時デコード**。
export は offline/freewheel (壁時計デッドライン無し) だが preview は実時間デッドライン + scrub seek +
audio engine が別デッドラインを持つ。「8〜20× 実時間」は 1080p **1 本**の値。

→ **実装の最初のステップで、ユーザーの実素材 (10-bit High10) の 2 本 crossfade + scrub を libav SW で
デコードし持続 frame time を実測**してから module split を確定する。もし 2 本 10-bit が持続不能なら、
緩和策は「per-source デコードスレッド」「frame cache」「scrub 中は keyframe に落とす」等 (SW の正しさは
不変、pacing の問題)。現行 MF 経路も単一 worker で per-source デコードしていたので libav SW は同等以上。

## アーキテクチャ不変条件との整合

- **#2 wire は blob-less**: 変更なし (video frame は IPC を渡らず daw_gui 内で完結)。
- **#6 live と export は同じ render**: むしろ強化 — デコードも single engine で SSoT 化。
- **#8 daw-ui core はドメイン知識を持たない**: renderer は「BGRA 矩形を upload」だけ残す。D3D11/MF/
  backend 分岐を削除するので invariant がより綺麗になる。
- **#9 god-file budget**: `video/` 分割で全ファイル 3,000 行以内。現 `video_playback.rs` 1,946 行は解体。
- **RT 境界**: 全経路は GUI worker / export loop 上。audio RT スレッドは動画に触れない。BGRA scratch は
  再利用 (現 `libav_decoder.rs` の pattern) で hot loop の `Vec::new()` 無し。

## 検証計画

- `make check` / `make clippy` / `make test` green。
- **実機 (必須、build/test/clippy では捕まらない)**: `daw_gui` を自分で起動し `scratch/20260512` を再生 →
  ① クラッシュ再発しないこと ② preview が正しく描画 ③ 2 本 crossfade + scrub で持続 real-time。
- import 経路: 動画 import で thumbnail / 寸法 / fps / 長さ / 音声抽出が MF 時と一致すること。
- commit 前にユーザー sign-off (`feedback_confirm_before_commit`)。

## スコープ確認 (唯一の人間判断)

デコード architecture に未解決の設計 fork は無い (研究が確定)。唯一の判断は **スコープ**:
import_video.rs の MF (thumbnail/metadata/audio 抽出) も**今**移植するか、preview/decode の MF だけ
先に消すか。

**推奨: 今すべて移植する。** 目標は MF を「削除」すること。import 経路にも同じ COM アクセス違反
クラッシュ面が存在し、同じ 10-bit ファイルで落ち得る。deferする と MFStartup が生き残り第二の MF
入口が残る = 削除が名目だけになる。「中途状態を残さない・エンジン一本」の最終形として一括撤去する。
