//! Inline pixel transforms for the IM-style geometry / negate / tonal ops.
//!
//! Round-1 covered the "no resampler needed" subset:
//! `-rotate {±90,±180,±270}`, `-flip`, `-flop`, `-crop WxH+X+Y`,
//! `-negate`. Round-after-next extends this to the round-37 tonal /
//! colour-grading ops (`-sharpen`, `-unsharp`, `-gamma`,
//! `-brightness-contrast`, `-contrast`, `-sepia`, `-modulate`,
//! `-level`, `-normalize`, `-threshold`, `-posterize`, `-solarize`,
//! `-grayscale`) so PDF inputs honour them on the side-channel render
//! path.
//!
//! Geometry / negate ops reduce to either:
//!
//! - a per-pixel 1:1 substitution (negate), or
//! - a row/column reshuffle (flip / flop / quarter-turn rotate / crop).
//!
//! Tonal ops dispatch into [`oxideav_image_filter`]'s pure-Rust
//! single-frame factories (`Sharpen`, `Gamma`, …) by wrapping the
//! [`RgbaImage`] in a [`VideoFrame`], applying the filter, and copying
//! back. Same factories the regular pipeline path uses via the JSON
//! registry — keeps the PDF side-channel pixel-identical to non-PDF
//! inputs at the same op chain.
//!
//! Each transform takes ownership of an [`RgbaImage`] (3- or 4-byte
//! packed) and returns a re-laid-out image.  Width/height/stride are
//! recomputed at each step so a chained call sequence Just Works
//! (`flip` → `rotate 90` → `crop` walks a coherent buffer at every
//! stage).
//!
//! The module is consumed by [`crate::pdf_runner`] and
//! [`crate::mesh3d_render`] today; once the non-PDF pipeline path
//! grows the same hook the executor will call straight in here too.

use oxideav_core::{Error, PixelFormat, VideoFrame, VideoPlane};
use oxideav_image_filter::{
    BrightnessContrast, Gamma, Grayscale, ImageFilter, Level, Modulate, Normalize, Posterize,
    Resize, Sepia, Sharpen, Solarize, Threshold, Trim, Unsharp, VideoStreamParams,
};

use crate::op::Op;
use crate::raster_io::RgbaImage;

/// Bytes per packed pixel (3 = Rgb24, 4 = Rgba). Computed off
/// `stride / width` since `RgbaImage` doesn't carry the format tag
/// directly.
fn bpp(img: &RgbaImage) -> usize {
    let w = img.width as usize;
    if w == 0 {
        // Defensive: a zero-width image can't be transformed; treat
        // it as RGBA so downstream array math doesn't divide by zero.
        return 4;
    }
    img.stride / w
}

/// Apply each `Op` in source order, mutating `img` in place where
/// possible and re-allocating where the layout changes (rotate/crop).
/// Ops the inline path doesn't own (Resize/Blur/Edge/Colors/etc.) are
/// silently skipped — they're handled upstream / downstream.
///
/// Returns `Err` for ops with bounds problems (out-of-range crop bbox)
/// so the user gets a clean message instead of a panic.
pub fn apply_pixel_transform_chain(mut img: RgbaImage, ops: &[Op]) -> Result<RgbaImage, String> {
    for op in ops {
        match op {
            Op::Rotate { degrees } => {
                img = rotate(img, *degrees);
            }
            Op::Flip => {
                flip(&mut img);
            }
            Op::Flop => {
                flop(&mut img);
            }
            Op::Crop { x, y, w, h } => {
                img = crop(img, *x, *y, *w, *h)?;
            }
            Op::Extent {
                width,
                height,
                x,
                y,
                bg,
            } => {
                img = extent(img, *width, *height, *x, *y, *bg)?;
            }
            Op::Negate => {
                negate(&mut img);
            }
            // ---- Tonal / colour-grading ops dispatched into
            // oxideav-image-filter. The factories take a VideoFrame; we
            // wrap-and-unwrap the RgbaImage so the side-channel sees the
            // same pixel-output the non-PDF pipeline path produces. ----
            Op::Sharpen { radius, sigma } => {
                img = run_image_filter(img, &Sharpen::new(*radius, *sigma))?;
            }
            Op::Unsharp {
                radius,
                sigma,
                amount,
                threshold,
            } => {
                img = run_image_filter(img, &Unsharp::new(*radius, *sigma, *amount, *threshold))?;
            }
            Op::Gamma { value } => {
                img = run_image_filter(img, &Gamma::new(*value))?;
            }
            Op::BrightnessContrast {
                brightness,
                contrast,
            } => {
                img = run_image_filter(img, &BrightnessContrast::new(*brightness, *contrast))?;
            }
            Op::Contrast { delta } => {
                let pct = (*delta as f32) * 5.0;
                img = run_image_filter(img, &BrightnessContrast::new(0.0, pct))?;
            }
            Op::Sepia { threshold } => {
                img = run_image_filter(img, &Sepia::new(*threshold))?;
            }
            Op::Modulate {
                brightness,
                saturation,
                hue,
            } => {
                // IM hue is "percent-of-base around 100" — translate to
                // degrees the same way plan_to_job does.
                let hue_degrees = (hue - 100.0) * 1.8;
                img = run_image_filter(img, &Modulate::new(*brightness, *saturation, hue_degrees))?;
            }
            Op::Level {
                black,
                gamma,
                white,
            } => {
                img = run_image_filter(img, &Level::new(*black, *white, *gamma))?;
            }
            Op::Normalize => {
                img = run_image_filter(img, &Normalize::new(0.0, 0.0))?;
            }
            Op::Threshold { value } => {
                img = run_image_filter(img, &Threshold::new(*value))?;
            }
            Op::Posterize { levels } => {
                img = run_image_filter(img, &Posterize::new(*levels))?;
            }
            Op::Solarize { value } => {
                img = run_image_filter(img, &Solarize::new(*value))?;
            }
            Op::Colorspace(cs) => {
                let lower = cs.to_ascii_lowercase();
                if lower == "gray" || lower == "grey" {
                    img = run_image_filter(
                        img,
                        &Grayscale::new()
                            .with_preserve_alpha(true)
                            .with_output_gray8(false),
                    )?;
                }
                // `rgb` / `srgb` are recorded no-ops.
            }
            // Geometry-aware resize. We know the source dims here, so
            // every [`ResizeMode`] is honoured; the pipeline path is
            // limited to `Default` / `Force` until the executor learns
            // to resolve the source-aware variants too.
            Op::Resize {
                width,
                height,
                mode,
            } => {
                let (out_w, out_h) = mode.resolve(*width, *height, img.width, img.height);
                if out_w == img.width && out_h == img.height {
                    // No-op for shrink-only / grow-only when the input
                    // already fits the policy; skip the resampler pass.
                    continue;
                }
                img = run_image_filter_resize(img, &Resize::new(out_w, out_h), out_w, out_h)?;
            }
            // Same shape as Resize for the side-channel; the Strip
            // half is honoured at encode time when a future
            // metadata-emitting raster encoder lands.
            Op::Thumbnail {
                width,
                height,
                mode,
            } => {
                let (out_w, out_h) = mode.resolve(*width, *height, img.width, img.height);
                if !(out_w == img.width && out_h == img.height) {
                    img = run_image_filter_resize(img, &Resize::new(out_w, out_h), out_w, out_h)?;
                }
            }
            // `-trim` is shape-changing: the factory computes the
            // non-background bbox at apply() time, so the output
            // dimensions aren't known up front. Recover them from the
            // returned plane's stride / data length (every supported
            // packed format has `stride = new_width * bpp`, and
            // `data.len() == stride * new_height`). The same crop fast-
            // paths the factory already takes (whole-image background,
            // whole-image content) are preserved by the
            // `run_image_filter_reshape` helper.
            Op::Trim { fuzz } => {
                img = run_image_filter_reshape(img, &Trim::new().with_fuzz(*fuzz))?;
            }
            // `-define` is a sink-side concern, not a pixel transform.
            Op::Define { .. } => {}
            // Other ops aren't ours: rasteriser / encoder / pipeline
            // applies them.
            _ => {}
        }
    }
    Ok(img)
}

