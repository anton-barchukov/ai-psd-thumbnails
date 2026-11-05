//! DllRegisterServer / DllUnregisterServer — write / delete Shell registry keys.
//!
//! Registration invariants (anti-SageThumbs):
//!   Writes ONLY to our own keys:
//!     HKCR\CLSID\{CLSID-A}\InprocServer32         (thumbnail; DLL path + ThreadingModel)
//!     HKCR\CLSID\{CLSID-B}\InprocServer32         (preview; DLL path + ThreadingModel)
//!     HKCR\CLSID\{CLSID-B}  AppID named value     (= {6d2b5079-...} prevhost surrogate)
//!     HKCR\SystemFileAssociations\.<ext>\ShellEx\{e357fccd-...}   (thumbnail, 5 exts)
//!     HKCR\SystemFileAssociations\.<ext>\shellex\{8895b1c6-...}   (preview, same 5 exts)
//!     HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\PreviewHandlers  (preview display name)
//!   NEVER creates, overwrites, or deletes:
//!     HKCR\AppID\{6d2b5079-2f0b-48dd-ab7f-97cec514d30b}  — OS-owned prevhost key.
//!       Touching it would break every preview handler machine-wide.
//!   NEVER touches HKCR\.<ext> default value, ProgID, or OpenWith.
//!   ThreadingModel = "Apartment" (STA) for both CLSIDs.
//!
//! DllRegisterServer / DllUnregisterServer are invoked by dev-register.ps1
//! (via regsvr32).  Production registration is the MSI's responsibility.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, S_OK};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteKeyW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_SZ,
};

use crate::factory::{CLSID_PREVIEW, CLSID_THUMB};

// ── Shell-extension GUIDs (Windows-defined, fixed) ────────────────────────────

/// Thumbnail handler ShellEx sub-key GUID.
const THUMB_SHELLEXT_GUID: &str = "{e357fccd-a995-4576-b01f-234630154e96}";

/// Preview handler ShellEx sub-key GUID.
const PREVIEW_SHELLEXT_GUID: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";

/// AppID value to set on CLSID-B key — points to the OS-owned prevhost surrogate.
/// We only SET this as a named VALUE on our CLSID-B key; we NEVER create or
/// delete the HKCR\AppID\{...} key itself.
const PREVHOST_APPID: &str = "{6d2b5079-2f0b-48dd-ab7f-97cec514d30b}";

/// Extensions to register the THUMBNAIL handler for. `.pdf` is deliberately
/// absent: PDFs stay with the OS / PowerToys / Acrobat handlers so that
/// arbitrary downloaded PDFs are never auto-parsed by our pinned pdfium build
/// (which does not self-update). PDF-based `.ai` files still render via pdfium.
const THUMB_EXTS: &[&str] = &[".ai", ".psd", ".psb", ".eps", ".svg"];

/// Extensions to register the PREVIEW handler for. Deliberately excludes `.pdf`:
/// the OS/Edge preview handler occupies the ext-level shellex slot on every Win11
/// machine, so a `.pdf` preview registration here would be dead/shadowed. YAGNI.
const PREVIEW_EXTS: &[&str] = &[".ai", ".psd", ".psb", ".eps", ".svg"];

/// Preview handler display name shown in the Windows PreviewHandlers list.
const PREVIEW_DISPLAY_NAME: &str = "ai-psd-thumbnails Preview";

/// Registry path for the system preview-handlers list (HKLM).
const PREVIEW_HANDLERS_KEY: &str =
    "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\PreviewHandlers";

// ── Helpers ───────────────────────────────────────────────────────────────────

fn win32_to_hresult(code: u32) -> HRESULT {
    if code == ERROR_SUCCESS.0 {
        S_OK
    } else {
        HRESULT(((code & 0xFFFF) | 0x80070000) as i32)
    }
}

