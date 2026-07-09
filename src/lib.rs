//! `oxideav convert` engine.
//!
//! Translates ImageMagick-style arg syntax into an in-memory
//! [`oxideav_pipeline::Job`] and runs it through the same executor
//! the `oxideav run` subcommand uses. One code path serves image,
//! video, and audio inputs — a PNG → JPG is just a 1-frame stream.
//!
//! Entry point: [`run`] takes the slice of arguments that come after
//! `oxideav convert` plus the caller-supplied registries, and does
//! everything.
//!
//! # Example
//!
//! ```no_run
//! use oxideav_core::RuntimeContext;
//!
//! let mut ctx = RuntimeContext::new();
//! oxideav_source::register(&mut ctx);
//!
//! // Equivalent to:
//! //   oxideav convert in.png -resize 800x600 out.jpg
//! oxideav_cli_convert::run(
//!     &[
//!         "in.png".into(),
//!         "-resize".into(),
//!         "800x600".into(),
//!         "out.jpg".into(),
//!     ],
//!     &ctx,
//! ).unwrap();
//! ```

pub mod args;
#[cfg(feature = "ico")]
pub mod ico_runner;
#[cfg(feature = "mesh3d")]
pub mod mesh3d_render;
#[cfg(feature = "mesh3d")]
pub mod mesh3d_runner;
pub mod op;
pub mod pdf_runner;
pub mod ping;
pub mod pixel_xform;
pub mod plan_to_job;
pub mod probe;
pub mod raster_io;
pub mod route;
pub mod suggest;

pub use op::{AlphaOp, ConvertPlan, Dither, Op, PrintfTemplate, ResizeMode};
pub use route::Route;

use oxideav_core::{Error, RuntimeContext};

/// Run convert with a caller-supplied [`RuntimeContext`].
///
/// The CLI passes the same context produced by `oxideav::with_all_features()`
/// it uses for `remux` / `transcode` / `run`; third-party embedders can
/// pass a narrower set.
pub fn run(args: &[String], ctx: &RuntimeContext) -> Result<(), Error> {
    let plan = args::parse(args)?;

    // All routing decisions live in `route::decide` (pure, matrix-
    // tested); this function only performs the dispatched work. See
    // the `route` module docs for the precedence order and why input
    // classification outranks output classification.
    match route::decide(&plan)? {
        // `--help`: print the usage synopsis (generated from the
        // parser's own flag table) and do nothing else.
        Route::Help => {
            print!("{}", args::usage());
            Ok(())
        }

        // `-ping`: one IM-format header line per "image" (page /
        // video stream) to stdout, no pixel decode, no output write.
        // The ping module owns the PDF-vs-container split for itself.
        Route::Ping => ping::run(&plan, ctx),

        // `--probe`: decode the input far enough to extract metadata
        // (page count, mesh count, sample rate, …), print a summary
        // to stdout, and skip any output write. The args parser
        // already enforced the "no output positional" rule, so the
        // probe module only deals with one shape (input-only).
        Route::Probe => probe::run(&plan, ctx),

        // PDF inputs go through a Scene-aware runner that bypasses
        // the regular FrameSource pipeline. PDF pages don't fit the
        // `Frame::Video` shape `oxideav-pipeline` expects, and the
        // routing rule (encoder accepts Scene → pass through;
        // otherwise fan out per page when the filename has a `%d`
        // template) is specific to `convert`. See `pdf_runner`.
        Route::PdfSideChannel => pdf_runner::run(&plan),

        // 3D-asset same-class round-trip: decode → re-encode through
        // the `Mesh3DRegistry`. 3D scenes don't fit any of the
        // codec/container shapes the regular pipeline walks. See
        // `mesh3d_runner` module docs.
        #[cfg(feature = "mesh3d")]
        Route::Mesh3dToMesh3d => {
            if !plan.ops.is_empty() {
                eprintln!(
                    "convert: note: raster ops ignored on 3D-asset conversion (stl/obj/gltf/glb/mtl/usdz have no pixel grid)"
                );
            }
            mesh3d_runner::run(&plan.input, &plan.output, &plan.mesh3d_options)
        }

        // 3D-asset input paired with a raster output: software-render
        // the scene to RGBA, then apply raster ops and encode. See
        // `mesh3d_render` module docs.
        #[cfg(feature = "mesh3d")]
        Route::Mesh3dToRaster => {
            mesh3d_render::run(&plan.input, &plan.output, &plan.ops, &plan.mesh3d_options)
        }

        // `.ico` output: the ICO container carries one OR more
        // sub-images at different resolutions, driven on the IM CLI
        // by `-define icon:auto-resize=W1,W2,…`. The regular pipeline
        // path's "one Frame::Video per track" shape doesn't fit that
        // multi-image fan-out. See `ico_runner` module docs.
        #[cfg(feature = "ico")]
        Route::IcoOutput => ico_runner::run(&plan),

        Route::Pipeline => {
            let job = plan_to_job::plan_to_job(&plan, ctx)?;
            let stats = oxideav_pipeline::Executor::new(&job, ctx).run()?;
            eprintln!(
                "convert: {} packet(s) read, {} frame(s) decoded, {} frame(s) written",
                stats.packets_read, stats.frames_decoded, stats.frames_written
            );
            Ok(())
        }
    }
}
