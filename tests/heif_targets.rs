//! The HEIF-family targets through the released `oxideav-heif` /
//! `oxideav-avif`: the requested format name picks the codec (`.avif`
//! → `avif`, `.heic` → `heif`), ICC / Exif blobs survive HEIC → AVIF →
//! HEIC and `-strip` drops them, `-quality` yields a monotone size
//! ladder on textured content for HEIC / AVIF / JPEG, the thread budget
//! never changes the bytes, and a `.heics` sequence feeds a timed
//! output while a single still output takes the primary picture.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oxideav_core::{BlobKind, MetadataBlob, RuntimeContext};
use oxideav_image::{Image, SaveOptions};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-heif-{name}-{}-{}",
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
    oxideav_gif::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    oxideav_avif::register(&mut ctx);
    oxideav_h265::register(&mut ctx);
    oxideav_image_filter::register(&mut ctx);
    ctx
}

fn convert(args: &[&str]) -> Result<(), oxideav_core::Error> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    oxideav_cli_convert::run(&args, &ctx())
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Textured RGB content: a smooth field plus a hash-noise term, so a
/// lossy encoder's size moves with its quality (a linear gradient
/// would not).
fn textured(w: u32, h: u32) -> Image {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let mut seed: u32 = 0x9E37_79B9;
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = (seed >> 24) as i32 - 128;
            let base = |k: i32| (x as i32 * 3 + y as i32 * k) % 256;
            for (c, k) in [(0i32, 5i32), (1, 7), (2, 11)] {
                let v = base(k) + n / 3 + c * 20;
                rgb.push(v.clamp(0, 255) as u8);
            }
        }
    }
    Image::from_rgb8(w, h, rgb).unwrap()
}

fn icc() -> MetadataBlob {
    MetadataBlob::new(BlobKind::ICC, (0..64u8).collect::<Vec<u8>>())
        .with_mime("application/vnd.iccprofile")
}

fn exif() -> MetadataBlob {
    MetadataBlob::new(
        BlobKind::EXIF,
        vec![b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 1, 2],
    )
}

fn write_png(dir: &Path, name: &str, img: &Image) -> PathBuf {
    let p = dir.join(name);
    oxideav_image::save(&ctx(), img, &p, &SaveOptions::default()).expect("png");
    p
}

#[test]
fn the_requested_format_name_picks_the_codec() {
    let c = ctx();
    let avif = oxideav_cli_convert::image_sink::resolve_output_codec(None, "out.avif", &c);
    assert_eq!(
        avif.as_deref(),
        Some("avif"),
        "the avif encoder, not the heif container's HEVC"
    );
    let heic = oxideav_cli_convert::image_sink::resolve_output_codec(None, "out.heic", &c);
    assert_eq!(heic.as_deref(), Some("heif"));
    let heif = oxideav_cli_convert::image_sink::resolve_output_codec(Some("heif"), "out.bin", &c);
    assert_eq!(heif.as_deref(), Some("heif"));
    let jpg = oxideav_cli_convert::image_sink::resolve_output_codec(None, "out.jpg", &c);
    assert_eq!(jpg.as_deref(), Some("mjpeg"));

    // End to end: `.avif` written through the avif encoder reads back
    // as an AV1 item; `.heic` as HEVC.
    let dir = temp_dir("codecpick");
    let png = write_png(&dir, "in.png", &textured(64, 48));
    let avif = dir.join("out.avif");
    let heic = dir.join("out.heic");
    convert(&[s(&png), s(&avif)]).unwrap();
    convert(&[s(&png), s(&heic)]).unwrap();
    for (p, brand) in [(&avif, b"avif"), (&heic, b"heic")] {
        let bytes = fs::read(p).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp", "{}", p.display());
        assert_eq!(&bytes[8..12], brand, "{}", p.display());
        let img = oxideav_image::open(&ctx(), p).unwrap();
        assert_eq!((img.primary().width(), img.primary().height()), (64, 48));
    }
}

