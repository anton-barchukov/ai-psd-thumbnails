//! Post-decode rendering helpers.
//!
//! `fit_square`: letterbox-scale a `DecodedImage` to fit within `size × size`,
//!   preserving aspect ratio, producing a new `DecodedImage` padded with transparent pixels.
//! `to_bgra_premul`: convert RGBA (non-premultiplied) to premultiplied BGRA for HBITMAP consumers.

use crate::DecodedImage;

// ── fit_square ────────────────────────────────────────────────────────────────

/// Scale `img` to fit within a `size × size` square, preserving aspect ratio.
///
/// The result is exactly `size × size`, with unused areas filled with transparent
/// black (0x00000000).  If the input is already smaller than `size`, it is scaled
/// up rather than left at original size, to keep the output dimensions consistent.
pub fn fit_square(img: DecodedImage, size: u32) -> DecodedImage {
    if size == 0 {
        return DecodedImage { width: 0, height: 0, rgba: Vec::new() };
    }

    // If source already matches the target exactly, return as-is.
    if img.width == size && img.height == size {
        return img;
    }

    // Compute scale factor to fit both dimensions within `size`.
    let scale = if img.width == 0 || img.height == 0 {
        1.0_f64
    } else {
        (size as f64 / img.width as f64).min(size as f64 / img.height as f64)
    };

    let scaled_w = ((img.width as f64 * scale).round() as u32).max(1).min(size);
    let scaled_h = ((img.height as f64 * scale).round() as u32).max(1).min(size);

    // Downscale with box averaging — halftone/dithered sources (common in EPS
    // previews) alias into visible checker patterns under nearest-neighbour.
    // Upscale bilinearly: the sources that land here are small embedded
    // previews (160 px PSD thumbnails, EPS TIFF previews) and nearest-neighbour
    // turns them into visible blocks at Explorer's 256 px tiles.
    let scaled = if scaled_w < img.width || scaled_h < img.height {
        scale_box_average(&img, scaled_w, scaled_h)
    } else if scaled_w == img.width && scaled_h == img.height {
        img.rgba.clone()
    } else {
        scale_bilinear(&img, scaled_w, scaled_h)
    };

    // Letterbox: centre the scaled image in a size × size transparent canvas.
    let offset_x = (size - scaled_w) / 2;
    let offset_y = (size - scaled_h) / 2;

    let mut canvas = vec![0u8; (size * size * 4) as usize];

    for row in 0..scaled_h {
        let src_start = (row * scaled_w * 4) as usize;
        let src_end = src_start + (scaled_w * 4) as usize;
        let dst_start = ((offset_y + row) * size * 4 + offset_x * 4) as usize;
        let dst_end = dst_start + (scaled_w * 4) as usize;
        canvas[dst_start..dst_end].copy_from_slice(&scaled[src_start..src_end]);
    }

    DecodedImage { width: size, height: size, rgba: canvas }
}

