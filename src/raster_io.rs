//! Raster output plumbing shared by every side-channel that produces
//! pixel buffers (PDF page rasterisation, 3D scene rendering).
//!
//! Centralises:
//!
//! * The owned [`RgbaImage`] handoff buffer.
//! * Raster output classification ([`OutputClass`] / [`RasterFormat`])
//!   so a single match-arm covers `.png` / `.jpg` / `.bmp` / `.webp`.
//! * The per-format encoders (`encode_png`, `encode_jpeg`,
//!   `encode_bmp`, `encode_webp`) that wrap the workspace's
//!   `oxideav-png` / `oxideav-mjpeg` / `oxideav-bmp` / `oxideav-webp`
//!   public APIs.
//! * The alpha-channel post-processors (`-alpha on/off/remove/set/
//!   transparent`) so both the PDF runner and the 3D-render runner
//!   honour `-alpha` identically.
//!
//! Keeping these one module up means the new 3D-render side-channel
//! doesn't have to reach into [`crate::pdf_runner`] internals — and a
//! future TIFF / GIF / PPM encoder lands in one place rather than two.

use std::fs;
use std::path::Path;

use oxideav_core::{Error, PixelFormat, Result, VideoFrame, VideoPlane};
use oxideav_pixfmt::{convert as pix_convert, ConvertOptions, FrameInfo};

use crate::op::{AlphaOp, Op};

/// What kind of output the convert verb is producing — drives the
/// routing decision in the side-channel runners. Add new arms when new
/// formats are wired (e.g. `.tiff` → Raster once oxideav-tiff has an
/// encoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputClass {
    /// Format consumes a whole [`oxideav_scene::Scene`] (`.pdf` today;
    /// multi-page SVG would land here if we ever support it).
    Scene,
    /// Format takes a single [`oxideav_core::vector::VectorFrame`]
    /// per file, no rasterisation (`.svg`).
    Vector,
    /// Format takes a rasterised RGBA / RGB buffer per file
    /// (`.png` `.jpg` `.bmp` `.webp`, …).
    Raster(RasterFormat),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RasterFormat {
    Png,
    Jpeg,
    Bmp,
    Webp,
}

/// Classify an output path / printf template by extension.
///
/// Returns `Err(Error::Unsupported)` for extensions outside the
/// currently-wired set. Used by every side-channel that decides
/// "Scene-aware writer? Vector? Raster?" off the output filename.
pub fn classify_output(path_or_template: &str) -> Result<OutputClass> {
    let ext = ext_of(path_or_template)
        .ok_or_else(|| {
            Error::invalid(format!(
                "convert: output '{path_or_template}' has no extension"
            ))
        })?
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Ok(OutputClass::Scene),
        "svg" => Ok(OutputClass::Vector),
        "png" | "apng" => Ok(OutputClass::Raster(RasterFormat::Png)),
        "jpg" | "jpeg" => Ok(OutputClass::Raster(RasterFormat::Jpeg)),
        "bmp" | "dib" => Ok(OutputClass::Raster(RasterFormat::Bmp)),
        "webp" => Ok(OutputClass::Raster(RasterFormat::Webp)),
        other => Err(Error::unsupported(format!(
            "convert: output extension '.{other}' not yet supported (today png/jpg/bmp/webp/svg/pdf)"
        ))),
    }
}

fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

/// Owned packed RGBA / RGB24 buffer with explicit dimensions. Lives
/// only long enough to hand off to an encoder. The pixel-format side
/// is encoded by `stride / width` (4 = Rgba, 3 = Rgb24).
#[derive(Debug, Clone)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub stride: usize,
}

impl RgbaImage {
    /// True when the buffer is packed Rgb24 (3 bytes per pixel) rather
    /// than Rgba (4 bytes). Inferred off `stride` since the struct
    /// doesn't carry an explicit pixel-format tag.
    pub fn is_rgb(&self) -> bool {
        self.stride == (self.width as usize) * 3
    }
}

