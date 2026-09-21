//! 読み込んだプラグインモジュール (DSO) をパス単位で 1 つだけ持つ。
//!
//! **1 パス = 1 `LoadLibrary` + 1 entry 呼び出し + 1 factory** で、そこから N
//! インスタンスを作る。インスタンスごとに読み直してはいけない。
//!
//! # 一次情報
//!
//! - VST3: SDK の `VST3::Hosting::Module` (`vst3_public_sdk/source/vst/hosting/module_win32.cpp`)
//!   が規範。`Win32Module::load` が `LoadLibrary` → `InitDll` → `GetPluginFactory` を **1 回**
//!   行い、`~Win32Module` が factory を release → `ExitDll` → `FreeLibrary` の順で畳む。
//!   ホストは `Module::Ptr` (shared_ptr) をパス単位で持ち回す。
//! - CLAP 1.2.0: `clap/include/clap/entry.h` —
//!   「hosts will call each once and in matched pairs」「a host should make an absolute
//!   best effort to call `init()` and `deinit()` once」。
//!
//! # なぜ効くか
//!
//! entry (`InitDll` / `clap_entry.init`) はプラグインのグローバル (サンプル / プリセット
//! キャッシュ / DSP テーブル) を初期化する場所で、多くのプラグインは冪等ではない。
//! インスタンスごとに呼ぶと、そのグローバルが本数ぶん重複して常駐する。実測 (2026-09-21、
//! Analog Lab V × 40): 1 本あたり 275 MB / 読み込み 110 秒。
//!
//! # 所有
//!
//! [`ModuleCache`] は `PluginHost` (plugin-main スレッド) が 1 つ持つ。インスタンスは
//! `Arc<…Module>` を握り、**最後のインスタンスが消えた時点で** モジュールが畳まれる
//! (SDK の shared_ptr と同じ寿命)。cache 側は `Weak` なので、生存本数を二重に数えない。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::version::clap_version_is_compatible;
use libloading::{Library, Symbol};
use vst3::{ComPtr, Steinberg::IPluginFactory};

use crate::vst3_scan::resolve_vst3_dll;

/// 読み込み済みの VST3 モジュール。
///
/// フィールドは **drop 順が契約**: `factory` (`Option`、Drop で先に落とす) → `ExitDll`
/// → `library` (= `FreeLibrary`)。SDK の `~Win32Module` と同じ順。
pub struct Vst3Module {
    /// 解決後の `.vst3` DLL 実体のパス (bundle ではない)。cache の鍵と同じ。
    path: PathBuf,
    /// `GetPluginFactory()` の戻り。`Drop` で `library` より先に release する。
    factory: Option<ComPtr<IPluginFactory>>,
    /// `Drop` で `take()` してから `ExitDll` → `FreeLibrary`。
    library: Option<Library>,
}

impl Vst3Module {
    /// `dll_path` の DLL を読み、`InitDll` と `GetPluginFactory` を 1 回ずつ呼ぶ。
    fn load(dll_path: &Path) -> Result<Self> {
        let library = unsafe { Library::new(dll_path) }
            .with_context(|| format!("LoadLibrary {}", dll_path.display()))?;

        // InitDll() は optional (VST3 3.6.x の Windows 要件)。無い = 正常。
        unsafe {
            if let Ok(init_dll) = library.get::<Symbol<extern "system" fn() -> bool>>(b"InitDll\0")
                && !init_dll()
            {
                anyhow::bail!("InitDll returned false for {}", dll_path.display());
            }
        }

        let factory_raw: *mut IPluginFactory = unsafe {
            let sym: Symbol<extern "system" fn() -> *mut IPluginFactory> = library
                .get(b"GetPluginFactory\0")
                .context("missing GetPluginFactory export")?;
            sym()
        };
        anyhow::ensure!(!factory_raw.is_null(), "GetPluginFactory returned null");
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(factory_raw) }
            .context("factory came back null via from_raw")?;