#[test]
fn icc_and_exif_survive_the_heif_family_and_strip_drops_them() {
    let dir = temp_dir("blobs");
    let src = dir.join("in.heic");
    let opts = SaveOptions::new().with_blob(icc()).with_blob(exif());
    oxideav_image::save(&ctx(), &textured(48, 32), &src, &opts).unwrap();
    let file = oxideav_image::open(&ctx(), &src).unwrap();
    assert_eq!(
        file.icc(),
        Some(&icc().data[..]),
        "fixture carries the profile"
    );
    assert_eq!(file.exif(), Some(&exif().data[..]));

    // HEIC → HEIC: the heif encoder embeds the stream's blobs.
    let heic = dir.join("b.heic");
    convert(&[s(&src), s(&heic)]).unwrap();
    let b = oxideav_image::open(&ctx(), &heic).unwrap();
    assert_eq!(
        b.icc(),
        Some(&icc().data[..]),
        "ICC bytes through HEIC → HEIC"
    );
    assert_eq!(
        b.exif(),
        Some(&exif().data[..]),
        "Exif payload through HEIC → HEIC"
    );

    // An AVIF-brand source (AV1 items written by the heif encoder with
    // the same blobs) → HEIC keeps them too.
    let avif_src = dir.join("in.avif");
    let av1 = SaveOptions::new()
        .with_blob(icc())
        .with_blob(exif())
        .with_codec("heif")
        .with_option("codec", "av1");
    oxideav_image::save(&ctx(), &textured(48, 32), &avif_src, &av1).unwrap();
    assert_eq!(&fs::read(&avif_src).unwrap()[8..12], b"avif");
    let from_avif = dir.join("c.heic");
    convert(&[s(&avif_src), s(&from_avif)]).unwrap();
    let c = oxideav_image::open(&ctx(), &from_avif).unwrap();
    assert_eq!(c.icc(), Some(&icc().data[..]), "… and AVIF → HEIC");
    assert_eq!(c.exif(), Some(&exif().data[..]));

    // With filter ops the stream's blobs still reach the writer.
    let small = dir.join("small.heic");
    convert(&[s(&src), "-resize", "24x16!", s(&small)]).unwrap();
    let sm = oxideav_image::open(&ctx(), &small).unwrap();
    assert_eq!(sm.icc(), Some(&icc().data[..]));
    assert_eq!((sm.primary().width(), sm.primary().height()), (24, 16));

    let stripped = dir.join("d.heic");
    convert(&[s(&heic), "-strip", s(&stripped)]).unwrap();
    let d = oxideav_image::open(&ctx(), &stripped).unwrap();
    assert_eq!(d.icc(), None, "-strip withholds the profile");
    assert_eq!(d.exif(), None);
    assert!(!d.has_metadata());
}

/// Sizes at increasing `-quality` on textured content never decrease
/// (strictly increase across the span), for every lossy target.
#[test]
fn quality_ladders_are_monotone_on_textured_content() {
    let dir = temp_dir("ladder");
    let png = write_png(&dir, "in.png", &textured(96, 64));
    for ext in ["heic", "avif", "jpg"] {
        let mut sizes = Vec::new();
        for q in ["20", "50", "80", "95"] {
            let out = dir.join(format!("q{q}.{ext}"));
            convert(&[s(&png), "-quality", q, s(&out)])
                .unwrap_or_else(|e| panic!("{ext} at quality {q}: {e}"));
            sizes.push(fs::metadata(&out).unwrap().len());
        }
        assert!(
            sizes.windows(2).all(|w| w[0] <= w[1]),
            "{ext}: sizes not monotone: {sizes:?}"
        );
        assert!(
            sizes[0] < sizes[3],
            "{ext}: quality 20 and 95 the same size: {sizes:?}"
        );
    }
}

#[test]
fn the_thread_budget_does_not_change_the_bytes() {
    let dir = temp_dir("budget");
    let png = write_png(&dir, "in.png", &textured(80, 48));
    for ext in ["heic", "avif"] {
        let one = dir.join(format!("one.{ext}"));
        let many = dir.join(format!("many.{ext}"));
        convert(&[s(&png), "-threads", "1", s(&one)]).unwrap();
        convert(&[s(&png), "-threads", "4", s(&many)]).unwrap();
        assert_eq!(fs::read(&one).unwrap(), fs::read(&many).unwrap(), "{ext}");
    }
}

