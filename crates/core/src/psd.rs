//! PSD (version 1) and PSB (version 2) decoder.
//!
//! A PSD carries two usable pixel sources:
//!
//!   1. The embedded JPEG thumbnail (image resource 1036, legacy 1033). Cheap to
//!      decode, but Photoshop caps it at 160 px on the long side. Scaled up to
//!      Explorer's 256 px tiles it looks blocky.
//!   2. The merged composite in the Image Data section: the full-resolution
//!      flattened image Photoshop writes when "Maximize Compatibility" is on.
//!      Decoded row by row and box-averaged straight into the requested size,
//!      so memory stays O(row + output). For large images only a few evenly
//!      spaced source rows per output row are decoded, so time stays bounded
//!      too (see `SAMPLE_BUDGET`).
//!
//! Choice: when the embedded thumbnail is at least as large as the requested
//! size it is used (downscaling is sharp and it costs nothing). Otherwise the
//! composite is preferred, unless the writer flagged it as not real (resource
//! 1057, `hasRealMergedData = 0`) or it is in a form we don't decode (ZIP
//! compression, 32-bit float, exotic color modes), in which case the small
//! thumbnail is returned and upscaled by the caller.
//!
//! Section length-field widths (only Layer & Mask Info widens in PSB):
//!   - File Header:              identical layout for PSD and PSB (version differs)
//!   - Color Mode Data length:   4 bytes in both
//!   - Image Resources length:   4 bytes in both
//!   - Layer & Mask Info length: 4 bytes (PSD) / 8 bytes (PSB)
//!   - Image Data:               no length field; 2-byte compression word, then
//!                               planar channel data to EOF

use std::io::{Cursor, Read, Seek, SeekFrom};

use crate::{CoreError, DecodedImage, Ext};

/// Work budget for composite decoding, in decoded sample bytes
/// (rows × width × planes × bytes-per-sample). Bounds time, not memory (rows
/// are streamed). 256 M ≈ a few hundred ms in release builds. Large images
/// stay under it by decoding only a few source rows per output row; only if
/// even one row per output row is too much does the thumbnail fallback kick in.
const SAMPLE_BUDGET: u64 = 256 * 1024 * 1024;

/// Source rows averaged per output row at most. With 8 evenly spaced rows the
/// result is visually indistinguishable from a full box filter at thumbnail
/// size, while a 12 800 × 6 400 composite at 256 px decodes 1 024 instead of
/// 6 400 rows.
const MAX_ROWS_PER_BIN: u64 = 8;

/// Hard per-dimension cap (the PSB format limit).
const MAX_DIM: u32 = 300_000;

/// Largest internal downsample target. Explorer asks for ≤ 256 px at 100 %
/// scaling (≤ 512 px at 200 %) and the preview pane for 1024 px; anything
/// larger is upscaled from this by the caller. Bounds the accumulator at
/// 1024 × 1024 × 40 bytes.
const MAX_TARGET: u32 = 1024;

// ── Public entry ──────────────────────────────────────────────────────────────

/// Decode the best available preview for a `size × size` thumbnail.
///
/// The returned image is not yet square-fitted; `render::fit_square` does that.
pub fn decode(data: &[u8], ext: Ext, size: u32) -> Result<DecodedImage, CoreError> {
    let mut cur = Cursor::new(data);
    let hdr = parse_header(&mut cur, ext)?;
    skip_color_mode_data(&mut cur)?;
    let res = scan_image_resources(&mut cur)?;

    // Embedded thumbnail (may be absent, e.g. files written by other tools).
    let thumb = res.decode_thumbnail();

    // Already big enough → done. This is the fast path for list/small-icon views.
    if let Some(t) = &thumb {
        if t.width.max(t.height) >= size {
            return Ok(thumb.unwrap());
        }
    }

    // Prefer the real composite over an upscaled 160 px JPEG.
    if res.has_real_merged_data != Some(false) {
        match decode_composite(&mut cur, data, &hdr, size) {
            Ok(img) => return Ok(img),
            Err(e) => log::debug!("composite unavailable ({e}); using embedded thumbnail"),
        }
    } else {
        log::debug!("resource 1057 says merged data is not real; using embedded thumbnail");
    }

    thumb.ok_or(CoreError::NoThumbnail)
}

// ── Header ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Header {
    is_psb: bool,
    /// Channel count including alpha/spot channels.
    channels: u16,
    height: u32,
    width: u32,
    /// Bits per channel sample: 1, 8, 16 or 32.
    depth: u16,
    /// 0 Bitmap, 1 Grayscale, 2 Indexed, 3 RGB, 4 CMYK, 7 Multichannel,
    /// 8 Duotone, 9 Lab.
    color_mode: u16,
}

/// Parse the 26-byte file header.
fn parse_header(cur: &mut Cursor<&[u8]>, ext: Ext) -> Result<Header, CoreError> {
    let mut sig = [0u8; 4];
    cur.read_exact(&mut sig).map_err(io_err)?;
    if &sig != b"8BPS" {
        return Err(CoreError::Decode("not a PSD/PSB file (bad signature)".into()));
    }

    let version = read_u16_be(cur)?;
    let is_psb = match version {
        1 => false,
        2 => true,
        v => return Err(CoreError::Decode(format!("unknown PSD version: {v}"))),
    };

    // Validate that the ext hint matches the on-disk version.
    match (ext, is_psb) {
        (Ext::Psb, false) => log::warn!("file has PSD version=1 but .psb extension"),
        (Ext::Psd, true) => log::warn!("file has PSB version=2 but .psd extension"),
        _ => {}
    }

    // Reserved: 6 zero bytes
    cur.seek(SeekFrom::Current(6)).map_err(io_err)?;

    let channels = read_u16_be(cur)?;
    let height = read_u32_be(cur)?;
    let width = read_u32_be(cur)?;
    let depth = read_u16_be(cur)?;
    let color_mode = read_u16_be(cur)?;

    Ok(Header { is_psb, channels, height, width, depth, color_mode })
}

// ── Color Mode Data (4-byte length in BOTH PSD and PSB) ──────────────────────

fn skip_color_mode_data(cur: &mut Cursor<&[u8]>) -> Result<(), CoreError> {
    let len = read_u32_be(cur)? as u64;
    seek_within(cur, cur.position() + len)
}

// ── Image Resources (4-byte length in BOTH PSD and PSB) ──────────────────────

