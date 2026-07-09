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

use crate::op::{ConvertPlan, Dither, Mesh3DOptions, Mesh3DRenderMode, Op, ProjectionMode};
use indexmap::IndexMap;
use oxideav_core::{Error, RuntimeContext};
use oxideav_pipeline::{
    FilterNode, Job, OutputSpec, Render3DNode, SourceRef, TrackInput, TrackSpec,
};
use serde_json::{json, Value};

/// Backend name handed to [`Render3DNode::backend`] for the auto-route
/// 3D→raster pipeline path.  Mirrors the only [`oxideav_render::RenderBackend`]
/// variant the workspace ships today (`Scanline`); the pipeline executor
/// hands this name to the user-installed
/// [`oxideav_pipeline::executor::RenderSourceFactory`] which deserialises
/// the opts JSON back into the renderer's typed
/// [`oxideav_render::RenderOptions`].
///
/// When future render backends (`raycast`, `pathtrace`, …) land, a new
/// `-render-backend NAME` arg on the CLI side will override this default.
pub const RENDER3D_DEFAULT_BACKEND: &str = "scanline";

/// Default raster canvas dimensions for the 3D→raster job when no
/// `-resize` op pins them.  Matches [`crate::mesh3d_render`]'s
/// in-tree default so the auto-route helper produces the same canvas
/// as the legacy side-channel runner.
const DEFAULT_RENDER3D_WIDTH: u32 = 1024;
const DEFAULT_RENDER3D_HEIGHT: u32 = 1024;

/// Default `-bg` for the 3D render canvas: transparent black, matching
/// [`crate::mesh3d_render`].  Composites cleanly against any downstream
/// `-alpha remove`.
const DEFAULT_RENDER3D_BG: [u8; 4] = [0, 0, 0, 0];

/// Default vertical FOV for perspective projection (degrees).
const DEFAULT_RENDER3D_FOV_DEG: f32 = 60.0;

