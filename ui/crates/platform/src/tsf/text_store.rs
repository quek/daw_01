//! `DocumentStore` — `#[implement(ITextStoreACP)]` の薄い COM shim。
//!
//! msctf (OS IME framework) が呼ぶ `ITextStoreACP` の各 method を、純粋な [`DocState`] への
//! 転送に翻訳するだけ。rtry/MS-IME が使う `ITfRange::GetText`/`SetText` 等は msctf が
//! この ACP store の上に合成する。
//!
//! **再入規律 (最重要)**: sink callback (`OnLockGranted`/`OnTextChange`/…) は IME が同期的に
//! GetText/SetText で再入してくるため、**`RefCell` borrow を保持したまま sink を呼ばない**。
//! borrow → 値変更 → drop → (sink を clone して) 呼ぶ、の順を厳守する。

// COM shim は windows API の wildcard import / `#[implement]` マクロ生成コード (inline_always 等) /
// ACP i32 cast / out-param への raw pointer 参照が不可避。これらの pedantic lint をモジュール単位で許容。
#![allow(
    clippy::wildcard_imports,
    clippy::cast_possible_wrap,
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::borrow_as_ptr,
    clippy::too_many_lines
)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::TextServices::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::*;

use super::doc_state::{DocState, Notify};
use crate::text_document::{ImeTextEdit, TextDocument};

// CONNECT_E_* は windows 0.62 の Foundation に無いので局所定義 (FACILITY_ITF)。
const CONNECT_E_NOCONNECTION: HRESULT = HRESULT(0x8004_0200_u32 as i32);
const CONNECT_E_ADVISELIMIT: HRESULT = HRESULT(0x8004_0201_u32 as i32);

/// document lock の状態 (TSF の同期ロックモデル)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LockKind {
    #[default]
    Unlocked,
    Read,
    ReadWrite,
}

/// msctf から advise された sink。
#[derive(Default)]
pub(crate) struct SinkState {
    pub sink: Option<ITextStoreACPSink>,
    pub mask: u32,
}

/// COM オブジェクトと [`super::thread_mgr::TsfManager`] が共有する状態 (UI スレッド専有)。
#[derive(Clone)]
pub(crate) struct TsfShared {
    pub doc: Rc<RefCell<DocState>>,
    pub sinks: Rc<RefCell<SinkState>>,
    pub lock: Rc<Cell<LockKind>>,
    /// IME がメッセージポンプ中に store を編集したとき再描画を促す callback。
    /// これが無いと、TIP の SetText で積んだ pending edit が「次に何か別のイベントで
    /// 再描画される」まで widget に反映されず、event-driven app (daw_01) で入力が遅延する。
    pub redraw: Rc<dyn Fn()>,
}

impl TsfShared {
    pub fn new(redraw: Rc<dyn Fn()>) -> Self {
        Self {
            doc: Rc::new(RefCell::new(DocState::new())),
            sinks: Rc::new(RefCell::new(SinkState::default())),
            lock: Rc::new(Cell::new(LockKind::Unlocked)),
            redraw,
        }
    }
}

/// 生ポインタ out-param への安全な書き込み (null は無視)。
unsafe fn write_out<T>(p: *mut T, v: T) {
    if !p.is_null() {
        unsafe { *p = v };
    }
}

/// `#[implement(ITextStoreACP)]` 本体。
#[implement(ITextStoreACP)]
pub(crate) struct DocumentStore {
    shared: TsfShared,
    hwnd: HWND,
}

impl DocumentStore {
    /// 共有状態と HWND から store を生成し、`ITextStoreACP` interface を返す。
    pub fn create(shared: TsfShared, hwnd: HWND) -> ITextStoreACP {
        DocumentStore { shared, hwnd }.into()
    }

    fn read_locked(&self) -> bool {
        self.shared.lock.get() != LockKind::Unlocked
    }
    fn write_locked(&self) -> bool {
        self.shared.lock.get() == LockKind::ReadWrite
    }