/// Encode a `&str` as null-terminated UTF-16.
fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encode a `&str` as little-endian UTF-16 bytes (including null terminator)
/// for use as REG_SZ data in RegSetValueExW.
fn wide_bytes_sz(s: &str) -> Vec<u8> {
    wide_null(s).iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// Open-or-create a key under HKCR.  Caller must RegCloseKey the result.
unsafe fn create_key_hkcr(subkey: &str) -> Result<HKEY, HRESULT> {
    let w = wide_null(subkey);
    let mut hkey = HKEY::default();
    let rc = unsafe { RegCreateKeyW(HKEY_CLASSES_ROOT, PCWSTR(w.as_ptr()), &mut hkey) };
    if rc == ERROR_SUCCESS {
        Ok(hkey)
    } else {
        Err(win32_to_hresult(rc.0))
    }
}

/// Open-or-create a key under HKLM.  Caller must RegCloseKey the result.
unsafe fn create_key_hklm(subkey: &str) -> Result<HKEY, HRESULT> {
    let w = wide_null(subkey);
    let mut hkey = HKEY::default();
    let rc = unsafe { RegCreateKeyW(HKEY_LOCAL_MACHINE, PCWSTR(w.as_ptr()), &mut hkey) };
    if rc == ERROR_SUCCESS {
        Ok(hkey)
    } else {
        Err(win32_to_hresult(rc.0))
    }
}

/// Set a named REG_SZ value on an open key.
unsafe fn set_sz(hkey: HKEY, name: &str, value: &str) -> Result<(), HRESULT> {
    let wname = wide_null(name);
    let bytes = wide_bytes_sz(value);
    let rc = unsafe {
        RegSetValueExW(hkey, PCWSTR(wname.as_ptr()), None, REG_SZ, Some(bytes.as_slice()))
    };
    if rc == ERROR_SUCCESS { Ok(()) } else { Err(win32_to_hresult(rc.0)) }
}

/// Set the default (unnamed) REG_SZ value on an open key.
unsafe fn set_default(hkey: HKEY, value: &str) -> Result<(), HRESULT> {
    unsafe { set_sz(hkey, "", value) }
}

/// Delete a registry key under HKCR (non-recursive; must have no subkeys).
/// Missing keys are silently ignored (idempotent).
unsafe fn delete_key_hkcr(subkey: &str) -> Result<(), HRESULT> {
    let w = wide_null(subkey);
    let rc = unsafe { RegDeleteKeyW(HKEY_CLASSES_ROOT, PCWSTR(w.as_ptr())) };
    // 2 = ERROR_FILE_NOT_FOUND — already absent, treat as success.
    if rc == ERROR_SUCCESS || rc.0 == 2 {
        Ok(())
    } else {
        Err(win32_to_hresult(rc.0))
    }
}

/// Delete a named value from a key under HKLM.
/// Missing value is silently ignored (idempotent).
unsafe fn delete_value_hklm(subkey: &str, value_name: &str) -> Result<(), HRESULT> {
    let wkey = wide_null(subkey);
    let mut hkey = HKEY::default();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(wkey.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    // ERROR_FILE_NOT_FOUND (2) — key absent, nothing to delete.
    if rc.0 == 2 {
        return Ok(());
    }
    if rc != ERROR_SUCCESS {
        return Err(win32_to_hresult(rc.0));
    }

    let wname = wide_null(value_name);
    let rc2 = unsafe {
        windows::Win32::System::Registry::RegDeleteValueW(hkey, PCWSTR(wname.as_ptr()))
    };
    unsafe { let _ = RegCloseKey(hkey); }

    // ERROR_FILE_NOT_FOUND (2) — value absent, treat as success.
    if rc2 == ERROR_SUCCESS || rc2.0 == 2 {
        Ok(())
    } else {
        Err(win32_to_hresult(rc2.0))
    }
}

// ── CLSID string formatting ───────────────────────────────────────────────────

fn format_clsid(d: &windows::core::GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d.data1, d.data2, d.data3,
        d.data4[0], d.data4[1],
        d.data4[2], d.data4[3], d.data4[4], d.data4[5], d.data4[6], d.data4[7],
    )
}

fn clsid_thumb_string() -> String { format_clsid(&CLSID_THUMB) }
fn clsid_preview_string() -> String { format_clsid(&CLSID_PREVIEW) }

// ── DLL path resolution ───────────────────────────────────────────────────────

/// Derive the absolute path of this DLL from the HMODULE stored at load time.
///
/// Using GetModuleFileNameW on our own HMODULE is the only reliable way to get
/// the path we were loaded from: it is independent of the working directory,
/// the PDFIUM_DLL_PATH environment override (a dev/test hook), and any fallback
/// that might produce a relative path.  A relative "thumb_viewer.dll" in
/// InprocServer32 would break COM activation for any working directory other
/// than the install folder.
fn dll_path_string() -> Result<String, HRESULT> {
    let hmodule = crate::our_hmodule().ok_or_else(|| {
        log::error!("DllRegisterServer: HMODULE not available (DllMain not called?)");
        windows::Win32::Foundation::E_FAIL
    })?;

    let mut buf = vec![0u16; 32768];
    let len = unsafe { GetModuleFileNameW(Some(hmodule), &mut buf) };
    if len == 0 {
        log::error!("DllRegisterServer: GetModuleFileNameW returned 0");
        return Err(windows::Win32::Foundation::E_FAIL);
    }

    let os = OsString::from_wide(&buf[..len as usize]);
    Ok(os.to_string_lossy().into_owned())
}

// ── DllRegisterServer ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    match register_impl() {
        Ok(()) => S_OK,
        Err(hr) => { log::error!("DllRegisterServer failed: {hr:?}"); hr }
    }
}

