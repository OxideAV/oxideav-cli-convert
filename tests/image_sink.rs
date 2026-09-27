//! Output-content golden tests for the pipeline route's still-image
//! sinks (`convert in.X out.png`).
//!
//! Each case synthesises its input in-test (deterministic pixel
//! patterns, encoded through the crate's own dependencies), runs the
//! public [`oxideav_cli_convert::run`] entry point, decodes the written
//! PNG back and pins
//!
//! * the PNG pixel layout (IHDR colour type / depth, as the decoder's
//!   `PngPixelFormat`), and
//! * the exact decoded samples — against the source pixels where the
//!   route is a pure pass-through, or against the decoded source frame
//!   run through the default `oxideav-pixfmt` conversion where a
//!   colour conversion is involved.
//!
//! Pinning decoded content rather than the compressed bytes keeps the
//! goldens stable when a PNG encoder release changes its deflate
//! choices, while still catching any change in what `convert` hands
//! the encoder (a different target pixel format, a different
//! conversion, a dropped or synthesised channel).

use std::fs;
use std::path::PathBuf;

use oxideav_core::{PixelFormat, RuntimeContext, VideoFrame, VideoPlane};
use oxideav_png::{decode_png, encode_png_image, PngImage, PngPixelFormat};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-image-sink-{name}-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_source::register(&mut ctx);
    oxideav_png::register(&mut ctx);
    oxideav_mjpeg::register(&mut ctx);
    ctx
}

fn convert(args: &[&str]) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    oxideav_cli_convert::run(&args, &ctx()).expect("convert succeeds");
}

/// Deterministic `channels`-per-pixel test pattern.
fn pattern(w: u32, h: u32, channels: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize * channels);
    for y in 0..h {
        for x in 0..w {
            for c in 0..channels {
                let s = (x * 7 + y * 13 + c as u32 * 41 + (x * y) % 17) % 256;
                v.push(s as u8);
            }
        }
    }
    v
}

fn write_png(path: &PathBuf, fmt: PngPixelFormat, w: u32, h: u32, data: Vec<u8>) {
    let img = PngImage {
        width: w,
        height: h,
        pixel_format: fmt,
        stride: w as usize * fmt.bytes_per_pixel(),
        data,
        palette: Vec::new(),
    };
    fs::write(path, encode_png_image(&img).expect("png encode")).expect("write png");
}

/// A 4:4:4 JPEG (the mjpeg encoder's own RGB → YUV path is bypassed:
/// the pattern goes straight in as planar Y / Cb / Cr so the fixture
/// is independent of any RGB matrix choice).
fn write_jpeg444(path: &PathBuf, w: u32, h: u32) {
    let n = (w * h) as usize;
    let p = pattern(w, h, 3);
    let (mut y, mut cb, mut cr) = (Vec::with_capacity(n), Vec::new(), Vec::new());
    for px in p.chunks_exact(3) {
        y.push(px[0]);
        cb.push(px[1] / 2 + 64);
        cr.push(px[2] / 2 + 64);
    }
    let frame = VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: w as usize,
                data: y,
            },
            VideoPlane {
                stride: w as usize,
                data: cb,
            },
            VideoPlane {
                stride: w as usize,
                data: cr,
            },
        ],
    };
    let bytes = oxideav_mjpeg::encoder::encode_jpeg(&frame, w, h, PixelFormat::Yuv444P, 90)
        .expect("jpeg encode");
    fs::write(path, bytes).expect("write jpeg");
}

fn read_png(path: &PathBuf) -> PngImage {
    decode_png(&fs::read(path).expect("output written")).expect("output decodes")
}

#[test]
fn rgb24_png_passes_through_unchanged() {
    let dir = temp_dir("rgb");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 3);
    write_png(&src, PngPixelFormat::Rgb24, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.pixel_format, PngPixelFormat::Rgb24);
    assert_eq!((got.width, got.height), (24, 16));
    assert_eq!(got.data, px, "rgb24 → png must be sample-exact");
}

#[test]
fn rgba_png_passes_through_unchanged() {
    let dir = temp_dir("rgba");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 4);
    write_png(&src, PngPixelFormat::Rgba, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.pixel_format, PngPixelFormat::Rgba);
    assert_eq!(got.data, px, "rgba → png must be sample-exact");
}

#[test]
fn gray8_png_passes_through_unchanged() {
    let dir = temp_dir("gray");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 1);
    write_png(&src, PngPixelFormat::Gray8, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.pixel_format, PngPixelFormat::Gray8);
    assert_eq!(got.data, px, "gray8 → png must be sample-exact");
}

#[test]
fn rgb48_png_keeps_sixteen_bits() {
    let dir = temp_dir("rgb48");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 6);
    write_png(&src, PngPixelFormat::Rgb48Le, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.pixel_format, PngPixelFormat::Rgb48Le);
    assert_eq!(got.data, px, "rgb48 → png must be sample-exact");
}

/// Demux + decode the first video frame of `path` through the
/// registry — the reference the YUV cases compare against.
fn decode_first_frame(path: &PathBuf) -> (VideoFrame, PixelFormat, u32, u32) {
    let ctx = ctx();
    let file = fs::File::open(path).expect("open fixture");
    let mut input: Box<dyn oxideav_core::ReadSeek> = Box::new(file);
    let fmt = ctx
        .containers
        .probe_input(&mut *input, path.extension().and_then(|e| e.to_str()))
        .expect("probe");
    let mut dmx = ctx
        .containers
        .open_demuxer(&fmt, input, &ctx.codecs)
        .expect("demuxer");
    let params = dmx.streams()[0].params.clone();
    let mut dec = ctx.codecs.first_decoder(&params).expect("decoder");
    dec.send_packet(&dmx.next_packet().expect("packet"))
        .expect("send");
    let _ = dec.flush();
    let frame = match dec.receive_frame().expect("frame") {
        oxideav_core::Frame::Video(v) => v,
        other => panic!("expected a video frame, got {other:?}"),
    };
    (
        frame,
        params.pixel_format.expect("pixel format"),
        params.width.expect("width"),
        params.height.expect("height"),
    )
}

/// The reference RGB samples: the decoded source frame through the
/// default `oxideav-pixfmt` conversion.
fn reference_rgb(path: &PathBuf) -> Vec<u8> {
    let (frame, fmt, w, h) = decode_first_frame(path);
    let rgb = oxideav_pixfmt::convert(
        &frame,
        oxideav_pixfmt::FrameInfo::new(fmt, w, h),
        PixelFormat::Rgb24,
        &oxideav_pixfmt::ConvertOptions::default(),
    )
    .expect("reference conversion");
    rgb.planes[0].data.clone()
}

/// YUV source → PNG: the colour samples must be exactly the decoded
/// frame through the default pixfmt conversion. Today the pipeline
/// picks the PNG encoder's first accepted layout (RGBA) for a YUV
/// source, synthesising an opaque alpha channel.
#[test]
fn yuv444_jpeg_to_png_samples_match_the_reference_conversion() {
    let dir = temp_dir("jpeg444");
    let (src, out) = (dir.join("in.jpg"), dir.join("out.png"));
    write_jpeg444(&src, 24, 16);
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!((got.width, got.height), (24, 16));
    assert_eq!(got.pixel_format, PngPixelFormat::Rgba);
    let rgb: Vec<u8> = got
        .data
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    assert!(got.data.chunks_exact(4).all(|p| p[3] == 255));
    assert_eq!(rgb, reference_rgb(&src), "RGB samples changed");
}
