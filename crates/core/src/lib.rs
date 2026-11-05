//! ai-psd-thumbnails-core: pure-Rust decode library.
//!
//! Extracts a square RGBA thumbnail from .ai, .psd, .psb, .eps, .svg files.
//! No Win32 dependencies — fully unit-testable in isolation.

pub mod ai;
pub mod detect;
pub mod eps;
pub mod gs;
pub mod pdf;
pub mod psd;
pub mod render;
pub mod svg;
pub(crate) mod xmp;

use thiserror::Error;

// ── Public types ─────────────────────────────────────────────────────────────

/// Decoded image: 8-bit RGBA, non-premultiplied, row-major.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Length == width * height * 4.
    pub rgba: Vec<u8>,
}

/// Supported input format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext {
    Ai,
    Psd,
    Psb,
    Eps,
    Svg,
}

/// Errors produced by the core decoders.
#[derive(Debug, Error)]
pub enum CoreError {
    /// No usable thumbnail found in the file; caller should return E_FAIL to the shell.
    /// This is an expected, non-fatal outcome (e.g. ZIP-compressed PSD composite, EPS without preview).
    #[error("no thumbnail available")]
    NoThumbnail,

    /// Malformed or unsupported file data.
    #[error("decode error: {0}")]
    Decode(String),

    /// I/O error (e.g. reading from IStream).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// PDF rendering error.
    #[error("PDF error: {0}")]
    Pdf(String),
}

// ── Primary entry point ───────────────────────────────────────────────────────

/// Decode a thumbnail from `data` and scale it to fit within a `size × size` square.
///
/// Returns `CoreError::NoThumbnail` when the format has no usable embedded preview
/// and no fallback is available (e.g. EPS without DOS header, ZIP-compressed PSD).
pub fn decode_thumbnail(data: &[u8], ext: Ext, size: u32) -> Result<DecodedImage, CoreError> {
    let raw = match ext {
        Ext::Psd | Ext::Psb => psd::decode(data, ext, size)?,
        Ext::Ai => ai::decode(data, size)?,
        Ext::Eps => eps::decode(data)?,
        Ext::Svg => svg::decode(data, size)?,
    };
    Ok(render::fit_square(raw, size))
}
