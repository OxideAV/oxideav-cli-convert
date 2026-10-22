//! Output-content golden tests for the pipeline route's still-image
//! sinks (`convert in.X out.png`).
//!
//! Each case synthesises its input in-test (deterministic pixel
//! patterns, encoded through the `oxideav-image` gateway over the
//! registered format crates), runs the public
//! [`oxideav_cli_convert::run`] entry point, decodes the written PNG
//! back through the gateway and pins
//!
//! * the PNG pixel layout (IHDR colour type / depth, as the decoded
//!   `PixelFormat`), and
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
use oxideav_image::{Image, SaveOptions};

mod common;

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
    oxideav_bmp::register(&mut ctx);
    oxideav_mjpeg::register(&mut ctx);
    oxideav_webp::register(&mut ctx);
    oxideav_gif::register(&mut ctx);
    oxideav_tiff::register(&mut ctx);
    oxideav_qoi::register(&mut ctx);
    oxideav_ico::register(&mut ctx);
    oxideav_image_filter::register(&mut ctx);
    common::register(&mut ctx);
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

fn write_png(path: &PathBuf, fmt: PixelFormat, w: u32, h: u32, data: Vec<u8>) {
    let img = Image::from_raw(w, h, fmt, data).unwrap();
    // The source layout is the PNG's layout: force it so the fixture
    // pins the IHDR colour type the test names.
    let opts = SaveOptions::new().with_pixel_format(fmt);
    oxideav_image::save(&ctx(), &img, path, &opts).expect("png encode");
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
    let planes = vec![
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
    ];
    let img = Image::from_planes(w, h, PixelFormat::Yuv444P, planes).unwrap();
    let opts = SaveOptions::new()
        .with_quality(90)
        .with_pixel_format(PixelFormat::Yuv444P);
    oxideav_image::save(&ctx(), &img, path, &opts).expect("jpeg encode");
}

/// A decoded PNG flattened to its layout, geometry and raw plane.
struct Decoded {
    format: PixelFormat,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

fn read_png(path: &PathBuf) -> Decoded {
    let file = oxideav_image::open(&ctx(), path).expect("output decodes");
    let img = file.primary();
    Decoded {
        format: img.format(),
        width: img.width(),
        height: img.height(),
        data: img.to_packed(img.format()).expect("packed layout"),
    }
}

#[test]
fn rgb24_png_passes_through_unchanged() {
    let dir = temp_dir("rgb");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 3);
    write_png(&src, PixelFormat::Rgb24, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgb24);
    assert_eq!((got.width, got.height), (24, 16));
    assert_eq!(got.data, px, "rgb24 → png must be sample-exact");
}

#[test]
fn rgba_png_passes_through_unchanged() {
    let dir = temp_dir("rgba");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 4);
    write_png(&src, PixelFormat::Rgba, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgba);
    assert_eq!(got.data, px, "rgba → png must be sample-exact");
}

#[test]
fn gray8_png_passes_through_unchanged() {
    let dir = temp_dir("gray");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 1);
    write_png(&src, PixelFormat::Gray8, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Gray8);
    assert_eq!(got.data, px, "gray8 → png must be sample-exact");
}

#[test]
fn rgb48_png_keeps_sixteen_bits() {
    let dir = temp_dir("rgb48");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 6);
    write_png(&src, PixelFormat::Rgb48Le, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgb48Le);
    assert_eq!(got.data, px, "rgb48 → png must be sample-exact");
}

