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

/// A gateway-written still (the side-channels' writer) hands the
/// plan's budget to its encoder through the gateway's
/// `SaveOptions::with_execution_context` — every encoder, not only one
/// whose schema declares a `threads` option (the test encoder declares
/// none). Without `-threads` the budget is the host's parallelism.
#[test]
fn gateway_written_stills_grant_the_budget_to_every_encoder() {
    use oxideav_cli_convert::raster_io::{encode_raster, RgbaImage};
    use oxideav_cli_convert::Op;
    let dir = temp_dir("gateway-budget");
    let img = RgbaImage {
        width: 4,
        height: 2,
        pixels: (0..32).collect(),
        stride: 16,
    };
    let out = dir.join("out.tprobe");
    let bytes = encode_raster(&ctx(), &img, s(&out), &[Op::Threads(3)]).unwrap();
    fs::write(&out, bytes).unwrap();
    assert_eq!(Report::read(&out).threads(), 3);
    let bytes = encode_raster(&ctx(), &img, s(&out), &[Op::Threads(0)]).unwrap();
    fs::write(&out, bytes).unwrap();
    assert_eq!(
        Report::read(&out).threads(),
        oxideav_core::ExecutionContext::auto().threads,
        "-threads 0 / none = the host's parallelism"
    );
}

/// `-set KEY=VALUE` / `-comment TEXT` reach the muxer before its
/// header exactly for the keys it carries (`Muxer::carries_metadata_key`
/// — the probe format stores `title` and `comment`); a later value for
/// a key wins; other keys are dropped at plan time (a note), never an
/// error; `-strip` withholds the source's blobs, not the caller's own
/// pairs. A malformed `-set` is `InvalidData` (umbrella exit 3).
#[test]
fn set_and_comment_reach_the_muxer_for_the_keys_it_carries() {
    let dir = temp_dir("set-metadata");
    let png = rgb_source(&dir, "in.tprobe", vec![]);
    let out = dir.join("out.tprobe");
    convert(&[
        s(&png),
        "-set",
        "title=First",
        "-comment",
        "a note",
        "-set",
        "artist=Nobody",
        "-set",
        "title=Hello",
        "-strip",
        s(&out),
    ])
    .unwrap();
    let r = Report::read(&out);
    assert_eq!(
        r.metadata(),
        vec![
            ("title".to_string(), "Hello".to_string()),
            ("comment".to_string(), "a note".to_string()),
        ]
    );
    assert!(
        r.muxer_blob_kinds().is_empty(),
        "-strip still withholds blobs"
    );

    // No pairs: the muxer sees none (its stored set stays empty).
    convert(&[s(&png), s(&out)]).unwrap();
    assert!(Report::read(&out).metadata().is_empty());

    let e = convert(&[s(&png), "-set", "title", s(&out)]).unwrap_err();
    assert!(matches!(e, oxideav_core::Error::InvalidData(_)), "{e}");
    assert!(!out.exists() || Report::read(&out).metadata().is_empty());
}

