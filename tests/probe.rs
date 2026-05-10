//! End-to-end coverage for `--probe` (structural inspection mode).
//!
//! The probe is a write-free decode that surfaces input metadata
//! (page count, mesh count, sample rate, …) to stdout. Each test
//! constructs a minimal fixture in `temp_dir` (so we don't depend on
//! any sample files), drives `oxideav_cli_convert::run` directly with
//! `--probe`, and asserts the routing behaved (no error). Stdout
//! capture is intentionally NOT used — calling `convert_run` directly
//! exercises every code path except the actual `print!` line, and
//! relying on `gag`/`std::io::set_output_capture` would pull in a dep
//! and a nightly-only API respectively.
//!
//! For the JSON formatter shape we drive the parser via a separate
//! test that asserts `--probe --json` is accepted at parse-time.

use std::fs;
use std::path::PathBuf;

use oxideav_cli_convert::run as convert_run;
use oxideav_core::vector::{
    Group, Node, Paint, Path as VPath, PathCommand, PathNode, Point, Rgba, VectorFrame,
};
use oxideav_core::RuntimeContext;
use oxideav_pdf::write_pdf_from_scene;
use oxideav_scene::{Metadata, Page, Scene};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-probe-{name}-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_source::register(&mut ctx);
    ctx
}

// ─────────────────────────── PDF probe ───────────────────────────

fn write_two_page_pdf(path: &std::path::Path) {
    // Re-use the same shape as `tests/pdf_to_png.rs`: a 2-page Scene
    // with one rectangular path per page. The probe doesn't care
    // about the actual painting — what matters is page_count == 2.
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
    let bytes = write_pdf_from_scene(&scene).expect("pdf encode");
    fs::write(path, bytes).expect("write PDF fixture");
}

#[test]
fn probe_pdf_two_pages_succeeds_without_output_arg() {
    let dir = temp_dir("pdf-pretty");
    let pdf_path = dir.join("doc.pdf");
    write_two_page_pdf(&pdf_path);

    // Pretty-form: no `--json` flag.
    convert_run(
        &["--probe".into(), pdf_path.to_string_lossy().into_owned()],
        &ctx(),
    )
    .expect("--probe on PDF should succeed");
}