fn register_impl() -> Result<(), HRESULT> {
    let dll_path = dll_path_string()?;
    let clsid_a = clsid_thumb_string();
    let clsid_b = clsid_preview_string();

    // ── Thumbnail: HKCR\CLSID\{A}\InprocServer32 ─────────────────────────────
    let inproc_a = format!("CLSID\\{}\\InprocServer32", clsid_a);
    unsafe {
        let hkey = create_key_hkcr(&inproc_a)?;
        let r1 = set_default(hkey, &dll_path);
        let r2 = set_sz(hkey, "ThreadingModel", "Apartment");
        let _ = RegCloseKey(hkey);
        r1?;
        r2?;
    }

    // ── Thumbnail: HKCR\SystemFileAssociations\.<ext>\ShellEx\{thumb-guid} ───
    for ext in THUMB_EXTS {
        let key_path = format!(
            "SystemFileAssociations\\{}\\ShellEx\\{}",
            ext, THUMB_SHELLEXT_GUID
        );
        unsafe {
            let hkey = create_key_hkcr(&key_path)?;
            let r = set_default(hkey, &clsid_a);
            let _ = RegCloseKey(hkey);
            r?;
        }
    }

    // ── Preview: HKCR\CLSID\{B} — set AppID named value (prevhost surrogate) ─
    // We only set the AppID VALUE on our own CLSID-B key.
    // We NEVER create or modify HKCR\AppID\{6d2b5079-...} — that is OS-owned.
    let clsid_b_key = format!("CLSID\\{}", clsid_b);
    unsafe {
        let hkey = create_key_hkcr(&clsid_b_key)?;
        let r1 = set_sz(hkey, "AppID", PREVHOST_APPID);
        let r2 = set_sz(hkey, "", "ai-psd-thumbnails Preview"); // optional display name
        let _ = RegCloseKey(hkey);
        r1?;
        r2?;
    }

    // ── Preview: HKCR\CLSID\{B}\InprocServer32 — Apartment threading ─────────
    let inproc_b = format!("CLSID\\{}\\InprocServer32", clsid_b);
    unsafe {
        let hkey = create_key_hkcr(&inproc_b)?;
        let r1 = set_default(hkey, &dll_path);
        let r2 = set_sz(hkey, "ThreadingModel", "Apartment");
        let _ = RegCloseKey(hkey);
        r1?;
        r2?;
    }

    // ── Preview: HKCR\SystemFileAssociations\.<ext>\shellex\{preview-guid} ───
    for ext in PREVIEW_EXTS {
        let key_path = format!(
            "SystemFileAssociations\\{}\\shellex\\{}",
            ext, PREVIEW_SHELLEXT_GUID
        );
        unsafe {
            let hkey = create_key_hkcr(&key_path)?;
            let r = set_default(hkey, &clsid_b);
            let _ = RegCloseKey(hkey);
            r?;
        }
    }

    // ── Preview: HKLM PreviewHandlers list ───────────────────────────────────
    // Adds our CLSID-B as a named value so it appears in the Windows
    // "Preview Handlers" management list.
    unsafe {
        let hkey = create_key_hklm(PREVIEW_HANDLERS_KEY)?;
        let r = set_sz(hkey, &clsid_b, PREVIEW_DISPLAY_NAME);
        let _ = RegCloseKey(hkey);
        r?;
    }

    Ok(())
}

