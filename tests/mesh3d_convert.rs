//! End-to-end: build a tiny [`Scene3D`] (one triangle), encode it as
//! STL on disk, run `convert` to translate STL → OBJ and STL → glTF,
//! then assert the output files exist with sensible byte signatures.
//!
//! Cargo-feature-gated on `mesh3d` — the test crate is empty when
//! the feature is off (matches the side-channel module's gating).

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
        "oxideav-cli-convert-mesh3d-{name}-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

/// Build a single-triangle [`Scene3D`] suitable for round-tripping
/// through every 3D format we wire (STL/OBJ/glTF). Keeps the topology
/// to plain `Triangles` + raw positions so the OBJ encoder (which
/// requires triangulated input) doesn't reject it.
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

/// Encode the scene as binary STL and write it to `path`. Goes through
/// the same registry the convert verb uses so the bytes that land on
/// disk match what the CLI sees.
fn write_stl_fixture(path: &std::path::Path, scene: &Scene3D) {
    let mut reg = Mesh3DRegistry::new();
    oxideav_meta::populate_mesh3d_registry(&mut reg);
    let mut enc = reg
        .encoder_for_extension("stl")
        .expect("STL encoder registered");
    let bytes = enc.encode(scene).expect("STL encode succeeds");
    fs::write(path, bytes).expect("write STL fixture");
}

#[test]
fn convert_stl_to_obj_writes_obj_with_v_lines() {
    let dir = temp_dir("stl-to-obj");
    let stl_path = dir.join("input.stl");
    let obj_path = dir.join("output.obj");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        obj_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert STL→OBJ succeeds");

    assert!(obj_path.exists(), "OBJ output should exist");
    let body = fs::read_to_string(&obj_path).expect("read OBJ output");
    // Wavefront OBJ vertex lines start with `v `; expect at least one.
    assert!(
        body.lines().any(|l| l.starts_with("v ")),
        "OBJ output should contain `v` (vertex) lines, got:\n{body}"
    );
    // And a face line — STL→OBJ via Scene3D round-trip should
    // preserve the single triangle.
    assert!(
        body.lines().any(|l| l.starts_with("f ")),
        "OBJ output should contain `f` (face) lines, got:\n{body}"
    );
}

#[test]
fn convert_stl_to_gltf_writes_json() {
    let dir = temp_dir("stl-to-gltf");
    let stl_path = dir.join("input.stl");
    let gltf_path = dir.join("output.gltf");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        gltf_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert STL→glTF succeeds");

    assert!(gltf_path.exists(), "glTF output should exist");
    let body = fs::read_to_string(&gltf_path).expect("read glTF output");
    // glTF JSON envelope should mention the asset block.
    assert!(
        body.contains("\"asset\""),
        "glTF output should be JSON with an \"asset\" key, got:\n{body}"
    );
}

