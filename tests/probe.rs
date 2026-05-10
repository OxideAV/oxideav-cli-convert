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
use oxideav_scene::{Page, Scene};

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
