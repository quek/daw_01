<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

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

#### P4 詳細設計 (2026-05-26)

##### 全体図

```text
   main thread                     video-decode-worker thread
   ───────────                     ──────────────────────────
   active_sources_at(playhead)     wait_for_request()
        │                                │
        │ for each source:               ▼
        │  request(id, path,            engine.decode_at(id, path,
        │          center=current,                       target_i, slot_idx_i)
        │          step=1/fps)              for i in 0..N
        │                                       │
        │                                       ▼
        ▼                              CopySubresource into
   drain_rings()                       SharedPool.slots[i]
        │                                       │
        ▼                                       ▼
   for ring in rings:                  result_tx.send((id, ring))
       for slot in ring.slots:                   │
           preview.upload_frame(                 │
               id, slot_idx,                    (8 bytes / slot, ~50 bytes / ring)
               &slot.frame)
        │
        ▼
   for src in active:
       nearest = ring.find(src.μs)
       composite_layers.push(
           texture = preview.frame_textures[(id, nearest.slot_idx)],
           alpha = src.alpha,
       )
```

CPU pixel data は触らない (= P3 を維持)、 ring は **handle / slot_idx の metadata** だけ流通する。

##### Ring size 確定

`PREVIEW_RING_SIZE = 6`。 根拠:

- 30fps preview target × 200ms 先読み = 6 frames
- worker decode jitter: HW H.264 で 1 burst あたり ~5 frames (= 連続 P-frame の forward-walk が一発で固まる、 1 frame ~5-10ms HW decode × 5 = 25-50ms)
- main vsync 16ms (= 60Hz) で 1 frame ずつ消費するので、 6 frame 先読みで 100ms 分の jitter を吸収

##### データモデル変更

###### `SharedEntry` → `SharedPool` (per-source N slots)

```rust
struct SharedSlot {
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: HANDLE,
}
struct SharedPool {
    slots: [SharedSlot; PREVIEW_RING_SIZE],
}
struct ReaderEntry {
    // ...
    shared_pool: Option<SharedPool>,  // 旧 `shared: Option<SharedEntry>` を置換
}
```

各 `SharedSlot` は独立した D3D11 texture + 独立した NT handle。 worker は round-robin で slot を書き、 main は ring 内の各 slot の handle を別個に wgpu import。

###### `DecodedFrame::Shared` に `slot_idx` 追加

```rust
pub enum DecodedFrame {
    Shared {
        width: u32,
        height: u32,
        handle: SendHandle,
        slot_idx: u8,  // 新規 — same source_id でも slot 違いを区別
    },
    Bgra { /* unchanged */ },
}
```

Bgra fallback path は ring 化対象外 (= HW decode が無効 = GPU path が成立しない場合の fallback、 ring jitter 吸収より「動く」 が優先)。 Bgra path はそのまま 1-frame 結果を返す。

###### `decode_at` シグネチャ変更

```rust
pub fn decode_at(
    &mut self,
    video_source_id: VideoSourceId,
    source_path: &Path,
    target_micros: u64,
    slot_idx: u8,  // 新規 — write 先 SharedSlot の index
) -> Result<DecodedFrame, String>
```

slot_idx は呼び元 worker が round-robin で決定。 Bgra path では未使用。

###### Worker `PendingRequest` を center + step に拡張

```rust
struct PendingRequest {
    source_path: PathBuf,
    center_target_micros: u64,    // ring の先頭 target
    step_micros: u64,             // = 1_000_000 / project_framerate
}
```

`request` API:

```rust
pub fn request(
    &self,
    source_id: VideoSourceId,
    source_path: PathBuf,
    center_target_micros: u64,
    step_micros: u64,
)
```

###### Worker → main の通信

```rust
pub struct DecodedRing {
    pub source_id: VideoSourceId,
    pub slots: Vec<RingSlot>,  // sorted by target_micros ascending, len <= N
}
pub struct RingSlot {
    pub target_micros: u64,
    pub frame: DecodedFrame,   // Shared { slot_idx } or Bgra
}
```

`drain_results()` の戻り型を `Vec<DecodeResult>` から `Vec<DecodedRing>` に変更。 mpsc::channel 1 件 = 1 ring。

##### Worker ループ

