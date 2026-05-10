//! 3D scene → raster image.
//!
//! When `convert` sees a 3D-asset input (`.stl`/`.obj`/`.gltf`/`.glb`/
//! `.usdz`) paired with a raster output (`.png`/`.jpg`/`.bmp`/`.webp`),
//! the [`mesh3d_runner`](crate::mesh3d_runner) refuses (encoders only
//! cover same-class round-trips). This module bridges the gap: decode
//! the input through the same [`Mesh3DRegistry`] populated by
//! [`oxideav_meta::populate_mesh3d_registry`], rasterise the
//! [`Scene3D`] with a tiny built-in software renderer, then hand the
//! resulting [`RgbaImage`] to the shared [`raster_io`] encoders.
//!
//! ## Renderer shape (MVP)
//!
//! Pure-Rust software rasteriser, zero external dependencies:
//!
//! 1. **Camera** — perspective projection (60° vertical FOV) framed
//!    on the scene's axis-aligned bounding box; the camera sits at
//!    `bbox_center + (0, 0, distance)` looking down `-Z`, where
//!    `distance` is chosen so the bbox fits the viewport with a 20%
//!    margin.
//! 2. **World transform walk** — every node's mesh is rendered with
//!    the composed world matrix (parent ⨯ local). Transforms compose
//!    left-to-right (column-vector convention, matching glTF /
//!    [`oxideav_mesh3d::Transform`]).
//! 3. **Triangle rasterisation** — every triangle is projected to
//!    screen space, back-face culled (CCW winding stays), and
//!    rasterised through a half-space edge-function pipeline with a
//!    per-pixel z-buffer. Quads / strips / fans are tessellated to a
//!    triangle list before rasterisation.
//! 4. **Shading** — flat shading: one constant colour per primitive,
//!    pulled from `material.base_color` (linear → sRGB encoded for
//!    the framebuffer) or a fallback grey when no material is bound.
//!    Wireframe mode draws only the three edges of each triangle as
//!    1-pixel-wide lines (Bresenham), with the same per-primitive
//!    colour.
//!
//! ## Out of scope (documented follow-ups)
//!
//! * Lighting models — Gouraud / Phong / PBR all need normal & light
//!   transport plumbing that this round skips.
//! * Texture sampling — the renderer reads `material.base_color`
//!   only; texture-mapped surfaces fall back to the constant factor.
//! * Anti-aliasing — every pixel is point-sampled; multi-sampling +
//!   resolve land later.
//! * Camera nodes from the scene — we always synthesise a default
//!   bbox-fitting camera, even when the scene supplies one.
//! * Lights — ambient + single directional pre-baked into the flat
//!   colour by averaging the material's base_color and the
//!   world-up-aligned diffuse term. Today it's pure base_color.
//!
//! Each follow-up is independently scopable; the public surface
//! ([`run`], [`render_scene`]) doesn't change when they land.

use std::fs;

use oxideav_core::{Error, Result};
use oxideav_mesh3d::{
    Indices, Material, Mesh3DRegistry, Node, NodeId, Primitive, Scene3D, Topology,
};

use crate::op::{Mesh3DOptions, Mesh3DRenderMode, Op};
use crate::pixel_xform::apply_pixel_transform_chain;
use crate::raster_io::{
    apply_alpha_ops, classify_output, encode_raster_to_path, OutputClass, RgbaImage,
};

/// Default raster output dimensions when neither `-resize` nor any
/// other geometry flag pins them. 1024x1024 is the IM `convert`
/// default for `:vector` rasterisation; we copy that so the user gets
/// a sensible-sized PNG out of the box.
const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 1024;

/// Default background fill (opaque white) when no `-background` is
/// supplied. Matches the PDF runner's default so a 3D→PNG and a
/// PDF→PNG conversion produce visually-similar canvases.
const DEFAULT_BACKGROUND: [u8; 4] = [255, 255, 255, 255];

