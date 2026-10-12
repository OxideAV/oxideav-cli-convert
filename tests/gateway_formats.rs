//! The side-channels' still-image I/O rides the `oxideav-image`
//! gateway: `-quality`, `-depth` and `--opt` reach the encoder through
//! its schema, the encoder input layout follows the pipeline route's
//! pick (and ImageMagick's 4:2:0 for JPEG), and every format the
//! registry encodes is an output.

use std::fs;
use std::path::{Path, PathBuf};

use oxideav_core::vector::{
    FillRule, Group, Node, Paint, Path as VPath, PathCommand, PathNode, Point, Rgba, VectorFrame,
};
use oxideav_core::{PixelFormat, RuntimeContext};
use oxideav_pdf::write_pdf_from_scene;
use oxideav_scene::{Page, Scene};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxideav-cli-convert-gateway-{name}-{}-{}",
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
    ctx
}

/// A one-page PDF: a 200×100 pt page with a red rectangle over the left
/// half and a blue one over the right, on a white background.
fn make_pdf() -> Vec<u8> {
    let rect = |x0: f32, x1: f32, color: Rgba| {
        Node::Path(PathNode {
            path: VPath {
                commands: vec![
                    PathCommand::MoveTo(Point::new(x0, 0.0)),
                    PathCommand::LineTo(Point::new(x1, 0.0)),
                    PathCommand::LineTo(Point::new(x1, 100.0)),
                    PathCommand::LineTo(Point::new(x0, 100.0)),
                    PathCommand::Close,
                ],
            },
            fill: Some(Paint::Solid(color)),
            stroke: None,
            fill_rule: FillRule::NonZero,
        })
    };
    let mut content = VectorFrame::new(200.0, 100.0);
    content.root = Group {
        children: vec![
            rect(0.0, 100.0, Rgba::new(255, 0, 0, 255)),
            rect(100.0, 200.0, Rgba::new(0, 0, 255, 255)),
        ],
        ..Group::default()
    };
    let scene = Scene {
        pages: Some(vec![Page {
            width: 200.0,
            height: 100.0,
            content,
            label: None,
            orientation: 0,
        }]),
        ..Scene::default()
    };
    write_pdf_from_scene(&scene).expect("pdf")
}

