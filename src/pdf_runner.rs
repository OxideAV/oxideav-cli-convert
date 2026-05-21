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

use oxideav_core::{Error, Result, Rgba};
use oxideav_pdf::{read_pdf_to_scene, write_pdf_from_scene};
use oxideav_raster::Renderer;
use oxideav_scene::{Page, Scene};
use oxideav_svg::write_svg;

use crate::op::{ConvertPlan, Op};
use crate::pixel_xform::apply_pixel_transform_chain;
use crate::raster_io::{
    apply_alpha_ops, classify_output, encode_raster_to_path, OutputClass, RgbaImage,
};

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
            if !plan.ops.is_empty()
                && plan
                    .ops
                    .iter()
                    .any(|o| !matches!(o, Op::Density(_) | Op::Define { .. }))
            {
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
    let img = RgbaImage {
        width: width_px,
        height: height_px,
        pixels: plane.data,
        stride: plane.stride,
    };

    // Geometry / negate ops walk between rasterisation and the alpha
    // grammar. IM applies them in source order; we follow suit.
    // Crop bbox-out-of-range surfaces here as Error::invalid so the
    // user sees the IM-style "bbox WxH+X+Y exceeds input W'xH'" line.
    let mut img = apply_pixel_transform_chain(img, ops)
        .map_err(|e| Error::invalid(format!("convert: -crop: {e}")))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{Op, PageSelector, PrintfTemplate as Tmpl};

    #[test]
    fn detects_pdf_extension_case_insensitively() {
        assert!(is_pdf_input("foo.pdf"));
        assert!(is_pdf_input("foo.PDF"));
        assert!(is_pdf_input("/abs/path/x.Pdf"));
        assert!(!is_pdf_input("foo.png"));
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

    // -------- render_page_to_rgba dimension contract --------
    //
    // PDF page sizes are stored in PostScript points (1/72 inch). The
    // rasteriser scales by `density / 72`, so:
    //   - US Letter 612×792 pt at 72 DPI  → 612×792 px
    //   - US Letter 612×792 pt at 300 DPI → 2550×3300 px
    //   - A4 595×842 pt   at 150 DPI      → 1240×1754 px (rounded)
    // These tests pin the math directly so a future refactor of the
    // density / point-conversion expression can't silently drift.

    fn empty_page(width_pt: f32, height_pt: f32) -> Page {
        // An empty vector frame is enough to drive the rasteriser
        // through the dimension-computation path. The Renderer paints
        // the background-coloured canvas, returns it; nothing else to
        // assert beyond width × height.
        Page {
            width: width_pt,
            height: height_pt,
            content: oxideav_core::vector::VectorFrame::new(width_pt, height_pt),
            label: None,
            orientation: 0,
        }
    }

    #[test]
    fn render_page_at_72_dpi_letter_emits_612x792() {
        let page = empty_page(612.0, 792.0);
        let img = render_page_to_rgba(&page, &[]).expect("default density");
        assert_eq!(img.width, 612);
        assert_eq!(img.height, 792);
        assert_eq!(img.stride, 612 * 4);
        assert_eq!(img.pixels.len(), 612 * 792 * 4);
    }

    #[test]
    fn render_page_at_300_dpi_letter_emits_2550x3300() {
        let page = empty_page(612.0, 792.0);
        let ops = vec![Op::Density(300)];
        let img = render_page_to_rgba(&page, &ops).expect("300 dpi");
        assert_eq!(img.width, 2550);
        assert_eq!(img.height, 3300);
        assert_eq!(img.stride, 2550 * 4);
    }

    #[test]
    fn render_page_at_150_dpi_a4_emits_expected_dims() {
        // A4 = 595 × 842 pt; × 150/72 = 1239.58 × 1754.17 → round-half
        // up gives 1240 × 1754.
        let page = empty_page(595.0, 842.0);
        let ops = vec![Op::Density(150)];
        let img = render_page_to_rgba(&page, &ops).expect("a4 150 dpi");
        assert_eq!(img.width, 1240);
        assert_eq!(img.height, 1754);
    }

    #[test]
    fn render_page_last_density_wins() {
        // Two -density ops in sequence: only the last one should be
        // honoured. Mirrors the IM behaviour of operations applied in
        // source order with the last-wins convention for scalar tunables.
        let page = empty_page(612.0, 792.0);
        let ops = vec![Op::Density(72), Op::Density(300)];
        let img = render_page_to_rgba(&page, &ops).expect("density chain");
        assert_eq!((img.width, img.height), (2550, 3300));
    }

    #[test]
    fn render_page_background_colour_paints_canvas() {
        // Background `[64, 128, 192, 255]` should appear on every
        // sample of the rendered empty page. Verifies the renderer
        // received the background field set by `-background`.
        let page = empty_page(8.0, 4.0); // tiny so the assert is cheap
        let ops = vec![Op::Background([64, 128, 192, 255])];
        let img = render_page_to_rgba(&page, &ops).expect("bg paint");
        // 8×4 px at 72 DPI default — confirm bg colour on every pixel.
        for y in 0..img.height as usize {
            for x in 0..img.width as usize {
                let off = y * img.stride + x * 4;
                assert_eq!(
                    img.pixels[off..off + 4],
                    [64, 128, 192, 255],
                    "px @ ({x},{y}) wrong"
                );
            }
        }
    }
}
