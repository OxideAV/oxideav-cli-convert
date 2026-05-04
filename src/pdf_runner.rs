//! PDF-input side-channel for the convert verb.
//!
//! When `convert` sees an input ending in `.pdf` (or whose magic bytes
//! say so), it sniffs the file as a [`Scene`], then routes the work
//! based on what the output side can accept:
//!
//! | Output                         | Scene encoder?  | Fan out?      | Action                                      |
//! |--------------------------------|:---------------:|:-------------:|---------------------------------------------|
//! | `out.pdf` (no `%d`)            | yes             | no            | pass Scene through to `write_pdf_from_scene`|
//! | `page-%03d.png` (printf)       | no              | yes           | render every page, write one file each      |
//! | `out.png` (no `%d`, multi-page)| no              | no            | error — suggest a printf template           |
//! | `out.png` (no `%d`, 1 page)    | no              | no            | render page 0, write it                     |
//! | `out.pdf` (with `%d`)          | yes             | yes           | error — Scene-aware output + printf is bogus|
//!
//! Architecturally this stays out of `oxideav-pipeline`: we never
//! wire PDF as a `Demuxer` because pages don't fit the `Frame::Video`
//! shape. The convert verb is the only consumer that needs Scene
//! input today, so a side-channel here is cheaper than a core change.

use std::fs;
use std::path::Path;

use oxideav_core::{Error, Result, Rgba};
use oxideav_pdf::{read_pdf_to_scene, write_pdf_from_scene};
use oxideav_png::{encode_png_image, PngImage, PngPixelFormat};
use oxideav_raster::Renderer;
use oxideav_scene::Page;

use crate::op::{AlphaOp, ConvertPlan, Op};

/// File extensions whose encoders consume an entire `Scene` rather
/// than a single rasterised frame. Update as more Scene-aware
/// encoders land (oxideav-svg already accepts a single VectorFrame
/// — multi-page SVG would extend this list).
const SCENE_AWARE_OUTPUTS: &[&str] = &["pdf"];

fn output_format_accepts_scene(path_or_template: &str) -> bool {
    let lit = match ext_of(path_or_template) {
        Some(s) => s.to_ascii_lowercase(),
        None => return false,
    };
    SCENE_AWARE_OUTPUTS.iter().any(|&e| e == lit)
}

fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

/// Returns true when the input path's extension is `.pdf` or its
/// first 5 bytes are `%PDF-`. Cheap enough to call unconditionally.
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

    let pages = scene
        .pages
        .as_ref()
        .ok_or_else(|| Error::invalid("convert: PDF has no pages — refusing to write empty output"))?;
    if pages.is_empty() {
        return Err(Error::invalid(
            "convert: PDF has no pages — refusing to write empty output",
        ));
    }

    let scene_aware = output_format_accepts_scene(&plan.output);
    let has_template = plan.output_template.is_some();

    match (scene_aware, has_template) {
        (true, true) => Err(Error::invalid(format!(
            "convert: output '{}' is Scene-aware (.pdf/.svg) but the filename has a `%d` template; remove the template OR pick a per-frame output format like .png",
            plan.output
        ))),
        (true, false) => {
            // PDF → PDF (or future PDF → SVG with a Scene-aware writer).
            // Pass the Scene through verbatim. The `-resize` / `-blur` /
            // etc. ops are silently dropped on this path — they're raster
            // operations and don't apply to a Scene. Document the drop.
            if !plan.ops.is_empty() {
                eprintln!("convert: note: raster ops ignored on Scene-aware output");
            }
            let out = write_pdf_from_scene(&scene)
                .map_err(|e| Error::invalid(format!("convert: failed to write PDF: {e:?}")))?;
            fs::write(&plan.output, out)
                .map_err(|e| Error::invalid(format!("convert: failed to write {}: {e}", plan.output)))?;
            Ok(())
        }
        (false, false) if pages.len() > 1 => Err(Error::invalid(format!(
            "convert: PDF has {} pages but output '{}' is a single file with no `%d` template; use e.g. `page-%03d.png`",
            pages.len(),
            plan.output
        ))),
        (false, false) => {
            // Single page → single output file.
            let frame = render_page_to_rgba(&pages[0], &plan.ops)?;
            encode_to_path(&frame, &plan.output, &plan.ops)?;
            Ok(())
        }
        (false, true) => {
            // Multi-page → fan out via the printf template.
            let tmpl = plan.output_template.as_ref().expect("checked above");
            for (i, page) in pages.iter().enumerate() {
                let frame = render_page_to_rgba(page, &plan.ops)?;
                let path = tmpl.expand(i);
                encode_to_path(&frame, &path, &plan.ops)?;
            }
            Ok(())
        }
    }
}

