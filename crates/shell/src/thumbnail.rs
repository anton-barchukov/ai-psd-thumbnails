//! ThumbProvider: implements IThumbnailProvider + IInitializeWithStream.
//!
//! RefCell soundness contract:
//!   GetThumbnail clones the `Arc` handle (not the file bytes) OUT of the RefCell
//!   borrow, then drops the borrow, BEFORE calling any decoder or catch_unwind.
//!   Sharing via `Arc` avoids a second full copy of the file, which for a
//!   250 MB PSD would otherwise double the surrogate's peak memory.  This means no RefCell borrow is live
//!   across the unwind boundary, preventing broken-invariant leaks if a decoder panics.
//!
//! IInitializeWithStream::Initialize rejects a second call (already-initialized guard).
//! Stream bytes are capped at 2 GB by read_stream_all (stream_util).

use std::cell::RefCell;
use std::sync::Arc;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

use windows::core::{implement, Ref, Result as WinResult, HRESULT};
use windows::Win32::Foundation::{E_FAIL, E_UNEXPECTED};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::IStream;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTS_ALPHATYPE, WTSAT_ARGB,
};

use ai_psd_thumbnails_core::{decode_thumbnail, detect, render::to_bgra_premul};

use crate::bitmap::create_hbitmap;
use crate::factory::OBJ_COUNT;
use crate::pdfium_path::ensure_pdfium_init;
use crate::stream_util::read_stream_all;

// ── ThumbProvider COM object ───────────────────────────────────────────────────

#[implement(IThumbnailProvider, IInitializeWithStream)]
pub(crate) struct ThumbProvider {
    /// Raw file bytes after Initialize; None until initialized.
    bytes: RefCell<Option<Arc<Vec<u8>>>>,
}

impl ThumbProvider {
    pub(crate) fn new() -> Self {
        OBJ_COUNT.fetch_add(1, Ordering::SeqCst);
        Self { bytes: RefCell::new(None) }
    }
}

impl Drop for ThumbProvider {
    fn drop(&mut self) {
        OBJ_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

// ── IInitializeWithStream ─────────────────────────────────────────────────────

impl IInitializeWithStream_Impl for ThumbProvider_Impl {
    fn Initialize(&self, pstream: Ref<IStream>, _grfmode: u32) -> WinResult<()> {
        let result = catch_unwind(AssertUnwindSafe(|| -> WinResult<()> {
            // Reject second initialization — shell should not call this twice, but guard anyway.
            {
                let guard = self.bytes.borrow();
                if guard.is_some() {
                    // HRESULT_FROM_WIN32(ERROR_ALREADY_INITIALIZED) = 0x800704DF
                    return Err(windows::core::Error::from_hresult(
                        HRESULT(0x800704DFu32 as i32),
                    ));
                }
            } // borrow dropped here

            // Ref<IStream>::ok() yields &IStream or E_POINTER if null.
            let stream: &IStream = pstream.ok().map_err(|_| {
                windows::core::Error::from_hresult(E_UNEXPECTED)
            })?;

            // Drain the stream using the shared helper (DRY — same logic as preview).
            let data = read_stream_all(stream)?;
            *self.bytes.borrow_mut() = Some(Arc::new(data));
            Ok(())
        }));

        match result {
            Ok(r) => r,
            Err(_panic) => {
                log::error!("panic in ThumbProvider::Initialize");
                Err(windows::core::Error::from_hresult(E_FAIL))
            }
        }
    }
}

// ── IThumbnailProvider ────────────────────────────────────────────────────────

impl IThumbnailProvider_Impl for ThumbProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> WinResult<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(windows::core::Error::from_hresult(E_UNEXPECTED));
        }

        // --- RefCell soundness: take a handle OUT, drop the borrow, THEN decode ---
        // No RefCell borrow must be live across catch_unwind or any decoder call.
        let owned_bytes: Arc<Vec<u8>> = {
            let guard = self.bytes.borrow();
            match &*guard {
                None => return Err(windows::core::Error::from_hresult(E_UNEXPECTED)),
                Some(v) => Arc::clone(v),
            }
            // guard drops here — borrow released before any decode work
        };

        // Wrap the borrow-free decode body in catch_unwind.
        // A panic in any decoder (including pdfium init) is caught and returned as E_FAIL.
        // ensure_pdfium_init is called INSIDE catch_unwind so any panic it raises
        // is contained here rather than escaping across the COM vtable boundary.
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<(HBITMAP, WTS_ALPHATYPE), HRESULT> {
            // Initialise pdfium if not yet done (no-op after first call; skips if DLL absent).
            ensure_pdfium_init();

            let ext = detect_ext_from_bytes(&owned_bytes).ok_or(E_FAIL)?;

            let decoded = decode_thumbnail(&owned_bytes, ext, cx).map_err(|e| {
                log::warn!("decode_thumbnail: {e}");
                E_FAIL
            })?;

            let bgra = to_bgra_premul(&decoded);

            let hbmp = create_hbitmap(&bgra, decoded.width, decoded.height).map_err(|e| {
                log::warn!("create_hbitmap: {e}");
                E_FAIL
            })?;

            Ok((hbmp, WTSAT_ARGB))
        }));

        match result {
            Err(_panic) => {
                log::error!("panic in GetThumbnail");
                Err(windows::core::Error::from_hresult(E_FAIL))
            }
            Ok(Err(hr)) => Err(windows::core::Error::from_hresult(hr)),
            Ok(Ok((hbmp, alpha))) => {
                unsafe {
                    *phbmp = hbmp;
                    *pdwalpha = alpha;
                }
                Ok(())
            }
        }
    }
}

// ── Format detection ──────────────────────────────────────────────────────────

/// Detect format from magic bytes.  Returns None for unrecognised data.
pub(crate) fn detect_ext_from_bytes(data: &[u8]) -> Option<ai_psd_thumbnails_core::Ext> {
    if detect::is_8bps(data) {
        // PSD = version field 0x0001; PSB = 0x0002 (bytes 4-5, big-endian).
        if data.len() >= 6 && data[5] == 2 {
            return Some(ai_psd_thumbnails_core::Ext::Psb);
        }
        return Some(ai_psd_thumbnails_core::Ext::Psd);
    }
    if detect::is_dos_eps(data) || detect::is_ps_adobe(data) {
        return Some(ai_psd_thumbnails_core::Ext::Eps);
    }
    if detect::is_pdf(data) {
        // Both PDF-native .ai and pure PDF map to Ext::Ai (ai::decode handles PDF via pdf::render_page1).
        return Some(ai_psd_thumbnails_core::Ext::Ai);
    }
    if detect::is_svg(data) {
        return Some(ai_psd_thumbnails_core::Ext::Svg);
    }
    None
}