    /// publish された caret rect (window client px) を screen 座標の `RECT` に変換する。
    /// 候補ウィンドウ / ストロークヘルプを caret 位置に出すために `GetTextExt` が返す。
    fn caret_screen_rect(&self) -> RECT {
        let c = self.shared.doc.borrow().caret();
        let mut tl = POINT { x: c.x as i32, y: c.y as i32 };
        let mut br = POINT { x: (c.x + c.w) as i32, y: (c.y + c.h) as i32 };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &raw mut tl);
            let _ = ClientToScreen(self.hwnd, &raw mut br);
        }
        RECT {
            left: tl.x,
            top: tl.y,
            right: br.x.max(tl.x + 1),
            bottom: br.y.max(tl.y + 1),
        }
    }
}

impl ITextStoreACP_Impl for DocumentStore_Impl {
    fn AdviseSink(&self, riid: *const GUID, punk: Ref<IUnknown>, dwmask: u32) -> Result<()> {
        if riid.is_null() || unsafe { *riid } != ITextStoreACPSink::IID {
            return Err(E_INVALIDARG.into());
        }
        let unk = punk.clone().ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        let mut slot = self.shared.sinks.borrow_mut();
        if let Some(existing) = slot.sink.as_ref() {
            let existing_unk: IUnknown = existing.cast()?;
            if existing_unk.as_raw() != unk.as_raw() {
                return Err(Error::from_hresult(CONNECT_E_ADVISELIMIT));
            }
        }
        slot.sink = Some(unk.cast()?);
        slot.mask = dwmask;
        Ok(())
    }

    fn UnadviseSink(&self, punk: Ref<IUnknown>) -> Result<()> {
        let unk = punk.clone().ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        let mut slot = self.shared.sinks.borrow_mut();
        let matches = slot
            .sink
            .as_ref()
            .and_then(|s| s.cast::<IUnknown>().ok())
            .is_some_and(|u| u.as_raw() == unk.as_raw());
        if matches {
            slot.sink = None;
            slot.mask = 0;
            Ok(())
        } else {
            Err(Error::from_hresult(CONNECT_E_NOCONNECTION))
        }
    }

    fn RequestLock(&self, dwlockflags: u32) -> Result<HRESULT> {
        // sink を clone して borrow を持ち越さない (OnLockGranted が再入する)。
        let sink = self.shared.sinks.borrow().sink.clone();
        let Some(sink) = sink else { return Err(E_FAIL.into()) };
        if self.shared.lock.get() != LockKind::Unlocked {
            // 同期ロックのみ対応 → 再入ロック要求は同期不可を返す。
            return Ok(TS_E_SYNCHRONOUS);
        }
        let want_write = (dwlockflags & TS_LF_READWRITE.0) == TS_LF_READWRITE.0;
        self.shared
            .lock
            .set(if want_write { LockKind::ReadWrite } else { LockKind::Read });
        // borrow は一切持たずに sink を呼ぶ (IME がここで GetText/SetText を同期再入する)。
        let session = unsafe { sink.OnLockGranted(TEXT_STORE_LOCK_FLAGS(dwlockflags)) };
        self.shared.lock.set(LockKind::Unlocked);
        // RequestLock の戻り値 (Ok の中身) は edit session の結果 HRESULT として phrSession に
        // 書かれる。OnLockGranted が失敗したら成功偽装せずその HRESULT を返す。
        match session {
            Ok(()) => Ok(S_OK),
            Err(e) => Ok(e.code()),
        }
    }

    fn GetStatus(&self) -> Result<TS_STATUS> {
        let active = self.shared.doc.borrow().active();
        Ok(TS_STATUS {
            dwDynamicFlags: if active { 0 } else { TS_SD_READONLY },
            dwStaticFlags: TS_SS_NOHIDDENTEXT,
        })
    }

    fn QueryInsert(
        &self,
        acpteststart: i32,
        acptestend: i32,
        _cch: u32,
        pacpresultstart: *mut i32,
        pacpresultend: *mut i32,
    ) -> Result<()> {
        let end = self.shared.doc.borrow().end_acp();
        let s = acpteststart.clamp(0, end);
        let e = acptestend.clamp(s, end);
        unsafe {
            write_out(pacpresultstart, s);
            write_out(pacpresultend, e);
        }
        Ok(())
    }

