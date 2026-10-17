//! Still-image sink planning for the regular pipeline route.
//!
//! When the output resolves to an image codec (`png`, `pbm`, `heif`,
//! `tga`, …) the planner knows more than the pipeline's generic
//! defaults do:
//!
//! * **Which stream.** A HEIF / AVIF file with an image-sequence track
//!   opens as stream 0 = the primary still plus one stream per track;
//!   a single-image muxer refuses two streams. The planner probes the
//!   input's stream list (container header only — no decode) and pins
//!   the track to one video stream via a
//!   [`StreamSelector`](oxideav_pipeline::schema::StreamSelector).
//! * **Which encoder input layout.** Left alone, the pipeline converts
//!   a source the encoder does not accept to the encoder's *first*
//!   accepted layout — RGBA for PNG (an opaque alpha channel the
//!   source never had, 33% more bytes through deflate), and 1-bit
//!   `MonoBlack` for the Netpbm encoder (a thresholded bitmap in a
//!   `.ppm`). [`pick_sink_format`] instead picks the accepted layout
//!   that loses nothing the source carries (colour, alpha, bit depth)
//!   at the fewest bits per pixel, and the planner pins it with an
//!   explicit convert node.
//! * **Which codec.** Container names and codec ids differ for some
//!   formats (the `jpeg` container carries the `mjpeg` codec);
//!   [`resolve_output_codec`] maps the output extension onto a codec
//!   that actually has an encoder.
//!
//! Everything here is planning only: the pipeline executor still does
//! the demux, decode, conversion, encode and mux.

use oxideav_core::{
    CodecId, Error, MediaType, OptionField, OptionKind, PixelFormat, ReadSeek, RuntimeContext,
    SourceOutput, StreamInfo, TimeBase,
};
use oxideav_pixfmt::FormatInfo;

/// Layout family an output extension implies on its own, independent
/// of the codec behind it. Only the Netpbm family names its layout in
/// the extension (`.pbm` bitmap, `.pgm` graymap, `.ppm` pixmap);
/// every other extension leaves the choice to [`pick_sink_format`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutHint {
    /// No constraint from the extension.
    Any,
    /// 1-bit bilevel (`.pbm`).
    Mono,
    /// Grayscale without alpha (`.pgm`).
    Gray,
    /// Colour without alpha (`.ppm`).
    Color,
}

impl LayoutHint {
    /// Hint for an output path's extension (case-insensitive).
    pub fn for_output(output: &str) -> LayoutHint {
        match ext_of(output).map(|e| e.to_ascii_lowercase()).as_deref() {
            Some("pbm") => LayoutHint::Mono,
            Some("pgm") => LayoutHint::Gray,
            Some("ppm") => LayoutHint::Color,
            _ => LayoutHint::Any,
        }
    }

    fn admits(self, s: &Shape) -> bool {
        match self {
            LayoutHint::Any => true,
            LayoutHint::Mono => s.mono,
            LayoutHint::Gray => s.gray && !s.mono && !s.alpha,
            LayoutHint::Color => !s.gray && !s.palette && !s.alpha,
        }
    }
}

/// What a pixel layout can carry, as far as target selection cares.
#[derive(Clone, Copy, Debug)]
struct Shape {
    gray: bool,
    mono: bool,
    palette: bool,
    alpha: bool,
    depth: u8,
    bits_per_pixel: u32,
}

fn shape(f: PixelFormat) -> Shape {
    let info = FormatInfo::of(f);
    let mono = matches!(f, PixelFormat::MonoBlack | PixelFormat::MonoWhite);
    let gray = mono
        || matches!(
            f,
            PixelFormat::Gray8
                | PixelFormat::Gray10Le
                | PixelFormat::Gray12Le
                | PixelFormat::Gray16Le
                | PixelFormat::GrayF32Le
                | PixelFormat::Ya8
                | PixelFormat::Ya16Le
        );
    Shape {
        gray,
        mono,
        palette: info.is_palette,
        alpha: info.has_alpha,
        depth: if mono { 1 } else { info.bit_depth },
        bits_per_pixel: f.bits_per_pixel_approx(),
    }
}

