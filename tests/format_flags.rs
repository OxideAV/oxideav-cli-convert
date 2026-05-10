//! End-to-end coverage for the per-format encoder option flags
//! (`-stl-format`, `-gltf-format`).
//!
//! Builds a tiny `Scene3D` (one triangle), writes it to disk as a binary
//! STL fixture, then drives the convert verb with each flag and asserts
//! the on-disk output matches the requested flavour.
//!
//! Cargo-feature-gated on `mesh3d` — the test crate is empty when the
//! feature is off (matches the side-channel module's gating).

#![cfg(feature = "mesh3d")]

use std::fs;
use std::path::PathBuf;

use oxideav_cli_convert::run as convert_run;
use oxideav_core::RuntimeContext;
use oxideav_mesh3d::{Mesh, Mesh3DRegistry, Node, Primitive, Scene3D, Topology, Transform};

fn ctx() -> RuntimeContext {
    RuntimeContext::new()
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-format-flags-{name}-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

/// Single-triangle scene — same shape as `tests/mesh3d_convert.rs`.
fn make_one_triangle_scene() -> Scene3D {
    let mut prim = Primitive::new(Topology::Triangles);
    prim.positions = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let mesh = Mesh::new("triangle".to_string()).with_primitive(prim);

    let mut scene = Scene3D::new();
    scene.meshes.push(mesh);

    let node = Node {
        name: Some("triangle-node".to_string()),
        mesh: Some(oxideav_mesh3d::MeshId(0)),
        transform: Transform::identity(),
        ..Node::default()
    };
    scene.nodes.push(node);
    scene.roots.push(oxideav_mesh3d::NodeId(0));

    scene
}

fn write_stl_fixture(path: &std::path::Path, scene: &Scene3D) {
    let mut reg = Mesh3DRegistry::new();
    oxideav_meta::populate_mesh3d_registry(&mut reg);
    let mut enc = reg
        .encoder_for_extension("stl")
        .expect("STL encoder registered");
    let bytes = enc.encode(scene).expect("STL encode succeeds");
    fs::write(path, bytes).expect("write STL fixture");
}

// ---------------------------------------------------------------------
// -stl-format
// ---------------------------------------------------------------------

#[test]
fn stl_format_ascii_emits_solid_endsolid() {
    let dir = temp_dir("stl-ascii");
    let in_stl = dir.join("input.stl");
    let out_stl = dir.join("output.stl");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-stl-format".into(),
        "ascii".into(),
        out_stl.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert with -stl-format ascii succeeds");

    let body = fs::read_to_string(&out_stl).expect("read STL output as UTF-8 (ASCII flavour)");
    assert!(
        body.starts_with("solid"),
        "ASCII STL should start with `solid`, got: {body:?}"
    );
    assert!(
        body.contains("endsolid"),
        "ASCII STL should contain `endsolid`, got: {body:?}"
    );
}

#[test]
fn stl_format_binary_emits_84_byte_header_or_more() {
    let dir = temp_dir("stl-binary");
    let in_stl = dir.join("input.stl");
    let out_stl = dir.join("output.stl");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-stl-format".into(),
        "binary".into(),
        out_stl.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert with -stl-format binary succeeds");

    let bytes = fs::read(&out_stl).expect("read STL output");
    // Binary STL: 80-byte header + 4-byte little-endian triangle count
    // = 84 bytes minimum, plus 50 bytes per triangle (one triangle here).
    assert!(
        bytes.len() >= 84 + 50,
        "binary STL should be at least header (80) + count (4) + 50 per triangle = 134 bytes for one triangle, got {} bytes",
        bytes.len()
    );
    // Triangle count is the 4 bytes immediately after the header.
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]);
    assert_eq!(count, 1, "binary STL should encode exactly one triangle");
    // Sanity: the `solid` ASCII marker should NOT appear at the start.
    // The 80-byte header may legally contain `solid` anywhere as a
    // free-form string, so we only check the first byte: ASCII STL
    // starts with the literal token, binary stuffs whatever the
    // encoder put in the materialised header.
    assert_ne!(
        &bytes[..5],
        b"solid",
        "binary STL header must not begin with the ASCII `solid` token"
    );
}