/// What we pick out of the Image Resources section.
#[derive(Default)]
struct Resources {
    /// Resource 1036: JPEG thumbnail, RGB (Photoshop 5.0+).
    jpeg_1036: Option<Vec<u8>>,
    /// Resource 1033: JPEG thumbnail, BGR (Photoshop 4.0).
    jpeg_1033: Option<Vec<u8>>,
    /// Resource 1057 `hasRealMergedData`. `None` when the resource is absent
    /// (pre-6.0 Photoshop or third-party writers, which always write a real
    /// composite).
    has_real_merged_data: Option<bool>,
}

impl Resources {
    /// Decode the embedded JPEG thumbnail, preferring the RGB variant.
    /// A corrupt thumbnail is treated as absent rather than fatal.
    fn decode_thumbnail(&self) -> Option<DecodedImage> {
        if let Some(j) = &self.jpeg_1036 {
            match decode_jpeg_rgb(j) {
                Ok(img) => return Some(img),
                Err(e) => log::debug!("resource 1036: {e}"),
            }
        }
        if let Some(j) = &self.jpeg_1033 {
            match decode_jpeg_bgr(j) {
                Ok(img) => return Some(img),
                Err(e) => log::debug!("resource 1033: {e}"),
            }
        }
        None
    }
}

/// Walk the Image Resources section. Leaves the cursor at the start of the
/// Layer & Mask Info section.
fn scan_image_resources(cur: &mut Cursor<&[u8]>) -> Result<Resources, CoreError> {
    let section_len = read_u32_be(cur)? as u64;
    let section_start = cur.position();
    let section_end = section_start + section_len;
    let file_len = cur.get_ref().len() as u64;
    if section_end > file_len {
        return Err(CoreError::Decode("image resources section exceeds file".into()));
    }

    let mut res = Resources::default();

    while cur.position() < section_end {
        // 8BIM signature
        let mut sig = [0u8; 4];
        if cur.read_exact(&mut sig).is_err() {
            break;
        }
        if &sig != b"8BIM" {
            // Desynced — bail gracefully with what we have.
            break;
        }

        let resource_id = read_u16_be(cur)?;

        // Pascal name: 1-byte length prefix + that many bytes, padded to even total.
        let name_len = read_u8(cur)? as u64;
        let name_field_padded = (1 + name_len + 1) & !1; // round up to even
        cur.seek(SeekFrom::Current((name_field_padded - 1) as i64)).map_err(io_err)?;

        // Resource data length: 4 bytes in BOTH PSD and PSB; data padded to even.
        let data_len = read_u32_be(cur)? as u64;
        let data_padded = (data_len + 1) & !1;
        let data_start = cur.position();

        match resource_id {
            1036 => res.jpeg_1036 = Some(read_thumbnail_jpeg(cur, data_len)?),
            1033 => res.jpeg_1033 = Some(read_thumbnail_jpeg(cur, data_len)?),
            1057 => {
                // Version Info: version(4) hasRealMergedData(1) writer(ustr)
                // reader(ustr) fileVersion(4). Only the flag matters here.
                if data_len >= 5 && data_start + 5 <= file_len {
                    cur.seek(SeekFrom::Current(4)).map_err(io_err)?;
                    res.has_real_merged_data = Some(read_u8(cur)? != 0);
                }
            }
            _ => {}
        }

        // Seek to the end of this resource (handles partial reads above).
        seek_within(cur, data_start + data_padded)?;
    }

    seek_within(cur, section_end)?;
    Ok(res)
}

// ── Thumbnail JPEG resource reading ──────────────────────────────────────────

/// Read `data_len` bytes for a resource-1036/1033 block.
/// The block starts with a 28-byte thumbnail header; the JPEG payload follows.
fn read_thumbnail_jpeg(cur: &mut Cursor<&[u8]>, data_len: u64) -> Result<Vec<u8>, CoreError> {
    if data_len < 28 {
        return Err(CoreError::Decode("thumbnail resource too short".into()));
    }

    // Validate data_len against the number of bytes actually remaining in the
    // buffer before allocating.  A hostile length field can otherwise cause a
    // multi-gigabyte zero-fill before read_exact fails with an I/O error.
    let remaining = cur.get_ref().len() as u64 - cur.position();
    if data_len > remaining {
        return Err(CoreError::Decode(
            "thumbnail resource length exceeds remaining file data".into(),
        ));
    }

    // 28-byte header: format(4) + width(4) + height(4) + widthbytes(4) +
    //                 totalsize(4) + compressedsize(4) + bitsperpixel(2) + planes(2)
    cur.seek(SeekFrom::Current(28)).map_err(io_err)?;
    let jpeg_len = (data_len - 28) as usize;
    let mut jpeg = vec![0u8; jpeg_len];
    cur.read_exact(&mut jpeg).map_err(io_err)?;
    Ok(jpeg)
}

// ── JPEG decode helpers ───────────────────────────────────────────────────────

fn decode_jpeg_rgb(jpeg: &[u8]) -> Result<DecodedImage, CoreError> {
    let mut dec = jpeg_decoder::Decoder::new(jpeg);
    let pixels = dec.decode().map_err(|e| CoreError::Decode(format!("JPEG 1036: {e}")))?;
    let info = dec.info().ok_or_else(|| CoreError::Decode("JPEG 1036: no info".into()))?;
    // Use the format-aware converter to handle grayscale and other non-RGB24
    // layouts without panicking on chunks_exact(3) for non-3-byte-per-pixel data.
    let rgba = jpeg_pixels_to_rgba_psd(&pixels, info.pixel_format);
    Ok(DecodedImage { width: info.width as u32, height: info.height as u32, rgba })
}

fn decode_jpeg_bgr(jpeg: &[u8]) -> Result<DecodedImage, CoreError> {
    let mut dec = jpeg_decoder::Decoder::new(jpeg);
    let pixels = dec.decode().map_err(|e| CoreError::Decode(format!("JPEG 1033: {e}")))?;
    let info = dec.info().ok_or_else(|| CoreError::Decode("JPEG 1033: no info".into()))?;
    // Legacy BGR format from resource 1033: swap B↔R on top of any format conversion.
    let rgba = jpeg_pixels_to_rgba_psd(&pixels, info.pixel_format);
    // For RGB24 the conversion already produced RGBA; swap R↔B channels.
    // For non-RGB24 formats (grayscale etc.) the BGR swap is a no-op/identical.
    if info.pixel_format == jpeg_decoder::PixelFormat::RGB24 {
        let swapped: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        return Ok(DecodedImage { width: info.width as u32, height: info.height as u32, rgba: swapped });
    }
    Ok(DecodedImage { width: info.width as u32, height: info.height as u32, rgba })
}

