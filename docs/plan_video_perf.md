# plan_video_perf.md — Preview window 30fps 達成計画 (zero-copy GPU pipeline)

related: [plan_video.md](plan_video.md)、 [gui_01_conversation.md #045](gui_01_conversation.md)

## 目標

debug build でも 1080p60 H.264 source を **30fps 以上** で smooth に preview する。 理想は **CPU が pixel data に触らない zero-copy GPU pipeline**。

## 現状 (2026-05-25 計測)

実プレイバック中の diagnostic log (`decode timing` / `preview upload timing` / `main render fps`) から:

| 項目 | 実測 (debug build) | 内訳 |
|---|---|---|
| `walk_ms` (= WMF H.264 SW decode) | 40-60ms / decode | 4-6 source frames × ~10ms each |
| `swap_ms` (= 1080p BGRA→RGBA SSSE3) | 28ms / decode | debug は SIMD loop 最適化されない |
| `upload_ms` (= wgpu GPU upload) | 1-3ms / frame | non-bottleneck |
| **worker decode rate** | ~11-14fps | walk + swap = 70-90ms |
| **target_micros 進行 / 壁時計** | 0.98x ≈ 1.0x | playhead 同期は取れている |

→ 体感「10fps コマ送り」 と一致。 bottleneck は CPU 側の 1080p H.264 復号 + channel swap 両方。

## 理想 architecture

```text
   WMF SourceReader (D3D11 device manager attached)
          │
          │ ReadSample
          ▼
   IMFSample → IMFDXGIBuffer → ID3D11Texture2D  ← HW H.264 decoder (GPU)
          │
          │ 共有 KEYED_MUTEX + NT shared handle
          ▼
   wgpu Renderer (DX12 backend)
          │ OpenSharedHandle on ID3D12Device
          ▼
   wgpu::Texture (= 同じ GPU メモリを別 API view で参照)
          │
          │ Scene::push_textured_quad
          ▼
   fragment shader sampling → preview window
```

**CPU が pixel data に触らない。** 復号 → 中継 → 表示 全部 GPU。 8 MB / frame の memcpy が消える、 SIMD loop の debug 速度低下も無関係になる。

## Phases (実装順)

### P1: WMF D3D11 HW decode (CPU readback 経由)

**daw_gui 側のみ。** gui_01 変更不要。 walk_ms の削減が主目的。

- `import_video::ensure_mf_startup_pub` + `ensure_d3d11_device()` の新設
  - `D3D11CreateDevice` で feature level 11.1+ の hardware device を作る
  - `MFCreateDXGIDeviceManager` で IMFDXGIDeviceManager 作って `ResetDevice(d3d11_device)`
- `create_reader_for_source` で `MFCreateAttributes` 経由で `MF_SOURCE_READER_D3D_MANAGER` に dxgi manager をセット
- `read_sample_only` の戻り IMFSample から `IMFDXGIBuffer::GetResource → ID3D11Texture2D` を取り出す
- staging texture (`USAGE_STAGING + CPU_ACCESS_READ`) に `CopyResource` → `Map(D3D11_MAP_READ)` → `bgra_to_rgba` (既存 path 流用) → 既存の preview upload に渡す

効果見込み (debug build): walk_ms **40-60ms → 5-10ms** (HW decode は Intel iGPU で 1080p H.264 ~5ms/frame)。 swap_ms 28ms は据え置きで合計 33-38ms = **約 28-30fps**。

### P2: BGRA8 直 upload (CPU swap 除去)

gui_01 #045-A 要望に依存。 P1 と独立に実装可。

- gui_01 で `Renderer::{create_texture_bgra, upload_texture_bgra}` が landing 後、 `preview_window.upload_frame` を BGRA 受けに切替
- `bgra_to_rgba` 呼び出しを `import_video` 系の thumbnail 抽出 path (= 1 回切りなので速度関係なし) だけに限定、 playback path は BGRA そのまま GPU 送り
- DXGI texture sharing で受け取る WMF の subtype は **MFVideoFormat_NV12** が HW decoder のネイティブ出力だが、 まずは MFVideoFormat_ARGB32 (= BGRA8) で video processor MFT に変換させて GPU 内で BGRA に着地させる。 NV12 直サンプリングは shader 側 YUV→RGB が要るので P3 以降の選択肢

効果: swap_ms **28ms → 0ms**。 合計 5-10ms = **60-90fps 余裕**。

### P3: DXGI shared texture (zero-copy)

gui_01 #045-B 要望に依存。 P1 + P2 後の正式形態。

- WMF が返した ID3D11Texture2D を `KEYED_MUTEX` + `SHARED_NTHANDLE` 付きで作り直す (= IMFSample が返す texture が共有可能 flag を持っていなければ inter-mediate copy が要る、 持っていれば同じ texture を直接 share)
- `CreateSharedHandle` で NT handle 取得
- `Renderer::create_texture_from_d3d11_shared_handle(handle, Bgra8UnormSrgb, w, h)` で wgpu に import
- worker → main thread channel は handle + texture metadata だけ送る (= 8 MB / frame の Vec<u8> から ~20 bytes に縮む)
- preview upload は no-op (texture が既に GPU にある)

効果: CPU 側の pixel 関連 work が完全消失。 GPU 側だけで 1080p120 でも余裕。

### P4: Lookahead ring buffer

P1-P3 が落ち着いたら最後の仕上げ。

- worker は `target_micros` を 1 つの値ではなく **連続する N frames** 先読みして ring buffer に並べる
  - ring buffer size = N (= 5-10 frames 程度、 60fps source なら 100-200ms 先読み)
  - GUI thread からの `request(target)` は ring の中心を移動するだけ (= seek 判定は ring の左右がカバー範囲内かで決まる)
- main thread は vsync で起きるたびに ring から最も近い target の frame を pick → preview window に push
  - 復号 jitter (4-6 frame walk が一発で来る) は ring が吸収
- ring の前端 (= oldest) は drop、 後端 (= 先読み) は GPU texture pool で再利用

効果: 30fps preview の **frame-pacing が安定**。 vsync 同期で aliasing なし。

## 検証

各 phase 完了後の判定基準 (= diagnostic log で確認):

- **P1**: `decode timing` の `walk_ms` < 15
- **P2**: `decode timing` の `swap_ms` < 2 (or 完全に未計測 = path 通っていない)
- **P3**: `preview upload timing` がほぼ 0ms (= upload 自体が無いか、 metadata のみ)
- **P4**: `main render fps` が安定して 30+、 max_dt_ms < 50

## Out of scope

- HEVC / VP9 / AV1 HW decode (現状 H.264 だけ targeting)
- macOS / Linux 同等構成 (= VideoToolbox / VA-API、 P5 範囲外)
- color space management (= BT.601 / BT.709 / BT.2020) — 現状 SDR BT.709 only

## 関連 doc

- [plan_video.md](plan_video.md) §3 P5 lookahead architecture の正式実装が本 plan
- [gui_01_conversation.md #045](gui_01_conversation.md) — gui_01 への API 追加要望 (P2 / P3 前提)
- [video_playback.rs](../daw_gui/src/video_playback.rs)、 [video_playback_worker.rs](../daw_gui/src/video_playback_worker.rs)、 [view/preview_window.rs](../daw_gui/src/view/preview_window.rs) — 実装対象
