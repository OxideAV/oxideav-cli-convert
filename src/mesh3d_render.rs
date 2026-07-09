//! 3D scene → raster image.
//!
//! When `convert` sees a 3D-asset input (`.stl`/`.obj`/`.gltf`/`.glb`/
//! `.usdz`/`.fbx`) paired with a raster output (`.png`/`.jpg`/`.bmp`/
//! `.webp`), the [`mesh3d_runner`](crate::mesh3d_runner) refuses
//! (encoders only cover same-class round-trips). This module bridges
//! the gap: decode the input through the same [`Mesh3DRegistry`]
//! populated by [`oxideav_meta::populate_mesh3d_registry`] (plus our
//! direct FBX wiring — meta 0.0.1 didn't know about FBX yet),
//! rasterise the [`Scene3D`] with the [`oxideav-render`][r] scanline
//! backend, then hand the resulting [`RgbaImage`] to the shared
//! [`raster_io`] encoders.
//!
//! The rasteriser body itself lives in [`oxideav-render`][r] — it
//! migrated out of this file in 2026-06-07 so the renderer lives
//! behind a stable trait surface and can grow new backends
//! (raycaster, path tracer) without churning the CLI crate. This file
//! now exists for the [`Mesh3DOptions`] → [`oxideav_render::RenderOptions`]
//! adapter and the `-resize` / `-bg` / `-background` plumbing that
//! decides what to send the renderer in the first place.
//!
//! [r]: https://docs.rs/oxideav-render
//!
//! ## Renderer shape (lives in `oxideav-render::scanline`)
//!
//! 1. Camera — perspective OR orthographic; auto-framed on the
//!    scene's axis-aligned bounding box at the requested vertical FOV
//!    or placed at a user-supplied `(elevation, azimuth, distance)`
//!    orbit (`-camera`).
//! 2. Triangle rasterisation — half-space edge-function with z-buffer.
//! 3. Shading — `-render flat|wireframe|gouraud|phong|normal-debug|depth-debug`.
//! 4. Lighting — single directional `-light AZIMUTH,ELEVATION,INTENSITY`
//!    + constant ambient.
//! 5. SSAA — `-aa N` (1..=8) supersamples + box-filters.
//!
//! Out of scope (deliberate): texture sampling, PBR, raytraced shadows,
//! scene cameras. All documented as Phase D / E follow-ups on the
//! renderer crate side.

use std::fs;

use oxideav_core::{Error, Result};
use oxideav_mesh3d::Mesh3DRegistry;
use oxideav_render::{
    make_renderer, BackgroundColor, CameraSpec as RCameraSpec, LightSpec as RLightSpec, Projection,
    RenderBackend, RenderOptions, RgbaImage as RRgbaImage, ShadingMode,
};

use crate::op::{Mesh3DOptions, Mesh3DRenderMode, Op, ProjectionMode};
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

/// Default `-bg` background fill: transparent black so the canvas
/// composites cleanly against any downstream `-alpha remove`.
const DEFAULT_BG: [u8; 4] = [0, 0, 0, 0];

/// Default vertical field of view in degrees for perspective projection.
const DEFAULT_FOV_DEG: f32 = 60.0;

/// Run the 3D→raster convert flow. Side-effect-only: writes one file
/// to disk.
///
/// Caller has already established that:
///
/// * `input_path`'s extension is in [`crate::mesh3d_runner::is_mesh3d_input`].
/// * `output_path`'s extension is a raster format ([`OutputClass::Raster`]).
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
    crate::mesh3d_runner::populate_registry(&mut registry);
    let mut decoder = registry.decoder_for_extension(&in_ext).ok_or_else(|| {
        Error::unsupported(format!(
            "convert: no 3D decoder registered for input extension '.{in_ext}'"
        ))
    })?;

    let bytes = fs::read(input_path)
        .map_err(|e| Error::invalid(format!("convert: failed to read {input_path}: {e}")))?;
    let scene = decoder.decode(&bytes)?;

    let (width, height) = pick_dims(ops);
    let bg = pick_render_bg(ops, options);

    let render_opts = to_render_options(options, width, height, bg);
    let mut renderer = make_renderer(RenderBackend::Scanline)
        .map_err(|e| Error::invalid(format!("convert: 3D-render: {e}")))?;
    let rendered: RRgbaImage = renderer
        .render(&scene, &render_opts)
        .map_err(|e| Error::invalid(format!("convert: 3D-render: {e}")))?;
    let mut img = into_local_image(rendered);

    img = apply_pixel_transform_chain(img, ops)
        .map_err(|e| Error::invalid(format!("convert: 3D-render: {e}")))?;
    apply_alpha_ops(&mut img, ops, bg);

    encode_raster_to_path(&img, fmt, output_path, ops)
}

