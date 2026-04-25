//! IM-style arg parser.
//!
//! `convert INPUT [-op VALUE]... OUTPUT` — first positional arg is
//! the input, last positional arg is the output, everything in
//! between is `-flag` pairs.  Most single-word ops take exactly one
//! following value; a few (`-strip`) are valueless.  Any unrecognised
//! flag errors out with a clear message — we never silently drop.

use crate::op::{ConvertPlan, Dither, Op};
use oxideav_core::Error;

/// Parse the slice that comes after `oxideav convert`.
///
/// Returns a `ConvertPlan` on success or an `Error::Invalid` /
/// `Error::Unsupported` tagged with the specific `-flag` so callers
/// can print the offending argument.
pub fn parse(args: &[String]) -> Result<ConvertPlan, Error> {
    if args.is_empty() {
        return Err(Error::invalid(
            "convert: no input file given (usage: convert INPUT [-op VALUE]... OUTPUT)",
        ));
    }
    if args.len() < 2 {
        return Err(Error::invalid(
            "convert: no output file given (usage: convert INPUT [-op VALUE]... OUTPUT)",
        ));
    }

    // First positional is input; last positional is output; the
    // middle is a stream of -flag [value] pairs.
    let input = args[0].clone();
    let output = args[args.len() - 1].clone();
    if output.starts_with('-') {
        return Err(Error::invalid(format!(
            "convert: output '{output}' looks like a flag — outputs must be the last positional argument",
        )));
    }

    let mut ops: Vec<Op> = Vec::new();
    // Dither state carries forward until the next -colors; it can
    // precede or follow -colors in the arg list, but the pairing
    // rule is "the most recent dither before a -colors".
    let mut pending_dither = Dither::None;

    let middle = &args[1..args.len() - 1];
    let mut i = 0;
    while i < middle.len() {
        let flag = &middle[i];
        let val = |k: usize| -> Result<&str, Error> {
            middle
                .get(k)
                .map(|s| s.as_str())
                .ok_or_else(|| Error::invalid(format!("convert: {flag}: missing value")))
        };

        match flag.as_str() {
            "-resize" => {
                let v = val(i + 1)?;
                let (bang, core) = match v.strip_suffix('!') {
                    Some(c) => (true, c),
                    None => (false, v),
                };
                let (w, h) = parse_wxh(core)
                    .map_err(|e| Error::invalid(format!("convert: -resize: {e}")))?;
                ops.push(Op::Resize {
                    width: w,
                    height: h,
                    bang,
                });
                i += 2;
            }
            "-blur" => {
                let v = val(i + 1)?;
                let (radius, sigma) =
                    parse_blur(v).map_err(|e| Error::invalid(format!("convert: -blur: {e}")))?;
                ops.push(Op::Blur { radius, sigma });
                i += 2;
            }
            "-edge" => {
                let v = val(i + 1)?;
                let r: u32 = v.parse().map_err(|_| {
                    Error::invalid(format!(
                        "convert: -edge: '{v}' is not a non-negative integer"
                    ))
                })?;
                ops.push(Op::Edge { radius: r });
                i += 2;
            }
            "-colors" => {
                let v = val(i + 1)?;
                let n: u32 = v.parse().map_err(|_| {
                    Error::invalid(format!(
                        "convert: -colors: '{v}' is not a non-negative integer"
                    ))
                })?;
                if !(2..=256).contains(&n) {
                    return Err(Error::invalid(format!(
                        "convert: -colors {n} out of range (2..=256)"
                    )));
                }
                ops.push(Op::Colors {
                    count: n,
                    dither: pending_dither,
                });
                i += 2;
            }
            "-dither" => {
                let v = val(i + 1)?;
                pending_dither = Dither::parse(v).map_err(Error::invalid)?;
                i += 2;
            }
            "-format" => {
                let v = val(i + 1)?;
                ops.push(Op::Format(v.to_string()));
                i += 2;
            }
            "-quality" => {
                let v = val(i + 1)?;
                let q: u32 = v.parse().map_err(|_| {
                    Error::invalid(format!(
                        "convert: -quality: '{v}' is not a non-negative integer"
                    ))
                })?;
                ops.push(Op::Quality(q));
                i += 2;
            }
            "-strip" => {
                ops.push(Op::Strip);
                i += 1;
            }
            // Known IM ops we don't yet have a primitive for. Friendly
            // message so users know what's missing, not a generic
            // parse failure.
            "-rotate"
            | "-crop"
            | "-flip"
            | "-flop"
            | "-negate"
            | "-brightness-contrast"
            | "-contrast"
            | "-gamma"
            | "-sepia"
            | "-modulate"
            | "-colorspace"
            | "-level"
            | "-normalize"
            | "-sharpen"
            | "-unsharp"
            | "-threshold" => {
                return Err(Error::unsupported(format!(
                    "convert: {flag} is not yet implemented"
                )));
            }
            other if other.starts_with('-') => {
                return Err(Error::invalid(format!("convert: unknown flag '{other}'")));
            }
            other => {
                return Err(Error::invalid(format!(
                    "convert: unexpected positional argument '{other}' between input and output"
                )));
            }
        }
    }

    Ok(ConvertPlan { input, ops, output })
}

