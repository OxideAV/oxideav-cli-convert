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

use crate::op::{ConvertPlan, Dither, Op};
use indexmap::IndexMap;
use oxideav_core::{Error, RuntimeContext};
use oxideav_pipeline::{FilterNode, Job, OutputSpec, SourceRef, TrackInput, TrackSpec};
use serde_json::{json, Value};

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
    // to the sink.
    let mut codec_params = json!({});
    let mut strip_metadata = false;
    let mut format_override: Option<String> = None;

    // Starting input: the file itself.
    let mut chain: TrackInput = TrackInput::Source(SourceRef {
        from: plan.input.clone(),
    });

    // A filter-chain step — wrap the current chain in a FilterNode.
    let wrap = |prev: TrackInput, filter: &str, params: Value| {
        TrackInput::Filter(FilterNode {
            filter: filter.to_string(),
            params,
            input: Box::new(prev),
        })
    };

    for op in &plan.ops {
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
}
