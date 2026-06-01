//! `-ping` runner — print one IM-format header line per "image" and
//! exit without decoding pixels or writing any output.
//!
//! Output line shape (matches `imagemagick identify` / `convert -ping
//! input info:`):
//!
//! ```text
//! <path> <FORMAT> <W>x<H> <W>x<H>+0+0 <DEPTH>-bit <COLORSPACE> <BYTES>B
//! ```
//!
//! For multi-page inputs (PDF) one line per selected page is emitted
//! with the path suffixed `[N]`. For container inputs we emit one line
//! per video stream (skipping audio-only files with a friendly note).

use std::fs;

use oxideav_core::{Error, PixelFormat, Result, RuntimeContext, SourceOutput};
use oxideav_pdf::read_pdf_to_scene;

use crate::op::ConvertPlan;
use crate::pdf_runner::is_pdf_input;

/// Print IM-format ping lines for `plan.input` to stdout.
pub fn run(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<()> {
    if is_pdf_input(&plan.input) {
        return ping_pdf(plan);
    }
    ping_container(plan, ctx)
}

fn ping_pdf(plan: &ConvertPlan) -> Result<()> {
    let bytes = fs::read(&plan.input)
        .map_err(|e| Error::invalid(format!("convert: failed to read {}: {e}", plan.input)))?;
    let file_size = bytes.len();
    let scene = read_pdf_to_scene(&bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to parse PDF: {e:?}")))?;
    let pages = scene.pages.as_deref().unwrap_or(&[]);
    if pages.is_empty() {
        return Err(Error::invalid("convert: PDF has no pages to ping"));
    }
    let indices: Vec<usize> = match &plan.input_pages {
        Some(sel) => sel
            .resolve(pages.len())
            .map_err(|e| Error::invalid(format!("convert: input '{}': {e}", plan.input)))?,
        None => (0..pages.len()).collect(),
    };
    for idx in indices {
        let page = &pages[idx];
        // PDF Page width/height come in PostScript points (1/72 inch).
        // IM reports them as integer pixels for `-ping` (no rasterisation
        // happens; the geometry is the page's intrinsic point size).
        let w = page.width.round() as u32;
        let h = page.height.round() as u32;
        println!(
            "{}[{}] PDF {w}x{h} {w}x{h}+0+0 8-bit sRGB {}B",
            plan.input, idx, file_size
        );
    }
    Ok(())
}

fn ping_container(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<()> {
    let raw = match ctx.sources.open(&plan.input)? {
        SourceOutput::Bytes(b) => b,
        SourceOutput::Packets(_) => {
            return Err(Error::unsupported(format!(
                "convert -ping: {}: packet-shape source not supported",
                plan.input
            )));
        }
        SourceOutput::Frames(_) => {
            return Err(Error::unsupported(format!(
                "convert -ping: {}: frame-shape source not supported",
                plan.input
            )));
        }
        // Multi-title sources (BD-ROM, DVD-Video, multi-edition MKV)
        // need a per-title fan-out the convert verb doesn't model;
        // `-ping` is single-stream by design.
        SourceOutput::MultiTitle(_) => {
            return Err(Error::unsupported(format!(
                "convert -ping: {}: multi-title sources are not supported",
                plan.input
            )));
        }
        // Any future `#[non_exhaustive]`-gated variant — the local
        // umbrella's `oxideav-core` enables non_exhaustive but the
        // currently-published crate doesn't, so the arm appears
        // unreachable on CI. The allow keeps both builds green.
        #[allow(unreachable_patterns)]
        _ => {
            return Err(Error::unsupported(format!(
                "convert -ping: {}: source kind not supported by ping",
                plan.input
            )));
        }
    };
    let file_size = fs::metadata(&plan.input).map(|m| m.len()).unwrap_or(0);
    // BytesSource is Read+Seek+Send; the demuxer just needs ReadSeek.
    let mut handle: Box<dyn oxideav_core::ReadSeek> = Box::new(raw);
    let ext = ext_from_uri(&plan.input);
    let format = ctx
        .containers
        .probe_input(&mut *handle, ext.as_deref())
        .map_err(|e| Error::invalid(format!("convert -ping: {}: {e}", plan.input)))?;
    let demuxer = ctx
        .containers
        .open_demuxer(&format, handle, &ctx.codecs)
        .map_err(|e| Error::invalid(format!("convert -ping: {}: {e}", plan.input)))?;

    let format_label = format_label_for(&format, demuxer.format_name());
    let video_streams: Vec<_> = demuxer
        .streams()
        .iter()
        .filter(|s| matches!(s.params.media_type, oxideav_core::MediaType::Video))
        .collect();

    if video_streams.is_empty() {
        // Audio-only / data-only container: emit a 0x0 line so callers
        // don't get silent failures, and a stderr note for humans.
        eprintln!(
            "convert -ping: {}: no video stream — emitting 0x0 line",
            plan.input
        );
        println!("{} {format_label} 0x0 0x0+0+0 - {file_size}B", plan.input);
        return Ok(());
    }

    for (i, s) in video_streams.iter().enumerate() {
        let p = &s.params;
        let w = p.width.unwrap_or(0);
        let h = p.height.unwrap_or(0);
        let (depth, colorspace) = match p.pixel_format {
            Some(pf) => describe_pixel_format(pf),
            None => (0, "Unknown"),
        };
        let depth_field = if depth == 0 {
            String::from("?-bit")
        } else {
            format!("{depth}-bit")
        };
        // Single-stream files (the common case for PNG/JPG) get the
        // bare path; multi-stream files disambiguate with [N].
        let label = if video_streams.len() == 1 {
            plan.input.clone()
        } else {
            format!("{}[{i}]", plan.input)
        };
        println!(
            "{label} {format_label} {w}x{h} {w}x{h}+0+0 {depth_field} {colorspace} {file_size}B"
        );
    }
    Ok(())
}

/// Map a `PixelFormat` to (bit depth per channel, IM-style colorspace
/// label). The depth refers to the per-channel bit width as IM reports
/// it; colorspace is `sRGB` / `Gray` / `CMYK` / `Unknown`.
fn describe_pixel_format(pf: PixelFormat) -> (u8, &'static str) {
    use PixelFormat::*;
    match pf {
        // 8-bit sRGB (RGB / RGBA / BGR / BGRA / ARGB / ABGR)
        Rgb24 | Rgba | Bgr24 | Bgra | Argb | Abgr => (8, "sRGB"),
        // 16-bit packed RGB / RGBA
        Rgb48Le | Rgba64Le => (16, "sRGB"),
        // YUV planar / packed at 8 / 10 / 12 — IM treats YUV as sRGB.
        Yuv420P | Yuv422P | Yuv444P | YuvJ420P | YuvJ422P | YuvJ444P | Nv12 | Nv21 | Yuyv422
        | Uyvy422 | Yuva420P => (8, "sRGB"),
        Yuv420P10Le | Yuv422P10Le | Yuv444P10Le => (10, "sRGB"),
        Yuv420P12Le | Yuv422P12Le | Yuv444P12Le => (12, "sRGB"),
        // Grayscale family
        Gray8 | Ya8 => (8, "Gray"),
        Gray10Le => (10, "Gray"),
        Gray12Le => (12, "Gray"),
        Gray16Le => (16, "Gray"),
        MonoBlack | MonoWhite => (1, "Gray"),
        // Palette
        Pal8 => (8, "sRGB"),
        // CMYK (prepress)
        Cmyk => (8, "CMYK"),
        // Anything we don't have a clean depth/colorspace for.
        _ => (0, "Unknown"),
    }
}

/// Extract a friendly IM-style format label.
///
/// `format_name` is whatever the demuxer reports (typically already the
/// container's preferred shorthand). `format` is the registry-key form.
/// We prefer `format_name` upper-cased; if it's empty fall back to the
/// registry key.
fn format_label_for(format: &str, format_name: &str) -> String {
    let chosen = if !format_name.is_empty() {
        format_name
    } else {
        format
    };
    chosen.to_ascii_uppercase()
}

/// Best-effort extension hint from a URI: takes everything after the
/// last `/`-segment's `.`, ignoring `?…` query strings. Mirrors the
/// helper in `oxideav-cli` (kept local to avoid a new dep).
fn ext_from_uri(uri: &str) -> Option<String> {
    let last = uri.rsplit('/').next().unwrap_or(uri);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(last[dot + 1..].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_label_uppercase() {
        assert_eq!(format_label_for("png", "png"), "PNG");
        assert_eq!(format_label_for("png", ""), "PNG");
        assert_eq!(format_label_for("matroska", "MKV"), "MKV");
    }

    #[test]
    fn ext_hint() {
        assert_eq!(ext_from_uri("/tmp/a.png").as_deref(), Some("png"));
        assert_eq!(
            ext_from_uri("https://x/y.JPG?cache=1").as_deref(),
            Some("jpg")
        );
        assert_eq!(ext_from_uri("noext").as_deref(), None);
    }

    #[test]
    fn pixel_format_mapping_covers_common_cases() {
        assert_eq!(describe_pixel_format(PixelFormat::Rgb24), (8, "sRGB"));
        assert_eq!(describe_pixel_format(PixelFormat::Rgba64Le), (16, "sRGB"));
        assert_eq!(describe_pixel_format(PixelFormat::Gray8), (8, "Gray"));
        assert_eq!(describe_pixel_format(PixelFormat::Gray16Le), (16, "Gray"));
        assert_eq!(
            describe_pixel_format(PixelFormat::Yuv420P10Le),
            (10, "sRGB")
        );
        assert_eq!(describe_pixel_format(PixelFormat::Cmyk), (8, "CMYK"));
        assert_eq!(describe_pixel_format(PixelFormat::MonoBlack), (1, "Gray"));
    }
}