#[test]
fn convert_obj_to_gltf_works_end_to_end() {
    // Build an OBJ fixture via the registry (so we don't have to
    // hand-author the text format), then convert OBJ → glTF.
    let dir = temp_dir("obj-to-gltf");
    let obj_path = dir.join("input.obj");
    let gltf_path = dir.join("output.gltf");

    let scene = make_one_triangle_scene();
    {
        let mut reg = Mesh3DRegistry::new();
        oxideav_meta::populate_mesh3d_registry(&mut reg);
        let mut enc = reg
            .encoder_for_extension("obj")
            .expect("OBJ encoder registered");
        let bytes = enc.encode(&scene).expect("OBJ encode succeeds");
        fs::write(&obj_path, bytes).expect("write OBJ fixture");
    }

    let args = vec![
        obj_path.to_string_lossy().into_owned(),
        gltf_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert OBJ→glTF succeeds");

    let body = fs::read_to_string(&gltf_path).expect("read glTF output");
    assert!(body.contains("\"asset\""), "glTF should mention asset");
}

#[test]
fn convert_stl_to_glb_writes_binary_container() {
    let dir = temp_dir("stl-to-glb");
    let stl_path = dir.join("input.stl");
    let glb_path = dir.join("output.glb");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        glb_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert STL→GLB succeeds");

    let bytes = fs::read(&glb_path).expect("read GLB output");
    // GLB binary container starts with the magic `glTF` (0x46546C67 LE).
    assert!(
        bytes.len() >= 4 && &bytes[..4] == b"glTF",
        "GLB output should start with the `glTF` magic, got first 4 bytes: {:?}",
        &bytes[..bytes.len().min(4)]
    );
}

#[test]
fn convert_3d_input_to_raster_renders_png() {
    // STL → PNG now goes through the 3D→raster software renderer
    // instead of being rejected. The output PNG should exist + start
    // with the PNG magic bytes.
    let dir = temp_dir("stl-to-png-renders");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG should render via mesh3d_render");

    let bytes = fs::read(&png_path).expect("read PNG output");
    assert!(
        bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n",
        "PNG output should start with the PNG magic, got: {:?}",
        &bytes[..bytes.len().min(8)]
    );
}

#[test]
fn convert_3d_input_to_unrelated_output_errors() {
    // Anything that's neither a 3D output nor a known raster output
    // (e.g. `.mp4`) is still rejected with the pairing-mismatch error.
    let dir = temp_dir("stl-to-mp4-rejected");
    let stl_path = dir.join("input.stl");
    let mp4_path = dir.join("output.mp4");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        mp4_path.to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("STL→MP4 should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("must pair with a 3D output") || msg.contains("must pair with"),
        "expected pairing-mismatch error, got: {msg}"
    );
}

#[test]
fn convert_stl_to_usdz_round_trips() {
    // USDZ is now a round-trip target — encoder ships in oxideav-usdz
    // and the runner's MESH3D_OUTPUT_EXTS includes "usdz".
    let dir = temp_dir("stl-to-usdz");
    let stl_path = dir.join("input.stl");
    let usdz_path = dir.join("output.usdz");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        usdz_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("convert STL→USDZ succeeds");

    let bytes = fs::read(&usdz_path).expect("read USDZ output");
    // USDZ is a STORED ZIP; first 4 bytes are the local-file-header
    // signature `PK\x03\x04`.
    assert!(
        bytes.len() >= 4 && &bytes[..4] == b"PK\x03\x04",
        "USDZ output should start with PK\\x03\\x04, got: {:?}",
        &bytes[..bytes.len().min(4)]
    );
}

#[test]
fn convert_3d_input_to_raster_renders_with_gouraud_shading() {
    // Round-next: -render gouraud must be honoured end-to-end.
    let dir = temp_dir("stl-gouraud");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-render".into(),
        "gouraud".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG -render gouraud should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
}

#[test]
fn convert_3d_input_to_raster_renders_with_phong_shading() {
    // Round-next: -render phong must be honoured end-to-end.
    let dir = temp_dir("stl-phong");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-render".into(),
        "phong".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG -render phong should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
}

#[test]
fn convert_3d_input_to_raster_with_camera_and_projection() {
    // Round-next: -camera + -projection + -fov + -bg + -light all
    // pass through to the renderer cleanly.
    let dir = temp_dir("stl-cam-ortho");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-projection".into(),
        "orthographic".into(),
        "-camera".into(),
        "30,45,1.5".into(),
        "-fov".into(),
        "45".into(),
        "-light".into(),
        "60,30,1.0".into(),
        "-bg".into(),
        "#202020".into(),
        "-render".into(),
        "gouraud".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG with all renderer flags should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
}

#[test]
fn fbx_extension_is_recognised_as_mesh3d_input() {
    // Once oxideav-fbx is wired in, the runner should claim `.fbx`
    // as a 3D input. We can't easily author a real binary FBX in a
    // smoke test (the format needs a 27-byte magic + a recursive node
    // tree), so just assert the recognition + that random bytes
    // produce a decoder error (NOT an "unknown extension" error).
    use oxideav_cli_convert::mesh3d_runner::is_mesh3d_input;
    assert!(is_mesh3d_input("foo.fbx"));
    assert!(is_mesh3d_input("FOO.FBX"));

    let dir = temp_dir("fbx-bad-bytes");
    let fbx_path = dir.join("input.fbx");
    let gltf_path = dir.join("output.gltf");
    fs::write(&fbx_path, b"not really an fbx file").expect("write bogus FBX");

    let args = vec![
        fbx_path.to_string_lossy().into_owned(),
        gltf_path.to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("bogus FBX should fail at decode");
    let msg = format!("{err:?}");
    // The decoder owns the message; we just want to be sure the
    // failure didn't come from "no decoder registered".
    assert!(
        !msg.contains("no 3D decoder registered for input extension '.fbx'"),
        "FBX should be wired; decoder must produce its own error, got: {msg}"
    );
}

#[test]
fn convert_3d_input_to_raster_renders_with_normal_debug() {
    // Round 45: -render normal-debug paints normals as RGB.
    let dir = temp_dir("stl-normal-debug");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-render".into(),
        "normal-debug".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG -render normal-debug should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
}

#[test]
fn convert_3d_input_to_raster_renders_with_depth_debug() {
    // Round 45: -render depth-debug paints NDC z as grayscale.
    let dir = temp_dir("stl-depth-debug");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-render".into(),
        "depth-debug".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG -render depth-debug should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
}

#[test]
fn convert_3d_input_to_raster_with_aa_supersampling() {
    // Round 45: -aa N must be honoured end-to-end and produce the
    // same output dimensions as the no-aa render (only the smoothness
    // changes — the framebuffer footprint is invisible to the user).
    let dir = temp_dir("stl-aa");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-resize".into(),
        "64x64".into(),
        "-aa".into(),
        "4".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG -aa 4 should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
    // PNG IHDR chunk starts at byte 8, length 4 bytes (== 13), then
    // chunk type 4 bytes (b"IHDR"), then 4 bytes width, 4 bytes height.
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    assert_eq!(width, 64, "AA must not change output width");
    assert_eq!(height, 64, "AA must not change output height");
}

#[test]
fn convert_3d_input_to_raster_aa_one_means_no_supersampling() {
    // -aa 1 is documented as "off"; must succeed and produce a valid
    // PNG, even though it's a no-op compared to omitting the flag.
    let dir = temp_dir("stl-aa1");
    let stl_path = dir.join("input.stl");
    let png_path = dir.join("output.png");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        "-aa".into(),
        "1".into(),
        png_path.to_string_lossy().into_owned(),
    ];
    convert_run(&args, &ctx()).expect("STL→PNG -aa 1 should succeed");
    let bytes = fs::read(&png_path).expect("read PNG");
    assert!(&bytes[..8] == b"\x89PNG\r\n\x1a\n");
}

#[test]
fn convert_3d_input_with_printf_template_errors() {
    // 3D scenes are single-document — a `%d` template makes no sense.
    let dir = temp_dir("stl-with-template-rejected");
    let stl_path = dir.join("input.stl");

    let scene = make_one_triangle_scene();
    write_stl_fixture(&stl_path, &scene);

    let args = vec![
        stl_path.to_string_lossy().into_owned(),
        dir.join("out-%02d.obj").to_string_lossy().into_owned(),
    ];
    let err = convert_run(&args, &ctx()).expect_err("STL + %d should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("`%d` template"),
        "expected template-rejection error, got: {msg}"
    );
}