/// Build a [`Job`] from a [`ConvertPlan`].
///
/// `ctx` is consulted via its [`ContainerRegistry`](oxideav_core::ContainerRegistry)
/// to map the output extension to a codec id — sibling crates that
/// register a container (e.g. `oxideav-png`, `oxideav-qoi`,
/// `oxideav-dds`) automatically extend the supported output set without
/// needing a hard-coded entry here.
pub fn plan_to_job(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<Job, Error> {
    // Walk ops, distinguishing track-side filters from sink-side
    // metadata (format / quality / strip).  Filters are flattened
    // into the recursive `TrackInput` chain; the rest are deferred
    // to the sink.  Per-op lowering lives in [`lower_op`], shared
    // with [`plan_to_render3d_job`].
    let mut sink = SinkSide::default();

    // Starting input: the file itself.
    let mut chain: TrackInput = TrackInput::Source(SourceRef {
        from: plan.input.clone(),
    });

    for op in &plan.ops {
        chain = lower_op(chain, op, &mut sink, false);
    }

    finish_job(plan, ctx, chain, sink)
}

/// Build a 3D→raster [`Job`] from a [`ConvertPlan`] whose input is a
/// 3D-asset path (`.stl`/`.obj`/`.gltf`/`.glb`/`.usdz`/`.fbx`/`.mtl`).
///
/// Shape:
///
/// * The track input is [`TrackInput::Render3D`] — the pipeline
///   executor hands the `source` URI + `backend` name + opaque `opts`
///   JSON to the caller-installed
///   [`oxideav_pipeline::executor::RenderSourceFactory`].  That factory
///   is the layer that talks to [`oxideav_render`] and the
///   [`oxideav_mesh3d::Mesh3DRegistry`]; pipeline itself stays codec-
///   and render-agnostic.
/// * `Op::Resize` and `Op::Background` are absorbed by the renderer —
///   the resize dims seed `width` / `height` on the opts struct, and
///   the background seeds `background`.  Neither survives as a filter
///   node because the renderer produces the canvas at the requested
///   size directly (no downsample step, AA stays native to the raster
///   grid).
/// * `Mesh3DOptions::bg` wins over `Op::Background` so users can keep
///   the IM canvas-fill semantics separate from the renderer's clear
///   colour — mirrors [`crate::mesh3d_render`]'s `pick_render_bg`.
/// * Every other op (rotate / flip / negate / tonal / colour-grading
///   / strip / quality / …) is forwarded through the regular
///   [`plan_to_job`] filter chain on top of the Render3D source so the
///   post-rasterisation transforms keep their existing JSON dialect.
/// * The output codec is resolved by the same
///   [`ContainerRegistry`](oxideav_core::ContainerRegistry) lookup used
///   by [`plan_to_job`].
///
/// `opts` JSON mirrors the field shape of
/// [`oxideav_render::RenderOptions`] (one field per key, lowercase /
/// snake_case names) so the consumer's `RenderSourceFactory` can
/// deserialise it directly.  Keys: `width`, `height`, `background`
/// (RGBA `[u8; 4]`), `shading`, `projection`, `fov_deg`, `light` (object
/// with `azimuth_deg` / `elevation_deg` / `intensity`), `camera`
/// (object with `elevation_deg` / `azimuth_deg` / `distance`, or `null`),
/// `aa`.  Unset `Mesh3DOptions` fall back to the renderer's documented
/// defaults so a bare `convert in.gltf out.png` produces a sensible
/// canvas without any flag plumbing.
pub fn plan_to_render3d_job(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<Job, Error> {
    let (width, height) = pick_render3d_dims(&plan.ops);
    let bg = pick_render3d_bg(&plan.ops, &plan.mesh3d_options);
    let opts = build_render3d_opts(&plan.mesh3d_options, width, height, bg);

    // Source node: the renderer absorbs the file IO via the installed
    // RenderSourceFactory, so the source URI lives on Render3DNode
    // rather than on a SourceRef leaf.
    let mut chain = TrackInput::Render3D(Render3DNode {
        source: plan.input.clone(),
        backend: RENDER3D_DEFAULT_BACKEND.to_string(),
        opts,
    });

    let mut sink = SinkSide::default();
    for op in &plan.ops {
        chain = lower_op(chain, op, &mut sink, true);
    }

    finish_job(plan, ctx, chain, sink)
}

/// Pull the render canvas dimensions from the op chain: the LAST
/// `-resize WxH` (any mode) seeds the canvas; otherwise the default
/// 1024×1024.  Mirrors [`crate::mesh3d_render::pick_dims`] so the
/// auto-route helper and the legacy side-channel runner agree on
/// canvas size for the same flag combination.
fn pick_render3d_dims(ops: &[Op]) -> (u32, u32) {
    for op in ops.iter().rev() {
        if let Op::Resize { width, height, .. } = op {
            return (*width, *height);
        }
    }
    (DEFAULT_RENDER3D_WIDTH, DEFAULT_RENDER3D_HEIGHT)
}

/// `-bg COLOR` (`Mesh3DOptions::bg`) wins over `-background COLOR`
/// (`Op::Background`); both fall back to transparent black.  Mirrors
/// [`crate::mesh3d_render::pick_render_bg`].
fn pick_render3d_bg(ops: &[Op], options: &Mesh3DOptions) -> [u8; 4] {
    if let Some(bg) = options.bg {
        return bg;
    }
    for op in ops.iter().rev() {
        if let Op::Background(c) = op {
            return *c;
        }
    }
    DEFAULT_RENDER3D_BG
}

/// Build the `opts` JSON payload that ships on
/// [`Render3DNode::opts`].  Field shape mirrors
/// [`oxideav_render::RenderOptions`] one-for-one so the consumer's
/// installed `RenderSourceFactory` can `serde_json::from_value` it
/// straight into the typed struct.  None-valued `Mesh3DOptions` fall
/// back to the renderer's documented defaults (Flat shading,
/// perspective projection, 60° FOV, default directional light at
/// 45°/45°/1.0, auto-frame camera, 1× AA).
fn build_render3d_opts(options: &Mesh3DOptions, width: u32, height: u32, bg: [u8; 4]) -> Value {
    let shading = options
        .render_mode
        .map(shading_tag_from_mode)
        .unwrap_or("flat");
    let projection = options
        .projection
        .map(projection_tag_from_mode)
        .unwrap_or("perspective");
    let fov_deg = f32j(options.fov_deg.unwrap_or(DEFAULT_RENDER3D_FOV_DEG));
    // The renderer's default light spec lives behind
    // `LightSpec::default_light()` on the oxideav-render side; mirror
    // the same defaults here so the JSON carries the same values the
    // typed struct would have produced.
    let light = options
        .light
        .map(|l| {
            json!({
                "azimuth_deg": f32j(l.azimuth_deg),
                "elevation_deg": f32j(l.elevation_deg),
                "intensity": f32j(l.intensity),
            })
        })
        .unwrap_or_else(|| {
            json!({
                "azimuth_deg": 45.0,
                "elevation_deg": 45.0,
                "intensity": 1.0,
            })
        });
    let camera = options.camera.map(|c| {
        json!({
            "elevation_deg": f32j(c.elevation_deg),
            "azimuth_deg": f32j(c.azimuth_deg),
            "distance": f32j(c.distance),
        })
    });
    let aa = options.aa.unwrap_or(1).clamp(1, 8);

    json!({
        "width": width,
        "height": height,
        "background": [bg[0], bg[1], bg[2], bg[3]],
        "shading": shading,
        "projection": projection,
        "fov_deg": fov_deg,
        "light": light,
        "camera": camera,
        "aa": aa,
    })
}

/// Stable lowercase tag for the JSON-side `shading` field.  Matches the
/// snake-case identifier scheme used by
/// [`oxideav_render::ShadingMode`]'s variants (Flat/Wireframe/Gouraud/
/// Phong are single-word, NormalDebug/DepthDebug become
/// `normal-debug` / `depth-debug` — same dash-cased shape as
/// `video.auto-gamma` etc. elsewhere in the JSON dialect).
fn shading_tag_from_mode(mode: Mesh3DRenderMode) -> &'static str {
    match mode {
        Mesh3DRenderMode::Flat => "flat",
        Mesh3DRenderMode::Wireframe => "wireframe",
        Mesh3DRenderMode::Gouraud => "gouraud",
        Mesh3DRenderMode::Phong => "phong",
        Mesh3DRenderMode::NormalDebug => "normal-debug",
        Mesh3DRenderMode::DepthDebug => "depth-debug",
    }
}

/// Stable lowercase tag for the JSON-side `projection` field.
fn projection_tag_from_mode(mode: ProjectionMode) -> &'static str {
    match mode {
        ProjectionMode::Perspective => "perspective",
        ProjectionMode::Orthographic => "orthographic",
    }
}

/// Sink-side (non-filter) state accumulated while walking ops:
/// the encoder params bag, metadata stripping, `-format` override.
struct SinkSide {
    codec_params: Value,
    strip_metadata: bool,
    format_override: Option<String>,
}

impl Default for SinkSide {
    fn default() -> Self {
        SinkSide {
            codec_params: json!({}),
            strip_metadata: false,
            format_override: None,
        }
    }
}

/// Convert an `f32` op value to a JSON number without widening noise.
///
/// A plain `json!(x)` widens f32→f64 bit-exactly, so `-light 10,20,0.9`
/// would emit `"intensity": 0.8999999761581421` into the job document.
/// Round-tripping through the f32's shortest decimal representation
/// keeps the number the user actually typed (`0.9`) — the JSON stays
/// snapshot-clean, and a consumer narrowing f64→f32 lands on the same
/// f32 either way (shortest-repr is round-trip-exact by definition).
fn f32j(v: f32) -> Value {
    json!(v.to_string().parse::<f64>().unwrap_or(f64::from(v)))
}

