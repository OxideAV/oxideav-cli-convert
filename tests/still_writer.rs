//! The still-image write path (`frame_tap`) pinned through the
//! test-only `tprobe` format (`tests/common`): what the encoder and
//! muxer receive for a still output — the thread budget, the
//! schema-gated `quality`, the metadata blobs at both levels and their
//! precedence, `-strip`, and the layout the planner picked.

use std::fs;
use std::path::{Path, PathBuf};

use oxideav_core::{BlobKind, MetadataBlob, PixelFormat, RuntimeContext};
use oxideav_image::{Image, SaveOptions};

mod common;
use common::{Report, Source};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-still-writer-{name}-{}-{}",
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
    common::register(&mut ctx);
    ctx
}

fn convert(args: &[&str]) -> Result<(), oxideav_core::Error> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    oxideav_cli_convert::run(&args, &ctx())
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// An RGB source picture with the given stream-level blobs.
fn rgb_source(dir: &Path, name: &str, stream_blobs: Vec<MetadataBlob>) -> PathBuf {
    let p = dir.join(name);
    Source::packed(6, 4, PixelFormat::Rgb24, |x, y| {
        vec![(x * 40) as u8, (y * 60) as u8, 7]
    })
    .with_stream_blobs(stream_blobs)
    .write(&p);
    p
}

fn icc() -> MetadataBlob {
    MetadataBlob::new(BlobKind::ICC, vec![0x49, 0x43, 0x43, 1, 2, 3])
        .with_mime("application/vnd.iccprofile")
}

fn exif(tag: u8) -> MetadataBlob {
    MetadataBlob::new(BlobKind::EXIF, vec![b'M', b'M', 0, 42, tag])
}

#[test]
fn quality_is_forwarded_only_to_a_declaring_encoder_and_opt_wins() {
    let dir = temp_dir("quality");
    let src = rgb_source(&dir, "in.tprobe", vec![]);
    let plain = dir.join("plain.tprobe");
    let q42 = dir.join("q42.tprobe");
    let opt = dir.join("opt.tprobe");
    convert(&[s(&src), s(&plain)]).unwrap();
    convert(&[s(&src), "-quality", "42", s(&q42)]).unwrap();
    convert(&[s(&src), "-quality", "42", "--opt", "quality=50", s(&opt)]).unwrap();
    assert_eq!(Report::read(&plain).quality(), None, "nothing asked");
    assert_eq!(Report::read(&q42).quality(), Some(42));
    assert_eq!(Report::read(&opt).quality(), Some(50), "--opt is explicit");

    // An encoder whose schema has no `quality` (png) is never handed
    // one: the conversion succeeds and the option is not forwarded.
    let png = dir.join("q.png");
    convert(&[s(&src), "-quality", "42", s(&png)]).unwrap();
    let img = oxideav_image::open(&ctx(), &png).unwrap();
    assert_eq!(img.primary().width(), 6);
}