/// Filter ops are planned on the filters' declared layouts: `-resize`
/// takes 16-bit words natively, so a 16-bit PNG stays 16-bit end to
/// end (no conversion node ahead of the filter, a 16-bit output); an
/// op whose filter declares no deep layout (`-sepia-tone`) gets one
/// conversion ahead of it, to the declared 8-bit layout that keeps
/// what the source carries, and the sink pick follows the chain's
/// output layout (an 8-bit PNG — nothing is widened back).
#[test]
fn resize_keeps_sixteen_bits_and_a_narrowing_filter_narrows_once() {
    use oxideav_cli_convert::frame_tap::TAP_SINK;
    use oxideav_pipeline::schema::TrackInput;
    let dir = temp_dir("deep-filters");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgb48Le, 24, 16, pattern(24, 16, 6));

    let out = dir.join("resized.png");
    convert(&[
        src.to_str().unwrap(),
        "-resize",
        "12x8!",
        out.to_str().unwrap(),
    ]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgb48Le, "resize keeps 16 bits");
    assert_eq!((got.width, got.height), (12, 8));
    let plan = oxideav_cli_convert::args::parse(&[
        src.to_str().unwrap().to_string(),
        "-resize".into(),
        "12x8!".into(),
        out.to_str().unwrap().to_string(),
    ])
    .unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    let f = job.outputs[TAP_SINK].video[0]
        .input
        .as_filter()
        .expect("resize node");
    assert!(
        f.input.is_source(),
        "no conversion ahead of a filter that takes the layout: {:?}",
        f.input
    );

    let out = dir.join("sepia.png");
    convert(&[
        src.to_str().unwrap(),
        "-sepia",
        "80%",
        out.to_str().unwrap(),
    ]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgb24, "the chain emits 8-bit RGB");
    let plan = oxideav_cli_convert::args::parse(&[
        src.to_str().unwrap().to_string(),
        "-sepia".into(),
        "80%".into(),
        out.to_str().unwrap().to_string(),
    ])
    .unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    let f = job.outputs[TAP_SINK].video[0]
        .input
        .as_filter()
        .expect("sepia node");
    let conv = match f.input.as_ref() {
        TrackInput::Convert(c) => c,
        other => panic!("one conversion ahead of sepia, got {other:?}"),
    };
    assert_eq!(conv.convert, "rgb24");
    assert!(conv.input.is_source());

    // A deep grey picture through a classic filter goes to Gray8 (its
    // declared grey route), never to RGB.
    let gray = dir.join("gray16.png");
    write_png(&gray, PixelFormat::Gray16Le, 8, 8, pattern(8, 8, 2));
    let out = dir.join("negated.png");
    convert(&[gray.to_str().unwrap(), "-negate", out.to_str().unwrap()]);
    assert_eq!(read_png(&out).format, PixelFormat::Gray8);
    // `-flip` takes the 16-bit words natively: the output is the same
    // 16-bit layout the plain conversion writes (the encoder's pick for
    // a `Gray16Le` source, whatever its declared set names).
    let bare = dir.join("bare.png");
    convert(&[gray.to_str().unwrap(), bare.to_str().unwrap()]);
    let out = dir.join("flipped.png");
    convert(&[gray.to_str().unwrap(), "-flip", out.to_str().unwrap()]);
    let (bare, flipped) = (read_png(&bare), read_png(&out));
    assert_eq!(flipped.format, bare.format, "flip keeps the plain pick");
    assert_eq!(
        oxideav_pixfmt::FormatInfo::of(flipped.format).bit_depth,
        16,
        "flip keeps 16 bits ({:?})",
        flipped.format
    );
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

/// The reference samples: the decoded source frame through the
/// default `oxideav-pixfmt` conversion to `to`.
fn reference(path: &PathBuf, to: PixelFormat) -> Vec<u8> {
    let (frame, fmt, w, h) = decode_first_frame(path);
    let out = oxideav_pixfmt::convert(
        &frame,
        oxideav_pixfmt::FrameInfo::new(fmt, w, h),
        to,
        &oxideav_pixfmt::ConvertOptions::default(),
    )
    .expect("reference conversion");
    out.planes[0].data.clone()
}

fn reference_rgb(path: &PathBuf) -> Vec<u8> {
    reference(path, PixelFormat::Rgb24)
}

/// YUV source → PNG: the colour samples must be exactly the decoded
/// frame through the default pixfmt conversion, carried as RGB — the
/// planner pins the encoder input to the narrowest layout that loses
/// nothing (the pipeline's own default, the PNG encoder's first
/// accepted layout, would synthesise an opaque alpha channel).
#[test]
fn yuv444_jpeg_to_png_is_rgb_with_the_reference_samples() {
    let dir = temp_dir("jpeg444");
    let (src, out) = (dir.join("in.jpg"), dir.join("out.png"));
    write_jpeg444(&src, 24, 16);
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!((got.width, got.height), (24, 16));
    assert_eq!(got.format, PixelFormat::Rgb24);
    assert_eq!(got.data, reference_rgb(&src), "RGB samples changed");
}