/// Information lost converting `src` to `dst`, as an ordered penalty:
/// colour → bilevel ≫ colour → palette ≫ colour → gray ≫ alpha ≫ bits
/// of depth. `want_depth` is the bit depth the output should keep
/// (the source's own, or a `-depth N` request).
fn loss(src: &Shape, dst: &Shape, want_depth: u8) -> u32 {
    let mut l = 0;
    if dst.mono && !src.mono {
        l += 4000;
    } else if dst.palette && !src.palette {
        l += 2000;
    }
    if dst.gray && !src.gray {
        l += 1000;
    }
    if src.alpha && !dst.alpha {
        l += 100;
    }
    if dst.depth < want_depth {
        l += 10 * u32::from(want_depth - dst.depth);
    }
    l
}

/// Pick the encoder input layout for a `src`-layout source.
///
/// * `accepted` — the encoder's declared accepted layouts, in its
///   preference order (empty = accepts anything).
/// * `hint` — the output extension's implied layout family.
/// * `depth` — a `-depth N` request: caps (and targets) the output
///   bit depth.
///
/// Returns `None` when the pipeline's own behaviour is already right
/// (the encoder accepts `src` as-is, or its first accepted layout
/// loses nothing a better candidate would keep), `Some(fmt)` when an
/// explicit conversion to `fmt` should be planned. Candidates the
/// pixfmt converter cannot reach from `src` are never picked.
pub fn pick_sink_format(
    src: PixelFormat,
    accepted: &[PixelFormat],
    hint: LayoutHint,
    depth: Option<u8>,
) -> Option<PixelFormat> {
    if accepted.is_empty() {
        return None;
    }
    let s = shape(src);
    let want_depth = depth.unwrap_or(s.depth);
    let reachable: Vec<(usize, PixelFormat, Shape)> = accepted
        .iter()
        .enumerate()
        .filter(|(_, &f)| oxideav_pixfmt::supports(src, f))
        .map(|(i, &f)| (i, f, shape(f)))
        .collect();
    // Narrow by the extension's family, then by the depth request —
    // each only when it leaves at least one candidate.
    let mut cands: Vec<_> = reachable
        .iter()
        .filter(|(_, _, sh)| hint.admits(sh))
        .cloned()
        .collect();
    if cands.is_empty() {
        cands = reachable.clone();
    }
    if let Some(d) = depth {
        let capped: Vec<_> = cands
            .iter()
            .filter(|(_, _, sh)| sh.depth <= d)
            .cloned()
            .collect();
        if !capped.is_empty() {
            cands = capped;
        }
    }
    let rank = |sh: &Shape| (loss(&s, sh, want_depth), sh.bits_per_pixel);
    let &(_, best, best_shape) = cands.iter().min_by_key(|(i, _, sh)| (rank(sh), *i))?;

    let explicit = hint != LayoutHint::Any || depth.is_some();
    if explicit {
        // The user (or the extension) asked for a layout: honour it
        // even when the encoder would take the source unchanged.
        return (best != src).then_some(best);
    }
    if accepted.contains(&src) {
        return None;
    }
    // The pipeline default is the first accepted layout; only
    // override it when the pick is strictly better.
    let default = accepted[0];
    if !oxideav_pixfmt::supports(src, default) || rank(&best_shape) < rank(&shape(default)) {
        Some(best)
    } else {
        None
    }
}

/// Codec ids that code pictures as YCbCr with subsampled chroma by
/// convention — the JPEG family — for which a packed-RGB source is
/// coded as 4:2:0 YCbCr (the `convert` default below quality 90, and
/// what every JPEG consumer expects) rather than as the RGB-coded
/// JPEG the encoder also accepts.
const CHROMA_SUBSAMPLED_CODECS: &[&str] = &["mjpeg"];