/// Run a single image-filter on the RgbaImage by converting to
/// VideoFrame, applying, and copying back. Errors from the filter are
/// surfaced as the IM-style `String` shape the caller expects.
///
/// Output width/height are inherited from the input. Filters that
/// change shape (Resize, Edge → Gray8) MUST go through
/// [`run_image_filter_resize`] instead, which lets the caller declare
/// the new dimensions.
fn run_image_filter(img: RgbaImage, filter: &dyn ImageFilter) -> Result<RgbaImage, String> {
    run_image_filter_inner(img, filter, None)
}

/// Same as [`run_image_filter`] but the output uses the supplied
/// `(out_w, out_h)` dimensions. Used by the geometry-aware
/// [`Op::Resize`] / [`Op::Thumbnail`] arms which know the target size
/// before the filter runs.
fn run_image_filter_resize(
    img: RgbaImage,
    filter: &dyn ImageFilter,
    out_w: u32,
    out_h: u32,
) -> Result<RgbaImage, String> {
    run_image_filter_inner(img, filter, Some((out_w, out_h)))
}

fn run_image_filter_inner(
    img: RgbaImage,
    filter: &dyn ImageFilter,
    out_dims: Option<(u32, u32)>,
) -> Result<RgbaImage, String> {
    let format = if img.is_rgb() {
        PixelFormat::Rgb24
    } else {
        PixelFormat::Rgba
    };
    let in_w = img.width;
    let in_h = img.height;
    let frame = VideoFrame {
        pts: None,
        planes: vec![VideoPlane {
            stride: img.stride,
            data: img.pixels,
        }],
    };
    let params = VideoStreamParams {
        format,
        width: in_w,
        height: in_h,
    };
    let out = filter
        .apply(&frame, params)
        .map_err(|e: Error| format!("{e:?}"))?;
    let plane = out
        .planes
        .into_iter()
        .next()
        .ok_or_else(|| "image-filter returned no planes".to_string())?;
    let (w, h) = out_dims.unwrap_or((in_w, in_h));
    Ok(RgbaImage {
        width: w,
        height: h,
        pixels: plane.data,
        stride: plane.stride,
    })
}

