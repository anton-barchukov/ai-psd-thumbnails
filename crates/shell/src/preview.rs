//! PreviewProvider: implements IPreviewHandler + IInitializeWithStream +
//! IObjectWithSite + IOleWindow.
//!
//! Lifecycle (per Microsoft preview-handler contract):
//!   Initialize(stream) → SetWindow(hwnd, rect) → SetRect(rect) →
//!   DoPreview() → [SetFocus / QueryFocus / SetRect resize] → Unload()
//!
//! Decode is lazy: Initialize buffers bytes; DoPreview decodes and paints.
//!
//! RefCell re-entrancy contract:
//!   Never hold a RefCell borrow across any Win32 call, decode operation, or
//!   catch_unwind boundary.  Copy/clone values out, drop the borrow, then work.
//!   HWND/HBITMAP are stored in Cell<> (Copy) to avoid borrow overhead.
//!
//! Window lifetime contract:
//!   The child HWND is created in DoPreview and destroyed in Unload.
//!   If the host releases the COM object without calling Unload (abrupt teardown
//!   or error path) Drop destroys the window as a fallback, so the window never
//!   outlives its backing state.  destroy_preview_child() clears GWLP_USERDATA
//!   before DestroyWindow so the wndproc sees null during teardown messages.

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

use windows::core::{implement, Interface, IUnknown, Ref, Result as WinResult, GUID, HRESULT};
use windows::Win32::Foundation::{E_FAIL, E_UNEXPECTED, HWND, RECT};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus as Win32SetFocus};
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandler_Impl};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithStream, IInitializeWithStream_Impl};
use windows::Win32::UI::WindowsAndMessaging::{
    MSG, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
};

use ai_psd_thumbnails_core::{decode_thumbnail, render::to_bgra_premul};

use crate::bitmap::{create_bitmap_data, BitmapData};
use crate::factory::OBJ_COUNT;
use crate::pdfium_path::ensure_pdfium_init;
use crate::preview_window::{
    create_preview_child, destroy_preview_child, ensure_window_class_registered,
    PreviewWindowState,
};
use crate::stream_util::read_stream_all;
use crate::thumbnail::detect_ext_from_bytes;

/// Maximum dimension (pixels) for preview decode — larger than thumbnail to
/// give a crisp pane image while capping allocations on hostile input.
const PREVIEW_MAX_DIM: u32 = 1024;

// ── ALREADY-INITIALIZED sentinel HRESULT ─────────────────────────────────────
// HRESULT_FROM_WIN32(ERROR_ALREADY_INITIALIZED) = 0x800704DF
const E_ALREADY_INITIALIZED: HRESULT = HRESULT(0x800704DFu32 as i32);

// ── PreviewProvider COM object ────────────────────────────────────────────────

#[implement(IPreviewHandler, IInitializeWithStream, IObjectWithSite, IOleWindow)]
pub(crate) struct PreviewProvider {
    /// Buffered stream bytes (set in Initialize, consumed in DoPreview).
    /// `Arc` so DoPreview can hold them without copying the whole file.
    bytes: RefCell<Option<std::sync::Arc<Vec<u8>>>>,
    /// Parent HWND provided by SetWindow.
    parent_hwnd: Cell<HWND>,
    /// Child HWND we created in DoPreview.  Null until DoPreview succeeds.
    child_hwnd: Cell<HWND>,
    /// Bounding rect within the parent (SetWindow / SetRect).
    bounds: Cell<RECT>,
    /// Site object provided by the preview frame (IObjectWithSite::SetSite).
    site: RefCell<Option<IUnknown>>,
    /// Heap-allocated window state shared with the wndproc via GWLP_USERDATA.
    /// Kept in a Box so its address is stable for the life of the child HWND.
    window_state: RefCell<Option<Box<PreviewWindowState>>>,
}

impl PreviewProvider {
    pub(crate) fn new() -> Self {
        OBJ_COUNT.fetch_add(1, Ordering::SeqCst);
        Self {
            bytes: RefCell::new(None),
            parent_hwnd: Cell::new(HWND::default()),
            child_hwnd: Cell::new(HWND::default()),
            bounds: Cell::new(RECT::default()),
            site: RefCell::new(None),
            window_state: RefCell::new(None),
        }
    }

}