/// 8-bit packed RGB layouts (with or without alpha).
fn is_packed_rgb8(f: PixelFormat) -> bool {
    matches!(
        f,
        PixelFormat::Rgb24
            | PixelFormat::Bgr24
            | PixelFormat::Rgba
            | PixelFormat::Bgra
            | PixelFormat::Argb
            | PixelFormat::Abgr
    )
}

/// The YCbCr layout a JPEG-family encoder (`codec`) codes an 8-bit
/// packed-RGB `src` as: 4:2:0 (`YuvJ420P`) below quality 90, 4:4:4
/// (`YuvJ444P`) from 90 up — the `convert` convention, which the
/// encoder's own default (keep the input layout: an RGB-coded JPEG)
/// does not follow. `None` when `codec` is not in the family, the
/// source is not packed 8-bit RGB, the user chose the chroma layout
/// (`chroma` / `sampling` options), or the encoder does not accept
/// the layout.
pub fn chroma_subsampled_default(
    codec: &str,
    src: PixelFormat,
    accepted: &[PixelFormat],
    quality: Option<u8>,
    options: &[(String, String)],
) -> Option<PixelFormat> {
    if !CHROMA_SUBSAMPLED_CODECS.contains(&codec) || !is_packed_rgb8(src) {
        return None;
    }
    if options
        .iter()
        .any(|(k, _)| k == "chroma" || k == "sampling")
    {
        return None;
    }
    let target = if quality.is_some_and(|q| q >= 90) {
        PixelFormat::YuvJ444P
    } else {
        PixelFormat::YuvJ420P
    };
    let takes = accepted.is_empty() || accepted.contains(&target);
    (takes && oxideav_pixfmt::supports(src, target)).then_some(target)
}

/// The `quality` `convert` writes with when the user gave none and
/// the encoder's default would not be what a `convert` user expects
/// of the format: the `avif` encoder defaults to lossless (a 12 MP
/// photo becomes a 20 MB file); `convert` writes AVIF at quality 60,
/// the same default the HEIF encoder applies to its AV1 items, so
/// `.avif` output is a lossy photo format by default and `-quality
/// 100` asks for lossless. Every other encoder keeps its own default.
pub fn default_quality(codec: &str) -> Option<u8> {
    match codec {
        "avif" => Some(60),
        _ => None,
    }
}

/// Resolve the output codec for `output` (or the `-format` override).
///
/// The container registered for the extension names the codec for
/// image formats whose container and codec share a name (`png`,
/// `pbm`, `heif`, …). When that name has no encoder the extension
/// itself, then a known container → codec alias, is tried. If none
/// of them has an encoder, a container with a registered muxer
/// (`webm`, `matroska`, `wav`, …) yields `None` — the codec is chosen
/// per stream — and anything else returns the container name
/// unchanged (the pipeline then reports what it cannot build).
pub fn resolve_output_codec(
    format_override: Option<&str>,
    output: &str,
    ctx: &RuntimeContext,
) -> Option<String> {
    let ext = format_override
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| ext_of(output).map(|s| s.to_ascii_lowercase()))?;
    let container = ctx.containers.container_for_extension(&ext)?.to_string();
    let has_enc = |id: &str| ctx.codecs.has_encoder(&CodecId::new(id));
    // The gateway's resolution first: the requested format name picks
    // the codec on a multi-brand container (`.avif` on the heif
    // container is the `avif` encoder, not the container's HEVC
    // default), and a container's default codec is the gateway's
    // (`jpeg` → `mjpeg`). Only an answer that names an image encoder
    // for a format this planner would also treat as an image is taken;
    // a container whose extension names no codec stays per-stream.
    let image_format = has_enc(&container)
        || has_enc(&ext)
        || CONTAINER_CODEC_ALIASES
            .iter()
            .any(|(c, codec)| *c == container && has_enc(codec));
    if image_format {
        if let Ok(target) =
            oxideav_image::resolve_target(ctx, &ext, &oxideav_image::SaveOptions::default())
        {
            if has_enc(target.codec()) {
                return Some(target.codec().to_string());
            }
        }
    }
    if has_enc(&container) {
        return Some(container);
    }
    if has_enc(&ext) {
        return Some(ext);
    }
    if let Some(&(_, codec)) = CONTAINER_CODEC_ALIASES
        .iter()
        .find(|(c, _)| *c == container)
    {
        if has_enc(codec) {
            return Some(codec.to_string());
        }
    }
    // A container that is not itself a codec (`.webm`, `.mkv`, `.wav`,
    // …): no single output codec — the planner picks per stream
    // (stream copy, or the container's default encoder; see
    // `crate::container_defaults`).
    if ctx.containers.muxer_names().any(|m| m == container) {
        return None;
    }
    Some(container)
}