    fn GetSelection(
        &self,
        _ulindex: u32,
        ulcount: u32,
        pselection: *mut TS_SELECTION_ACP,
        pcfetched: *mut u32,
    ) -> Result<()> {
        if !self.read_locked() {
            return Err(TS_E_NOLOCK.into());
        }
        unsafe { write_out(pcfetched, 0) };
        if ulcount == 0 {
            return Ok(());
        }
        let (s, e, reversed) = self.shared.doc.borrow().selection_acp();
        let ase = if reversed { TS_AE_START } else { TS_AE_END };
        unsafe {
            write_out(
                pselection,
                TS_SELECTION_ACP {
                    acpStart: s,
                    acpEnd: e,
                    style: TS_SELECTIONSTYLE { ase, fInterimChar: BOOL(0) },
                },
            );
            write_out(pcfetched, 1);
        }
        Ok(())
    }

    fn SetSelection(&self, ulcount: u32, pselection: *const TS_SELECTION_ACP) -> Result<()> {
        if !self.write_locked() {
            return Err(TS_E_NOLOCK.into());
        }
        if ulcount == 0 || pselection.is_null() {
            return Ok(());
        }
        let sel = unsafe { *pselection };
        let reversed = sel.style.ase.0 == TS_AE_START.0;
        self.shared
            .doc
            .borrow_mut()
            .set_selection_acp(sel.acpStart, sel.acpEnd, reversed);
        (self.shared.redraw)();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn GetText(
        &self,
        acpstart: i32,
        acpend: i32,
        pchplain: PWSTR,
        cchplainreq: u32,
        pcchplainret: *mut u32,
        prgruninfo: *mut TS_RUNINFO,
        cruninforeq: u32,
        pcruninforet: *mut u32,
        pacpnext: *mut i32,
    ) -> Result<()> {
        if !self.read_locked() {
            return Err(TS_E_NOLOCK.into());
        }
        let doc = self.shared.doc.borrow();
        let end_acp = doc.end_acp();
        let start = acpstart.max(0).min(end_acp);
        let slice = doc.text_utf16_range(acpstart, acpend);
        let n = slice.len().min(cchplainreq as usize);
        unsafe {
            if !pchplain.is_null() && n > 0 {
                std::ptr::copy_nonoverlapping(slice.as_ptr(), pchplain.0, n);
            }
            write_out(pcchplainret, n as u32);
            if cruninforeq > 0 && !prgruninfo.is_null() {
                write_out(
                    prgruninfo,
                    TS_RUNINFO { uCount: n as u32, r#type: TS_RT_PLAIN },
                );
                write_out(pcruninforet, u32::from(n > 0));
            } else {
                write_out(pcruninforet, 0);
            }
            write_out(pacpnext, start + n as i32);
        }
        Ok(())
    }

    fn SetText(
        &self,
        _dwflags: u32,
        acpstart: i32,
        acpend: i32,
        pchtext: &PCWSTR,
        cch: u32,
    ) -> Result<TS_TEXTCHANGE> {
        if !self.write_locked() {
            return Err(TS_E_NOLOCK.into());
        }
        let new16: &[u16] = if cch == 0 || pchtext.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(pchtext.0, cch as usize) }
        };
        let (s, old_e, new_e) = self.shared.doc.borrow_mut().set_text_acp(acpstart, acpend, new16);
        (self.shared.redraw)(); // pending edit を次フレームで drain させる
        Ok(TS_TEXTCHANGE { acpStart: s, acpOldEnd: old_e, acpNewEnd: new_e })
    }

    fn GetFormattedText(
        &self,
        _acpstart: i32,
        _acpend: i32,
    ) -> Result<windows::Win32::System::Com::IDataObject> {
        Err(E_NOTIMPL.into())
    }

