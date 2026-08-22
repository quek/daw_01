<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# vendor/ の由来

`ara-sys/vendor` と同じ方針: 一次ソースをリポジトリに取り込み、外部取得
(`make fetch-*` / submodule) を不要にする。ヘッダオンリーなのでビルドに必要なのは
C++17 コンパイラだけ (Windows = MSVC、Linux = g++/clang++)。

| path | upstream | commit | license |
|---|---|---|---|
| `vendor/signalsmith-stretch/signalsmith-stretch.h` | https://github.com/Signalsmith-Audio/signalsmith-stretch | `57b93f4e9206a089a45387eaa39bdc9f310d3308` (2026-01-24) | MIT (`LICENSE.txt` 同梱) |
| `vendor/signalsmith-linear/{stft.h,fft.h}` | https://github.com/Signalsmith-Audio/linear | `0dd6b823783f1fe8768e2700e0937903f4270698` (2026-08-01) | MIT (`LICENSE.txt` 同梱) |

いずれも **無改変**。

## なぜこの 2 ファイルだけか

`signalsmith-stretch.h` の唯一の非標準 include は `"signalsmith-linear/stft.h"`、
`stft.h` の唯一の非標準 include は `"./fft.h"`、`fft.h` は `<complex>` `<vector>`
`<cmath>` `<cstring>` のみ。upstream の `linear.h` / `approx.h` / `platform/*.h`
(Accelerate / IPP / CMSIS-DSP / pffft / xsimd バックエンド) は
`SIGNALSMITH_USE_*` マクロを定義したときだけ include される (`fft.h` 末尾の
`#if defined(...)` チェーン、既定は `#else` を持たない = 内蔵 FFT)。このビルドは
どのマクロも定義しないので、コピーしていないヘッダは到達不能。

「ヘッダは在るのにビルドが通らない」を防ぐため、更新時は上の include グラフを
再確認すること (upstream が新しい include を足していれば、そのファイルも
`vendor/signalsmith-linear/` に足す)。

## 更新手順

```bash
curl -L -o ss.tar.gz https://codeload.github.com/Signalsmith-Audio/signalsmith-stretch/tar.gz/refs/heads/main
curl -L -o sl.tar.gz https://codeload.github.com/Signalsmith-Audio/linear/tar.gz/refs/heads/main
# 展開して signalsmith-stretch.h / stft.h / fft.h / LICENSE.txt を差し替え、
# 上の表の commit を更新し、cargo test -p daw_audio で回帰を確認する
```

`daw_audio/src/stretch_engine.rs` の latency 較正テスト
(`output_seek_aligns_output_to_stream_start`) が、更新でレイテンシ規約が変わって
いないことを機械検査する。

## shim 側で吸収している 2 点 (vendor は無改変のまま)

- **乱数源**: エンジンは 2 倍超のストレッチで位相をランダム化するが、
  `randomEngine` は private かつ `reset()` / `outputSeek()` で巻き戻らない。
  pool でエンジンを使い回すと「その枠でそれまで何を処理したか」で乱数位置が
  変わり、live と offline export が食い違う。 shim は `RandomEngine`
  **テンプレート引数**に自前の xorshift32 を渡し、その状態をエンジンの外
  (`sms_stretch::rng_state`) に置くことで、`sms_output_seek` のたびに巻き戻す。
  標準ライブラリ実装に依らない再現性も同時に得られる。
- **確保の可視化**: `alloc-count` feature を有効にすると shim が global
  `operator new` / `delete` を置換して回数を数え、`sms_alloc_count()` で読める。
  Rust の `#[global_allocator]` フックは C++ の確保を **一切見られない**ので、
  これが無いと RT 無確保テストが vendored エンジン側の回帰を素通しする
  (`make test-rt`)。
