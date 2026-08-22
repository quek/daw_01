<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: Video export を in-process libav (rsmpeg + NVENC) に置き換える

最終更新: 2026-06-06。根拠リサーチ: `~/.claude/.../wf_f3b6750f-6b4`（5 agent、引用付き）。

## 実装状況（2026-06-06）

| 項目 | 状態 | commit |
|---|---|---|
| Phase 1: export encode を NVENC 化 | ✅ 完了 | `6e3ae59` |
| Phase 3 (export decode): 10-bit を in-process libav に | ✅ 完了（10-bit 欠落バグ修正） | `422ab8c` |
| Phase 3 (preview decode): preview も libav 化、**ffmpeg.exe 完全廃止** | ✅ 完了 | `fb56e89` |
| NVENC フォールバック (libopenh264 / h264_mf + swscale) | ✅ 完了 | `aecc7e6` |
| Phase 2a: encode を専用スレッドに分離 (encode∥composite+readback) | ✅ 完了（21.0s→18.3s, ~13%） | `cd75a0a` |
| Phase 2b: async readback で composite∥readback∥encode を3段 overlap | ✅ 完了（gui_01 #077 の submit_readback/finish_readback を統合。18.3s→15.5s、元 21.0s から ~26%） | `73a1b2c` |

**全 Phase 完了**（2026-06-06）。production の ffmpeg.exe / MF sink writer / scalar nv12 / 同期 readback を
すべて撤去し、decode(libav SW)→ composite(wgpu)→ readback(async double-buffer)→ encode(NVENC worker)の
パイプラインに。実測 27.4s/1080p(10-bit + 立ち絵 + 音声)を **15.5s**(debug, ~1.8×RT)。
| 補足: Phase 3 は当初 Phase 2 より後の予定だったが、10-bit 欠落バグ対応で前倒し。decode/encode とも libav に統一済（production の ffmpeg.exe/MF sink writer は撤去、preview の 8-bit MF HW zero-copy のみ存置）。 |

実測: 27.4s/1080p（10-bit + 立ち絵 + 音声）が **video のみ 18.7s / 音声込み 21.0s**（debug, 1.3-1.5×RT）。
正しく合成（夜明け動画 + 立ち絵, 色 OK）・A/V 同期確認済。

## 0. 背景 / 実測診断（実データから始める）

ユーザー報告: `20260512.daw` の Video export が「とても時間がかかる」「タスクマネージャの
GPU **Video Decode 0%** のまま」。

ソース動画を ffprobe で実測:

| 項目 | 値 |
|---|---|
| codec / profile | **H.264 High 10（10-bit, `yuv420p10le`）** |
| 解像度 / fps / 長さ | 1920×1080 / 30fps / 58.7s（1763 frame） |

ffmpeg 8.0 で各ステージを実測:

| ステージ | 実測 | 意味 |
|---|---|---|
| CPU software decode（全クリップ） | **1.6s**（≈36×RT） | **デコードは全くボトルネックでない** |
| NVDEC `-hwaccel cuda` | **init 失敗** | 10-bit H.264 は Ampere の NVDEC 非対応（後述） |
| SW encode（libx264 medium 代理） | **13s** | 現状の MF SW エンコーダ相当の重さ |
| NVENC encode 自体 | ほぼ一瞬 | RTX 3070 の専用シリコンが完全に遊休 |

### 真のボトルネック（3 つ。全部「手を抜いた」箇所）

1. **ソフトウェア H.264 エンコーダ**: `MFCreateSinkWriterFromURL(url, None, None)` は
   `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` を opt-in していない → MF の **SW エンコーダ**
   → タスクマネージャ **Video Encode 0%**。
2. **毎フレーム同期 GPU readback**: `OffscreenRenderer::render_to_rgba` は
   `submit → map_async → device.poll(Wait) → recv` で毎フレーム GPU を全フラッシュして
   CPU を待たせる（`gui_01/crates/renderer/src/offscreen.rs:398-411`）→ **何も並列化されない**
   （CPU 3%・GPU 遊休なのに遅い＝ストール律速）。
3. **スカラー単スレッド `rgba_to_nv12`**: 200 万画素の二重ループをクリティカルパス上で。