/// Lower one [`Op`] onto the recursive filter chain, or fold it into
/// the sink-side state.
///
/// Shared by [`plan_to_job`] and [`plan_to_render3d_job`].
/// `absorb_canvas` selects the 3D-render behaviour where the renderer
/// produces the canvas directly ([`pick_render3d_dims`] /
/// [`pick_render3d_bg`] already consumed `-resize` / `-background`,
/// and the resize half of `-thumbnail` is likewise absorbed) so those
/// ops must not ALSO survive as filter nodes — skipping them avoids a
/// redundant resize / background pass downstream of the Render3D
/// source.
fn lower_op(chain: TrackInput, op: &Op, sink: &mut SinkSide, absorb_canvas: bool) -> TrackInput {
    // A filter-chain step — wrap the current chain in a FilterNode.
    let wrap = |prev: TrackInput, filter: &str, params: Value| {
        TrackInput::Filter(FilterNode {
            filter: filter.to_string(),
            params,
            input: Box::new(prev),
        })
    };

    match op {
        Op::Resize {
            width,
            height,
            mode,
        } => {
            if absorb_canvas {
                // Renderer absorbs the dims — encoded directly onto
                // the RenderOptions payload, not as a filter node.
                return chain;
            }
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
            wrap(
                chain,
                "video.resize",
                json!({
                    "width": width,
                    "height": height,
                    "interpolation": "bilinear",
                    "mode": mode.as_tag(),
                }),
            )
        }
        Op::Thumbnail {
            width,
            height,
            mode,
        } => {
            // `-thumbnail` is sugar for `Resize + Strip` (and, on a
            // future pass, EXIF auto-orient). On the render path the
            // renderer already produced the canvas at the requested
            // size via `pick_render3d_dims`, so only the strip half
            // survives there.
            sink.strip_metadata = true;
            if absorb_canvas {
                return chain;
            }
            wrap(
                chain,
                "video.resize",
                json!({
                    "width": width,
                    "height": height,
                    "interpolation": "bilinear",
                    "mode": mode.as_tag(),
                }),
            )
        }
        Op::Define { key, value } => {
            // Forward the literal key (preserving any `:` namespace
            // separator) onto the codec params bag. Values that
            // parse as integers / floats / booleans still come
            // through as JSON strings — codecs that care about
            // type can re-parse from the string. Bare `-define KEY`
            // (no `=VALUE`) becomes `{"KEY": true}`.
            match value {
                Some(v) => sink.codec_params[key.clone()] = json!(v),
                None => sink.codec_params[key.clone()] = json!(true),
            }
            chain
        }
        Op::Blur { radius, sigma } => wrap(
            chain,
            "video.blur",
            json!({
                "radius": radius,
                "sigma": f32j(*sigma),
                "planes": "all"
            }),
        ),
        Op::Edge { radius } => wrap(chain, "video.edge", json!({ "radius": radius })),
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
            wrap(
                chain,
                "video.pixfmt",
                json!({
                    "format": "pal8",
                    "dither": dither_str,
                    "colors": count,
                }),
            )
        }
        Op::Format(fmt) => {
            sink.format_override = Some(fmt.clone());
            chain
        }
        Op::Quality(q) => {
            // Codec-specific key; the encoder drops it when
            // unsupported.  Use `quality` for JPEG/WebP, `crf`
            // could be added by a later pass for h264/h265.
            sink.codec_params["quality"] = json!(q);
            chain
        }
        Op::Strip => {
            sink.strip_metadata = true;
            chain
        }
        // Vector-input ops; dropped on both paths. Raster inputs have
        // no DPI, no composite-over-bg semantics that wouldn't
        // conflict with `-resize`/etc., and no alpha-channel grammar
        // to honour beyond what the encoder already does; on the
        // render path `-background` was already consumed by
        // `pick_render3d_bg` before the walk. The pdf_runner
        // side-channel applies them when reading PDFs.
        Op::Density(_) | Op::Background(_) | Op::Alpha(_) => chain,
        Op::Rotate { degrees } => wrap(chain, "video.rotate", json!({ "degrees": degrees })),
        Op::Flip => wrap(chain, "video.flip", json!({})),
        Op::Flop => wrap(chain, "video.flop", json!({})),
        Op::Crop { x, y, w, h } => wrap(
            chain,
            "video.crop",
            json!({ "x": x, "y": y, "width": w, "height": h }),
        ),
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
            wrap(
                chain,
                "video.extent",
                json!({
                    "width": width,
                    "height": height,
                    "offset_x": x,
                    "offset_y": y,
                    "background": [bg[0], bg[1], bg[2], bg[3]],
                }),
            )
        }
        Op::Negate => wrap(chain, "video.negate", json!({})),
        Op::Sharpen { radius, sigma } => wrap(
            chain,
            "video.sharpen",
            json!({ "radius": radius, "sigma": f32j(*sigma) }),
        ),
        Op::Unsharp {
            radius,
            sigma,
            amount,
            threshold,
        } => wrap(
            chain,
            "video.unsharp",
            json!({
                "radius": radius,
                "sigma": f32j(*sigma),
                "amount": f32j(*amount),
                "threshold": threshold,
            }),
        ),
        Op::Gamma { value } => wrap(chain, "video.gamma", json!({ "value": f32j(*value) })),
        Op::BrightnessContrast {
            brightness,
            contrast,
        } => wrap(
            chain,
            "video.brightness-contrast",
            json!({ "brightness": f32j(*brightness), "contrast": f32j(*contrast) }),
        ),
        Op::Contrast { delta } => {
            // IM's bare `-contrast` flag bumps contrast by a single
            // 5%-of-range step. Multiple `-contrast` accumulate
            // linearly. Map to the brightness-contrast factory's
            // contrast-only knob so the executor can fold this into
            // a single LUT pass alongside any explicit
            // `-brightness-contrast` already in the chain.
            let pct = f64::from(*delta) * 5.0;
            wrap(chain, "video.contrast", json!({ "value": pct }))
        }
        Op::Sepia { threshold } => wrap(
            chain,
            "video.sepia",
            json!({ "threshold": f32j(*threshold) }),
        ),
        Op::Modulate {
            brightness,
            saturation,
            hue,
        } => {
            // IM's hue is "percent-of-base around 100" — 0 is
            // -180°, 100 is identity, 200 is +180°. The
            // image-filter factory accepts degrees directly via
            // `hue_degrees`, so translate. The subtract/scale runs
            // in f64 on the noise-free decimal value so round CLI
            // inputs produce round degree values (hue=150 → 90.0,
            // not 89.99999…).
            let hue_clean = f32j(*hue).as_f64().unwrap_or(f64::from(*hue));
            let hue_degrees = (hue_clean - 100.0) * 1.8;
            wrap(
                chain,
                "video.modulate",
                json!({
                    "brightness": f32j(*brightness),
                    "saturation": f32j(*saturation),
                    "hue_degrees": hue_degrees,
                }),
            )
        }
        Op::Level {
            black,
            gamma,
            white,
        } => wrap(
            chain,
            "video.level",
            json!({ "black": black, "gamma": f32j(*gamma), "white": white }),
        ),
        Op::Normalize => wrap(chain, "video.normalize", json!({})),
        Op::Threshold { value } => wrap(chain, "video.threshold", json!({ "value": value })),
        Op::Posterize { levels } => wrap(chain, "video.posterize", json!({ "levels": levels })),
        Op::Solarize { value } => wrap(chain, "video.solarize", json!({ "value": value })),
        Op::Colorspace(cs) => {
            let lower = cs.to_ascii_lowercase();
            if lower == "gray" || lower == "grey" {
                return wrap(chain, "video.grayscale", json!({ "preserve_alpha": true }));
            }
            // `rgb` / `srgb` are recorded no-ops — input keeps its
            // colourspace, downstream encoder converts as needed.
            chain
        }
        Op::Vignette {
            radius,
            sigma,
            x,
            y,
        } => wrap(
            chain,
            "video.vignette",
            json!({
                "x": f32j(*x),
                "y": f32j(*y),
                "radius": f32j(*radius),
                "sigma": f32j(*sigma),
            }),
        ),
        Op::Colorize { color, amount } => wrap(
            chain,
            "video.colorize",
            json!({
                "color": [color[0], color[1], color[2], color[3]],
                "amount": f32j(*amount),
            }),
        ),
        Op::Equalize => wrap(chain, "video.equalize", json!({})),
        Op::AutoGamma => wrap(chain, "video.auto-gamma", json!({})),
        Op::Trim { fuzz } => {
            // The image-filter `Trim` factory accepts `fuzz` as a
            // 0..=255 byte plus an optional `background` array. We
            // forward only `fuzz` here — the args parser doesn't
            // expose a per-`-trim` colour override yet, so the
            // factory falls back to its corner-pixel auto-detection
            // (matching IM's behaviour when neither `-bordercolor`
            // nor `-background` is set).
            wrap(chain, "video.trim", json!({ "fuzz": fuzz }))
        }
        Op::Roll { dx, dy } => {
            // Signed circular shift; the `oxideav-image-filter`
            // `Roll` factory accepts `dx`/`dy` (also `x`/`y`
            // aliases). Pixels that fall off one edge wrap around to
            // the opposite edge so the visible image translates as a
            // rigid block; width/height stay unchanged, so no shape
            // recovery is needed downstream.
            wrap(chain, "video.roll", json!({ "dx": dx, "dy": dy }))
        }
    }
}