/// `-depth 16` asks for the deep layout even for an 8-bit source; the
/// samples are the converter's own 16-bit reconstruction.
#[test]
fn depth_16_widens_a_yuv_source() {
    let dir = temp_dir("depth16");
    let (src, out) = (dir.join("in.jpg"), dir.join("out.png"));
    write_jpeg444(&src, 24, 16);
    convert(&[src.to_str().unwrap(), "-depth", "16", out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgb48Le);
    assert_eq!(got.data, reference(&src, PixelFormat::Rgb48Le));
}

/// `-depth 8` caps a 16-bit source.
#[test]
fn depth_8_narrows_a_sixteen_bit_png() {
    let dir = temp_dir("depth8");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 6);
    write_png(&src, PixelFormat::Rgb48Le, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), "-depth", "8", out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PixelFormat::Rgb24);
    let high: Vec<u8> = px.chunks_exact(2).map(|w| w[1]).collect();
    assert_eq!(got.data, high, "16 → 8 keeps the high byte");
}

/// The job the planner emits for a still-image sink: the executor
/// decodes / converts one video track, pinned to video stream #0, into
/// the frame tap's reserved `@out` sink (the writer owns the codec),
/// with the explicit conversion node when the source layout is not
/// the pick.
#[test]
fn still_sink_job_pins_stream_and_layout() {
    use oxideav_cli_convert::frame_tap::TAP_SINK;
    let dir = temp_dir("jobshape");
    let src = dir.join("in.jpg");
    write_jpeg444(&src, 24, 16);
    let argv: Vec<String> = [src.to_str().unwrap(), "out.png"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let plan = oxideav_cli_convert::args::parse(&argv).unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    let out = &job.outputs[TAP_SINK];
    assert!(out.all.is_empty(), "still sinks do not fan out over kinds");
    assert_eq!(out.video.len(), 1);
    let track = &out.video[0];
    assert_eq!(track.codec, None, "the frame tap owns the encoder");
    let sel = track.stream_selector.as_ref().expect("selector");
    assert_eq!(sel.index, Some(0));
    let conv = track.input.as_convert().expect("explicit convert node");
    assert_eq!(conv.convert, "rgb24");

    // A PNG input the encoder takes as-is gets no conversion node.
    let png = dir.join("in.png");
    write_png(&png, PixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
    let argv: Vec<String> = [png.to_str().unwrap(), "out.png"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let plan = oxideav_cli_convert::args::parse(&argv).unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    assert!(job.outputs[TAP_SINK].video[0].input.is_source());
}

/// The JPEG container carries the `mjpeg` codec: a `.jpg` output must
/// resolve to the codec that has the encoder, not the container name.
#[test]
fn jpg_output_resolves_to_the_mjpeg_encoder() {
    let argv: Vec<String> = ["in.png", "out.jpg"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let plan = oxideav_cli_convert::args::parse(&argv).unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    let out = &job.outputs["out.jpg"];
    let track = out.all.first().or(out.video.first()).expect("one track");
    assert_eq!(track.codec.as_deref(), Some("mjpeg"));
}

/// `.webp`'s single-picture encoders are a prefixed family the gateway
/// resolves (`webp_vp8l` lossless by default, `webp_vp8` under a
/// `-quality`): the planner takes that pick as the still codec, so the
/// output runs through the writer (previously the stream was copied
/// into the webp muxer, which refused the source codec id).
#[test]
fn webp_output_resolves_to_the_gateway_prefixed_encoder() {
    use oxideav_cli_convert::image_sink::resolve_output_codec_with;
    let c = ctx();
    assert_eq!(
        resolve_output_codec_with(None, "out.webp", None, &c).as_deref(),
        Some("webp_vp8l")
    );
    assert_eq!(
        resolve_output_codec_with(None, "out.webp", Some(80), &c).as_deref(),
        Some("webp_vp8")
    );
    let dir = temp_dir("webp");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgba, 20, 12, pattern(20, 12, 4));
    let lossless = dir.join("out.webp");
    convert(&[src.to_str().unwrap(), lossless.to_str().unwrap()]);
    let back = oxideav_image::open(&c, &lossless).unwrap().into_primary();
    assert_eq!(back.to_rgba8().unwrap(), pattern(20, 12, 4), "lossless");
    let lossy = dir.join("lossy.webp");
    convert(&[
        src.to_str().unwrap(),
        "-quality",
        "50",
        lossy.to_str().unwrap(),
    ]);
    let back = oxideav_image::open(&c, &lossy).unwrap().into_primary();
    assert_eq!((back.width(), back.height()), (20, 12));
    assert_ne!(
        fs::read(&lossless).unwrap(),
        fs::read(&lossy).unwrap(),
        "a quality picks the lossy encoder"
    );
    // `-quality N` reaches the lossy encoder exactly when its schema
    // declares `quality` (the gateway's rule): then a low quality
    // writes fewer bytes than a high one on texture; an encoder that
    // declares none (webp_vp8 today) writes the same bytes for both.
    let declares_quality = c
        .codecs
        .encoder_options_schema(&oxideav_core::CodecId::new("webp_vp8"))
        .is_some_and(|schema| schema.iter().any(|f| f.name == "quality"));
    let noisy = dir.join("noise.png");
    write_noise_png(&noisy, 96, 64);
    let (lo, hi) = (dir.join("q20.webp"), dir.join("q90.webp"));
    convert(&[
        noisy.to_str().unwrap(),
        "-quality",
        "20",
        lo.to_str().unwrap(),
    ]);
    convert(&[
        noisy.to_str().unwrap(),
        "-quality",
        "90",
        hi.to_str().unwrap(),
    ]);
    let (lo, hi) = (fs::read(&lo).unwrap(), fs::read(&hi).unwrap());
    if declares_quality {
        assert!(lo.len() < hi.len(), "quality 20 < quality 90 in bytes");
    } else {
        assert_eq!(lo, hi, "no `quality` option: the quality cannot reach it");
    }
}

fn convert_err(args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    format!(
        "{}",
        oxideav_cli_convert::run(&args, &ctx()).expect_err("convert must fail")
    )
}

/// The encoder options path (convert's own encode + mux) produces the
/// same bytes as the pipeline's for the same encoder settings — here
/// the PNG encoder's default filter policy spelled out explicitly.
#[test]
fn opt_path_matches_the_plain_path_byte_for_byte() {
    let dir = temp_dir("optsame");
    let src = dir.join("in.jpg");
    write_jpeg444(&src, 24, 16);
    let (plain, tapped) = (dir.join("plain.png"), dir.join("tap.png"));
    convert(&[src.to_str().unwrap(), plain.to_str().unwrap()]);
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        "filter=adaptive",
        tapped.to_str().unwrap(),
    ]);
    assert_eq!(
        fs::read(&plain).unwrap(),
        fs::read(&tapped).unwrap(),
        "same encoder settings → same file"
    );
}

/// `--opt` reaches the encoder: an interlaced PNG carries IHDR
/// interlace method 1 and still decodes to the same samples.
#[test]
fn opt_reaches_the_encoder() {
    let dir = temp_dir("optinterlace");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 3);
    write_png(&src, PixelFormat::Rgb24, 24, 16, px.clone());
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        "interlace=true",
        out.to_str().unwrap(),
    ]);
    let bytes = fs::read(&out).unwrap();
    // Signature (8) + IHDR length/type (8) + width, height, depth,
    // colour type, compression, filter → interlace method at 28.
    assert_eq!(&bytes[12..16], b"IHDR");
    assert_eq!(bytes[28], 1, "Adam7 interlace");
    assert_eq!(read_png(&out).data, px);
}