/// Same as [`run_image_filter`] but the output dimensions are
/// **recovered** from the returned plane's stride / data length rather
/// than declared up front. Used by [`Op::Trim`] where the bbox crop is
/// computed inside the filter and the caller doesn't know the new size
/// before applying. Assumes the filter produces a packed plane
/// (`stride == new_width * bpp`, `data.len() == stride * new_height`),
/// which every [`oxideav_image_filter`] crop / trim path satisfies.
fn run_image_filter_reshape(img: RgbaImage, filter: &dyn ImageFilter) -> Result<RgbaImage, String> {
    let bpp_in = bpp(&img);
    let format = if img.is_rgb() {
        PixelFormat::Rgb24
    } else {
        PixelFormat::Rgba
    };
    let in_w = img.width;
    let in_h = img.height;
    let frame = VideoFrame {
        pts: None,
        planes: vec![VideoPlane {
            stride: img.stride,
            data: img.pixels,
        }],
    };
    let params = VideoStreamParams {
        format,
        width: in_w,
        height: in_h,
    };
    let out = filter
        .apply(&frame, params)
        .map_err(|e: Error| format!("{e:?}"))?;
    let plane = out
        .planes
        .into_iter()
        .next()
        .ok_or_else(|| "image-filter returned no planes".to_string())?;
    if bpp_in == 0 || plane.stride == 0 {
        return Err("image-filter returned a zero-stride plane".to_string());
    }
    // Recover (new_w, new_h) by inverting the packed-plane layout. The
    // factories under this path emit packed strides; defensive checks
    // catch any future regression to padded strides.
    if plane.stride % bpp_in != 0 {
        return Err(format!(
            "image-filter stride {} not a multiple of bpp {bpp_in}",
            plane.stride
        ));
    }
    let new_w = (plane.stride / bpp_in) as u32;
    if new_w == 0 || plane.data.len() % plane.stride != 0 {
        return Err(format!(
            "image-filter packed-plane mismatch: stride={}, data_len={}",
            plane.stride,
            plane.data.len()
        ));
    }
    let new_h = (plane.data.len() / plane.stride) as u32;
    Ok(RgbaImage {
        width: new_w,
        height: new_h,
        pixels: plane.data,
        stride: plane.stride,
    })
}

/// Vertical flip — reverse row order. Width / height / stride
/// unchanged.
pub fn flip(img: &mut RgbaImage) {
    let h = img.height as usize;
    if h < 2 {
        return;
    }
    let stride = img.stride;
    // Swap row i with row (h-1-i). Pulled out into a temp buffer
    // because Rust borrow checker won't let us index two mutable
    // slices at once.
    let mut tmp = vec![0u8; stride];
    for i in 0..h / 2 {
        let j = h - 1 - i;
        let (lo, hi) = img.pixels.split_at_mut(j * stride);
        let row_i = &mut lo[i * stride..i * stride + stride];
        let row_j = &mut hi[..stride];
        tmp.copy_from_slice(row_i);
        row_i.copy_from_slice(row_j);
        row_j.copy_from_slice(&tmp);
    }
}

/// Horizontal flip — reverse column order within each row.
pub fn flop(img: &mut RgbaImage) {
    let w = img.width as usize;
    let h = img.height as usize;
    if w < 2 {
        return;
    }
    let bpp = bpp(img);
    let stride = img.stride;
    for row in 0..h {
        let base = row * stride;
        // Swap pixel at column c with column (w-1-c).
        for c in 0..w / 2 {
            let lo = base + c * bpp;
            let hi = base + (w - 1 - c) * bpp;
            for k in 0..bpp {
                img.pixels.swap(lo + k, hi + k);
            }
        }
    }
}

/// Per-pixel `out = 255 - in` on the colour channels (R/G/B). Alpha
/// (when present) is left unchanged.
pub fn negate(img: &mut RgbaImage) {
    let bpp = bpp(img);
    if bpp == 4 {
        for px in img.pixels.chunks_exact_mut(4) {
            px[0] = 255 - px[0];
            px[1] = 255 - px[1];
            px[2] = 255 - px[2];
            // px[3] (alpha) untouched.
        }
    } else {
        for px in img.pixels.chunks_exact_mut(3) {
            px[0] = 255 - px[0];
            px[1] = 255 - px[1];
            px[2] = 255 - px[2];
        }
    }
}

/// Quarter-turn rotation. `degrees` must be one of `{±90,±180,±270}`.
/// 90/270 swap width and height (and rebuild the stride).
pub fn rotate(img: RgbaImage, degrees: i32) -> RgbaImage {
    // Normalise to {0, 90, 180, 270}. Inputs outside this set are
    // already rejected by args::parse, so any deviation here is a
    // contract violation; treat as identity rather than panic.
    let n = degrees.rem_euclid(360);
    match n {
        0 => img,
        180 => rotate_180(img),
        90 => rotate_90_cw(img),
        270 => rotate_270_cw(img),
        _ => img,
    }
}

fn rotate_180(mut img: RgbaImage) -> RgbaImage {
    flip(&mut img);
    flop(&mut img);
    img
}

fn rotate_90_cw(img: RgbaImage) -> RgbaImage {
    let w = img.width as usize;
    let h = img.height as usize;
    let bpp = bpp(&img);
    let new_w = h;
    let new_h = w;
    let new_stride = new_w * bpp;
    let mut out = vec![0u8; new_h * new_stride];
    // Source (x, y) → destination (h - 1 - y, x).
    for y in 0..h {
        for x in 0..w {
            let src = y * img.stride + x * bpp;
            let dst_x = h - 1 - y;
            let dst_y = x;
            let dst = dst_y * new_stride + dst_x * bpp;
            out[dst..dst + bpp].copy_from_slice(&img.pixels[src..src + bpp]);
        }
    }
    RgbaImage {
        width: new_w as u32,
        height: new_h as u32,
        pixels: out,
        stride: new_stride,
    }
}

