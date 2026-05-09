//! PDF-input side-channel for the convert verb.
//!
//! When `convert` sees an input ending in `.pdf` (or whose magic bytes
//! say so), it sniffs the file as a [`Scene`], applies any `[N]`/
//! `[N-M]` page selector, then routes the work based on what the
//! output format can accept:
//!
//! ## Output classes
//!
//! | Class      | Examples       | Behaviour                                   |
//! |------------|----------------|---------------------------------------------|
//! | Scene      | `.pdf`         | writer consumes the whole `Scene`           |
//! | Vector     | `.svg`         | one VectorFrame per page, no rasterisation  |
//! | Raster     | `.png` `.jpg` `.bmp` `.webp` | render each page, encode |
//!
//! ## Routing
//!
//! - `Scene → Scene` (no `%d`): pass selected-pages Scene through.
//! - `Scene → Vector` (single page selected, no `%d`): write one
//!   document file containing that page's VectorFrame.
//! - `Scene → Vector` (multi-page, `%d`): one Vector file per page.
//! - `Scene → Vector` (multi-page, no `%d`): error — suggest `%d`.
//! - `Scene → Raster` (single page selected, no `%d`): render + encode.
//! - `Scene → Raster` (multi-page, `%d`): render + encode per page.
//! - `Scene → Raster` (multi-page, no `%d`): error — suggest `%d`.
//! - `Scene → Scene` (with `%d`): error — Scene + printf is bogus.
//!
//! Architecturally this stays out of `oxideav-pipeline`: we never
//! wire PDF as a `Demuxer` because pages don't fit the `Frame::Video`
//! shape, and the routing rule is convert-specific.

use std::fs;
use std::path::Path;

use oxideav_core::{Error, PixelFormat, Result, Rgba, VideoFrame, VideoPlane};
use oxideav_pdf::{read_pdf_to_scene, write_pdf_from_scene};
use oxideav_pixfmt::{convert as pix_convert, ConvertOptions, FrameInfo};
use oxideav_raster::Renderer;
use oxideav_scene::{Page, Scene};
use oxideav_svg::write_svg;

use crate::op::{AlphaOp, ConvertPlan, Op};

/// What kind of output the convert verb is producing — drives the
/// routing decision in [`run`]. Add new arms when new formats are
/// wired (e.g. `.tiff` → Raster once oxideav-tiff has an encoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputClass {
    /// Format consumes a whole [`Scene`] (`.pdf` today; multi-page
    /// SVG would land here if we ever support it).
    Scene,
    /// Format takes a single [`oxideav_core::vector::VectorFrame`]
    /// per file, no rasterisation (`.svg`).
    Vector,
    /// Format takes a rasterised RGBA / RGB buffer per file
    /// (`.png` `.jpg` `.bmp` `.webp`, …).
    Raster(RasterFormat),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RasterFormat {
    Png,
    Jpeg,
    Bmp,
    Webp,
}

fn classify_output(path_or_template: &str) -> Result<OutputClass> {
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
            "convert: PDF→{other} not yet supported (round-2 covers png/jpg/bmp/webp/svg/pdf)"
        ))),
    }
}

fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

/// Returns true when the input path's extension is `.pdf` or its
/// first 5 bytes are `%PDF-`.
pub fn is_pdf_input(input: &str) -> bool {
    if input.to_ascii_lowercase().ends_with(".pdf") {
        return true;
    }
    if let Ok(mut f) = fs::File::open(input) {
        use std::io::Read;
        let mut head = [0u8; 5];
        if f.read_exact(&mut head).is_ok() && &head == b"%PDF-" {
            return true;
        }
    }
    false
}