impl Drop for PreviewProvider {
    /// Fallback cleanup: if the host released us without calling Unload, destroy
    /// the child window now (on the STA thread — safe to call DestroyWindow here).
    fn drop(&mut self) {
        let child = self.child_hwnd.get();
        if !child.is_invalid() {
            // Clear the cell first so Unload (if somehow called concurrently)
            // cannot double-destroy.
            self.child_hwnd.set(HWND::default());
            // destroy_preview_child clears GWLP_USERDATA before DestroyWindow.
            destroy_preview_child(child);
        }
        // Drop the window_state Box after the window is gone so no live pointer
        // to it can remain in GWLP_USERDATA.
        *self.window_state.borrow_mut() = None;
        OBJ_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

// ── IInitializeWithStream ─────────────────────────────────────────────────────

impl IInitializeWithStream_Impl for PreviewProvider_Impl {
    fn Initialize(&self, pstream: Ref<IStream>, _grfmode: u32) -> WinResult<()> {
        let result = catch_unwind(AssertUnwindSafe(|| -> WinResult<()> {
            // Reject re-initialization.
            {
                let guard = self.bytes.borrow();
                if guard.is_some() {
                    return Err(windows::core::Error::from_hresult(E_ALREADY_INITIALIZED));
                }
            } // borrow dropped here

            let stream: &IStream = pstream.ok().map_err(|_| {
                windows::core::Error::from_hresult(E_UNEXPECTED)
            })?;

            let data = read_stream_all(stream)?;
            *self.bytes.borrow_mut() = Some(std::sync::Arc::new(data));
            Ok(())
        }));

        match result {
            Ok(r) => r,
            Err(_panic) => {
                log::error!("panic in PreviewProvider::Initialize");
                Err(windows::core::Error::from_hresult(E_FAIL))
            }
        }
    }
}

// ── IPreviewHandler ───────────────────────────────────────────────────────────

impl IPreviewHandler_Impl for PreviewProvider_Impl {
    fn SetWindow(
        &self,
        hwnd: HWND,
        prc: *const RECT,
    ) -> WinResult<()> {
        let result = catch_unwind(AssertUnwindSafe(|| -> WinResult<()> {
            self.parent_hwnd.set(hwnd);
            if !prc.is_null() {
                let r = unsafe { *prc };
                self.bounds.set(r);
                reposition_child(self.child_hwnd.get(), r);
            }
            Ok(())
        }));
        flatten_unwind(result)
    }

    fn SetRect(&self, prc: *const RECT) -> WinResult<()> {
        let result = catch_unwind(AssertUnwindSafe(|| -> WinResult<()> {
            if prc.is_null() {
                return Ok(());
            }
            let r = unsafe { *prc };
            self.bounds.set(r);
            let child = self.child_hwnd.get();
            reposition_child(child, r);
            if !child.is_invalid() {
                unsafe { let _ = InvalidateRect(Some(child), None, true); };
            }
            Ok(())
        }));
        flatten_unwind(result)
    }

    fn DoPreview(&self) -> WinResult<()> {
        let result = catch_unwind(AssertUnwindSafe(|| -> WinResult<()> {
            let parent = self.parent_hwnd.get();
            if parent.is_invalid() {
                return Err(windows::core::Error::from_hresult(E_UNEXPECTED));
            }

            // Idempotency guard: if a child window already exists from a previous
            // DoPreview call (misbehaving host or retry), destroy it before creating
            // a new one.  Dropping the window_state Box while the old HWND still has
            // its GWLP_USERDATA pointing at it would be a use-after-free.
            let existing_child = self.child_hwnd.get();
            if !existing_child.is_invalid() {
                // Clear the HWND cell first so Drop sees null and won't double-destroy.
                self.child_hwnd.set(HWND::default());
                // destroy_preview_child clears GWLP_USERDATA before DestroyWindow.
                destroy_preview_child(existing_child);
                // Release the old state Box only after the window is gone.
                *self.window_state.borrow_mut() = None;
            }

            // Take a handle out (no copy) — drop borrow before any decode or Win32 call.
            let owned_bytes: std::sync::Arc<Vec<u8>> = {
                let guard = self.bytes.borrow();
                match &*guard {
                    None => return Err(windows::core::Error::from_hresult(E_UNEXPECTED)),
                    Some(v) => std::sync::Arc::clone(v),
                }
                // borrow dropped here
            };

            ensure_pdfium_init();

            let hinstance = crate::our_hmodule().unwrap_or_default();
            ensure_window_class_registered(hinstance);

            // Allocate stable window state on the heap.
            let mut state_box = Box::new(PreviewWindowState { bitmap: None });

            // Decode lazily here (not in Initialize) — per preview-handler spec.
            let bitmap_result = decode_to_bitmap(&owned_bytes);
            match bitmap_result {
                Ok(bmp) => state_box.bitmap = Some(bmp),
                Err(e) => {
                    // Log but continue — DoPreview must succeed; we'll paint an
                    // empty background (the pane stays visible and clean).
                    log::warn!("DoPreview: decode failed: {e}");
                }
            }

            // Pass the stable Box pointer as lpParam so the wndproc can bind it.
            let state_ptr: *mut PreviewWindowState = &mut *state_box;
            let bounds = self.bounds.get();

            let child = create_preview_child(parent, bounds, hinstance, state_ptr)?;

            // Commit: store everything only after the window is created.
            self.child_hwnd.set(child);
            *self.window_state.borrow_mut() = Some(state_box);

            unsafe { let _ = InvalidateRect(Some(child), None, true); };
            Ok(())
        }));
        flatten_unwind(result)
    }

    fn Unload(&self) -> WinResult<()> {
        let result = catch_unwind(AssertUnwindSafe(|| -> WinResult<()> {
            let child = self.child_hwnd.get();
            if !child.is_invalid() {
                // Clear cell first so Drop sees null and does not double-destroy.
                self.child_hwnd.set(HWND::default());
                // destroy_preview_child clears GWLP_USERDATA before DestroyWindow,
                // preventing wndproc messages during teardown from accessing the
                // state we are about to free.
                destroy_preview_child(child);
            }
            // Drop the window state Box (frees BitmapData → DeleteObject).
            *self.window_state.borrow_mut() = None;
            // Clear buffered bytes.
            *self.bytes.borrow_mut() = None;
            Ok(())
        }));
        flatten_unwind(result)
    }

    fn SetFocus(&self) -> WinResult<()> {
        let child = self.child_hwnd.get();
        if !child.is_invalid() {
            unsafe { let _ = Win32SetFocus(Some(child)); }
        }
        Ok(())
    }

    fn QueryFocus(&self) -> WinResult<HWND> {
        Ok(unsafe { GetFocus() })
    }

    fn TranslateAccelerator(&self, _pmsg: *const MSG) -> WinResult<()> {
        // We do not handle accelerators; return S_FALSE via a custom HRESULT.
        // S_FALSE = 1 — the caller interprets this as "not handled".
        Err(windows::core::Error::from_hresult(windows::Win32::Foundation::S_FALSE))
    }
}

// ── IObjectWithSite ───────────────────────────────────────────────────────────

impl IObjectWithSite_Impl for PreviewProvider_Impl {
    fn SetSite(&self, punksite: Ref<IUnknown>) -> WinResult<()> {
        // Store or clear the site.  Ref<T> has is_null() and deref to Option<T>.
        // Clone the IUnknown if non-null (AddRefs), or store None to clear.
        *self.site.borrow_mut() = if punksite.is_null() {
            None
        } else {
            punksite.ok().map(|u| u.clone()).ok()
        };
        Ok(())
    }

    fn GetSite(
        &self,
        riid: *const GUID,
        ppvsite: *mut *mut std::ffi::c_void,
    ) -> WinResult<()> {
        if ppvsite.is_null() || riid.is_null() {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_POINTER,
            ));
        }
        // Clone the site IUnknown out before any QI (no borrow across unsafe).
        let site_opt: Option<IUnknown> = self.site.borrow().clone();
        match site_opt {
            None => {
                unsafe { *ppvsite = std::ptr::null_mut(); }
                Err(windows::core::Error::from_hresult(E_FAIL))
            }
            Some(site) => unsafe { Interface::query(&site, riid, ppvsite).ok() },
        }
    }
}