/// Rasterise a single PDF page using `-density` + `-background` + the
/// `-alpha` chain from `ops`. Returns a packed RGBA `VideoFrame` whose
/// stride is `width * 4`.
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

    // Convert the renderer's VideoFrame (one Rgba plane, stride =
    // width * 4) into a tightly-packed buffer we own. The renderer
    // always emits straight-alpha RGBA at exactly one plane.
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

    // Apply the alpha-handling chain in source order so the user's
    // intent (`-alpha remove` then `-alpha off`) composes predictably.
    apply_alpha_ops(&mut img, ops, bg_arr);

    Ok(img)
}

/// Owned packed RGBA buffer with explicit dimensions. Lives only
/// long enough to hand off to an encoder.
struct RgbaImage {
    width: u32,
    height: u32,
    /// `stride * height` bytes. Renderer guarantees straight alpha.
    pixels: Vec<u8>,
    stride: usize,
    // Track whether the buffer is still RGBA (true) or has been
    // collapsed to RGB24 by `-alpha off`.
}

impl RgbaImage {
    fn pixel_format_for_encode(&self) -> PngPixelFormat {
        // The buffer is always straight-alpha RGBA at this point;
        // `apply_alpha_ops` mutates `pixels` in place and may shrink
        // it via `-alpha off`. We track which side we're on by
        // looking at `stride / width` — 4 = RGBA, 3 = RGB24.
        if self.stride == (self.width as usize) * 3 {
            PngPixelFormat::Rgb24
        } else {
            PngPixelFormat::Rgba
        }
    }
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

fn apply_alpha_ops(img: &mut RgbaImage, ops: &[Op], bg: [u8; 4]) {
    for op in ops {
        if let Op::Alpha(a) = op {
            match a {
                AlphaOp::On => { /* RGBA stays RGBA — no-op */ }
                AlphaOp::Off => drop_alpha(img),
                AlphaOp::Remove => flatten_alpha_over(img, bg),
                AlphaOp::Set => set_alpha(img, 255),
                AlphaOp::Transparent => set_alpha(img, 0),
            }
        }
    }
}

/// Composite each pixel over `bg` (straight alpha) and force the
/// output alpha to 255. After this the image is still RGBA but every
/// pixel is fully opaque.
fn flatten_alpha_over(img: &mut RgbaImage, bg: [u8; 4]) {
    if img.stride != (img.width as usize) * 4 {
        return; // already collapsed to RGB24
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

/// Drop the alpha channel: RGBA → RGB24 in place.
fn drop_alpha(img: &mut RgbaImage) {
    if img.stride != (img.width as usize) * 4 {
        return; // already RGB24
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
    if img.stride != (img.width as usize) * 4 {
        return;
    }
    for px in img.pixels.chunks_exact_mut(4) {
        px[3] = value;
    }
}

fn encode_to_path(img: &RgbaImage, path: &str, ops: &[Op]) -> Result<()> {
    // For now the only fan-out target we know how to encode is PNG.
    // Other formats (JPG, WebP, etc.) on the fan-out path require
    // routing through their own encoders — round-2 followup.
    let ext = ext_of(path)
        .ok_or_else(|| Error::invalid(format!("convert: output '{path}' has no extension")))?
        .to_ascii_lowercase();
    let _ = ops; // future: -quality forwarded per format
    match ext.as_str() {
        "png" => {
            let png = PngImage {
                width: img.width,
                height: img.height,
                pixel_format: img.pixel_format_for_encode(),
                stride: img.stride,
                data: img.pixels.clone(),
                palette: Vec::new(),
            };
            let bytes = encode_png_image(&png)
                .map_err(|e| Error::invalid(format!("convert: PNG encode failed: {e:?}")))?;
            fs::write(Path::new(path), bytes)
                .map_err(|e| Error::invalid(format!("convert: failed to write {path}: {e}")))?;
            Ok(())
        }
        other => Err(Error::unsupported(format!(
            "convert: PDF→{other} fan-out not yet supported (only PNG today; JPG/WebP/etc. is a follow-up)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::PrintfTemplate as Tmpl;

    #[test]
    fn detects_pdf_extension_case_insensitively() {
        assert!(is_pdf_input("foo.pdf"));
        assert!(is_pdf_input("foo.PDF"));
        assert!(is_pdf_input("/abs/path/x.Pdf"));
        assert!(!is_pdf_input("foo.png"));
    }

    #[test]
    fn scene_aware_table_recognises_pdf() {
        assert!(output_format_accepts_scene("out.pdf"));
        assert!(output_format_accepts_scene("page-%03d.pdf"));
        assert!(!output_format_accepts_scene("out.png"));
        assert!(!output_format_accepts_scene("out.jpg"));
        assert!(!output_format_accepts_scene("noext"));
    }

    #[test]
    fn drop_alpha_shrinks_buffer_three_quarters() {
        let mut img = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                10, 20, 30, 40,
                50, 60, 70, 80,
                90, 100, 110, 120,
                130, 140, 150, 160,
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
            pixels: vec![
                255, 0, 0, 255,   // opaque red — unchanged
                0, 0, 0, 0,       // fully transparent — becomes white
            ],
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
}