/// Containers whose single image codec is registered under a
/// different id.
const CONTAINER_CODEC_ALIASES: &[(&str, &str)] = &[("jpeg", "mjpeg")];

/// Output extensions that name an *image sequence* brand of the HEIF
/// family, with the track codec a timed input is coded as: the
/// still-image encoder the container resolves to writes one picture
/// per file, so a timed source goes to the sequence muxer as a video
/// track instead (see [`sequence_codec`]).
const SEQUENCE_BRANDS: &[(&str, &str)] = &[("heics", "hevc"), ("heifs", "hevc"), ("avifs", "av1")];

/// The track codec for a sequence-brand output extension (`heics` /
/// `heifs` → `hevc`, `avifs` → `av1`) when that encoder is registered;
/// `None` for every other extension, or when the codec has no encoder
/// (the still path then applies and the muxer reports what it cannot
/// append).
pub fn sequence_codec(ext: &str, ctx: &RuntimeContext) -> Option<&'static str> {
    let ext = ext.to_ascii_lowercase();
    SEQUENCE_BRANDS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, c)| *c)
        .filter(|c| ctx.codecs.has_encoder(&CodecId::new(*c)))
}

/// Accepted input layouts of the encoder the pipeline will build for
/// `codec`: the preferred implementation (lowest priority value, then
/// earliest registration — the selection walk's first candidate).
/// `None` when no implementation of `codec` can encode video.
pub fn encoder_accepted_formats(ctx: &RuntimeContext, codec: &str) -> Option<Vec<PixelFormat>> {
    ctx.codecs
        .implementations(&CodecId::new(codec))
        .iter()
        .enumerate()
        .filter(|(_, i)| i.make_encoder.is_some() && i.caps.media_type == MediaType::Video)
        .min_by_key(|(order, i)| (i.caps.priority, *order))
        .map(|(_, i)| i.caps.accepted_pixel_formats.clone())
}

/// Build the encoder option list for a still-image output.
///
/// * `user` — `--opt KEY=VALUE` pairs in command-line order (a later
///   value for a key wins). Each key must be declared by the target
///   encoder's options schema and each value must parse as the
///   declared kind; otherwise a typed error names the key and the
///   accepted set.
/// * Extension-implied options are added when the user did not set
///   the key: a `.avif` / `.avifs` output written through an encoder
///   whose `codec` option offers `av1` gets `codec=av1` (the same
///   inference `oxideav transcode` makes). Other extensions (`.heic`,
///   …) name the encoder's default and add nothing.
///
/// Returns an empty list when there is nothing to set.
pub fn encoder_options(
    ctx: &RuntimeContext,
    codec: Option<&str>,
    ext: Option<&str>,
    user: &[(String, String)],
) -> oxideav_core::Result<Vec<(String, String)>> {
    let schema = codec.and_then(|c| ctx.codecs.encoder_options_schema(&CodecId::new(c)));
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in user {
        let Some(codec) = codec else {
            return Err(Error::invalid(format!(
                "convert: --opt {k}={v}: the output has no encoder to hand options to"
            )));
        };
        let fields = schema.unwrap_or(&[]);
        let field = fields.iter().find(|f| f.name == k).ok_or_else(|| {
            let known: Vec<&str> = fields.iter().map(|f| f.name).collect();
            let known = if known.is_empty() {
                "it declares no options".to_string()
            } else {
                format!("known: {}", known.join(", "))
            };
            Error::invalid(format!(
                "convert: --opt {k}={v}: encoder '{codec}' has no option '{k}' ({known})"
            ))
        })?;
        check_option_value(field, v).map_err(|why| {
            Error::invalid(format!("convert: --opt {k}={v}: {why} ({})", field.help))
        })?;
        out.retain(|(ek, _)| ek != k);
        out.push((k.clone(), v.clone()));
    }
    let avif =
        ext.is_some_and(|e| e.eq_ignore_ascii_case("avif") || e.eq_ignore_ascii_case("avifs"));
    if avif && !out.iter().any(|(k, _)| k == "codec") {
        let offers_av1 = schema
            .and_then(|fields| fields.iter().find(|f| f.name == "codec"))
            .is_some_and(|f| matches!(f.kind, OptionKind::Enum(vals) if vals.contains(&"av1")));
        if offers_av1 {
            out.push(("codec".to_string(), "av1".to_string()));
        }
    }
    Ok(out)
}

