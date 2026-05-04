//! IM-style arg parser.
//!
//! `convert INPUT [-op VALUE]... OUTPUT` — first positional arg is
//! the input, last positional arg is the output, everything in
//! between is `-flag` pairs.  Most single-word ops take exactly one
//! following value; a few (`-strip`) are valueless.  Any unrecognised
//! flag errors out with a clear message — we never silently drop.

use crate::op::{AlphaOp, ConvertPlan, Dither, Op, PageSelector, PrintfTemplate};
use oxideav_core::Error;

/// Parse the slice that comes after `oxideav convert`.
///
/// Returns a `ConvertPlan` on success or an `Error::Invalid` /
/// `Error::Unsupported` tagged with the specific `-flag` so callers
/// can print the offending argument.
pub fn parse(args: &[String]) -> Result<ConvertPlan, Error> {
    if args.is_empty() {
        return Err(Error::invalid(
            "convert: no input file given (usage: convert [-op VALUE]... INPUT [-op VALUE]... OUTPUT)",
        ));
    }

    // ImageMagick allows ops to appear before AND after the input
    // (e.g. `convert -density 300 in.pdf -resize 800x600 out.png`).
    // Walk every arg in order: anything starting with `-` is a flag
    // (which may consume the following arg as its value); everything
    // else is a positional. After parsing, the FIRST positional is the
    // input and the LAST is the output. Multi-input is a documented
    // round-2 follow-up; we error if more than two positionals are
    // present.
    let mut ops: Vec<Op> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut pending_dither = Dither::None;

    let mut i = 0;
    while i < args.len() {
        let flag = &args[i];
        let val = |k: usize| -> Result<&str, Error> {
            args.get(k)
                .map(|s| s.as_str())
                .ok_or_else(|| Error::invalid(format!("convert: {flag}: missing value")))
        };

        // Non-flag → positional.
        if !flag.starts_with('-') {
            positionals.push(flag.clone());
            i += 1;
            continue;
        }

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
            "-density" => {
                let v = val(i + 1)?;
                let n: u32 = v.parse().map_err(|_| {
                    Error::invalid(format!(
                        "convert: -density: '{v}' is not a non-negative integer"
                    ))
                })?;
                if n == 0 {
                    return Err(Error::invalid("convert: -density must be > 0"));
                }
                ops.push(Op::Density(n));
                i += 2;
            }
            "-background" => {
                let v = val(i + 1)?;
                let rgba = parse_color(v).map_err(Error::invalid)?;
                ops.push(Op::Background(rgba));
                i += 2;
            }
            "-alpha" => {
                let v = val(i + 1)?;
                let a = AlphaOp::parse(v).map_err(Error::invalid)?;
                ops.push(Op::Alpha(a));
                i += 2;
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
            other => {
                // Reach here only on `-`-prefixed args (non-`-` was
                // pushed to `positionals` above).
                return Err(Error::invalid(format!("convert: unknown flag '{other}'")));
            }
        }
    }

    if positionals.is_empty() {
        return Err(Error::invalid(
            "convert: no input file given (usage: convert [-op VALUE]... INPUT [-op VALUE]... OUTPUT)",
        ));
    }
    if positionals.len() < 2 {
        return Err(Error::invalid(
            "convert: no output file given (usage: convert [-op VALUE]... INPUT [-op VALUE]... OUTPUT)",
        ));
    }
    if positionals.len() > 2 {
        return Err(Error::unsupported(format!(
            "convert: {} positional arguments given but multi-input is not yet supported (round-2 follow-up); pass exactly INPUT OUTPUT",
            positionals.len()
        )));
    }

    let (raw_input, input_pages) = split_input_selector(&positionals[0])?;
    let input = translate_input_shorthand(raw_input);
    let output = positionals[1].clone();
    let output_template = parse_printf_template(&output)?;

    Ok(ConvertPlan {
        input,
        input_pages,
        ops,
        output,
        output_template,
    })
}

