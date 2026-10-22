//! Raster output plumbing shared by every side-channel that produces
//! pixel buffers (PDF page rasterisation, 3D scene rendering) and by
//! the ICO writer's input side.
//!
//! Centralises:
//!
//! * The owned [`RgbaImage`] handoff buffer and its bridges to the
//!   `oxideav-image` gateway's [`Image`] ([`RgbaImage::from_image`],
//!   [`RgbaImage::to_image`]).
//! * Raster output classification ([`OutputClass`] / [`RasterFormat`]):
//!   the pure, extension-only [`classify_output`] the routing layer
//!   uses, and [`classify_output_with`], which additionally admits every
//!   extension the caller's `RuntimeContext` can encode a still image
//!   as — tiff, gif, qoi, tga, heic, … become raster outputs the moment
//!   their crate is registered, with no per-format code here.
//! * The gateway-backed writer [`encode_raster_to_path`]: the picture
//!   goes through [`oxideav_image::encode`], which resolves encoder and
//!   muxer from the extension. `-quality`, `-depth` and `--opt` map
//!   onto the gateway's schema-gated [`SaveOptions`]; the encoder input
//!   layout is the same pick the pipeline route makes
//!   ([`crate::image_sink::pick_sink_format`]) so `pdf[0] out.jpg` and
//!   `in.png out.jpg` hand the encoder the same layout.
//! * The gateway-backed reader [`read_image_to_rgba`] (any registered
//!   format → packed RGBA, primary picture only).
//! * The alpha-channel post-processors (`-alpha on/off/remove/set/
//!   transparent`) so both the PDF runner and the 3D-render runner
//!   honour `-alpha` identically.

use std::fs;
use std::path::Path;

use oxideav_core::{
    CodecId, CodecParameters, Error, PixelFormat, Result, RuntimeContext, VideoPlane,
};
use oxideav_image::{Image, ImageError, OpenOptions, SaveOptions};

use crate::image_sink::{self, LayoutHint};
use crate::op::{AlphaOp, Op};

/// What kind of output the convert verb is producing — drives the
/// routing decision in the side-channel runners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputClass {
    /// Format consumes a whole [`oxideav_scene::Scene`] (`.pdf` today;
    /// multi-page SVG would land here if we ever support it).
    Scene,
    /// Format takes a single [`oxideav_core::vector::VectorFrame`]
    /// per file, no rasterisation (`.svg`).
    Vector,
    /// Format takes a rasterised RGBA / RGB buffer per file
    /// (`.png` `.jpg` `.bmp` `.webp`, plus every extension the
    /// registry can encode a still image as).
    Raster(RasterFormat),
}

/// The raster family an output extension names. The four historical
/// arms are recognised without a registry; [`RasterFormat::Registry`]
/// is every other extension [`classify_output_with`] found an encoder
/// for. The writer does not branch on this: the gateway resolves the
/// encoder from the extension either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RasterFormat {
    Png,
    Jpeg,
    Bmp,
    Webp,
    /// Any other extension the caller's `RuntimeContext` has a
    /// still-image encoder for (`tiff`, `gif`, `qoi`, `heic`, …).
    Registry,
}

/// Classify an output path / printf template by extension alone.
///
/// Recognises `.pdf`, `.svg` and the four historical raster families;
/// everything else is `Err(Error::Unsupported)`. The routing layer,
/// which is pure, uses this; the runners, which have a registry, use
/// [`classify_output_with`].
pub fn classify_output(path_or_template: &str) -> Result<OutputClass> {
    classify_output_with(None, path_or_template)
}

