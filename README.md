# ai-psd-thumbnails

## ❤️ Ủng hộ / Support

Nếu dự án này hữu ích với bạn, hãy ủng hộ mình một ly cà phê nhé. Cảm ơn rất nhiều!

If this project helps you, consider buying me a coffee. Thank you!

<p align="center">
  <img src="assets/support/donate-qr.jpg" alt="Ủng hộ / Support - LÂM HIỀN TÂM (MoMo · VietQR · napas 247)" width="360">
</p>

<p align="center"><b>LÂM HIỀN TÂM</b> - MoMo · VietQR · napas 247</p>

## 🤖 How this was built

This entire project, a Rust COM shell extension with an MSI installer, was shipped in **one shot** by an autonomous loop skill (brainstorm → plan → cook → review), powered by **[AgentKit Engineer](https://agentkit.best/engineer?ref=EPU2ZB5U)**.

Want the same workflow? [**Get AgentKit Engineer with 50% off**](https://agentkit.best/engineer?ref=EPU2ZB5U).

---

## Changes in this fork

- **Sharp PSD thumbnails.** Upstream always uses the 160 px JPEG preview Photoshop
  embeds and scales it up to Explorer's 256 px tiles, which looks blocky. This fork
  decodes the full-resolution merged composite instead (row-streamed and
  box-downsampled; for very large canvases only a few evenly spaced rows per
  output row are read, so a 12 800 × 6 400 file still takes well under a
  second), and only
  falls back to the embedded JPEG when the composite is missing, flagged as not
  real (Maximize Compatibility off), ZIP-compressed, 32-bit or in an unsupported
  colour mode. Grayscale, RGB, CMYK, 8- and 16-bit, RAW and RLE are covered;
  merged transparency is honoured (alpha-weighted, so no white fringes on logos).
- **Smooth fallback.** When a small preview does have to be upscaled it is now
  interpolated bilinearly instead of nearest-neighbour.
- **Installer hygiene.** The sequential placeholder component GUIDs in
  `registry-shellex.wxs` were replaced with real ones.
- **No PDF thumbnails.** Upstream's `.pdf` thumbnail registration was dropped:
  PDFs stay with Windows / PowerToys / Acrobat, so downloaded PDFs are never
  auto-parsed by the bundled pdfium, which does not update itself. PDF-based
  `.ai` files still render through pdfium.
- **CI build.** `.github/workflows/build.yml` builds the MSI on a GitHub-hosted
  Windows runner for every push and attaches it to the release on `v*` tags, so
  the installer is traceable to its commit.

Windows Explorer thumbnails & preview pane for **Adobe Illustrator (.ai)**, **Photoshop (.psd/.psb)**, **EPS** and **SVG** files, without installing any Adobe software.

A lightweight COM shell extension written in Rust, shipped as a per-machine x64 MSI.

## Why

Explorer shows blank icons for design files unless you install heavyweight suites or risky third-party tools (some of which hijack your file associations, looking at you, SageThumbs). This project renders real content thumbnails and previews while **never touching your file associations**: `.ai` stays owned by Illustrator, double-click behavior never changes, and uninstalling removes every trace.

## Formats

| Format | How the thumbnail is produced |
|---|---|
| `.ai` (modern, PDF-based) | First page rendered via bundled [pdfium](https://github.com/bblanchon/pdfium-binaries) |
| `.ai` (legacy, EPS-based) | Embedded DOS-EPS TIFF → `%AI_Thumbnail` → XMP thumbnail → Ghostscript* |
| `.psd` / `.psb` | Embedded Photoshop thumbnail resource, falling back to the flattened composite |
| `.eps` | Embedded DOS-EPS TIFF preview → XMP thumbnail → Ghostscript* |
| `.svg` | Rendered with [resvg](https://github.com/linebender/resvg) (external references disabled) |

\* Files with **no embedded preview at all** are rendered through [Ghostscript](https://ghostscript.com) **if it is installed**, as an isolated, sandboxed (`-dSAFER`), timeout-guarded subprocess. Ghostscript is AGPL and is therefore never bundled; the optional `-setup.exe` bundle offers to download the official installer as a separate product.

## Install

Grab from [Releases](https://github.com/teexiii/ai-psd-thumbnails/releases):

- **`ai-psd-thumbnails-<ver>.msi`** - the shell extension only (pick this if you don't care about preview-less EPS files, or already have Ghostscript).
- **`ai-psd-thumbnails-setup-<ver>.exe`** - same MSI, plus offers to download & install official Ghostscript first when it's missing (needs internet during install).

After installing, thumbnails appear for newly viewed files. Files Explorer has already cached as "no thumbnail" need a cache clear (`scripts/clear-thumb-cache.ps1` from an elevated PowerShell) or simply copy/rename them once.

## What it never does

- Never changes the default program, ProgID, or OpenWith entry of any extension, everything registers under association-neutral `HKCR\SystemFileAssociations`.
- Never loads Ghostscript in-process and never bundles it.
- Uninstall removes only its own registry keys and files.
- Bonus: the installer detects a known SageThumbs leftover that hijacks `.ai` and restores `Adobe.Illustrator.28`.

## Build from source

Requires Rust (MSVC toolchain), and WiX 4.0.6 for the installer.

```powershell
cargo build --workspace --release           # → target/release/thumb_viewer.dll
cargo test --workspace --exclude installer-ca
installer/build-msi.ps1                     # → installer/ai-psd-thumbnails-<ver>.msi
installer/build-bundle.ps1                  # → installer/ai-psd-thumbnails-setup-<ver>.exe
```

Dev registration without the MSI: elevated `scripts/dev-register.ps1` / `dev-unregister.ps1`. See [CLAUDE.md](CLAUDE.md) for the full operational reference (architecture, pdfium pin, registry contract, dev-loop gotchas).

## License

[MIT](LICENSE). Bundled `pdfium.dll` is BSD/Apache-licensed ([licenses](third_party/licenses)). Ghostscript is AGPL and is not part of this software; it is only ever an optional, separately installed product.