    fn GetEmbedded(
        &self,
        _acppos: i32,
        _rguidservice: *const GUID,
        _riid: *const GUID,
    ) -> Result<IUnknown> {
        Err(E_NOTIMPL.into())
    }

    fn QueryInsertEmbedded(
        &self,
        _pguidservice: *const GUID,
        _pformatetc: *const windows::Win32::System::Com::FORMATETC,
    ) -> Result<BOOL> {
        Ok(BOOL(0))
    }

    fn InsertEmbedded(
        &self,
        _dwflags: u32,
        _acpstart: i32,
        _acpend: i32,
        _pdataobject: Ref<windows::Win32::System::Com::IDataObject>,
    ) -> Result<TS_TEXTCHANGE> {
        Err(E_NOTIMPL.into())
    }

    fn InsertTextAtSelection(
        &self,
        dwflags: u32,
        pchtext: &PCWSTR,
        cch: u32,
        pacpstart: *mut i32,
        pacpend: *mut i32,
        pchange: *mut TS_TEXTCHANGE,
    ) -> Result<()> {
        if dwflags & TS_IAS_QUERYONLY != 0 {
            // 挿入せず現在選択の範囲だけ返す (rtry が StartComposition 前に呼ぶ)。
            let (s, e) = self.shared.doc.borrow().query_insert_at_selection();
            unsafe {
                write_out(pacpstart, s);
                write_out(pacpend, e);
            }
            return Ok(());
        }
        if !self.write_locked() {
            return Err(TS_E_NOLOCK.into());
        }
        let new16: &[u16] = if cch == 0 || pchtext.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(pchtext.0, cch as usize) }
        };
        let (start, end, (cs, coe, cne)) =
            self.shared.doc.borrow_mut().insert_at_selection_acp(new16);
        (self.shared.redraw)();
        unsafe {
            write_out(pacpstart, start);
            write_out(pacpend, end);
            write_out(
                pchange,
                TS_TEXTCHANGE { acpStart: cs, acpOldEnd: coe, acpNewEnd: cne },
            );
        }
        Ok(())
    }

    fn InsertEmbeddedAtSelection(
        &self,
        _dwflags: u32,
        _pdataobject: Ref<windows::Win32::System::Com::IDataObject>,
        _pacpstart: *mut i32,
        _pacpend: *mut i32,
        _pchange: *mut TS_TEXTCHANGE,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn RequestSupportedAttrs(
        &self,
        _dwflags: u32,
        _cfilterattrs: u32,
        _pafilterattrs: *const GUID,
    ) -> Result<()> {
        Ok(())
    }

    fn RequestAttrsAtPosition(
        &self,
        _acppos: i32,
        _cfilterattrs: u32,
        _pafilterattrs: *const GUID,
        _dwflags: u32,
    ) -> Result<()> {
        Ok(())
    }

    fn RequestAttrsTransitioningAtPosition(
        &self,
        _acppos: i32,
        _cfilterattrs: u32,
        _pafilterattrs: *const GUID,
        _dwflags: u32,
    ) -> Result<()> {
        Ok(())
    }

    fn FindNextAttrTransition(
        &self,
        acpstart: i32,
        acphalt: i32,
        _cfilterattrs: u32,
        _pafilterattrs: *const GUID,
        _dwflags: u32,
        pacpnext: *mut i32,
        pffound: *mut BOOL,
        plfoundoffset: *mut i32,
    ) -> Result<()> {
        // 一様な plain text なので transition 無し。
        let _ = acpstart;
        unsafe {
            write_out(pacpnext, acphalt);
            write_out(pffound, BOOL(0));
            write_out(plfoundoffset, 0);
        }
        Ok(())
    }

    fn RetrieveRequestedAttrs(
        &self,
        _ulcount: u32,
        _paattrvals: *mut TS_ATTRVAL,
        pcfetched: *mut u32,
    ) -> Result<()> {
        unsafe { write_out(pcfetched, 0) };
        Ok(())
    }

    fn GetEndACP(&self) -> Result<i32> {
        if !self.read_locked() {
            return Err(TS_E_NOLOCK.into());
        }
        Ok(self.shared.doc.borrow().end_acp())
    }

    fn GetActiveView(&self) -> Result<u32> {
        Ok(0)
    }

    fn GetACPFromPoint(
        &self,
        _vcview: u32,
        _ptscreen: *const POINT,
        _dwflags: u32,
    ) -> Result<i32> {
        // P3: CaretResolver の逆 hit-test。P1 は未対応。
        Err(TS_E_NOLAYOUT.into())
    }

    fn GetTextExt(
        &self,
        _vcview: u32,
        _acpstart: i32,
        _acpend: i32,
        prc: *mut RECT,
        pfclipped: *mut BOOL,
    ) -> Result<()> {
        // 編集対象が無ければ layout 無し。あれば caret の screen rect を返す
        // (単一行なので range を問わず caret 位置で近似 → 候補窓は caret 直下に出る)。
        if !self.shared.doc.borrow().active() {
            return Err(TS_E_NOLAYOUT.into());
        }
        let rc = self.caret_screen_rect();
        unsafe {
            write_out(prc, rc);
            write_out(pfclipped, BOOL(0));
        }
        Ok(())
    }

    fn GetScreenExt(&self, _vcview: u32) -> Result<RECT> {
        // 編集領域の screen 上の外接矩形 (候補窓 clamp 用)。window の client 全体を返す。
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &raw mut rc);
        }
        let mut tl = POINT { x: rc.left, y: rc.top };
        let mut br = POINT { x: rc.right, y: rc.bottom };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &raw mut tl);
            let _ = ClientToScreen(self.hwnd, &raw mut br);
        }
        Ok(RECT { left: tl.x, top: tl.y, right: br.x, bottom: br.y })
    }

    fn GetWnd(&self, _vcview: u32) -> Result<HWND> {
        Ok(self.hwnd)
    }
}

