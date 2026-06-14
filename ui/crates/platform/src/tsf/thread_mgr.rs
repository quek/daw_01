//! `TsfManager` — TSF client 側のライフサイクル管理。
//!
//! `CoInitializeEx(STA)` → `CoCreateInstance(CLSID_TF_ThreadMgr)` → `Activate` →
//! `CreateDocumentMgr` → `CreateContext(ITextStoreACP)` → `Push` を組み、text field の
//! focus 取得/喪失で `SetFocus(doc_mgr / empty_doc_mgr)` を切り替える。
//!
//! すべて UI スレッド (winit イベントループ) 上で動く。`CoInitializeEx` が `RPC_E_CHANGED_MODE`
//! (既に MTA) を返したら TSF を諦め、呼び出し側 (winit backend) は winit IMM に fallback する。

// windows API の wildcard import を許容 (COM 連携)。
#![allow(clippy::wildcard_imports)]

use std::cell::Cell;
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::TextServices::*;
use windows::core::*;

use super::text_store::{DocumentStore, TsfShared, republish, take_ime_edits};
use crate::text_document::{ImeTextEdit, TextDocument};

/// 1 ウィンドウ分の TSF context を保持し、document の publish / focus / edit drain を担う。
pub struct TsfManager {
    shared: TsfShared,
    thread_mgr: ITfThreadMgr,
    doc_mgr: ITfDocumentMgr,
    empty_doc_mgr: ITfDocumentMgr,
    _context: ITfContext,
    _store: ITextStoreACP,
    hwnd: HWND,
    focused: Cell<bool>,
    /// **最後の field** に置く: Rust は `Drop::drop` body を先に実行し、その後 field を宣言順に
    /// drop する (= 各 COM interface の `Release`)。`CoUninitialize` を `Drop::drop` body で呼ぶと
    /// interface の `Release` より **先**に apartment を壊して UB になる。これを最後の field の
    /// Drop で行うことで「全 interface Release → CoUninitialize」 の順を保証する。
    _apartment: ApartmentGuard,
}

/// `CoInitializeEx` の balance を、全 COM interface の `Release` 後に行うための guard。
struct ApartmentGuard {
    did_coinit: bool,
}

impl Drop for ApartmentGuard {
    fn drop(&mut self) {
        if self.did_coinit {
            unsafe { CoUninitialize() };
        }
    }
}

impl TsfManager {
    /// TSF を初期化して text store を context に push する。
    ///
    /// `redraw` は IME がメッセージポンプ中に store を編集した直後に呼ばれ、次フレームでの
    /// pending edit drain を促す (event-driven app での入力遅延防止)。
    ///
    /// # Errors
    /// apartment 衝突 (`RPC_E_CHANGED_MODE`) や msctf の生成失敗時。呼び出し側は IMM へ fallback する。
    pub fn new(hwnd: HWND, redraw: Rc<dyn Fn()>) -> Result<Self> {
        let did_coinit = unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if hr == RPC_E_CHANGED_MODE {
                // このスレッドは既に MTA。STA 専有の TSF は使えない → IMM fallback。
                return Err(Error::from_hresult(hr));
            }
            // S_OK / S_FALSE どちらも自分の CoInitialize は CoUninitialize で balance する。
            hr == S_OK || hr == S_FALSE
        };

        // guard を **最初**の local に宣言する: 途中の `?` で早期 return しても、local は
        // 逆宣言順に drop = COM interface (thread_mgr 等) が Release された後に guard が
        // CoUninitialize する。成功時は全 local が Self に move され、Self の最後の field
        // (_apartment) として同じ順序保証を引き継ぐ。
        let apartment = ApartmentGuard { did_coinit };

        let thread_mgr: ITfThreadMgr =
            unsafe { CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)? };
        let client_id = unsafe { thread_mgr.Activate()? };
        let doc_mgr = unsafe { thread_mgr.CreateDocumentMgr()? };
        let empty_doc_mgr = unsafe { thread_mgr.CreateDocumentMgr()? };

        let shared = TsfShared::new(redraw);
        let store = DocumentStore::create(shared.clone(), hwnd);
        let store_unk: IUnknown = store.cast()?;

        let mut context: Option<ITfContext> = None;
        let mut edit_cookie = 0u32;
        unsafe {
            doc_mgr.CreateContext(client_id, 0, &store_unk, &raw mut context, &raw mut edit_cookie)?;
        }
        let context = context.ok_or_else(|| Error::from_hresult(E_FAIL))?;
        unsafe { doc_mgr.Push(&context)? };

        Ok(Self {
            shared,
            thread_mgr,
            doc_mgr,
            empty_doc_mgr,
            _context: context,
            _store: store,
            hwnd,
            focused: Cell::new(false),
            _apartment: apartment,
        })
    }

    /// app の publish した document を反映し、focus を切り替える。
    /// `Some(doc)` で content/selection/caret 更新 + (初回) `SetFocus(doc_mgr)`。
    /// `None` で focus を空 doc_mgr に移し store を空にする (IME 非アクティブ化)。
    pub fn set_document(&self, doc: Option<&TextDocument>) {
        if doc.is_some() {
            republish(&self.shared, doc);
            if !self.focused.get() {
                unsafe {
                    // AssociateFocus: 我々の doc を HWND に **束縛** する。これが無いと
                    // window が OS focus を得たとき msctf は CUAS の既定 document を使い、
                    // TIP (rtry) の編集が我々の ITextStoreACP に届かない (ShiftStart=0 / GetText 失敗)。
                    let _ = self.thread_mgr.AssociateFocus(self.hwnd, &self.doc_mgr);
                    // SetFocus: 既に focus 中の window へ即時反映する (AssociateFocus は次の
                    // focus 変化で効くため)。
                    let _ = self.thread_mgr.SetFocus(&self.doc_mgr);
                }
                self.focused.set(true);
            }
        } else {
            if self.focused.get() {
                unsafe {
                    let _ = self.thread_mgr.AssociateFocus(self.hwnd, &self.empty_doc_mgr);
                    let _ = self.thread_mgr.SetFocus(&self.empty_doc_mgr);
                }
                self.focused.set(false);
            }
            republish(&self.shared, None);
        }
    }

    /// IME がこのフレームに store へ加えた編集を byte 空間で取り出す。
    pub fn take_ime_edits(&self) -> Vec<ImeTextEdit> {
        take_ime_edits(&self.shared)
    }
}

impl Drop for TsfManager {
    fn drop(&mut self) {
        // interface 生存中に Pop / Deactivate。CoUninitialize は _apartment field の Drop が
        // 全 interface の Release 後に実行する (struct コメント参照)。
        unsafe {
            let _ = self.doc_mgr.Pop(0);
            let _ = self.thread_mgr.Deactivate();
        }
    }
}