/// Parse `WxH` — width × height. Accepts either lowercase `x` or
/// uppercase `X`. Both parts must be non-negative integers.
fn parse_wxh(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("'{s}' is not in WxH form"))?;
    let w: u32 = w
        .parse()
        .map_err(|_| format!("'{w}' is not a non-negative integer"))?;
    let h: u32 = h
        .parse()
        .map_err(|_| format!("'{h}' is not a non-negative integer"))?;
    if w == 0 || h == 0 {
        return Err("width and height must both be positive".into());
    }
    Ok((w, h))
}

/// Parse `-blur RxS` or `-blur R`. When sigma is omitted we follow
/// IM's convention of `sigma = radius / 2.0`. Unlike IM we don't
/// accept floats for radius — `Blur::new` takes `u32`.
fn parse_blur(s: &str) -> Result<(u32, f32), String> {
    let (radius_str, sigma_str) = match s.split_once(['x', 'X']) {
        Some((r, s)) => (r, Some(s)),
        None => (s, None),
    };
    let radius: u32 = radius_str
        .parse()
        .map_err(|_| format!("'{radius_str}' is not a non-negative integer"))?;
    let sigma: f32 = match sigma_str {
        Some(s) => s
            .parse()
            .map_err(|_| format!("'{s}' is not a non-negative float"))?,
        None => (radius as f32) / 2.0,
    };
    Ok((radius, sigma))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_vec(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn minimal_input_output() {
        let p = parse(&to_vec(&["in.png", "out.jpg"])).unwrap();
        assert_eq!(p.input, "in.png");
        assert_eq!(p.output, "out.jpg");
        assert!(p.ops.is_empty());
    }

    #[test]
    fn resize_bilinear_basic() {
        let p = parse(&to_vec(&["a.png", "-resize", "800x600", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 800,
                height: 600,
                bang: false
            }]
        );
    }

    #[test]
    fn resize_bang_flag() {
        let p = parse(&to_vec(&["a.png", "-resize", "64x32!", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 64,
                height: 32,
                bang: true
            }]
        );
    }

    #[test]
    fn blur_sigma_defaults_to_half_radius() {
        let p = parse(&to_vec(&["a.png", "-blur", "4", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Blur {
                radius: 4,
                sigma: 2.0
            }]
        );
    }

    #[test]
    fn blur_explicit_sigma() {
        let p = parse(&to_vec(&["a.png", "-blur", "3x1.5", "b.jpg"])).unwrap();
        match p.ops.as_slice() {
            [Op::Blur { radius: 3, sigma }] => assert!((sigma - 1.5).abs() < 1e-6),
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colors_picks_up_prior_dither() {
        let p = parse(&to_vec(&[
            "a.png",
            "-dither",
            "floyd_steinberg",
            "-colors",
            "64",
            "b.gif",
        ]))
        .unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Colors {
                count: 64,
                dither: Dither::FloydSteinberg
            }]
        );
    }

    #[test]
    fn rotate_is_unsupported() {
        let err = parse(&to_vec(&["a.png", "-rotate", "90", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("-rotate is not yet implemented"));
    }

    #[test]
    fn unknown_flag_errors() {
        let err = parse(&to_vec(&["a.png", "-fnord", "42", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("unknown flag"));
    }

    #[test]
    fn missing_output_errors() {
        let err = parse(&to_vec(&["in.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("no output file given"));
    }

    #[test]
    fn quality_strip() {
        let p = parse(&to_vec(&["a.png", "-quality", "85", "-strip", "b.jpg"])).unwrap();
        assert_eq!(p.ops, vec![Op::Quality(85), Op::Strip]);
    }
}
