//! Golden plan snapshots: CLI argv in, exact pipeline-job JSON out.
//!
//! These pin the ENTIRE planning path end-to-end — arg parsing, op
//! lowering, codec resolution, JSON serialisation — so any change to
//! the emitted job dialect is a conscious, reviewed diff here rather
//! than a silent drift that downstream job consumers discover later.
//! The output must also be deterministic: same argv → byte-identical
//! JSON, every run (IndexMap preserves output-key order; serde_json
//! orders params keys; f32 values serialise via shortest-decimal).

use oxideav_cli_convert::{args, plan_to_job};
use oxideav_core::RuntimeContext;

/// Context with a few synthetic extension→codec registrations so the
/// snapshots cover the codec-resolution branch without depending on
/// which sibling crates happen to be linked.
fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    ctx.containers.register_extension("jpg", "mjpeg");
    ctx.containers.register_extension("png", "png");
    ctx.containers.register_extension("webp", "webp");
    ctx
}

fn plan_json(argv: &[&str]) -> String {
    let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let plan = args::parse(&args).expect("argv must parse");
    let job = plan_to_job::plan_to_job(&plan, &ctx()).expect("plan must lower");
    job.to_json_pretty()
}

#[track_caller]
fn assert_golden(argv: &[&str], expected: &str) {
    let got = plan_json(argv);
    assert_eq!(
        got.trim(),
        expected.trim(),
        "golden mismatch for {argv:?}\n--- got ---\n{got}\n--- expected ---\n{expected}"
    );
    // Determinism: a second independent parse+lower produces the
    // identical document.
    assert_eq!(got, plan_json(argv), "plan output not deterministic");
}

#[test]
fn golden_bare_image_transcode() {
    assert_golden(
        &["in.png", "out.jpg"],
        r#"
{
  "out.jpg": {
    "all": [
      {
        "from": "in.png",
        "codec": "mjpeg"
      }
    ]
  }
}
"#,
    );
}

#[test]
fn golden_resize_blur_quality_strip() {
    assert_golden(
        &[
            "in.png", "-resize", "800x600", "-blur", "0x2", "-quality", "85", "-strip", "out.jpg",
        ],
        r#"
{
  "out.jpg": {
    "all": [
      {
        "filter": "video.blur",
        "params": {
          "planes": "all",
          "radius": 0,
          "sigma": 2.0
        },
        "input": {
          "filter": "video.resize",
          "params": {
            "height": 600,
            "interpolation": "bilinear",
            "mode": "default",
            "width": 800
          },
          "input": {
            "from": "in.png"
          }
        },
        "codec": "mjpeg",
        "codec_params": {
          "quality": 85,
          "strip_metadata": true
        }
      }
    ]
  }
}
"#,
    );
}

#[test]
fn golden_monochrome_unrolls_to_grayscale_plus_pal8() {
    // `-density` and `-background` are PDF-path settings: recorded,
    // then dropped on the raster pipeline path — the snapshot proves
    // they leave no residue in the job.
    assert_golden(
        &[
            "-density",
            "300",
            "in.png",
            "-background",
            "white",
            "-monochrome",
            "out.png",
        ],
        r#"
{
  "out.png": {
    "all": [
      {
        "filter": "video.pixfmt",
        "params": {
          "colors": 2,
          "dither": "floyd_steinberg",
          "format": "pal8"
        },
        "input": {
          "filter": "video.grayscale",
          "params": {
            "preserve_alpha": true
          },
          "input": {
            "from": "in.png"
          }
        },
        "codec": "png"
      }
    ]
  }
}
"#,
    );
}

#[test]
fn golden_video_container_leaves_codec_to_pipeline() {
    // `.mkv` has no registration in the test ctx → no `codec` key:
    // the pipeline infers per-track (right answer for containers
    // that don't pin a single codec).
    assert_golden(
        &["movie.mp4", "-resize", "640x360", "movie.mkv"],
        r#"
{
  "movie.mkv": {
    "all": [
      {
        "filter": "video.resize",
        "params": {
          "height": 360,
          "interpolation": "bilinear",
          "mode": "default",
          "width": 640
        },
        "input": {
          "from": "movie.mp4"
        }
      }
    ]
  }
}
"#,
    );
}

/// f32 op values must reach the job document as the decimal the user
/// typed, not the f64-widened bit pattern (`2.3f32` used to serialise
/// as `2.299999952316284`).
#[test]
fn golden_float_params_are_noise_free() {
    let json = plan_json(&["in.png", "-blur", "1x2.3", "-gamma", "0.9", "out.png"]);
    assert!(json.contains("\"sigma\": 2.3"), "got:\n{json}");
    assert!(json.contains("\"value\": 0.9"), "got:\n{json}");
    assert!(!json.contains("2.29999"), "f32 widening noise:\n{json}");
    assert!(!json.contains("0.89999"), "f32 widening noise:\n{json}");
}

/// Round CLI hue percentages must produce round degree values: the
/// translate runs in f64 on the noise-free decimal (hue=150 → 90.0).
#[test]
fn golden_modulate_hue_degrees_are_round() {
    let json = plan_json(&["in.png", "-modulate", "100,100,150", "out.png"]);
    assert!(json.contains("\"hue_degrees\": 90.0"), "got:\n{json}");
}

#[cfg(feature = "mesh3d")]
#[test]
fn golden_render3d_full_options() {
    let argv = [
        "scene.gltf",
        "-resize",
        "512x512",
        "-render",
        "phong",
        "-projection",
        "ortho",
        "-fov",
        "45",
        "-aa",
        "2",
        "-bg",
        "#102030",
        "-camera",
        "30,45,1.5",
        "-light",
        "10,20,0.9",
        "-negate",
        "-quality",
        "90",
        "out.png",
    ];
    let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let plan = args::parse(&args).expect("argv must parse");
    let job = plan_to_job::plan_to_render3d_job(&plan, &ctx()).expect("plan must lower");
    let got = job.to_json_pretty();
    let expected = r#"
{
  "out.png": {
    "all": [
      {
        "filter": "video.negate",
        "input": {
          "render3d": "scene.gltf",
          "backend": "scanline",
          "opts": {
            "aa": 2,
            "background": [
              16,
              32,
              48,
              255
            ],
            "camera": {
              "azimuth_deg": 45.0,
              "distance": 1.5,
              "elevation_deg": 30.0
            },
            "fov_deg": 45.0,
            "height": 512,
            "light": {
              "azimuth_deg": 10.0,
              "elevation_deg": 20.0,
              "intensity": 0.9
            },
            "projection": "orthographic",
            "shading": "phong",
            "width": 512
          }
        },
        "codec": "png",
        "codec_params": {
          "quality": 90
        }
      }
    ]
  }
}
"#;
    assert_eq!(
        got.trim(),
        expected.trim(),
        "render3d golden mismatch\n--- got ---\n{got}"
    );
}
