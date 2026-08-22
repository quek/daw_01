# Third-party crate notices

**このファイルは生成物です。手で編集しないでください。**
再生成: `python scripts/dep_licenses.py --write` (`make license-check` が鮮度を検査します)

daw_gui / daw_audio / daw_plugin_host に取り込まれる Rust クレート **389 件** のライセンス一覧です。
依存グラフは `cargo metadata --filter-platform x86_64-pc-windows-msvc --locked` の `resolve` から、normal と build のエッジだけを辿って求めています (dev-dependencies は配布物に入らないので除外)。

crate ではない第三者コンポーネント (FFmpeg / ARA / Signalsmith / VST 3 / CLAP / VOICEVOX) の帰属は [`NOTICE`](NOTICE) にあります。daw_01 自身のライセンスは [`LICENSE`](LICENSE) (GPL-3.0-or-later)。

## ライセンス別の内訳

| 件数 | ライセンス (SPDX) |
|---:|---|
| 184 | `MIT OR Apache-2.0` |
| 73 | `MIT` |
| 22 | `Apache-2.0 OR MIT` |
| 18 | `Unicode-3.0` |
| 17 | `MIT/Apache-2.0` |
| 15 | `MPL-2.0` |
| 11 | `Apache-2.0` |
| 7 | `MIT OR Apache-2.0 OR Zlib` |
| 6 | `Unlicense OR MIT` |
| 4 | `BSD-3-Clause` |
| 3 | `Apache-2.0/MIT` |
| 3 | `ISC` |
| 3 | `Zlib` |
| 3 | `Zlib OR Apache-2.0 OR MIT` |
| 2 | `Apache-2.0 OR ISC OR MIT` |
| 2 | `BSD-2-Clause OR Apache-2.0 OR MIT` |
| 2 | `BSD-3-Clause OR Apache-2.0` |
| 2 | `BSL-1.0` |
| 1 | `(Apache-2.0 OR MIT) AND BSD-3-Clause` |
| 1 | `(MIT OR Apache-2.0) AND Unicode-3.0` |
| 1 | `0BSD OR MIT OR Apache-2.0` |
| 1 | `Apache-2.0 / MIT` |
| 1 | `Apache-2.0 AND ISC` |
| 1 | `Apache-2.0 AND MIT` |
| 1 | `Apache-2.0 OR BSL-1.0` |
| 1 | `Apache-2.0 OR GPL-2.0-only` |
| 1 | `BSD-2-Clause` |
| 1 | `CC0-1.0` |
| 1 | `MIT OR Zlib OR Apache-2.0` |
| 1 | `Unlicense` |

## クレート一覧