        tracing::info!(path = %dll_path.display(), "VST3 module loaded (InitDll + factory)");
        Ok(Self {
            path: dll_path.to_path_buf(),
            factory: Some(factory),
            library: Some(library),
        })
    }

    /// このモジュールの唯一の factory。全インスタンスがこれを共有する。
    pub fn factory(&self) -> &ComPtr<IPluginFactory> {
        // `Drop` の中以外では必ず `Some` (`load` が `Some` で構築する)。
        self.factory.as_ref().expect("factory taken outside Drop")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Vst3Module {
    fn drop(&mut self) {
        // SDK `~Win32Module` の順: factory を release してから ExitDll、最後に FreeLibrary。
        self.factory = None;
        if let Some(library) = self.library.take() {
            unsafe {
                if let Ok(exit_dll) =
                    library.get::<Symbol<extern "system" fn() -> bool>>(b"ExitDll\0")
                {
                    exit_dll();
                }
            }
            tracing::info!(path = %self.path.display(), "VST3 module unloaded (ExitDll + FreeLibrary)");
            drop(library);
        }
    }
}

/// 読み込み済みの CLAP モジュール。
///
/// `entry` / `factory` は DSO の静的領域を指す (host 側で所有しない)。`library` が
/// 生きている間だけ有効で、`Drop` が `deinit()` → `FreeLibrary` の順で畳む。
pub struct ClapModule {
    path: PathBuf,
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    library: Option<Library>,
}

impl ClapModule {
    fn load(path: &Path) -> Result<Self> {
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load CLAP library at {}", path.display()))?;

        let entry_ptr: *const clap_plugin_entry = unsafe {
            let sym: Symbol<*const clap_plugin_entry> = library
                .get(b"clap_entry\0")
                .context("CLAP library does not export clap_entry symbol")?;
            *sym
        };
        anyhow::ensure!(!entry_ptr.is_null(), "clap_entry symbol is null");
        let entry = unsafe { &*entry_ptr };

        anyhow::ensure!(
            clap_version_is_compatible(entry.clap_version),
            "CLAP version {}.{}.{} is incompatible with host",
            entry.clap_version.major,
            entry.clap_version.minor,
            entry.clap_version.revision
        );

        let path_str = path.to_string_lossy();
        let c_path = std::ffi::CString::new(path_str.as_bytes())
            .context("plugin path contains interior nul byte")?;
        let init_fn = entry.init.context("clap_plugin_entry::init is null")?;
        anyhow::ensure!(
            unsafe { init_fn(c_path.as_ptr()) },
            "clap_entry.init returned false for {}",
            path.display()
        );

        let get_factory = entry
            .get_factory
            .context("clap_plugin_entry::get_factory is null")?;
        let factory = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }
            as *const clap_plugin_factory;
        if factory.is_null() {
            // init() が成功した以上、deinit() を必ず対で呼んでから畳む。
            if let Some(deinit) = entry.deinit {
                unsafe { deinit() };
            }
            anyhow::bail!("clap_plugin_factory is null for {}", path.display());
        }

        tracing::info!(path = %path.display(), "CLAP module loaded (entry.init + factory)");
        Ok(Self {
            path: path.to_path_buf(),
            entry: entry_ptr,
            factory,
            library: Some(library),
        })
    }

    pub fn factory(&self) -> *const clap_plugin_factory {
        self.factory
    }

    /// DSO の静的 entry。ARA factory の問い合わせなど「モジュール全体に効く」拡張の入口。
    /// **`deinit` はここから呼ばない** — 対の呼び出しは [`Drop`] が持つ。
    pub fn entry(&self) -> *const clap_plugin_entry {
        self.entry
    }
}

// `entry` / `factory` は DSO の静的領域を指す生ポインタ。**モジュールを触るのは
// plugin-main スレッドだけ** (`PluginHost` が `ModuleCache` を所有し、インスタンスの
// 生成も `teardown_plugin` もそのスレッドで走る) で、`Arc` がスレッドを跨ぐのは
// `ClapPlugin` (これ自身が同じ理由で `unsafe impl Send`) に連れられるときだけ。
// `Drop` (= `entry.deinit`、CLAP 仕様では main-thread) も同じスレッドで起きる。
unsafe impl Send for ClapModule {}
unsafe impl Sync for ClapModule {}

impl Drop for ClapModule {
    fn drop(&mut self) {
        if let Some(library) = self.library.take() {
            // entry.h: 「for every init() which returns true, one deinit() should be called」。
            unsafe {
                if let Some(deinit) = (*self.entry).deinit {
                    deinit();
                }
            }
            tracing::info!(path = %self.path.display(), "CLAP module unloaded (entry.deinit)");
            drop(library);
        }
    }
}

/// パス → 読み込み済みモジュール。`PluginHost` が 1 つ所有する。
///
/// 値は `Weak` — 生きているインスタンスが 1 本も無くなったらモジュールも畳まれ、次の
/// 読み込みで作り直される。cache が強参照を持つと「使っていない DLL が常駐し続ける」
/// 別の負債になる。
#[derive(Default)]
pub struct ModuleCache {
    vst3: HashMap<PathBuf, Weak<Vst3Module>>,
    clap: HashMap<PathBuf, Weak<ClapModule>>,
}

impl ModuleCache {
    /// `path` (bundle でも DLL でも可) の VST3 モジュールを返す。既に読み込み済みなら
    /// **同じ** モジュール (= 同じ factory) を返す。
    pub fn vst3(&mut self, path: &Path) -> Result<Arc<Vst3Module>> {
        let dll_path = resolve_vst3_dll(path)
            .with_context(|| format!("resolving VST3 at {}", path.display()))?;
        if let Some(hit) = self.vst3.get(&dll_path).and_then(Weak::upgrade) {
            return Ok(hit);
        }
        let module = Arc::new(Vst3Module::load(&dll_path)?);
        self.vst3.insert(dll_path, Arc::downgrade(&module));
        Ok(module)
    }

    /// `path` の CLAP モジュールを返す (VST3 版と対称)。
    pub fn clap(&mut self, path: &Path) -> Result<Arc<ClapModule>> {
        let key = path.to_path_buf();
        if let Some(hit) = self.clap.get(&key).and_then(Weak::upgrade) {
            return Ok(hit);
        }
        let module = Arc::new(ClapModule::load(path)?);
        self.clap.insert(key, Arc::downgrade(&module));
        Ok(module)
    }

    /// 生存参照を失った項目を落とす。読み込みのたびに呼ぶほどではないので、
    /// プロジェクトを閉じた後など「まとめて消えた」直後に呼ぶ。
    pub fn prune(&mut self) {
        self.vst3.retain(|_, w| w.strong_count() > 0);
        self.clap.retain(|_, w| w.strong_count() > 0);
    }
}
