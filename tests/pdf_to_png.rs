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
        let mut path = VPath::default();
        path.commands = vec![
            PathCommand::MoveTo(Point::new(10.0, 10.0)),
            PathCommand::LineTo(Point::new(100.0, 10.0)),
            PathCommand::LineTo(Point::new(100.0, 80.0)),
            PathCommand::LineTo(Point::new(10.0, 80.0)),
            PathCommand::Close,
        ];
        let node = PathNode {
            path,
            fill: Some(Paint::Solid(paint_color)),
            stroke: None,
            fill_rule: oxideav_core::vector::FillRule::NonZero,
        };
        let mut group = Group::default();
        group.children = vec![Node::Path(node)];
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

    let mut scene = Scene::default();
    scene.pages = Some(vec![
        make_page(Rgba::new(255, 0, 0, 255)),
        make_page(Rgba::new(0, 0, 255, 255)),
    ]);
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