fn run(args: &[String]) -> Result<(), oxideav_core::Error> {
    oxideav_cli_convert::run(args, &ctx())
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn open(p: &Path) -> oxideav_image::Image {
    oxideav_image::open(&ctx(), p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .into_primary()
}

/// Pixel `(x, y)` of a picture as RGB.
fn rgb_at(img: &oxideav_image::Image, x: u32, y: u32) -> [u8; 3] {
    let rgb = img.to_rgb8().unwrap();
    let i = ((y * img.width() + x) * 3) as usize;
    [rgb[i], rgb[i + 1], rgb[i + 2]]
}

fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
    a.iter()
        .zip(b)
        .all(|(&x, y)| (x as i32 - y as i32).abs() <= tol)
}

/// PDF → every format the test registry encodes; each decodes back to
/// the PNG render (JPEG within its quantisation error, and in colour:
/// an RGB render must never come out as a grayscale JPEG).
#[test]
fn pdf_page_round_trips_through_every_registered_format() {
    let dir = temp_dir("roundtrip");
    let pdf = dir.join("in.pdf");
    fs::write(&pdf, make_pdf()).unwrap();
    let render = |ext: &str| {
        let out = dir.join(format!("page.{ext}"));
        run(&[
            s(&pdf),
            "-alpha".into(),
            "remove".into(),
            "-alpha".into(),
            "off".into(),
            s(&out),
        ])
        .unwrap_or_else(|e| panic!("pdf → {ext}: {e}"));
        open(&out)
    };
    let png = render("png");
    assert_eq!((png.width(), png.height()), (200, 100));
    assert_eq!(png.format(), PixelFormat::Rgb24, "RGB render stays RGB");
    assert_eq!(rgb_at(&png, 50, 50), [255, 0, 0]);
    assert_eq!(rgb_at(&png, 150, 50), [0, 0, 255]);
    let reference = png.to_rgb8().unwrap();
    for ext in ["bmp", "webp", "gif", "tif", "qoi"] {
        let img = render(ext);
        assert_eq!((img.width(), img.height()), (200, 100), "{ext}");
        assert_eq!(img.to_rgb8().unwrap(), reference, "{ext} is lossless here");
    }
    let jpg = render("jpg");
    assert_eq!((jpg.width(), jpg.height()), (200, 100));
    assert!(
        matches!(jpg.format(), PixelFormat::Yuv420P | PixelFormat::YuvJ420P),
        "an RGB render is a 4:2:0 JPEG (ImageMagick's default), got {:?}",
        jpg.format()
    );
    // Saturated primaries through 4:2:0 at the encoder's default
    // quality (75) land within a couple of dozen levels; grayscale
    // would be ~ (76, 76, 76) and (29, 29, 29).
    assert!(
        close(rgb_at(&jpg, 50, 50), [255, 0, 0], 24),
        "{:?}",
        rgb_at(&jpg, 50, 50)
    );
    assert!(
        close(rgb_at(&jpg, 150, 50), [0, 0, 255], 24),
        "{:?}",
        rgb_at(&jpg, 150, 50)
    );
}

/// `-quality` reaches the JPEG encoder through the gateway's
/// schema-gated option: a lower quality is a smaller file, and the
/// option is silently dropped for encoders without the knob (PNG).
#[test]
fn quality_reaches_the_encoder_on_the_side_channel() {
    let dir = temp_dir("quality");
    let pdf = dir.join("in.pdf");
    fs::write(&pdf, make_pdf()).unwrap();
    let sizes: Vec<u64> = [30u32, 75, 95]
        .iter()
        .map(|q| {
            let out = dir.join(format!("q{q}.jpg"));
            run(&[s(&pdf), "-quality".into(), q.to_string(), s(&out)]).expect("pdf → jpg");
            fs::metadata(&out).unwrap().len()
        })
        .collect();
    assert!(
        sizes[0] < sizes[1] && sizes[1] < sizes[2],
        "sizes {sizes:?}"
    );
    // Without `-quality` the encoder's declared default (75) applies:
    // the same quantisation, so the same samples as an explicit 75 (the
    // encoder orders its header segments differently when the option
    // is set, so the files are compared decoded).
    let plain = dir.join("plain.jpg");
    run(&[s(&pdf), s(&plain)]).unwrap();
    assert_eq!(
        open(&plain).to_rgb8().unwrap(),
        open(&dir.join("q75.jpg")).to_rgb8().unwrap(),
        "no -quality == the encoder's declared default (75)"
    );
    let png = dir.join("q30.png");
    run(&[s(&pdf), "-quality".into(), "30".into(), s(&png)]).expect("quality is ignored by png");
    assert_eq!(rgb_at(&open(&png), 50, 50), [255, 0, 0]);
}

/// `--opt` reaches the encoder on the side-channel like on the pipeline
/// route: an interlaced PNG carries IHDR interlace method 1; an unknown
/// key is the planner's typed error and nothing is written.
#[test]
fn opt_reaches_the_encoder_on_the_side_channel() {
    let dir = temp_dir("opt");
    let pdf = dir.join("in.pdf");
    fs::write(&pdf, make_pdf()).unwrap();
    let out = dir.join("interlaced.png");
    run(&[s(&pdf), "--opt".into(), "interlace=true".into(), s(&out)]).unwrap();
    let bytes = fs::read(&out).unwrap();
    assert_eq!(&bytes[12..16], b"IHDR");
    assert_eq!(bytes[28], 1, "Adam7 interlace");
    assert_eq!(rgb_at(&open(&out), 150, 50), [0, 0, 255]);

    let bad = dir.join("bad.png");
    let err = run(&[s(&pdf), "--opt".into(), "bogus=1".into(), s(&bad)]).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("has no option 'bogus'"), "{msg}");
    assert!(!bad.exists());
}

/// `-depth 16` on a side-channel PNG widens the layout like the
/// pipeline route does; `-alpha off` first makes it RGB.
#[test]
fn depth_sixteen_widens_the_side_channel_png() {
    let dir = temp_dir("depth");
    let pdf = dir.join("in.pdf");
    fs::write(&pdf, make_pdf()).unwrap();
    let out = dir.join("deep.png");
    run(&[
        s(&pdf),
        "-alpha".into(),
        "off".into(),
        "-depth".into(),
        "16".into(),
        s(&out),
    ])
    .unwrap();
    let img = open(&out);
    assert_eq!(img.format(), PixelFormat::Rgb48Le);
    assert_eq!(rgb_at(&img, 50, 50), [255, 0, 0]);

    let rgba = dir.join("rgba.png");
    run(&[s(&pdf), s(&rgba)]).unwrap();
    assert_eq!(
        open(&rgba).format(),
        PixelFormat::Rgba,
        "the render carries alpha"
    );
}

/// The side-channel's JPEG and the pipeline route's JPEG of the same
/// picture decode to the same colours (both go through the registry's
/// mjpeg encoder); the two routes are allowed to differ in sampling.
#[test]
fn side_channel_and_pipeline_jpegs_agree_on_colour() {
    let dir = temp_dir("agree");
    let pdf = dir.join("in.pdf");
    fs::write(&pdf, make_pdf()).unwrap();
    let png = dir.join("page.png");
    run(&[s(&pdf), "-alpha".into(), "off".into(), s(&png)]).unwrap();
    let side = dir.join("side.jpg");
    run(&[s(&pdf), "-alpha".into(), "off".into(), s(&side)]).unwrap();
    let pipe = dir.join("pipe.jpg");
    run(&[s(&png), s(&pipe)]).unwrap();
    let (a, b) = (open(&side), open(&pipe));
    for (x, y) in [(50, 50), (150, 50), (10, 10), (190, 90)] {
        assert!(close(rgb_at(&a, x, y), rgb_at(&b, x, y), 16), "({x},{y})");
    }
}