// ── IOleWindow ────────────────────────────────────────────────────────────────

impl IOleWindow_Impl for PreviewProvider_Impl {
    fn GetWindow(&self) -> WinResult<HWND> {
        let child = self.child_hwnd.get();
        if child.is_invalid() {
            // Before DoPreview, return the parent hwnd if we have one.
            let parent = self.parent_hwnd.get();
            if parent.is_invalid() {
                return Err(windows::core::Error::from_hresult(E_UNEXPECTED));
            }
            return Ok(parent);
        }
        Ok(child)
    }

    fn ContextSensitiveHelp(&self, _fentermode: windows::core::BOOL) -> WinResult<()> {
        // Preview handlers do not provide context-sensitive help.
        Ok(())
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Decode `bytes` into a `BitmapData` at PREVIEW_MAX_DIM resolution.
fn decode_to_bitmap(bytes: &[u8]) -> Result<BitmapData, String> {
    let ext = detect_ext_from_bytes(bytes).ok_or_else(|| "unrecognised format".to_owned())?;

    let decoded = decode_thumbnail(bytes, ext, PREVIEW_MAX_DIM)
        .map_err(|e| format!("decode_thumbnail: {e}"))?;

    let bgra = to_bgra_premul(&decoded);

    create_bitmap_data(&bgra, decoded.width, decoded.height)
        .map_err(|e| format!("create_bitmap_data: {e}"))
}

/// Reposition the child window to a new bounding rect (no-op if HWND is null).
fn reposition_child(child: HWND, r: RECT) {
    if child.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            child,
            None,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Map a `catch_unwind` result into a `WinResult<()>`, treating panics as E_FAIL.
fn flatten_unwind(r: Result<WinResult<()>, Box<dyn std::any::Any + Send>>) -> WinResult<()> {
    match r {
        Ok(inner) => inner,
        Err(_panic) => {
            log::error!("panic in PreviewProvider COM method");
            Err(windows::core::Error::from_hresult(E_FAIL))
        }
    }
}
