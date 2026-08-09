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

    // The vendored headers are third-party; their warnings are noise in our
    // build log and must never fail the build.
    if build.get_compiler().is_like_msvc() {
        // /EHsc: the shim never lets an exception cross the C ABI, but the STL
        // containers inside the engine need the standard unwinding model.
        build.flag("/EHsc").flag("/W0");
    } else {
        build.flag("-w");
    }

    build.compile("signalsmith_stretch_shim");
}
