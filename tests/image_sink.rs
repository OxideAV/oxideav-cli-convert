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
use oxideav_png::{decode, encode, EncodeOptions, PngImage, PngPixelFormat};

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
    oxideav_image_filter::register(&mut ctx);
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
    let img = PngImage::packed(w, h, fmt, w as usize * fmt.bytes_per_pixel(), data).unwrap();
    fs::write(
        path,
        encode(&img, &EncodeOptions::default()).expect("png encode"),
    )
    .expect("write png");
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

/// A decoded PNG flattened to its layout, geometry and raw plane.
struct Decoded {
    format: PngPixelFormat,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

fn read_png(path: &PathBuf) -> Decoded {
    let img = decode(&fs::read(path).expect("output written")).expect("output decodes");
    Decoded {
        format: img.format(),
        width: img.width,
        height: img.height,
        data: img.into_raw(),
    }
}

#[test]
fn rgb24_png_passes_through_unchanged() {
    let dir = temp_dir("rgb");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 3);
    write_png(&src, PngPixelFormat::Rgb24, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PngPixelFormat::Rgb24);
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
    assert_eq!(got.format, PngPixelFormat::Rgba);
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
    assert_eq!(got.format, PngPixelFormat::Gray8);
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
    assert_eq!(got.format, PngPixelFormat::Rgb48Le);
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
    assert_eq!(got.format, PngPixelFormat::Rgb24);
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
    assert_eq!(got.format, PngPixelFormat::Rgb48Le);
    assert_eq!(got.data, reference(&src, PixelFormat::Rgb48Le));
}

/// `-depth 8` caps a 16-bit source.
#[test]
fn depth_8_narrows_a_sixteen_bit_png() {
    let dir = temp_dir("depth8");
    let (src, out) = (dir.join("in.png"), dir.join("out.png"));
    let px = pattern(24, 16, 6);
    write_png(&src, PngPixelFormat::Rgb48Le, 24, 16, px.clone());
    convert(&[src.to_str().unwrap(), "-depth", "8", out.to_str().unwrap()]);
    let got = read_png(&out);
    assert_eq!(got.format, PngPixelFormat::Rgb24);
    let high: Vec<u8> = px.chunks_exact(2).map(|w| w[1]).collect();
    assert_eq!(got.data, high, "16 → 8 keeps the high byte");
}

/// The job the planner emits for a still-image sink: one video track
/// pinned to video stream #0, with the explicit conversion node when
/// the source layout is not the pick.
#[test]
fn still_sink_job_pins_stream_and_layout() {
    let dir = temp_dir("jobshape");
    let src = dir.join("in.jpg");
    write_jpeg444(&src, 24, 16);
    let argv: Vec<String> = [src.to_str().unwrap(), "out.png"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let plan = oxideav_cli_convert::args::parse(&argv).unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    let out = &job.outputs["out.png"];
    assert!(out.all.is_empty(), "still sinks do not fan out over kinds");
    assert_eq!(out.video.len(), 1);
    let track = &out.video[0];
    assert_eq!(track.codec.as_deref(), Some("png"));
    let sel = track.stream_selector.as_ref().expect("selector");
    assert_eq!(sel.index, Some(0));
    let conv = track.input.as_convert().expect("explicit convert node");
    assert_eq!(conv.convert, "rgb24");

    // A PNG input the encoder takes as-is gets no conversion node.
    let png = dir.join("in.png");
    write_png(&png, PngPixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
    let argv: Vec<String> = [png.to_str().unwrap(), "out.png"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let plan = oxideav_cli_convert::args::parse(&argv).unwrap();
    let job = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap();
    assert!(job.outputs["out.png"].video[0].input.is_source());
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
    write_png(&src, PngPixelFormat::Rgb24, 24, 16, px.clone());
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
    write_png(&src, PngPixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
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
    write_png(&src, PngPixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
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
    write_png(&src, PngPixelFormat::Rgb24, 4, 4, pattern(4, 4, 3));
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
    let imgs: Vec<PngImage> = frames
        .iter()
        .map(|px| {
            PngImage::packed(w, h, PngPixelFormat::Rgb24, w as usize * 3, px.clone()).unwrap()
        })
        .collect();
    let bytes = oxideav_png::encode_apng(&imgs, 10, 0).expect("apng encode");
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
        assert_eq!(got.format, PngPixelFormat::Rgb24);
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
    write_png(&src, PngPixelFormat::Rgb24, 8, 8, px.clone());
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
    write_png(path, PngPixelFormat::Rgb24, w, h, data);
}

fn timed_convert(args: &[&str]) -> std::time::Duration {
    let t = std::time::Instant::now();
    convert(args);
    t.elapsed()
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
    let default_level = format!("level={}", oxideav_png::DEFAULT_COMPRESSION_LEVEL);
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
/// executor's own codecs: the bytes do not depend on the budget, and
/// on a multi-core host the parallel encode is faster than `-threads 1`.
#[test]
fn opt_encoder_receives_the_thread_budget() {
    let dir = temp_dir("optthreads");
    let src = dir.join("in.png");
    write_noise_png(&src, 1536, 1024);
    let serial = dir.join("serial.png");
    let auto = dir.join("auto.png");
    let t_serial = timed_convert(&[
        src.to_str().unwrap(),
        "--opt",
        "level=6",
        "-threads",
        "1",
        serial.to_str().unwrap(),
    ]);
    let t_auto = timed_convert(&[
        src.to_str().unwrap(),
        "--opt",
        "level=6",
        auto.to_str().unwrap(),
    ]);
    assert_eq!(
        fs::read(&serial).unwrap(),
        fs::read(&auto).unwrap(),
        "the segment grid does not depend on the thread budget"
    );
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores >= 2 {
        assert!(
            t_auto < t_serial,
            "parallel encode ({t_auto:?}) should beat -threads 1 ({t_serial:?}) on {cores} cores"
        );
    }
}
