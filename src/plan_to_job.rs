//! Translate an [`crate::op::ConvertPlan`] into an
//! [`oxideav_pipeline::Job`].
//!
//! The generated job has exactly one output keyed by the plan's
//! output path.  That output owns one or two tracks — an `all`
//! selector that pulls every stream through the filter chain in order.
//! The pipeline executor figures out which tracks actually exist in
//! the input (just a video stream for a PNG, both audio + video for
//! an MP4) and dispatches filters to the matching `FilterKind` at
//! DAG-build time.
//!
//! ## 3D-input auto-route (Phase C-3b)
//!
//! [`plan_to_render3d_job`] builds a Job whose track input is a
//! [`TrackInput::Render3D`] node — the pipeline-side bridge for the
//! 3D-asset → raster path that today's `mesh3d_render::run` covers via
//! a direct in-process renderer. The companion C-3e step (cli-convert
//! installs a [`oxideav_pipeline::RenderSourceFactory`] on the
//! executor and flips `lib.rs::run` over) consumes this helper. Until
//! that flip lands the existing `mesh3d_render::run` stays the live
//! path; this helper is the plumbing it will switch onto.

use crate::op::{ConvertPlan, Dither, Mesh3DOptions, Mesh3DRenderMode, Op, ProjectionMode};
use indexmap::IndexMap;
use oxideav_core::{Error, RuntimeContext};
use oxideav_pipeline::{
    FilterNode, Job, OutputSpec, Render3DNode, SourceRef, TrackInput, TrackSpec,
};
use serde_json::{json, Value};

/// Default canvas size for the 3D→raster path when no `-resize` flag
/// pins it. Mirrors [`crate::mesh3d_render`]'s `DEFAULT_WIDTH` /
/// `DEFAULT_HEIGHT` so the two routes (direct + pipeline) frame the
/// scene identically.
const RENDER3D_DEFAULT_WIDTH: u32 = 1024;
const RENDER3D_DEFAULT_HEIGHT: u32 = 1024;

/// Default `-bg` background fill when neither `-bg` nor `-background`
/// is set: transparent black. Matches [`crate::mesh3d_render`]'s
/// `DEFAULT_BG`.
const RENDER3D_DEFAULT_BG: [u8; 4] = [0, 0, 0, 0];

/// Backend id baked into [`TrackInput::Render3D::backend`] today. Only
/// `oxideav-render`'s scanline rasteriser exists; Phase D / E will add
/// raycast / pathtrace selectors. Centralised so the C-3e callback
/// dispatches on the same name string.
pub const RENDER3D_DEFAULT_BACKEND: &str = "scanline";

/// Build a [`Job`] from a [`ConvertPlan`].
///
/// `ctx` is consulted via its [`ContainerRegistry`](oxideav_core::ContainerRegistry)
/// to map the output extension to a codec id — sibling crates that
/// register a container (e.g. `oxideav-png`, `oxideav-qoi`,
/// `oxideav-dds`) automatically extend the supported output set without
/// needing a hard-coded entry here.
pub fn plan_to_job(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<Job, Error> {
    // Starting input: the file itself.
    let initial: TrackInput = TrackInput::Source(SourceRef {
        from: plan.input.clone(),
    });
    build_job_from_initial(plan, ctx, initial, &plan.ops)
}

/// Build a [`Job`] whose track input is a [`TrackInput::Render3D`] node
/// — the pipeline-side bridge for the 3D-asset → raster path.
///
/// Phase C-3b: this returns the Job shape the C-3e callback flip will
/// hand to [`oxideav_pipeline::Executor`]. The executor consults the
/// installed [`oxideav_pipeline::RenderSourceFactory`] (set via
/// [`oxideav_pipeline::Executor::with_render_source_factory`]) to
/// materialise the [`TrackInput::Render3D`] leaf into a concrete
/// [`oxideav_core::FrameSource`].
///
/// ## Opts shape
///
/// The `opts` JSON carried on the Render3D node mirrors
/// [`oxideav_render::RenderOptions`] field-by-field so the C-3e
/// callback can deserialise it directly into the renderer's option
/// struct:
///
/// ```json
/// {
///   "width": 1024,
///   "height": 1024,
///   "background": [0, 0, 0, 0],
///   "shading": "flat",          // optional; "flat"|"gouraud"|"phong"|"wireframe"|"normal-debug"|"depth-debug"
///   "projection": "perspective",// optional; "perspective"|"orthographic"
///   "fov_deg": 60.0,            // optional; required for perspective
///   "light": { "azimuth_deg": ..., "elevation_deg": ..., "intensity": ... }, // optional
///   "camera": { "elevation_deg": ..., "azimuth_deg": ..., "distance": ... }, // optional
///   "aa": 1                     // optional; 1..=8
/// }
/// ```
///
/// ## Op absorption
///
/// `Op::Resize { width, height, .. }` and `Op::Background(color)` are
/// absorbed by the renderer (width/height seed the canvas dims; the
/// background pre-fills the framebuffer) and therefore stripped from
/// the post-render filter chain — same policy
/// [`crate::mesh3d_render::run`] applies via `pick_dims` /
/// `pick_render_bg`. `-bg` (carried on [`Mesh3DOptions::bg`]) wins
/// over `-background` to keep the renderer-clear vs. canvas-fill
/// colours separate.
///
/// Every other op flows through the standard filter-chain walker.
///
/// ## When the Render3D variant is unavailable
///
/// The Render3D variant landed in oxideav-pipeline 0.1.10. Callers
/// built against older pipeline patches see a missing variant at
/// compile time; we depend on `oxideav-pipeline = "0.1"` so the
/// version resolved against the workspace always carries it.
pub fn plan_to_render3d_job(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<Job, Error> {
    let (width, height) = pick_render_dims(&plan.ops);
    let bg = pick_render_bg(&plan.ops, &plan.mesh3d_options);
    let opts = build_render3d_opts_json(&plan.mesh3d_options, width, height, bg);

    let initial = TrackInput::Render3D(Render3DNode {
        source: plan.input.clone(),
        backend: RENDER3D_DEFAULT_BACKEND.to_string(),
        opts,
    });

    // The renderer absorbs `-resize` (canvas seed) and `-background`
    // (framebuffer clear). Drop them from the post-render filter chain
    // so the pipeline doesn't double-apply them.
    let remaining_ops: Vec<Op> = plan
        .ops
        .iter()
        .filter(|op| !is_op_absorbed_by_renderer(op))
        .cloned()
        .collect();

    build_job_from_initial(plan, ctx, initial, &remaining_ops)
}

/// Return `true` for ops whose effect is already applied by the 3D
/// renderer (canvas seed / framebuffer clear). Stripped from the
/// post-render filter chain to avoid double-application.
fn is_op_absorbed_by_renderer(op: &Op) -> bool {
    matches!(op, Op::Resize { .. } | Op::Background(_))
}

/// Pick render-canvas dimensions from `-resize WxH` (any mode); falls
/// back to [`RENDER3D_DEFAULT_WIDTH`] × [`RENDER3D_DEFAULT_HEIGHT`].
/// Mirrors [`crate::mesh3d_render`]'s `pick_dims`.
fn pick_render_dims(ops: &[Op]) -> (u32, u32) {
    for op in ops.iter().rev() {
        if let Op::Resize { width, height, .. } = op {
            return (*width, *height);
        }
    }
    (RENDER3D_DEFAULT_WIDTH, RENDER3D_DEFAULT_HEIGHT)
}

/// Pick the framebuffer clear colour: `-bg` (Mesh3DOptions::bg) wins,
/// then the most recent `-background` op, then transparent black.
/// Mirrors [`crate::mesh3d_render`]'s `pick_render_bg`.
fn pick_render_bg(ops: &[Op], options: &Mesh3DOptions) -> [u8; 4] {
    if let Some(bg) = options.bg {
        return bg;
    }
    for op in ops.iter().rev() {
        if let Op::Background(c) = op {
            return *c;
        }
    }
    RENDER3D_DEFAULT_BG
}

/// Translate [`Mesh3DOptions`] + canvas dims + bg into the opaque
/// `opts` JSON value carried on [`TrackInput::Render3D`]. Field order
/// and key names mirror [`oxideav_render::RenderOptions`] so the C-3e
/// callback can deserialise it directly.
///
/// Optional fields are emitted only when the user set them via a
/// per-format flag — letting the C-3e callback fall back to the
/// renderer's own defaults when a key is absent rather than forcing
/// us to hard-code those defaults here.
fn build_render3d_opts_json(
    options: &Mesh3DOptions,
    width: u32,
    height: u32,
    bg: [u8; 4],
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("width".to_string(), json!(width));
    obj.insert("height".to_string(), json!(height));
    obj.insert(
        "background".to_string(),
        json!([bg[0], bg[1], bg[2], bg[3]]),
    );
    if let Some(mode) = options.render_mode {
        obj.insert("shading".to_string(), json!(shading_tag(mode)));
    }
    if let Some(proj) = options.projection {
        obj.insert("projection".to_string(), json!(projection_tag(proj)));
    }
    if let Some(fov) = options.fov_deg {
        obj.insert("fov_deg".to_string(), json!(fov));
    }
    if let Some(light) = options.light {
        obj.insert(
            "light".to_string(),
            json!({
                "azimuth_deg": light.azimuth_deg,
                "elevation_deg": light.elevation_deg,
                "intensity": light.intensity,
            }),
        );
    }
    if let Some(cam) = options.camera {
        obj.insert(
            "camera".to_string(),
            json!({
                "elevation_deg": cam.elevation_deg,
                "azimuth_deg": cam.azimuth_deg,
                "distance": cam.distance,
            }),
        );
    }
    if let Some(aa) = options.aa {
        obj.insert("aa".to_string(), json!(aa.clamp(1, 8)));
    }
    Value::Object(obj)
}

/// Lowercase tag mirroring `oxideav_render::ShadingMode`'s wire form.
/// Hyphenated forms (`normal-debug`, `depth-debug`) match the IM-style
/// `-render` arg and what `oxideav_render` parses on its side.
fn shading_tag(mode: Mesh3DRenderMode) -> &'static str {
    match mode {
        Mesh3DRenderMode::Flat => "flat",
        Mesh3DRenderMode::Wireframe => "wireframe",
        Mesh3DRenderMode::Gouraud => "gouraud",
        Mesh3DRenderMode::Phong => "phong",
        Mesh3DRenderMode::NormalDebug => "normal-debug",
        Mesh3DRenderMode::DepthDebug => "depth-debug",
    }
}

/// Lowercase tag mirroring `oxideav_render::Projection`'s wire form.
fn projection_tag(mode: ProjectionMode) -> &'static str {
    match mode {
        ProjectionMode::Perspective => "perspective",
        ProjectionMode::Orthographic => "orthographic",
    }
}

