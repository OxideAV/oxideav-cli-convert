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
                bang,
            } => {
                // IM's `!` skips aspect-ratio preservation. Our
                // Resize always takes literal target dims, so bang vs
                // non-bang maps to the same filter for now.  When we
                // add aspect-preserving Resize mode the non-bang path
                // gates it.
                let _ = bang;
                chain = wrap(
                    chain,
                    "video.resize",
                    json!({
                        "width": width,
                        "height": height,
                        "interpolation": "bilinear"
                    }),
                );
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

    fn plan_with(ops: Vec<Op>) -> ConvertPlan {
        ConvertPlan {
            input: "in.png".into(),
            input_pages: None,
            ops,
            output: "out.jpg".into(),
            output_template: None,
            ping: false,
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
                bang: false,
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
                    bang: false,
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