/// Check `raw` against the declared kind of `field` (the encoder does
/// the definitive parse; this surfaces a typo before any decoding).
fn check_option_value(field: &OptionField, raw: &str) -> Result<(), String> {
    let ok = match field.kind {
        OptionKind::Bool => matches!(
            raw,
            "true" | "1" | "yes" | "on" | "false" | "0" | "no" | "off"
        ),
        OptionKind::U32 => raw.parse::<u32>().is_ok(),
        OptionKind::I32 => raw.parse::<i32>().is_ok(),
        OptionKind::F32 => raw.parse::<f32>().is_ok_and(f32::is_finite),
        OptionKind::String => true,
        OptionKind::Enum(vals) => vals.contains(&raw),
    };
    if ok {
        return Ok(());
    }
    Err(match field.kind {
        OptionKind::Bool => "expected a boolean (true/false/1/0/yes/no/on/off)".to_string(),
        OptionKind::U32 => "expected a non-negative integer".to_string(),
        OptionKind::I32 => "expected an integer".to_string(),
        OptionKind::F32 => "expected a finite number".to_string(),
        OptionKind::Enum(vals) => format!("expected one of {}", vals.join(" | ")),
        OptionKind::String => unreachable!("strings always pass"),
    })
}

/// A still is a stream whose timeline is one tick of a 1/1 time base —
/// how the HEIF demuxer exposes its image items (a 1/1 time base means
/// "not timed", never "one second").
pub(crate) fn is_still(s: &StreamInfo) -> bool {
    s.time_base == TimeBase::new(1, 1) && s.duration == Some(1)
}

/// Video-stream ordinal a `%d` fan-out writes: the first stream that
/// is not a single still image (an image-sequence track), else stream
/// 0. Container order is not relied on: HEIF lists its tracks ahead of
/// the cover still on sequence-brand files and the still first on
/// image-brand ones.
pub fn fanout_stream(streams: &[StreamInfo]) -> usize {
    if streams.len() < 2 {
        return 0;
    }
    streams.iter().position(|s| !is_still(s)).unwrap_or(0)
}

/// Still-image output extensions whose file is a *timed* sequence of
/// pictures (an animation): fed by an input with an image-sequence
/// track, they take the track's frames — like a `%d` fan-out and like
/// a video output — rather than the cover still a single-picture
/// output takes.
const TIMED_STILL_EXTENSIONS: &[&str] = &["gif", "apng"];