/// Encode a single rendered image to disk via the format-specific
/// encoder. Format-side details (palette, YUV conversion, lossless
/// vs lossy choice) live here so the routing in the side-channel
/// runners stays simple.
pub fn encode_raster_to_path(
    img: &RgbaImage,
    fmt: RasterFormat,
    path: &str,
    ops: &[Op],
) -> Result<()> {
    let bytes = match fmt {
        RasterFormat::Png => encode_png(img)?,
        RasterFormat::Bmp => encode_bmp(img)?,
        RasterFormat::Webp => encode_webp(img)?,
        RasterFormat::Jpeg => encode_jpeg(img, ops)?,
    };
    fs::write(Path::new(path), bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to write {path}: {e}")))?;
    Ok(())
}

fn last_quality(ops: &[Op]) -> Option<u8> {
    ops.iter().rev().find_map(|o| match o {
        Op::Quality(q) => Some((*q).min(100) as u8),
        _ => None,
    })
}

/// Apply the `-alpha …` ops in source order to an in-place [`RgbaImage`].
///
/// `bg` is the active `-background` value (defaults to opaque white at
/// the call site); only `AlphaOp::Remove` actually consults it.
pub fn apply_alpha_ops(img: &mut RgbaImage, ops: &[Op], bg: [u8; 4]) {
    for op in ops {
        if let Op::Alpha(a) = op {
            match a {
                AlphaOp::On => {}
                AlphaOp::Off => drop_alpha(img),
                AlphaOp::Remove => flatten_alpha_over(img, bg),
                AlphaOp::Set => set_alpha(img, 255),
                AlphaOp::Transparent => set_alpha(img, 0),
            }
        }
    }
}

fn flatten_alpha_over(img: &mut RgbaImage, bg: [u8; 4]) {
    if img.is_rgb() {
        return;
    }
    let bg_r = bg[0] as u32;
    let bg_g = bg[1] as u32;
    let bg_b = bg[2] as u32;
    for px in img.pixels.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        let inv = 255 - a;
        let r = (px[0] as u32 * a + bg_r * inv + 127) / 255;
        let g = (px[1] as u32 * a + bg_g * inv + 127) / 255;
        let b = (px[2] as u32 * a + bg_b * inv + 127) / 255;
        px[0] = r as u8;
        px[1] = g as u8;
        px[2] = b as u8;
        px[3] = 255;
    }
}

fn drop_alpha(img: &mut RgbaImage) {
    if img.is_rgb() {
        return;
    }
    let w = img.width as usize;
    let h = img.height as usize;
    let mut out = Vec::with_capacity(w * h * 3);
    for px in img.pixels.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    img.pixels = out;
    img.stride = w * 3;
}

fn set_alpha(img: &mut RgbaImage, value: u8) {
    if img.is_rgb() {
        return;
    }
    for px in img.pixels.chunks_exact_mut(4) {
        px[3] = value;
    }
}

fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    use oxideav_png::{encode_png_image, PngImage, PngPixelFormat};
    let pf = if img.is_rgb() {
        PngPixelFormat::Rgb24
    } else {
        PngPixelFormat::Rgba
    };
    let png = PngImage {
        width: img.width,
        height: img.height,
        pixel_format: pf,
        stride: img.stride,
        data: img.pixels.clone(),
        palette: Vec::new(),
    };
    encode_png_image(&png).map_err(|e| Error::invalid(format!("convert: PNG encode failed: {e:?}")))
}

fn encode_bmp(img: &RgbaImage) -> Result<Vec<u8>> {
    use oxideav_bmp::{encode_bmp as bmp_encode, BmpImage, BmpPixelFormat, BmpPlane};
    let pf = if img.is_rgb() {
        BmpPixelFormat::Rgb24
    } else {
        BmpPixelFormat::Rgba
    };
    let bmp = BmpImage {
        width: img.width,
        height: img.height,
        pixel_format: pf,
        planes: vec![BmpPlane {
            stride: img.stride,
            data: img.pixels.clone(),
        }],
        palette: None,
        pts: None,
    };
    bmp_encode(&bmp)
        .map(|(bytes, _format)| bytes)
        .map_err(|e| Error::invalid(format!("convert: BMP encode failed: {e:?}")))
}