/// app の publish した snapshot を store へ反映し、必要なら sink へ変更通知を出す。
///
/// 再入規律: `DocState` borrow は publish 計算のみで保持し、sink 呼び出しは borrow を
/// drop してから (sink は clone で取り出す)。lock 中は通知を遅延する。
pub(crate) fn republish(shared: &TsfShared, doc: Option<&TextDocument>) {
    // lock 保持中 (理論上は frame flush 経路なので来ない) は publish だけ行い通知は次回へ。
    if shared.lock.get() != LockKind::Unlocked {
        shared.doc.borrow_mut().publish(doc);
        return;
    }
    let (notify, change): (Notify, TS_TEXTCHANGE) = {
        let mut d = shared.doc.borrow_mut();
        let old_end = d.end_acp();
        d.publish(doc);
        let new_end = d.end_acp();
        (
            d.take_notify(),
            TS_TEXTCHANGE { acpStart: 0, acpOldEnd: old_end, acpNewEnd: new_end },
        )
    };
    if notify.is_empty() {
        return;
    }
    // sink を clone して borrow を持ち越さない。
    let (sink, mask) = {
        let s = shared.sinks.borrow();
        (s.sink.clone(), s.mask)
    };
    let Some(sink) = sink else { return };
    unsafe {
        if notify.text && (mask & TS_AS_TEXT_CHANGE) != 0 {
            let _ = sink.OnTextChange(TEXT_STORE_TEXT_CHANGE_FLAGS(0), &change);
        }
        if notify.selection && (mask & TS_AS_SEL_CHANGE) != 0 {
            let _ = sink.OnSelectionChange();
        }
        if notify.layout && (mask & TS_AS_LAYOUT_CHANGE) != 0 {
            let _ = sink.OnLayoutChange(TS_LC_CHANGE, 0);
        }
    }
}

/// IME が store に積んだ編集を byte 空間で取り出す (`frame()` 先頭で widget に流す)。
pub(crate) fn take_ime_edits(shared: &TsfShared) -> Vec<ImeTextEdit> {
    shared.doc.borrow_mut().take_pending_edits()
}