/// True when a still-image output with extension `ext` is a timed
/// sequence (see [`TIMED_STILL_EXTENSIONS`]).
pub fn is_timed_still_output(ext: &str) -> bool {
    TIMED_STILL_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// Video-stream ordinal a single still output writes: the first still
/// stream (the composed primary image), else stream 0.
pub fn still_stream(streams: &[StreamInfo]) -> usize {
    if streams.len() < 2 {
        return 0;
    }
    streams.iter().position(is_still).unwrap_or(0)
}

/// The input's video streams in container order, read from the
/// container header (no decode). `None` when the input is not a
/// byte-shaped source the registry can open and probe (generators,
/// packet sources, unknown formats, missing files) — callers then
/// keep the pipeline's defaults.
pub fn probe_video_streams(input: &str, ctx: &RuntimeContext) -> Option<Vec<StreamInfo>> {
    Some(
        probe_streams(input, ctx)?
            .into_iter()
            .filter(|s| s.params.media_type == MediaType::Video)
            .collect(),
    )
}

/// Every stream of `input` as its container header declares it, or
/// `None` under the same conditions as [`probe_video_streams`].
pub fn probe_streams(input: &str, ctx: &RuntimeContext) -> Option<Vec<StreamInfo>> {
    probe_streams_restricted(input, ctx, &crate::restrict::Restrictions::default())
        .ok()
        .flatten()
}

/// [`probe_streams`] under allow / deny lists: the probed container
/// name is checked before the header is parsed and every video
/// stream's codec id before anything is decoded, exactly as the
/// `oxideav-image` gateway checks its `OpenOptions` lists. `Ok(None)`
/// when the input is not a byte-shaped source the registry can probe
/// (nothing to check: the pipeline reports what it cannot open),
/// `Err` (`crate::restrict::is_restricted`) on a refusal.
pub fn probe_streams_restricted(
    input: &str,
    ctx: &RuntimeContext,
    restrictions: &crate::restrict::Restrictions,
) -> oxideav_core::Result<Option<Vec<StreamInfo>>> {
    let raw = match ctx.sources.open(input) {
        Ok(SourceOutput::Bytes(b)) => b,
        _ => return Ok(None),
    };
    let mut handle: Box<dyn ReadSeek> = Box::new(raw);
    let ext = ext_of(input).map(|e| e.to_ascii_lowercase());
    let Ok(format) = ctx.containers.probe_input(&mut *handle, ext.as_deref()) else {
        return Ok(None);
    };
    restrictions.check_container(&format)?;
    let Ok(demuxer) = ctx.containers.open_demuxer(&format, handle, &ctx.codecs) else {
        return Ok(None);
    };
    let streams = demuxer.streams().to_vec();
    restrictions.check_streams(&streams)?;
    Ok(Some(streams))
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
    use PixelFormat::*;

    const PNG: &[PixelFormat] = &[Rgba, Rgb24, Gray8, Pal8, Rgb48Le, Rgba64Le];
    const PNM: &[PixelFormat] = &[
        MonoBlack, Gray8, Gray16Le, Rgb24, Rgb48Le, Rgba, Rgba64Le, Ya8,
    ];

    fn pick(src: PixelFormat, acc: &[PixelFormat]) -> Option<PixelFormat> {
        pick_sink_format(src, acc, LayoutHint::Any, None)
    }

    #[test]
    fn accepted_source_is_left_alone() {
        for f in [Rgb24, Rgba, Gray8, Rgb48Le] {
            assert_eq!(pick(f, PNG), None, "{f:?}");
        }
        assert_eq!(pick(Yuv420P, &[]), None, "empty list accepts anything");
    }

    #[test]
    fn yuv_to_png_drops_the_synthetic_alpha() {
        assert_eq!(pick(Yuv420P, PNG), Some(Rgb24));
        assert_eq!(pick(YuvJ420P, PNG), Some(Rgb24));
        assert_eq!(pick(Yuv444P, PNG), Some(Rgb24));
    }

    #[test]
    fn deep_yuv_to_png_keeps_its_bits() {
        assert_eq!(pick(Yuv420P10Le, PNG), Some(Rgb48Le));
    }

    #[test]
    fn yuv_with_alpha_keeps_alpha() {
        // Rgba is the PNG encoder's first choice already.
        assert_eq!(pick(Yuva420P, PNG), None);
    }

    #[test]
    fn yuv_to_netpbm_is_not_thresholded_to_one_bit() {
        assert_eq!(pick(Yuv420P, PNM), Some(Rgb24));
        assert_eq!(pick(Yuv420P10Le, PNM), Some(Rgb48Le));
    }

    #[test]
    fn netpbm_extensions_pick_their_family() {
        let h = |o| LayoutHint::for_output(o);
        assert_eq!(
            pick_sink_format(Yuv420P, PNM, h("x.pbm"), None),
            Some(MonoBlack)
        );
        assert_eq!(
            pick_sink_format(Yuv420P, PNM, h("x.pgm"), None),
            Some(Gray8)
        );
        assert_eq!(
            pick_sink_format(Yuv420P, PNM, h("x.ppm"), None),
            Some(Rgb24)
        );
        assert_eq!(pick_sink_format(Rgb24, PNM, h("x.PGM"), None), Some(Gray8));
        assert_eq!(pick_sink_format(Rgb24, PNM, h("x.ppm"), None), None);
        assert_eq!(pick_sink_format(Rgba, PNM, h("x.ppm"), None), Some(Rgb24));
        assert_eq!(h("x.pam"), LayoutHint::Any);
    }

    #[test]
    fn depth_request_caps_and_targets() {
        let d = |src, n| pick_sink_format(src, PNG, LayoutHint::Any, Some(n));
        assert_eq!(d(Yuv420P10Le, 8), Some(Rgb24));
        assert_eq!(d(Rgb48Le, 8), Some(Rgb24));
        assert_eq!(d(Yuv420P, 16), Some(Rgb48Le));
        assert_eq!(d(Rgb24, 8), None);
    }

    #[test]
    fn unreachable_candidates_are_skipped() {
        // Nothing reachable → leave it to the pipeline.
        assert_eq!(pick(Yuv420P, &[Pal8]), None);
    }

    #[test]
    fn jpeg_codes_packed_rgb_as_subsampled_ycbcr() {
        const JPEG: &[PixelFormat] = &[YuvJ444P, YuvJ422P, YuvJ420P, Gray8, Rgb24];
        let d = |src, q, opts: &[(String, String)]| {
            chroma_subsampled_default("mjpeg", src, JPEG, q, opts)
        };
        assert_eq!(d(Rgb24, None, &[]), Some(YuvJ420P));
        assert_eq!(d(Rgba, Some(75), &[]), Some(YuvJ420P));
        assert_eq!(d(Bgr24, Some(89), &[]), Some(YuvJ420P));
        assert_eq!(d(Rgb24, Some(90), &[]), Some(YuvJ444P));
        assert_eq!(d(Rgb24, Some(100), &[]), Some(YuvJ444P));
        // Not the family, not packed RGB, or the user chose the chroma.
        assert_eq!(
            chroma_subsampled_default("png", Rgb24, &[Rgb24, Rgba], None, &[]),
            None
        );
        assert_eq!(d(Gray8, None, &[]), None);
        assert_eq!(d(Yuv420P, None, &[]), None);
        assert_eq!(d(Rgb48Le, None, &[]), None);
        let chroma = [("chroma".to_string(), "yuv444".to_string())];
        assert_eq!(d(Rgb24, None, &chroma), None);
        let sampling = [("sampling".to_string(), "1x1".to_string())];
        assert_eq!(d(Rgb24, None, &sampling), None);
        // An encoder without the layout keeps the generic pick.
        assert_eq!(
            chroma_subsampled_default("mjpeg", Rgb24, &[Rgb24, Gray8], None, &[]),
            None
        );
        // No declared layouts: the family default still applies.
        assert_eq!(
            chroma_subsampled_default("mjpeg", Rgb24, &[], None, &[]),
            Some(YuvJ420P)
        );
    }

    #[test]
    fn output_hint_reads_the_last_extension() {
        assert_eq!(LayoutHint::for_output("dir.ppm/out.png"), LayoutHint::Any);
        assert_eq!(LayoutHint::for_output("a/b.pgm"), LayoutHint::Gray);
        assert_eq!(LayoutHint::for_output("noext"), LayoutHint::Any);
    }
}
