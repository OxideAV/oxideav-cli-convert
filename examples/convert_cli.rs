//! `oxideav convert` as a standalone process for the README's budget
//! table: the formats the integration tests register (png, bmp, jpeg,
//! webp, gif, tiff, qoi, heif, avif, h265) plus the image filters, then
//! this crate's [`run`](oxideav_cli_convert::run) over the arguments
//! verbatim — so `/usr/bin/time -l` measures wall and peak RSS of the
//! planner + executor + frame tap alone, outside the umbrella.
//!
//! ```text
//! cargo build --release --example convert_cli
//! /usr/bin/time -l target/release/examples/convert_cli in.heic -threads 4 out.png
//! ```
//!
//! Exit status follows the umbrella CLI's table for the error classes
//! this crate raises (3 invalid data, 4 unsupported, 5 format not
//! found, 6 codec not found, 7 resource exhausted, 8 input not found,
//! 9 permission denied, 10 other I/O, 1 anything else).

use oxideav_core::{Error, RuntimeContext};

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
    oxideav_heif::register(&mut ctx);
    oxideav_avif::register(&mut ctx);
    oxideav_h265::register(&mut ctx);
    oxideav_image_filter::register(&mut ctx);
    ctx
}

fn exit_code(e: &Error) -> u8 {
    use std::io::ErrorKind;
    match e {
        Error::InvalidData(_) => 3,
        Error::Unsupported(_) => 4,
        Error::FormatNotFound(_) => 5,
        Error::CodecNotFound(_) => 6,
        Error::ResourceExhausted(_) => 7,
        Error::Io(io) => match io.kind() {
            ErrorKind::NotFound => 8,
            ErrorKind::PermissionDenied => 9,
            _ => 10,
        },
        _ => 1,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = oxideav_cli_convert::run(&args, &ctx()) {
        eprintln!("{e}");
        std::process::exit(i32::from(exit_code(&e)));
    }
}