/// Three timed pictures (an animated GIF) → `.heics`: the frames are
/// coded as an HEVC track for the sequence muxer (not refused by the
/// still encoder). From that sequence a `%d` template writes the
/// track's frames, a single still output the primary picture, and a
/// timed output (animated GIF) takes the track.
#[test]
fn heics_sequence_feeds_timed_outputs_and_a_still_takes_the_primary() {
    let dir = temp_dir("heics");
    let frames: Vec<Image> = (0..3u32)
        .map(|i| {
            let mut rgb = Vec::new();
            for y in 0..16u32 {
                for x in 0..16u32 {
                    rgb.extend_from_slice(&[(x * 16) as u8, (y * 16) as u8, (i * 100) as u8]);
                }
            }
            Image::from_rgb8(16, 16, rgb)
                .unwrap()
                .with_delay(Some(Duration::from_millis(100)))
        })
        .collect();
    let gif = dir.join("anim.gif");
    let bytes = oxideav_image::encode_frames(&ctx(), &frames, "gif", &SaveOptions::default())
        .expect("gif animation");
    fs::write(&gif, bytes).unwrap();
    assert_eq!(oxideav_image::open(&ctx(), &gif).unwrap().len(), 3);

    let seq = dir.join("seq.heics");
    convert(&[s(&gif), s(&seq)]).unwrap();
    let streams = oxideav_cli_convert::image_sink::probe_video_streams(s(&seq), &ctx()).unwrap();
    assert!(streams.len() >= 2, "a cover still and a track: {streams:?}");
    assert_eq!(oxideav_cli_convert::image_sink::still_stream(&streams), 0);
    assert_ne!(oxideav_cli_convert::image_sink::fanout_stream(&streams), 0);

    let tmpl = dir.join("frame-%d.png");
    convert(&[s(&seq), s(&tmpl)]).unwrap();
    let written: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("frame-{i}.png"))).collect();
    assert!(written.iter().all(|p| p.exists()), "three frames");
    assert!(!dir.join("frame-3.png").exists());
    let first = oxideav_image::open(&ctx(), &written[0]).unwrap();
    let last = oxideav_image::open(&ctx(), &written[2]).unwrap();
    let (b0, b2) = (
        first.primary().to_rgb8().unwrap()[2],
        last.primary().to_rgb8().unwrap()[2],
    );
    assert!(
        b2 > b0 + 100,
        "the third frame is bluer than the first ({b0} → {b2})"
    );

    let still = dir.join("still.png");
    convert(&[s(&seq), s(&still)]).unwrap();
    let st = oxideav_image::open(&ctx(), &still).unwrap();
    assert_eq!(st.len(), 1);
    assert_eq!((st.primary().width(), st.primary().height()), (16, 16));

    let anim = dir.join("anim2.gif");
    convert(&[s(&seq), s(&anim)]).unwrap();
    let g = oxideav_image::open(&ctx(), &anim).unwrap();
    assert_eq!(g.len(), 3, "the timed output took the sequence track");
}

#[test]
fn convert_quality_matches_the_gateway_quality() {
    // `-quality N` on `.heic` is the gateway's `SaveOptions::quality`
    // byte for byte (same encoder, same option).
    let dir = temp_dir("parity");
    let img = textured(40, 24);
    let png = write_png(&dir, "in.png", &img);
    let via_convert = dir.join("c.heic");
    convert(&[s(&png), "-quality", "70", "-threads", "1", s(&via_convert)]).unwrap();
    let decoded = oxideav_image::open(&ctx(), &png).unwrap().into_primary();
    let via_gateway = oxideav_image::encode(
        &ctx(),
        &decoded,
        "heic",
        &SaveOptions::new()
            .with_quality(70)
            .with_option("threads", "1"),
    )
    .unwrap();
    let c = oxideav_image::open(&ctx(), &via_convert).unwrap();
    let g = oxideav_image::decode_vec(&ctx(), via_gateway).unwrap();
    assert_eq!(c.primary().format(), g.primary().format());
    assert_eq!(
        c.primary().to_rgb8().unwrap(),
        g.primary().to_rgb8().unwrap()
    );
}

/// `.avif` without `-quality` is a lossy photo (the `convert` default,
/// quality 60, the HEIF encoder's own AV1 default) — the `avif`
/// encoder's own default is lossless; `-quality 100` asks for that.
#[test]
fn avif_defaults_to_a_lossy_quality() {
    let dir = temp_dir("avifdefault");
    let png = write_png(&dir, "in.png", &textured(96, 64));
    let plain = dir.join("plain.avif");
    let q60 = dir.join("q60.avif");
    let lossless = dir.join("q100.avif");
    convert(&[s(&png), s(&plain)]).unwrap();
    convert(&[s(&png), "-quality", "60", s(&q60)]).unwrap();
    convert(&[s(&png), "-quality", "100", s(&lossless)]).unwrap();
    assert_eq!(
        fs::read(&plain).unwrap(),
        fs::read(&q60).unwrap(),
        "default = quality 60"
    );
    assert!(
        fs::metadata(&plain).unwrap().len() * 2 < fs::metadata(&lossless).unwrap().len(),
        "the default is far from lossless"
    );
    let src = oxideav_image::open(&ctx(), &png)
        .unwrap()
        .into_primary()
        .to_rgb8()
        .unwrap();
    let back = oxideav_image::open(&ctx(), &lossless)
        .unwrap()
        .into_primary()
        .to_rgb8()
        .unwrap();
    assert_eq!(src, back, "quality 100 is lossless");
    assert_eq!(
        oxideav_cli_convert::image_sink::default_quality("avif"),
        Some(60)
    );
    assert_eq!(
        oxideav_cli_convert::image_sink::default_quality("heif"),
        None
    );
}