#[test]
fn unknown_opt_key_is_a_typed_error() {
    let dir = temp_dir("optunknown");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
    let out = dir.join("out.png");
    let msg = convert_err(&[
        src.to_str().unwrap(),
        "--opt",
        "bogus=1",
        out.to_str().unwrap(),
    ]);
    assert!(msg.contains("has no option 'bogus'"), "{msg}");
    assert!(msg.contains("interlace"), "lists the known keys: {msg}");
    assert!(!out.exists(), "nothing written on a refused option");
}

#[test]
fn bad_opt_value_is_a_typed_error() {
    let dir = temp_dir("optbadvalue");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
    let msg = convert_err(&[
        src.to_str().unwrap(),
        "--opt",
        "interlace=maybe",
        dir.join("out.png").to_str().unwrap(),
    ]);
    assert!(msg.contains("expected a boolean"), "{msg}");
}

#[test]
fn opt_without_an_image_encoder_is_refused() {
    let dir = temp_dir("optnoenc");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
    let msg = convert_err(&[
        src.to_str().unwrap(),
        "--opt",
        "interlace=true",
        dir.join("out.unregistered").to_str().unwrap(),
    ]);
    assert!(msg.contains("--opt"), "{msg}");
}

/// Three distinct frames, as an APNG.
fn write_apng3(path: &PathBuf, w: u32, h: u32) -> Vec<Vec<u8>> {
    let frames: Vec<Vec<u8>> = (0..3u8)
        .map(|k| {
            pattern(w, h, 3)
                .iter()
                .map(|b| b.wrapping_add(k * 60))
                .collect()
        })
        .collect();
    let imgs: Vec<Image> = frames
        .iter()
        .map(|px| {
            Image::from_rgb8(w, h, px.clone())
                .unwrap()
                .with_delay(Some(std::time::Duration::from_millis(100)))
        })
        .collect();
    let opts = SaveOptions::new().with_pixel_format(PixelFormat::Rgb24);
    let bytes = oxideav_image::encode_frames(&ctx(), &imgs, "png", &opts).expect("apng encode");
    fs::write(path, bytes).expect("write apng");
    frames
}

