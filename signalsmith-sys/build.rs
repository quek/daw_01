//! Build script for `signalsmith-sys`.
//!
//! Compiles the C ABI shim (`shim/stretch_shim.cpp`) against the vendored
//! header-only Signalsmith Stretch / Signalsmith Linear sources. There is no
//! external library to fetch or link — `vendor/` is committed (MIT), so a
//! fresh checkout builds with nothing but a C++17 compiler (MSVC on Windows,
//! g++/clang++ elsewhere). bindgen / libclang are **not** used: the C ABI is
//! narrow enough to declare by hand in `src/lib.rs` (`ara-sys` needs bindgen
//! because ARA is a large pure-type header; this crate exposes 10 functions).

fn main() {
    println!("cargo:rerun-if-changed=shim/stretch_shim.cpp");
    println!("cargo:rerun-if-changed=shim/stretch_shim.h");
    println!("cargo:rerun-if-changed=vendor/signalsmith-stretch/signalsmith-stretch.h");
    println!("cargo:rerun-if-changed=vendor/signalsmith-linear/stft.h");
    println!("cargo:rerun-if-changed=vendor/signalsmith-linear/fft.h");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        // `signalsmith-stretch.h` includes "signalsmith-linear/stft.h", so the
        // vendor root is the include dir that makes that path resolve.
        .include("vendor")
        .include("vendor/signalsmith-stretch")
        .file("shim/stretch_shim.cpp");

    // `alloc-count`: C++ 側のヒープ確保を数えられるようにする (RT 検証用)。
    if std::env::var_os("CARGO_FEATURE_ALLOC_COUNT").is_some() {
        build.define("DAW01_SMS_COUNT_ALLOCS", None);
    }

    // CPU の要求水準は Rust 側と同じ (`.cargo/config.toml` の `target-cpu`、cargo が有効な機能を
    // `CARGO_CFG_TARGET_FEATURE` で渡す)。演算順は変えない — MSVC は VS2022 以降 `/fp:precise` (既定) で
    // FMA 合成を作らない、gcc / clang は既定で合成するので `-ffp-contract=off` で止める。
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let avx2 = features.split(',').any(|f| f == "avx2");
    let fma = features.split(',').any(|f| f == "fma");

    // The vendored headers are third-party; their warnings are noise in our
    // build log and must never fail the build.
    if build.get_compiler().is_like_msvc() {
        // /EHsc: the shim never lets an exception cross the C ABI, but the STL
        // containers inside the engine need the standard unwinding model.
        build.flag("/EHsc").flag("/W0");
        if avx2 {
            build.flag("/arch:AVX2");
        }
    } else {
        build.flag("-w");
        if avx2 {
            build.flag("-mavx2");
        }
        if fma {
            build.flag("-mfma");
        }
        build.flag("-ffp-contract=off");
    }

    build.compile("signalsmith_stretch_shim");
}
