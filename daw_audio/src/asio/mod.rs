//! ASIO ドライバーを **SDK 無しで** 直接呼ぶ層 (r.md #156)。
//!
//! ASIO ドライバーは in-process の COM object で、interface は `IASIO` 1 つだけ。 ここでは
//! その vtable を Rust で宣言して直に呼ぶ。 ASIO SDK の C++ (`asio.cpp` / `asiodrivers.cpp`)・
//! bindgen・LLVM・C++ のビルド・cpal の ASIO host はどれも使わない (SDK のソースは repo に入れない)。
//!
//! - [`sys`]: ABI (型・定数・vtable)。
//! - [`registry`]: インストール済みドライバーの列挙 (load しない)。
//! - [`driver`]: ドライバー 1 つの wrapper (問い合わせ・設定画面)。
//! - [`stream`]: buffer と callback の受け口。
//! - [`probe`]: `daw_audio --asio-probe [名前]` (この層がこの PC で動くことの実測)。
//!
//! # 一次情報 (Steinberg ASIO SDK 2.3、2025 年版。 GPLv3 との dual license = `LICENSE.txt`)
//!
//! | 確かめたこと | 出典 |
//! |---|---|
//! | `IASIO` の method の並び (= vtable の slot 順) とシグネチャ | `common/iasiodrv.h:27-53` |
//! | 型・定数・struct、全体が `#pragma pack(push,4)` | `common/asio.h:88-89`, `94-171`, `217-228`, `237-285`, `400-476`, `687-694`, `775-783`, `824-829`, `1071-1072` |
//! | Windows は `NATIVE_INT64` = 0 (`ASIOSamples` は hi/lo の 2 語)、`ASIOSampleRate` は `double` | `common/asiosys.h:20-30` |
//! | 列挙する registry の場所と値、DLL の実在確認 | `host/pc/asiolist.cpp:51-54`, `59-100`, `114-136`, `188-190` |
//! | 生成は `CoCreateInstance(clsid, 0, CLSCTX_INPROC_SERVER, clsid, ..)` (IID に CLSID)、COM は `CoInitialize(0)` (STA) | `host/pc/asiolist.cpp:203`, `230` |
//! | `init(sysRef)` の引数は主窓の HWND、失敗時は `getErrorMessage` | `common/asio.h:485-493`, `common/asio.cpp:69-78` |
//! | 終了は `Release` だけ (exit の method は無い) | `common/asio.cpp:104-116` |
//! | `outputReady` は create の後に 1 回試し、`ASE_OK` なら buffer ごとに呼ぶ。 `bufferSwitchTimeInfo` は 0 を返す | `host/sample/hostsample.cpp:180-186`, `311-320` |
//! | latency は `createBuffers` の後に取る | `host/sample/hostsample.cpp:475` |
//! | `kAsioResetRequest` はその場で畳めない (ドライバーの中から呼ばれる)。 後で stop → dispose → 破棄 → 作り直す | `common/asio.h:442-451`, `host/sample/hostsample.cpp:370-377` |
//!
//! struct の大きさと field の位置は、SDK のヘッダを MSVC (x64) でコンパイルして出した値を
//! `sys` の `const _` で固定している (自然配置とずれるのは `ASIOTimeCode` 84 byte と `ASIOTime` 148 byte)。
//!
//! # スレッドの規約
//!
//! - **ドライバーを作ったスレッド = 制御スレッド**。 `CoInitializeEx(COINIT_APARTMENTTHREADED)` した
//!   (STA) スレッドで [`driver::Driver::open`] し、問い合わせ・`createBuffers`・`start`・`stop`・
//!   `disposeBuffers`・`controlPanel`・`Release` はすべて同じスレッドから呼ぶ ([`driver::Driver`] は
//!   `!Send`)。 SDK の host がそうしている (`CoInitialize(0)`)。
//! - 制御スレッドは **message を回し続ける**。 STA の決まりであり、`controlPanel` が出す窓も、
//!   ドライバーが内部で使う窓 message もこのスレッドに届く。
//! - **callback はドライバーのスレッドから来る** (制御スレッドではない。 probe で実測)。
//!   `outputReady` だけは callback の中からそのスレッドで呼ぶ (marshaling を通さない直呼び。
//!   ASIO の host はすべてそうする)。
//! - 対象は 64 bit Windows だけ。 32 bit の `IASIO` は thiscall で、この宣言では呼べない
//!   ([`sys::IAsioVtbl`])。

pub mod driver;
pub mod probe;
pub mod registry;
pub mod stream;
pub mod sys;