/// Shared back-end for [`plan_to_job`] + [`plan_to_render3d_job`]:
/// walk the op list, fold each op into the recursive `TrackInput`
/// chain (or into sink-side metadata), wire the codec, and assemble
/// the one-output Job.
///
/// The two public entry points differ only in what `initial` they
/// pass: the file-source variant for the regular path, the Render3D
/// variant for the 3D auto-route. Op semantics + sink-side rules are
/// identical from then on.
fn build_job_from_initial(
    plan: &ConvertPlan,
    ctx: &RuntimeContext,
    initial: TrackInput,
    ops: &[Op],
) -> Result<Job, Error> {
    // Walk ops, distinguishing track-side filters from sink-side
    // metadata (format / quality / strip).  Filters are flattened
    // into the recursive `TrackInput` chain; the rest are deferred
    // to the sink.
    let mut codec_params = json!({});
    let mut strip_metadata = false;
    let mut format_override: Option<String> = None;

    let mut chain: TrackInput = initial;

    // A filter-chain step — wrap the current chain in a FilterNode.
    let wrap = |prev: TrackInput, filter: &str, params: Value| {
        TrackInput::Filter(FilterNode {
            filter: filter.to_string(),
            params,
            input: Box::new(prev),
        })
    };

    for op in ops {
        match op {
            Op::Resize {
                width,
                height,
                mode,
            } => {
                // The pipeline's resize factory takes literal target
                // dims today. We forward `width`/`height` along with
                // the geometry mode tag so a future executor pass can
                // resolve the source-aware variants (Fill/Shrink/Grow/
                // Percent/Area) against the actual frame size at
                // DAG-build time. Until that lands, only `Default` and
                // `Force` are pixel-accurate on the pipeline path —
                // the source-aware modes degrade to `Default` semantics
                // when the executor sees a mode it doesn't understand.
                // The PDF side-channel (which already knows the source
                // dims) honours every mode today.
                chain = wrap(
                    chain,
                    "video.resize",
                    json!({
                        "width": width,
                        "height": height,
                        "interpolation": "bilinear",
                        "mode": mode.as_tag(),
                    }),
                );
            }
            Op::Thumbnail {
                width,
                height,
                mode,
            } => {
                // `-thumbnail` is sugar for `Resize + Strip` (and, on a
                // future pass, EXIF auto-orient). Emit both downstream
                // ops so the executor sees the same shape as a
                // hand-written `-resize ... -strip`.
                chain = wrap(
                    chain,
                    "video.resize",
                    json!({
                        "width": width,
                        "height": height,
                        "interpolation": "bilinear",
                        "mode": mode.as_tag(),
                    }),
                );
                strip_metadata = true;
            }
            Op::Define { key, value } => {
                // Forward the literal key (preserving any `:` namespace
                // separator) onto the codec params bag. Values that
                // parse as integers / floats / booleans still come
                // through as JSON strings — codecs that care about
                // type can re-parse from the string. Bare `-define KEY`
                // (no `=VALUE`) becomes `{"KEY": true}`.
                match value {
                    Some(v) => codec_params[key.clone()] = json!(v),
                    None => codec_params[key.clone()] = json!(true),
                }
            }
            Op::Blur { radius, sigma } => {
                chain = wrap(
                    chain,
                    "video.blur",
                    json!({
                        "radius": radius,
                        "sigma": sigma,
                        "planes": "all"
                    }),
                );
            }
            Op::Edge { radius } => {
                chain = wrap(chain, "video.edge", json!({ "radius": radius }));
            }
            Op::Colors { count, dither } => {
                // Palette quantisation: rely on the pipeline's
                // ConvertNode (pixfmt) which already supports
                // `pal8` + dither via `oxideav-pixfmt`.  We nest
                // the palette step inline as a filter-shaped node
                // that the executor recognises by name.
                let dither_str = match dither {
                    Dither::None => "none",
                    Dither::Bayer => "bayer",
                    Dither::FloydSteinberg => "floyd_steinberg",
                };
                chain = wrap(
                    chain,
                    "video.pixfmt",
                    json!({
                        "format": "pal8",
                        "dither": dither_str,
                        "colors": count,
                    }),
                );
            }
            Op::Format(fmt) => format_override = Some(fmt.clone()),
            Op::Quality(q) => {
                // Codec-specific key; the encoder drops it when
                // unsupported.  Use `quality` for JPEG/WebP, `crf`
                // could be added by a later pass for h264/h265.
                codec_params["quality"] = json!(q);
            }
            Op::Strip => strip_metadata = true,
            // Vector-input ops; silently dropped on the raster
            // pipeline path (raster inputs have no DPI, no
            // composite-over-bg semantics that wouldn't conflict
            // with `-resize`/etc., and no alpha-channel grammar to
            // honour beyond what the encoder already does). The
            // pdf_runner side-channel applies them when reading PDFs.
            Op::Density(_) | Op::Background(_) | Op::Alpha(_) => {}
            // Round-1 inline geometry / negate ops. The PDF
            // side-channel applies these via crate::pixel_xform; the
            // generic pipeline path now also wires them through to
            // the matching `oxideav-image-filter` factory so non-PDF
            // input paths (PNG/JPG/MP4/…) honour them too.
            Op::Rotate { degrees } => {
                chain = wrap(chain, "video.rotate", json!({ "degrees": degrees }));
            }
            Op::Flip => {
                chain = wrap(chain, "video.flip", json!({}));
            }
            Op::Flop => {
                chain = wrap(chain, "video.flop", json!({}));
            }
            Op::Crop { x, y, w, h } => {
                chain = wrap(
                    chain,
                    "video.crop",
                    json!({ "x": x, "y": y, "width": w, "height": h }),
                );
            }
            Op::Extent {
                width,
                height,
                x,
                y,
                bg,
            } => {
                // The `oxideav-image-filter` `Extent` factory accepts
                // `width`/`height` plus optional signed `offset_x` /
                // `offset_y` and an RGBA `background` array. We forward
                // everything verbatim — the background was already
                // resolved against the source-order `-background` ops
                // at args-parse time so the plan walker stays
                // stateless.
                chain = wrap(
                    chain,
                    "video.extent",
                    json!({
                        "width": width,
                        "height": height,
                        "offset_x": x,
                        "offset_y": y,
                        "background": [bg[0], bg[1], bg[2], bg[3]],
                    }),
                );
            }
            Op::Negate => {
                chain = wrap(chain, "video.negate", json!({}));
            }
            // ---- Round-next: tonal / colour-grading ops wired to
            // the matching `oxideav-image-filter` factory. ----
            Op::Sharpen { radius, sigma } => {
                chain = wrap(
                    chain,
                    "video.sharpen",
                    json!({ "radius": radius, "sigma": sigma }),
                );
            }
            Op::Unsharp {
                radius,
                sigma,
                amount,
                threshold,
            } => {
                chain = wrap(
                    chain,
                    "video.unsharp",
                    json!({
                        "radius": radius,
                        "sigma": sigma,
                        "amount": amount,
                        "threshold": threshold,
                    }),
                );
            }
            Op::Gamma { value } => {
                chain = wrap(chain, "video.gamma", json!({ "value": value }));
            }
            Op::BrightnessContrast {
                brightness,
                contrast,
            } => {
                chain = wrap(
                    chain,
                    "video.brightness-contrast",
                    json!({ "brightness": brightness, "contrast": contrast }),
                );
            }
            Op::Contrast { delta } => {
                // IM's bare `-contrast` flag bumps contrast by a single
                // 5%-of-range step. Multiple `-contrast` accumulate
                // linearly. Map to the brightness-contrast factory's
                // contrast-only knob so the executor can fold this into
                // a single LUT pass alongside any explicit
                // `-brightness-contrast` already in the chain.
                let pct = (*delta as f32) * 5.0;
                chain = wrap(chain, "video.contrast", json!({ "value": pct }));
            }
            Op::Sepia { threshold } => {
                chain = wrap(chain, "video.sepia", json!({ "threshold": threshold }));
            }
            Op::Modulate {
                brightness,
                saturation,
                hue,
            } => {
                // IM's hue is "percent-of-base around 100" — 0 is
                // -180°, 100 is identity, 200 is +180°. The
                // image-filter factory accepts degrees directly via
                // `hue_degrees`, so translate.
                let hue_degrees = (hue - 100.0) * 1.8;
                chain = wrap(
                    chain,
                    "video.modulate",
                    json!({
                        "brightness": brightness,
                        "saturation": saturation,
                        "hue_degrees": hue_degrees,
                    }),
                );
            }
            Op::Level {
                black,
                gamma,
                white,
            } => {
                chain = wrap(
                    chain,
                    "video.level",
                    json!({ "black": black, "gamma": gamma, "white": white }),
                );
            }
            Op::Normalize => {
                chain = wrap(chain, "video.normalize", json!({}));
            }
            Op::Threshold { value } => {
                chain = wrap(chain, "video.threshold", json!({ "value": value }));
            }
            Op::Posterize { levels } => {
                chain = wrap(chain, "video.posterize", json!({ "levels": levels }));
            }
            Op::Solarize { value } => {
                chain = wrap(chain, "video.solarize", json!({ "value": value }));
            }
            Op::Colorspace(cs) => {
                let lower = cs.to_ascii_lowercase();
                if lower == "gray" || lower == "grey" {
                    chain = wrap(chain, "video.grayscale", json!({ "preserve_alpha": true }));
                }
                // `rgb` / `srgb` are recorded no-ops — input keeps its
                // colourspace, downstream encoder converts as needed.
            }
            // ---- Round-after-next: vignette / colorize / equalize /
            // auto-gamma. JSON shape mirrors the image-filter factories
            // (resolves to identity defaults when the value is absent). ----
            Op::Vignette {
                radius,
                sigma,
                x,
                y,
            } => {
                chain = wrap(
                    chain,
                    "video.vignette",
                    json!({ "x": x, "y": y, "radius": radius, "sigma": sigma }),
                );
            }
            Op::Colorize { color, amount } => {
                // Factory wants a 4-element JSON array for `color`.
                chain = wrap(
                    chain,
                    "video.colorize",
                    json!({
                        "color": [color[0], color[1], color[2], color[3]],
                        "amount": amount,
                    }),
                );
            }
            Op::Equalize => {
                chain = wrap(chain, "video.equalize", json!({}));
            }
            Op::AutoGamma => {
                chain = wrap(chain, "video.auto-gamma", json!({}));
            }
            Op::Trim { fuzz } => {
                // The image-filter `Trim` factory accepts `fuzz` as a
                // 0..=255 byte plus an optional `background` array. We
                // forward only `fuzz` here — the args parser doesn't
                // expose a per-`-trim` colour override yet, so the
                // factory falls back to its corner-pixel auto-detection
                // (matching IM's behaviour when neither `-bordercolor`
                // nor `-background` is set).
                chain = wrap(chain, "video.trim", json!({ "fuzz": fuzz }));
            }
            Op::Roll { dx, dy } => {
                // The `oxideav-image-filter` `Roll` factory accepts
                // signed `dx`/`dy` (also as `x`/`y` aliases); pixels
                // that fall off one edge wrap around to the opposite
                // edge so the visible image translates as a rigid
                // block. Width/height stay unchanged, so no shape
                // recovery is needed downstream.
                chain = wrap(chain, "video.roll", json!({ "dx": dx, "dy": dy }));
            }
        }
    }

    // Sink-side metadata handling.
    if strip_metadata {
        codec_params["strip_metadata"] = json!(true);
    }
    if let Some(ref f) = format_override {
        codec_params["format"] = json!(f);
    }

    // The pipeline insists every filter-terminated track carry an
    // output codec — frames can't be stream-copied.  Infer from the
    // output extension (or `-format` override) so the IM-style
    // `convert in.png -resize 64x64 out.jpg` works without the user
    // stating the obvious.
    let codec = codec_for_output(format_override.as_deref(), &plan.output, ctx);

    let track = TrackSpec {
        input: chain,
        codec,
        params: codec_params,
        stream_selector: None,
    };

    let mut outputs = IndexMap::new();
    outputs.insert(
        plan.output.clone(),
        OutputSpec {
            audio: vec![],
            video: vec![],
            subtitle: vec![],
            all: vec![track],
        },
    );

    Ok(Job {
        outputs,
        aliases: IndexMap::new(),
        threads: None,
    })
}