/// Every failure class this crate raises maps onto the umbrella CLI's
/// exit table (3 invalid data, 4 unsupported, 5 format not found, 8
/// input not found, 9 permission denied, 10 other I/O): the error
/// variant — and for I/O the kind — is what the binary branches on.
#[test]
fn failure_classes_match_the_umbrella_exit_table() {
    use oxideav_core::Error;
    use std::io::ErrorKind;
    let dir = temp_dir("exit-classes");
    let png = rgb_source(&dir, "in.tprobe", vec![]);
    let out = dir.join("out.tprobe");
    let io_kind = |e: &Error| match e {
        Error::Io(io) => Some(io.kind()),
        _ => None,
    };

    // 8: the input does not exist.
    let missing = dir.join("missing.tprobe");
    let e = convert(&[s(&missing), s(&out)]).unwrap_err();
    assert_eq!(io_kind(&e), Some(ErrorKind::NotFound), "{e}");

    // 8: the output's directory does not exist (the writer's create).
    let nodir = dir.join("no-such-dir").join("out.tprobe");
    let e = convert(&[s(&png), s(&nodir)]).unwrap_err();
    assert_eq!(io_kind(&e), Some(ErrorKind::NotFound), "{e}");

    // 3: a flag the parser rejects, a malformed `-set`, an `--opt`
    // the encoder does not declare.
    for argv in [
        vec![s(&png), "-no-such-flag", s(&out)],
        vec![s(&png), "-set", "=x", s(&out)],
        vec![s(&png), "--opt", "nope=1", s(&out)],
        vec![s(&png), "-quality", "x", s(&out)],
    ] {
        let e = convert(&argv).unwrap_err();
        assert!(matches!(e, Error::InvalidData(_)), "{argv:?}: {e}");
    }

    // 5: an output extension no container claims.
    let e = convert(&[s(&png), s(&dir.join("out.no_such_ext"))]).unwrap_err();
    assert!(matches!(e, Error::FormatNotFound(_)), "{e}");

    // 4: an input the restrictions refuse.
    let e = convert(&[s(&png), "--deny-containers", "tprobe", s(&out)]).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e}");

    // 1: an encoder panic is no typed class — `Other` with the panic
    // text — and the in-flight output is removed.
    let e = convert(&[s(&png), "--opt", "tag=panic", s(&out)]).unwrap_err();
    assert!(matches!(e, Error::Other(_)), "{e}");
    assert!(e.to_string().contains("asked to panic"), "{e}");
    assert!(!out.exists(), "a panicked write leaves no file behind");

    // 9: a directory the process may not write into (skipped for a
    // user the OS never refuses).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let target = locked.join("out.tprobe");
        let r = convert(&[s(&png), s(&target)]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        if let Err(e) = r {
            assert_eq!(io_kind(&e), Some(ErrorKind::PermissionDenied), "{e}");
            assert!(!target.exists(), "no file left behind");
        }
    }
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
fn filter_ops_on_a_palette_source_follow_the_filters_declared_routes() {
    // A `Pal8` source through a pointwise filter (`-negate`) takes the
    // filter's palette route: the indices are untouched and the
    // palette is negated, so a palette-accepting encoder gets the
    // palette picture. A filter that is not pointwise (`-blur`) takes
    // `Pal8` through its packed-RGB hop and emits RGBA (palette
    // entries may be transparent). The planner inserts nothing in
    // either case: the filters declare both routes.
    let dir = temp_dir("palfilter");
    let p = dir.join("in.tprobe");
    Source::packed(4, 4, PixelFormat::Pal8, |x, _| vec![(x % 2) as u8])
        .with_palette(vec![[255, 0, 0, 255], [0, 0, 255, 255]])
        .write(&p);
    let out = dir.join("out.tprobe");
    convert(&[s(&p), "-negate", s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.format(), "Pal8");
    assert_eq!(r.first_bytes()[..4], [0, 1, 0, 1], "indices untouched");
    // Negated red (255,0,0) → (0,255,255), blue → yellow.
    assert_eq!(r.palette_rgb().unwrap(), [0, 255, 255, 255, 255, 0]);

    let blurred = dir.join("blur.tprobe");
    convert(&[s(&p), "-blur", "1x0.5", s(&blurred)]).unwrap();
    let r = Report::read(&blurred);
    assert_eq!(r.format(), "Rgba", "the hop's layout");
    assert_eq!(r.first_bytes()[3], 255, "opaque entries stay opaque");
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

/// A `Pal8` source with a transparent entry.
fn palette_source(dir: &Path) -> PathBuf {
    let p = dir.join("pal.tprobe");
    // Columns: 0 = opaque red, 1 = transparent blue, 2 = half green.
    Source::packed(3, 2, PixelFormat::Pal8, |x, _| vec![x as u8])
        .with_palette(vec![[255, 0, 0, 255], [0, 0, 255, 0], [0, 255, 0, 128]])
        .with_stream_blobs(vec![icc()])
        .write(&p);
    p
}

#[test]
fn palette_alpha_rides_to_a_palette_target() {
    let dir = temp_dir("pal-keep");
    let src = palette_source(&dir);
    let out = dir.join("out.tprobe");
    convert(&[s(&src), s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.format(), "Pal8", "the encoder takes Pal8: no expansion");
    assert_eq!(r.palette_len(), Some(3));
    assert_eq!(
        r.palette_alpha(),
        Some(vec![255, 0, 128]),
        "the record is on the frame"
    );
    assert_eq!(r.first_bytes()[..3], [0, 1, 2]);
    assert_eq!(r.stream_blob_kinds(), ["icc"]);
}

#[test]
fn palette_alpha_expands_to_an_alpha_target() {
    let dir = temp_dir("pal-rgba");
    let src = palette_source(&dir);
    let out = dir.join("out.tprgba");
    convert(&[s(&src), s(&out)]).unwrap();
    let r = Report::read(&out);
    assert_eq!(r.format(), "Rgba");
    let px = r.first_bytes();
    assert_eq!(&px[..4], &[255, 0, 0, 255]);
    assert_eq!(&px[4..8], &[0, 0, 255, 0], "transparent entry → alpha 0");
    assert_eq!(
        r.stream_blob_kinds(),
        ["icc"],
        "blobs survive the expansion"
    );
}

#[test]
fn palette_alpha_is_composited_for_a_target_without_alpha() {
    let dir = temp_dir("pal-rgb");
    let src = palette_source(&dir);
    let white = dir.join("white.tprgb");
    let red = dir.join("red.tprgb");
    convert(&[s(&src), s(&white)]).unwrap();
    convert(&[s(&src), "-background", "red", s(&red)]).unwrap();
    let w = Report::read(&white);
    assert_eq!(w.format(), "Rgb24");
    let px = w.first_bytes();
    assert_eq!(&px[..3], &[255, 0, 0], "opaque entry as is");
    assert_eq!(&px[3..6], &[255, 255, 255], "transparent entry = white");
    assert_eq!(&px[6..8], &[127, 255], "half green over white");
    let r = Report::read(&red);
    let px = r.first_bytes();
    assert_eq!(
        &px[3..6],
        &[255, 0, 0],
        "transparent entry = the -background colour"
    );
    assert_eq!(&px[6..8], &[127, 128], "half green over red");
    assert_eq!(r.muxer_blob_kinds(), ["icc"]);
}

#[test]
fn source_aware_resize_modes_resolve_against_the_input_geometry() {
    let dir = temp_dir("geometry");
    let src = rgb_source(&dir, "in.tprobe", vec![]); // 6 × 4
    let size = |args: &[&str]| {
        let out = dir.join(format!(
            "{}.tprobe",
            args.join("_").replace(['%', '>', '<', '^', '@', '!'], "x")
        ));
        let mut v = vec![s(&src)];
        v.extend_from_slice(args);
        v.push(s(&out));
        convert(&v).unwrap();
        let r = Report::read(&out);
        (
            r.frame()["width"].as_u64().unwrap(),
            r.frame()["height"].as_u64().unwrap(),
        )
    };
    assert_eq!(size(&["-resize", "50%"]), (3, 2), "percent of the source");
    assert_eq!(
        size(&["-resize", "3x3"]),
        (3, 2),
        "default keeps the aspect"
    );
    assert_eq!(size(&["-resize", "3x3!"]), (3, 3), "force is literal");
    assert_eq!(size(&["-resize", "100x100>"]), (6, 4), "shrink only");
    assert_eq!(size(&["-resize", "12x12<"]), (12, 8), "grow only");
    assert_eq!(size(&["-resize", "3x3^"]), (5, 3), "fill covers the box");
    assert_eq!(size(&["-resize", "3x2@"]), (3, 2), "area");
    assert_eq!(
        size(&["-crop", "4x4+0+0", "-resize", "50%"]),
        (2, 2),
        "percent of the cropped geometry"
    );
    assert_eq!(size(&["-thumbnail", "50%"]), (3, 2));
    use oxideav_cli_convert::{Op, ResizeMode};
    let ops = vec![
        Op::Resize {
            width: 50,
            height: 50,
            mode: ResizeMode::Percent,
        },
        Op::Trim { fuzz: 0 },
        Op::Resize {
            width: 50,
            height: 50,
            mode: ResizeMode::Percent,
        },
    ];
    let resolved = oxideav_cli_convert::plan_to_job::resolve_geometry_ops(&ops, Some((100, 60)));
    assert!(matches!(
        resolved[0],
        Op::Resize {
            width: 50,
            height: 30,
            mode: ResizeMode::Force
        }
    ));
    assert!(
        matches!(
            resolved[2],
            Op::Resize {
                width: 50,
                height: 50,
                mode: ResizeMode::Percent
            }
        ),
        "unknown after -trim"
    );
    assert_eq!(
        oxideav_cli_convert::plan_to_job::resolve_geometry_ops(&ops, None).len(),
        3
    );
}