/// Convert jpeg-decoder pixel output to RGBA, handling all pixel formats
/// produced by jpeg-decoder so that non-RGB24 thumbnails (e.g. grayscale)
/// do not panic inside chunks_exact(3).
fn jpeg_pixels_to_rgba_psd(pixels: &[u8], fmt: jpeg_decoder::PixelFormat) -> Vec<u8> {
    use jpeg_decoder::PixelFormat;
    match fmt {
        PixelFormat::RGB24 => pixels
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        PixelFormat::L8 => pixels
            .iter()
            .flat_map(|&g| [g, g, g, 255])
            .collect(),
        PixelFormat::L16 => pixels
            .chunks_exact(2)
            .flat_map(|p| {
                let g = p[0]; // high byte as approximation
                [g, g, g, 255]
            })
            .collect(),
        PixelFormat::CMYK32 => pixels
            .chunks_exact(4)
            .flat_map(|p| {
                let c = p[0] as f32 / 255.0;
                let m = p[1] as f32 / 255.0;
                let y = p[2] as f32 / 255.0;
                let k = p[3] as f32 / 255.0;
                let r = ((1.0 - c) * (1.0 - k) * 255.0) as u8;
                let g = ((1.0 - m) * (1.0 - k) * 255.0) as u8;
                let b = ((1.0 - y) * (1.0 - k) * 255.0) as u8;
                [r, g, b, 255]
            })
            .collect(),
        #[allow(unreachable_patterns)]
        _ => {
            // Unknown future pixel format: return a single opaque grey pixel
            // rather than panicking on an incorrect chunks_exact stride.
            vec![128u8, 128u8, 128u8, 255u8]
        }
    }
}

// ── Layer & Mask Info ─────────────────────────────────────────────────────────

/// Skip the Layer & Mask Info section and report whether the merged image has
/// a transparency channel.
///
/// Per the format spec, a negative layer count means "the first alpha channel
/// contains the transparency data for the merged result". Any other extra
/// channels (saved selections, spot colours) are not transparency and must be
/// ignored, or a saved selection would punch holes into the thumbnail.
fn skip_layer_mask_info(cur: &mut Cursor<&[u8]>, is_psb: bool) -> Result<bool, CoreError> {
    let section_len = if is_psb { read_u64_be(cur)? } else { read_u32_be(cur)? as u64 };
    let section_start = cur.position();
    if section_len == 0 {
        return Ok(false);
    }
    let section_end = section_start
        .checked_add(section_len)
        .ok_or_else(|| CoreError::Decode("layer section length overflow".into()))?;
    if section_end > cur.get_ref().len() as u64 {
        return Err(CoreError::Decode("layer & mask section exceeds file".into()));
    }

    let layer_info_len = if is_psb { read_u64_be(cur)? } else { read_u32_be(cur)? as u64 };
    let has_transparency = if layer_info_len >= 2 {
        (read_u16_be(cur)? as i16) < 0
    } else {
        false
    };

    seek_within(cur, section_end)?;
    Ok(has_transparency)
}

// ── Composite (merged image data) ─────────────────────────────────────────────

/// How the first channels of the composite map to colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorLayout {
    Gray,
    Rgb,
    Cmyk,
}

impl ColorLayout {
    fn from_mode(mode: u16) -> Option<Self> {
        match mode {
            1 => Some(Self::Gray),
            3 => Some(Self::Rgb),
            4 => Some(Self::Cmyk),
            _ => None,
        }
    }

    fn channels(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
        }
    }
}

/// Decode the merged composite, box-downsampled to fit `size × size`
/// (capped at `MAX_TARGET`). The cursor must sit at the Layer & Mask Info
/// section; `data` is the whole file.
fn decode_composite(
    cur: &mut Cursor<&[u8]>,
    data: &[u8],
    hdr: &Header,
    size: u32,
) -> Result<DecodedImage, CoreError> {
    let layout = ColorLayout::from_mode(hdr.color_mode).ok_or_else(|| {
        CoreError::Decode(format!("composite: color mode {} not supported", hdr.color_mode))
    })?;
    let bps = match hdr.depth {
        8 => 1usize,
        16 => 2,
        d => return Err(CoreError::Decode(format!("composite: {d}-bit depth not supported"))),
    };
    let color_channels = layout.channels();
    if (hdr.channels as usize) < color_channels {
        return Err(CoreError::Decode(format!(
            "composite: {} channels, need {color_channels}",
            hdr.channels
        )));
    }

    let (w, h) = (hdr.width, hdr.height);
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
        return Err(CoreError::Decode(format!("composite: bad dimensions {w}×{h}")));
    }

    let has_transparency = skip_layer_mask_info(cur, hdr.is_psb)?;
    let alpha_plane = if has_transparency && (hdr.channels as usize) > color_channels {
        Some(color_channels)
    } else {
        None
    };

    let compression = read_u16_be(cur)?;
    let start = cur.position() as usize;
    let row_bytes = (w as usize) * bps;
    let planes_needed = color_channels + alpha_plane.map_or(0, |_| 1);

    let source = match compression {
        0 => RowSource::raw(data, start, row_bytes, h as usize, planes_needed)?,
        1 => RowSource::rle(data, start, row_bytes, h as usize, hdr.channels as usize, hdr.is_psb)?,
        2 | 3 => {
            // ZIP (with/without prediction): rare in merged data; not worth a
            // zlib dependency for a thumbnail. Caller falls back to the JPEG.
            return Err(CoreError::NoThumbnail);
        }
        c => return Err(CoreError::Decode(format!("composite: unknown compression {c}"))),
    };

    // Target size: fit into size × size, never upscale, bounded by MAX_TARGET.
    let target = size.clamp(1, MAX_TARGET) as u64;
    let long = w.max(h) as u64;
    let (tw, th) = if long <= target {
        (w, h)
    } else {
        (
            ((w as u64 * target + long / 2) / long).max(1) as u32,
            ((h as u64 * target + long / 2) / long).max(1) as u32,
        )
    };

    // How many source rows feed each output row, within the work budget.
    let row_cost = (w as u64) * (planes_needed as u64) * (bps as u64);
    let rows_per_bin = plan_rows_per_bin(h as u64, th as u64, row_cost)
        .ok_or(CoreError::NoThumbnail)?;

    // Per-column bin index, computed once instead of a division per pixel.
    let xmap: Vec<u32> = (0..w as u64).map(|x| (x * tw as u64 / w as u64) as u32).collect();

    // Accumulators: alpha-weighted colour sums, alpha sum, pixel count.
    // Alpha weighting keeps the RGB that Photoshop leaves under fully
    // transparent pixels (usually white) from bleeding into edges.
    let mut acc = vec![[0u64; 5]; (tw as usize) * (th as usize)];

    let mut rows: Vec<Vec<u8>> = vec![Vec::with_capacity(row_bytes); planes_needed];
    let mut rgb_row = vec![0u8; (w as usize) * 3];

    for by in 0..th as u64 {
        // Source rows covered by this output row: [y0, y1).
        let y0 = by * h as u64 / th as u64;
        let y1 = ((by + 1) * h as u64 / th as u64).max(y0 + 1).min(h as u64);
        let span = y1 - y0;
        let k = span.min(rows_per_bin);
        let bin_row = by as usize * tw as usize;

        for j in 0..k {
            // k evenly spaced rows, centred in their sub-span; k == span
            // degenerates to every row.
            let y = (y0 + (2 * j + 1) * span / (2 * k)) as usize;

            for p in 0..planes_needed {
                source.read_row(p, y, &mut rows[p])?;
            }
            convert_row(layout, &rows[..color_channels], bps, &mut rgb_row);
            let alpha = alpha_plane.map(|p| &rows[p]);

            for x in 0..w as usize {
                let a = match alpha {
                    Some(ar) => ar[x * bps] as u64,
                    None => 255,
                };
                let bin = &mut acc[bin_row + xmap[x] as usize];
                let px = &rgb_row[x * 3..x * 3 + 3];
                bin[0] += px[0] as u64 * a;
                bin[1] += px[1] as u64 * a;
                bin[2] += px[2] as u64 * a;
                bin[3] += a;
                bin[4] += 1;
            }
        }
    }

    let mut rgba = Vec::with_capacity(acc.len() * 4);
    for bin in &acc {
        let [r, g, b, a_sum, n] = *bin;
        if a_sum == 0 || n == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            rgba.extend_from_slice(&[
                (r / a_sum) as u8,
                (g / a_sum) as u8,
                (b / a_sum) as u8,
                (a_sum / n) as u8,
            ]);
        }
    }

    Ok(DecodedImage { width: tw, height: th, rgba })
}