/// `%d` fans out one file per frame of the stream, 0-based.
#[test]
fn template_fans_out_every_frame() {
    let dir = temp_dir("fanout");
    let src = dir.join("anim.png");
    let frames = write_apng3(&src, 20, 12);
    let tmpl = dir.join("frame-%02d.png");
    convert(&[src.to_str().unwrap(), tmpl.to_str().unwrap()]);
    for (i, px) in frames.iter().enumerate() {
        let f = dir.join(format!("frame-{i:02}.png"));
        let got = read_png(&f);
        assert_eq!(got.format, PixelFormat::Rgb24);
        assert_eq!(&got.data, px, "frame {i}");
    }
    assert!(!dir.join("frame-03.png").exists());
}

/// A single still through a template writes index 0.
#[test]
fn template_on_a_still_writes_index_zero() {
    let dir = temp_dir("fanout1");
    let src = dir.join("in.png");
    let px = pattern(8, 8, 3);
    write_png(&src, PixelFormat::Rgb24, 8, 8, px.clone());
    convert(&[
        src.to_str().unwrap(),
        dir.join("o_%d.png").to_str().unwrap(),
    ]);
    assert_eq!(read_png(&dir.join("o_0.png")).data, px);
}

/// Fan-out composes with filter ops: the frame geometry after the
/// ops is read off the frames themselves.
#[test]
fn template_with_a_geometry_op_uses_the_filtered_size() {
    let dir = temp_dir("fanoutcrop");
    let src = dir.join("anim.png");
    write_apng3(&src, 20, 12);
    let tmpl = dir.join("c%d.png");
    convert(&[
        src.to_str().unwrap(),
        "-crop",
        "10x6+2+3",
        tmpl.to_str().unwrap(),
    ]);
    for i in 0..3 {
        let got = read_png(&dir.join(format!("c{i}.png")));
        assert_eq!((got.width, got.height), (10, 6), "frame {i}");
    }
}

/// A noisy RGB image big enough to span several of the PNG encoder's
/// ~1 MiB deflate segments (LCG noise, so deflate has real work).
fn write_noise_png(path: &PathBuf, w: u32, h: u32) {
    let mut x: u32 = 0x9e37_79b9;
    let data: Vec<u8> = (0..(w * h * 3) as usize)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect();
    write_png(path, PixelFormat::Rgb24, w, h, data);
}