#[test]
fn stream_blobs_reach_the_encoder_and_the_muxer() {
    let dir = temp_dir("blobs");
    let src = rgb_source(&dir, "in.tprobe", vec![icc(), exif(1)]);
    let out = dir.join("out.tprobe");
    convert(&[s(&src), s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.stream_blob_kinds(), ["icc", "exif"]);
    assert_eq!(r.stream_blob("icc").unwrap(), icc().data);
    assert_eq!(r.stream_blob("exif").unwrap(), exif(1).data);
    assert_eq!(
        r.muxer_blob_kinds(),
        ["icc", "exif"],
        "the muxer's stream carries them even though the encoder's output parameters do not"
    );
    assert!(r.frame_blob_kinds().is_empty(), "nothing per frame");
}

#[test]
fn a_frame_blob_overrides_the_stream_blob_of_its_kind() {
    let dir = temp_dir("precedence");
    let p = dir.join("in.tprobe");
    Source::packed(4, 4, PixelFormat::Rgb24, |_, _| vec![1, 2, 3])
        .with_stream_blobs(vec![icc(), exif(1)])
        .with_frame_blobs(vec![exif(2)])
        .write(&p);
    let out = dir.join("out.tprobe");
    convert(&[s(&p), s(&out)]).unwrap();
    let r = Report::read(&out);
    // Single-picture promotion: the frame's Exif is the file's, ahead
    // of the stream's ICC; the stream's Exif is replaced.
    assert_eq!(r.stream_blob_kinds(), ["exif", "icc"]);
    assert_eq!(r.stream_blob("exif").unwrap(), exif(2).data);
    assert_eq!(r.frame_blob_kinds(), ["exif"], "the frame keeps its record");
    assert_eq!(r.muxer_blob_kinds(), ["exif", "icc"]);
}

#[test]
fn strip_withholds_every_blob() {
    let dir = temp_dir("strip");
    let p = dir.join("in.tprobe");
    Source::packed(4, 4, PixelFormat::Rgb24, |_, _| vec![1, 2, 3])
        .with_stream_blobs(vec![icc(), exif(1)])
        .with_frame_blobs(vec![exif(2)])
        .write(&p);
    let out = dir.join("out.tprobe");
    convert(&[s(&p), "-strip", s(&out)]).unwrap();
    let r = Report::read(&out);
    assert!(r.stream_blob_kinds().is_empty(), "{:?}", r.frame());
    assert!(r.frame_blob_kinds().is_empty());
    assert!(r.muxer_blob_kinds().is_empty());
    // `-thumbnail` implies `-strip` (its documented contract). The
    // source carries its blobs on the stream only: the resize filter
    // (oxideav-image-filter) does not accept a frame with a metadata
    // record ahead of its planes today.
    let plain = rgb_source(&dir, "plain.tprobe", vec![icc(), exif(1)]);
    let thumb = dir.join("thumb.tprobe");
    convert(&[s(&plain), "-thumbnail", "2x2", s(&thumb)]).unwrap();
    let r = Report::read(&thumb);
    assert!(r.stream_blob_kinds().is_empty() && r.muxer_blob_kinds().is_empty());
}

#[test]
fn blobs_survive_filter_ops_and_the_layout_follows_the_pick() {
    let dir = temp_dir("filtered");
    let src = rgb_source(&dir, "in.tprobe", vec![icc()]);
    let out = dir.join("out.tprobe");
    convert(&[s(&src), "-resize", "3x2!", "-negate", s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.format(), "Rgb24");
    assert_eq!(r.frame()["width"], 3);
    assert_eq!(r.frame()["height"], 2);
    assert_eq!(
        r.stream_blob_kinds(),
        ["icc"],
        "stream blobs ride the plan, not the frame"
    );
    assert_eq!(r.muxer_blob_kinds(), ["icc"]);
}

#[test]
fn a_png_source_through_the_gateway_lands_in_its_own_layout() {
    // A PNG written by the gateway in gray; the probe encoder accepts
    // Gray8, so the picture arrives untouched with the budget granted.
    let dir = temp_dir("png");
    let png = dir.join("in.png");
    let img = Image::from_raw(5, 3, PixelFormat::Gray8, (0..15).collect()).unwrap();
    oxideav_image::save(
        &ctx(),
        &img,
        &png,
        &SaveOptions::new().with_pixel_format(PixelFormat::Gray8),
    )
    .unwrap();
    let out = dir.join("out.tprobe");
    convert(&[s(&png), "-threads", "2", s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.format(), "Gray8");
    assert_eq!(r.threads(), 2);
    assert_eq!(r.first_bytes(), [0, 1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn allow_and_deny_lists_refuse_before_any_decode() {
    use oxideav_cli_convert::restrict::is_restricted;
    let dir = temp_dir("restrict");
    let src = rgb_source(&dir, "in.tprobe", vec![]);
    let out = dir.join("out.tprobe");
    let refused = |args: &[&str]| {
        let mut v = vec![s(&src)];
        v.extend_from_slice(args);
        v.push(s(&out));
        let e = convert(&v).unwrap_err();
        assert!(is_restricted(&e), "{e}");
        assert!(!out.exists(), "nothing written: {e}");
    };
    refused(&["--deny-containers", "tprobe"]);
    refused(&["--allow-containers", "png,heif"]);
    refused(&["--deny-codecs", "TPROBE"]);
    refused(&["--allow-codecs", "png"]);
    // Allowed: the lists name the input.
    convert(&[
        s(&src),
        "--allow-containers",
        "tprobe",
        "--allow-codecs",
        "tprobe",
        "--deny-codecs",
        "hevc",
        s(&out),
    ])
    .unwrap();
    assert!(out.exists());

    // The planner refuses the same way (library callers of plan_to_job).
    let plan = oxideav_cli_convert::ConvertPlan::new(s(&src), s(&out)).with_restrictions(
        oxideav_cli_convert::Restrictions::new().with_deny_containers(["tprobe"]),
    );
    let e = oxideav_cli_convert::plan_to_job::plan_to_job(&plan, &ctx()).unwrap_err();
    assert!(is_restricted(&e), "{e}");

    // The gateway-backed side-channel (`.ico` output reads through the
    // gateway's OpenOptions) maps the gateway's refusal to the same shape.
    let ico = dir.join("out.ico");
    let mut v = vec![s(&src), "--deny-containers", "tprobe"];
    v.push(s(&ico));
    let e = oxideav_cli_convert::run(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>(), &{
        let mut c = ctx();
        oxideav_ico::register(&mut c);
        c
    })
    .unwrap_err();
    assert!(is_restricted(&e), "{e}");
}

#[test]
fn filter_ops_on_a_palette_source_run_in_rgba_and_keep_the_pick() {
    // A `Pal8` source: the filters do not take it, so the planner
    // converts it ahead of the first filter op (RGBA: palette entries
    // may be transparent); the probe encoder accepts RGBA and gets the
    // filtered picture.
    let dir = temp_dir("palfilter");
    let p = dir.join("in.tprobe");
    Source::packed(4, 4, PixelFormat::Pal8, |x, _| vec![(x % 2) as u8])
        .with_palette(vec![[255, 0, 0, 255], [0, 0, 255, 255]])
        .write(&p);
    let out = dir.join("out.tprobe");
    convert(&[s(&p), "-negate", s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.format(), "Rgba");
    // Negated red (255,0,0) → (0,255,255), alpha untouched.
    assert_eq!(r.first_bytes()[..4], [0, 255, 255, 255]);
    assert_eq!(
        oxideav_cli_convert::plan_to_job::filter_input_layout(PixelFormat::Pal8),
        Some(PixelFormat::Rgba)
    );
    assert_eq!(
        oxideav_cli_convert::plan_to_job::filter_input_layout(PixelFormat::YuvJ420P),
        Some(PixelFormat::Rgb24)
    );
    assert_eq!(
        oxideav_cli_convert::plan_to_job::filter_input_layout(PixelFormat::Gray16Le),
        Some(PixelFormat::Gray8)
    );
    assert_eq!(
        oxideav_cli_convert::plan_to_job::filter_input_layout(PixelFormat::Yuva420P),
        Some(PixelFormat::Rgba)
    );
    assert_eq!(
        oxideav_cli_convert::plan_to_job::filter_input_layout(PixelFormat::Rgb24),
        None
    );
}