| クレート | バージョン | ライセンス (SPDX) | 用途 | 配布元 |
|---|---|---|---|---|
| adler2 | 2.0.1 | `0BSD OR MIT OR Apache-2.0` | link | <https://github.com/oyvindln/adler2> |
| aho-corasick | 1.1.4 | `Unlicense OR MIT` | link | <https://github.com/BurntSushi/aho-corasick> |
| allocator-api2 | 0.2.21 | `MIT OR Apache-2.0` | link | <https://github.com/zakarumych/allocator-api2> |
| anyhow | 1.0.102 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/anyhow> |
| arboard | 3.6.1 | `MIT OR Apache-2.0` | link | <https://github.com/1Password/arboard> |
| arc-swap | 1.9.1 | `MIT OR Apache-2.0` | link | <https://github.com/vorner/arc-swap> |
| arrayref | 0.3.9 | `BSD-2-Clause` | link | <https://github.com/droundy/arrayref> |
| arrayvec | 0.7.6 | `MIT OR Apache-2.0` | link | <https://github.com/bluss/arrayvec> |
| ash | 0.38.0+1.3.281 | `MIT OR Apache-2.0` | link | <https://github.com/ash-rs/ash> |
| atomic-waker | 1.1.2 | `Apache-2.0 OR MIT` | link | <https://github.com/smol-rs/atomic-waker> |
| autocfg | 1.5.0 | `Apache-2.0 OR MIT` | build | <https://github.com/cuviper/autocfg> |
| base64 | 0.22.1 | `MIT OR Apache-2.0` | link | <https://github.com/marshallpierce/rust-base64> |
| bincode | 2.0.1 | `MIT` | link | <https://github.com/bincode-org/bincode> |
| bincode_derive | 2.0.1 | `MIT` | link | <https://github.com/bincode-org/bincode> |
| bindgen | 0.71.1 | `BSD-3-Clause` | build | <https://github.com/rust-lang/rust-bindgen> |
| bit-set | 0.9.1 | `Apache-2.0 OR MIT` | link | <https://github.com/contain-rs/bit-set> |
| bit-vec | 0.9.1 | `Apache-2.0 OR MIT` | link | <https://github.com/contain-rs/bit-vec> |
| bitflags | 1.3.2 | `MIT/Apache-2.0` | link | <https://github.com/bitflags/bitflags> |
| bitflags | 2.11.1 | `MIT OR Apache-2.0` | link | <https://github.com/bitflags/bitflags> |
| block-buffer | 0.10.4 | `MIT OR Apache-2.0` | link | <https://github.com/RustCrypto/utils> |
| bon | 3.9.1 | `MIT OR Apache-2.0` | link | <https://github.com/elastio/bon> |
| bon-macros | 3.9.1 | `MIT OR Apache-2.0` | link | <https://github.com/elastio/bon> |
| bytemuck | 1.25.0 | `Zlib OR Apache-2.0 OR MIT` | link | <https://github.com/Lokathor/bytemuck> |
| bytemuck_derive | 1.10.2 | `Zlib OR Apache-2.0 OR MIT` | link | <https://github.com/Lokathor/bytemuck> |
| byteorder | 1.5.0 | `Unlicense OR MIT` | link | <https://github.com/BurntSushi/byteorder> |
| byteorder-lite | 0.1.0 | `Unlicense OR MIT` | link | <https://github.com/image-rs/byteorder-lite> |
| bytes | 1.11.1 | `MIT` | link | <https://github.com/tokio-rs/bytes> |
| camino | 1.2.2 | `MIT OR Apache-2.0` | build | <https://github.com/camino-rs/camino> |
| cc | 1.2.60 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/cc-rs> |
| cexpr | 0.6.0 | `Apache-2.0/MIT` | link | <https://github.com/jethrogb/rust-cexpr> |
| cfg-if | 1.0.4 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/cfg-if> |
| cfg_aliases | 0.2.1 | `MIT` | build | <https://github.com/katharostech/cfg_aliases> |
| clang-sys | 1.8.1 | `Apache-2.0` | link | <https://github.com/KyleMayes/clang-sys> |
| clap-sys | 0.5.0 | `MIT/Apache-2.0` | link | <https://github.com/micahrj/clap-sys> |
| clipboard-win | 5.4.1 | `BSL-1.0` | link | <https://github.com/DoumanAsh/clipboard-win> |
| codespan-reporting | 0.13.1 | `Apache-2.0` | link | <https://github.com/brendanzab/codespan> |
| color_quant | 1.1.0 | `MIT` | link | <https://github.com/image-rs/color_quant.git> |
| com-scrape-types | 0.1.1 | `MIT OR Apache-2.0` | link | <https://github.com/coupler-rs/vst3-rs> |
| core_maths | 0.1.1 | `MIT` | link | <https://github.com/robertbastian/core_maths> |
| cosmic-text | 0.18.2 | `MIT OR Apache-2.0` | link | <https://github.com/pop-os/cosmic-text> |
| cpal | 0.17.1 | `Apache-2.0` | link | <https://github.com/RustAudio/cpal> |
| cpufeatures | 0.2.17 | `MIT OR Apache-2.0` | link | <https://github.com/RustCrypto/utils> |
| crc32fast | 1.5.0 | `MIT OR Apache-2.0` | link | <https://github.com/srijs/rust-crc32fast> |
| crossbeam-channel | 0.5.15 | `MIT OR Apache-2.0` | link | <https://github.com/crossbeam-rs/crossbeam> |
| crossbeam-deque | 0.8.6 | `MIT OR Apache-2.0` | link | <https://github.com/crossbeam-rs/crossbeam> |
| crossbeam-epoch | 0.9.18 | `MIT OR Apache-2.0` | link | <https://github.com/crossbeam-rs/crossbeam> |
| crossbeam-utils | 0.8.21 | `MIT OR Apache-2.0` | link | <https://github.com/crossbeam-rs/crossbeam> |
| crypto-common | 0.1.7 | `MIT OR Apache-2.0` | link | <https://github.com/RustCrypto/traits> |
| cursor-icon | 1.2.0 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/rust-windowing/cursor-icon> |
| darling | 0.23.0 | `MIT` | link | <https://github.com/TedDriggs/darling> |
| darling_core | 0.23.0 | `MIT` | link | <https://github.com/TedDriggs/darling> |
| darling_macro | 0.23.0 | `MIT` | link | <https://github.com/TedDriggs/darling> |
| dasp_sample | 0.11.0 | `MIT OR Apache-2.0` | link | <https://github.com/rustaudio/sample.git> |
| data-url | 0.3.2 | `MIT OR Apache-2.0` | link | <https://github.com/servo/rust-url> |
| deranged | 0.5.8 | `MIT OR Apache-2.0` | link | <https://github.com/jhpratt/deranged> |
| digest | 0.10.7 | `MIT OR Apache-2.0` | link | <https://github.com/RustCrypto/traits> |
| dirs | 5.0.1 | `MIT OR Apache-2.0` | link | <https://github.com/soc/dirs-rs> |
| dirs-sys | 0.4.1 | `MIT OR Apache-2.0` | link | <https://github.com/dirs-dev/dirs-sys-rs> |
| displaydoc | 0.2.5 | `MIT OR Apache-2.0` | link | <https://github.com/yaahc/displaydoc> |
| document-features | 0.2.12 | `MIT OR Apache-2.0` | link | <https://github.com/slint-ui/document-features> |
| dpi | 0.1.2 | `Apache-2.0 AND MIT` | link | <https://github.com/rust-windowing/winit> |
| either | 1.15.0 | `MIT OR Apache-2.0` | link | <https://github.com/rayon-rs/either> |
| embed-resource | 3.0.9 | `MIT` | build | <https://github.com/nabijaczleweli/rust-embed-resource> |
| encoding_rs | 0.8.35 | `(Apache-2.0 OR MIT) AND BSD-3-Clause` | link | <https://github.com/hsivonen/encoding_rs> |
| equivalent | 1.0.2 | `Apache-2.0 OR MIT` | link | <https://github.com/indexmap-rs/equivalent> |
| error-code | 3.3.2 | `BSL-1.0` | link | <https://github.com/DoumanAsh/error-code> |
| etagere | 0.3.0 | `MIT OR Apache-2.0` | link | <https://github.com/nical/etagere> |
| euclid | 0.22.14 | `MIT OR Apache-2.0` | link | <https://github.com/servo/euclid> |
| extended | 0.1.0 | `MIT` | link | <https://github.com/depp/extended-rs> |
| fax | 0.2.7 | `MIT` | link | <https://github.com/pdf-rs/fax> |
| fdeflate | 0.3.7 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/fdeflate> |
| find-msvc-tools | 0.1.9 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/cc-rs> |
| flate2 | 1.1.9 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/flate2-rs> |
| float-cmp | 0.9.0 | `MIT` | link | <https://github.com/mikedilger/float-cmp> |
| fnv | 1.0.7 | `Apache-2.0 / MIT` | link | <https://github.com/servo/rust-fnv> |
| foldhash | 0.1.5 | `Zlib` | link | <https://github.com/orlp/foldhash> |
| foldhash | 0.2.0 | `Zlib` | link | <https://github.com/orlp/foldhash> |
| font-types | 0.11.3 | `MIT OR Apache-2.0` | link | <https://github.com/googlefonts/fontations> |
| fontdb | 0.23.0 | `MIT` | link | <https://github.com/RazrFalcon/fontdb> |
| form_urlencoded | 1.2.2 | `MIT OR Apache-2.0` | link | <https://github.com/servo/rust-url> |
| futures-channel | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| futures-core | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| futures-io | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| futures-macro | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| futures-sink | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| futures-task | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| futures-util | 0.3.32 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/futures-rs> |
| generic-array | 0.14.7 | `MIT` | link | <https://github.com/fizyk20/generic-array.git> |
| getrandom | 0.2.17 | `MIT OR Apache-2.0` | link | <https://github.com/rust-random/getrandom> |
| getrandom | 0.3.4 | `MIT OR Apache-2.0` | link | <https://github.com/rust-random/getrandom> |
| getrandom | 0.4.2 | `MIT OR Apache-2.0` | link | <https://github.com/rust-random/getrandom> |
| gif | 0.14.1 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/image-gif> |
| gl_generator | 0.14.0 | `Apache-2.0` | build | <https://github.com/brendanzab/gl-rs/> |
| glob | 0.3.3 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/glob> |
| glow | 0.17.0 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/grovesNL/glow> |
| glutin_wgl_sys | 0.6.1 | `Apache-2.0` | link | <https://github.com/rust-windowing/glutin> |
| glyphon | 0.11.0 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/grovesNL/glyphon> |
| gpu-allocator | 0.28.0 | `MIT OR Apache-2.0` | link | <https://github.com/Traverse-Research/gpu-allocator> |
| gpu-descriptor | 0.3.2 | `MIT OR Apache-2.0` | link | <https://github.com/zakarumych/gpu-descriptor> |
| gpu-descriptor-types | 0.2.0 | `MIT OR Apache-2.0` | link | <https://github.com/zakarumych/gpu-descriptor> |
| grid | 1.0.1 | `MIT` | link | <https://github.com/becheran/grid> |
| h2 | 0.4.13 | `MIT` | link | <https://github.com/hyperium/h2> |
| half | 2.7.1 | `MIT OR Apache-2.0` | link | <https://github.com/VoidStarKat/half-rs> |
| harfrust | 0.5.2 | `MIT` | link | <https://github.com/harfbuzz/harfrust> |
| hashbrown | 0.15.5 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/hashbrown> |
| hashbrown | 0.16.1 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/hashbrown> |
| hashbrown | 0.17.0 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/hashbrown> |
| hexf-parse | 0.2.1 | `CC0-1.0` | link | <https://github.com/lifthrasiir/hexf> |
| hound | 3.5.1 | `Apache-2.0` | link | <https://github.com/ruuda/hound> |
| http | 1.4.0 | `MIT OR Apache-2.0` | link | <https://github.com/hyperium/http> |
| http-body | 1.0.1 | `MIT` | link | <https://github.com/hyperium/http-body> |
| http-body-util | 0.1.3 | `MIT` | link | <https://github.com/hyperium/http-body> |
| httparse | 1.10.1 | `MIT OR Apache-2.0` | link | <https://github.com/seanmonstar/httparse> |
| hyper | 1.9.0 | `MIT` | link | <https://github.com/hyperium/hyper> |
| hyper-rustls | 0.27.9 | `Apache-2.0 OR ISC OR MIT` | link | <https://github.com/rustls/hyper-rustls> |
| hyper-tls | 0.6.0 | `MIT/Apache-2.0` | link | <https://github.com/hyperium/hyper-tls> |
| hyper-util | 0.1.20 | `MIT` | link | <https://github.com/hyperium/hyper-util> |
| ico | 0.5.0 | `MIT` | build | <https://github.com/mdsteele/rust-ico> |
| icu_collections | 2.0.0 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| icu_locale_core | 2.2.0 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| icu_normalizer | 2.0.1 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| icu_normalizer_data | 2.0.0 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| icu_properties | 2.0.2 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| icu_properties_data | 2.0.1 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| icu_provider | 2.2.0 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| ident_case | 1.0.1 | `MIT/Apache-2.0` | link | <https://github.com/TedDriggs/ident_case> |
| idna | 1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/servo/rust-url/> |
| idna_adapter | 1.2.1 | `Apache-2.0 OR MIT` | link | <https://github.com/hsivonen/idna_adapter> |
| image | 0.25.10 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/image> |
| image-webp | 0.2.4 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/image-webp> |
| imagesize | 0.14.0 | `MIT` | link | <https://github.com/Roughsketch/imagesize> |
| indexmap | 2.14.0 | `Apache-2.0 OR MIT` | link | <https://github.com/indexmap-rs/indexmap> |
| ipnet | 2.12.0 | `MIT OR Apache-2.0` | link | <https://github.com/krisprice/ipnet> |
| iri-string | 0.7.12 | `MIT OR Apache-2.0` | link | <https://github.com/lo48576/iri-string> |
| itertools | 0.13.0 | `MIT OR Apache-2.0` | link | <https://github.com/rust-itertools/itertools> |
| itoa | 1.0.18 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/itoa> |
| jobserver | 0.1.34 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/jobserver-rs> |
| khronos-egl | 6.0.0 | `MIT/Apache-2.0` | link | <https://github.com/timothee-haudebourg/khronos-egl> |
| khronos_api | 3.1.0 | `Apache-2.0` | link | <https://github.com/brendanzab/gl-rs/> |
| kurbo | 0.13.1 | `Apache-2.0 OR MIT` | link | <https://github.com/linebender/kurbo> |
| lazy_static | 1.5.0 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang-nursery/lazy-static.rs> |
| libc | 0.2.185 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/libc> |
| libloading | 0.8.9 | `ISC` | link | <https://github.com/nagisa/rust_libloading/> |
| libm | 0.2.16 | `MIT` | link | <https://github.com/rust-lang/compiler-builtins> |
| linebender_resource_handle | 0.1.1 | `Apache-2.0 OR MIT` | link | <https://github.com/linebender/raw_resource_handle> |
| litemap | 0.8.2 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| litrs | 1.0.0 | `MIT OR Apache-2.0` | link | <https://github.com/LukasKalbertodt/litrs> |
| lock_api | 0.4.14 | `MIT OR Apache-2.0` | link | <https://github.com/Amanieu/parking_lot> |
| log | 0.4.29 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/log> |
| lru | 0.16.4 | `MIT` | link | <https://github.com/jeromefroe/lru-rs.git> |
| matchers | 0.2.0 | `MIT` | link | <https://github.com/hawkw/matchers> |
| memchr | 2.8.0 | `Unlicense OR MIT` | link | <https://github.com/BurntSushi/memchr> |
| memmap2 | 0.9.10 | `MIT OR Apache-2.0` | link | <https://github.com/RazrFalcon/memmap2-rs> |
| midir | 0.10.4 | `MIT` | link | <https://github.com/Boddlnagg/midir> |
| midly | 0.5.3 | `Unlicense` | link | <https://github.com/negamartin/midly> |
| mime | 0.3.17 | `MIT OR Apache-2.0` | link | <https://github.com/hyperium/mime> |
| minimal-lexical | 0.2.1 | `MIT/Apache-2.0` | link | <https://github.com/Alexhuszagh/minimal-lexical> |
| miniz_oxide | 0.8.9 | `MIT OR Zlib OR Apache-2.0` | link | <https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide> |
| mio | 1.2.0 | `MIT` | link | <https://github.com/tokio-rs/mio> |
| moxcms | 0.8.1 | `BSD-3-Clause OR Apache-2.0` | link | <https://github.com/awxkee/moxcms.git> |
| naga | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| native-tls | 0.2.18 | `MIT OR Apache-2.0` | link | <https://github.com/rust-native-tls/rust-native-tls> |
| nom | 7.1.3 | `MIT` | link | <https://github.com/Geal/nom> |
| ntapi | 0.4.3 | `Apache-2.0 OR MIT` | link | <https://github.com/MSxDOS/ntapi> |
| nu-ansi-term | 0.50.3 | `MIT` | link | <https://github.com/nushell/nu-ansi-term> |
| num-complex | 0.4.6 | `MIT OR Apache-2.0` | link | <https://github.com/rust-num/num-complex> |
| num-conv | 0.2.1 | `MIT OR Apache-2.0` | link | <https://github.com/jhpratt/num-conv> |
| num-integer | 0.1.46 | `MIT OR Apache-2.0` | link | <https://github.com/rust-num/num-integer> |
| num-traits | 0.2.19 | `MIT OR Apache-2.0` | link | <https://github.com/rust-num/num-traits> |
| once_cell | 1.21.4 | `MIT OR Apache-2.0` | link | <https://github.com/matklad/once_cell> |
| option-ext | 0.2.0 | `MPL-2.0` | link | <https://github.com/soc/option-ext.git> |
| ordered-float | 5.3.0 | `MIT` | link | <https://github.com/reem/rust-ordered-float> |
| parking_lot | 0.12.5 | `MIT OR Apache-2.0` | link | <https://github.com/Amanieu/parking_lot> |
| parking_lot_core | 0.9.12 | `MIT OR Apache-2.0` | link | <https://github.com/Amanieu/parking_lot> |
| paste | 1.0.15 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/paste> |
| percent-encoding | 2.3.2 | `MIT OR Apache-2.0` | link | <https://github.com/servo/rust-url/> |
| pico-args | 0.5.0 | `MIT` | link | <https://github.com/RazrFalcon/pico-args> |
| pin-project-lite | 0.2.17 | `Apache-2.0 OR MIT` | link | <https://github.com/taiki-e/pin-project-lite> |
| pkg-config | 0.3.33 | `MIT OR Apache-2.0` | build | <https://github.com/rust-lang/pkg-config-rs> |
| png | 0.17.16 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/image-png> |
| png | 0.18.1 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/image-png> |
| pollster | 0.4.0 | `Apache-2.0/MIT` | link | <https://github.com/zesterer/pollster> |
| polycool | 0.4.0 | `MIT OR Apache-2.0` | link | <https://github.com/linebender/kurbo> |
| potential_utf | 0.1.5 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| powerfmt | 0.2.0 | `MIT OR Apache-2.0` | link | <https://github.com/jhpratt/powerfmt> |
| presser | 0.3.1 | `MIT OR Apache-2.0` | link | <https://github.com/EmbarkStudios/presser> |
| prettyplease | 0.2.37 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/prettyplease> |
| primal-check | 0.3.4 | `MIT OR Apache-2.0` | link | <https://github.com/huonw/primal> |
| proc-macro2 | 1.0.106 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/proc-macro2> |
| profiling | 1.0.17 | `MIT OR Apache-2.0` | link | <https://github.com/aclysma/profiling> |
| pxfm | 0.1.29 | `BSD-3-Clause OR Apache-2.0` | link | <https://github.com/awxkee/pxfm> |
| quick-error | 2.0.1 | `MIT/Apache-2.0` | link | <http://github.com/tailhook/quick-error> |
| quote | 1.0.45 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/quote> |
| range-alloc | 0.1.5 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/range-alloc> |
| rangemap | 1.7.1 | `MIT/Apache-2.0` | link | <https://github.com/jeffparsons/rangemap> |
| raw-window-handle | 0.6.2 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/rust-windowing/raw-window-handle> |
| rayon | 1.12.0 | `MIT OR Apache-2.0` | link | <https://github.com/rayon-rs/rayon> |
| rayon-core | 1.13.0 | `MIT OR Apache-2.0` | link | <https://github.com/rayon-rs/rayon> |
| read-fonts | 0.37.0 | `MIT OR Apache-2.0` | link | <https://github.com/googlefonts/fontations> |
| realfft | 3.5.0 | `MIT` | link | <https://github.com/HEnquist/realfft> |
| regex | 1.12.3 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/regex> |
| regex-automata | 0.4.14 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/regex> |
| regex-syntax | 0.8.10 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/regex> |
| renderdoc-sys | 1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/ebkalderon/renderdoc-rs> |
| reqwest | 0.12.28 | `MIT OR Apache-2.0` | link | <https://github.com/seanmonstar/reqwest> |
| resvg | 0.47.0 | `Apache-2.0 OR MIT` | build | <https://github.com/linebender/resvg> |
| rfd | 0.15.4 | `MIT` | link | <https://github.com/PolyMeilex/rfd> |
| rgb | 0.8.53 | `MIT` | link | <https://github.com/kornelski/rust-rgb> |
| ring | 0.17.14 | `Apache-2.0 AND ISC` | link | <https://github.com/briansmith/ring> |
| roxmltree | 0.21.1 | `MIT OR Apache-2.0` | link | <https://github.com/RazrFalcon/roxmltree> |
| rsmpeg | 0.17.0+ffmpeg.7.1 | `MIT` | link | <https://github.com/larksuite/rsmpeg> |
| rtrb | 0.3.4 | `MIT OR Apache-2.0` | link | <https://github.com/mgeier/rtrb> |
| rustc-hash | 1.1.0 | `Apache-2.0/MIT` | link | <https://github.com/rust-lang-nursery/rustc-hash> |
| rustc-hash | 2.1.2 | `Apache-2.0 OR MIT` | link | <https://github.com/rust-lang/rustc-hash> |
| rustc_version | 0.4.1 | `MIT OR Apache-2.0` | link | <https://github.com/djc/rustc-version-rs> |
| rustfft | 6.4.1 | `MIT OR Apache-2.0` | link | <https://github.com/ejmahler/RustFFT> |
| rustls | 0.23.39 | `Apache-2.0 OR ISC OR MIT` | link | <https://github.com/rustls/rustls> |
| rustls-pki-types | 1.14.1 | `MIT OR Apache-2.0` | link | <https://github.com/rustls/pki-types> |
| rustls-webpki | 0.103.13 | `ISC` | link | <https://github.com/rustls/webpki> |
| rustversion | 1.0.22 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/rustversion> |
| rusty_ffmpeg | 0.16.5+ffmpeg.7.1 | `MIT` | link | <https://github.com/CCExtractor/rusty_ffmpeg/> |
| rustybuzz | 0.20.1 | `MIT` | link | <https://github.com/harfbuzz/rustybuzz> |
| ryu | 1.0.23 | `Apache-2.0 OR BSL-1.0` | link | <https://github.com/dtolnay/ryu> |
| schannel | 0.1.29 | `MIT` | link | <https://github.com/steffengy/schannel-rs> |
| scopeguard | 1.2.0 | `MIT OR Apache-2.0` | link | <https://github.com/bluss/scopeguard> |
| self_cell | 1.2.2 | `Apache-2.0 OR GPL-2.0-only` | link | <https://github.com/Voultapher/self_cell> |
| semver | 1.0.28 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/semver> |
| serde | 1.0.228 | `MIT OR Apache-2.0` | link | <https://github.com/serde-rs/serde> |
| serde_core | 1.0.228 | `MIT OR Apache-2.0` | link | <https://github.com/serde-rs/serde> |
| serde_derive | 1.0.228 | `MIT OR Apache-2.0` | link | <https://github.com/serde-rs/serde> |
| serde_json | 1.0.149 | `MIT OR Apache-2.0` | link | <https://github.com/serde-rs/json> |
| serde_spanned | 1.1.1 | `MIT OR Apache-2.0` | link | <https://github.com/toml-rs/toml> |
| serde_urlencoded | 0.7.1 | `MIT/Apache-2.0` | link | <https://github.com/nox/serde_urlencoded> |
| sha2 | 0.10.9 | `MIT OR Apache-2.0` | link | <https://github.com/RustCrypto/hashes> |
| sharded-slab | 0.1.7 | `MIT` | link | <https://github.com/hawkw/sharded-slab> |
| shlex | 1.3.0 | `MIT OR Apache-2.0` | link | <https://github.com/comex/rust-shlex> |
| simd-adler32 | 0.3.9 | `MIT` | link | <https://github.com/mcountryman/simd-adler32> |
| simplecss | 0.2.2 | `Apache-2.0 OR MIT` | link | <https://github.com/linebender/simplecss> |
| siphasher | 1.0.3 | `MIT/Apache-2.0` | link | <https://github.com/jedisct1/rust-siphash> |
| skrifa | 0.40.0 | `MIT OR Apache-2.0` | link | <https://github.com/googlefonts/fontations> |
| slab | 0.4.12 | `MIT` | link | <https://github.com/tokio-rs/slab> |
| slotmap | 1.1.1 | `Zlib` | link | <https://github.com/orlp/slotmap> |
| smallvec | 1.15.1 | `MIT OR Apache-2.0` | link | <https://github.com/servo/rust-smallvec> |
| smol_str | 0.2.2 | `MIT OR Apache-2.0` | link | <https://github.com/rust-analyzer/smol_str> |
| smol_str | 0.3.6 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/rust-analyzer/tree/master/lib/smol_str> |
| socket2 | 0.6.3 | `MIT OR Apache-2.0` | link | <https://github.com/rust-lang/socket2> |
| spirv | 0.4.0+sdk-1.4.341.0 | `Apache-2.0` | link | <https://github.com/gfx-rs/rspirv> |
| stable_deref_trait | 1.2.1 | `MIT OR Apache-2.0` | link | <https://github.com/storyyeller/stable_deref_trait> |
| static_assertions | 1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/nvzqz/static-assertions-rs> |
| strength_reduce | 0.2.4 | `MIT OR Apache-2.0` | link | <http://github.com/ejmahler/strength_reduce> |
| strict-num | 0.1.1 | `MIT` | link | <https://github.com/RazrFalcon/strict-num> |
| strsim | 0.11.1 | `MIT` | link | <https://github.com/rapidfuzz/strsim-rs> |
| subtle | 2.6.1 | `BSD-3-Clause` | link | <https://github.com/dalek-cryptography/subtle> |
| svgtypes | 0.16.1 | `Apache-2.0 OR MIT` | link | <https://github.com/linebender/svgtypes> |
| swash | 0.2.7 | `Apache-2.0 OR MIT` | link | <https://github.com/dfrg/swash> |
| symlink | 0.1.0 | `MIT/Apache-2.0` | link | <https://gitlab.com/chris-morgan/symlink> |
| symphonia | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-bundle-flac | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-bundle-mp3 | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-codec-aac | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-codec-adpcm | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-codec-alac | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-codec-pcm | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-codec-vorbis | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-core | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-format-isomp4 | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-format-ogg | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-format-riff | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-metadata | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| symphonia-utils-xiph | 0.5.5 | `MPL-2.0` | link | <https://github.com/pdeljanov/Symphonia> |
| syn | 2.0.117 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/syn> |
| sync_wrapper | 1.0.2 | `Apache-2.0` | link | <https://github.com/Actyx/sync_wrapper> |
| synstructure | 0.13.2 | `MIT` | link | <https://github.com/mystor/synstructure> |
| sys-locale | 0.3.2 | `MIT OR Apache-2.0` | link | <https://github.com/1Password/sys-locale> |
| sysinfo | 0.33.1 | `MIT` | link | <https://github.com/GuillaumeGomez/sysinfo> |
| taffy | 0.10.1 | `MIT` | link | <https://github.com/DioxusLabs/taffy> |
| termcolor | 1.4.1 | `Unlicense OR MIT` | link | <https://github.com/BurntSushi/termcolor> |
| thiserror | 2.0.18 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/thiserror> |
| thiserror-impl | 2.0.18 | `MIT OR Apache-2.0` | link | <https://github.com/dtolnay/thiserror> |
| thread_local | 1.1.9 | `MIT OR Apache-2.0` | link | <https://github.com/Amanieu/thread_local-rs> |
| tiff | 0.11.3 | `MIT` | link | <https://github.com/image-rs/image-tiff> |
| time | 0.3.47 | `MIT OR Apache-2.0` | link | <https://github.com/time-rs/time> |
| time-core | 0.1.8 | `MIT OR Apache-2.0` | link | <https://github.com/time-rs/time> |
| time-macros | 0.2.27 | `MIT OR Apache-2.0` | link | <https://github.com/time-rs/time> |
| tiny-skia | 0.12.0 | `BSD-3-Clause` | link | <https://github.com/linebender/tiny-skia> |
| tiny-skia-path | 0.12.0 | `BSD-3-Clause` | link | <https://github.com/linebender/tiny-skia/tree/master/path> |
| tinystr | 0.8.3 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| tinyvec | 1.11.0 | `Zlib OR Apache-2.0 OR MIT` | link | <https://github.com/Lokathor/tinyvec> |
| tinyvec_macros | 0.1.1 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/Soveu/tinyvec_macros> |
| tokio | 1.52.1 | `MIT` | link | <https://github.com/tokio-rs/tokio> |
| tokio-macros | 2.7.0 | `MIT` | link | <https://github.com/tokio-rs/tokio> |
| tokio-native-tls | 0.3.1 | `MIT` | link | <https://github.com/tokio-rs/tls> |
| tokio-rustls | 0.26.4 | `MIT OR Apache-2.0` | link | <https://github.com/rustls/tokio-rustls> |
| tokio-util | 0.7.18 | `MIT` | link | <https://github.com/tokio-rs/tokio> |
| toml | 1.1.2+spec-1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/toml-rs/toml> |
| toml_datetime | 1.1.1+spec-1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/toml-rs/toml> |
| toml_parser | 1.1.2+spec-1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/toml-rs/toml> |
| toml_writer | 1.1.1+spec-1.1.0 | `MIT OR Apache-2.0` | link | <https://github.com/toml-rs/toml> |
| tower | 0.5.3 | `MIT` | link | <https://github.com/tower-rs/tower> |
| tower-http | 0.6.8 | `MIT` | link | <https://github.com/tower-rs/tower-http> |
| tower-layer | 0.3.3 | `MIT` | link | <https://github.com/tower-rs/tower> |
| tower-service | 0.3.3 | `MIT` | link | <https://github.com/tower-rs/tower> |
| tracing | 0.1.44 | `MIT` | link | <https://github.com/tokio-rs/tracing> |
| tracing-appender | 0.2.5 | `MIT` | link | <https://github.com/tokio-rs/tracing> |
| tracing-attributes | 0.1.31 | `MIT` | link | <https://github.com/tokio-rs/tracing> |
| tracing-core | 0.1.36 | `MIT` | link | <https://github.com/tokio-rs/tracing> |
| tracing-log | 0.2.0 | `MIT` | link | <https://github.com/tokio-rs/tracing> |
| tracing-subscriber | 0.3.23 | `MIT` | link | <https://github.com/tokio-rs/tracing> |
| transpose | 0.2.3 | `MIT OR Apache-2.0` | link | <https://github.com/ejmahler/transpose> |
| try-lock | 0.2.5 | `MIT` | link | <https://github.com/seanmonstar/try-lock> |
| ttf-parser | 0.25.1 | `MIT OR Apache-2.0` | link | <https://github.com/harfbuzz/ttf-parser> |
| typenum | 1.20.0 | `MIT OR Apache-2.0` | link | <https://github.com/paholg/typenum> |
| unicode-bidi | 0.3.18 | `MIT OR Apache-2.0` | link | <https://github.com/servo/unicode-bidi> |
| unicode-bidi-mirroring | 0.4.0 | `MIT/Apache-2.0` | link | <https://github.com/RazrFalcon/unicode-bidi-mirroring> |
| unicode-ccc | 0.4.0 | `MIT/Apache-2.0` | link | <https://github.com/RazrFalcon/unicode-ccc> |
| unicode-ident | 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` | link | <https://github.com/dtolnay/unicode-ident> |
| unicode-linebreak | 0.1.5 | `Apache-2.0` | link | <https://github.com/axelf4/unicode-linebreak> |
| unicode-properties | 0.1.4 | `MIT/Apache-2.0` | link | <https://github.com/unicode-rs/unicode-properties> |
| unicode-script | 0.5.8 | `MIT OR Apache-2.0` | link | <https://github.com/unicode-rs/unicode-script> |
| unicode-segmentation | 1.13.2 | `MIT OR Apache-2.0` | link | <https://github.com/unicode-rs/unicode-segmentation> |
| unicode-vo | 0.1.0 | `MIT/Apache-2.0` | link | <https://github.com/RazrFalcon/unicode-vo> |
| unicode-width | 0.2.2 | `MIT OR Apache-2.0` | link | <https://github.com/unicode-rs/unicode-width> |
| untrusted | 0.9.0 | `ISC` | link | <https://github.com/briansmith/untrusted> |
| unty | 0.0.4 | `MIT OR Apache-2.0` | link | <https://github.com/bincode-org/unty> |
| url | 2.5.8 | `MIT OR Apache-2.0` | link | <https://github.com/servo/rust-url> |
| usvg | 0.47.0 | `Apache-2.0 OR MIT` | link | <https://github.com/linebender/resvg> |
| utf8_iter | 1.0.4 | `Apache-2.0 OR MIT` | link | <https://github.com/hsivonen/utf8_iter> |
| uuid | 1.23.1 | `Apache-2.0 OR MIT` | link | <https://github.com/uuid-rs/uuid> |
| version_check | 0.9.5 | `MIT/Apache-2.0` | build | <https://github.com/SergioBenitez/version_check> |
| virtue | 0.0.18 | `MIT` | link | <https://github.com/bincode-org/virtue> |
| vst3 | 0.3.0 | `MIT OR Apache-2.0` | link | <https://github.com/coupler-rs/vst3-rs> |
| vswhom | 0.1.0 | `MIT` | link | <https://github.com/nabijaczleweli/vswhom.rs> |
| vswhom-sys | 0.1.3 | `MIT` | link | <https://github.com/nabijaczleweli/vswhom-sys.rs> |
| want | 0.3.1 | `MIT` | link | <https://github.com/seanmonstar/want> |
| weezl | 0.1.12 | `MIT OR Apache-2.0` | link | <https://github.com/image-rs/weezl> |
| wgpu | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| wgpu-core | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| wgpu-core-deps-windows-linux-android | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| wgpu-hal | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| wgpu-naga-bridge | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| wgpu-types | 29.0.3 | `MIT OR Apache-2.0` | link | <https://github.com/gfx-rs/wgpu> |
| winapi | 0.3.9 | `MIT/Apache-2.0` | link | <https://github.com/retep998/winapi-rs> |
| winapi-util | 0.1.11 | `Unlicense OR MIT` | link | <https://github.com/BurntSushi/winapi-util> |
| windows | 0.56.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows | 0.62.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-collections | 0.3.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-core | 0.56.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-core | 0.62.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-future | 0.3.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-implement | 0.56.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-implement | 0.60.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-interface | 0.56.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-interface | 0.59.3 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-link | 0.2.1 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-numerics | 0.3.1 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-registry | 0.6.1 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-result | 0.1.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-result | 0.4.1 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-strings | 0.5.1 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-sys | 0.48.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-sys | 0.52.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-sys | 0.59.0 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-sys | 0.61.2 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-targets | 0.48.5 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-targets | 0.52.6 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows-threading | 0.2.1 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows_x86_64_msvc | 0.48.5 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| windows_x86_64_msvc | 0.52.6 | `MIT OR Apache-2.0` | link | <https://github.com/microsoft/windows-rs> |
| winit | 0.30.13 | `Apache-2.0` | link | <https://github.com/rust-windowing/winit> |
| winnow | 1.0.2 | `MIT` | link | <https://github.com/winnow-rs/winnow> |
| winreg | 0.55.0 | `MIT` | link | <https://github.com/gentoo90/winreg-rs> |
| writeable | 0.6.3 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| xml-rs | 0.8.28 | `MIT` | link | <https://github.com/kornelski/xml-rs> |
| xmlwriter | 0.1.0 | `MIT` | link | <https://github.com/RazrFalcon/xmlwriter> |
| yazi | 0.2.1 | `Apache-2.0 OR MIT` | link | <https://github.com/dfrg/yazi> |
| yoke | 0.8.2 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| yoke-derive | 0.8.2 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| zeno | 0.3.3 | `Apache-2.0 OR MIT` | link | <https://github.com/dfrg/zeno> |
| zerocopy | 0.8.48 | `BSD-2-Clause OR Apache-2.0 OR MIT` | link | <https://github.com/google/zerocopy> |
| zerocopy-derive | 0.8.48 | `BSD-2-Clause OR Apache-2.0 OR MIT` | link | <https://github.com/google/zerocopy> |
| zerofrom | 0.1.7 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| zerofrom-derive | 0.1.7 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| zeroize | 1.8.2 | `Apache-2.0 OR MIT` | link | <https://github.com/RustCrypto/utils> |
| zerotrie | 0.2.4 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| zerovec | 0.11.6 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| zerovec-derive | 0.11.3 | `Unicode-3.0` | link | <https://github.com/unicode-org/icu4x> |
| zmij | 1.0.21 | `MIT` | link | <https://github.com/dtolnay/zmij> |
| zune-core | 0.5.1 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/etemesi254/zune-image> |
| zune-jpeg | 0.5.15 | `MIT OR Apache-2.0 OR Zlib` | link | <https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg> |