/// Shared tail for both planners: fold sink-side state into the codec
/// params, resolve the output codec, assemble the single-output
/// [`Job`], and enforce the schema-validation guarantee.
fn finish_job(
    plan: &ConvertPlan,
    ctx: &RuntimeContext,
    chain: TrackInput,
    mut sink: SinkSide,
) -> Result<Job, Error> {
    // Sink-side metadata handling.
    if sink.strip_metadata {
        sink.codec_params["strip_metadata"] = json!(true);
    }
    if let Some(ref f) = sink.format_override {
        sink.codec_params["format"] = json!(f);
    }

    // The pipeline insists every filter-terminated track carry an
    // output codec — frames can't be stream-copied.  Infer from the
    // output extension (or `-format` override) so the IM-style
    // `convert in.png -resize 64x64 out.jpg` works without the user
    // stating the obvious.
    let codec = codec_for_output(sink.format_override.as_deref(), &plan.output, ctx);

    let track = TrackSpec {
        input: chain,
        codec,
        params: sink.codec_params,
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

    let job = Job {
        outputs,
        aliases: IndexMap::new(),
        threads: None,
    };
    // Guarantee: every job this planner emits satisfies the pipeline
    // schema's own invariants (non-empty source, non-blank codec ids,
    // …). Validating here turns a planner bug into a typed error at
    // plan time instead of an opaque executor failure later.
    job.validate()?;
    Ok(job)
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

    // ---- plan_to_render3d_job coverage (Phase C-3b) ----

    use crate::op::{CameraSpec as OpCameraSpec, LightSpec as OpLightSpec};

    fn render3d_plan(input: &str, output: &str, ops: Vec<Op>, opts: Mesh3DOptions) -> ConvertPlan {
        ConvertPlan {
            input: input.into(),
            input_pages: None,
            ops,
            output: output.into(),
            output_template: None,
            ping: false,
            probe: false,
            probe_json: false,
            probe_watch: false,
            mesh3d_options: opts,
        }
    }

    /// Bare `convert cube.stl out.png` lowers to a single Render3D
    /// source with the scanline backend, no surrounding filters, and
    /// the default 1024×1024 canvas with transparent black background.
    /// `opts` carries every key the renderer needs at default values.
    #[test]
    fn render3d_defaults_emit_scanline_source_at_1024x1024() {
        let plan = render3d_plan("cube.stl", "out.png", vec![], Mesh3DOptions::default());
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        assert_eq!(job.outputs.len(), 1);
        let out = job.outputs.values().next().unwrap();
        assert_eq!(out.all.len(), 1);
        let track = &out.all[0];
        let node = match &track.input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D, got {other:?}"),
        };
        assert_eq!(node.source, "cube.stl");
        assert_eq!(node.backend, RENDER3D_DEFAULT_BACKEND);
        assert_eq!(node.backend, "scanline");
        assert_eq!(node.opts["width"], 1024);
        assert_eq!(node.opts["height"], 1024);
        let bg = node.opts["background"]
            .as_array()
            .expect("background array");
        assert_eq!(bg.len(), 4);
        for c in bg {
            assert_eq!(c.as_u64(), Some(0));
        }
        assert_eq!(node.opts["shading"], "flat");
        assert_eq!(node.opts["projection"], "perspective");
        assert!((node.opts["fov_deg"].as_f64().unwrap() - 60.0).abs() < 1e-6);
        assert_eq!(node.opts["aa"], 1);
        // Default light spec: 45°/45°/1.0 — matches LightSpec::default_light
        // on the renderer side.
        let l = &node.opts["light"];
        assert!((l["azimuth_deg"].as_f64().unwrap() - 45.0).abs() < 1e-6);
        assert!((l["elevation_deg"].as_f64().unwrap() - 45.0).abs() < 1e-6);
        assert!((l["intensity"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        // Auto-frame camera = null.
        assert!(node.opts["camera"].is_null());
    }

    /// `-resize WxH` seeds the render canvas dims on the opts struct
    /// and does NOT survive as a `video.resize` filter node — the
    /// renderer produces the canvas at the requested size directly.
    #[test]
    fn resize_is_absorbed_into_render_dims_not_emitted_as_filter() {
        let plan = render3d_plan(
            "scene.gltf",
            "out.png",
            vec![Op::Resize {
                width: 800,
                height: 600,
                mode: ResizeMode::Default,
            }],
            Mesh3DOptions::default(),
        );
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = match &track.input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D directly (resize absorbed), got {other:?}"),
        };
        assert_eq!(node.opts["width"], 800);
        assert_eq!(node.opts["height"], 600);
    }

    /// `-background COLOR` seeds the render canvas clear colour on the
    /// opts struct and does NOT survive as a separate node.
    #[test]
    fn background_op_is_absorbed_into_render_bg() {
        let plan = render3d_plan(
            "cube.stl",
            "out.png",
            vec![Op::Background([12, 34, 56, 200])],
            Mesh3DOptions::default(),
        );
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        let node = match &track.input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D directly, got {other:?}"),
        };
        let bg = node.opts["background"]
            .as_array()
            .expect("background array");
        assert_eq!(bg[0], 12);
        assert_eq!(bg[1], 34);
        assert_eq!(bg[2], 56);
        assert_eq!(bg[3], 200);
    }

    /// `-bg` on `Mesh3DOptions` wins over a preceding `-background`
    /// op (mirrors `mesh3d_render::pick_render_bg`'s precedence).
    #[test]
    fn bg_option_overrides_background_op() {
        let opts = Mesh3DOptions {
            bg: Some([10, 20, 30, 255]),
            ..Mesh3DOptions::default()
        };
        let plan = render3d_plan(
            "cube.stl",
            "out.png",
            vec![Op::Background([99, 99, 99, 255])],
            opts,
        );
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let node = match &job.outputs.values().next().unwrap().all[0].input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D, got {other:?}"),
        };
        let bg = node.opts["background"].as_array().unwrap();
        assert_eq!(bg[0], 10);
        assert_eq!(bg[1], 20);
        assert_eq!(bg[2], 30);
        assert_eq!(bg[3], 255);
    }

    /// Every `Mesh3DRenderMode` round-trips through the opts JSON via a
    /// stable lowercase tag.
    #[test]
    fn render_mode_round_trips_through_opts() {
        let cases = [
            (Mesh3DRenderMode::Flat, "flat"),
            (Mesh3DRenderMode::Wireframe, "wireframe"),
            (Mesh3DRenderMode::Gouraud, "gouraud"),
            (Mesh3DRenderMode::Phong, "phong"),
            (Mesh3DRenderMode::NormalDebug, "normal-debug"),
            (Mesh3DRenderMode::DepthDebug, "depth-debug"),
        ];
        for (mode, tag) in cases {
            let opts = Mesh3DOptions {
                render_mode: Some(mode),
                ..Mesh3DOptions::default()
            };
            let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
            let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
            let node = match &job.outputs.values().next().unwrap().all[0].input {
                TrackInput::Render3D(n) => n,
                other => panic!("expected Render3D, got {other:?}"),
            };
            assert_eq!(node.opts["shading"], tag, "mode={mode:?}");
        }
    }

    /// Projection mode round-trips between `Mesh3DOptions::projection`
    /// and the opts JSON.
    #[test]
    fn projection_mode_round_trips_through_opts() {
        for (mode, tag) in [
            (ProjectionMode::Perspective, "perspective"),
            (ProjectionMode::Orthographic, "orthographic"),
        ] {
            let opts = Mesh3DOptions {
                projection: Some(mode),
                ..Mesh3DOptions::default()
            };
            let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
            let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
            let node = match &job.outputs.values().next().unwrap().all[0].input {
                TrackInput::Render3D(n) => n,
                other => panic!("expected Render3D, got {other:?}"),
            };
            assert_eq!(node.opts["projection"], tag, "mode={mode:?}");
        }
    }

    /// `-light A,E,I` lands on opts as a three-key sub-object.
    #[test]
    fn light_override_round_trips_through_opts() {
        let opts = Mesh3DOptions {
            light: Some(OpLightSpec {
                azimuth_deg: 12.0,
                elevation_deg: 34.0,
                intensity: 0.75,
            }),
            ..Mesh3DOptions::default()
        };
        let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let node = match &job.outputs.values().next().unwrap().all[0].input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D, got {other:?}"),
        };
        let l = &node.opts["light"];
        assert!((l["azimuth_deg"].as_f64().unwrap() - 12.0).abs() < 1e-6);
        assert!((l["elevation_deg"].as_f64().unwrap() - 34.0).abs() < 1e-6);
        assert!((l["intensity"].as_f64().unwrap() - 0.75).abs() < 1e-6);
    }

    /// `-camera E,A,D` lands on opts as a three-key sub-object; absence
    /// stays `null`.
    #[test]
    fn camera_override_round_trips_through_opts() {
        let opts = Mesh3DOptions {
            camera: Some(OpCameraSpec {
                elevation_deg: 30.0,
                azimuth_deg: 45.0,
                distance: 1.5,
            }),
            ..Mesh3DOptions::default()
        };
        let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let node = match &job.outputs.values().next().unwrap().all[0].input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D, got {other:?}"),
        };
        let c = &node.opts["camera"];
        assert!((c["elevation_deg"].as_f64().unwrap() - 30.0).abs() < 1e-6);
        assert!((c["azimuth_deg"].as_f64().unwrap() - 45.0).abs() < 1e-6);
        assert!((c["distance"].as_f64().unwrap() - 1.5).abs() < 1e-6);
    }

    /// `-fov DEGREES` round-trips through opts as a float.
    #[test]
    fn fov_override_round_trips_through_opts() {
        let opts = Mesh3DOptions {
            fov_deg: Some(35.0),
            ..Mesh3DOptions::default()
        };
        let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let node = match &job.outputs.values().next().unwrap().all[0].input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D, got {other:?}"),
        };
        assert!((node.opts["fov_deg"].as_f64().unwrap() - 35.0).abs() < 1e-6);
    }

    /// `-aa N` round-trips through opts; the helper clamps to `1..=8`
    /// to match the renderer's documented range.
    #[test]
    fn aa_round_trips_through_opts_and_clamps_to_range() {
        for in_aa in [1_u32, 2, 4, 8] {
            let opts = Mesh3DOptions {
                aa: Some(in_aa),
                ..Mesh3DOptions::default()
            };
            let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
            let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
            let node = match &job.outputs.values().next().unwrap().all[0].input {
                TrackInput::Render3D(n) => n,
                other => panic!("expected Render3D, got {other:?}"),
            };
            assert_eq!(node.opts["aa"], in_aa, "in_aa={in_aa}");
        }
        // Out-of-range values clamp to the [1, 8] window: 0 → 1, 9 → 8.
        for (in_aa, expected) in [(0_u32, 1_u32), (9, 8), (100, 8)] {
            let opts = Mesh3DOptions {
                aa: Some(in_aa),
                ..Mesh3DOptions::default()
            };
            let plan = render3d_plan("cube.stl", "out.png", vec![], opts);
            let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
            let node = match &job.outputs.values().next().unwrap().all[0].input {
                TrackInput::Render3D(n) => n,
                other => panic!("expected Render3D, got {other:?}"),
            };
            assert_eq!(
                node.opts["aa"], expected,
                "in_aa={in_aa} expected clamp→{expected}"
            );
        }
    }

    /// Post-rasterisation ops (rotate / flip / negate / sharpen / strip
    /// / quality) survive as filter nodes wrapping the Render3D source,
    /// preserving source order from innermost (= first parsed) to
    /// outermost (= last parsed).
    #[test]
    fn post_raster_ops_wrap_the_render3d_source_in_source_order() {
        let plan = render3d_plan(
            "scene.gltf",
            "out.png",
            vec![
                Op::Rotate { degrees: 90 },
                Op::Negate,
                Op::Sharpen {
                    radius: 1,
                    sigma: 0.5,
                },
                Op::Strip,
                Op::Quality(85),
            ],
            Mesh3DOptions::default(),
        );
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        // Quality + strip land on codec params, not as filter nodes.
        assert_eq!(track.params["quality"], 85);
        assert_eq!(track.params["strip_metadata"], true);
        // Walk the chain: outermost (last in source order) is sharpen,
        // then negate, then rotate, then the Render3D leaf.
        let outer = match &track.input {
            TrackInput::Filter(f) => f,
            other => panic!("expected outer filter, got {other:?}"),
        };
        assert_eq!(outer.filter, "video.sharpen");
        let mid = match outer.input.as_ref() {
            TrackInput::Filter(f) => f,
            other => panic!("expected mid filter, got {other:?}"),
        };
        assert_eq!(mid.filter, "video.negate");
        let inner = match mid.input.as_ref() {
            TrackInput::Filter(f) => f,
            other => panic!("expected inner filter, got {other:?}"),
        };
        assert_eq!(inner.filter, "video.rotate");
        assert_eq!(inner.params["degrees"], 90);
        // The recursion bottoms out at a Render3D node (not Source).
        match inner.input.as_ref() {
            TrackInput::Render3D(n) => {
                assert_eq!(n.source, "scene.gltf");
                assert_eq!(n.backend, "scanline");
            }
            other => panic!("expected Render3D leaf, got {other:?}"),
        }
    }

    /// `Op::Thumbnail` strips metadata and lets the renderer absorb the
    /// resize half — no `video.resize` filter survives on a 3D→raster
    /// job.
    #[test]
    fn thumbnail_absorbs_resize_and_sets_strip_metadata() {
        let plan = render3d_plan(
            "cube.stl",
            "out.png",
            vec![Op::Thumbnail {
                width: 256,
                height: 256,
                mode: ResizeMode::Fill,
            }],
            Mesh3DOptions::default(),
        );
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        // No video.resize filter because thumbnail absorbed.  The
        // renderer canvas stays at the helper's default since
        // Op::Thumbnail isn't consulted by pick_render3d_dims.  Strip
        // metadata still lands on codec params per IM's contract.
        assert_eq!(track.params["strip_metadata"], true);
        match &track.input {
            TrackInput::Render3D(_) => {}
            other => panic!("expected Render3D (no surviving filters), got {other:?}"),
        }
    }

    /// The output codec resolves through the same `ContainerRegistry`
    /// lookup `plan_to_job` uses — register an extension, observe the
    /// resolved codec id on the track.
    #[test]
    fn render3d_output_codec_resolves_via_registry() {
        let mut ctx = RuntimeContext::new();
        ctx.containers.register_extension("png", "png_codec");
        let plan = render3d_plan("scene.gltf", "out.png", vec![], Mesh3DOptions::default());
        let job = plan_to_render3d_job(&plan, &ctx).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.codec.as_deref(), Some("png_codec"));
    }

    /// `-format FMT` override seeds the codec params bag plus the
    /// registry-resolved codec; mirrors `plan_to_job`'s shape.
    #[test]
    fn render3d_format_override_threads_through_to_codec_params() {
        let mut ctx = RuntimeContext::new();
        ctx.containers.register_extension("jpg", "jpeg_codec");
        let plan = render3d_plan(
            "scene.gltf",
            "out.unrelated",
            vec![Op::Format("jpg".into())],
            Mesh3DOptions::default(),
        );
        let job = plan_to_render3d_job(&plan, &ctx).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.codec.as_deref(), Some("jpeg_codec"));
        assert_eq!(track.params["format"], "jpg");
    }

    /// `-define KEY=VALUE` lands on codec params on the 3D path same as
    /// on the regular pipeline path.
    #[test]
    fn render3d_define_lands_on_codec_params() {
        let plan = render3d_plan(
            "cube.stl",
            "out.png",
            vec![
                Op::Define {
                    key: "png:strip-comments".into(),
                    value: None,
                },
                Op::Define {
                    key: "jpeg:dct-method".into(),
                    value: Some("float".into()),
                },
            ],
            Mesh3DOptions::default(),
        );
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let track = &job.outputs.values().next().unwrap().all[0];
        assert_eq!(track.params["png:strip-comments"], true);
        assert_eq!(track.params["jpeg:dct-method"], "float");
    }

    /// The full opts struct round-trips through `serde_json` (every
    /// field is present, every field is a value the consumer's typed
    /// deserialiser can read).
    #[test]
    fn render3d_opts_carry_every_documented_field() {
        let plan = render3d_plan("cube.stl", "out.png", vec![], Mesh3DOptions::default());
        let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
        let node = match &job.outputs.values().next().unwrap().all[0].input {
            TrackInput::Render3D(n) => n,
            other => panic!("expected Render3D, got {other:?}"),
        };
        for key in [
            "width",
            "height",
            "background",
            "shading",
            "projection",
            "fov_deg",
            "light",
            "camera",
            "aa",
        ] {
            assert!(
                node.opts.get(key).is_some(),
                "opts is missing documented field {key:?} — full opts: {}",
                node.opts
            );
        }
    }

    // ---- schema-validation + JSON round-trip guarantees ----

    /// Every op the planner knows how to lower, exercised once. Kept
    /// as a function so both the validation sweep and the round-trip
    /// sweep iterate the same corpus — adding an `Op` variant without
    /// extending this list is caught by the exhaustive `match` in
    /// `plan_to_job` going non-exhaustive, and adding it HERE keeps
    /// the guarantees covering it.
    fn representative_op_corpus() -> Vec<Vec<Op>> {
        vec![
            vec![],
            vec![Op::Resize {
                width: 64,
                height: 32,
                mode: ResizeMode::Fill,
            }],
            vec![Op::Thumbnail {
                width: 128,
                height: 128,
                mode: ResizeMode::Default,
            }],
            vec![Op::Define {
                key: "jpeg:dct-method".into(),
                value: Some("float".into()),
            }],
            vec![Op::Blur {
                radius: 2,
                sigma: 1.0,
            }],
            vec![Op::Edge { radius: 1 }],
            vec![Op::Colors {
                count: 16,
                dither: Dither::FloydSteinberg,
            }],
            vec![Op::Format("png".into()), Op::Quality(85), Op::Strip],
            vec![Op::Density(300), Op::Background([1, 2, 3, 4])],
            vec![Op::Alpha(crate::op::AlphaOp::Remove)],
            vec![Op::Rotate { degrees: -270 }, Op::Flip, Op::Flop],
            vec![Op::Crop {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            }],
            vec![Op::Extent {
                width: 10,
                height: 20,
                x: -1,
                y: 1,
                bg: [0, 0, 0, 255],
            }],
            vec![Op::Negate],
            vec![Op::Sharpen {
                radius: 1,
                sigma: 0.5,
            }],
            vec![Op::Unsharp {
                radius: 2,
                sigma: 1.0,
                amount: 0.8,
                threshold: 3,
            }],
            vec![Op::Gamma { value: 2.2 }],
            vec![Op::BrightnessContrast {
                brightness: 10.0,
                contrast: -5.0,
            }],
            vec![Op::Contrast { delta: 2 }],
            vec![Op::Sepia { threshold: 0.8 }],
            vec![Op::Modulate {
                brightness: 110.0,
                saturation: 90.0,
                hue: 150.0,
            }],
            vec![Op::Level {
                black: 16,
                gamma: 1.1,
                white: 235,
            }],
            vec![Op::Normalize, Op::Equalize, Op::AutoGamma],
            vec![Op::Threshold { value: 128 }],
            vec![Op::Posterize { levels: 4 }],
            vec![Op::Solarize { value: 200 }],
            vec![Op::Colorspace("gray".into())],
            vec![Op::Colorspace("srgb".into())],
            vec![Op::Vignette {
                radius: 50.0,
                sigma: 25.0,
                x: 0.5,
                y: 0.5,
            }],
            vec![Op::Colorize {
                color: [200, 100, 50, 255],
                amount: 0.4,
            }],
            vec![Op::Trim { fuzz: 12 }],
            vec![Op::Roll { dx: -3, dy: 7 }],
            // A deep mixed chain — closest to real-world flag soup.
            vec![
                Op::Density(150),
                Op::Resize {
                    width: 800,
                    height: 600,
                    mode: ResizeMode::Shrink,
                },
                Op::Background([255, 255, 255, 255]),
                Op::Alpha(crate::op::AlphaOp::Off),
                Op::Sharpen {
                    radius: 1,
                    sigma: 0.5,
                },
                Op::Colorspace("gray".into()),
                Op::Colors {
                    count: 2,
                    dither: Dither::FloydSteinberg,
                },
                Op::Quality(92),
                Op::Strip,
                Op::Define {
                    key: "png:compression-level".into(),
                    value: Some("9".into()),
                },
            ],
        ]
    }

    /// GUARANTEE: every job the planner emits passes the pipeline
    /// schema's own `Job::validate()`. `plan_to_job` also enforces
    /// this at runtime now — the test documents the invariant and
    /// keeps the corpus honest.
    #[test]
    fn every_emitted_job_passes_pipeline_validation() {
        for (i, ops) in representative_op_corpus().into_iter().enumerate() {
            let job = plan_to_job(&plan_with(ops.clone()), &empty_ctx())
                .unwrap_or_else(|e| panic!("corpus[{i}] {ops:?} failed to plan: {e}"));
            job.validate()
                .unwrap_or_else(|e| panic!("corpus[{i}] {ops:?} emitted invalid job: {e}"));
        }
    }

    /// GUARANTEE: every emitted job survives a full trip through the
    /// pipeline's JSON dialect — serialise, re-parse, re-serialise,
    /// byte-identical. This is what makes `convert`-planned jobs
    /// storable / replayable as `oxideav run` documents.
    #[test]
    fn every_emitted_job_round_trips_through_job_json() {
        let mut ctx = RuntimeContext::new();
        ctx.containers.register_extension("jpg", "mjpeg");
        for (i, ops) in representative_op_corpus().into_iter().enumerate() {
            let job = plan_to_job(&plan_with(ops.clone()), &ctx).unwrap();
            let first = job.to_json_pretty();
            let reparsed = Job::from_json(&first).unwrap_or_else(|e| {
                panic!("corpus[{i}] {ops:?} JSON did not re-parse: {e}\n{first}")
            });
            reparsed
                .validate()
                .unwrap_or_else(|e| panic!("corpus[{i}] re-parsed job invalid: {e}"));
            let second = reparsed.to_json_pretty();
            assert_eq!(
                first, second,
                "corpus[{i}] {ops:?}: JSON round-trip not stable"
            );
        }
    }

    /// Same two guarantees for the 3D→raster planner: validation +
    /// byte-stable JSON round-trip, across default and fully-specified
    /// `Mesh3DOptions` plus post-raster op chains.
    #[test]
    fn every_emitted_render3d_job_validates_and_round_trips() {
        let option_corpus = vec![
            Mesh3DOptions::default(),
            Mesh3DOptions {
                render_mode: Some(Mesh3DRenderMode::Phong),
                projection: Some(ProjectionMode::Orthographic),
                fov_deg: Some(35.0),
                light: Some(crate::op::LightSpec {
                    azimuth_deg: 12.0,
                    elevation_deg: 34.0,
                    intensity: 0.75,
                }),
                camera: Some(crate::op::CameraSpec {
                    elevation_deg: 30.0,
                    azimuth_deg: 45.0,
                    distance: 1.5,
                }),
                bg: Some([10, 20, 30, 255]),
                aa: Some(4),
                ..Mesh3DOptions::default()
            },
        ];
        let op_corpus = vec![
            vec![],
            vec![
                Op::Resize {
                    width: 800,
                    height: 600,
                    mode: ResizeMode::Default,
                },
                Op::Background([255, 255, 255, 255]),
                Op::Rotate { degrees: 90 },
                Op::Negate,
                Op::Quality(85),
                Op::Strip,
            ],
        ];
        for opts in &option_corpus {
            for ops in &op_corpus {
                let plan = render3d_plan("scene.gltf", "out.png", ops.clone(), opts.clone());
                let job = plan_to_render3d_job(&plan, &empty_ctx()).unwrap();
                job.validate()
                    .unwrap_or_else(|e| panic!("render3d job invalid: {e} (ops={ops:?})"));
                let first = job.to_json_pretty();
                let reparsed = Job::from_json(&first)
                    .unwrap_or_else(|e| panic!("render3d JSON did not re-parse: {e}\n{first}"));
                let second = reparsed.to_json_pretty();
                assert_eq!(first, second, "render3d JSON round-trip not stable");
                // The Render3D node itself must survive structurally:
                // source / backend / opts equal after the trip.
                let orig = match &job.outputs.values().next().unwrap().all[0].input.leaf() {
                    TrackInput::Render3D(n) => (*n).clone(),
                    other => panic!("expected Render3D leaf, got {other:?}"),
                };
                let back = match &reparsed.outputs.values().next().unwrap().all[0]
                    .input
                    .leaf()
                {
                    TrackInput::Render3D(n) => (*n).clone(),
                    other => panic!("expected Render3D leaf after re-parse, got {other:?}"),
                };
                assert_eq!(orig, back, "Render3D node changed across the round-trip");
            }
        }
    }

    /// An empty input path can never reach the executor: the planner's
    /// validation hook rejects it with the pipeline's typed error
    /// instead of letting an unopenable "" source fail deep inside a
    /// run.
    #[test]
    fn empty_input_path_is_rejected_at_plan_time() {
        let mut plan = plan_with(vec![]);
        plan.input = String::new();
        let err = plan_to_job(&plan, &empty_ctx()).expect_err("empty source must not plan");
        let msg = format!("{err}");
        assert!(msg.contains("empty `from`"), "got: {msg}");
    }
}