/// Strip an ImageMagick-style `[N]` / `[N-M]` page selector suffix
/// from the input path. `input.pdf[0]` → `("input.pdf", Some(Single(0)))`.
/// Inputs with no `[…]` suffix return `(input, None)`.
///
/// Returns an `Err` for malformed selectors (`[abc]`, `[1-2-3]`, `[]`,
/// unbalanced brackets, etc.) so the user gets a clear message.
pub(crate) fn split_input_selector(s: &str) -> Result<(&str, Option<PageSelector>), Error> {
    if !s.ends_with(']') {
        return Ok((s, None));
    }
    let open = match s.rfind('[') {
        Some(i) => i,
        None => {
            return Err(Error::invalid(format!(
                "convert: input '{s}' has a closing `]` with no matching `[`"
            )));
        }
    };
    let body = &s[open + 1..s.len() - 1];
    if body.is_empty() {
        return Err(Error::invalid(format!(
            "convert: input '{s}' has an empty `[]` page selector"
        )));
    }
    let sel = match body.split_once('-') {
        None => {
            let n: usize = body
                .parse()
                .map_err(|_| Error::invalid(format!("convert: input '{s}': '{body}' is not a non-negative integer page index")))?;
            PageSelector::Single(n)
        }
        Some((a, b)) => {
            if b.contains('-') {
                return Err(Error::invalid(format!(
                    "convert: input '{s}': page selector '{body}' has more than one `-` (expected `[N]` or `[N-M]`)"
                )));
            }
            let a: usize = a.parse().map_err(|_| {
                Error::invalid(format!(
                    "convert: input '{s}': '{a}' in range '{body}' is not a non-negative integer"
                ))
            })?;
            let b: usize = b.parse().map_err(|_| {
                Error::invalid(format!(
                    "convert: input '{s}': '{b}' in range '{body}' is not a non-negative integer"
                ))
            })?;
            PageSelector::Range(a, b)
        }
    };
    Ok((&s[..open], Some(sel)))
}

/// Scan an output filename for a single `%[0-9]*d` token.
///
/// Returns `Ok(Some(template))` when exactly one such token is found;
/// `Ok(None)` when there is no `%` at all (the literal-filename case);
/// `Err` when there are multiple `%d` tokens or any unsupported format
/// specifier (`%s`, `%x`, `%%`, …). The two-pass design keeps the
/// invariant that `output_template == None` iff the filename is a plain
/// path that should be written to verbatim.
pub(crate) fn parse_printf_template(s: &str) -> Result<Option<PrintfTemplate>, Error> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut found: Option<(usize, u8, usize)> = None; // (start, width, end)
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut width: u8 = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            // Cap width to a sensible limit to avoid pathological inputs.
            let d = (bytes[i] - b'0') as u32;
            let next = (width as u32) * 10 + d;
            if next > 20 {
                return Err(Error::invalid(format!(
                    "convert: output template '{s}' has an unreasonable width specifier"
                )));
            }
            width = next as u8;
            i += 1;
        }
        if i >= bytes.len() {
            return Err(Error::invalid(format!(
                "convert: output template '{s}' has a `%` with no format specifier"
            )));
        }
        match bytes[i] {
            b'd' => {
                if found.is_some() {
                    return Err(Error::invalid(format!(
                        "convert: output template '{s}' has more than one `%d` token (expected exactly one)"
                    )));
                }
                found = Some((start, width, i + 1));
                i += 1;
            }
            other => {
                return Err(Error::invalid(format!(
                    "convert: output template '{s}' uses unsupported format specifier '%{}' (only `%d`, `%03d`, etc. are accepted)",
                    other as char
                )));
            }
        }
    }
    Ok(found.map(|(start, width, end)| PrintfTemplate {
        prefix: s[..start].to_string(),
        width,
        suffix: s[end..].to_string(),
    }))
}