/// Run the PDF-input convert flow. Side-effect-only: writes one or
/// more files to disk.
pub fn run(plan: &ConvertPlan) -> Result<()> {
    let bytes = fs::read(&plan.input)
        .map_err(|e| Error::invalid(format!("convert: failed to read {}: {e}", plan.input)))?;
    let scene = read_pdf_to_scene(&bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to parse PDF: {e:?}")))?;

    let all_pages = scene.pages.as_ref().ok_or_else(|| {
        Error::invalid("convert: PDF has no pages — refusing to write empty output")
    })?;
    if all_pages.is_empty() {
        return Err(Error::invalid(
            "convert: PDF has no pages — refusing to write empty output",
        ));
    }

    // Resolve the input page selector against the actual page count.
    let indices: Vec<usize> = match &plan.input_pages {
        Some(sel) => sel
            .resolve(all_pages.len())
            .map_err(|e| Error::invalid(format!("convert: input '{}': {e}", plan.input)))?,
        None => (0..all_pages.len()).collect(),
    };
    let selected: Vec<&Page> = indices.iter().map(|i| &all_pages[*i]).collect();

    let class = classify_output(&plan.output)?;
    let has_template = plan.output_template.is_some();

    match (class, has_template, selected.len()) {
        // Scene → Scene with printf is bogus.
        (OutputClass::Scene, true, _) => Err(Error::invalid(format!(
            "convert: output '{}' is Scene-aware (.pdf) but the filename has a `%d` template; remove the template OR pick a per-frame output format",
            plan.output
        ))),

        // Scene → Scene: pass selected pages through.
        (OutputClass::Scene, false, _) => {
            if !plan.ops.is_empty() && plan.ops.iter().any(|o| !matches!(o, Op::Density(_))) {
                eprintln!("convert: note: raster ops ignored on Scene-aware output");
            }
            let out_scene = Scene {
                pages: Some(selected.into_iter().cloned().collect()),
                ..Scene::default()
            };
            let bytes = write_pdf_from_scene(&out_scene)
                .map_err(|e| Error::invalid(format!("convert: failed to write PDF: {e:?}")))?;
            fs::write(&plan.output, bytes).map_err(|e| {
                Error::invalid(format!("convert: failed to write {}: {e}", plan.output))
            })?;
            Ok(())
        }

        // Vector → with printf template (per-page SVG fan-out).
        (OutputClass::Vector, true, _) => {
            let tmpl = plan.output_template.as_ref().expect("checked above");
            for (out_idx, page) in selected.iter().enumerate() {
                let bytes = write_svg(&page.content);
                let path = tmpl.expand(out_idx);
                fs::write(&path, bytes).map_err(|e| {
                    Error::invalid(format!("convert: failed to write {path}: {e}"))
                })?;
            }
            Ok(())
        }
        // Vector → single literal file. Must have exactly one selected
        // page; multi-page SVG isn't a thing.
        (OutputClass::Vector, false, _) => {
            if selected.len() == 1 {
                let bytes = write_svg(&selected[0].content);
                fs::write(&plan.output, bytes).map_err(|e| {
                    Error::invalid(format!("convert: failed to write {}: {e}", plan.output))
                })?;
                Ok(())
            } else {
                Err(Error::invalid(format!(
                    "convert: {} selected pages but vector output '{}' has no `%d` template (SVG is one-page-per-file); use a `%d` template OR a `[N]` selector to pick one page",
                    selected.len(),
                    plan.output
                )))
            }
        }

        // Raster → with printf template (per-page raster fan-out).
        (OutputClass::Raster(fmt), true, _) => {
            let tmpl = plan.output_template.as_ref().expect("checked above");
            for (out_idx, page) in selected.iter().enumerate() {
                let img = render_page_to_rgba(page, &plan.ops)?;
                let path = tmpl.expand(out_idx);
                encode_raster_to_path(&img, fmt, &path, &plan.ops)?;
            }
            Ok(())
        }
        // Raster → single literal file. Single page only.
        (OutputClass::Raster(fmt), false, _) => {
            if selected.len() == 1 {
                let img = render_page_to_rgba(selected[0], &plan.ops)?;
                encode_raster_to_path(&img, fmt, &plan.output, &plan.ops)
            } else {
                Err(Error::invalid(format!(
                    "convert: {} selected pages but output '{}' is a single file with no `%d` template; use e.g. `page-%03d.png` OR a `[N]` selector to pick one page",
                    selected.len(),
                    plan.output
                )))
            }
        }
    }
}

/// Owned packed RGBA / RGB24 buffer with explicit dimensions. Lives
/// only long enough to hand off to an encoder. The pixel-format side
/// is encoded by `stride / width` (4 = Rgba, 3 = Rgb24).
pub(crate) struct RgbaImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
    pub(crate) stride: usize,
}

impl RgbaImage {
    fn is_rgb(&self) -> bool {
        self.stride == (self.width as usize) * 3
    }
}