/// Rows to sample per output row so that decoding stays within
/// `SAMPLE_BUDGET`. Returns `None` when even one row per output row is over
/// budget (absurdly wide PSBs), so the caller falls back to the thumbnail.
fn plan_rows_per_bin(h: u64, th: u64, row_cost: u64) -> Option<u64> {
    let per_bin = h.div_ceil(th.max(1)).clamp(1, MAX_ROWS_PER_BIN);
    let rows_affordable = SAMPLE_BUDGET / row_cost.max(1);
    let k = per_bin.min(rows_affordable / th.max(1));
    (k >= 1).then_some(k)
}

/// Convert one row of planar colour channels into packed RGB8.
/// 16-bit samples are big-endian; the high byte is kept.
fn convert_row(layout: ColorLayout, planes: &[Vec<u8>], bps: usize, out: &mut [u8]) {
    let w = out.len() / 3;
    match layout {
        ColorLayout::Gray => {
            let g = &planes[0];
            for x in 0..w {
                let v = g[x * bps];
                out[x * 3..x * 3 + 3].copy_from_slice(&[v, v, v]);
            }
        }
        ColorLayout::Rgb => {
            let (r, g, b) = (&planes[0], &planes[1], &planes[2]);
            for x in 0..w {
                out[x * 3] = r[x * bps];
                out[x * 3 + 1] = g[x * bps];
                out[x * 3 + 2] = b[x * bps];
            }
        }
        ColorLayout::Cmyk => {
            // PSD stores CMYK inverted (255 = no ink), so the stored values are
            // already (1 − C) etc.; R = (1 − C)(1 − K). Not colour-managed, but
            // a recognisable thumbnail without an ICC pipeline.
            let (c, m, y, k) = (&planes[0], &planes[1], &planes[2], &planes[3]);
            for x in 0..w {
                let kk = k[x * bps] as u32;
                out[x * 3] = (c[x * bps] as u32 * kk / 255) as u8;
                out[x * 3 + 1] = (m[x * bps] as u32 * kk / 255) as u8;
                out[x * 3 + 2] = (y[x * bps] as u32 * kk / 255) as u8;
            }
        }
    }
}

/// Random access to rows of the planar composite without holding a plane.
enum RowSource<'a> {
    Raw {
        data: &'a [u8],
        start: usize,
        row_bytes: usize,
        height: usize,
    },
    Rle {
        data: &'a [u8],
        /// Byte offset of every compressed row, plane-major (`plane * height + row`).
        offsets: Vec<usize>,
        /// Compressed length of every row.
        lens: Vec<u32>,
        row_bytes: usize,
        height: usize,
    },
}

impl<'a> RowSource<'a> {
    fn raw(
        data: &'a [u8],
        start: usize,
        row_bytes: usize,
        height: usize,
        planes_needed: usize,
    ) -> Result<Self, CoreError> {
        let need = row_bytes
            .checked_mul(height)
            .and_then(|p| p.checked_mul(planes_needed))
            .and_then(|n| n.checked_add(start))
            .ok_or_else(|| CoreError::Decode("composite: size overflow".into()))?;
        if need > data.len() {
            return Err(CoreError::Decode("composite: raw data truncated".into()));
        }
        Ok(Self::Raw { data, start, row_bytes, height })
    }