/// The PNG encoder's declared default for its `level` option.
fn png_default_level() -> u32 {
    let schema = oxideav_image::encoder_options(&ctx(), "png", &SaveOptions::default())
        .expect("png encoder schema");
    let field = schema
        .iter()
        .find(|f| f.name == "level")
        .expect("png encoder declares `level`");
    match &field.default {
        oxideav_core::OptionValue::U32(n) => *n,
        oxideav_core::OptionValue::I32(n) => *n as u32,
        other => panic!("unexpected default for level: {other:?}"),
    }
}

/// `--opt level=<default>` through the frame tap equals the plain
/// pipeline path byte for byte; `level=6` is a different (smaller or
/// equal) stream with the same samples.
#[test]
fn opt_level_matches_the_plain_path_at_the_default_level() {
    let dir = temp_dir("optlevel");
    let src = dir.join("in.png");
    write_noise_png(&src, 256, 128);
    let plain = dir.join("plain.png");
    let same = dir.join("same.png");
    let six = dir.join("six.png");
    convert(&[src.to_str().unwrap(), plain.to_str().unwrap()]);
    let default_level = format!("level={}", png_default_level());
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        &default_level,
        same.to_str().unwrap(),
    ]);
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        "level=6",
        six.to_str().unwrap(),
    ]);
    let (a, b, c) = (
        fs::read(&plain).unwrap(),
        fs::read(&same).unwrap(),
        fs::read(&six).unwrap(),
    );
    assert_eq!(a, b, "default level through --opt == plain path");
    assert!(c.len() <= a.len(), "level 6 is no larger than the default");
    assert_eq!(read_png(&six).data, read_png(&plain).data);
}

/// The frame-tap encoder is granted the resolved thread budget (the
/// job's `threads` key, else the host's parallelism) exactly like the
/// executor's own codecs. Pinned through the `tprobe` encoder, which
/// reports the budget it received instead of timing a race.
#[test]
fn opt_encoder_receives_the_thread_budget() {
    let dir = temp_dir("optthreads");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgb24, 8, 4, pattern(8, 4, 3));
    let three = dir.join("three.tprobe");
    let auto = dir.join("auto.tprobe");
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        "tag=x",
        "-threads",
        "3",
        three.to_str().unwrap(),
    ]);
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        "tag=x",
        auto.to_str().unwrap(),
    ]);
    assert_eq!(common::Report::read(&three).threads(), 3);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    assert_eq!(common::Report::read(&auto).threads(), cores);
}

/// A packed-RGB PNG becomes a 4:2:0 YCbCr JPEG (4:4:4 from `-quality
/// 90`): the `convert` convention, even though the JPEG encoder also
/// accepts RGB-coded input and would keep the layout on its own.
#[test]
fn rgb_png_to_jpeg_is_coded_as_subsampled_ycbcr() {
    let dir = temp_dir("jpegchroma");
    let src = dir.join("in.png");
    write_png(&src, PixelFormat::Rgb24, 32, 16, pattern(32, 16, 3));
    let plain = dir.join("plain.jpg");
    let hi = dir.join("hi.jpg");
    let chosen = dir.join("chosen.jpg");
    convert(&[src.to_str().unwrap(), plain.to_str().unwrap()]);
    convert(&[
        src.to_str().unwrap(),
        "-quality",
        "95",
        hi.to_str().unwrap(),
    ]);
    convert(&[
        src.to_str().unwrap(),
        "--opt",
        "chroma=yuv422",
        chosen.to_str().unwrap(),
    ]);
    let layout = |p: &PathBuf| {
        oxideav_image::open(&ctx(), p)
            .expect("jpeg decodes")
            .primary()
            .format()
    };
    assert!(
        matches!(layout(&plain), PixelFormat::YuvJ420P | PixelFormat::Yuv420P),
        "{:?}",
        layout(&plain)
    );
    assert!(
        matches!(layout(&hi), PixelFormat::YuvJ444P | PixelFormat::Yuv444P),
        "{:?}",
        layout(&hi)
    );
    assert!(
        matches!(
            layout(&chosen),
            PixelFormat::YuvJ422P | PixelFormat::Yuv422P
        ),
        "{:?}",
        layout(&chosen)
    );
}