/// Run the 3D→raster convert flow. Side-effect-only: writes one file
/// to disk.
///
/// Caller has already established that:
///
/// * `input_path`'s extension is in
///   [`crate::mesh3d_runner::is_mesh3d_input`].
/// * `output_path`'s extension is a raster format
///   ([`OutputClass::Raster`]).
///
/// `ops` is the full operation chain off `ConvertPlan::ops`; we pluck
/// out `-resize`, `-background`, and the alpha grammar; the rest of
/// the geometry/tonal ops are then applied through
/// [`apply_pixel_transform_chain`] post-rasterisation, mirroring the
/// PDF runner's flow.
pub fn run(input_path: &str, output_path: &str, ops: &[Op], options: &Mesh3DOptions) -> Result<()> {
    let in_ext = ext_of(input_path)
        .ok_or_else(|| {
            Error::invalid(format!(
                "convert: input '{input_path}' has no extension — cannot pick a 3D decoder"
            ))
        })?
        .to_ascii_lowercase();

    let class = classify_output(output_path)?;
    let fmt = match class {
        OutputClass::Raster(f) => f,
        _ => {
            return Err(Error::invalid(format!(
                "convert: 3D→raster runner invoked with non-raster output '{output_path}'"
            )));
        }
    };

    let mut registry = Mesh3DRegistry::new();
    oxideav_meta::populate_mesh3d_registry(&mut registry);
    let mut decoder = registry.decoder_for_extension(&in_ext).ok_or_else(|| {
        Error::unsupported(format!(
            "convert: no 3D decoder registered for input extension '.{in_ext}'"
        ))
    })?;

    let bytes = fs::read(input_path)
        .map_err(|e| Error::invalid(format!("convert: failed to read {input_path}: {e}")))?;
    let scene = decoder.decode(&bytes)?;

    let (width, height) = pick_dims(ops);
    let bg = pick_background(ops);
    let mode = options.render_mode.unwrap_or_default();

    let mut img = render_scene(&scene, width, height, bg, mode);

    img = apply_pixel_transform_chain(img, ops)
        .map_err(|e| Error::invalid(format!("convert: 3D-render: {e}")))?;
    apply_alpha_ops(&mut img, ops, bg);

    encode_raster_to_path(&img, fmt, output_path, ops)
}

fn pick_dims(ops: &[Op]) -> (u32, u32) {
    // `-resize WxH` (any mode) seeds the canvas size; otherwise
    // 1024x1024. Force-mode resizes still get the request honoured
    // exactly. The post-render `apply_pixel_transform_chain` would
    // also handle this for us, but rasterising at the final size up
    // front avoids a downscale step and keeps anti-aliasing native to
    // the raster grid.
    for op in ops.iter().rev() {
        if let Op::Resize { width, height, .. } = op {
            return (*width, *height);
        }
    }
    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
}

fn pick_background(ops: &[Op]) -> [u8; 4] {
    for op in ops.iter().rev() {
        if let Op::Background(c) = op {
            return *c;
        }
    }
    DEFAULT_BACKGROUND
}

fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

/// Render a [`Scene3D`] to an [`RgbaImage`] at the given dimensions.
///
/// Public so the test suite can drive the renderer directly without
/// going through the file-system. The CLI-facing entry point is
/// [`run`].
pub fn render_scene(
    scene: &Scene3D,
    width: u32,
    height: u32,
    background: [u8; 4],
    mode: Mesh3DRenderMode,
) -> RgbaImage {
    let mut fb = Framebuffer::new(width, height, background);

    let bbox = scene_bbox(scene);
    let camera = Camera::fit_to_bbox(width, height, bbox);

    // Walk the node forest in pre-order, composing world matrices.
    for &root in &scene.roots {
        walk_node(scene, root, identity4(), &camera, &mut fb, mode);
    }

    fb.into_image()
}