#[test]
fn probe_pdf_two_pages_json_form_succeeds() {
    let dir = temp_dir("pdf-json");
    let pdf_path = dir.join("doc.pdf");
    write_two_page_pdf(&pdf_path);

    // JSON-form: `--probe --json`.
    convert_run(
        &[
            "--probe".into(),
            "--json".into(),
            pdf_path.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("--probe --json on PDF should succeed");
}

/// Write a 1-page PDF with a populated `/Info` dictionary so we can
/// assert the probe surfaces title / author / producer / etc.
fn write_pdf_with_metadata(path: &std::path::Path) {
    let make_page = || {
        let path_node = PathNode {
            path: VPath {
                commands: vec![
                    PathCommand::MoveTo(Point::new(10.0, 10.0)),
                    PathCommand::LineTo(Point::new(50.0, 10.0)),
                    PathCommand::LineTo(Point::new(50.0, 50.0)),
                    PathCommand::Close,
                ],
            },
            fill: Some(Paint::Solid(Rgba::new(0, 200, 0, 255))),
            stroke: None,
            fill_rule: oxideav_core::vector::FillRule::NonZero,
        };
        let group = Group {
            children: vec![Node::Path(path_node)],
            ..Group::default()
        };
        let mut frame = VectorFrame::new(100.0, 100.0);
        frame.root = group;
        Page {
            width: 100.0,
            height: 100.0,
            content: frame,
            label: None,
            orientation: 0,
        }
    };
    let scene = Scene {
        pages: Some(vec![make_page()]),
        metadata: Metadata {
            title: Some("Probe Test Doc".into()),
            author: Some("oxideav".into()),
            subject: Some("automated probe coverage".into()),
            keywords: vec!["probe".into(), "test".into()],
            creator: Some("oxideav-cli-convert tests".into()),
            producer: Some("oxideav-pdf".into()),
            created_at: Some("2026-05-10T12:00:00Z".into()),
            ..Metadata::default()
        },
        ..Scene::default()
    };
    let bytes = write_pdf_from_scene(&scene).expect("pdf encode");
    fs::write(path, bytes).expect("write PDF fixture");
}

#[test]
fn probe_pdf_surfaces_info_dict_fields() {
    use oxideav_cli_convert::args::parse as args_parse;
    use oxideav_cli_convert::probe::render as probe_render;

    let dir = temp_dir("pdf-info");
    let pdf_path = dir.join("doc.pdf");
    write_pdf_with_metadata(&pdf_path);

    let plan = args_parse(&[
        "--probe".into(),
        "--json".into(),
        pdf_path.to_string_lossy().into_owned(),
    ])
    .expect("parse");
    let json = probe_render(&plan, &ctx()).expect("render");
    // is_encrypted is always present for PDFs.
    assert!(
        json.contains("\"is_encrypted\":\"no\""),
        "expected is_encrypted=no, got: {json}"
    );
    assert!(
        json.contains("\"title\":\"Probe Test Doc\""),
        "expected title, got: {json}"
    );
    assert!(
        json.contains("\"author\":\"oxideav\""),
        "expected author, got: {json}"
    );
    assert!(
        json.contains("\"producer\":\"oxideav-pdf\""),
        "expected producer, got: {json}"
    );
    assert!(
        json.contains("\"creator\":\"oxideav-cli-convert tests\""),
        "expected creator, got: {json}"
    );
    assert!(
        json.contains("\"creation_date\":"),
        "expected creation_date, got: {json}"
    );
    assert!(
        json.contains("\"keywords\":[\"probe\",\"test\"]"),
        "expected keywords array, got: {json}"
    );
}

// ─────────────────────────── SVG probe ───────────────────────────

fn write_minimal_svg(path: &std::path::Path) {
    // Hand-rolled minimal SVG — no embedded images, just a single
    // rect. Enough for the probe to read width/height + a non-zero
    // file size and report `kind: svg`.
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100" viewBox="0 0 200 100">
  <rect x="10" y="10" width="180" height="80" fill="red" />
</svg>
"#;
    fs::write(path, body.as_bytes()).expect("write SVG fixture");
}

#[test]
fn probe_svg_succeeds_without_output_arg() {
    let dir = temp_dir("svg");
    let svg_path = dir.join("simple.svg");
    write_minimal_svg(&svg_path);

    convert_run(
        &["--probe".into(), svg_path.to_string_lossy().into_owned()],
        &ctx(),
    )
    .expect("--probe on SVG should succeed");
}

// ─────────────────────────── 3D probe (mesh3d) ───────────────────────────

#[cfg(feature = "mesh3d")]
fn write_one_triangle_stl_fixture(path: &std::path::Path) {
    use oxideav_mesh3d::{
        Mesh, Mesh3DRegistry, Node as MNode, Primitive, Scene3D, Topology, Transform,
    };
    let mut prim = Primitive::new(Topology::Triangles);
    prim.positions = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let mesh = Mesh::new("triangle".to_string()).with_primitive(prim);
    let mut scene = Scene3D::new();
    scene.meshes.push(mesh);
    let node = MNode {
        name: Some("triangle-node".to_string()),
        mesh: Some(oxideav_mesh3d::MeshId(0)),
        transform: Transform::identity(),
        ..MNode::default()
    };
    scene.nodes.push(node);
    scene.roots.push(oxideav_mesh3d::NodeId(0));

    let mut reg = Mesh3DRegistry::new();
    oxideav_meta::populate_mesh3d_registry(&mut reg);
    let mut enc = reg
        .encoder_for_extension("stl")
        .expect("STL encoder registered");
    let bytes = enc.encode(&scene).expect("STL encode succeeds");
    fs::write(path, bytes).expect("write STL fixture");
}

#[cfg(feature = "mesh3d")]
#[test]
fn probe_stl_succeeds_without_output_arg() {
    let dir = temp_dir("stl");
    let stl_path = dir.join("triangle.stl");
    write_one_triangle_stl_fixture(&stl_path);

    convert_run(
        &["--probe".into(), stl_path.to_string_lossy().into_owned()],
        &ctx(),
    )
    .expect("--probe on STL should succeed");
}

#[cfg(feature = "mesh3d")]
#[test]
fn probe_stl_json_form_succeeds() {
    let dir = temp_dir("stl-json");
    let stl_path = dir.join("triangle.stl");
    write_one_triangle_stl_fixture(&stl_path);

    convert_run(
        &[
            "--probe".into(),
            "--json".into(),
            stl_path.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("--probe --json on STL should succeed");
}

#[cfg(feature = "mesh3d")]
#[test]
fn probe_stl_json_surfaces_per_mesh_detail() {
    use oxideav_cli_convert::args::parse as args_parse;
    use oxideav_cli_convert::probe::render as probe_render;

    let dir = temp_dir("stl-per-mesh");
    let stl_path = dir.join("triangle.stl");
    write_one_triangle_stl_fixture(&stl_path);

    let plan = args_parse(&[
        "--probe".into(),
        "--json".into(),
        stl_path.to_string_lossy().into_owned(),
    ])
    .expect("parse");
    let json = probe_render(&plan, &ctx()).expect("render");
    // Per-mesh array surfaced.
    assert!(
        json.contains("\"meshes\":["),
        "expected meshes array, got: {json}"
    );
    // STL doesn't carry a mesh name (the binary header is opaque /
    // ASCII solid name doesn't propagate as a `Mesh::name`); the
    // unnamed fallback is the right answer.
    assert!(
        json.contains("\"vertex_count\":3"),
        "expected per-mesh vertex_count=3, got: {json}"
    );
    assert!(
        json.contains("\"triangle_count\":1"),
        "expected per-mesh triangle_count=1, got: {json}"
    );
    // Per-mesh bounding_box is computed from the triangle positions
    // (0..1 unit cube corner).
    assert!(
        json.contains("\"bounding_box\":{\"min_x\":0,\"min_y\":0"),
        "expected per-mesh bounding_box, got: {json}"
    );
    // Empty materials/animations are still emitted as empty arrays.
    assert!(
        json.contains("\"materials\":[]"),
        "expected empty materials array, got: {json}"
    );
    assert!(
        json.contains("\"animations\":[]"),
        "expected empty animations array, got: {json}"
    );
}

// ─────────────────────────── Mutual-exclusion / arg-parsing ─────────────

#[test]
fn probe_with_output_arg_errors_clearly() {
    // `--probe in out.png` is the misuse case — we surface a clear
    // actionable error rather than picking one or the other silently.
    let dir = temp_dir("probe-with-output");
    let svg_path = dir.join("simple.svg");
    write_minimal_svg(&svg_path);

    let out_path = dir.join("out.png");
    let err = convert_run(
        &[
            "--probe".into(),
            svg_path.to_string_lossy().into_owned(),
            out_path.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect_err("--probe with output positional should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("--probe cannot be combined with an output file"),
        "expected mutual-exclusion error, got: {msg}"
    );
}

#[test]
fn json_without_probe_errors_clearly() {
    // `--json` is only meaningful paired with `--probe`. Without
    // `--probe` we'd otherwise silently swallow the flag — which is
    // worse UX than a clear "needs --probe" error.
    let dir = temp_dir("json-alone");
    let svg_path = dir.join("simple.svg");
    write_minimal_svg(&svg_path);
    let out_path = dir.join("out.svg");

    let err = convert_run(
        &[
            "--json".into(),
            svg_path.to_string_lossy().into_owned(),
            out_path.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect_err("--json without --probe should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("--json requires --probe"),
        "expected --json-needs-probe error, got: {msg}"
    );
}

#[test]
fn probe_without_input_errors_clearly() {
    // `--probe` alone (no input positional) should error the same way
    // a regular convert without inputs does.
    let err =
        convert_run(&["--probe".into()], &ctx()).expect_err("--probe without input should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("no input file given"),
        "expected no-input error, got: {msg}"
    );
}