/// Translate the cli-convert option struct + framing inputs into the
/// framework-wide [`RenderOptions`] consumed by
/// [`oxideav_render::make_renderer`]. `None` fields fall back to the
/// renderer's documented defaults.
fn to_render_options(
    options: &Mesh3DOptions,
    width: u32,
    height: u32,
    bg: [u8; 4],
) -> RenderOptions {
    let shading = options
        .render_mode
        .map(shading_from_mode)
        .unwrap_or(ShadingMode::Flat);
    let projection = options
        .projection
        .map(projection_from_mode)
        .unwrap_or(Projection::Perspective);
    let fov_deg = options.fov_deg.unwrap_or(DEFAULT_FOV_DEG);
    let light = options
        .light
        .map(|l| RLightSpec {
            azimuth_deg: l.azimuth_deg,
            elevation_deg: l.elevation_deg,
            intensity: l.intensity,
        })
        .unwrap_or(RLightSpec::default_light());
    let camera = options.camera.map(|c| RCameraSpec {
        elevation_deg: c.elevation_deg,
        azimuth_deg: c.azimuth_deg,
        distance: c.distance,
    });
    let aa = options.aa.unwrap_or(1).clamp(1, 8);
    RenderOptions {
        width,
        height,
        background: BackgroundColor(bg),
        shading,
        projection,
        fov_deg,
        light,
        camera,
        aa,
    }
}

fn shading_from_mode(mode: Mesh3DRenderMode) -> ShadingMode {
    match mode {
        Mesh3DRenderMode::Flat => ShadingMode::Flat,
        Mesh3DRenderMode::Wireframe => ShadingMode::Wireframe,
        Mesh3DRenderMode::Gouraud => ShadingMode::Gouraud,
        Mesh3DRenderMode::Phong => ShadingMode::Phong,
        Mesh3DRenderMode::NormalDebug => ShadingMode::NormalDebug,
        Mesh3DRenderMode::DepthDebug => ShadingMode::DepthDebug,
    }
}

fn projection_from_mode(mode: ProjectionMode) -> Projection {
    match mode {
        ProjectionMode::Perspective => Projection::Perspective,
        ProjectionMode::Orthographic => Projection::Orthographic,
    }
}

/// Move the renderer's owned RGBA buffer into the cli-convert
/// [`RgbaImage`] (same field shape, just a different crate root).
fn into_local_image(src: RRgbaImage) -> RgbaImage {
    RgbaImage {
        width: src.width,
        height: src.height,
        stride: src.stride,
        pixels: src.pixels,
    }
}

fn pick_dims(ops: &[Op]) -> (u32, u32) {
    // `-resize WxH` / `-thumbnail WxH` (any mode) seeds the canvas
    // size; otherwise 1024x1024. Force-mode resizes still get the
    // request honoured exactly. The post-render
    // `apply_pixel_transform_chain` would also handle this for us
    // (its Resize / Thumbnail arms then re-apply as an identity
    // pass), but rasterising at the final size up front avoids a
    // downscale step and keeps anti-aliasing native to the raster
    // grid.
    for op in ops.iter().rev() {
        if let Op::Resize { width, height, .. } | Op::Thumbnail { width, height, .. } = op {
            return (*width, *height);
        }
    }
    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
}

/// Pick the render-canvas background. `-bg` (Mesh3DOptions::bg) wins
/// over `-background` (Op::Background) so users can keep the IM
/// canvas-fill semantics separate from the renderer's clear colour.
fn pick_render_bg(ops: &[Op], options: &Mesh3DOptions) -> [u8; 4] {
    if let Some(bg) = options.bg {
        return bg;
    }
    for op in ops.iter().rev() {
        if let Op::Background(c) = op {
            return *c;
        }
    }
    DEFAULT_BG
}

fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{CameraSpec, LightSpec, ResizeMode};

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
            mode: ResizeMode::Default,
        }];
        let (w, h) = pick_dims(&ops);
        assert_eq!((w, h), (320, 240));
    }

    #[test]
    fn render_bg_default_is_transparent() {
        let bg = pick_render_bg(&[], &Mesh3DOptions::default());
        assert_eq!(bg, [0, 0, 0, 0]);
    }

    #[test]
    fn render_bg_honours_options_first() {
        let opts = Mesh3DOptions {
            bg: Some([10, 20, 30, 200]),
            ..Mesh3DOptions::default()
        };
        let ops = vec![Op::Background([99, 99, 99, 255])];
        // -bg wins over -background.
        let bg = pick_render_bg(&ops, &opts);
        assert_eq!(bg, [10, 20, 30, 200]);
    }

    #[test]
    fn render_bg_falls_back_to_background_op() {
        let opts = Mesh3DOptions::default();
        let ops = vec![Op::Background([7, 8, 9, 255])];
        let bg = pick_render_bg(&ops, &opts);
        assert_eq!(bg, [7, 8, 9, 255]);
    }

    #[test]
    fn shading_mode_round_trips_every_variant() {
        // Every Mesh3DRenderMode must map onto a distinct ShadingMode.
        let modes = [
            Mesh3DRenderMode::Flat,
            Mesh3DRenderMode::Wireframe,
            Mesh3DRenderMode::Gouraud,
            Mesh3DRenderMode::Phong,
            Mesh3DRenderMode::NormalDebug,
            Mesh3DRenderMode::DepthDebug,
        ];
        let expected = [
            ShadingMode::Flat,
            ShadingMode::Wireframe,
            ShadingMode::Gouraud,
            ShadingMode::Phong,
            ShadingMode::NormalDebug,
            ShadingMode::DepthDebug,
        ];
        for (m, e) in modes.iter().zip(expected.iter()) {
            assert_eq!(shading_from_mode(*m), *e);
        }
    }

    #[test]
    fn projection_round_trips() {
        assert_eq!(
            projection_from_mode(ProjectionMode::Perspective),
            Projection::Perspective
        );
        assert_eq!(
            projection_from_mode(ProjectionMode::Orthographic),
            Projection::Orthographic
        );
    }

    #[test]
    fn to_render_options_carries_dims_bg_and_aa() {
        let opts = Mesh3DOptions {
            aa: Some(4),
            ..Mesh3DOptions::default()
        };
        let r = to_render_options(&opts, 320, 240, [10, 20, 30, 255]);
        assert_eq!(r.width, 320);
        assert_eq!(r.height, 240);
        assert_eq!(r.background, BackgroundColor([10, 20, 30, 255]));
        assert_eq!(r.aa, 4);
        // Defaults flow through the options that the caller didn't set.
        assert_eq!(r.shading, ShadingMode::Flat);
        assert_eq!(r.projection, Projection::Perspective);
        assert!((r.fov_deg - DEFAULT_FOV_DEG).abs() < 1e-6);
        assert!(r.camera.is_none());
    }

    #[test]
    fn to_render_options_propagates_camera_override() {
        let opts = Mesh3DOptions {
            camera: Some(CameraSpec {
                elevation_deg: 30.0,
                azimuth_deg: 45.0,
                distance: 1.5,
            }),
            ..Mesh3DOptions::default()
        };
        let r = to_render_options(&opts, 64, 64, [0, 0, 0, 255]);
        let cam = r.camera.expect("camera override must propagate");
        assert!((cam.elevation_deg - 30.0).abs() < 1e-6);
        assert!((cam.azimuth_deg - 45.0).abs() < 1e-6);
        assert!((cam.distance - 1.5).abs() < 1e-6);
    }

    #[test]
    fn to_render_options_propagates_light_override() {
        let opts = Mesh3DOptions {
            light: Some(LightSpec {
                azimuth_deg: 12.0,
                elevation_deg: 34.0,
                intensity: 0.5,
            }),
            ..Mesh3DOptions::default()
        };
        let r = to_render_options(&opts, 64, 64, [0, 0, 0, 255]);
        assert!((r.light.azimuth_deg - 12.0).abs() < 1e-6);
        assert!((r.light.elevation_deg - 34.0).abs() < 1e-6);
        assert!((r.light.intensity - 0.5).abs() < 1e-6);
    }
}
