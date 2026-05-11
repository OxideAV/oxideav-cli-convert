//! ICO-output side-channel for the convert verb.
//!
//! `.ico` files carry one OR MORE sub-images at different resolutions
//! inside a single container; the IM convention is to drive the
//! per-size set with `-define icon:auto-resize=W1,W2,…` on the command
//! line:
//!
//! ```text
//! convert src.png -define icon:auto-resize=16,32,48,64,128,256 out.ico
//! ```
//!
//! Each comma-separated `W` becomes an `W × W` sub-image down-scaled
//! from the source. Without `-define icon:auto-resize` the writer emits
//! a 1-entry `.ico` at the source dimensions (subject to the 1..=256
//! ICO size limit).
//!
//! Architecturally the runner sits next to [`crate::pdf_runner`] and
//! [`crate::mesh3d_runner`]: the multi-image fan-out doesn't fit the
//! regular pipeline path's single-`Frame::Video`-per-track shape, and
//! the routing (`Op::Define` key parsing → per-size resize → multi-
//! entry write) is convert-specific.
//!
//! Input decode covers PNG and BMP today via the standalone decoders
//! on those crates (`oxideav_png::decode_png_to_rgba`,
//! `oxideav_bmp::decode_bmp`). The pipeline path handles every other
//! input; if the user pairs `.ico` output with a non-PNG / non-BMP
//! input we surface a clear "input not supported on the ICO writer
//! path yet" error instead of silently bailing.

use std::fs;

use oxideav_core::{Error, PixelFormat, Result, VideoFrame, VideoPlane};
use oxideav_ico::{write_ico, IconImage, IconType, WriteOptions};
use oxideav_image_filter::{ImageFilter, Resize, VideoStreamParams};

use crate::op::{ConvertPlan, Op};
use crate::raster_io::RgbaImage;

/// Returns true when the plan's output extension is `.ico`.
///
/// `.cur` (Windows cursor) deliberately doesn't route here — the writer
/// supports `IconType::Cur` but the per-image hotspot doesn't have an
/// IM CLI surface yet; route `.cur` once a hotspot grammar exists.
pub fn is_ico_output(output: &str) -> bool {
    let ext = output.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    ext == "ico"
}

/// Run the ICO-output convert flow. Side-effect-only: writes a
/// single `.ico` file at `plan.output`.
pub fn run(plan: &ConvertPlan) -> Result<()> {
    let src = read_source_to_rgba(&plan.input)?;

    let sizes = parse_auto_resize_define(&plan.ops)?;

    let images: Vec<IconImage> = if sizes.is_empty() {
        // Single-entry MVP: write the source as-is at its current dims,
        // subject to the ICO 1..=256 bounds.
        let (w, h) = (src.width, src.height);
        validate_size(w, h)?;
        let rgba = ensure_rgba(src);
        vec![IconImage::from_rgba(w, h, rgba.pixels)]
    } else {
        // Multi-entry: bilinear-downscale the source to each requested
        // N×N square. The image-filter `Resize` is the same factory the
        // pipeline path uses for `-resize`, so the visual result is
        // identical to running `convert src.png -resize NxN! out_N.png`
        // for each entry.
        let mut out = Vec::with_capacity(sizes.len());
        for &s in &sizes {
            let scaled = resize_to_square(&src, s)?;
            out.push(IconImage::from_rgba(s, s, scaled.pixels));
        }
        out
    };

    let bytes = write_ico(IconType::Ico, &images, WriteOptions::default())
        .map_err(|e| Error::invalid(format!("convert: ICO encode failed: {e:?}")))?;
    fs::write(&plan.output, bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to write {}: {e}", plan.output)))?;
    Ok(())
}

/// Decode the input file into an `RgbaImage`. PNG and BMP are wired
/// today (covering the round-1 smoke commands). Anything else gets a
/// clear "not supported on the ICO writer path" error pointing the
/// user at the workaround (`convert in.X tmp.png && convert tmp.png
/// out.ico`).
fn read_source_to_rgba(input: &str) -> Result<RgbaImage> {
    let bytes = fs::read(input)
        .map_err(|e| Error::invalid(format!("convert: failed to read {input}: {e}")))?;
    let ext = input.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "apng" => decode_png(&bytes),
        "bmp" | "dib" => decode_bmp(&bytes),
        other => Err(Error::unsupported(format!(
            "convert: ICO writer: input extension '.{other}' not yet supported (today png/bmp); convert to png first"
        ))),
    }
}