fn render_page_to_rgba(page: &Page, ops: &[Op]) -> Result<RgbaImage> {
    let density = last_density(ops).unwrap_or(72);
    let bg_arr = last_background(ops).unwrap_or([255, 255, 255, 255]);
    let bg = Rgba::new(bg_arr[0], bg_arr[1], bg_arr[2], bg_arr[3]);

    // PDF page dimensions are in PostScript points (1/72 inch).
    let width_px = ((page.width * density as f32) / 72.0).round().max(1.0) as u32;
    let height_px = ((page.height * density as f32) / 72.0).round().max(1.0) as u32;

    let mut renderer = Renderer::new(width_px, height_px);
    renderer.background = bg;
    let frame = renderer.render(&page.content);

    let plane = frame
        .planes
        .into_iter()
        .next()
        .ok_or_else(|| Error::invalid("convert: raster renderer returned no planes"))?;
    let mut img = RgbaImage {
        width: width_px,
        height: height_px,
        pixels: plane.data,
        stride: plane.stride,
    };

    apply_alpha_ops(&mut img, ops, bg_arr);
    Ok(img)
}

fn last_density(ops: &[Op]) -> Option<u32> {
    ops.iter().rev().find_map(|o| match o {
        Op::Density(d) => Some(*d),
        _ => None,
    })
}

fn last_background(ops: &[Op]) -> Option<[u8; 4]> {
    ops.iter().rev().find_map(|o| match o {
        Op::Background(c) => Some(*c),
        _ => None,
    })
}

fn last_quality(ops: &[Op]) -> Option<u8> {
    ops.iter().rev().find_map(|o| match o {
        Op::Quality(q) => Some((*q).min(100) as u8),
        _ => None,
    })
}

fn apply_alpha_ops(img: &mut RgbaImage, ops: &[Op], bg: [u8; 4]) {
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

/// Encode a single rendered page to disk via the format-specific
/// encoder. Format-side details (palette, YUV conversion, lossless
/// vs lossy choice) live here so the routing in `run` stays simple.
fn encode_raster_to_path(img: &RgbaImage, fmt: RasterFormat, path: &str, ops: &[Op]) -> Result<()> {
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
        pts: None,
    };
    bmp_encode(&bmp).map_err(|e| Error::invalid(format!("convert: BMP encode failed: {e:?}")))
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
    // Wrap the raw VP8L bitstream in a RIFF/WEBP container so the
    // output file is a real `.webp` players accept.
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

    // The MJPEG encoder takes a YUV420P planar VideoFrame. Walk the
    // RGBA / RGB24 buffer through pixfmt's RGB→YUV420P (the convert
    // table has both RGBA→YUV420P and Rgb24→YUV420P entries).
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
    use crate::op::{PageSelector, PrintfTemplate as Tmpl};

    #[test]
    fn detects_pdf_extension_case_insensitively() {
        assert!(is_pdf_input("foo.pdf"));
        assert!(is_pdf_input("foo.PDF"));
        assert!(is_pdf_input("/abs/path/x.Pdf"));
        assert!(!is_pdf_input("foo.png"));
    }

    #[test]
    fn classify_output_covers_round2_targets() {
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

    #[test]
    fn printf_template_expands_with_zero_padding() {
        let t = Tmpl {
            prefix: "page-".into(),
            width: 3,
            suffix: ".png".into(),
        };
        assert_eq!(t.expand(0), "page-000.png");
        assert_eq!(t.expand(7), "page-007.png");
        assert_eq!(t.expand(123), "page-123.png");
    }

    #[test]
    fn printf_template_no_width_unpadded() {
        let t = Tmpl {
            prefix: "out".into(),
            width: 0,
            suffix: ".jpg".into(),
        };
        assert_eq!(t.expand(0), "out0.jpg");
        assert_eq!(t.expand(42), "out42.jpg");
    }

    #[test]
    fn page_selector_single_resolves() {
        let s = PageSelector::Single(2);
        assert_eq!(s.resolve(5).unwrap(), vec![2]);
        assert!(s.resolve(2).is_err()); // out of range
    }

    #[test]
    fn page_selector_range_resolves_inclusive() {
        let s = PageSelector::Range(1, 3);
        assert_eq!(s.resolve(5).unwrap(), vec![1, 2, 3]);
        assert!(s.resolve(3).is_err()); // 3 is out of range when total=3
        let inverted = PageSelector::Range(3, 1);
        assert!(inverted.resolve(5).is_err());
    }
}