/// [`classify_output`] that also admits every extension `ctx` can
/// encode a still image as (through the `oxideav-image` gateway's own
/// resolution: a container with a muxer and a default codec that has
/// an encoder, or a codec-only format). `.ico` is never a raster
/// output here — the icon writer is its own side-channel.
pub fn classify_output_with(
    ctx: Option<&RuntimeContext>,
    path_or_template: &str,
) -> Result<OutputClass> {
    let ext = ext_of(path_or_template)
        .ok_or_else(|| {
            Error::invalid(format!(
                "convert: output '{path_or_template}' has no extension"
            ))
        })?
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Ok(OutputClass::Scene),
        "svg" => Ok(OutputClass::Vector),
        "png" | "apng" => Ok(OutputClass::Raster(RasterFormat::Png)),
        "jpg" | "jpeg" => Ok(OutputClass::Raster(RasterFormat::Jpeg)),
        "bmp" | "dib" => Ok(OutputClass::Raster(RasterFormat::Bmp)),
        "webp" => Ok(OutputClass::Raster(RasterFormat::Webp)),
        "ico" => Err(Error::unsupported(format!(
            "convert: output '{path_or_template}': .ico is written by the icon side-channel, not as a raster frame"
        ))),
        other => match ctx {
            Some(ctx) if registry_encodes(ctx, other) => {
                Ok(OutputClass::Raster(RasterFormat::Registry))
            }
            Some(_) => Err(Error::unsupported(format!(
                "convert: output extension '.{other}' has no registered still-image encoder (png/jpg/bmp/webp always work; tiff/gif/qoi/… need their crate registered)"
            ))),
            None => Err(Error::unsupported(format!(
                "convert: output extension '.{other}' not in the registry-free raster set (png/jpg/bmp/webp/svg/pdf)"
            ))),
        },
    }
}

/// True when the gateway resolves an encoder for `ext` in `ctx`.
fn registry_encodes(ctx: &RuntimeContext, ext: &str) -> bool {
    oxideav_image::encoder_options(ctx, ext, &SaveOptions::default()).is_ok()
}

fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

/// Owned packed RGBA / RGB24 buffer with explicit dimensions. Lives
/// only long enough to hand off to an encoder. The pixel-format side
/// is encoded by `stride / width` (4 = Rgba, 3 = Rgb24).
#[derive(Debug, Clone)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub stride: usize,
}

impl RgbaImage {
    /// True when the buffer is packed Rgb24 (3 bytes per pixel) rather
    /// than Rgba (4 bytes). Inferred off `stride` since the struct
    /// doesn't carry an explicit pixel-format tag.
    pub fn is_rgb(&self) -> bool {
        self.stride == (self.width as usize) * 3
    }

    /// The framework layout of the buffer (`Rgb24` or `Rgba`).
    pub fn pixel_format(&self) -> PixelFormat {
        if self.is_rgb() {
            PixelFormat::Rgb24
        } else {
            PixelFormat::Rgba
        }
    }

    /// Flatten a gateway picture to tightly packed RGBA (opaque alpha
    /// when the source has none).
    pub fn from_image(img: &Image) -> Result<RgbaImage> {
        let pixels = img
            .to_rgba8()
            .map_err(|e| gateway_error("convert: pixel conversion failed", e))?;
        Ok(RgbaImage {
            width: img.width(),
            height: img.height(),
            stride: img.width() as usize * 4,
            pixels,
        })
    }

    /// Wrap the buffer as a gateway [`Image`] in its own layout (no
    /// copy of the pixels beyond the clone of the buffer).
    pub fn to_image(&self) -> Result<Image> {
        Image::from_planes(
            self.width,
            self.height,
            self.pixel_format(),
            vec![VideoPlane {
                stride: self.stride,
                data: self.pixels.clone(),
            }],
        )
        .map_err(|e| gateway_error("convert: raster buffer", e))
    }
}

/// Fold a gateway error onto the framework's: registry / codec / muxer
/// errors pass through unchanged, an unknown format is `Unsupported`
/// (a request this registry cannot serve), I/O stays I/O, and the
/// gateway's own data checks are `InvalidData`. `what` prefixes the
/// message so the user sees which step of `convert` failed.
pub fn gateway_error(what: &str, e: ImageError) -> Error {
    match e {
        ImageError::Core(e) => e,
        ImageError::Io(e) => Error::Io(e),
        ImageError::UnknownFormat(m) | ImageError::Unsupported(m) => {
            Error::unsupported(format!("{what}: {m}"))
        }
        // The gateway's refusal under allow / deny lists, in the shape
        // `crate::restrict::is_restricted` recognises.
        ImageError::Restricted(m) => Error::unsupported(format!("{what}: restricted: {m}")),
        other => Error::invalid(format!("{what}: {other}")),
    }
}

