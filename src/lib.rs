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
pub mod op;
pub mod pdf_runner;
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

    // Side-channel: PDF inputs go through a Scene-aware runner that
    // bypasses the regular FrameSource pipeline. PDF pages don't fit
    // the `Frame::Video` shape `oxideav-pipeline` expects, and the
    // routing rule (encoder accepts Scene → pass through; otherwise
    // fan out per page when the filename has a `%d` template) is
    // specific to `convert`. See `pdf_runner` module docs.
    if pdf_runner::is_pdf_input(&plan.input) {
        return pdf_runner::run(&plan);
    }

    let job = plan_to_job::plan_to_job(&plan)?;
    let stats = oxideav_pipeline::Executor::new(&job, ctx).run()?;
    eprintln!(
        "convert: {} packet(s) read, {} frame(s) decoded, {} frame(s) written",
        stats.packets_read, stats.frames_decoded, stats.frames_written
    );
    Ok(())
}