fn rotate_270_cw(img: RgbaImage) -> RgbaImage {
    let w = img.width as usize;
    let h = img.height as usize;
    let bpp = bpp(&img);
    let new_w = h;
    let new_h = w;
    let new_stride = new_w * bpp;
    let mut out = vec![0u8; new_h * new_stride];
    // Source (x, y) → destination (y, w - 1 - x).
    for y in 0..h {
        for x in 0..w {
            let src = y * img.stride + x * bpp;
            let dst_x = y;
            let dst_y = w - 1 - x;
            let dst = dst_y * new_stride + dst_x * bpp;
            out[dst..dst + bpp].copy_from_slice(&img.pixels[src..src + bpp]);
        }
    }
    RgbaImage {
        width: new_w as u32,
        height: new_h as u32,
        pixels: out,
        stride: new_stride,
    }
}

/// Re-window `img` onto a `(out_w, out_h)` canvas with the source
/// placed at signed offset `(off_x, off_y)`. Pixels outside the source
/// rectangle are filled with `bg` (RGBA; the alpha byte is dropped on
/// Rgb24 outputs).
///
/// Negative offsets translate the source toward the upper-left so its
/// right / bottom edge can land inside the window — this is the IM
/// convention for `-extent WxH-X-Y`. A `(0, 0)` offset places the
/// source at the canvas top-left; an offset larger than the canvas
/// produces an all-background output (no source pixels visible) rather
/// than an error, also matching IM.
pub fn extent(
    img: RgbaImage,
    out_w: u32,
    out_h: u32,
    off_x: i32,
    off_y: i32,
    bg: [u8; 4],
) -> Result<RgbaImage, String> {
    if out_w == 0 || out_h == 0 {
        return Err(format!(
            "extent canvas {out_w}x{out_h} has zero width/height"
        ));
    }
    let bpp = bpp(&img);
    let new_stride = (out_w as usize) * bpp;
    let canvas_len = (out_h as usize)
        .checked_mul(new_stride)
        .ok_or_else(|| format!("extent canvas {out_w}x{out_h} byte size overflows usize"))?;

    // Build the per-pixel fill once, then memcpy across each output
    // row. RGB outputs drop the alpha byte; RGBA keep it.
    let fill: [u8; 4] = bg;
    let mut out = vec![0u8; canvas_len];
    // Pre-fill one row's worth of background and replicate.
    let mut row0 = vec![0u8; new_stride];
    for chunk in row0.chunks_exact_mut(bpp) {
        chunk[0] = fill[0];
        chunk[1] = fill[1];
        chunk[2] = fill[2];
        if bpp == 4 {
            chunk[3] = fill[3];
        }
    }
    for row in 0..(out_h as usize) {
        let dst_off = row * new_stride;
        out[dst_off..dst_off + new_stride].copy_from_slice(&row0);
    }

    // Stamp the source rectangle on top, intersected with the canvas.
    // Working in signed i64 keeps `out_w + |off|` in range without
    // overflow even for u32::MAX-ish dims.
    let src_w = img.width as i64;
    let src_h = img.height as i64;
    let canvas_w = out_w as i64;
    let canvas_h = out_h as i64;
    let ox = off_x as i64;
    let oy = off_y as i64;

    // Visible source rectangle in source-image coordinates.
    let sx_start = (-ox).max(0);
    let sy_start = (-oy).max(0);
    let sx_end = (canvas_w - ox).min(src_w);
    let sy_end = (canvas_h - oy).min(src_h);
    if sx_start >= sx_end || sy_start >= sy_end {
        // Source rectangle is entirely outside the canvas — return the
        // all-background image. This is the IM behaviour for offsets
        // that translate the source out of view.
        return Ok(RgbaImage {
            width: out_w,
            height: out_h,
            pixels: out,
            stride: new_stride,
        });
    }

    let copy_w = (sx_end - sx_start) as usize;
    let copy_h = (sy_end - sy_start) as usize;
    let dst_x = (sx_start + ox) as usize;
    let dst_y = (sy_start + oy) as usize;
    let src_x = sx_start as usize;
    let src_y = sy_start as usize;

    let row_bytes = copy_w * bpp;
    for row in 0..copy_h {
        let src_row_off = (src_y + row) * img.stride + src_x * bpp;
        let dst_row_off = (dst_y + row) * new_stride + dst_x * bpp;
        out[dst_row_off..dst_row_off + row_bytes]
            .copy_from_slice(&img.pixels[src_row_off..src_row_off + row_bytes]);
    }

    Ok(RgbaImage {
        width: out_w,
        height: out_h,
        pixels: out,
        stride: new_stride,
    })
}