    fn rle(
        data: &'a [u8],
        start: usize,
        row_bytes: usize,
        height: usize,
        channels: usize,
        is_psb: bool,
    ) -> Result<Self, CoreError> {
        // Row byte-count table covers ALL channels in the header, 2 bytes per
        // row (PSD) or 4 (PSB), and must fit before we read anything.
        let total_rows = height
            .checked_mul(channels)
            .ok_or_else(|| CoreError::Decode("composite: row table overflow".into()))?;
        let entry = if is_psb { 4 } else { 2 };
        let table_end = total_rows
            .checked_mul(entry)
            .and_then(|t| t.checked_add(start))
            .ok_or_else(|| CoreError::Decode("composite: row table overflow".into()))?;
        if table_end > data.len() {
            return Err(CoreError::Decode("composite: RLE row table truncated".into()));
        }

        let mut lens = Vec::with_capacity(total_rows);
        let mut offsets = Vec::with_capacity(total_rows);
        let mut pos = table_end;
        for i in 0..total_rows {
            let t = start + i * entry;
            let len = if is_psb {
                u32::from_be_bytes([data[t], data[t + 1], data[t + 2], data[t + 3]])
            } else {
                u16::from_be_bytes([data[t], data[t + 1]]) as u32
            };
            offsets.push(pos);
            lens.push(len);
            pos = pos
                .checked_add(len as usize)
                .ok_or_else(|| CoreError::Decode("composite: RLE offset overflow".into()))?;
        }
        // Rows are validated lazily in `read_row`, so a file truncated after
        // the planes we need still yields a thumbnail.
        Ok(Self::Rle { data, offsets, lens, row_bytes, height })
    }

    /// Fill `out` with the `row_bytes` uncompressed bytes of row `y` in `plane`.
    fn read_row(&self, plane: usize, y: usize, out: &mut Vec<u8>) -> Result<(), CoreError> {
        out.clear();
        match self {
            Self::Raw { data, start, row_bytes, height } => {
                let off = start + (plane * height + y) * row_bytes;
                out.extend_from_slice(&data[off..off + row_bytes]);
            }
            Self::Rle { data, offsets, lens, row_bytes, height } => {
                let i = plane * height + y;
                let off = offsets[i];
                let end = off
                    .checked_add(lens[i] as usize)
                    .ok_or_else(|| CoreError::Decode("composite: RLE row overflow".into()))?;
                if end > data.len() {
                    return Err(CoreError::Decode("composite: RLE row truncated".into()));
                }
                packbits_decode(&data[off..end], out, *row_bytes)?;
                if out.len() < *row_bytes {
                    return Err(CoreError::Decode("composite: RLE row too short".into()));
                }
            }
        }
        Ok(())
    }
}

/// PackBits decompression (Mac/PSD RLE).
///
/// `max_output` is a hard cap on the number of bytes written to `output`
/// (measured from the current length on entry).  Once `max_output` bytes have
/// been appended, further runs are silently dropped and the function returns
/// successfully — the caller checks the length afterward.
/// This prevents a crafted repeat-run sequence from driving unbounded Vec growth
/// (a single PackBits repeat token can expand ~64×; without a cap a small
/// compressed file could exhaust process memory).
pub(crate) fn packbits_decode(
    input: &[u8],
    output: &mut Vec<u8>,
    max_output: usize,
) -> Result<(), CoreError> {
    let start_len = output.len();
    let mut i = 0;
    while i < input.len() {
        // Stop early once the cap is reached; the caller will truncate/check.
        if output.len() - start_len >= max_output {
            break;
        }

        let header = input[i] as i8;
        i += 1;
        if header >= 0 {
            // Literal run: copy (header + 1) bytes.
            let count = header as usize + 1;
            if i + count > input.len() {
                return Err(CoreError::Decode("PackBits: literal overrun".into()));
            }
            let allowed = max_output.saturating_sub(output.len() - start_len);
            let copy = count.min(allowed);
            output.extend_from_slice(&input[i..i + copy]);
            i += count;
        } else if header != -128 {
            // Repeat run: copy the next byte (-header + 1) times.
            let count = (-header) as usize + 1;
            if i >= input.len() {
                return Err(CoreError::Decode("PackBits: repeat overrun".into()));
            }
            let byte = input[i];
            i += 1;
            let allowed = max_output.saturating_sub(output.len() - start_len);
            let copy = count.min(allowed);
            output.resize(output.len() + copy, byte);
        }
        // header == -128 is a NOP in PackBits.
    }
    Ok(())
}

// ── Byte-reading helpers ──────────────────────────────────────────────────────

/// Seek to an absolute position that must lie within the buffer.
fn seek_within(cur: &mut Cursor<&[u8]>, pos: u64) -> Result<(), CoreError> {
    if pos > cur.get_ref().len() as u64 {
        return Err(CoreError::Decode("section length exceeds file".into()));
    }
    cur.seek(SeekFrom::Start(pos)).map_err(io_err)?;
    Ok(())
}

fn read_u8(cur: &mut Cursor<&[u8]>) -> Result<u8, CoreError> {
    let mut b = [0u8; 1];
    cur.read_exact(&mut b).map_err(io_err)?;
    Ok(b[0])
}