fn encode_webp(img: &RgbaImage) -> Result<Vec<u8>> {
    use oxideav_webp::encode_vp8l_argb;
    use oxideav_webp::riff::{build_vp8l_with_alpha, build_webp_file, ImageKind, WebpMetadata};
    // VP8L (lossless) takes ARGB-packed `&[u32]` (one u32 per pixel,
    // alpha in the high byte). Pack accordingly.
    let n_px = (img.width as usize) * (img.height as usize);
    let mut argb = Vec::with_capacity(n_px);
    let has_alpha = !img.is_rgb();
    if has_alpha {
        for px in img.pixels.chunks_exact(4) {
            let v = ((px[3] as u32) << 24)
                | ((px[0] as u32) << 16)
                | ((px[1] as u32) << 8)
                | (px[2] as u32);
            argb.push(v);
        }
    } else {
        for px in img.pixels.chunks_exact(3) {
            let v =
                (0xff_u32 << 24) | ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32);
            argb.push(v);
        }
    }
    let bitstream = encode_vp8l_argb(img.width, img.height, &argb, has_alpha)
        .map_err(|e| Error::invalid(format!("convert: WebP VP8L encode failed: {e:?}")))?;
    let bytes = if has_alpha {
        build_vp8l_with_alpha(&bitstream, img.width, img.height, &WebpMetadata::default())
    } else {
        build_webp_file(
            ImageKind::Vp8lLossless,
            &bitstream,
            img.width,
            img.height,
            None,
            &WebpMetadata::default(),
        )
    };
    Ok(bytes)
}

fn encode_jpeg(img: &RgbaImage, ops: &[Op]) -> Result<Vec<u8>> {
    use oxideav_mjpeg::encoder::encode_jpeg as jpeg_encode;
    let quality = last_quality(ops).unwrap_or(85);

    let src_format = if img.is_rgb() {
        PixelFormat::Rgb24
    } else {
        PixelFormat::Rgba
    };
    let src_frame = VideoFrame {
        pts: None,
        planes: vec![VideoPlane {
            stride: img.stride,
            data: img.pixels.clone(),
        }],
    };
    let info = FrameInfo::new(src_format, img.width, img.height);
    let yuv = pix_convert(
        &src_frame,
        info,
        PixelFormat::Yuv420P,
        &ConvertOptions::default(),
    )
    .map_err(|e| Error::invalid(format!("convert: RGB→YUV420P conversion failed: {e:?}")))?;
    jpeg_encode(&yuv, img.width, img.height, PixelFormat::Yuv420P, quality)
        .map_err(|e| Error::invalid(format!("convert: JPEG encode failed: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_output_covers_known_targets() {
        assert!(matches!(classify_output("out.pdf"), Ok(OutputClass::Scene)));
        assert!(matches!(
            classify_output("out.svg"),
            Ok(OutputClass::Vector)
        ));
        assert!(matches!(
            classify_output("out.png"),
            Ok(OutputClass::Raster(RasterFormat::Png))
        ));
        assert!(matches!(
            classify_output("out.jpg"),
            Ok(OutputClass::Raster(RasterFormat::Jpeg))
        ));
        assert!(matches!(
            classify_output("out.jpeg"),
            Ok(OutputClass::Raster(RasterFormat::Jpeg))
        ));
        assert!(matches!(
            classify_output("out.bmp"),
            Ok(OutputClass::Raster(RasterFormat::Bmp))
        ));
        assert!(matches!(
            classify_output("out.webp"),
            Ok(OutputClass::Raster(RasterFormat::Webp))
        ));
        assert!(classify_output("out.tiff").is_err());
        assert!(classify_output("out.gif").is_err());
        assert!(classify_output("noext").is_err());
    }

    #[test]
    fn drop_alpha_shrinks_buffer_three_quarters() {
        let mut img = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
            ],
            stride: 8,
        };
        drop_alpha(&mut img);
        assert_eq!(img.stride, 6);
        assert_eq!(img.pixels.len(), 12);
        assert_eq!(img.pixels[..3], [10, 20, 30]);
        assert_eq!(img.pixels[3..6], [50, 60, 70]);
    }

    #[test]
    fn flatten_alpha_over_white_preserves_opaque_pixels() {
        let mut img = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 0, 0],
            stride: 8,
        };
        flatten_alpha_over(&mut img, [255, 255, 255, 255]);
        assert_eq!(img.pixels[..4], [255, 0, 0, 255]);
        assert_eq!(img.pixels[4..], [255, 255, 255, 255]);
    }

    #[test]
    fn set_alpha_to_zero_leaves_colour_intact() {
        let mut img = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![10, 20, 30, 100],
            stride: 4,
        };
        set_alpha(&mut img, 0);
        assert_eq!(img.pixels, [10, 20, 30, 0]);
    }
}