/// Extract a `WxH` bbox at offset `(x, y)`. Errors when the bbox runs
/// past the input dimensions — the caller surfaces the IM-style
/// "bbox WxH+X+Y exceeds input W'xH'" message verbatim.
pub fn crop(img: RgbaImage, x: u32, y: u32, w: u32, h: u32) -> Result<RgbaImage, String> {
    if w == 0 || h == 0 {
        return Err(format!("bbox {w}x{h}+{x}+{y} has zero width/height"));
    }
    let in_w = img.width;
    let in_h = img.height;
    let x_end = x
        .checked_add(w)
        .ok_or_else(|| format!("bbox {w}x{h}+{x}+{y} arithmetic overflow"))?;
    let y_end = y
        .checked_add(h)
        .ok_or_else(|| format!("bbox {w}x{h}+{x}+{y} arithmetic overflow"))?;
    if x_end > in_w || y_end > in_h {
        return Err(format!("bbox {w}x{h}+{x}+{y} exceeds input {in_w}x{in_h}"));
    }
    let bpp = bpp(&img);
    let new_stride = (w as usize) * bpp;
    let mut out = vec![0u8; (h as usize) * new_stride];
    for row in 0..(h as usize) {
        let src_y = (y as usize) + row;
        let src_x_byte = (x as usize) * bpp;
        let src_off = src_y * img.stride + src_x_byte;
        let dst_off = row * new_stride;
        out[dst_off..dst_off + new_stride]
            .copy_from_slice(&img.pixels[src_off..src_off + new_stride]);
    }
    Ok(RgbaImage {
        width: w,
        height: h,
        pixels: out,
        stride: new_stride,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::ResizeMode;

    /// Build a 2x2 RGBA image with a unique colour per pixel so any
    /// reshuffle is detectable by inspecting one byte per pixel.
    /// Layout (row-major):
    ///   (0,0) = [0x10, 0x11, 0x12, 0xff]
    ///   (1,0) = [0x20, 0x21, 0x22, 0xff]
    ///   (0,1) = [0x30, 0x31, 0x32, 0xff]
    ///   (1,1) = [0x40, 0x41, 0x42, 0xff]
    fn img_2x2_rgba() -> RgbaImage {
        RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                0x10, 0x11, 0x12, 0xff, 0x20, 0x21, 0x22, 0xff, 0x30, 0x31, 0x32, 0xff, 0x40, 0x41,
                0x42, 0xff,
            ],
            stride: 8,
        }
    }

    /// 4x4 checkerboard of two colours — used to probe flip-flop
    /// idempotence.
    fn checkerboard_4x4() -> RgbaImage {
        let a = [0x10, 0x20, 0x30, 0xff];
        let b = [0xc0, 0xd0, 0xe0, 0xff];
        let mut data = Vec::with_capacity(64);
        for y in 0..4 {
            for x in 0..4 {
                let p = if (x + y) % 2 == 0 { &a } else { &b };
                data.extend_from_slice(p);
            }
        }
        RgbaImage {
            width: 4,
            height: 4,
            pixels: data,
            stride: 16,
        }
    }

    fn pixel(img: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
        let bpp = bpp(img);
        let off = (y as usize) * img.stride + (x as usize) * bpp;
        [
            img.pixels[off],
            img.pixels[off + 1],
            img.pixels[off + 2],
            img.pixels[off + 3],
        ]
    }

    // ---- flip / flop ----

    #[test]
    fn flip_swaps_rows() {
        let mut img = img_2x2_rgba();
        flip(&mut img);
        // Row 0 should now hold what row 1 used to hold.
        assert_eq!(pixel(&img, 0, 0), [0x30, 0x31, 0x32, 0xff]);
        assert_eq!(pixel(&img, 1, 0), [0x40, 0x41, 0x42, 0xff]);
        assert_eq!(pixel(&img, 0, 1), [0x10, 0x11, 0x12, 0xff]);
        assert_eq!(pixel(&img, 1, 1), [0x20, 0x21, 0x22, 0xff]);
    }

    #[test]
    fn flop_swaps_columns() {
        let mut img = img_2x2_rgba();
        flop(&mut img);
        assert_eq!(pixel(&img, 0, 0), [0x20, 0x21, 0x22, 0xff]);
        assert_eq!(pixel(&img, 1, 0), [0x10, 0x11, 0x12, 0xff]);
        assert_eq!(pixel(&img, 0, 1), [0x40, 0x41, 0x42, 0xff]);
        assert_eq!(pixel(&img, 1, 1), [0x30, 0x31, 0x32, 0xff]);
    }

    #[test]
    fn flip_is_self_inverse() {
        let original = img_2x2_rgba().pixels.clone();
        let mut img = img_2x2_rgba();
        flip(&mut img);
        flip(&mut img);
        assert_eq!(img.pixels, original);
    }

    #[test]
    fn flip_then_flop_equals_rotate_180() {
        let original = checkerboard_4x4();
        // Checkerboard: every diagonal alternation means rotating 180°
        // produces the SAME pattern (the 4x4 is symmetric under that
        // shuffle). Confirm flip+flop matches.
        let mut a = checkerboard_4x4();
        flip(&mut a);
        flop(&mut a);
        assert_eq!(
            a.pixels, original.pixels,
            "flip+flop on a checkerboard should round-trip back to itself"
        );
        // And independently confirm rotate(180) does the same.
        let b = rotate(checkerboard_4x4(), 180);
        assert_eq!(b.pixels, original.pixels);
    }

    // ---- rotate ----

    #[test]
    fn rotate_90_swaps_dims() {
        let img = img_2x2_rgba();
        let r = rotate(img, 90);
        assert_eq!(r.width, 2);
        assert_eq!(r.height, 2);
        // (0,0) of the source ends up at (h-1, 0) = (1, 0) after 90°cw.
        assert_eq!(pixel(&r, 1, 0), [0x10, 0x11, 0x12, 0xff]);
        // (1,0) → (1, 1).
        assert_eq!(pixel(&r, 1, 1), [0x20, 0x21, 0x22, 0xff]);
        // (0,1) → (0, 0).
        assert_eq!(pixel(&r, 0, 0), [0x30, 0x31, 0x32, 0xff]);
        // (1,1) → (0, 1).
        assert_eq!(pixel(&r, 0, 1), [0x40, 0x41, 0x42, 0xff]);
    }

    #[test]
    fn rotate_90_then_270_round_trips() {
        let original = img_2x2_rgba().pixels.clone();
        let r = rotate(rotate(img_2x2_rgba(), 90), 270);
        assert_eq!(r.pixels, original);
        assert_eq!(r.width, 2);
        assert_eq!(r.height, 2);
    }

    #[test]
    fn rotate_180_preserves_dims() {
        let r = rotate(img_2x2_rgba(), 180);
        assert_eq!(r.width, 2);
        assert_eq!(r.height, 2);
        assert_eq!(pixel(&r, 1, 1), [0x10, 0x11, 0x12, 0xff]);
        assert_eq!(pixel(&r, 0, 0), [0x40, 0x41, 0x42, 0xff]);
    }

    #[test]
    fn rotate_negative_90_equals_270() {
        let a = rotate(img_2x2_rgba(), -90);
        let b = rotate(img_2x2_rgba(), 270);
        assert_eq!(a.pixels, b.pixels);
        assert_eq!((a.width, a.height), (b.width, b.height));
    }

    #[test]
    fn rotate_360_is_identity() {
        let original = img_2x2_rgba().pixels.clone();
        let r = rotate(img_2x2_rgba(), -360);
        assert_eq!(r.pixels, original);
    }

    #[test]
    fn rotate_90_on_non_square_swaps_width_and_height() {
        // 3 wide × 1 tall RGBA strip.
        let img = RgbaImage {
            width: 3,
            height: 1,
            pixels: vec![0xaa, 0, 0, 0xff, 0xbb, 0, 0, 0xff, 0xcc, 0, 0, 0xff],
            stride: 12,
        };
        let r = rotate(img, 90);
        assert_eq!(r.width, 1);
        assert_eq!(r.height, 3);
        // Rows top-to-bottom should be the original columns
        // right-to-left: cc, bb, aa.
        assert_eq!(pixel(&r, 0, 0), [0xaa, 0, 0, 0xff]);
        assert_eq!(pixel(&r, 0, 1), [0xbb, 0, 0, 0xff]);
        assert_eq!(pixel(&r, 0, 2), [0xcc, 0, 0, 0xff]);
    }

    // ---- crop ----

    #[test]
    fn crop_extracts_inner_pixel() {
        let img = img_2x2_rgba();
        let c = crop(img, 1, 1, 1, 1).unwrap();
        assert_eq!(c.width, 1);
        assert_eq!(c.height, 1);
        assert_eq!(c.pixels, vec![0x40, 0x41, 0x42, 0xff]);
    }

    #[test]
    fn crop_full_image_is_identity() {
        let img = img_2x2_rgba();
        let original = img.pixels.clone();
        let c = crop(img, 0, 0, 2, 2).unwrap();
        assert_eq!(c.pixels, original);
        assert_eq!((c.width, c.height), (2, 2));
    }

    #[test]
    fn crop_out_of_bounds_errors() {
        let err = crop(img_2x2_rgba(), 1, 1, 2, 2).unwrap_err();
        assert!(err.contains("exceeds input"));
        assert!(err.contains("2x2"));
    }

    #[test]
    fn crop_zero_size_errors() {
        let err = crop(img_2x2_rgba(), 0, 0, 0, 1).unwrap_err();
        assert!(err.contains("zero width/height"));
    }

    // ---- extent ----

    // Zero-offset, equal-sized canvas is a byte-for-byte copy: every
    // input pixel maps 1:1, no background pixels visible.
    #[test]
    fn extent_zero_offset_same_size_is_identity() {
        let img = img_2x2_rgba();
        let original = img.pixels.clone();
        let out = extent(img, 2, 2, 0, 0, [0xff, 0xff, 0xff, 0xff]).unwrap();
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 2);
        assert_eq!(out.pixels, original);
    }

    // Growing the canvas paints the new pixels with the background
    // colour and leaves the source rectangle at (0, 0) untouched.
    #[test]
    fn extent_grows_canvas_pads_background() {
        let img = img_2x2_rgba();
        let bg = [0xaa, 0xbb, 0xcc, 0xdd];
        let out = extent(img, 4, 3, 0, 0, bg).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 3);
        // Source pixels intact at top-left.
        assert_eq!(pixel(&out, 0, 0), [0x10, 0x11, 0x12, 0xff]);
        assert_eq!(pixel(&out, 1, 0), [0x20, 0x21, 0x22, 0xff]);
        // Background fills the extension.
        assert_eq!(pixel(&out, 2, 0), bg);
        assert_eq!(pixel(&out, 3, 0), bg);
        assert_eq!(pixel(&out, 0, 2), bg);
        assert_eq!(pixel(&out, 3, 2), bg);
    }

    // Positive offset shifts the source toward the lower-right; the
    // top / left rows become background.
    #[test]
    fn extent_positive_offset_shifts_source() {
        let img = img_2x2_rgba();
        let bg = [0x55, 0x55, 0x55, 0xff];
        let out = extent(img, 4, 4, 1, 1, bg).unwrap();
        assert_eq!(pixel(&out, 0, 0), bg);
        assert_eq!(pixel(&out, 1, 0), bg);
        // Source pixels offset by (1, 1).
        assert_eq!(pixel(&out, 1, 1), [0x10, 0x11, 0x12, 0xff]);
        assert_eq!(pixel(&out, 2, 1), [0x20, 0x21, 0x22, 0xff]);
        assert_eq!(pixel(&out, 1, 2), [0x30, 0x31, 0x32, 0xff]);
        assert_eq!(pixel(&out, 2, 2), [0x40, 0x41, 0x42, 0xff]);
        // Bottom-right is still background.
        assert_eq!(pixel(&out, 3, 3), bg);
    }

    // Negative offset translates the source toward the upper-left so
    // its right / bottom edge can land inside a smaller canvas —
    // pixels that fall off the canvas are discarded.
    #[test]
    fn extent_negative_offset_clips_source() {
        let img = img_2x2_rgba();
        let bg = [0x55, 0x55, 0x55, 0xff];
        // 1x1 canvas with offset (-1, -1) shows ONLY the source's
        // bottom-right pixel.
        let out = extent(img, 1, 1, -1, -1, bg).unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        assert_eq!(pixel(&out, 0, 0), [0x40, 0x41, 0x42, 0xff]);
    }

    // Source fully outside canvas → all-background output (not an
    // error). Mirrors IM's behaviour.
    #[test]
    fn extent_offset_off_canvas_yields_all_background() {
        let img = img_2x2_rgba();
        let bg = [0xab, 0xcd, 0xef, 0xff];
        let out = extent(img, 2, 2, 10, 10, bg).unwrap();
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(pixel(&out, x, y), bg);
            }
        }
    }

    // Shrinking the canvas keeps only the intersected window — the
    // top-left WxH bytes survive when offset is (0, 0).
    #[test]
    fn extent_shrinks_canvas_keeps_intersection() {
        let img = img_2x2_rgba();
        let out = extent(img, 1, 1, 0, 0, [0; 4]).unwrap();
        assert_eq!(pixel(&out, 0, 0), [0x10, 0x11, 0x12, 0xff]);
    }

    // Zero canvas dims are rejected — there's nothing to render.
    #[test]
    fn extent_zero_canvas_errors() {
        let err = extent(img_2x2_rgba(), 0, 4, 0, 0, [0; 4]).unwrap_err();
        assert!(err.contains("zero width/height"));
    }

    // RGB-24 (no alpha) input: the helper drops the alpha byte from
    // the background and copies the 3-byte pixels untouched.
    #[test]
    fn extent_rgb24_drops_alpha_from_background() {
        let img = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![0x10, 0x20, 0x30],
            stride: 3,
        };
        let out = extent(img, 2, 1, 0, 0, [0xaa, 0xbb, 0xcc, 0xff]).unwrap();
        assert_eq!(out.stride, 6);
        assert_eq!(out.pixels, vec![0x10, 0x20, 0x30, 0xaa, 0xbb, 0xcc]);
    }

    // ---- negate ----

    #[test]
    fn negate_inverts_rgb_channels_keeps_alpha() {
        let mut img = img_2x2_rgba();
        negate(&mut img);
        assert_eq!(pixel(&img, 0, 0), [0xef, 0xee, 0xed, 0xff]);
        assert_eq!(pixel(&img, 1, 0), [0xdf, 0xde, 0xdd, 0xff]);
        assert_eq!(pixel(&img, 0, 1), [0xcf, 0xce, 0xcd, 0xff]);
        assert_eq!(pixel(&img, 1, 1), [0xbf, 0xbe, 0xbd, 0xff]);
    }

    #[test]
    fn negate_is_self_inverse() {
        let original = img_2x2_rgba().pixels.clone();
        let mut img = img_2x2_rgba();
        negate(&mut img);
        negate(&mut img);
        assert_eq!(img.pixels, original);
    }

    #[test]
    fn negate_handles_rgb24_no_alpha() {
        // 1x1 Rgb24 image (stride = 3, no alpha byte).
        let mut img = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![0x10, 0x20, 0x30],
            stride: 3,
        };
        negate(&mut img);
        assert_eq!(img.pixels, vec![0xef, 0xdf, 0xcf]);
    }

    // ---- chain ----

    #[test]
    fn apply_chain_runs_in_source_order() {
        // Negate then flip — the negate should land on the original
        // pixels and the flip should reorder rows of the negated
        // image.
        let img = img_2x2_rgba();
        let out = apply_pixel_transform_chain(img, &[Op::Negate, Op::Flip]).unwrap();
        // Original (0,1) was [0x30,0x31,0x32]; negated → [0xcf,0xce,0xcd];
        // flip moves row 1 to row 0.
        assert_eq!(pixel(&out, 0, 0), [0xcf, 0xce, 0xcd, 0xff]);
    }

    #[test]
    fn apply_chain_flip_flop_round_trips_checkerboard() {
        let original = checkerboard_4x4();
        let out = apply_pixel_transform_chain(checkerboard_4x4(), &[Op::Flip, Op::Flop]).unwrap();
        assert_eq!(out.pixels, original.pixels);
    }

    #[test]
    fn apply_chain_skips_unhandled_ops() {
        // Blur / Edge / Colors / etc. pass through unchanged. Resize
        // IS now handled by pixel_xform (the source dims are known
        // here), so test those separately below.
        let img = img_2x2_rgba();
        let original = img.pixels.clone();
        let out = apply_pixel_transform_chain(img, &[Op::Strip]).unwrap();
        assert_eq!(out.pixels, original);
    }

    #[test]
    fn apply_chain_resize_default_aspect_fit() {
        // 2×2 source, target 4×8 default (fit-inside) → both axes scale by
        // 2.0 (limited by width), so output is 4×4.
        let img = img_2x2_rgba();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Resize {
                width: 4,
                height: 8,
                mode: ResizeMode::Default,
            }],
        )
        .unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
    }

    #[test]
    fn apply_chain_resize_force_ignores_aspect() {
        // 2×2 source, target 4×8 force → exact 4×8.
        let img = img_2x2_rgba();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Resize {
                width: 4,
                height: 8,
                mode: ResizeMode::Force,
            }],
        )
        .unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 8);
    }

    #[test]
    fn apply_chain_resize_fill_picks_larger_scale() {
        // 2×2 source, target 4×8 fill → scale = max(4/2, 8/2) = 4 → 8×8.
        let img = img_2x2_rgba();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Resize {
                width: 4,
                height: 8,
                mode: ResizeMode::Fill,
            }],
        )
        .unwrap();
        assert_eq!(out.width, 8);
        assert_eq!(out.height, 8);
    }

    #[test]
    fn apply_chain_resize_shrink_only_passes_through_smaller() {
        // 2×2 source, target 100×100 shrink-only → input is already
        // smaller than target → no-op.
        let img = img_2x2_rgba();
        let original = img.pixels.clone();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Resize {
                width: 100,
                height: 100,
                mode: ResizeMode::Shrink,
            }],
        )
        .unwrap();
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 2);
        assert_eq!(out.pixels, original);
    }

    #[test]
    fn apply_chain_resize_percent_50_halves_dims() {
        // 2×2 source, percent 50% on both axes → 1×1.
        let img = img_2x2_rgba();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Resize {
                width: 50,
                height: 50,
                mode: ResizeMode::Percent,
            }],
        )
        .unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
    }

    #[test]
    fn apply_chain_thumbnail_resizes_and_drops_metadata_op() {
        // Thumbnail in pixel_xform reduces to a resize at the resolved
        // dims; the Strip half is the encoder's job.
        let img = img_2x2_rgba();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Thumbnail {
                width: 4,
                height: 4,
                mode: ResizeMode::Default,
            }],
        )
        .unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
    }

    #[test]
    fn apply_chain_define_is_sink_side_no_op() {
        // -define is a sink-side hint; the pixel transform chain leaves
        // the pixels alone.
        let img = img_2x2_rgba();
        let original = img.pixels.clone();
        let out = apply_pixel_transform_chain(
            img,
            &[Op::Define {
                key: "jpeg:dct-method".into(),
                value: Some("float".into()),
            }],
        )
        .unwrap();
        assert_eq!(out.pixels, original);
    }

    #[test]
    fn apply_chain_propagates_crop_error() {
        let err = apply_pixel_transform_chain(
            img_2x2_rgba(),
            &[Op::Crop {
                x: 0,
                y: 0,
                w: 5,
                h: 5,
            }],
        )
        .unwrap_err();
        assert!(err.contains("exceeds input"));
    }

    /// `Op::Trim` cuts a uniform border around a smaller non-background
    /// region. Build a 4×4 white image with a single non-white pixel at
    /// `(1, 1)`; trim should collapse to a 1×1 image holding that pixel.
    /// Pins the bbox-from-stride recovery used by the side-channel.
    #[test]
    fn apply_chain_trim_recovers_new_dims_from_stride() {
        let bg = [0xff, 0xff, 0xff, 0xff];
        let fg = [0x10, 0x20, 0x30, 0xff];
        let mut data = Vec::with_capacity(64);
        for y in 0..4u32 {
            for x in 0..4u32 {
                let p = if x == 1 && y == 1 { &fg } else { &bg };
                data.extend_from_slice(p);
            }
        }
        let img = RgbaImage {
            width: 4,
            height: 4,
            pixels: data,
            stride: 16,
        };
        let out = apply_pixel_transform_chain(img, &[Op::Trim { fuzz: 0 }]).unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        // The retained pixel is the foreground sample.
        assert_eq!(&out.pixels[..4], &fg[..]);
        // Packed stride: 1 pixel × 4 bytes.
        assert_eq!(out.stride, 4);
    }

    /// `Op::Trim` with `fuzz` >= the actual per-channel border deviation
    /// matches the would-be background even though the corner pixel
    /// isn't bit-identical; the bbox shrinks to the inner content only.
    /// Pins the `with_fuzz` plumbing.
    #[test]
    fn apply_chain_trim_honours_fuzz_tolerance() {
        let bg = [0xfe, 0xfe, 0xfe, 0xff]; // not pure white
        let fg = [0x10, 0x20, 0x30, 0xff];
        let mut data = Vec::with_capacity(64);
        for y in 0..4u32 {
            for x in 0..4u32 {
                let p = if x == 2 && y == 2 { &fg } else { &bg };
                data.extend_from_slice(p);
            }
        }
        let img = RgbaImage {
            width: 4,
            height: 4,
            pixels: data,
            stride: 16,
        };
        // fuzz=2 covers the 2-unit deviation between bg and pure white;
        // the corner pixel is the reference so its self-similarity is
        // exact and only `fg` differs by more than fuzz.
        let out = apply_pixel_transform_chain(img, &[Op::Trim { fuzz: 2 }]).unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        assert_eq!(&out.pixels[..4], &fg[..]);
    }

    /// Uniform-background image — trim's "no foreground" branch collapses
    /// to a 1×1 pixel (preserving a representable frame rather than
    /// returning an empty buffer). Pins the factory's documented fall-
    /// back so callers downstream of the side-channel keep a valid image.
    #[test]
    fn apply_chain_trim_uniform_collapses_to_1x1() {
        let bg = [0x80, 0x80, 0x80, 0xff];
        let mut data = Vec::with_capacity(64);
        for _ in 0..16 {
            data.extend_from_slice(&bg);
        }
        let img = RgbaImage {
            width: 4,
            height: 4,
            pixels: data,
            stride: 16,
        };
        let out = apply_pixel_transform_chain(img, &[Op::Trim { fuzz: 0 }]).unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        // The retained pixel is the corner reference itself.
        assert_eq!(&out.pixels[..4], &bg[..]);
    }
}
