//! End-to-end: generate a 2-page PDF via `oxideav-pdf`'s writer,
//! then run `convert` with a `%03d` template and assert two PNGs land
//! on disk with the expected dimensions.

use std::fs;
use std::path::PathBuf;

use oxideav_core::vector::{
    Group, Node, Paint, Path as VPath, PathCommand, PathNode, Point, Rgba, VectorFrame,
};
use oxideav_pdf::write_pdf_from_scene;
use oxideav_scene::{Page, Scene};

fn temp_dir(name: &str) -> PathBuf {
    // Per-PID + nanosecond suffix keeps each test run in its own
    // directory so re-runs / parallel runs don't trample each other.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-test-{name}-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

fn ctx() -> oxideav_core::RuntimeContext {
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_source::register(&mut ctx);
    ctx
}

fn make_two_page_pdf() -> Vec<u8> {
    // A 2-page Scene: page 0 has a red rectangle, page 1 has a blue
    // rectangle. The actual painting is irrelevant for the test —
    // what matters is that the PDF reader produces a Scene with
    // exactly 2 pages and that the convert verb writes 2 PNGs.
    let make_page = |paint_color: Rgba| {
        let path = VPath {
            commands: vec![
                PathCommand::MoveTo(Point::new(10.0, 10.0)),
                PathCommand::LineTo(Point::new(100.0, 10.0)),
                PathCommand::LineTo(Point::new(100.0, 80.0)),
                PathCommand::LineTo(Point::new(10.0, 80.0)),
                PathCommand::Close,
            ],
        };
        let node = PathNode {
            path,
            fill: Some(Paint::Solid(paint_color)),
            stroke: None,
            fill_rule: oxideav_core::vector::FillRule::NonZero,
        };
        let group = Group {
            children: vec![Node::Path(node)],
            ..Group::default()
        };
        let mut frame = VectorFrame::new(200.0, 100.0);
        frame.root = group;
        Page {
            width: 200.0,
            height: 100.0,
            content: frame,
            label: None,
            orientation: 0,
        }
    };

    let scene = Scene {
        pages: Some(vec![
            make_page(Rgba::new(255, 0, 0, 255)),
            make_page(Rgba::new(0, 0, 255, 255)),
        ]),
        ..Scene::default()
    };
    write_pdf_from_scene(&scene).expect("pdf encode")
}

#[test]
fn pdf_two_pages_fans_out_to_two_pngs_with_printf_template() {
    let dir = temp_dir("two-pages-printf");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let template = dir.join("page-%02d.png");
    oxideav_cli_convert::run(
        &[
            pdf_path.to_string_lossy().into_owned(),
            "-density".into(),
            "150".into(),
            "-background".into(),
            "white".into(),
            "-alpha".into(),
            "remove".into(),
            "-alpha".into(),
            "off".into(),
            template.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert run");

    let out0 = dir.join("page-00.png");
    let out1 = dir.join("page-01.png");
    assert!(out0.exists(), "page-00.png missing");
    assert!(out1.exists(), "page-01.png missing");

    // Sanity: both files are non-empty PNGs (signature 89 50 4e 47).
    for p in &[&out0, &out1] {
        let bytes = fs::read(p).unwrap();
        assert!(
            bytes.len() > 8,
            "PNG {p:?} too small ({} bytes)",
            bytes.len()
        );
        assert_eq!(&bytes[..4], b"\x89PNG", "PNG {p:?} signature wrong");
    }
}

#[test]
fn pdf_multi_page_no_template_errors_with_helpful_message() {
    let dir = temp_dir("multi-page-no-template");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let out = dir.join("out.png");
    let err = oxideav_cli_convert::run(
        &[
            pdf_path.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("%d"),
        "expected hint about printf template, got: {msg}"
    );
}

#[test]
fn pdf_to_pdf_with_template_is_rejected() {
    let dir = temp_dir("pdf-to-pdf-template");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let bogus = dir.join("page-%02d.pdf");
    let err = oxideav_cli_convert::run(
        &[
            pdf_path.to_string_lossy().into_owned(),
            bogus.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Scene-aware") && msg.contains("template"),
        "expected scene-aware-with-template hint, got: {msg}"
    );
}

#[test]
fn pdf_to_pdf_passes_scene_through() {
    let dir = temp_dir("pdf-to-pdf-passthrough");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let out = dir.join("out.pdf");
    oxideav_cli_convert::run(
        &[
            pdf_path.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf→pdf");

    let bytes = fs::read(&out).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "output is not a PDF");
}

// -------- Round 2 -------- //

#[test]
fn pdf_page_selector_single_renders_one_file() {
    let dir = temp_dir("selector-single");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    // [1] selects page 1 only — single output (no %d) should work.
    let out = dir.join("page1.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[1]", pdf_path.to_string_lossy()),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert with [1] selector");
    assert!(out.exists(), "page1.png missing");
    assert_eq!(&fs::read(&out).unwrap()[..4], b"\x89PNG");
}

#[test]
fn pdf_page_selector_out_of_range_errors() {
    let dir = temp_dir("selector-oor");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    let err = oxideav_cli_convert::run(
        &[
            format!("{}[5]", pdf_path.to_string_lossy()),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    assert!(format!("{err:?}").contains("out of range"));
}

#[test]
fn pdf_to_svg_single_page_writes_svg() {
    let dir = temp_dir("svg-single");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("page0.svg");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf[0]→svg");
    let bytes = fs::read(&out).unwrap();
    assert!(
        bytes.starts_with(b"<?xml") || bytes.starts_with(b"<svg"),
        "output is not an SVG (first 16 bytes: {:?})",
        &bytes[..bytes.len().min(16)]
    );
}

#[test]
fn pdf_to_svg_multi_page_via_template() {
    let dir = temp_dir("svg-multi");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let template = dir.join("page-%d.svg");
    oxideav_cli_convert::run(
        &[
            pdf_path.to_string_lossy().into_owned(),
            template.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf→svg fan-out");
    assert!(dir.join("page-0.svg").exists());
    assert!(dir.join("page-1.svg").exists());
}

#[test]
fn pdf_to_svg_multi_page_no_template_errors() {
    let dir = temp_dir("svg-multi-no-tmpl");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.svg");
    let err = oxideav_cli_convert::run(
        &[
            pdf_path.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    assert!(
        format!("{err:?}").contains("one-page-per-file")
            || format!("{err:?}").contains("`%d` template")
    );
}

#[test]
fn pdf_to_pdf_with_selector_keeps_only_selected_pages() {
    let dir = temp_dir("pdf-pdf-selector");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.pdf");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf[0]→pdf");
    let bytes = fs::read(&out).unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    // Round-trip the result and assert it has exactly 1 page.
    let scene = oxideav_pdf::read_pdf_to_scene(&bytes).unwrap();
    assert_eq!(scene.pages.unwrap().len(), 1);
}

#[test]
fn pdf_to_jpg_single_page() {
    let dir = temp_dir("jpg");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("page0.jpg");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-background".into(),
            "white".into(),
            "-alpha".into(),
            "remove".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf[0]→jpg");
    let bytes = fs::read(&out).unwrap();
    // JPEG SOI marker.
    assert_eq!(&bytes[..2], &[0xff, 0xd8]);
}

#[test]
fn pdf_to_bmp_single_page() {
    let dir = temp_dir("bmp");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("page0.bmp");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf[0]→bmp");
    let bytes = fs::read(&out).unwrap();
    assert_eq!(&bytes[..2], b"BM"); // Windows BMP signature
}

#[test]
fn pdf_to_webp_single_page() {
    let dir = temp_dir("webp");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("page0.webp");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf[0]→webp");
    let bytes = fs::read(&out).unwrap();
    // RIFF container with WEBP fourcc at offset 8.
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
}

#[test]
fn pdf_range_selector_fans_out_to_two_pngs() {
    let dir = temp_dir("range-selector");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let template = dir.join("p-%d.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0-1]", pdf_path.to_string_lossy()),
            template.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert pdf[0-1]→png fan-out");
    assert!(dir.join("p-0.png").exists());
    assert!(dir.join("p-1.png").exists());
}

// -------- Round-3 inline geometry / negate ops -------- //
//
// Each of these runs the full PDF→PNG path with a new op and checks
// that the output PNG's IHDR width/height matches what the op should
// produce. PNG IHDR sits at byte 16 (after the 8-byte signature +
// 4-byte chunk-length + 4-byte chunk-type "IHDR"); width is bytes
// 16..20, height 20..24, big-endian.

fn png_dims(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..4], b"\x89PNG", "not a PNG");
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w, h)
}

#[test]
fn rotate_90_swaps_output_dimensions() {
    let dir = temp_dir("rotate-90");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    // First render plain, capture dims.
    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");
    let (pw, ph) = png_dims(&fs::read(&plain).unwrap());

    // Now render with -rotate 90; dims should swap.
    let rotated = dir.join("rotated.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-rotate".into(),
            "90".into(),
            rotated.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("rotate render");
    let (rw, rh) = png_dims(&fs::read(&rotated).unwrap());
    assert_eq!((rw, rh), (ph, pw), "rotate 90 should swap dims");
}

#[test]
fn rotate_180_preserves_output_dimensions() {
    let dir = temp_dir("rotate-180");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");
    let (pw, ph) = png_dims(&fs::read(&plain).unwrap());

    let rotated = dir.join("rotated.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-rotate".into(),
            "180".into(),
            rotated.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("rotate 180 render");
    let (rw, rh) = png_dims(&fs::read(&rotated).unwrap());
    assert_eq!((rw, rh), (pw, ph), "rotate 180 should preserve dims");
}

#[test]
fn flip_and_flop_preserve_output_dimensions() {
    let dir = temp_dir("flip-flop");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");
    let plain_bytes = fs::read(&plain).unwrap();
    let (pw, ph) = png_dims(&plain_bytes);

    let flipped = dir.join("flipped.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-flip".into(),
            "-flop".into(),
            flipped.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("flip+flop render");
    let flipped_bytes = fs::read(&flipped).unwrap();
    let (fw, fh) = png_dims(&flipped_bytes);
    assert_eq!((fw, fh), (pw, ph), "-flip -flop should preserve dimensions");
}

#[test]
fn crop_emits_smaller_output() {
    let dir = temp_dir("crop");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    // Plain render to capture full dims.
    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");
    let (pw, ph) = png_dims(&fs::read(&plain).unwrap());
    assert!(pw >= 50 && ph >= 30, "test fixture too small for crop");

    let cropped = dir.join("cropped.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-crop".into(),
            "50x30+0+0".into(),
            cropped.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("crop render");
    let (cw, ch) = png_dims(&fs::read(&cropped).unwrap());
    assert_eq!((cw, ch), (50, 30));
}

#[test]
fn crop_out_of_bounds_errors_cleanly() {
    let dir = temp_dir("crop-oob");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    let err = oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-crop".into(),
            "10000x10000+0+0".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("exceeds input"),
        "expected exceeds-input message, got: {msg}"
    );
}

#[test]
fn negate_runs_through_full_pipeline() {
    // Just check it returns Ok and the file is a valid PNG with the
    // unchanged dimensions of the plain render. Pixel-level negate
    // correctness is covered by the unit tests in pixel_xform.
    let dir = temp_dir("negate");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");
    let plain_bytes = fs::read(&plain).unwrap();

    let negated = dir.join("negated.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-negate".into(),
            negated.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("negate render");
    let negated_bytes = fs::read(&negated).unwrap();

    assert_eq!(png_dims(&plain_bytes), png_dims(&negated_bytes));
    // The two encoded PNGs must NOT byte-match — negate would produce
    // different pixel data, so the deflate-compressed IDAT chunks
    // differ.
    assert_ne!(
        plain_bytes, negated_bytes,
        "-negate produced byte-identical output (suggests it didn't run)"
    );
}

#[test]
fn sharpen_through_pdf_side_channel_matches_image_filter_standalone() {
    // End-to-end: render a PDF page through `convert` with `-sharpen`,
    // then run the same image-filter `Sharpen` directly on the
    // un-sharpened render and assert the byte-for-byte output matches.
    // Locks down the "PDF side-channel honours tonal ops via image-filter"
    // contract from round-after-next: the sharpened PDF render must be
    // pixel-identical to what the standalone Sharpen filter produces on
    // the plain render.
    use oxideav_core::{PixelFormat, VideoFrame, VideoPlane};
    use oxideav_image_filter::{ImageFilter, Sharpen, VideoStreamParams};
    use oxideav_png::decode_png;

    let dir = temp_dir("sharpen-side-channel");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    // Plain render — gives us the un-sharpened reference RGBA buffer.
    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");

    // Side-channel render with `-sharpen 1x0.5`.
    let sharpened = dir.join("sharpened.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-sharpen".into(),
            "1x0.5".into(),
            sharpened.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("sharpen render");

    // Decode both PNGs and run Sharpen standalone on the plain pixels.
    let plain_png = decode_png(&fs::read(&plain).unwrap()).expect("decode plain PNG");
    let sharp_png = decode_png(&fs::read(&sharpened).unwrap()).expect("decode sharpened PNG");
    assert_eq!(plain_png.width, sharp_png.width);
    assert_eq!(plain_png.height, sharp_png.height);

    // PNG decode emits Rgba (4 bpp) for our test fixture's alpha-bearing
    // render; if it happened to land on Rgb24 the stride-vs-width math
    // would still hold but the filter would need a different format tag.
    let bpp = plain_png.stride / plain_png.width as usize;
    let format = match bpp {
        4 => PixelFormat::Rgba,
        3 => PixelFormat::Rgb24,
        other => panic!("unexpected bpp {other} from decoded PNG"),
    };
    let plain_frame = VideoFrame {
        pts: None,
        planes: vec![VideoPlane {
            stride: plain_png.stride,
            data: plain_png.data.clone(),
        }],
    };
    let reference = Sharpen::new(1, 0.5)
        .apply(
            &plain_frame,
            VideoStreamParams {
                format,
                width: plain_png.width,
                height: plain_png.height,
            },
        )
        .expect("standalone Sharpen apply");
    assert_eq!(
        sharp_png.data, reference.planes[0].data,
        "PDF side-channel sharpen output must match standalone Sharpen byte-for-byte"
    );
}

#[test]
fn rotate_90_then_270_round_trips_dims() {
    // Chained ops in source order: 90° then 270° should be the
    // identity. Spot-check the dims survive.
    let dir = temp_dir("rotate-roundtrip");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();

    let plain = dir.join("plain.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            plain.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("plain render");
    let (pw, ph) = png_dims(&fs::read(&plain).unwrap());

    let rt = dir.join("rt.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-rotate".into(),
            "90".into(),
            "-rotate".into(),
            "270".into(),
            rt.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("90+270 render");
    let (rw, rh) = png_dims(&fs::read(&rt).unwrap());
    assert_eq!((rw, rh), (pw, ph));
}

// -------- Round-next: -resize geometry modes, -thumbnail, -define -------- //

/// `-resize WxH!` forces exact dims regardless of source aspect ratio.
/// Source is 200×100 pt at 72 DPI = 200×100 px; force-resize to 50×50.
#[test]
fn resize_force_bang_lands_on_exact_dims() {
    let dir = temp_dir("resize-force");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-resize".into(),
            "50x50!".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("resize force render");
    let (w, h) = png_dims(&fs::read(&out).unwrap());
    assert_eq!((w, h), (50, 50), "force mode must give exact 50x50");
}

/// `-resize WxH` (default mode) preserves aspect ratio, fitting inside
/// the box. Source 200×100 → request 100×100 → aspect-fit gives 100×50
/// (width is the limit).
#[test]
fn resize_default_fits_inside_preserving_aspect() {
    let dir = temp_dir("resize-default");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-resize".into(),
            "100x100".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("resize default render");
    let (w, h) = png_dims(&fs::read(&out).unwrap());
    assert_eq!(
        (w, h),
        (100, 50),
        "default mode must fit 200×100 into 100×100"
    );
}

/// `-resize WxH^` fills the box: 200×100 → 100×100^ → 200×100 (smaller
/// dim hits target via the larger scale = max(100/200, 100/100) = 1.0;
/// effectively no change here). Use 50×50^ which scales by max(50/200,
/// 50/100) = 0.5, giving 100×50.
#[test]
fn resize_fill_caret_picks_larger_scale() {
    let dir = temp_dir("resize-fill");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-resize".into(),
            "50x50^".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("resize fill render");
    let (w, h) = png_dims(&fs::read(&out).unwrap());
    assert_eq!((w, h), (100, 50), "fill mode picks the larger scale");
}

/// `-resize WxH>` only shrinks if the source is larger; otherwise
/// pass-through. Source 200×100; request 1000×1000> → no-op.
#[test]
fn resize_shrink_only_skips_when_already_smaller() {
    let dir = temp_dir("resize-shrink");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-resize".into(),
            "1000x1000>".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("resize shrink render");
    let (w, h) = png_dims(&fs::read(&out).unwrap());
    assert_eq!(
        (w, h),
        (200, 100),
        "shrink-only must pass through smaller input"
    );
}

/// `-resize N%` scales both axes by N percent. Source 200×100 at 50% →
/// 100×50.
#[test]
fn resize_percent_halves_both_axes() {
    let dir = temp_dir("resize-percent");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-resize".into(),
            "50%".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("resize percent render");
    let (w, h) = png_dims(&fs::read(&out).unwrap());
    assert_eq!((w, h), (100, 50), "50% must halve both axes");
}

/// `-thumbnail WxH` resolves the same geometry as `-resize` and (for
/// the side-channel) is otherwise indistinguishable from
/// `-resize WxH`. Source 200×100; thumbnail 100×100 → 100×50.
#[test]
fn thumbnail_renders_with_geometry_resolution() {
    let dir = temp_dir("thumbnail");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.png");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-density".into(),
            "72".into(),
            "-thumbnail".into(),
            "100x100".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("thumbnail render");
    let (w, h) = png_dims(&fs::read(&out).unwrap());
    assert_eq!((w, h), (100, 50), "thumbnail honours fit-inside aspect");
}

/// `-define KEY=VALUE` is accepted on the PDF side-channel. The
/// side-channel doesn't consume the key (it's a sink-side hint), but
/// the render must still succeed end-to-end.
#[test]
fn define_flag_accepted_on_pdf_side_channel() {
    let dir = temp_dir("define");
    let pdf_path = dir.join("in.pdf");
    fs::write(&pdf_path, make_two_page_pdf()).unwrap();
    let out = dir.join("out.jpg");
    oxideav_cli_convert::run(
        &[
            format!("{}[0]", pdf_path.to_string_lossy()),
            "-background".into(),
            "white".into(),
            "-alpha".into(),
            "remove".into(),
            "-define".into(),
            "jpeg:dct-method=float".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("define render");
    let bytes = fs::read(&out).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8], "JPEG SOI expected");
}