// ── DllUnregisterServer ───────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    match unregister_impl() {
        Ok(()) => S_OK,
        Err(hr) => { log::error!("DllUnregisterServer failed: {hr:?}"); hr }
    }
}

fn unregister_impl() -> Result<(), HRESULT> {
    let clsid_a = clsid_thumb_string();
    let clsid_b = clsid_preview_string();

    // ── Thumbnail: per-ext ShellEx values ────────────────────────────────────
    for ext in THUMB_EXTS {
        let key_path = format!(
            "SystemFileAssociations\\{}\\ShellEx\\{}",
            ext, THUMB_SHELLEXT_GUID
        );
        unsafe { delete_key_hkcr(&key_path)?; }
    }

    // ── Thumbnail: InprocServer32 then CLSID container ────────────────────────
    let inproc_a = format!("CLSID\\{}\\InprocServer32", clsid_a);
    unsafe { delete_key_hkcr(&inproc_a)?; }
    let clsid_a_key = format!("CLSID\\{}", clsid_a);
    unsafe { delete_key_hkcr(&clsid_a_key)?; }

    // ── Preview: per-ext shellex values ──────────────────────────────────────
    for ext in PREVIEW_EXTS {
        let key_path = format!(
            "SystemFileAssociations\\{}\\shellex\\{}",
            ext, PREVIEW_SHELLEXT_GUID
        );
        unsafe { delete_key_hkcr(&key_path)?; }
    }

    // ── Preview: InprocServer32 then CLSID container ──────────────────────────
    // NOTE: we NEVER delete HKCR\AppID\{6d2b5079-...} — that is OS-owned.
    // We only delete our own CLSID-B subtree.
    let inproc_b = format!("CLSID\\{}\\InprocServer32", clsid_b);
    unsafe { delete_key_hkcr(&inproc_b)?; }
    let clsid_b_key = format!("CLSID\\{}", clsid_b);
    unsafe { delete_key_hkcr(&clsid_b_key)?; }

    // ── Preview: HKLM PreviewHandlers list entry ──────────────────────────────
    unsafe { delete_value_hklm(PREVIEW_HANDLERS_KEY, &clsid_b)?; }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PREVIEW_EXTS, THUMB_EXTS};

    /// `.pdf` is never registered: PDFs stay with the OS / PowerToys / Acrobat
    /// handlers so downloaded PDFs are not auto-parsed by the pinned pdfium.
    #[test]
    fn pdf_is_not_registered() {
        assert!(!THUMB_EXTS.contains(&".pdf"), "THUMB_EXTS must NOT include .pdf");
        assert!(!PREVIEW_EXTS.contains(&".pdf"), "PREVIEW_EXTS must NOT include .pdf");
    }

    /// Every preview ext also gets a thumbnail (preview ⊆ thumbnail); registration
    /// symmetry relies on this so unregister never orphans a thumbnail key.
    #[test]
    fn preview_exts_subset_of_thumb_exts() {
        for ext in PREVIEW_EXTS {
            assert!(THUMB_EXTS.contains(ext), "{ext} in PREVIEW_EXTS but not THUMB_EXTS");
        }
    }
}