/// Decode the primary picture of any registered still-image format
/// into packed RGBA through the `oxideav-image` gateway. Only the first
/// picture is decoded (`max_frames = 1`).
pub fn read_image_to_rgba(ctx: &RuntimeContext, input: &str) -> Result<RgbaImage> {
    read_image_to_rgba_with(ctx, input, &OpenOptions::new())
}

/// [`read_image_to_rgba`] with the caller's gateway options (allow /
/// deny lists, decoder options); `max_frames` is forced to 1.
pub fn read_image_to_rgba_with(
    ctx: &RuntimeContext,
    input: &str,
    opts: &OpenOptions,
) -> Result<RgbaImage> {
    let opts = opts.clone().with_max_frames(1);
    let file = oxideav_image::open_with(ctx, input, &opts)
        .map_err(|e| gateway_error(&format!("convert: failed to read {input}"), e))?;
    RgbaImage::from_image(file.primary())
}

/// Encode a single rendered image to `path` through the
/// `oxideav-image` gateway; the extension picks encoder and muxer.
///
/// * `-quality N` → [`SaveOptions::quality`] (forwarded only to encoders
///   whose schema declares `quality`, like the gateway documents).
/// * `--opt KEY=VALUE` → validated against the encoder's schema
///   ([`image_sink::encoder_options`], the same check the pipeline
///   route makes) and forwarded; `.avif` implies `codec=av1` there too.
/// * `-depth 8|16` and the extension's layout family feed
///   [`image_sink::pick_sink_format`]; the chosen encoder input layout
///   is forced on the gateway so this path and the pipeline route hand
///   the encoder the same layout (RGB rather than RGBA for a PNG when
///   the alpha was dropped, …). An encoder that declares no accepted
///   layouts (JPEG) is probed in ImageMagick's order — the render's
///   own layout, then 4:2:0 YCbCr, 4:2:2, 4:4:4, RGB, gray — and the
///   first layout it constructs for is pinned, so an RGB render
///   becomes a 4:2:0 JPEG exactly as before. When the output codec
///   cannot be resolved the gateway's own ladder decides.
pub fn encode_raster_to_path(
    ctx: &RuntimeContext,
    img: &RgbaImage,
    path: &str,
    ops: &[Op],
) -> Result<()> {
    let bytes = encode_raster(ctx, img, path, ops)?;
    fs::write(Path::new(path), bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to write {path}: {e}")))?;
    Ok(())
}

/// [`encode_raster_to_path`] without the write: the encoded file.
pub fn encode_raster(
    ctx: &RuntimeContext,
    img: &RgbaImage,
    path: &str,
    ops: &[Op],
) -> Result<Vec<u8>> {
    let ext = ext_of(path)
        .ok_or_else(|| Error::invalid(format!("convert: output '{path}' has no extension")))?
        .to_ascii_lowercase();
    let image = img.to_image()?;
    let opts = save_options(ctx, img.pixel_format(), path, &ext, ops)?;
    oxideav_image::encode(ctx, &image, &ext, &opts).map_err(|e| {
        gateway_error(
            &format!("convert: {} encode failed", ext.to_ascii_uppercase()),
            e,
        )
    })
}

/// The gateway options for `ops` on an output with extension `ext`.
fn save_options(
    ctx: &RuntimeContext,
    src: PixelFormat,
    path: &str,
    ext: &str,
    ops: &[Op],
) -> Result<SaveOptions> {
    let mut opts = SaveOptions::new();
    let codec = image_sink::resolve_output_codec(None, path, ctx);
    let quality =
        last_quality(ops).or_else(|| codec.as_deref().and_then(image_sink::default_quality));
    if let Some(q) = quality {
        opts = opts.with_quality(q);
    }
    if strips_metadata(ops) {
        opts = opts.without_metadata();
    }
    let user: Vec<(String, String)> = ops
        .iter()
        .filter_map(|o| match o {
            Op::EncoderOpt { key, value } => Some((key.clone(), value.clone())),
            _ => None,
        })
        .collect();
    for (k, v) in image_sink::encoder_options(ctx, codec.as_deref(), Some(ext), &user)? {
        opts = opts.with_option(k, v);
    }
    // The plan's budget: the gateway hands it to every encoder it
    // creates (`Encoder::set_execution_context`) and, where the
    // encoder's schema declares a `threads` option the user did not
    // set, through that option as well (`oxideav-image` 0.0.5).
    opts = opts.with_execution_context(execution_context(ops));
    // Options can change the encoder's accepted layouts (a `codec`
    // option on a container encoder); the gateway's ladder handles
    // those. Otherwise pin the layout the pipeline route would pick.
    let has_codec_opt = opts.options.iter().any(|(k, _)| k == "codec");
    let Some(codec) = codec.filter(|_| !has_codec_opt) else {
        return Ok(opts);
    };
    let Some(accepted) = image_sink::encoder_accepted_formats(ctx, &codec) else {
        return Ok(opts);
    };
    let target = if accepted.is_empty() {
        probe_undeclared_encoder(ctx, &codec, src, &opts)
    } else {
        let hint = LayoutHint::for_output(path);
        Some(
            image_sink::chroma_subsampled_default(
                &codec,
                src,
                &accepted,
                opts.quality,
                &opts.options,
            )
            .or_else(|| image_sink::pick_sink_format(src, &accepted, hint, last_depth(ops)))
            .unwrap_or({
                if accepted.contains(&src) {
                    src
                } else {
                    accepted[0]
                }
            }),
        )
    };
    if let Some(t) = target {
        opts = opts.with_pixel_format(t);
    }
    Ok(opts)
}

/// Layouts offered to an encoder that declares no accepted set, in
/// ImageMagick's order of preference for a colour render: 4:2:0 YCbCr
/// (what `convert x.pdf x.jpg` writes), then the fuller chroma
/// layouts, RGB, and gray last so colour is never dropped while a
/// colour layout would do. A source that is not packed RGB (gray, YUV)
/// is offered first: a gray picture stays a gray JPEG.
const UNDECLARED_ORDER: &[PixelFormat] = &[
    PixelFormat::Yuv420P,
    PixelFormat::Yuv422P,
    PixelFormat::Yuv444P,
    PixelFormat::Rgb24,
    PixelFormat::Rgba,
    PixelFormat::Gray8,
];

/// The first layout of [`UNDECLARED_ORDER`] (preceded by `src` when
/// it is not packed RGB) the `codec` encoder constructs for, with
/// `opts`' options applied since an option can change what an encoder
/// takes. `None` when none does; the gateway's ladder then reports the
/// refusal.
fn probe_undeclared_encoder(
    ctx: &RuntimeContext,
    codec: &str,
    src: PixelFormat,
    opts: &SaveOptions,
) -> Option<PixelFormat> {
    let mut params = CodecParameters::video(CodecId::new(codec));
    params.width = Some(16);
    params.height = Some(16);
    if let Some(q) = opts.quality {
        params.options = params.options.set("quality", q.to_string());
    }
    for (k, v) in &opts.options {
        params.options = params.options.set(k.clone(), v.clone());
    }
    let packed_rgb = matches!(
        src,
        PixelFormat::Rgb24
            | PixelFormat::Rgba
            | PixelFormat::Bgr24
            | PixelFormat::Bgra
            | PixelFormat::Argb
            | PixelFormat::Abgr
            | PixelFormat::Rgb48Le
            | PixelFormat::Rgba64Le
    );
    let first = (!packed_rgb).then_some(src);
    first
        .into_iter()
        .chain(UNDECLARED_ORDER.iter().copied())
        .filter(|&f| f == src || oxideav_pixfmt::supports(src, f))
        .find(|&f| {
            params.pixel_format = Some(f);
            ctx.codecs.first_encoder(&params).is_ok()
        })
}

fn last_quality(ops: &[Op]) -> Option<u8> {
    ops.iter().rev().find_map(|o| match o {
        Op::Quality(q) => Some((*q).min(100) as u8),
        _ => None,
    })
}

fn last_threads(ops: &[Op]) -> Option<usize> {
    ops.iter().rev().find_map(|o| match o {
        Op::Threads(n) if *n >= 1 => Some(*n),
        Op::Threads(_) => None,
        _ => None,
    })
}

/// The execution budget `ops` ask for: the last `-threads N` (`N >= 1`),
/// else the host's parallelism — the same resolution
/// [`ConvertPlan::execution_context`](crate::ConvertPlan::execution_context)
/// makes for the pipeline route.
pub fn execution_context(ops: &[Op]) -> oxideav_core::ExecutionContext {
    match last_threads(ops) {
        Some(n) => oxideav_core::ExecutionContext::with_threads(n),
        None => oxideav_core::ExecutionContext::auto(),
    }
}

/// The gateway's open options for a side-channel read: the plan's
/// allow / deny lists and its execution budget, which the gateway
/// hands to every decoder it creates (and through a declared
/// `threads` decoder option the user did not set).
pub fn open_options(plan: &crate::ConvertPlan) -> OpenOptions {
    plan.restrictions
        .apply(OpenOptions::new())
        .with_execution_context(plan.execution_context())
}

fn strips_metadata(ops: &[Op]) -> bool {
    ops.iter()
        .any(|o| matches!(o, Op::Strip | Op::Thumbnail { .. }))
}

fn last_depth(ops: &[Op]) -> Option<u8> {
    ops.iter().rev().find_map(|o| match o {
        Op::Depth(d) => Some(*d),
        _ => None,
    })
}

/// Apply the `-alpha …` ops in source order to an in-place [`RgbaImage`].
///
/// `bg` is the active `-background` value (defaults to opaque white at
/// the call site); only `AlphaOp::Remove` actually consults it.
pub fn apply_alpha_ops(img: &mut RgbaImage, ops: &[Op], bg: [u8; 4]) {
    for op in ops {
        if let Op::Alpha(a) = op {
            match a {
                AlphaOp::On => {}
                AlphaOp::Off => drop_alpha(img),
                AlphaOp::Remove => flatten_alpha_over(img, bg),
                AlphaOp::Set => set_alpha(img, 255),
                AlphaOp::Transparent => set_alpha(img, 0),
            }
        }
    }
}

fn flatten_alpha_over(img: &mut RgbaImage, bg: [u8; 4]) {
    if img.is_rgb() {
        return;
    }
    let bg_r = bg[0] as u32;
    let bg_g = bg[1] as u32;
    let bg_b = bg[2] as u32;
    for px in img.pixels.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        let inv = 255 - a;
        let r = (px[0] as u32 * a + bg_r * inv + 127) / 255;
        let g = (px[1] as u32 * a + bg_g * inv + 127) / 255;
        let b = (px[2] as u32 * a + bg_b * inv + 127) / 255;
        px[0] = r as u8;
        px[1] = g as u8;
        px[2] = b as u8;
        px[3] = 255;
    }
}

