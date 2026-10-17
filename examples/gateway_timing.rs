//! Library-side timings for the README's budget table: the same
//! conversions `oxideav convert` runs, measured in-process three ways —
//! the `oxideav-image` gateway alone (open + encode, an encoder's
//! `threads` option carrying the budget), this crate's `run` (the
//! planner + frame tap, the decoder budgeted through the executor),
//! and whatever the caller times the CLI process at.
//!
//! ```text
//! cargo run --release --example gateway_timing -- in.avif out.heic [threads]
//! ```
//!
//! Formats are registered directly (png, mjpeg, heif, avif, h265): the
//! example runs outside the umbrella, where `oxideav-meta` is not
//! available.

use std::time::Instant;

use oxideav_core::RuntimeContext;
use oxideav_image::{OpenOptions, SaveOptions};

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_source::register(&mut ctx);
    oxideav_png::register(&mut ctx);
    oxideav_mjpeg::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    oxideav_avif::register(&mut ctx);
    oxideav_h265::register(&mut ctx);
    oxideav_image_filter::register(&mut ctx);
    ctx
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, output) = match args.as_slice() {
        [i, o, ..] => (i.clone(), o.clone()),
        _ => {
            eprintln!("usage: gateway_timing IN OUT [THREADS]");
            std::process::exit(2);
        }
    };
    let threads: usize = args
        .get(2)
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| oxideav_core::ExecutionContext::auto().threads);
    let ext = output.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ctx = ctx();

    // 1. The gateway alone.
    let t0 = Instant::now();
    let file = oxideav_image::open_with(&ctx, &input, &OpenOptions::new().with_max_frames(1))
        .expect("gateway open");
    let t_open = t0.elapsed();
    let mut opts = SaveOptions::new();
    if let Some(q) = oxideav_cli_convert::image_sink::default_quality(
        oxideav_image::resolve_target(&ctx, &ext, &SaveOptions::default())
            .map(|t| t.codec().to_string())
            .unwrap_or_default()
            .as_str(),
    ) {
        opts = opts.with_quality(q);
    }
    if let Ok(schema) = oxideav_image::encoder_options(&ctx, &ext, &opts) {
        if schema.iter().any(|f| f.name == "threads") {
            opts = opts.with_option("threads", threads.to_string());
        }
    }
    let t1 = Instant::now();
    let bytes = oxideav_image::encode(&ctx, file.primary(), &ext, &opts).expect("gateway encode");
    let t_encode = t1.elapsed();
    let gateway_out = format!("{output}.gateway.{ext}");
    std::fs::write(&gateway_out, &bytes).expect("write");
    println!(
        "gateway   open {:>7.3} s  encode {:>7.3} s  total {:>7.3} s  ({} bytes, {} threads to the encoder)",
        t_open.as_secs_f64(),
        t_encode.as_secs_f64(),
        t0.elapsed().as_secs_f64(),
        bytes.len(),
        threads
    );

    // 2. This crate's `run` (planner + executor decode + frame tap).
    let t2 = Instant::now();
    let argv = vec![
        input.clone(),
        "-threads".to_string(),
        threads.to_string(),
        output.clone(),
    ];
    oxideav_cli_convert::run(&argv, &ctx).expect("convert run");
    let t_run = t2.elapsed();
    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    println!(
        "convert   run  {:>7.3} s  ({size} bytes, budget {threads})",
        t_run.as_secs_f64()
    );
}