→ `new_cpu_only()`（HW デコード無効化）は CLAUDE.md principle 違反（"影響範囲を狭く" で
選んだ）だが、**遅さの原因ではない**。デコードは 1.6s。

## 1. 理想の確定（リサーチで一度修正した）

当初「libav が全動画 I/O を所有（preview decode も D3D11VA に統一）」を理想とした。
**リサーチで playback-decode 統一の部分は理想でないと判明** → honest に修正:

### 1.1 なぜ preview を libav D3D11VA に作り直さないか（却下、引用付き）

- libav D3D11VA decode → wgpu は**現行 MF keyed-mutex path と同形**（簡単にならない）。
  decoder は `D3D11_BIND_DECODER` の array texture を返し、SRV 不可・共有不可なので、
  結局自前の `SHARED_NTHANDLE+KEYEDMUTEX` single-slice texture に `CopySubresourceRegion`
  する必要がある（＝今 MF でやっている copy と同じ）。さらに NV12→RGBA の compute shader
  を**追加で**書く必要がある（MF の video processor MFT が今は無料でやっている）。
  根拠: `libavutil/hwcontext_d3d11va.h`, https://dev.to/the_lone_engineer/ffmpeg-video-playback-in-native-wgpu-552a
- **決定的**: H.264 High10（10-bit）の HW デコードは **RTX 3070 (Ampere) に存在しない**。
  NVIDIA は 10-bit H.264 NVDEC を **Blackwell (Video Codec SDK 13.0)** で初めて追加。
  → このソースでは D3D11VA も必ず SW に落ちる（ユーザーの `-hwaccel cuda` 失敗と一致）。
  根拠: https://developer.nvidia.com/blog/nvidia-video-codec-sdk-13-0-powered-by-nvidia-blackwell/
- デコードは 1.6s でボトルネックでない → preview decode 最適化はそもそも投資先が間違い。

### 1.2 確定した理想（SSoT 的かつエンジニアリング的に正直）

- **encode と software decode を libav に統一**し、**ffmpeg.exe subprocess と MF sink writer
  を廃止**する（バックエンド 3→2）。
- **8-bit preview の MF D3D11 zero-copy decode path は維持**する。動いており、libav D3D11VA
  に置き換えても同じ複雑さ＋追加 shader＋このハードでは 10-bit を HW decode できない。
  維持は妥協ではなく、置き換えが理想でないから。
- **合成は wgpu OffscreenRenderer のまま据え置き**（preview/export byte parity は hard 要件）。

## 2. アーキテクチャ

```
[source decode] ── P3 で libav SW decode に統一（今は MF/ffmpeg.exe のまま）
        │ BGRA upload
        ▼
[wgpu OffscreenRenderer 合成]  ← 不変（preview と同一 Scene、byte parity）
        │ render_to_rgba（P2 で async/double-buffer 化）
        ▼ RGBA8
[libav encoder backend (rsmpeg)]
   h264_nvenc（RGBA 直入力 → GPU で YUV 変換）
   + native AAC（hound WAV から）
   + mp4 mux（libavformat）
```

### 2.1 RGBA → NVENC のピクセルフォーマット方針（重要な発見）

- `h264_nvenc` は **packed 8-bit RGB を直接入力**できる（`AV_PIX_FMT_BGR0`/`0BGR32` 等、
  `IS_RGB` macro）。NVENC の CUDA path が GPU で RGB→YUV 変換する → **scalar `rgba_to_nv12`
  を CPU から完全に消せる**。根拠: `libavcodec/nvenc.c` `ff_nvenc_pix_fmts[]`, `nvenc_map_buffer_format`。
- **色の落とし穴**: FFmpeg の nvenc.c は RGB 入力時に **BT.601 (BT470BG) + limited range を
  ハードコード**し `colorspace`/`color_range` を無視する。
  根拠: `nvenc_setup_h264_config`, https://ffmpeg.org/pipermail/ffmpeg-user/2023-March/056142.html
- **ただし現状の `rgba_to_nv12` も BT.601 limited（係数 66/129/25）** なので、RGBA 直入力
  （Path A）は**現 export の色挙動と一致**する → 一貫性あり。これを既定とする。
- 色厳密化（Path B）は将来 optional: swscale で RGBA→NV12 を BT.709 + 明示 range 変換し
  （`SwsContext` の `threads` でマルチスレッド化、Blender 実測 4.7s→0.7s/300f）、NV12 を
  NVENC に渡して VUI を正しく signal。根拠: https://projects.blender.org/blender/blender/pulls/116008
