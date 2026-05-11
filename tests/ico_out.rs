//! End-to-end: generate a PNG source via `oxideav-png`'s encoder, then
//! run `convert` to ICO with and without `-define icon:auto-resize`.
//! Verifies the ICO directory structure (magic, count, per-entry width
//! / height / size / offset) so we don't have to shell out to `file(1)`.

#![cfg(feature = "ico")]

use std::fs;
use std::path::PathBuf;

use oxideav_png::{encode_png_image, PngImage, PngPixelFormat};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-test-{name}-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

fn ctx() -> oxideav_core::RuntimeContext {
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_source::register(&mut ctx);
    ctx
}

/// Write a solid-red PNG at `dims × dims` into `path`. Matches what
/// `oxideav convert "xc:red" -resize 256x256 src.png` would produce
/// (modulo a different filter chain), avoiding the round-trip
/// dependency on the generator.
fn write_solid_red_png(path: &PathBuf, dims: u32) {
    let n = (dims * dims) as usize;
    let mut data = Vec::with_capacity(n * 4);
    for _ in 0..n {
        data.extend_from_slice(&[255, 0, 0, 255]);
    }
    let img = PngImage {
        width: dims,
        height: dims,
        pixel_format: PngPixelFormat::Rgba,
        stride: (dims as usize) * 4,
        data,
        palette: Vec::new(),
    };
    let bytes = encode_png_image(&img).expect("encode source PNG");
    fs::write(path, bytes).unwrap();
}

/// Parse an ICO file's directory and return `(icon_type, [(w, h, size,
/// offset, is_png)…])` where `w = 0` decodes to `256` per the ICO
/// directory convention.
fn parse_ico_dir(bytes: &[u8]) -> (u16, Vec<(u32, u32, u32, u32, bool)>) {
    assert!(
        bytes.len() >= 6,
        "ICO too small for header ({}B)",
        bytes.len()
    );
    assert_eq!(&bytes[0..2], &[0, 0], "ICO reserved bytes must be 0");
    let ty = u16::from_le_bytes([bytes[2], bytes[3]]);
    let n = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let base = 6 + i * 16;
        let w = if bytes[base] == 0 {
            256
        } else {
            bytes[base] as u32
        };
        let h = if bytes[base + 1] == 0 {
            256
        } else {
            bytes[base + 1] as u32
        };
        let size = u32::from_le_bytes([
            bytes[base + 8],
            bytes[base + 9],
            bytes[base + 10],
            bytes[base + 11],
        ]);
        let offset = u32::from_le_bytes([
            bytes[base + 12],
            bytes[base + 13],
            bytes[base + 14],
            bytes[base + 15],
        ]);
        let payload = &bytes[offset as usize..offset as usize + size as usize];
        let is_png = payload.starts_with(b"\x89PNG\r\n\x1a\n");
        out.push((w, h, size, offset, is_png));
    }
    (ty, out)
}

#[test]
fn ico_single_entry_at_source_dims() {
    let dir = temp_dir("ico-single");
    let src = dir.join("src.png");
    write_solid_red_png(&src, 256);

    let out = dir.join("out.ico");
    oxideav_cli_convert::run(
        &[
            src.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert run");

    let bytes = fs::read(&out).expect("ico file");
    let (ty, dirs) = parse_ico_dir(&bytes);
    assert_eq!(ty, 1, "expected ICO (idType=1), got {ty}");
    assert_eq!(dirs.len(), 1, "expected 1 entry, got {}", dirs.len());
    let (w, h, _size, _off, _is_png) = dirs[0];
    assert_eq!((w, h), (256, 256), "entry dims");
}

#[test]
fn ico_multi_entry_auto_resize_four_sizes() {
    let dir = temp_dir("ico-multi-four");
    let src = dir.join("src.png");
    write_solid_red_png(&src, 256);

    let out = dir.join("multi.ico");
    oxideav_cli_convert::run(
        &[
            src.to_string_lossy().into_owned(),
            "-define".into(),
            "icon:auto-resize=16,32,48,64".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert run");

    let bytes = fs::read(&out).expect("ico file");
    let (ty, dirs) = parse_ico_dir(&bytes);
    assert_eq!(ty, 1);
    assert_eq!(dirs.len(), 4, "expected 4 entries, got {}", dirs.len());
    let got_dims: Vec<(u32, u32)> = dirs.iter().map(|d| (d.0, d.1)).collect();
    assert_eq!(got_dims, vec![(16, 16), (32, 32), (48, 48), (64, 64)]);
    // 64 hits the WriteOptions PNG threshold, 16/32/48 stay BMP.
    let is_png: Vec<bool> = dirs.iter().map(|d| d.4).collect();
    assert_eq!(is_png, vec![false, false, false, true]);
}

#[test]
fn ico_multi_entry_auto_resize_six_sizes() {
    let dir = temp_dir("ico-multi-six");
    let src = dir.join("src.png");
    write_solid_red_png(&src, 256);

    let out = dir.join("six.ico");
    oxideav_cli_convert::run(
        &[
            src.to_string_lossy().into_owned(),
            "-define".into(),
            "icon:auto-resize=16,32,48,64,128,256".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert run");

    let bytes = fs::read(&out).expect("ico file");
    let (_ty, dirs) = parse_ico_dir(&bytes);
    assert_eq!(dirs.len(), 6);
    let got_dims: Vec<(u32, u32)> = dirs.iter().map(|d| (d.0, d.1)).collect();
    assert_eq!(
        got_dims,
        vec![
            (16, 16),
            (32, 32),
            (48, 48),
            (64, 64),
            (128, 128),
            (256, 256)
        ]
    );
}

#[test]
fn ico_rejects_oversize_request() {
    let dir = temp_dir("ico-oversize");
    let src = dir.join("src.png");
    write_solid_red_png(&src, 64);

    let out = dir.join("bad.ico");
    let err = oxideav_cli_convert::run(
        &[
            src.to_string_lossy().into_owned(),
            "-define".into(),
            "icon:auto-resize=16,512".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("256") || msg.contains("range"), "got: {msg}");
}

#[test]
fn ico_rejects_source_larger_than_256_without_resize() {
    let dir = temp_dir("ico-source-oversize");
    let src = dir.join("src.png");
    write_solid_red_png(&src, 512);

    let out = dir.join("bad.ico");
    let err = oxideav_cli_convert::run(
        &[
            src.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("256") || msg.contains("range"), "got: {msg}");
}

#[test]
fn ico_dedup_and_sort() {
    // 32 listed twice + out of order; the runner sorts + dedups.
    let dir = temp_dir("ico-dedup");
    let src = dir.join("src.png");
    write_solid_red_png(&src, 128);

    let out = dir.join("dedup.ico");
    oxideav_cli_convert::run(
        &[
            src.to_string_lossy().into_owned(),
            "-define".into(),
            "icon:auto-resize=64,16,32,32".into(),
            out.to_string_lossy().into_owned(),
        ],
        &ctx(),
    )
    .expect("convert run");

    let bytes = fs::read(&out).expect("ico file");
    let (_ty, dirs) = parse_ico_dir(&bytes);
    let got_dims: Vec<(u32, u32)> = dirs.iter().map(|d| (d.0, d.1)).collect();
    assert_eq!(got_dims, vec![(16, 16), (32, 32), (64, 64)]);
}