/// CSS L3 named colours + `#hex` 3/4/6/8 form. Same grammar as
/// `oxideav-generator`'s `xc:` parser. Kept local to convert so we
/// don't have to pull `oxideav-generator` in when the `generator`
/// feature is off.
fn parse_color(s: &str) -> Result<[u8; 4], String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    match s.to_ascii_lowercase().as_str() {
        "transparent" | "none" => Ok([0, 0, 0, 0]),
        "black" => Ok([0, 0, 0, 255]),
        "white" => Ok([255, 255, 255, 255]),
        "red" => Ok([255, 0, 0, 255]),
        "green" => Ok([0, 128, 0, 255]),
        "lime" => Ok([0, 255, 0, 255]),
        "blue" => Ok([0, 0, 255, 255]),
        "yellow" => Ok([255, 255, 0, 255]),
        "cyan" | "aqua" => Ok([0, 255, 255, 255]),
        "magenta" | "fuchsia" => Ok([255, 0, 255, 255]),
        "gray" | "grey" => Ok([128, 128, 128, 255]),
        "silver" => Ok([192, 192, 192, 255]),
        "maroon" => Ok([128, 0, 0, 255]),
        "olive" => Ok([128, 128, 0, 255]),
        "purple" => Ok([128, 0, 128, 255]),
        "teal" => Ok([0, 128, 128, 255]),
        "navy" => Ok([0, 0, 128, 255]),
        "orange" => Ok([255, 165, 0, 255]),
        other => Err(format!(
            "convert: unknown colour '{other}' (try a `#hex` value or one of: black/white/red/green/blue/yellow/cyan/magenta/gray/transparent/…)"
        )),
    }
}

fn parse_hex_color(hex: &str) -> Result<[u8; 4], String> {
    fn hex_digit(c: u8) -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("'{}' is not a hex digit", c as char)),
        }
    }
    let h = hex.as_bytes();
    let pair = |a: u8, b: u8| -> Result<u8, String> { Ok(hex_digit(a)? * 16 + hex_digit(b)?) };
    let dup = |a: u8| -> Result<u8, String> { let d = hex_digit(a)?; Ok(d * 16 + d) };
    match h.len() {
        3 => Ok([dup(h[0])?, dup(h[1])?, dup(h[2])?, 255]),
        4 => Ok([dup(h[0])?, dup(h[1])?, dup(h[2])?, dup(h[3])?]),
        6 => Ok([pair(h[0], h[1])?, pair(h[2], h[3])?, pair(h[4], h[5])?, 255]),
        8 => Ok([
            pair(h[0], h[1])?,
            pair(h[2], h[3])?,
            pair(h[4], h[5])?,
            pair(h[6], h[7])?,
        ]),
        _ => Err(format!(
            "'#{hex}' is not a valid hex colour (expected 3/4/6/8 hex digits)"
        )),
    }
}

/// Apply the `oxideav-generator` shorthand translator when it's
/// linked in; otherwise return the input verbatim. Keeping the call
/// behind a feature gate means the convert verb still works (with a
/// clear error from the source registry) when the generator crate is
/// excluded.
#[cfg(feature = "generator")]
fn translate_input_shorthand(input: &str) -> String {
    oxideav_generator::shorthand::translate(input)
}