```rust
loop {
    let snapshot = wait_until_pending_nonempty_and_drain();  // 既存 idiom
    for (source_id, req) in snapshot {
        let mut ring = Vec::with_capacity(PREVIEW_RING_SIZE);
        for i in 0..PREVIEW_RING_SIZE {
            let slot_idx = i as u8;  // 単純な 0..N、 ring snapshot は毎回上書き
            let target = req.center_target_micros + (i as u64) * req.step_micros;
            match engine.decode_at(source_id, &req.source_path, target, slot_idx) {
                Ok(frame) => ring.push(RingSlot { target_micros: target, frame }),
                Err(e) => {
                    tracing::warn!(error = %e, source_id, target, slot_idx,
                        "video worker: ring slot decode failed, leaving slot empty");
                    // skip — ring stays valid with the slots we did get
                }
            }
        }
        if !ring.is_empty() {
            result_tx.send(DecodedRing { source_id, slots: ring })?;
        }
    }
}
```

forward-walk が効くので、 `decode_at` を target 順に呼ぶと 2nd-Nth の `walk_ms` は near-zero (= last_decoded_micros からの差が step_micros = 33ms 程度なので forward-walk path)。

##### Main 側の dispatch

###### `frame_textures` を per-slot 化

```rust
// 旧: HashMap<VideoSourceId, (TextureHandle, u32, u32)>
// 新:
pub frame_textures: HashMap<(VideoSourceId, u8), (TextureHandle, u32, u32)>
```

`upload_frame(source_id, frame)` を `upload_frame(source_id, slot_idx, frame)` に変更、 内部 key を `(source_id, slot_idx)` に。 destroy ロジックも per-(source, slot) で。

###### Cached rings

```rust
struct RunnerState {
    // ...
    cached_rings: HashMap<VideoSourceId, DecodedRing>,
}
```

`drain_preview_worker_results`:

```rust
for ring in worker.drain_results() {
    for slot in &ring.slots {
        if let DecodedFrame::Shared { slot_idx, .. } = slot.frame {
            preview.upload_frame(ring.source_id, slot_idx, &slot.frame);
        }
        // Bgra path: slot_idx default 0、 上書きでも問題なし
    }
    state.cached_rings.insert(ring.source_id, ring);  // 古い ring を置換
}
```

###### Pick logic in `drive_preview_playback`

```rust
for active in active_sources_at(...) {
    let ring = state.cached_rings.get(&active.video_source_id);
    let Some(slot) = ring.and_then(|r| nearest_slot(&r.slots, active.source_micros)) else {
        continue;  // first frame for this source not yet decoded
    };
    let DecodedFrame::Shared { slot_idx, width, height, .. } = slot.frame else {
        // Bgra fallback: slot_idx is 0 in our convention
        continue_with_handle((active.video_source_id, 0), ...);
        continue;
    };
    if let Some((tex, w, h)) =
        preview.frame_textures.get(&(active.video_source_id, slot_idx)).copied()
    {
        composite_layers.push(CompositeLayer { texture: tex, width: w, height: h,
                                                alpha: active.alpha });
    }
}

fn nearest_slot<'a>(slots: &'a [RingSlot], target: u64) -> Option<&'a RingSlot> {
    slots.iter().min_by_key(|s| s.target_micros.abs_diff(target))
}
```

###### Request API の呼び元変更

```rust
let step_micros = (1_000_000.0 / song.video_framerate.max(1.0)).round() as u64;
state.playback_worker.request(
    frame_info.video_source_id,
    abs_path,
    frame_info.source_micros,  // center
    step_micros,
);
```

##### Seek 処理 (ring flush)

新 `request` の `center_target_micros` が「前回 ring の cover 範囲外」 (= 後端 + step を超える / 先端より前) なら worker は ring 内部の `last_decoded_micros` を信頼せず `decode_at` 内部で seek が走る (既存の `should_seek` 判定をそのまま使う、 forward-walk 判定が `> 100_000` μs で seek するので scrub も自然に対応)。 ring 自体の flush は不要 (= 新 ring snapshot が古い ring を全置換する)。

##### Bgra fallback の扱い

D3D11 init 失敗時の Bgra path は ring 化対象外。 worker のループ自体は同じく N 回 `decode_at` を呼ぶが、 戻り値はすべて `DecodedFrame::Bgra { slot_idx: 0 }` (= 1 つの TextureHandle に N 回上書き)。 N-1 frame は無駄になるが Bgra path は HW なし環境の fallback なので perf 妥協を許容。

##### 検証

- `cargo build --workspace` / `cargo clippy -- -D warnings` / `cargo test --workspace`
- smoke test: `cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4` exit 0
- 実機: 1080p60 source × 30fps preview で frame-pacing 目視確認、 diagnostic log の `max_dt_ms < 50ms`

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