- 要確認: OffscreenRenderer の target が sRGB-encoded 8-bit であること（gamma trap 回避）。
  `Rgba8UnormSrgb` 想定（gui_01 CLAUDE.md）。バイトは plain 8-bit として matrix のみ適用。

### 2.2 NVENC エンコードパラメータ（~5 Mbit/s 1080p30 visually transparent）

`preset p5/p6 / tune hq / rc vbr / multipass fullres / b:v 5M / maxrate 8M / bufsize 16M /
profile high / bf 3 / rc-lookahead 32 / spatial-aq 1 / temporal-aq 1`。
codec-private opt は `AVDictionary` で `open(Some(dict))`。根拠: StreamFX NVENC wiki, NVENC SDK guide。

### 2.3 NVENC 非搭載機のフォールバック（LGPL-clean）

`find_encoder_by_name("h264_nvenc")` が None / `open()` が AVERROR の場合 → `libopenh264`
（Cisco BSD、BtbN lgpl build に同梱）か `h264_mf`（MF wrapper）。libx264 は GPL なので不可。

## 3. ライセンス & 配布（LGPL を厳守）

- **h264_nvenc・native `aac`・mp4 muxer は全て LGPL**（`--enable-gpl`/`--enable-nonfree` 不要）。
  GPL を強制するのは **libx264** だけ → 含めない。NVENC は nv-codec-headers（BSD/MIT）経由。
  根拠: https://www.ffmpeg.org/legal.html, BtbN `scripts.d/50-ffnvcodec.sh`(全 variant で有効)。
- **配布物**: BtbN **`ffmpeg-nX.Y-latest-win64-lgpl-shared-X.Y.zip`**（release-pinned、
  master-latest は不可＝source 対応義務 item 4）。gyan.dev は全て GPLv3 で不可。
- DLL（`avcodec-NN` / `avformat-NN` / `avutil-NN` / `swscale-N` / `swresample-N`）を
  `daw_gui.exe` の隣に loose で同梱・dynamic link。
- compliance（自分のコードを非 GPL に保つ、mechanical）: ① FFmpeg `LICENSE.txt` 同梱、
  ② 一致する source tarball を同じ場所で配布＋"This software uses code of FFmpeg licensed
  under the LGPLv2.1..." 表記、③ about box / EULA に帰属表記、④ EULA に reverse-engineering
  禁止条項を置かない・FFmpeg の所有権を主張しない、⑤ DLL 名規約を保つ・static link しない。
  根拠: https://www.ffmpeg.org/legal.html, LGPL-2.1 §6。
- 注意: NVENC SDK / driver 結合 — FFmpeg 7.0+ は NVENC SDK 12.x（NVIDIA driver ≥ 550）。
  最小サポート driver に合う release を pin、または detect→fallback。BtbN issue #382。
- H.264 自体の特許（MPEG-LA / Via LA）は LGPL とは別問題（商用配布時に別途要評価）。

## 4. ビルド設定（rsmpeg）

- crate: **rsmpeg**（ffmpeg-next は maintenance mode、rsmpeg は FFmpeg 8.0 追従＝`0.18.0+ffmpeg.8.0`）。
- `[workspace.dependencies]` に `rsmpeg`、`daw_gui` で有効化。
- env（rusty_ffmpeg）: `FFMPEG_LIBS_DIR`=BtbN の `lib/`、`FFMPEG_INCLUDE_DIR`=`include/`、
  `bin/` を PATH（runtime）。`.cargo/config.toml` で固定。
- **`FFMPEG_BINDING_PATH` に commit 済み binding.rs を指す** → 全 dev/CI box で libclang/LLVM 不要。
  binding は LLVM のある 1 台で生成して commit。FFmpeg major bump 時に再生成。
  根拠: rusty_ffmpeg `build.rs`, rsmpeg `doc/windows.md`。

## 5. Phase 計画