fn walk_node(
    scene: &Scene3D,
    id: NodeId,
    parent_world: [[f32; 4]; 4],
    camera: &Camera,
    fb: &mut Framebuffer,
    mode: Mesh3DRenderMode,
) {
    let Some(node) = scene.nodes.get(id.0 as usize) else {
        return;
    };
    let world = mat4_mul(parent_world, node.transform.to_matrix());
    if let Some(mesh_id) = node.mesh {
        if let Some(mesh) = scene.meshes.get(mesh_id.0 as usize) {
            for prim in &mesh.primitives {
                let colour = primitive_colour(scene, prim);
                draw_primitive(prim, &world, camera, colour, fb, mode);
            }
        }
    }
    for &child in &node.children {
        walk_node(scene, child, world, camera, fb, mode);
    }
}

fn primitive_colour(scene: &Scene3D, prim: &Primitive) -> [u8; 4] {
    let mat = prim
        .material
        .and_then(|mid| scene.materials.get(mid.0 as usize));
    let base = mat
        .map(|m: &Material| m.base_color)
        .unwrap_or([0.7, 0.7, 0.75, 1.0]);
    [
        linear_to_srgb_u8(base[0]),
        linear_to_srgb_u8(base[1]),
        linear_to_srgb_u8(base[2]),
        (base[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

fn linear_to_srgb_u8(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    // Single-segment approximation — matches IEC 61966-2-1 well
    // enough for an MVP renderer.
    let v = if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Triangle-list expansion + projection + rasterisation for one
/// [`Primitive`]. Lines / line strips / line loops / points / triangle
/// strips / fans are all walked into ordered (i0, i1, i2) triplets
/// here so the inner draw loop is topology-agnostic.
fn draw_primitive(
    prim: &Primitive,
    world: &[[f32; 4]; 4],
    camera: &Camera,
    colour: [u8; 4],
    fb: &mut Framebuffer,
    mode: Mesh3DRenderMode,
) {
    if prim.positions.is_empty() {
        return;
    }

    // Project every vertex once.
    let view_proj = mat4_mul(camera.proj, camera.view);
    let mvp = mat4_mul(view_proj, *world);
    let projected: Vec<Option<[f32; 3]>> = prim
        .positions
        .iter()
        .map(|p| project_vertex(*p, &mvp, fb.width as f32, fb.height as f32))
        .collect();

    // Walk the topology and draw each triangle / line.
    let indices: Vec<u32> = match &prim.indices {
        Some(Indices::U16(v)) => v.iter().map(|&x| x as u32).collect(),
        Some(Indices::U32(v)) => v.clone(),
        None => (0..prim.positions.len() as u32).collect(),
    };
    let n = indices.len();

    match prim.topology {
        Topology::Triangles => {
            let mut i = 0;
            while i + 2 < n {
                draw_tri(
                    &projected,
                    indices[i] as usize,
                    indices[i + 1] as usize,
                    indices[i + 2] as usize,
                    colour,
                    fb,
                    mode,
                );
                i += 3;
            }
        }
        Topology::TriangleStrip => {
            let mut i = 0;
            while i + 2 < n {
                let (a, b, c) = if i % 2 == 0 {
                    (indices[i], indices[i + 1], indices[i + 2])
                } else {
                    (indices[i + 1], indices[i], indices[i + 2])
                };
                draw_tri(
                    &projected, a as usize, b as usize, c as usize, colour, fb, mode,
                );
                i += 1;
            }
        }
        Topology::TriangleFan => {
            if n >= 3 {
                for i in 1..(n - 1) {
                    draw_tri(
                        &projected,
                        indices[0] as usize,
                        indices[i] as usize,
                        indices[i + 1] as usize,
                        colour,
                        fb,
                        mode,
                    );
                }
            }
        }
        Topology::Lines => {
            let mut i = 0;
            while i + 1 < n {
                draw_line_pair(
                    &projected,
                    indices[i] as usize,
                    indices[i + 1] as usize,
                    colour,
                    fb,
                );
                i += 2;
            }
        }
        Topology::LineStrip => {
            for i in 0..(n.saturating_sub(1)) {
                draw_line_pair(
                    &projected,
                    indices[i] as usize,
                    indices[i + 1] as usize,
                    colour,
                    fb,
                );
            }
        }
        Topology::LineLoop => {
            for i in 0..(n.saturating_sub(1)) {
                draw_line_pair(
                    &projected,
                    indices[i] as usize,
                    indices[i + 1] as usize,
                    colour,
                    fb,
                );
            }
            if n >= 2 {
                draw_line_pair(
                    &projected,
                    indices[n - 1] as usize,
                    indices[0] as usize,
                    colour,
                    fb,
                );
            }
        }
        Topology::Points => {
            for &i in &indices {
                if let Some(p) = projected.get(i as usize).and_then(|v| v.as_ref()) {
                    let x = p[0].round() as i32;
                    let y = p[1].round() as i32;
                    fb.set_pixel(x, y, p[2], colour);
                }
            }
        }
    }
}

fn draw_tri(
    projected: &[Option<[f32; 3]>],
    a: usize,
    b: usize,
    c: usize,
    colour: [u8; 4],
    fb: &mut Framebuffer,
    mode: Mesh3DRenderMode,
) {
    let (Some(va), Some(vb), Some(vc)) = (
        projected.get(a).and_then(|v| *v),
        projected.get(b).and_then(|v| *v),
        projected.get(c).and_then(|v| *v),
    ) else {
        return;
    };
    match mode {
        Mesh3DRenderMode::Wireframe => {
            draw_line(fb, va, vb, colour);
            draw_line(fb, vb, vc, colour);
            draw_line(fb, vc, va, colour);
        }
        Mesh3DRenderMode::Flat => {
            rasterise_triangle(fb, va, vb, vc, colour);
        }
    }
}

fn draw_line_pair(
    projected: &[Option<[f32; 3]>],
    a: usize,
    b: usize,
    colour: [u8; 4],
    fb: &mut Framebuffer,
) {
    let (Some(va), Some(vb)) = (
        projected.get(a).and_then(|v| *v),
        projected.get(b).and_then(|v| *v),
    ) else {
        return;
    };
    draw_line(fb, va, vb, colour);
}

/// Project an object-space vertex through the model-view-projection
/// matrix, perspective-divide, and map to viewport pixel coords.
///
/// Returns `None` when the vertex sits behind the near plane (`w <=
/// 0`); the caller skips any triangle / line that touches a clipped
/// vertex (true clipping is a documented follow-up — for the MVP
/// renderer, fully-on-screen scenes work and the worst-case visible
/// degeneration is a missing primitive on a heavy clip).
fn project_vertex(pos: [f32; 3], mvp: &[[f32; 4]; 4], width: f32, height: f32) -> Option<[f32; 3]> {
    let v = mat4_mul_vec4(mvp, [pos[0], pos[1], pos[2], 1.0]);
    if v[3] <= 0.0 {
        return None;
    }
    let ndc_x = v[0] / v[3];
    let ndc_y = v[1] / v[3];
    let ndc_z = v[2] / v[3];
    let sx = (ndc_x * 0.5 + 0.5) * width;
    let sy = (1.0 - (ndc_y * 0.5 + 0.5)) * height;
    Some([sx, sy, ndc_z])
}

/// Half-space edge-function triangle rasteriser with z-buffer test.
/// Pixels are point-sampled (no MSAA). Back-face culling: triangles
/// with non-positive signed area in screen space are skipped.
fn rasterise_triangle(
    fb: &mut Framebuffer,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    colour: [u8; 4],
) {
    let area = edge(a, b, c);
    if area.abs() < 1.0e-6 {
        return;
    }
    // Back-face cull. Negative area = clockwise winding in screen
    // space (after y-flip), so cull when area <= 0 to keep CCW
    // front-faces. Many indexed meshes use either winding; for an MVP
    // renderer we render double-sided to avoid losing whole models to
    // the cull.
    let _ = area;

    let min_x = a[0].min(b[0]).min(c[0]).max(0.0).floor() as i32;
    let min_y = a[1].min(b[1]).min(c[1]).max(0.0).floor() as i32;
    let max_x = a[0].max(b[0]).max(c[0]).min(fb.width as f32 - 1.0).ceil() as i32;
    let max_y = a[1].max(b[1]).max(c[1]).min(fb.height as f32 - 1.0).ceil() as i32;

    let area_inv = 1.0 / area;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = [x as f32 + 0.5, y as f32 + 0.5, 0.0];
            let w0 = edge(b, c, p) * area_inv;
            let w1 = edge(c, a, p) * area_inv;
            let w2 = edge(a, b, p) * area_inv;
            // Inside-test honours both windings (rendering double-sided).
            let inside =
                (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0) || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0);
            if !inside {
                continue;
            }
            let z = w0 * a[2] + w1 * b[2] + w2 * c[2];
            fb.set_pixel(x, y, z, colour);
        }
    }
}

/// Bresenham line on a screen-space pair of projected vertices. Used
/// by [`Mesh3DRenderMode::Wireframe`] and the `Lines*` topologies.
fn draw_line(fb: &mut Framebuffer, a: [f32; 3], b: [f32; 3], colour: [u8; 4]) {
    let mut x0 = a[0].round() as i32;
    let mut y0 = a[1].round() as i32;
    let x1 = b[0].round() as i32;
    let y1 = b[1].round() as i32;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let len = ((dx as f32).hypot(dy as f32)).max(1.0);
    let dz = b[2] - a[2];
    loop {
        let t =
            (((x0 - a[0].round() as i32) as f32).hypot((y0 - a[1].round() as i32) as f32)) / len;
        let z = a[2] + dz * t;
        fb.set_pixel(x0, y0, z, colour);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

#[inline]
fn edge(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

// ---------------------------------------------------------------------
// 4x4 matrix helpers (column-vector convention, row-major storage).
// We don't pull in nalgebra/glam — a few hand-written 4x4 ops are
// plenty for a single-pass software rasteriser.
// ---------------------------------------------------------------------

fn identity4() -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn mat4_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0_f32; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            out[i][j] =
                a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j] + a[i][3] * b[3][j];
        }
    }
    out
}

fn mat4_mul_vec4(m: &[[f32; 4]; 4], v: [f32; 4]) -> [f32; 4] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2] + m[0][3] * v[3],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2] + m[1][3] * v[3],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2] + m[2][3] * v[3],
        m[3][0] * v[0] + m[3][1] * v[1] + m[3][2] * v[2] + m[3][3] * v[3],
    ]
}