#[test]
fn stl_format_alias_bin_and_text() {
    // Spot-check the synonyms accepted by the parser are honoured.
    let dir = temp_dir("stl-alias");
    let in_stl = dir.join("input.stl");
    let out_bin = dir.join("out-bin.stl");
    let out_text = dir.join("out-text.stl");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    convert_run(
        &[
            in_stl.to_string_lossy().into_owned(),
            "-stl-format".into(),
            "bin".into(),
            out_bin.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("`bin` alias accepted");
    let bin_bytes = fs::read(&out_bin).unwrap();
    assert_ne!(&bin_bytes[..5], b"solid", "bin alias → binary output");

    convert_run(
        &[
            in_stl.to_string_lossy().into_owned(),
            "-stl-format".into(),
            "text".into(),
            out_text.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("`text` alias accepted");
    let text_body = fs::read_to_string(&out_text).unwrap();
    assert!(
        text_body.starts_with("solid"),
        "text alias → ASCII output, got: {text_body:?}"
    );
}

#[test]
fn stl_format_unknown_value_rejected_at_parse() {
    let dir = temp_dir("stl-unknown");
    let in_stl = dir.join("input.stl");
    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-stl-format".into(),
        "xml".into(),
        dir.join("out.stl").to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("unknown -stl-format value should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("-stl-format")
            && (msg.contains("unknown flavour") || msg.contains("expected 'binary'")),
        "expected -stl-format value error, got: {msg}"
    );
}

#[test]
fn stl_format_ignored_when_output_is_obj_errors_clearly() {
    // -stl-format is meaningless with a `.obj` output. We surface this
    // as an actionable error rather than silently dropping the flag.
    let dir = temp_dir("stl-flag-on-obj");
    let in_stl = dir.join("input.stl");
    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-stl-format".into(),
        "ascii".into(),
        dir.join("out.obj").to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("-stl-format on .obj output should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("-stl-format set but output extension is '.obj'"),
        "expected mismatched-output error, got: {msg}"
    );
}

// ---------------------------------------------------------------------
// -gltf-format
// ---------------------------------------------------------------------

#[test]
fn gltf_format_glb_writes_binary_container_even_with_gltf_extension() {
    // `-gltf-format glb` overrides the by-extension default of
    // `JsonEmbedded` for `.gltf`, so the on-disk bytes start with
    // the `glTF` magic instead of an opening JSON brace.
    let dir = temp_dir("gltf-glb-on-gltf-ext");
    let in_stl = dir.join("input.stl");
    let out_gltf = dir.join("output.gltf");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-gltf-format".into(),
        "glb".into(),
        out_gltf.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert with -gltf-format glb succeeds");

    let bytes = fs::read(&out_gltf).expect("read glTF output");
    assert!(
        bytes.len() >= 4 && &bytes[..4] == b"glTF",
        "GLB output should start with the `glTF` magic, got first 4 bytes: {:?}",
        &bytes[..bytes.len().min(4)]
    );
}

#[test]
fn gltf_format_embedded_writes_json_even_with_glb_extension() {
    // The mirror case: `-gltf-format embedded` → JSON output even
    // though the extension is `.glb`.
    let dir = temp_dir("gltf-embedded-on-glb-ext");
    let in_stl = dir.join("input.stl");
    let out_glb = dir.join("output.glb");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-gltf-format".into(),
        "embedded".into(),
        out_glb.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert with -gltf-format embedded succeeds");

    let body = fs::read_to_string(&out_glb).expect("read glTF output as JSON");
    assert!(
        body.contains("\"asset\""),
        "JSON-embedded glTF should mention the asset block, got: {body}"
    );
    // And the binary buffer should be a `data:` URI.
    assert!(
        body.contains("data:application/octet-stream;base64,"),
        "JSON-embedded glTF should inline the buffer as a base64 data URI, got: {body}"
    );
}

#[test]
fn gltf_format_external_reports_followup_error() {
    // `external` parses fine (the parser accepts the token) but the
    // encoder builder rejects it with a clean follow-up message.
    let dir = temp_dir("gltf-external-followup");
    let in_stl = dir.join("input.stl");
    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-gltf-format".into(),
        "external".into(),
        dir.join("out.gltf").to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("`external` should currently error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("-gltf-format external") && msg.contains("gltf-rN"),
        "expected gltf-rN follow-up error, got: {msg}"
    );
}

#[test]
fn gltf_format_unknown_value_rejected_at_parse() {
    let dir = temp_dir("gltf-unknown");
    let in_stl = dir.join("input.stl");
    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-gltf-format".into(),
        "obj".into(),
        dir.join("out.gltf").to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("unknown -gltf-format value should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("-gltf-format")
            && (msg.contains("unknown flavour") || msg.contains("expected 'glb'")),
        "expected -gltf-format value error, got: {msg}"
    );
}

#[test]
fn gltf_format_on_stl_output_errors_clearly() {
    let dir = temp_dir("gltf-flag-on-stl");
    let in_stl = dir.join("input.stl");
    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    let args = vec![
        in_stl.to_string_lossy().into_owned(),
        "-gltf-format".into(),
        "glb".into(),
        dir.join("out.stl").to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("-gltf-format on .stl output should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("-gltf-format set but output extension is '.stl'"),
        "expected mismatched-output error, got: {msg}"
    );
}

// ---------------------------------------------------------------------
// No-flag baseline — the default code path must be unchanged.
// ---------------------------------------------------------------------

#[test]
fn default_no_flags_picks_registry_default() {
    // No `-stl-format`, no `-gltf-format`. STL→STL should still
    // default to binary output (the registry's factory choice).
    let dir = temp_dir("stl-default");
    let in_stl = dir.join("input.stl");
    let out_stl = dir.join("output.stl");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&in_stl, &scene);

    convert_run(
        &[
            in_stl.to_string_lossy().into_owned(),
            out_stl.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("default STL→STL succeeds");

    let bytes = fs::read(&out_stl).unwrap();
    assert_ne!(
        &bytes[..5],
        b"solid",
        "registry default should be binary STL, not ASCII"
    );
}