### Phase 1 — export encode を libav/NVENC に置換（ユーザー向けの本丸）
- `render_video.rs` の MF sink writer 一式を `LibavEncoderContext`（rsmpeg）に置換。
- NVENC に **RGBA を直入力**（Path A, BT.601 一貫）→ `rgba_to_nv12` をクリティカルパスから除去。
- AAC は hound WAV → libav native aac。mux は libavformat mp4。
- NVENC 不可時は openh264/h264_mf フォールバック。
- export decode は当面 `new_cpu_only()`（MF→ffmpeg.exe）のまま。
- 依存追加（rsmpeg + LGPL DLL + binding + `.cargo/config.toml`）。
- これ単体で「数分 → encoder 律速（NVENC 数×RT）」の主要改善。

### Phase 2 — readback パイプライン化（ストール除去）
- `render_to_rgba` を async/double-buffered staging 化（map-on-fence、ring）。
- encode を専用スレッドに分離 → frame N+1 の合成と frame N の encode を overlap。
- 同期 readback ストールが消え、真に encoder 律速になる。

### Phase 3 — ffmpeg.exe subprocess 廃止（SSoT、3→2 backend）
- export source decode（と 10-bit preview fallback）を **libav SW decode（in-process）** に統一。
- `import_video::extract_metadata` も libav に寄せる。
- `FfmpegSource`（ffmpeg.exe child）を削除。

### Phase 4 — 色厳密化（optional）
- Path B（swscale BT.709 + 明示 range + 正しい VUI）を選べるように。既定は BT.601 一貫のまま。

### やらないこと（明示）
- **8-bit preview の MF D3D11 zero-copy decode を libav D3D11VA に作り直さない**（§1.1）。
  維持する。

## 6. 変更マップ（`render_video.rs`、agent 調査）

- **削除**: `add_video_stream`(339-391), `add_audio_stream`(393-451), `set_frame_size`/
  `set_frame_rate`/`set_pixel_aspect_ratio`(454-488), `write_video_sample`(815-856),
  `write_audio_sample`/`write_all_audio_samples`(858-952), `MFCreateSinkWriterFromURL`+
  `BeginWriting`+`Finalize`(138-180, 304)。
- **維持（byte parity 不変）**: `render_mp4(_cancellable)` のループ構造(248-282),
  `build_frame_scene`(500-735), `aspect_fit`, `offscreen.render_to_rgba`(270-272)。
- **`rgba_to_nv12`(773-813)**: Path A 採用なら export から不要（RGBA 直入力）。Path B 用に残す判断も可。
- **`AudioContext`(332-337)**: hound reader は維持、sink 先だけ libav AAC に。
- Cargo: workspace + daw_gui に rsmpeg。protocol(bincode derive) 型は無影響。
- test `render_mp4_video_only_smoke`(1018): 出力検証を WMF `extract_metadata` から ffprobe/libav へ。

## 7. 未解決質問（実装前に確定）

- NVENC の rate control（VBR+cq / CBR / constqp）と b:v/maxrate の最終値。
- Path A（BT.601 一貫・最速）を既定にするか、最初から Path B（BT.709 厳密）にするか。
- 最小サポート NVIDIA driver → どの BtbN release（NVENC SDK 版）を pin するか。
- Phase 3 を前倒しして Phase 1 と同時に ffmpeg.exe を畳むか。
- export を同期のまま（MVP）にするか、Phase 2 を Phase 1 と同時にやるか。

## 8. 引用（一次情報）

- rsmpeg: https://github.com/larksuite/rsmpeg （`doc/windows.md`, `tests/ffmpeg_examples/`）
- rusty_ffmpeg: https://github.com/CCExtractor/rusty_ffmpeg （`build.rs`, env 契約）
- nvenc RGB 入力 / 色: `libavcodec/nvenc.c`, https://ffmpeg.org/pipermail/ffmpeg-user/2023-March/056142.html
- BtbN builds / variants: https://github.com/BtbN/FFmpeg-Builds （`scripts.d/`）
- FFmpeg 法務: https://www.ffmpeg.org/legal.html
- D3D11VA→wgpu: `libavutil/hwcontext_d3d11va.h`, https://dev.to/the_lone_engineer/ffmpeg-video-playback-in-native-wgpu-552a, wgpu PR #6161
- Ampere 10-bit H.264 非対応: https://developer.nvidia.com/blog/nvidia-video-codec-sdk-13-0-powered-by-nvidia-blackwell/
- swscale threads: https://projects.blender.org/blender/blender/pulls/116008
