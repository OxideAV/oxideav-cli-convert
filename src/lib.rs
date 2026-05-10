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
#[cfg(feature = "mesh3d")]
pub mod mesh3d_runner;
pub mod op;
pub mod pdf_runner;
pub mod ping;
pub mod pixel_xform;
pub mod plan_to_job;

pub use op::{AlphaOp, ConvertPlan, Dither, Op, PrintfTemplate};

use oxideav_core::{Error, RuntimeContext};

/// Run convert with a caller-supplied [`RuntimeContext`].
///
/// The CLI passes the same context produced by `oxideav::with_all_features()`
/// it uses for `remux` / `transcode` / `run`; third-party embedders can
/// pass a narrower set.
pub fn run(args: &[String], ctx: &RuntimeContext) -> Result<(), Error> {
    let plan = args::parse(args)?;

    // `-ping` short-circuits everything: print one IM-format header
    // line per "image" (page / video stream) to stdout and exit
    // without any pixel decode or output write. Routed BEFORE the PDF
    // side-channel so the ping module owns the PDF-vs-container split
    // for itself.
    if plan.ping {
        return ping::run(&plan, ctx);
    }

    // Side-channel: PDF inputs go through a Scene-aware runner that
    // bypasses the regular FrameSource pipeline. PDF pages don't fit
    // the `Frame::Video` shape `oxideav-pipeline` expects, and the
    // routing rule (encoder accepts Scene → pass through; otherwise
    // fan out per page when the filename has a `%d` template) is
    // specific to `convert`. See `pdf_runner` module docs.
    if pdf_runner::is_pdf_input(&plan.input) {
        return pdf_runner::run(&plan);
    }

    // Side-channel: 3D-asset inputs (STL/OBJ/glTF/GLB/USDZ/MTL) go
    // through a Mesh3DRegistry-driven decode→encode bridge. 3D scenes
    // don't fit any of the codec/container shapes the regular pipeline
    // walks, and the dispatch contract is a separate registry from
    // `RuntimeContext`'s codec/container/filter/source path. See
    // `mesh3d_runner` module docs.
    #[cfg(feature = "mesh3d")]
    if mesh3d_runner::is_mesh3d_input(&plan.input) {
        if !mesh3d_runner::is_mesh3d_output(&plan.output) {
            return Err(Error::invalid(format!(
                "convert: 3D input '{}' must pair with a 3D output extension (.stl/.obj/.gltf/.glb/.mtl); got '{}'. 3D→raster rendering is a separate follow-up.",
                plan.input, plan.output
            )));
        }
        if plan.output_template.is_some() {
            return Err(Error::invalid(format!(
                "convert: output '{}' has a `%d` template but 3D scenes are single-document; remove the template",
                plan.output
            )));
        }
        if !plan.ops.is_empty() {
            eprintln!(
                "convert: note: raster ops ignored on 3D-asset conversion (stl/obj/gltf/glb/mtl have no pixel grid)"
            );
        }
        return mesh3d_runner::run(&plan.input, &plan.output);
    }

    let job = plan_to_job::plan_to_job(&plan, ctx)?;
    let stats = oxideav_pipeline::Executor::new(&job, ctx).run()?;
    eprintln!(
        "convert: {} packet(s) read, {} frame(s) decoded, {} frame(s) written",
        stats.packets_read, stats.frames_decoded, stats.frames_written
    );
    Ok(())
}