#[cfg(not(feature = "generator"))]
fn translate_input_shorthand(input: &str) -> String {
    input.to_string()
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

    #[test]
    fn density_background_alpha_chain() {
        let p = parse(&to_vec(&[
            "in.pdf",
            "-density",
            "300",
            "-background",
            "white",
            "-alpha",
            "remove",
            "-alpha",
            "off",
            "page-%03d.png",
        ]))
        .unwrap();
        assert_eq!(p.input, "in.pdf");
        assert_eq!(p.output, "page-%03d.png");
        assert_eq!(
            p.ops,
            vec![
                Op::Density(300),
                Op::Background([255, 255, 255, 255]),
                Op::Alpha(AlphaOp::Remove),
                Op::Alpha(AlphaOp::Off),
            ]
        );
    }

    #[test]
    fn density_zero_rejected() {
        let err = parse(&to_vec(&["a.png", "-density", "0", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("-density must be > 0"));
    }

    #[test]
    fn alpha_unknown_subcommand_rejected() {
        let err = parse(&to_vec(&["a.png", "-alpha", "fnord", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("unknown subcommand"));
    }

    #[test]
    fn background_hex_color() {
        let p = parse(&to_vec(&["a.png", "-background", "#ff8000", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Background([255, 128, 0, 255])]);
    }

    #[test]
    fn background_hex_with_alpha() {
        let p = parse(&to_vec(&["a.png", "-background", "#80808040", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Background([128, 128, 128, 64])]);
    }

    #[test]
    fn printf_template_zero_padded() {
        let t = parse_printf_template("page-%03d.png").unwrap().unwrap();
        assert_eq!(t.prefix, "page-");
        assert_eq!(t.width, 3);
        assert_eq!(t.suffix, ".png");
    }

    #[test]
    fn printf_template_unpadded() {
        let t = parse_printf_template("out%d.jpg").unwrap().unwrap();
        assert_eq!(t.prefix, "out");
        assert_eq!(t.width, 0);
        assert_eq!(t.suffix, ".jpg");
    }

    #[test]
    fn printf_template_absent_returns_none() {
        assert!(parse_printf_template("out.png").unwrap().is_none());
        assert!(parse_printf_template("/some/dir/out.jpg").unwrap().is_none());
    }

    #[test]
    fn printf_template_multiple_d_rejected() {
        let err = parse_printf_template("page-%d-frame-%d.png").unwrap_err();
        assert!(format!("{err:?}").contains("more than one"));
    }

    #[test]
    fn printf_template_unsupported_specifier_rejected() {
        let err = parse_printf_template("out-%s.png").unwrap_err();
        assert!(format!("{err:?}").contains("unsupported format specifier"));
    }

    #[test]
    fn parse_populates_output_template_when_present() {
        let p = parse(&to_vec(&["in.pdf", "page-%02d.png"])).unwrap();
        let t = p.output_template.unwrap();
        assert_eq!(t.prefix, "page-");
        assert_eq!(t.width, 2);
        assert_eq!(t.suffix, ".png");
    }

    #[test]
    fn parse_leaves_output_template_none_for_literal_path() {
        let p = parse(&to_vec(&["in.png", "out.jpg"])).unwrap();
        assert!(p.output_template.is_none());
    }

    #[test]
    fn input_selector_single_page() {
        let (path, sel) = split_input_selector("input.pdf[0]").unwrap();
        assert_eq!(path, "input.pdf");
        assert_eq!(sel, Some(PageSelector::Single(0)));
    }

    #[test]
    fn input_selector_range() {
        let (path, sel) = split_input_selector("foo.pdf[2-5]").unwrap();
        assert_eq!(path, "foo.pdf");
        assert_eq!(sel, Some(PageSelector::Range(2, 5)));
    }

    #[test]
    fn input_selector_absent_passes_through() {
        let (path, sel) = split_input_selector("plain.pdf").unwrap();
        assert_eq!(path, "plain.pdf");
        assert_eq!(sel, None);
    }

    #[test]
    fn input_selector_empty_brackets_rejected() {
        assert!(split_input_selector("foo.pdf[]").is_err());
    }

    #[test]
    fn input_selector_non_numeric_rejected() {
        assert!(split_input_selector("foo.pdf[abc]").is_err());
        assert!(split_input_selector("foo.pdf[1-x]").is_err());
    }

    #[test]
    fn input_selector_extra_dash_rejected() {
        assert!(split_input_selector("foo.pdf[1-2-3]").is_err());
    }

    #[test]
    fn parse_pulls_selector_into_input_pages() {
        let p = parse(&to_vec(&["in.pdf[0]", "out.png"])).unwrap();
        assert_eq!(p.input, "in.pdf");
        assert_eq!(p.input_pages, Some(PageSelector::Single(0)));
    }

    #[test]
    fn parse_pulls_range_selector_into_input_pages() {
        let p = parse(&to_vec(&["in.pdf[2-4]", "page-%02d.png"])).unwrap();
        assert_eq!(p.input, "in.pdf");
        assert_eq!(p.input_pages, Some(PageSelector::Range(2, 4)));
        assert!(p.output_template.is_some());
    }
}