/// Guess the output codec from the extension, honouring `-format`
/// override.
///
/// Resolution order:
///
/// 1. Consult [`ContainerRegistry::container_for_extension`]
///    (`oxideav_core::ContainerRegistry`). For image-format crates the
///    container name doubles as the codec id (e.g. `png` → `png`,
///    `qoi` → `qoi`, `dds` → `dds`), so a registered extension Just
///    Works without any hard-coded knowledge here. New sibling crates
///    extend the supported set the moment the umbrella registers them.
/// 2. Otherwise return `None` and let the pipeline infer per-track
///    (the right answer for video containers like MP4/MKV that don't
///    pin a single codec, and the prompt to add `-format CODEC` for
///    extensions nobody has registered yet).
fn codec_for_output(
    format_override: Option<&str>,
    output: &str,
    ctx: &RuntimeContext,
) -> Option<String> {
    let ext = format_override
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| ext_of(output).map(|s| s.to_ascii_lowercase()))?;
    ctx.containers
        .container_for_extension(&ext)
        .map(|s| s.to_string())
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
    use crate::op::ResizeMode;

    fn plan_with(ops: Vec<Op>) -> ConvertPlan {
        ConvertPlan {
            input: "in.png".into(),
            input_pages: None,
            ops,
            output: "out.jpg".into(),
            output_template: None,
            ping: false,
            probe: false,
            probe_json: false,
            probe_watch: false,
            mesh3d_options: Default::default(),
        }
    }

    /// Empty context — every extension resolves to `None` since no
    /// crates have registered any extension/codec pairs yet.
    fn empty_ctx() -> RuntimeContext {
        RuntimeContext::new()
    }

    #[test]
    fn empty_plan_is_passthrough() {
        let job = plan_to_job(&plan_with(vec![]), &empty_ctx()).unwrap();
        assert_eq!(job.outputs.len(), 1);
        let (key, out) = job.outputs.iter().next().unwrap();
        assert_eq!(key, "out.jpg");
        assert_eq!(out.all.len(), 1);
        match &out.all[0].input {
            TrackInput::Source(s) => assert_eq!(s.from, "in.png"),
            other => panic!("expected direct source, got {other:?}"),
        }
    }

    #[test]
    fn resize_produces_filter_wrapper() {
        let job = plan_to_job(
            &plan_with(vec![Op::Resize {
                width: 64,
                height: 32,
                mode: ResizeMode::Default,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let out = job.outputs.values().next().unwrap();
        match &out.all[0].input {
            TrackInput::Filter(f) => {
                // `video.` prefix — the pipeline's dag uses it to
                // route to FilterKind::Video.
                assert_eq!(f.filter, "video.resize");
                assert_eq!(f.params["width"], 64);
                assert_eq!(f.params["height"], 32);
            }
            other => panic!("expected filter, got {other:?}"),
        }
    }

    #[test]
    fn chain_order_preserved() {
        let job = plan_to_job(
            &plan_with(vec![
                Op::Resize {
                    width: 64,
                    height: 64,
                    mode: ResizeMode::Default,
                },
                Op::Blur {
                    radius: 2,
                    sigma: 1.0,
                },
            ]),
            &empty_ctx(),
        )
        .unwrap();
        let out = job.outputs.values().next().unwrap();
        // Ops apply outside-in on the recursive TrackInput: the
        // innermost node is the source; the outermost filter is the
        // last op in source order (blur).
        let outer = match &out.all[0].input {
            TrackInput::Filter(f) => f,
            other => panic!("expected outer filter, got {other:?}"),
        };
        assert_eq!(outer.filter, "video.blur");
        let inner = match outer.input.as_ref() {
            TrackInput::Filter(f) => f,
            other => panic!("expected inner filter, got {other:?}"),
        };
        assert_eq!(inner.filter, "video.resize");
    }

    #[test]
    fn quality_and_strip_land_in_codec_params() {
        let job = plan_to_job(&plan_with(vec![Op::Quality(85), Op::Strip]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.params["quality"], 85);
        assert_eq!(track.params["strip_metadata"], true);
    }

    #[test]
    fn resize_mode_tag_emitted_in_filter_params() {
        // Each ResizeMode round-trips through the JSON params as a
        // stable lowercase tag. Future executor passes can branch off
        // these without us re-shaping the IR.
        for (mode, tag) in [
            (ResizeMode::Default, "default"),
            (ResizeMode::Force, "force"),
            (ResizeMode::Fill, "fill"),
            (ResizeMode::Shrink, "shrink"),
            (ResizeMode::Grow, "grow"),
            (ResizeMode::Percent, "percent"),
            (ResizeMode::Area, "area"),
        ] {
            let job = plan_to_job(
                &plan_with(vec![Op::Resize {
                    width: 100,
                    height: 100,
                    mode,
                }]),
                &empty_ctx(),
            )
            .unwrap();
            let track = &job.outputs.values().next().unwrap().all[0];
            let f = match &track.input {
                TrackInput::Filter(f) => f,
                other => panic!("expected resize filter, got {other:?}"),
            };
            assert_eq!(f.params["mode"], tag, "mode={mode:?}");
        }
    }

    #[test]
    fn thumbnail_unrolls_into_resize_plus_strip() {
        let job = plan_to_job(
            &plan_with(vec![Op::Thumbnail {
                width: 200,
                height: 200,
                mode: ResizeMode::Fill,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        // Resize node present with mode=fill.
        let f = match &track.input {
            TrackInput::Filter(f) => f,
            other => panic!("expected outer resize filter, got {other:?}"),
        };
        assert_eq!(f.filter, "video.resize");
        assert_eq!(f.params["mode"], "fill");
        // And strip_metadata also set on the codec params side.
        assert_eq!(track.params["strip_metadata"], true);
    }

    #[test]
    fn define_with_value_lands_in_codec_params() {
        let job = plan_to_job(
            &plan_with(vec![Op::Define {
                key: "jpeg:dct-method".into(),
                value: Some("float".into()),
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.params["jpeg:dct-method"], "float");
    }

    #[test]
    fn define_bare_key_becomes_json_true() {
        let job = plan_to_job(
            &plan_with(vec![Op::Define {
                key: "png:strip-comments".into(),
                value: None,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.params["png:strip-comments"], true);
    }

    #[test]
    fn multiple_defines_all_preserved() {
        // Ensures sequential `-define` flags don't shadow each other.
        let job = plan_to_job(
            &plan_with(vec![
                Op::Define {
                    key: "jpeg:dct-method".into(),
                    value: Some("float".into()),
                },
                Op::Define {
                    key: "jpeg:optimize-coding".into(),
                    value: Some("true".into()),
                },
                Op::Define {
                    key: "webp:lossless".into(),
                    value: None,
                },
            ]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.params["jpeg:dct-method"], "float");
        assert_eq!(track.params["jpeg:optimize-coding"], "true");
        assert_eq!(track.params["webp:lossless"], true);
    }

    // ---- codec_for_output coverage ----

    #[test]
    fn unknown_extension_returns_none() {
        // Empty context — no registrations, so every lookup is None.
        let ctx = empty_ctx();
        assert!(codec_for_output(None, "out.jpg", &ctx).is_none());
        assert!(codec_for_output(None, "out.mkv", &ctx).is_none());
        assert!(codec_for_output(None, "out.unknown_ext_xyz", &ctx).is_none());
    }

    #[test]
    fn registry_hit_returns_container_name() {
        // Register a synthetic container under an extension and
        // confirm the resolver picks it up. This is the path every
        // image-format crate rides (qoi, dds, exr, icer, pict, jxs,
        // jxl, jp2, png, qoi, …) once their `register_containers`
        // call lands and the umbrella wires them in.
        let mut ctx = RuntimeContext::new();
        ctx.containers.register_extension("foo", "foo_codec");
        assert_eq!(
            codec_for_output(None, "out.foo", &ctx).as_deref(),
            Some("foo_codec")
        );
        // Case-insensitive on the input filename.
        assert_eq!(
            codec_for_output(None, "OUT.FOO", &ctx).as_deref(),
            Some("foo_codec")
        );
    }

    /// Walk the recursive TrackInput chain and collect every filter
    /// node's name in source order (innermost first → applied first).
    fn collect_filter_names(input: &TrackInput) -> Vec<String> {
        let mut names = Vec::new();
        let mut cur = input;
        while let TrackInput::Filter(f) = cur {
            names.push(f.filter.clone());
            cur = f.input.as_ref();
        }
        names.reverse();
        names
    }

    /// Pull out the filter node matching `name` (first match found).
    fn find_filter<'a>(input: &'a TrackInput, name: &str) -> Option<&'a FilterNode> {
        let mut cur = input;
        while let TrackInput::Filter(f) = cur {
            if f.filter == name {
                return Some(f);
            }
            cur = f.input.as_ref();
        }
        None
    }

    #[test]
    fn sharpen_wires_to_video_sharpen_factory() {
        let job = plan_to_job(
            &plan_with(vec![Op::Sharpen {
                radius: 2,
                sigma: 1.0,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.sharpen").expect("video.sharpen node");
        assert_eq!(f.params["radius"], 2);
        assert!((f.params["sigma"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn unsharp_wires_full_params() {
        let job = plan_to_job(
            &plan_with(vec![Op::Unsharp {
                radius: 3,
                sigma: 1.5,
                amount: 0.8,
                threshold: 7,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.unsharp").expect("video.unsharp node");
        assert_eq!(f.params["radius"], 3);
        assert!((f.params["sigma"].as_f64().unwrap() - 1.5).abs() < 1e-6);
        assert!((f.params["amount"].as_f64().unwrap() - 0.8).abs() < 1e-6);
        assert_eq!(f.params["threshold"], 7);
    }

    #[test]
    fn gamma_wires_value_key() {
        let job = plan_to_job(&plan_with(vec![Op::Gamma { value: 2.2 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.gamma").expect("video.gamma node");
        assert!((f.params["value"].as_f64().unwrap() - 2.2).abs() < 1e-6);
    }

    #[test]
    fn brightness_contrast_wires_both_keys() {
        let job = plan_to_job(
            &plan_with(vec![Op::BrightnessContrast {
                brightness: 10.0,
                contrast: -5.0,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.brightness-contrast")
            .expect("video.brightness-contrast node");
        assert!((f.params["brightness"].as_f64().unwrap() - 10.0).abs() < 1e-6);
        assert!((f.params["contrast"].as_f64().unwrap() - (-5.0)).abs() < 1e-6);
    }

    #[test]
    fn contrast_step_translates_to_factor() {
        let job = plan_to_job(&plan_with(vec![Op::Contrast { delta: 1 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.contrast").expect("video.contrast node");
        // delta=1 → 5% contrast bump.
        assert!((f.params["value"].as_f64().unwrap() - 5.0).abs() < 1e-6);
    }

    #[test]
    fn modulate_translates_hue_to_degrees() {
        // IM hue=200 (max) → +180°; hue=0 → -180°; hue=100 → 0°.
        let job = plan_to_job(
            &plan_with(vec![Op::Modulate {
                brightness: 100.0,
                saturation: 100.0,
                hue: 200.0,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.modulate").expect("video.modulate node");
        assert!((f.params["brightness"].as_f64().unwrap() - 100.0).abs() < 1e-6);
        assert!((f.params["saturation"].as_f64().unwrap() - 100.0).abs() < 1e-6);
        assert!((f.params["hue_degrees"].as_f64().unwrap() - 180.0).abs() < 1e-6);
    }

    #[test]
    fn level_wires_all_three_endpoints() {
        let job = plan_to_job(
            &plan_with(vec![Op::Level {
                black: 16,
                gamma: 1.2,
                white: 235,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.level").expect("video.level node");
        assert_eq!(f.params["black"], 16);
        assert!((f.params["gamma"].as_f64().unwrap() - 1.2).abs() < 1e-6);
        assert_eq!(f.params["white"], 235);
    }

    #[test]
    fn normalize_wires_empty_params() {
        let job = plan_to_job(&plan_with(vec![Op::Normalize]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.normalize").expect("video.normalize node");
        // Defaults — empty object.
        assert!(f.params.as_object().unwrap().is_empty());
    }

    #[test]
    fn threshold_wires_value() {
        let job =
            plan_to_job(&plan_with(vec![Op::Threshold { value: 200 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.threshold").expect("video.threshold node");
        assert_eq!(f.params["value"], 200);
    }

    #[test]
    fn posterize_wires_levels() {
        let job = plan_to_job(&plan_with(vec![Op::Posterize { levels: 6 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.posterize").expect("video.posterize node");
        assert_eq!(f.params["levels"], 6);
    }

    #[test]
    fn solarize_wires_value() {
        let job = plan_to_job(&plan_with(vec![Op::Solarize { value: 100 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.solarize").expect("video.solarize node");
        assert_eq!(f.params["value"], 100);
    }

    #[test]
    fn sepia_wires_threshold() {
        let job =
            plan_to_job(&plan_with(vec![Op::Sepia { threshold: 0.8 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.sepia").expect("video.sepia node");
        assert!((f.params["threshold"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn colorspace_gray_wires_grayscale_factory() {
        let job = plan_to_job(
            &plan_with(vec![Op::Colorspace("Gray".into())]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.grayscale").expect("video.grayscale node");
        assert_eq!(f.params["preserve_alpha"], true);
    }

    #[test]
    fn colorspace_rgb_is_pure_passthrough() {
        let job =
            plan_to_job(&plan_with(vec![Op::Colorspace("RGB".into())]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        // No filter wrapping the source — `Op::Colorspace("rgb")` is
        // a recorded no-op.
        match &track.input {
            TrackInput::Source(_) => {}
            other => panic!("expected raw source, got {other:?}"),
        }
    }

    #[test]
    fn rotate_wires_video_rotate_factory() {
        let job = plan_to_job(&plan_with(vec![Op::Rotate { degrees: 90 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.rotate").expect("video.rotate node");
        assert_eq!(f.params["degrees"], 90);
    }

    #[test]
    fn flip_flop_wire_independent_factories() {
        let job = plan_to_job(&plan_with(vec![Op::Flip, Op::Flop]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let names = collect_filter_names(&track.input);
        assert_eq!(
            names,
            vec!["video.flip".to_string(), "video.flop".to_string()]
        );
    }

    #[test]
    fn crop_wires_xywh() {
        let job = plan_to_job(
            &plan_with(vec![Op::Crop {
                x: 1,
                y: 2,
                w: 10,
                h: 20,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.crop").expect("video.crop node");
        assert_eq!(f.params["x"], 1);
        assert_eq!(f.params["y"], 2);
        assert_eq!(f.params["width"], 10);
        assert_eq!(f.params["height"], 20);
    }

    #[test]
    fn negate_wires_video_negate_factory() {
        let job = plan_to_job(&plan_with(vec![Op::Negate]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        find_filter(&track.input, "video.negate").expect("video.negate node");
    }

    /// `Op::Extent` lowers to a `video.extent` FilterNode carrying
    /// width / height / signed offsets / RGBA background — the four
    /// keys the `oxideav-image-filter` `Extent` factory consumes.
    #[test]
    fn extent_wires_width_height_offsets_background() {
        let job = plan_to_job(
            &plan_with(vec![Op::Extent {
                width: 200,
                height: 150,
                x: -10,
                y: 20,
                bg: [12, 34, 56, 200],
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.extent").expect("video.extent node");
        assert_eq!(f.params["width"], 200);
        assert_eq!(f.params["height"], 150);
        assert_eq!(f.params["offset_x"], -10);
        assert_eq!(f.params["offset_y"], 20);
        let bg = f.params["background"].as_array().expect("background array");
        assert_eq!(bg.len(), 4);
        assert_eq!(bg[0], 12);
        assert_eq!(bg[1], 34);
        assert_eq!(bg[2], 56);
        assert_eq!(bg[3], 200);
    }

    /// End-to-end shape check: build a plan with a sharpen op,
    /// translate to a Job, then take the JSON params we emitted and
    /// hand them to the **real** image-filter factory (registered in
    /// the test ctx). The factory must accept them — confirming the
    /// CLI side speaks the same JSON dialect the registry expects.
    ///
    /// NOTE: this verifies the JSON-dialect contract once the new
    /// image-filter factories land in the published registry. The
    /// published 0.1.1 image-filter only registers `blur`, `edge`,
    /// `resize`; the round-next factories (sharpen / gamma / etc.)
    /// are on master and pending publish. We skip the assertion in
    /// the standalone-published path to avoid flapping; inside the
    /// workspace (where image-filter is path-patched to in-tree),
    /// the assertion runs and proves the contract.
    #[test]
    fn sharpen_emitted_params_are_accepted_by_image_filter_registry() {
        use oxideav_core::{PixelFormat, PortSpec, TimeBase};

        let job = plan_to_job(
            &plan_with(vec![Op::Sharpen {
                radius: 1,
                sigma: 0.5,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.sharpen").expect("video.sharpen node");

        // Stand up a real ctx with the image-filter factories wired.
        let mut filter_ctx = RuntimeContext::new();
        oxideav_image_filter::register(&mut filter_ctx);

        // Skip the registry assertion when the build saw an older
        // image-filter that hasn't published the round-next factories
        // yet. The `wrap()`-produced JSON is the same in either case.
        if !filter_ctx.filters.contains("sharpen") {
            return;
        }

        // Pretend we're the executor: ask the registry to build a
        // StreamFilter from the JSON params we just emitted.
        let inputs = [PortSpec::video(
            "in",
            4,
            4,
            PixelFormat::Rgba,
            TimeBase::new(1, 30),
        )];
        filter_ctx
            .filters
            .make("video.sharpen", &f.params, &inputs)
            .expect("registry must accept the JSON params we emit");
    }

    /// Same shape check for every newly-wired round-next factory.
    /// Driving them all through one test keeps the test count modest
    /// while still catching the "we emitted a key the factory doesn't
    /// recognise" class of bugs.
    ///
    /// Same caveat as `sharpen_emitted_params_…`: skipped on
    /// standalone-published until image-filter publishes the new
    /// factories.
    #[test]
    fn every_round_next_op_round_trips_through_image_filter_registry() {
        use oxideav_core::{PixelFormat, PortSpec, TimeBase};

        // A representative input port; most factories ignore the dim
        // values but Crop reads `width`/`height` so we use 16x16.
        let make_inputs = |w: u32, h: u32, fmt: PixelFormat| {
            [PortSpec::video("in", w, h, fmt, TimeBase::new(1, 30))]
        };

        let mut filter_ctx = RuntimeContext::new();
        oxideav_image_filter::register(&mut filter_ctx);

        // Skip the registry-build assertion when the linked image-filter
        // pre-dates the round-next factories. JSON-shape coverage (the
        // other tests) still runs.
        if !filter_ctx.filters.contains("sharpen") {
            return;
        }

        // (op, expected filter-id, inputs)
        let cases: Vec<(Op, &str, [PortSpec; 1])> = vec![
            (
                Op::Sharpen {
                    radius: 1,
                    sigma: 0.5,
                },
                "video.sharpen",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::Unsharp {
                    radius: 1,
                    sigma: 0.5,
                    amount: 1.0,
                    threshold: 0,
                },
                "video.unsharp",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::Gamma { value: 1.8 },
                "video.gamma",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::BrightnessContrast {
                    brightness: 10.0,
                    contrast: -5.0,
                },
                "video.brightness-contrast",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::Contrast { delta: 1 },
                "video.contrast",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::Sepia { threshold: 0.8 },
                "video.sepia",
                make_inputs(4, 4, PixelFormat::Rgb24),
            ),
            (
                Op::Modulate {
                    brightness: 100.0,
                    saturation: 100.0,
                    hue: 100.0,
                },
                "video.modulate",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::Level {
                    black: 16,
                    gamma: 1.2,
                    white: 235,
                },
                "video.level",
                make_inputs(4, 4, PixelFormat::Gray8),
            ),
            (
                Op::Normalize,
                "video.normalize",
                make_inputs(4, 4, PixelFormat::Gray8),
            ),
            (
                Op::Threshold { value: 128 },
                "video.threshold",
                make_inputs(4, 4, PixelFormat::Gray8),
            ),
            (
                Op::Posterize { levels: 4 },
                "video.posterize",
                make_inputs(4, 4, PixelFormat::Gray8),
            ),
            (
                Op::Solarize { value: 100 },
                "video.solarize",
                make_inputs(4, 4, PixelFormat::Gray8),
            ),
            (
                Op::Colorspace("Gray".into()),
                "video.grayscale",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (
                Op::Rotate { degrees: 90 },
                "video.rotate",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
            (Op::Flip, "video.flip", make_inputs(4, 4, PixelFormat::Rgba)),
            (Op::Flop, "video.flop", make_inputs(4, 4, PixelFormat::Rgba)),
            (
                Op::Crop {
                    x: 1,
                    y: 1,
                    w: 2,
                    h: 2,
                },
                "video.crop",
                make_inputs(16, 16, PixelFormat::Rgba),
            ),
            (
                Op::Negate,
                "video.negate",
                make_inputs(4, 4, PixelFormat::Rgba),
            ),
        ];

        // Round-after-next factories. Skip them as a group when the
        // linked image-filter pre-dates the vignette/colorize/equalize/
        // auto-gamma additions (published 0.1.1 only registers up to the
        // round-prev set). We piggy-back on the same skip pattern as
        // above — if `vignette` isn't in the registry, none of the new
        // four are.
        let mut later_cases: Vec<(Op, &str, [PortSpec; 1])> = Vec::new();
        if filter_ctx.filters.contains("vignette") {
            later_cases.push((
                Op::Vignette {
                    radius: 50.0,
                    sigma: 25.0,
                    x: 0.5,
                    y: 0.5,
                },
                "video.vignette",
                make_inputs(4, 4, PixelFormat::Rgba),
            ));
            later_cases.push((
                Op::Colorize {
                    color: [200, 100, 50, 255],
                    amount: 0.4,
                },
                "video.colorize",
                make_inputs(4, 4, PixelFormat::Rgba),
            ));
            later_cases.push((
                Op::Equalize,
                "video.equalize",
                make_inputs(4, 4, PixelFormat::Gray8),
            ));
            later_cases.push((
                Op::AutoGamma,
                "video.auto-gamma",
                make_inputs(4, 4, PixelFormat::Gray8),
            ));
        }
        let cases: Vec<(Op, &str, [PortSpec; 1])> = cases.into_iter().chain(later_cases).collect();

        for (op, expected_name, inputs) in cases {
            let job = plan_to_job(&plan_with(vec![op.clone()]), &empty_ctx()).unwrap();
            let track = &job.outputs.values().next().unwrap().all[0];
            let f = find_filter(&track.input, expected_name)
                .unwrap_or_else(|| panic!("expected node {expected_name} for {op:?}"));
            filter_ctx
                .filters
                .make(expected_name, &f.params, &inputs)
                .unwrap_or_else(|e| {
                    panic!(
                        "registry rejected emitted params for {op:?}: {e:?} \
                         (json: {})",
                        f.params
                    )
                });
        }
    }

    /// True end-to-end on a 4x4 RGBA fixture: synthesise a frame, run
    /// it through `image_filter::Sharpen` directly to get the expected
    /// output bytes, then walk the same JSON params through the
    /// registry-built filter and compare. This locks down the
    /// "factory takes the same params we emit" contract at a pixel
    /// level, not just at a build-success level.
    ///
    /// Skipped on standalone-published until the image-filter sharpen
    /// factory publishes (today's published 0.1.1 doesn't have it).
    #[test]
    fn sharpen_4x4_pixel_match_through_registry() {
        use oxideav_core::{
            filter::FilterContext, Frame, PixelFormat, PortSpec, Result as CoreResult, TimeBase,
            VideoFrame, VideoPlane,
        };
        use oxideav_image_filter::{ImageFilter, Sharpen, VideoStreamParams};

        // 4x4 RGBA fixture with an obvious horizontal edge so sharpen
        // produces a visible delta.
        let make_frame = || {
            let mut data = Vec::with_capacity(4 * 4 * 4);
            for _y in 0..4 {
                for x in 0..4 {
                    let v = if x < 2 { 60 } else { 200 };
                    data.extend_from_slice(&[v, v, v, 255]);
                }
            }
            VideoFrame {
                pts: None,
                planes: vec![VideoPlane { stride: 16, data }],
            }
        };

        // Reference: call the library directly with the same params
        // the CLI translates `-sharpen 1x0.5` to.
        let reference = Sharpen::new(1, 0.5)
            .apply(
                &make_frame(),
                VideoStreamParams {
                    format: PixelFormat::Rgba,
                    width: 4,
                    height: 4,
                },
            )
            .expect("library Sharpen apply");

        // Through the registry: build the plan, translate to a Job,
        // grab the JSON, build the filter via the registry, run.
        let plan = ConvertPlan {
            input: "in.png".into(),
            input_pages: None,
            ops: vec![Op::Sharpen {
                radius: 1,
                sigma: 0.5,
            }],
            output: "out.png".into(),
            output_template: None,
            ping: false,
            probe: false,
            probe_json: false,
            probe_watch: false,
            mesh3d_options: Default::default(),
        };
        let job = plan_to_job(&plan, &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.sharpen").expect("video.sharpen node");

        let mut filter_ctx = RuntimeContext::new();
        oxideav_image_filter::register(&mut filter_ctx);

        // Skip when the linked image-filter pre-dates the sharpen
        // factory (published 0.1.1 only registers blur/edge/resize).
        if !filter_ctx.filters.contains("sharpen") {
            return;
        }

        let inputs = [PortSpec::video(
            "in",
            4,
            4,
            PixelFormat::Rgba,
            TimeBase::new(1, 30),
        )];
        let mut built = filter_ctx
            .filters
            .make("video.sharpen", &f.params, &inputs)
            .expect("factory build");

        struct Collect {
            out: Vec<Frame>,
        }
        impl FilterContext for Collect {
            fn emit(&mut self, _port: usize, frame: Frame) -> CoreResult<()> {
                self.out.push(frame);
                Ok(())
            }
        }
        let mut col = Collect { out: Vec::new() };
        built
            .push(&mut col, 0, &Frame::Video(make_frame()))
            .expect("filter push");
        let registry_out = match col.out.into_iter().next().unwrap() {
            Frame::Video(v) => v,
            other => panic!("expected video frame, got {other:?}"),
        };

        // Pixel-exact match: same params, same input → same output.
        assert_eq!(
            registry_out.planes[0].data, reference.planes[0].data,
            "registry-built sharpen must match library Sharpen byte-for-byte"
        );
    }

    #[test]
    fn vignette_wires_centre_radius_sigma() {
        let job = plan_to_job(
            &plan_with(vec![Op::Vignette {
                radius: 50.0,
                sigma: 25.0,
                x: 0.5,
                y: 0.5,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.vignette").expect("video.vignette node");
        assert!((f.params["x"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        assert!((f.params["y"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        assert!((f.params["radius"].as_f64().unwrap() - 50.0).abs() < 1e-6);
        assert!((f.params["sigma"].as_f64().unwrap() - 25.0).abs() < 1e-6);
    }

    #[test]
    fn colorize_wires_color_array_and_amount() {
        let job = plan_to_job(
            &plan_with(vec![Op::Colorize {
                color: [200, 100, 50, 255],
                amount: 0.4,
            }]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.colorize").expect("video.colorize node");
        let arr = f.params["color"].as_array().expect("color is array");
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0], 200);
        assert_eq!(arr[1], 100);
        assert_eq!(arr[2], 50);
        assert_eq!(arr[3], 255);
        assert!((f.params["amount"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn equalize_wires_empty_params() {
        let job = plan_to_job(&plan_with(vec![Op::Equalize]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.equalize").expect("video.equalize node");
        assert!(f.params.as_object().unwrap().is_empty());
    }

    #[test]
    fn auto_gamma_wires_empty_params() {
        let job = plan_to_job(&plan_with(vec![Op::AutoGamma]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.auto-gamma").expect("video.auto-gamma node");
        assert!(f.params.as_object().unwrap().is_empty());
    }

    #[test]
    fn long_chain_preserves_source_order() {
        // Sanity that all the new filters interleave with existing
        // ones in source order (innermost = first parsed).
        let job = plan_to_job(
            &plan_with(vec![
                Op::Resize {
                    width: 64,
                    height: 64,
                    mode: ResizeMode::Default,
                },
                Op::Sharpen {
                    radius: 1,
                    sigma: 0.5,
                },
                Op::Gamma { value: 1.8 },
                Op::Negate,
            ]),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let names = collect_filter_names(&track.input);
        assert_eq!(
            names,
            vec![
                "video.resize".to_string(),
                "video.sharpen".to_string(),
                "video.gamma".to_string(),
                "video.negate".to_string(),
            ]
        );
    }

    /// `Op::Trim` lowers to a `video.trim` FilterNode carrying the
    /// captured fuzz tolerance. The factory infers the reference
    /// background from the input's `(0, 0)` pixel when no explicit
    /// `background` array is forwarded (IM's default behaviour).
    #[test]
    fn trim_wires_fuzz_value() {
        let job = plan_to_job(&plan_with(vec![Op::Trim { fuzz: 17 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.trim").expect("video.trim node");
        assert_eq!(f.params["fuzz"], 17);
        // No background override → factory falls back to corner pixel.
        assert!(f.params.get("background").is_none());
    }

    /// Default `Op::Trim { fuzz: 0 }` is the canonical no-fuzz exact-
    /// match shape — pin it explicitly so a future refactor that
    /// switches the default representation surfaces here, not on the
    /// pipeline executor.
    #[test]
    fn trim_default_fuzz_lowers_to_zero() {
        let job = plan_to_job(&plan_with(vec![Op::Trim { fuzz: 0 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.trim").expect("video.trim node");
        assert_eq!(f.params["fuzz"], 0);
    }

    /// `Op::Roll` lowers to a `video.roll` FilterNode carrying the
    /// signed `dx`/`dy` shift values. The image-filter factory
    /// recognises both `dx`/`dy` and the `x`/`y` aliases; we forward
    /// the canonical pair.
    #[test]
    fn roll_lowers_to_video_roll() {
        let job = plan_to_job(&plan_with(vec![Op::Roll { dx: 5, dy: -10 }]), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let f = find_filter(&track.input, "video.roll").expect("video.roll node");
        assert_eq!(f.params["dx"], 5);
        assert_eq!(f.params["dy"], -10);
    }

    #[test]
    fn format_override_uses_registry() {
        let mut ctx = RuntimeContext::new();
        ctx.containers.register_extension("bar", "bar_codec");
        // -format BAR with output that ends in something else still
        // lands on bar_codec.
        assert_eq!(
            codec_for_output(Some("BAR"), "out.unrelated", &ctx).as_deref(),
            Some("bar_codec")
        );
        // -format with no matching registration → None (caller can
        // surface a "use -format CODEC" hint).
        assert!(codec_for_output(Some("nothing"), "out.unrelated", &ctx).is_none());
    }

    // ─────────────────── Phase C-3b: plan_to_render3d_job ───────────────────
    //
    // The new helper builds a Job whose track input is a
    // TrackInput::Render3D node. The C-3e callback flip will route 3D →
    // raster through Executor + RenderSourceFactory using this exact
    // Job shape, so the tests pin the shape it emits.

    use crate::op::{CameraSpec, LightSpec, Mesh3DOptions, Mesh3DRenderMode, ProjectionMode};

    /// Plan factory tuned for 3D-input flows. Uses a .stl input + .png
    /// output by default; callers override fields as needed.
    fn render3d_plan(ops: Vec<Op>, options: Mesh3DOptions) -> ConvertPlan {
        ConvertPlan {
            input: "cube.stl".into(),
            input_pages: None,
            ops,
            output: "out.png".into(),
            output_template: None,
            ping: false,
            probe: false,
            probe_json: false,
            probe_watch: false,
            mesh3d_options: options,
        }
    }

    /// Pull the Render3D node out of a Job's only track. Panics if the
    /// chain isn't rooted on a Render3D leaf (after peeling filter
    /// wrappers).
    fn find_render3d(track: &TrackSpec) -> &Render3DNode {
        let mut cur = &track.input;
        loop {
            match cur {
                TrackInput::Render3D(node) => return node,
                TrackInput::Filter(f) => cur = f.input.as_ref(),
                TrackInput::Convert(c) => cur = c.input.as_ref(),
                TrackInput::Source(_) => {
                    panic!("expected Render3D leaf at chain root, got Source")
                }
            }
        }
    }

    #[test]
    fn render3d_job_uses_render3d_track_input_with_input_uri() {
        // No ops, defaults everywhere — bare-bones Render3D Job.
        let job = plan_to_render3d_job(
            &render3d_plan(vec![], Mesh3DOptions::default()),
            &empty_ctx(),
        )
        .unwrap();
        assert_eq!(job.outputs.len(), 1);
        let (key, out) = job.outputs.iter().next().unwrap();
        assert_eq!(key, "out.png");
        assert_eq!(out.all.len(), 1);
        let node = find_render3d(&out.all[0]);
        assert_eq!(node.source, "cube.stl");
        assert_eq!(node.backend, RENDER3D_DEFAULT_BACKEND);
    }

    #[test]
    fn render3d_default_opts_carry_canvas_size_and_transparent_bg() {
        // No -resize, no -bg, no -background → 1024×1024 + RGBA zeros.
        let job = plan_to_render3d_job(
            &render3d_plan(vec![], Mesh3DOptions::default()),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = find_render3d(track);
        assert_eq!(node.opts["width"], RENDER3D_DEFAULT_WIDTH);
        assert_eq!(node.opts["height"], RENDER3D_DEFAULT_HEIGHT);
        let bg = node.opts["background"]
            .as_array()
            .expect("background array");
        assert_eq!(bg, &vec![json!(0), json!(0), json!(0), json!(0)]);
    }

    #[test]
    fn render3d_resize_op_seeds_canvas_dims() {
        // `-resize 640x480` (any mode) seeds the renderer's framebuffer
        // size; the Resize op itself is absorbed and MUST NOT appear in
        // the post-render filter chain.
        let job = plan_to_render3d_job(
            &render3d_plan(
                vec![Op::Resize {
                    width: 640,
                    height: 480,
                    mode: ResizeMode::Default,
                }],
                Mesh3DOptions::default(),
            ),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = find_render3d(track);
        assert_eq!(node.opts["width"], 640);
        assert_eq!(node.opts["height"], 480);
        // No video.resize filter on top — renderer already targeted
        // the right dims.
        assert!(
            collect_filter_names(&track.input)
                .iter()
                .all(|n| n != "video.resize"),
            "Resize should be absorbed by renderer, not also wired as a post-pass"
        );
    }

    #[test]
    fn render3d_bg_option_wins_over_background_op() {
        // `-bg` lives on Mesh3DOptions; `-background` is an Op. The
        // renderer reads `-bg` for the framebuffer clear; `-background`
        // is reserved for `-alpha remove` canvas-fill. When both are
        // set the opts carry `-bg`.
        let job = plan_to_render3d_job(
            &render3d_plan(
                vec![Op::Background([99, 99, 99, 255])],
                Mesh3DOptions {
                    bg: Some([10, 20, 30, 200]),
                    ..Mesh3DOptions::default()
                },
            ),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = find_render3d(track);
        let bg = node.opts["background"].as_array().unwrap();
        assert_eq!(bg, &vec![json!(10), json!(20), json!(30), json!(200)]);
    }

    #[test]
    fn render3d_falls_back_to_background_op_when_bg_unset() {
        // No `-bg`; an `-background` op is the next-most-specific
        // source. Op is absorbed (the renderer pre-fills the
        // framebuffer with it), so it MUST NOT appear in the
        // post-render filter chain.
        let job = plan_to_render3d_job(
            &render3d_plan(
                vec![Op::Background([7, 8, 9, 255])],
                Mesh3DOptions::default(),
            ),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = find_render3d(track);
        let bg = node.opts["background"].as_array().unwrap();
        assert_eq!(bg, &vec![json!(7), json!(8), json!(9), json!(255)]);
    }

    #[test]
    fn render3d_carries_every_user_set_option_into_opts_json() {
        // Pack every Mesh3DOptions field — the helper must round-trip
        // each one into the opts JSON so the C-3e callback can rebuild
        // the renderer's RenderOptions struct without losing data.
        let options = Mesh3DOptions {
            stl_format: None,
            gltf_format: None,
            render_mode: Some(Mesh3DRenderMode::Phong),
            light: Some(LightSpec {
                azimuth_deg: 12.0,
                elevation_deg: 34.0,
                intensity: 0.75,
            }),
            camera: Some(CameraSpec {
                elevation_deg: 30.0,
                azimuth_deg: 45.0,
                distance: 1.5,
            }),
            projection: Some(ProjectionMode::Orthographic),
            fov_deg: Some(45.0),
            bg: Some([1, 2, 3, 4]),
            aa: Some(4),
        };
        let job = plan_to_render3d_job(&render3d_plan(vec![], options), &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = find_render3d(track);
        let o = &node.opts;
        assert_eq!(o["shading"], "phong");
        assert_eq!(o["projection"], "orthographic");
        assert!((o["fov_deg"].as_f64().unwrap() - 45.0).abs() < 1e-6);
        assert!((o["light"]["azimuth_deg"].as_f64().unwrap() - 12.0).abs() < 1e-6);
        assert!((o["light"]["elevation_deg"].as_f64().unwrap() - 34.0).abs() < 1e-6);
        assert!((o["light"]["intensity"].as_f64().unwrap() - 0.75).abs() < 1e-6);
        assert!((o["camera"]["elevation_deg"].as_f64().unwrap() - 30.0).abs() < 1e-6);
        assert!((o["camera"]["azimuth_deg"].as_f64().unwrap() - 45.0).abs() < 1e-6);
        assert!((o["camera"]["distance"].as_f64().unwrap() - 1.5).abs() < 1e-6);
        assert_eq!(o["aa"], 4);
    }

    #[test]
    fn render3d_opts_omit_keys_for_unset_options_so_renderer_defaults_apply() {
        // The renderer has its own defaults (Phong, perspective, 60°
        // fov, default light, no camera override, aa=1). When the user
        // doesn't pin a per-format flag the opts must OMIT the key so
        // the C-3e callback uses RenderOptions::default() for it
        // instead of us hard-coding the renderer's default twice.
        let job = plan_to_render3d_job(
            &render3d_plan(vec![], Mesh3DOptions::default()),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = find_render3d(track);
        let o = node.opts.as_object().expect("opts object");
        // Required keys present.
        assert!(o.contains_key("width"));
        assert!(o.contains_key("height"));
        assert!(o.contains_key("background"));
        // Optional keys absent.
        assert!(!o.contains_key("shading"));
        assert!(!o.contains_key("projection"));
        assert!(!o.contains_key("fov_deg"));
        assert!(!o.contains_key("light"));
        assert!(!o.contains_key("camera"));
        assert!(!o.contains_key("aa"));
    }

    #[test]
    fn render3d_shading_tag_round_trips_every_mode() {
        // Each Mesh3DRenderMode lowers to a distinct lowercase tag
        // matching the wire form oxideav_render's ShadingMode expects.
        for (mode, tag) in [
            (Mesh3DRenderMode::Flat, "flat"),
            (Mesh3DRenderMode::Wireframe, "wireframe"),
            (Mesh3DRenderMode::Gouraud, "gouraud"),
            (Mesh3DRenderMode::Phong, "phong"),
            (Mesh3DRenderMode::NormalDebug, "normal-debug"),
            (Mesh3DRenderMode::DepthDebug, "depth-debug"),
        ] {
            assert_eq!(shading_tag(mode), tag);
            let job = plan_to_render3d_job(
                &render3d_plan(
                    vec![],
                    Mesh3DOptions {
                        render_mode: Some(mode),
                        ..Mesh3DOptions::default()
                    },
                ),
                &empty_ctx(),
            )
            .unwrap();
            let track = &job.outputs.values().next().unwrap().all[0];
            let node = find_render3d(track);
            assert_eq!(node.opts["shading"], tag, "shading tag for {mode:?}");
        }
    }

    #[test]
    fn render3d_projection_tag_round_trips() {
        assert_eq!(projection_tag(ProjectionMode::Perspective), "perspective");
        assert_eq!(projection_tag(ProjectionMode::Orthographic), "orthographic");
    }

    #[test]
    fn render3d_aa_is_clamped_into_1_through_8() {
        // The renderer's own RenderOptions::validate rejects aa outside
        // 1..=8. Clamp at the JSON-build layer so the callback can't
        // see an out-of-range value via stale caller code.
        for (input, expected) in [(0u32, 1u32), (1, 1), (4, 4), (8, 8), (9, 8), (u32::MAX, 8)] {
            let job = plan_to_render3d_job(
                &render3d_plan(
                    vec![],
                    Mesh3DOptions {
                        aa: Some(input),
                        ..Mesh3DOptions::default()
                    },
                ),
                &empty_ctx(),
            )
            .unwrap();
            let track = &job.outputs.values().next().unwrap().all[0];
            let node = find_render3d(track);
            assert_eq!(node.opts["aa"], expected, "aa={input} → {expected}");
        }
    }

    #[test]
    fn render3d_post_render_ops_wire_through_filter_chain() {
        // Non-absorbed ops (rotate/blur/sharpen/…) ride on top of the
        // Render3D leaf via the same filter-chain walker as the regular
        // plan_to_job path. Order matches source order from inside-out.
        let job = plan_to_render3d_job(
            &render3d_plan(
                vec![
                    Op::Rotate { degrees: 90 },
                    Op::Blur {
                        radius: 2,
                        sigma: 1.0,
                    },
                    Op::Negate,
                ],
                Mesh3DOptions::default(),
            ),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let names = collect_filter_names(&track.input);
        assert_eq!(
            names,
            vec![
                "video.rotate".to_string(),
                "video.blur".to_string(),
                "video.negate".to_string(),
            ]
        );
        // The chain still terminates on a Render3D leaf — the walker
        // wraps it, not a Source.
        let node = find_render3d(track);
        assert_eq!(node.source, "cube.stl");
    }

    #[test]
    fn render3d_strip_and_quality_still_land_in_codec_params() {
        // Sink-side rules (strip / quality / format) ride the same
        // path on the Render3D Job as they do on the regular Job.
        let job = plan_to_render3d_job(
            &render3d_plan(vec![Op::Quality(90), Op::Strip], Mesh3DOptions::default()),
            &empty_ctx(),
        )
        .unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.params["quality"], 90);
        assert_eq!(track.params["strip_metadata"], true);
    }

    #[test]
    fn render3d_resolves_output_codec_via_container_registry() {
        // Output codec selection mirrors plan_to_job — registry hit
        // wins; unknown extension stays None and lets the executor
        // surface its own "no codec" diagnostic.
        let mut ctx = RuntimeContext::new();
        ctx.containers.register_extension("png", "png");
        let job =
            plan_to_render3d_job(&render3d_plan(vec![], Mesh3DOptions::default()), &ctx).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.codec.as_deref(), Some("png"));
    }
}