fn drop_alpha(img: &mut RgbaImage) {
    if img.is_rgb() {
        return;
    }
    let w = img.width as usize;
    let h = img.height as usize;
    let mut out = Vec::with_capacity(w * h * 3);
    for px in img.pixels.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    img.pixels = out;
    img.stride = w * 3;
}

fn set_alpha(img: &mut RgbaImage, value: u8) {
    if img.is_rgb() {
        return;
    }
    for px in img.pixels.chunks_exact_mut(4) {
        px[3] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_output_covers_known_targets() {
        assert!(matches!(classify_output("out.pdf"), Ok(OutputClass::Scene)));
        assert!(matches!(
            classify_output("out.svg"),
            Ok(OutputClass::Vector)
        ));
        assert!(matches!(
            classify_output("out.png"),
            Ok(OutputClass::Raster(RasterFormat::Png))
        ));
        assert!(matches!(
            classify_output("out.jpg"),
            Ok(OutputClass::Raster(RasterFormat::Jpeg))
        ));
        assert!(matches!(
            classify_output("out.jpeg"),
            Ok(OutputClass::Raster(RasterFormat::Jpeg))
        ));
        assert!(matches!(
            classify_output("out.bmp"),
            Ok(OutputClass::Raster(RasterFormat::Bmp))
        ));
        assert!(matches!(
            classify_output("out.webp"),
            Ok(OutputClass::Raster(RasterFormat::Webp))
        ));
        assert!(classify_output("out.tiff").is_err());
        assert!(classify_output("out.gif").is_err());
        assert!(classify_output("out.ico").is_err());
        assert!(classify_output("noext").is_err());
    }

    #[test]
    fn classify_with_an_empty_registry_matches_the_pure_classification() {
        let ctx = RuntimeContext::new();
        for out in [
            "out.pdf", "out.svg", "out.png", "out.jpg", "out.bmp", "out.webp",
        ] {
            assert_eq!(
                classify_output_with(Some(&ctx), out).unwrap(),
                classify_output(out).unwrap(),
                "{out}"
            );
        }
        for out in ["out.tiff", "out.gif", "out.ico", "out.mp4", "noext"] {
            assert!(classify_output_with(Some(&ctx), out).is_err(), "{out}");
        }
    }

    #[test]
    fn rgba_image_round_trips_through_the_gateway_image() {
        let img = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
            stride: 8,
        };
        let gw = img.to_image().unwrap();
        assert_eq!(gw.format(), PixelFormat::Rgba);
        assert_eq!((gw.width(), gw.height()), (2, 1));
        let back = RgbaImage::from_image(&gw).unwrap();
        assert_eq!(back.pixels, img.pixels);
        assert_eq!(back.stride, 8);

        let rgb = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 5, 6, 7],
            stride: 6,
        };
        let gw = rgb.to_image().unwrap();
        assert_eq!(gw.format(), PixelFormat::Rgb24);
        let back = RgbaImage::from_image(&gw).unwrap();
        assert_eq!(back.pixels, vec![1, 2, 3, 255, 5, 6, 7, 255]);
    }

    #[test]
    fn padded_rows_are_kept_as_a_stride() {
        // A 1-pixel RGBA row stored on an 8-byte stride.
        let img = RgbaImage {
            width: 1,
            height: 2,
            pixels: vec![1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0],
            stride: 8,
        };
        let back = RgbaImage::from_image(&img.to_image().unwrap()).unwrap();
        assert_eq!(back.pixels, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn encode_without_a_registry_is_a_typed_unsupported_error() {
        let ctx = RuntimeContext::new();
        let img = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 4],
            stride: 4,
        };
        let err = encode_raster(&ctx, &img, "out.png", &[]).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    }

    #[test]
    fn drop_alpha_shrinks_buffer_three_quarters() {
        let mut img = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
            ],
            stride: 8,
        };
        drop_alpha(&mut img);
        assert_eq!(img.stride, 6);
        assert_eq!(img.pixels.len(), 12);
        assert_eq!(img.pixels[..3], [10, 20, 30]);
        assert_eq!(img.pixels[3..6], [50, 60, 70]);
    }

    #[test]
    fn flatten_alpha_over_white_preserves_opaque_pixels() {
        let mut img = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 0, 0],
            stride: 8,
        };
        flatten_alpha_over(&mut img, [255, 255, 255, 255]);
        assert_eq!(img.pixels[..4], [255, 0, 0, 255]);
        assert_eq!(img.pixels[4..], [255, 255, 255, 255]);
    }

    #[test]
    fn set_alpha_to_zero_leaves_colour_intact() {
        let mut img = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![10, 20, 30, 100],
            stride: 4,
        };
        set_alpha(&mut img, 0);
        assert_eq!(img.pixels, [10, 20, 30, 0]);
    }
}