fn decode_png(bytes: &[u8]) -> Result<RgbaImage> {
    let bmp = oxideav_png::decode_png_to_rgba(bytes)
        .map_err(|e| Error::invalid(format!("convert: PNG decode failed: {e:?}")))?;
    let stride = (bmp.width as usize) * 4;
    Ok(RgbaImage {
        width: bmp.width,
        height: bmp.height,
        pixels: bmp.data,
        stride,
    })
}

fn decode_bmp(bytes: &[u8]) -> Result<RgbaImage> {
    use oxideav_bmp::{decode_bmp as bmp_decode, BmpPixelFormat};
    let img = bmp_decode(bytes)
        .map_err(|e| Error::invalid(format!("convert: BMP decode failed: {e:?}")))?;
    let plane = img
        .planes
        .into_iter()
        .next()
        .ok_or_else(|| Error::invalid("convert: BMP decoded to zero planes"))?;
    let (pixels, stride) = match img.pixel_format {
        BmpPixelFormat::Rgba => (plane.data, plane.stride),
        BmpPixelFormat::Rgb24 => {
            // Promote Rgb24 → Rgba so the rest of the runner only has
            // one packed layout to think about.
            let w = img.width as usize;
            let h = img.height as usize;
            let mut out = Vec::with_capacity(w * h * 4);
            for row in 0..h {
                let r0 = row * plane.stride;
                for px in plane.data[r0..r0 + w * 3].chunks_exact(3) {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            (out, w * 4)
        }
        other => {
            return Err(Error::unsupported(format!(
                "convert: BMP decoded to {other:?}, only Rgb24/Rgba supported on ICO path"
            )));
        }
    };
    Ok(RgbaImage {
        width: img.width,
        height: img.height,
        pixels,
        stride,
    })
}

/// Ensure the image is packed RGBA (4 bpp). Rgb24 inputs get promoted
/// with an opaque alpha. The ICO writer always takes 32-bpp RGBA so we
/// don't carry the format split through the rest of the runner.
fn ensure_rgba(img: RgbaImage) -> RgbaImage {
    if !img.is_rgb() {
        return img;
    }
    let w = img.width as usize;
    let h = img.height as usize;
    let mut out = Vec::with_capacity(w * h * 4);
    for px in img.pixels.chunks_exact(3) {
        out.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    RgbaImage {
        width: img.width,
        height: img.height,
        pixels: out,
        stride: w * 4,
    }
}

/// Parse the `-define icon:auto-resize=W1,W2,…` value out of the op
/// stream. Returns a sorted (smallest-first) deduplicated list of
/// per-axis dimensions. The empty vec means "no auto-resize set —
/// single-entry MVP at source dims".
///
/// Multiple `-define icon:auto-resize` instances stack (last one wins,
/// matching IM's "last value of the key wins" semantics for `-define`).
pub(crate) fn parse_auto_resize_define(ops: &[Op]) -> Result<Vec<u32>> {
    // Last instance wins. We scan backwards and stop on the first match.
    let value = ops.iter().rev().find_map(|o| match o {
        Op::Define { key, value } if key.eq_ignore_ascii_case("icon:auto-resize") => {
            Some(value.clone())
        }
        _ => None,
    });
    let raw = match value {
        Some(Some(s)) => s,
        Some(None) => {
            // `-define icon:auto-resize` with no `=VALUE` is malformed —
            // surface it clearly rather than silently dropping.
            return Err(Error::invalid(
                "convert: -define icon:auto-resize requires a value (e.g. 'icon:auto-resize=16,32,48')",
            ));
        }
        None => return Ok(Vec::new()),
    };
    let mut sizes: Vec<u32> = Vec::new();
    for piece in raw.split(',') {
        let t = piece.trim();
        if t.is_empty() {
            continue;
        }
        let n: u32 = t.parse().map_err(|_| {
            Error::invalid(format!(
                "convert: -define icon:auto-resize: '{t}' is not a positive integer"
            ))
        })?;
        validate_size(n, n)?;
        sizes.push(n);
    }
    if sizes.is_empty() {
        return Err(Error::invalid(
            "convert: -define icon:auto-resize: value list is empty",
        ));
    }
    sizes.sort_unstable();
    sizes.dedup();
    Ok(sizes)
}

/// ICO format requires `1 ≤ dim ≤ 256` per axis (the directory entry
/// stores width / height as `u8` with `0` meaning `256`). Surface the
/// rejection at parse / dispatch time so the user sees the constraint
/// up front rather than mid-encode.
fn validate_size(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return Err(Error::invalid(format!(
            "convert: ICO size {w}×{h} out of range (each axis must be in 1..=256)"
        )));
    }
    Ok(())
}

/// Bilinear-downscale the source into an `N × N` RGBA buffer using the
/// `oxideav-image-filter` `Resize` factory. Same pixel result the
/// regular pipeline's `-resize` op produces.
fn resize_to_square(src: &RgbaImage, n: u32) -> Result<RgbaImage> {
    let src = ensure_rgba(src.clone());
    if src.width == n && src.height == n {
        return Ok(src);
    }
    let format = PixelFormat::Rgba;
    let in_w = src.width;
    let in_h = src.height;
    let frame = VideoFrame {
        pts: None,
        planes: vec![VideoPlane {
            stride: src.stride,
            data: src.pixels,
        }],
    };
    let params = VideoStreamParams {
        format,
        width: in_w,
        height: in_h,
    };
    let filter = Resize::new(n, n);
    let out = filter
        .apply(&frame, params)
        .map_err(|e| Error::invalid(format!("convert: Resize for ICO failed: {e:?}")))?;
    let plane = out
        .planes
        .into_iter()
        .next()
        .ok_or_else(|| Error::invalid("convert: Resize returned no planes"))?;
    Ok(RgbaImage {
        width: n,
        height: n,
        pixels: plane.data,
        stride: plane.stride,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op_define(key: &str, value: Option<&str>) -> Op {
        Op::Define {
            key: key.to_string(),
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn parse_auto_resize_simple_list() {
        let ops = vec![op_define("icon:auto-resize", Some("16,32,48,64,128,256"))];
        assert_eq!(
            parse_auto_resize_define(&ops).unwrap(),
            vec![16, 32, 48, 64, 128, 256]
        );
    }

    #[test]
    fn parse_auto_resize_sorts_and_dedups() {
        let ops = vec![op_define("icon:auto-resize", Some("256,16,32,16,128"))];
        assert_eq!(
            parse_auto_resize_define(&ops).unwrap(),
            vec![16, 32, 128, 256]
        );
    }

    #[test]
    fn parse_auto_resize_absent_returns_empty() {
        let ops = vec![Op::Quality(85)];
        assert!(parse_auto_resize_define(&ops).unwrap().is_empty());
    }

    #[test]
    fn parse_auto_resize_rejects_zero_and_oversize() {
        for v in ["0", "257", "1000"] {
            let ops = vec![op_define("icon:auto-resize", Some(v))];
            assert!(parse_auto_resize_define(&ops).is_err(), "v={v}");
        }
    }

    #[test]
    fn parse_auto_resize_rejects_non_numeric() {
        let ops = vec![op_define("icon:auto-resize", Some("16,thirty-two,48"))];
        assert!(parse_auto_resize_define(&ops).is_err());
    }

    #[test]
    fn parse_auto_resize_bare_key_errors() {
        let ops = vec![op_define("icon:auto-resize", None)];
        assert!(parse_auto_resize_define(&ops).is_err());
    }

    #[test]
    fn parse_auto_resize_empty_value_errors() {
        let ops = vec![op_define("icon:auto-resize", Some(""))];
        assert!(parse_auto_resize_define(&ops).is_err());
        let ops = vec![op_define("icon:auto-resize", Some(",,,"))];
        assert!(parse_auto_resize_define(&ops).is_err());
    }

    #[test]
    fn parse_auto_resize_case_insensitive_key() {
        let ops = vec![op_define("ICON:Auto-Resize", Some("16,32"))];
        assert_eq!(parse_auto_resize_define(&ops).unwrap(), vec![16, 32]);
    }

    #[test]
    fn parse_auto_resize_last_wins() {
        let ops = vec![
            op_define("icon:auto-resize", Some("16,32")),
            op_define("icon:auto-resize", Some("64,128")),
        ];
        assert_eq!(parse_auto_resize_define(&ops).unwrap(), vec![64, 128]);
    }

    #[test]
    fn is_ico_output_matches_case_insensitively() {
        assert!(is_ico_output("out.ico"));
        assert!(is_ico_output("OUT.ICO"));
        assert!(is_ico_output("path/to/out.ico"));
        assert!(!is_ico_output("out.png"));
        assert!(!is_ico_output("out.cur"));
        assert!(!is_ico_output("noext"));
    }

    #[test]
    fn validate_size_bounds() {
        assert!(validate_size(1, 1).is_ok());
        assert!(validate_size(256, 256).is_ok());
        assert!(validate_size(0, 32).is_err());
        assert!(validate_size(32, 0).is_err());
        assert!(validate_size(257, 32).is_err());
        assert!(validate_size(32, 257).is_err());
    }
}