fn read_u16_be(cur: &mut Cursor<&[u8]>) -> Result<u16, CoreError> {
    let mut b = [0u8; 2];
    cur.read_exact(&mut b).map_err(io_err)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32_be(cur: &mut Cursor<&[u8]>) -> Result<u32, CoreError> {
    let mut b = [0u8; 4];
    cur.read_exact(&mut b).map_err(io_err)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64_be(cur: &mut Cursor<&[u8]>) -> Result<u64, CoreError> {
    let mut b = [0u8; 8];
    cur.read_exact(&mut b).map_err(io_err)?;
    Ok(u64::from_be_bytes(b))
}

fn io_err(e: std::io::Error) -> CoreError {
    CoreError::Io(e)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Synthetic PSD builder ─────────────────────────────────────────────────

    /// Minimal PSD/PSB writer for fixtures. Planes are raw big-endian sample
    /// bytes, one `Vec` per channel, `w * h * bps` bytes each.
    struct Fixture {
        psb: bool,
        w: u32,
        h: u32,
        depth: u16,
        mode: u16,
        planes: Vec<Vec<u8>>,
        rle: bool,
        thumb_jpeg: Option<Vec<u8>>,
        has_real_merged: Option<bool>,
        /// `Some(n)` writes a Layer & Mask section with this layer count.
        layer_count: Option<i16>,
    }

    impl Fixture {
        fn rgb(w: u32, h: u32, planes: Vec<Vec<u8>>) -> Self {
            Self {
                psb: false,
                w,
                h,
                depth: 8,
                mode: 3,
                planes,
                rle: false,
                thumb_jpeg: None,
                has_real_merged: None,
                layer_count: None,
            }
        }

        fn build(&self) -> Vec<u8> {
            let mut d = Vec::new();
            d.extend_from_slice(b"8BPS");
            d.extend_from_slice(&(if self.psb { 2u16 } else { 1 }).to_be_bytes());
            d.extend_from_slice(&[0u8; 6]);
            d.extend_from_slice(&(self.planes.len() as u16).to_be_bytes());
            d.extend_from_slice(&self.h.to_be_bytes());
            d.extend_from_slice(&self.w.to_be_bytes());
            d.extend_from_slice(&self.depth.to_be_bytes());
            d.extend_from_slice(&self.mode.to_be_bytes());
            // Color mode data: empty
            d.extend_from_slice(&0u32.to_be_bytes());

            // Image resources
            let mut ir = Vec::new();
            if let Some(j) = &self.thumb_jpeg {
                let mut blk = Vec::new();
                blk.extend_from_slice(&1u32.to_be_bytes()); // kJpegRGB
                blk.extend_from_slice(&[0u8; 24]); // dims etc. (unused by decoder)
                blk.extend_from_slice(j);
                push_resource(&mut ir, 1036, &blk);
            }
            if let Some(flag) = self.has_real_merged {
                let mut blk = Vec::new();
                blk.extend_from_slice(&1u32.to_be_bytes());
                blk.push(flag as u8);
                blk.extend_from_slice(&0u32.to_be_bytes()); // writer name (empty)
                blk.extend_from_slice(&0u32.to_be_bytes()); // reader name (empty)
                blk.extend_from_slice(&1u32.to_be_bytes());
                push_resource(&mut ir, 1057, &blk);
            }
            d.extend_from_slice(&(ir.len() as u32).to_be_bytes());
            d.extend(ir);

            // Layer & mask info
            match self.layer_count {
                None => {
                    if self.psb {
                        d.extend_from_slice(&0u64.to_be_bytes());
                    } else {
                        d.extend_from_slice(&0u32.to_be_bytes());
                    }
                }
                Some(n) => {
                    // layer info: len + count, then padding to look like a real section
                    let mut li = Vec::new();
                    li.extend_from_slice(&n.to_be_bytes());
                    li.extend_from_slice(&[0u8; 6]);
                    let mut sec = Vec::new();
                    if self.psb {
                        sec.extend_from_slice(&(li.len() as u64).to_be_bytes());
                    } else {
                        sec.extend_from_slice(&(li.len() as u32).to_be_bytes());
                    }
                    sec.extend(li);
                    sec.extend_from_slice(&[0u8; 4]); // global mask info: empty
                    if self.psb {
                        d.extend_from_slice(&(sec.len() as u64).to_be_bytes());
                    } else {
                        d.extend_from_slice(&(sec.len() as u32).to_be_bytes());
                    }
                    d.extend(sec);
                }
            }

            // Image data
            let bps = (self.depth / 8).max(1) as usize;
            let row_bytes = self.w as usize * bps;
            if self.rle {
                d.extend_from_slice(&1u16.to_be_bytes());
                let mut rows: Vec<Vec<u8>> = Vec::new();
                for plane in &self.planes {
                    for y in 0..self.h as usize {
                        rows.push(packbits_encode(&plane[y * row_bytes..(y + 1) * row_bytes]));
                    }
                }
                for r in &rows {
                    if self.psb {
                        d.extend_from_slice(&(r.len() as u32).to_be_bytes());
                    } else {
                        d.extend_from_slice(&(r.len() as u16).to_be_bytes());
                    }
                }
                for r in rows {
                    d.extend(r);
                }
            } else {
                d.extend_from_slice(&0u16.to_be_bytes());
                for plane in &self.planes {
                    d.extend_from_slice(plane);
                }
            }
            d
        }
    }

    fn push_resource(ir: &mut Vec<u8>, id: u16, data: &[u8]) {
        ir.extend_from_slice(b"8BIM");
        ir.extend_from_slice(&id.to_be_bytes());
        ir.extend_from_slice(&[0, 0]); // empty pascal name, padded
        ir.extend_from_slice(&(data.len() as u32).to_be_bytes());
        ir.extend_from_slice(data);
        if data.len() % 2 == 1 {
            ir.push(0);
        }
    }

    /// Literal-runs-only PackBits encoder, plus one repeat run so both token
    /// kinds get exercised by the decoder.
    fn packbits_encode(row: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < row.len() {
            // Emit a repeat run when the next 3+ bytes are equal.
            let b = row[i];
            let mut run = 1;
            while i + run < row.len() && row[i + run] == b && run < 128 {
                run += 1;
            }
            if run >= 3 {
                out.push((1i16 - run as i16) as i8 as u8);
                out.push(b);
                i += run;
                continue;
            }
            let n = (row.len() - i).min(128);
            out.push((n - 1) as u8);
            out.extend_from_slice(&row[i..i + n]);
            i += n;
        }
        out
    }

    /// Solid-colour plane helper.
    fn plane(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w * h) as usize]
    }

    /// Encode a solid-colour JPEG of the given size (used as resource 1036).
    fn jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        use image::codecs::jpeg::JpegEncoder;
        use image::ImageEncoder;
        let px: Vec<u8> = (0..w * h).flat_map(|_| rgb).collect();
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 95)
            .write_image(&px, w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
    }

    fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
        a.iter().zip(b).all(|(x, y)| (*x as i16 - y as i16).abs() <= tol as i16)
    }

    // ── packbits ──────────────────────────────────────────────────────────────

    #[test]
    fn packbits_literal() {
        let input = [0x02u8, 0xAA, 0xBB, 0xCC];
        let mut out = Vec::new();
        packbits_decode(&input, &mut out, usize::MAX).unwrap();
        assert_eq!(out, [0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn packbits_repeat() {
        let input = [0xFEu8, 0x55];
        let mut out = Vec::new();
        packbits_decode(&input, &mut out, usize::MAX).unwrap();
        assert_eq!(out, [0x55, 0x55, 0x55]);
    }

    #[test]
    fn packbits_output_cap_prevents_amplification() {
        let input = [0xFEu8, 0xAB];
        let mut out = Vec::new();
        packbits_decode(&input, &mut out, 2).unwrap();
        assert_eq!(out, [0xAB, 0xAB]);
    }

    #[test]
    fn packbits_roundtrip_fixture_encoder() {
        let row: Vec<u8> = (0..300u32).map(|i| if i % 7 < 3 { 9 } else { (i % 251) as u8 }).collect();
        let enc = packbits_encode(&row);
        let mut dec = Vec::new();
        packbits_decode(&enc, &mut dec, usize::MAX).unwrap();
        assert_eq!(dec, row);
    }

    // ── header / hostile input ────────────────────────────────────────────────

    #[test]
    fn bad_signature_rejected() {
        assert!(matches!(decode(b"NOTAPSD\x00\x01", Ext::Psd, 256), Err(CoreError::Decode(_))));
    }

    #[test]
    fn truncated_header_rejected() {
        let result = decode(b"8BPS\x00\x01\x00\x00", Ext::Psd, 256);
        assert!(matches!(result, Err(CoreError::Decode(_) | CoreError::Io(_))));
    }

    #[test]
    fn hostile_thumbnail_length_rejected() {
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(b"8BPS\x00\x01");
        data.extend_from_slice(&[0u8; 6]);
        data.extend_from_slice(&[0, 3, 0, 0, 0, 8, 0, 0, 0, 8, 0, 8, 0, 3]);
        data.extend_from_slice(&[0, 0, 0, 0]);
        let mut ir: Vec<u8> = Vec::new();
        ir.extend_from_slice(b"8BIM\x04\x0C\x00\x00");
        ir.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // hostile data_len
        ir.extend_from_slice(&[0u8; 4]);
        data.extend_from_slice(&(ir.len() as u32).to_be_bytes());
        data.extend(ir);
        let result = decode(&data, Ext::Psd, 256);
        assert!(
            matches!(result, Err(CoreError::Decode(_) | CoreError::NoThumbnail | CoreError::Io(_))),
            "expected error for hostile data_len, got: {result:?}"
        );
    }

    #[test]
    fn hostile_section_lengths_do_not_panic() {
        // Image resources length pointing far past EOF.
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(b"8BPS\x00\x01");
        data.extend_from_slice(&[0u8; 6]);
        data.extend_from_slice(&[0, 3, 0, 0, 0, 8, 0, 0, 0, 8, 0, 8, 0, 3]);
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(&[0x7F, 0xFF, 0xFF, 0xFF]);
        assert!(decode(&data, Ext::Psd, 256).is_err());

        // Huge dimensions with a tiny body: must fail cleanly, not allocate.
        let mut f = Fixture::rgb(2, 2, vec![plane(2, 2, 1); 3]);
        f.layer_count = None;
        let mut d = f.build();
        d[14..18].copy_from_slice(&200_000u32.to_be_bytes());
        d[18..22].copy_from_slice(&200_000u32.to_be_bytes());
        assert!(matches!(decode(&d, Ext::Psd, 256), Err(CoreError::NoThumbnail)));

        // RLE row table claiming rows past EOF.
        let mut f = Fixture::rgb(4, 4, vec![plane(4, 4, 1); 3]);
        f.rle = true;
        let mut d = f.build();
        let n = d.len();
        d.truncate(n - 10);
        assert!(decode(&d, Ext::Psd, 256).is_err());
    }

    // ── composite decoding ────────────────────────────────────────────────────

    #[test]
    fn composite_raw_rgb_preferred_over_small_thumbnail() {
        // 64×64 solid blue composite, 8×8 red thumbnail. At size 256 the
        // thumbnail is too small, so the composite must win.
        let mut f = Fixture::rgb(64, 64, vec![plane(64, 64, 0), plane(64, 64, 0), plane(64, 64, 255)]);
        f.thumb_jpeg = Some(jpeg(8, 8, [255, 0, 0]));
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!((img.width, img.height), (64, 64));
        assert_eq!(px(&img, 10, 10), [0, 0, 255, 255]);
    }

    #[test]
    fn thumbnail_used_when_large_enough() {
        let mut f = Fixture::rgb(64, 64, vec![plane(64, 64, 0), plane(64, 64, 0), plane(64, 64, 255)]);
        f.thumb_jpeg = Some(jpeg(32, 32, [255, 0, 0]));
        // size 16 ≤ thumbnail → thumbnail (red), not composite (blue).
        let img = decode(&f.build(), Ext::Psd, 16).unwrap();
        assert_eq!((img.width, img.height), (32, 32));
        assert!(close(px(&img, 5, 5), [255, 0, 0, 255], 8), "{:?}", px(&img, 5, 5));
    }

    #[test]
    fn thumbnail_used_when_merged_data_flagged_fake() {
        let mut f = Fixture::rgb(64, 64, vec![plane(64, 64, 255); 3]); // white "fake" composite
        f.thumb_jpeg = Some(jpeg(8, 8, [255, 0, 0]));
        f.has_real_merged = Some(false);
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        assert!(close(px(&img, 3, 3), [255, 0, 0, 255], 8));

        // …and with the flag set to true the composite is used again.
        f.has_real_merged = Some(true);
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!((img.width, img.height), (64, 64));
    }

    #[test]
    fn composite_without_thumbnail_still_works_and_errors_without_either() {
        let f = Fixture::rgb(8, 8, vec![plane(8, 8, 10), plane(8, 8, 20), plane(8, 8, 30)]);
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 0, 0), [10, 20, 30, 255]);

        // ZIP-compressed composite, no thumbnail → NoThumbnail.
        let mut d = f.build();
        let n = d.len();
        let comp_pos = n - 8 * 8 * 3 - 2;
        d[comp_pos..comp_pos + 2].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(decode(&d, Ext::Psd, 256), Err(CoreError::NoThumbnail)));
    }

    #[test]
    fn composite_rle_and_psb_row_table() {
        for psb in [false, true] {
            let (w, h) = (40u32, 12u32);
            let mut r = plane(w, h, 0);
            // Gradient in R so runs and literals both occur.
            for y in 0..h {
                for x in 0..w {
                    r[(y * w + x) as usize] = if x < 20 { 200 } else { (x * 5) as u8 };
                }
            }
            let mut f = Fixture::rgb(w, h, vec![r, plane(w, h, 50), plane(w, h, 60)]);
            f.rle = true;
            f.psb = psb;
            let img = decode(&f.build(), if psb { Ext::Psb } else { Ext::Psd }, 1024).unwrap();
            assert_eq!((img.width, img.height), (w, h));
            assert_eq!(px(&img, 3, 3), [200, 50, 60, 255]);
            assert_eq!(px(&img, 30, 3), [150, 50, 60, 255]);
        }
    }

    #[test]
    fn composite_downsamples_with_box_average() {
        // 100×50 image: left half black, right half white → at 20×10 the
        // boundary column averages, everything else stays pure.
        let (w, h) = (100u32, 50u32);
        let mut p = plane(w, h, 0);
        for y in 0..h {
            for x in 50..w {
                p[(y * w + x) as usize] = 255;
            }
        }
        let f = Fixture::rgb(w, h, vec![p.clone(), p.clone(), p]);
        let img = decode(&f.build(), Ext::Psd, 20).unwrap();
        assert_eq!((img.width, img.height), (20, 10));
        assert_eq!(px(&img, 2, 5), [0, 0, 0, 255]);
        assert_eq!(px(&img, 17, 5), [255, 255, 255, 255]);
    }

    #[test]
    fn composite_target_is_capped() {
        let f = Fixture::rgb(2048, 2, vec![plane(2048, 2, 7); 3]);
        let img = decode(&f.build(), Ext::Psd, 4096).unwrap();
        assert_eq!(img.width, MAX_TARGET);
        assert_eq!(img.height, 1);
    }

    #[test]
    fn row_plan_respects_cap_and_budget() {
        // 12 800 × 6 400 RGB at 256 px → 128 output rows, 50 source rows each → capped at 8.
        assert_eq!(plan_rows_per_bin(6400, 128, 12800 * 3), Some(MAX_ROWS_PER_BIN));
        // Fewer source rows per bin than the cap → all of them.
        assert_eq!(plan_rows_per_bin(300, 100, 900), Some(3));
        // No downscale → one row per bin.
        assert_eq!(plan_rows_per_bin(64, 64, 192), Some(1));
        // Budget trims k below the cap.
        let th = 512;
        assert_eq!(plan_rows_per_bin(100_000, th, SAMPLE_BUDGET / (th * 4)), Some(4));
        // Absurdly wide 16-bit CMYK PSB: over budget even at one row per bin.
        assert_eq!(plan_rows_per_bin(300_000, 1024, 300_000 * 4 * 2), None);
    }

    #[test]
    fn tall_image_with_row_sampling_keeps_regions_intact() {
        // 6 × 4000: top half red, bottom half blue. At size 40 each output row
        // covers 100 source rows, of which 8 are sampled.
        let (w, h) = (6u32, 4000u32);
        let half = (w * h / 2) as usize;
        let mut r = plane(w, h, 0);
        let mut b = plane(w, h, 0);
        r[..half].fill(255);
        b[half..].fill(255);
        for rle in [false, true] {
            let mut f = Fixture::rgb(w, h, vec![r.clone(), plane(w, h, 0), b.clone()]);
            f.rle = rle;
            let img = decode(&f.build(), Ext::Psd, 40).unwrap();
            assert_eq!((img.width, img.height), (1, 40));
            assert_eq!(px(&img, 0, 5), [255, 0, 0, 255], "rle={rle}");
            assert_eq!(px(&img, 0, 34), [0, 0, 255, 255], "rle={rle}");
        }
    }

    #[test]
    fn transparency_only_with_negative_layer_count() {
        let (w, h) = (8u32, 8u32);
        let mut a = plane(w, h, 255);
        for i in 0..32 {
            a[i] = 0; // top half transparent
        }
        let planes = vec![plane(w, h, 255), plane(w, h, 0), plane(w, h, 0), a];

        // Negative layer count → 4th channel is transparency.
        let mut f = Fixture::rgb(w, h, planes.clone());
        f.layer_count = Some(-1);
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 1, 1), [0, 0, 0, 0]);
        assert_eq!(px(&img, 1, 6), [255, 0, 0, 255]);

        // Positive layer count → extra channel is a saved selection: ignore it.
        f.layer_count = Some(1);
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 1, 1), [255, 0, 0, 255]);

        // No layer section at all → opaque.
        f.layer_count = None;
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 1, 1), [255, 0, 0, 255]);
    }

    #[test]
    fn transparent_pixels_do_not_bleed_into_edges_when_downsampling() {
        // 2×1 → 1×1: one opaque red pixel next to a fully transparent WHITE
        // pixel (what Photoshop leaves under alpha=0). Alpha-weighted average
        // must give red at 50 %, not pink.
        let planes = vec![vec![255, 255], vec![0, 255], vec![0, 255], vec![255, 0]];
        let mut f = Fixture::rgb(2, 1, planes);
        f.layer_count = Some(-1);
        let img = decode(&f.build(), Ext::Psd, 1).unwrap();
        assert_eq!(px(&img, 0, 0), [255, 0, 0, 127]);
    }

    #[test]
    fn grayscale_and_cmyk_and_16bit() {
        // Grayscale
        let mut f = Fixture::rgb(4, 4, vec![plane(4, 4, 77)]);
        f.mode = 1;
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 1, 1), [77, 77, 77, 255]);

        // CMYK, stored inverted: pure cyan = C plane 0, others 255.
        let mut f = Fixture::rgb(4, 4, vec![plane(4, 4, 0), plane(4, 4, 255), plane(4, 4, 255), plane(4, 4, 255)]);
        f.mode = 4;
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 1, 1), [0, 255, 255, 255]);

        // 16-bit RGB: big-endian samples, high byte kept.
        let p16 = |v: u16| (0..16).flat_map(|_| v.to_be_bytes()).collect::<Vec<u8>>();
        let mut f = Fixture::rgb(4, 4, vec![p16(0x1234), p16(0xFF00), p16(0x0080)]);
        f.depth = 16;
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 2, 2), [0x12, 0xFF, 0x00, 255]);

        // 16-bit RLE too.
        f.rle = true;
        let img = decode(&f.build(), Ext::Psd, 256).unwrap();
        assert_eq!(px(&img, 2, 2), [0x12, 0xFF, 0x00, 255]);
    }

    #[test]
    fn unsupported_modes_fall_back_to_thumbnail() {
        for (mode, depth) in [(2u16, 8u16), (9, 8), (3, 32), (0, 1)] {
            let mut f = Fixture::rgb(4, 4, vec![plane(4, 4, 0); 3]);
            f.mode = mode;
            f.depth = depth;
            f.thumb_jpeg = Some(jpeg(8, 8, [0, 255, 0]));
            let img = decode(&f.build(), Ext::Psd, 256).unwrap();
            assert_eq!((img.width, img.height), (8, 8), "mode {mode} depth {depth}");
            assert!(close(px(&img, 3, 3), [0, 255, 0, 255], 8));
        }
    }
}