/// Bilinear upscale. Interpolates in premultiplied space so colour under
/// transparent pixels cannot fringe into opaque neighbours, then
/// un-premultiplies back to the straight-alpha `DecodedImage` convention.
fn scale_bilinear(img: &DecodedImage, target_w: u32, target_h: u32) -> Vec<u8> {
    let (sw, sh) = (img.width as usize, img.height as usize);
    let mut out = vec![0u8; (target_w * target_h * 4) as usize];
    if sw == 0 || sh == 0 {
        return out;
    }

    // Premultiplied source as f32 for cheap interpolation.
    let src: Vec<[f32; 4]> = img
        .rgba
        .chunks_exact(4)
        .map(|p| {
            let a = p[3] as f32 / 255.0;
            [p[0] as f32 * a, p[1] as f32 * a, p[2] as f32 * a, p[3] as f32]
        })
        .collect();

    // Pixel-centre mapping, clamped at the borders.
    let coord = |d: u32, dst: u32, srcn: usize| -> (usize, usize, f32) {
        let s = ((d as f32 + 0.5) * srcn as f32 / dst as f32 - 0.5).max(0.0);
        let i0 = (s as usize).min(srcn - 1);
        let i1 = (i0 + 1).min(srcn - 1);
        (i0, i1, s - i0 as f32)
    };

    for dy in 0..target_h {
        let (y0, y1, fy) = coord(dy, target_h, sh);
        for dx in 0..target_w {
            let (x0, x1, fx) = coord(dx, target_w, sw);
            let p00 = src[y0 * sw + x0];
            let p10 = src[y0 * sw + x1];
            let p01 = src[y1 * sw + x0];
            let p11 = src[y1 * sw + x1];
            let mut v = [0f32; 4];
            for c in 0..4 {
                let top = p00[c] + (p10[c] - p00[c]) * fx;
                let bottom = p01[c] + (p11[c] - p01[c]) * fx;
                v[c] = top + (bottom - top) * fy;
            }
            let a = v[3];
            let dst = ((dy * target_w + dx) * 4) as usize;
            if a > 0.0 {
                let k = 255.0 / a;
                out[dst] = (v[0] * k).round().clamp(0.0, 255.0) as u8;
                out[dst + 1] = (v[1] * k).round().clamp(0.0, 255.0) as u8;
                out[dst + 2] = (v[2] * k).round().clamp(0.0, 255.0) as u8;
                out[dst + 3] = a.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    out
}

/// Box-average downscale: each destination pixel is the mean of all source
/// pixels in its footprint. Averages out halftone dither instead of aliasing
/// it into moiré/checker artifacts.
fn scale_box_average(img: &DecodedImage, target_w: u32, target_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (target_w * target_h * 4) as usize];
    let (sw, sh) = (img.width as usize, img.height as usize);

    for dy in 0..target_h as usize {
        let sy0 = dy * sh / target_h as usize;
        let sy1 = (((dy + 1) * sh) / target_h as usize).max(sy0 + 1).min(sh.max(sy0 + 1));
        for dx in 0..target_w as usize {
            let sx0 = dx * sw / target_w as usize;
            let sx1 = (((dx + 1) * sw) / target_w as usize).max(sx0 + 1).min(sw.max(sx0 + 1));

            let (mut r, mut g, mut b, mut a) = (0u64, 0u64, 0u64, 0u64);
            for sy in sy0..sy1 {
                let row = sy * sw;
                for sx in sx0..sx1 {
                    let p = (row + sx) * 4;
                    r += img.rgba[p] as u64;
                    g += img.rgba[p + 1] as u64;
                    b += img.rgba[p + 2] as u64;
                    a += img.rgba[p + 3] as u64;
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u64;
            let dst = (dy * target_w as usize + dx) * 4;
            out[dst] = (r / n) as u8;
            out[dst + 1] = (g / n) as u8;
            out[dst + 2] = (b / n) as u8;
            out[dst + 3] = (a / n) as u8;
        }
    }

    out
}

// ── to_bgra_premul ────────────────────────────────────────────────────────────

/// Convert non-premultiplied RGBA to premultiplied BGRA.
///
/// Required by Windows HBITMAP / DIB consumers (DI_PREMULT format).
/// Channel order: output[i*4] = B, [i*4+1] = G, [i*4+2] = R, [i*4+3] = A.
/// Premultiplication: each colour channel is multiplied by alpha / 255.
pub fn to_bgra_premul(img: &DecodedImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.rgba.len());

    for px in img.rgba.chunks_exact(4) {
        let r = px[0];
        let g = px[1];
        let b = px[2];
        let a = px[3];

        // Premultiply: channel * a / 255, using integer arithmetic.
        let premul = |c: u8| -> u8 {
            ((c as u16 * a as u16 + 127) / 255) as u8
        };

        out.push(premul(b)); // B
        out.push(premul(g)); // G
        out.push(premul(r)); // R
        out.push(a);         // A (unchanged)
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(w: u32, h: u32, r: u8, g: u8, b: u8, a: u8) -> DecodedImage {
        let rgba = (0..w * h).flat_map(|_| [r, g, b, a]).collect();
        DecodedImage { width: w, height: h, rgba }
    }

    #[test]
    fn fit_square_identity() {
        let img = solid_rgba(64, 64, 255, 0, 0, 255);
        let out = fit_square(img, 64);
        assert_eq!(out.width, 64);
        assert_eq!(out.height, 64);
        // First pixel should still be red.
        assert_eq!(&out.rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn fit_square_scales_up_small_image() {
        let img = solid_rgba(16, 16, 100, 150, 200, 255);
        let out = fit_square(img, 64);
        assert_eq!(out.width, 64);
        assert_eq!(out.height, 64);
        // Solid colour must survive interpolation untouched, everywhere.
        assert!(out.rgba.chunks_exact(4).all(|p| p == [100, 150, 200, 255]));
    }

    #[test]
    fn bilinear_upscale_is_smooth_not_blocky() {
        // 2×1 black|white → 8×1: nearest would give 4 black then 4 white;
        // bilinear must produce a monotone ramp with intermediate greys.
        let img = DecodedImage { width: 2, height: 1, rgba: vec![0, 0, 0, 255, 255, 255, 255, 255] };
        let out = scale_bilinear(&img, 8, 1);
        let greys: Vec<u8> = out.chunks_exact(4).map(|p| p[0]).collect();
        assert!(greys.windows(2).all(|w| w[0] <= w[1]), "{greys:?}");
        assert!(greys.iter().any(|&g| g > 20 && g < 235), "{greys:?}");
        assert_eq!((greys[0], greys[7]), (0, 255));
    }

    #[test]
    fn bilinear_upscale_does_not_fringe_transparent_colour() {
        // Opaque red next to a transparent WHITE pixel: interpolated pixels
        // must stay red (only alpha ramps), never go pink.
        let img = DecodedImage { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 255, 255, 255, 0] };
        let out = scale_bilinear(&img, 8, 1);
        for p in out.chunks_exact(4) {
            if p[3] > 0 {
                assert_eq!(&p[..3], &[255, 0, 0], "{p:?}");
            }
        }
    }

    #[test]
    fn fit_square_letterboxes_wide_image() {
        // 200×100 → fit in 100×100: scaled to 100×50, centred.
        let img = solid_rgba(200, 100, 0, 255, 0, 255);
        let out = fit_square(img, 100);
        assert_eq!(out.width, 100);
        assert_eq!(out.height, 100);
        // Top-left corner should be transparent (letterbox padding).
        assert_eq!(&out.rgba[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn fit_square_zero_size() {
        let img = solid_rgba(10, 10, 0, 0, 0, 255);
        let out = fit_square(img, 0);
        assert_eq!(out.width, 0);
        assert_eq!(out.height, 0);
    }

    #[test]
    fn to_bgra_premul_opaque() {
        let img = solid_rgba(1, 1, 200, 100, 50, 255);
        let bgra = to_bgra_premul(&img);
        // Opaque: no premultiplication change; byte order is BGRA.
        assert_eq!(bgra[0], 50);  // B
        assert_eq!(bgra[1], 100); // G
        assert_eq!(bgra[2], 200); // R
        assert_eq!(bgra[3], 255); // A
    }

    #[test]
    fn to_bgra_premul_transparent() {
        let img = solid_rgba(1, 1, 200, 100, 50, 0);
        let bgra = to_bgra_premul(&img);
        // Fully transparent: all channels premultiplied to 0.
        assert_eq!(bgra[0], 0); // B
        assert_eq!(bgra[1], 0); // G
        assert_eq!(bgra[2], 0); // R
        assert_eq!(bgra[3], 0); // A
    }

    #[test]
    fn to_bgra_premul_half_alpha() {
        // R=200, G=100, B=50, A=128 → premul ≈ R*128/255 ≈ 100, G*128/255 ≈ 50, B*128/255 ≈ 25
        let img = solid_rgba(1, 1, 200, 100, 50, 128);
        let bgra = to_bgra_premul(&img);
        // Allow ±1 for rounding.
        assert!((bgra[0] as i16 - 25).abs() <= 1, "B premul={}", bgra[0]);
        assert!((bgra[1] as i16 - 50).abs() <= 1, "G premul={}", bgra[1]);
        assert!((bgra[2] as i16 - 100).abs() <= 1, "R premul={}", bgra[2]);
        assert_eq!(bgra[3], 128);
    }
}