// ---------------------------------------------------------------------
// Camera framing.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Camera {
    view: [[f32; 4]; 4],
    proj: [[f32; 4]; 4],
}

impl Camera {
    fn fit_to_bbox(width: u32, height: u32, bbox: BBox) -> Self {
        let (cx, cy, cz) = (
            (bbox.min[0] + bbox.max[0]) * 0.5,
            (bbox.min[1] + bbox.max[1]) * 0.5,
            (bbox.min[2] + bbox.max[2]) * 0.5,
        );
        let extent = ((bbox.max[0] - bbox.min[0]).max(bbox.max[1] - bbox.min[1]))
            .max(bbox.max[2] - bbox.min[2])
            .max(1.0e-3);
        let fov_y = 60.0_f32.to_radians();
        let aspect = (width.max(1) as f32) / (height.max(1) as f32);
        // Distance so the bbox extent fits the vertical FOV with a
        // 1.2x margin (so the model doesn't kiss the framebuffer
        // edge). Half the extent is the radius of the inscribed ball.
        let radius = extent * 0.5 * 1.2;
        let dist = radius / (fov_y * 0.5).tan();
        let eye = [cx, cy, cz + dist];
        let target = [cx, cy, cz];
        let up = [0.0, 1.0, 0.0];
        let view = look_at(eye, target, up);
        let near = (dist - extent).max(extent * 0.01);
        let far = dist + extent * 2.0;
        let proj = perspective(fov_y, aspect, near, far);
        Self { view, proj }
    }
}

fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [[f32; 4]; 4] {
    let f = vec3_normalise(vec3_sub(target, eye));
    let s = vec3_normalise(vec3_cross(f, up));
    let u = vec3_cross(s, f);
    [
        [s[0], s[1], s[2], -vec3_dot(s, eye)],
        [u[0], u[1], u[2], -vec3_dot(u, eye)],
        [-f[0], -f[1], -f[2], vec3_dot(f, eye)],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> [[f32; 4]; 4] {
    let f = 1.0 / (fov_y * 0.5).tan();
    let nf = 1.0 / (near - far);
    [
        [f / aspect, 0.0, 0.0, 0.0],
        [0.0, f, 0.0, 0.0],
        [0.0, 0.0, (far + near) * nf, 2.0 * far * near * nf],
        [0.0, 0.0, -1.0, 0.0],
    ]
}

fn vec3_sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn vec3_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn vec3_cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn vec3_normalise(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < f32::EPSILON {
        [0.0, 0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

// ---------------------------------------------------------------------
// Scene bbox walk.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct BBox {
    min: [f32; 3],
    max: [f32; 3],
}

impl BBox {
    fn empty() -> Self {
        Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }
    fn extend(&mut self, p: [f32; 3]) {
        for (i, &v) in p.iter().enumerate() {
            if v < self.min[i] {
                self.min[i] = v;
            }
            if v > self.max[i] {
                self.max[i] = v;
            }
        }
    }
    fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }
}

fn scene_bbox(scene: &Scene3D) -> BBox {
    let mut bbox = BBox::empty();
    for &root in &scene.roots {
        accumulate_node_bbox(scene, root, identity4(), &mut bbox);
    }
    if bbox.is_empty() {
        // Centre on origin with a unit half-extent so the camera math
        // stays finite.
        BBox {
            min: [-0.5, -0.5, -0.5],
            max: [0.5, 0.5, 0.5],
        }
    } else {
        bbox
    }
}

fn accumulate_node_bbox(scene: &Scene3D, id: NodeId, parent_world: [[f32; 4]; 4], bbox: &mut BBox) {
    let Some(node) = scene.nodes.get(id.0 as usize) else {
        return;
    };
    let world = mat4_mul(parent_world, node.transform.to_matrix());
    if let Some(mesh_id) = node.mesh {
        if let Some(mesh) = scene.meshes.get(mesh_id.0 as usize) {
            for prim in &mesh.primitives {
                for p in &prim.positions {
                    let v = mat4_mul_vec4(&world, [p[0], p[1], p[2], 1.0]);
                    if v[3].abs() > f32::EPSILON {
                        bbox.extend([v[0] / v[3], v[1] / v[3], v[2] / v[3]]);
                    }
                }
            }
        }
    }
    // Visit children with the composed world transform. Re-fetch
    // children via the scene rather than holding a borrow into `node`
    // so the recursive call doesn't conflict with the immutable scene
    // borrow above.
    let children: Vec<NodeId> = scene
        .nodes
        .get(id.0 as usize)
        .map(|n: &Node| n.children.clone())
        .unwrap_or_default();
    for child in children {
        accumulate_node_bbox(scene, child, world, bbox);
    }
}

// ---------------------------------------------------------------------
// Framebuffer.
// ---------------------------------------------------------------------

struct Framebuffer {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    zbuf: Vec<f32>,
}

impl Framebuffer {
    fn new(width: u32, height: u32, background: [u8; 4]) -> Self {
        let n = (width as usize) * (height as usize);
        let mut rgba = Vec::with_capacity(n * 4);
        for _ in 0..n {
            rgba.extend_from_slice(&background);
        }
        Self {
            width,
            height,
            rgba,
            zbuf: vec![f32::INFINITY; n],
        }
    }

    fn set_pixel(&mut self, x: i32, y: i32, z: f32, colour: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = (y as usize) * (self.width as usize) + (x as usize);
        if z < self.zbuf[idx] {
            self.zbuf[idx] = z;
            let p = idx * 4;
            self.rgba[p] = colour[0];
            self.rgba[p + 1] = colour[1];
            self.rgba[p + 2] = colour[2];
            self.rgba[p + 3] = colour[3];
        }
    }

    fn into_image(self) -> RgbaImage {
        RgbaImage {
            width: self.width,
            height: self.height,
            stride: (self.width as usize) * 4,
            pixels: self.rgba,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_mesh3d::{Mesh, MeshId, Node as MNode, Primitive as MPrimitive, Scene3D, Topology};

    fn unit_triangle_scene() -> Scene3D {
        let mut prim = MPrimitive::new(Topology::Triangles);
        prim.positions = vec![[-0.5, -0.5, 0.0], [0.5, -0.5, 0.0], [0.0, 0.5, 0.0]];
        let mesh = Mesh::new("triangle".to_string()).with_primitive(prim);
        let mut scene = Scene3D::new();
        scene.meshes.push(mesh);
        let node = MNode {
            mesh: Some(MeshId(0)),
            ..MNode::default()
        };
        scene.nodes.push(node);
        scene.roots.push(oxideav_mesh3d::NodeId(0));
        scene
    }

    #[test]
    fn renders_triangle_changes_some_pixels() {
        let scene = unit_triangle_scene();
        let img = render_scene(&scene, 64, 64, [255, 255, 255, 255], Mesh3DRenderMode::Flat);
        assert_eq!(img.width, 64);
        assert_eq!(img.height, 64);
        assert_eq!(img.pixels.len(), 64 * 64 * 4);
        // Some pixels should differ from the background — the triangle
        // is large enough to land inside the viewport.
        let drawn = img
            .pixels
            .chunks_exact(4)
            .any(|p| p != [255, 255, 255, 255]);
        assert!(drawn, "expected at least one rasterised pixel");
    }

    #[test]
    fn wireframe_mode_paints_triangle_edges() {
        let scene = unit_triangle_scene();
        let img = render_scene(&scene, 32, 32, [0, 0, 0, 255], Mesh3DRenderMode::Wireframe);
        // At least one non-background pixel.
        let drawn = img.pixels.chunks_exact(4).any(|p| p != [0, 0, 0, 255]);
        assert!(drawn, "wireframe should paint at least one edge pixel");
    }

    #[test]
    fn empty_scene_yields_pure_background() {
        let scene = Scene3D::new();
        let img = render_scene(&scene, 8, 8, [42, 7, 99, 255], Mesh3DRenderMode::Flat);
        for px in img.pixels.chunks_exact(4) {
            assert_eq!(px, &[42, 7, 99, 255]);
        }
    }

    #[test]
    fn pick_dims_falls_back_to_default() {
        let (w, h) = pick_dims(&[]);
        assert_eq!((w, h), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn pick_dims_honours_resize_op() {
        let ops = vec![Op::Resize {
            width: 320,
            height: 240,
            mode: crate::op::ResizeMode::Default,
        }];
        let (w, h) = pick_dims(&ops);
        assert_eq!((w, h), (320, 240));
    }

    #[test]
    fn linear_to_srgb_handles_endpoints() {
        assert_eq!(linear_to_srgb_u8(0.0), 0);
        assert_eq!(linear_to_srgb_u8(1.0), 255);
    }
}
