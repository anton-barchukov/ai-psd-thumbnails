# ai-psd-thumbnails MSI Installer

WiX v4 per-machine x64 MSI that installs the ai-psd-thumbnails shell extension.

## Build Prerequisites

| Prerequisite | Version | Install command |
|---|---|---|
| .NET SDK | 6.0+ | https://dotnet.microsoft.com/download |
| WiX v4 CLI | 4.0.6 | `dotnet tool install --global wix --version 4.0.6` |
| WixToolset.Util.wixext | 4.0.6 | `wix extension add --global WixToolset.Util.wixext/4.0.6` |
| Rust / cargo | 1.75+ x64-MSVC | https://rustup.rs |

After installing WiX, add `%USERPROFILE%\.dotnet\tools` to your `PATH`.

## Build

```powershell
# From repo root — builds all crates including installer-ca.exe
cargo build --workspace --release

# Assemble payload and produce MSI
.\installer\build-msi.ps1
```

Output: `installer\ai-psd-thumbnails-<version>.msi`

## What Gets Installed

- `%ProgramFiles%\ai-psd-thumbnails\thumb_viewer.dll` — COM shell extension (thumbnail + preview)
- `%ProgramFiles%\ai-psd-thumbnails\pdfium.dll` — bblanchon pdfium-binaries chromium/7763 win-x64

### Registry (per-machine HKLM/HKCR, declarative, clean rollback on uninstall)

| Key | Value |
|---|---|
| `HKCR\CLSID\{90E3822E...}\InprocServer32` | `[ProgramFiles]\ai-psd-thumbnails\thumb_viewer.dll`, `ThreadingModel=Apartment` |
| `HKCR\CLSID\{2CFC1F9E...}\InprocServer32` | same dll, `ThreadingModel=Apartment` |
| `HKCR\CLSID\{2CFC1F9E...}` | `AppID={6d2b5079-...}` (value only; the OS-owned `HKCR\AppID\{6d2b5079-...}` key is never touched) |
| `HKCR\SystemFileAssociations\.<ext>\ShellEx\{e357fccd-...}` | `{CLSID-A}` (5 exts: .ai .psd .psb .eps .svg) |
| `HKCR\SystemFileAssociations\.<ext>\shellex\{8895b1c6-...}` | `{CLSID-B}` (5 exts) |
| `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\PreviewHandlers\{2CFC1F9E-...}` | `"ai-psd-thumbnails Preview"` |

**Never authored:** `HKCR\.<ext>` default (ProgID), `HKCR\.<ext>\ShellEx`, `HKCR\AppID\{6d2b5079-...}`.

### Custom Actions

| CA | Schedule | Impersonate | Action |
|---|---|---|---|
| `CA_RestoreAiProgId` | deferred (elevated) | no (SYSTEM) | Reads `HKCR\.ai` default; if `SageThumbsImage.ai` → restores to `Adobe.Illustrator.28`. No-op otherwise. Not run on uninstall. |
| `CA_NotifyAssocChanged` | commit | yes (user session) | `SHChangeNotify(SHCNE_ASSOCCHANGED)` so Explorer refreshes without manual cache clear. Not run on uninstall. |

## Manual Installation Test Instructions

> Do NOT install during automated CI builds — requires elevation and interactive session.

1. Build the MSI: `.\installer\build-msi.ps1`
2. Open an elevated PowerShell prompt.
3. Install: `msiexec /i installer\ai-psd-thumbnails-<version>.msi /l*v install.log`
4. Verify registry with regedit:
   - `HKCR\SystemFileAssociations\.ai\ShellEx\{e357fccd-...}` = `{90E3822E-...}`
   - `HKCR\SystemFileAssociations\.ai\shellex\{8895b1c6-...}` = `{2CFC1F9E-...}`
   - Repeat for .psd .psb .eps .svg
   - `HKCR\AppID\{6d2b5079-...}` still has `DllSurrogate=...prevhost.exe` (unchanged)
5. Open Explorer, navigate to a folder with .ai/.psd/.psb/.eps/.svg files.
   - Thumbnails should appear. Use ProcMon to confirm `dllhost.exe` loads OUR DLL (not a cached/embedded thumbnail).
   - Press Alt+P for preview pane; select a file. Preview should render.
6. Test SageThumbs cleanup:
   ```powershell
   # Set leftover
   reg add "HKCR\.ai" /ve /d "SageThumbsImage.ai" /f
   # Repair install
   msiexec /fv installer\ai-psd-thumbnails-<version>.msi /l*v repair.log
   # Verify restored
   (Get-ItemProperty "HKCR:\.ai")."(default)"  # should be Adobe.Illustrator.28
   ```
7. Upgrade test: bump version to 0.2.0 in `ai-psd-thumbnails.wxs`, rebuild MSI, install over <version>. No duplicate keys; DLL replaced.
8. Uninstall:
   ```powershell
   msiexec /x installer\ai-psd-thumbnails-<version>.msi /l*v uninstall.log
   ```
   - Our registry keys removed; `HKCR\AppID\{6d2b5079-...}` still present.
   - Other preview handlers (Office PDF etc.) still work.

## DLL Locked During Upgrade / Uninstall

RestartManager is configured to ask Explorer/dllhost/prevhost to release the DLL.
Real-world effect is limited since Explorer re-loads shell-extension DLLs on demand
and dllhost/prevhost surrogates are short-lived. If the MSI reports a reboot is needed,
run the cache-clearing script first:

```powershell
# From an elevated PowerShell — kills dllhost/prevhost and clears thumbcache
.\scripts\clear-thumb-cache.ps1
# Then retry the upgrade/uninstall
```

## pdfium.dll Provenance

`third_party\bin\pdfium.dll` is sourced from [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)
release tag `chromium/7763`, asset `pdfium-win-x64.tgz`. Verify SHA-256 against the
release manifest before distributing.

## Code-Signing

The MSI is not code-signed in this build. For distribution, sign with:
```powershell
signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 `
    /f <cert.pfx> /p <password> installer\ai-psd-thumbnails-<version>.msi
```
