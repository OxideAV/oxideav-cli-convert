//! IM-style arg parser.
//!
//! `convert INPUT [-op VALUE]... OUTPUT` — first positional arg is
//! the input, last positional arg is the output, everything in
//! between is `-flag` pairs.  Most single-word ops take exactly one
//! following value; a few (`-strip`) are valueless.  Any unrecognised
//! flag errors out with a clear message — we never silently drop.

use crate::op::{
    AlphaOp, CameraSpec, ConvertPlan, Dither, GltfFormatChoice, LightSpec, Mesh3DOptions,
    Mesh3DRenderMode, Op, PageSelector, PrintfTemplate, ProjectionMode, ResizeMode,
    StlFormatChoice,
};
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
    let mut ping = false;
    let mut probe = false;
    let mut probe_json = false;
    let mut probe_watch = false;
    let mut mesh3d_options = Mesh3DOptions::default();

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
                let (mode, core) = ResizeMode::split_suffix(v);
                let (w, h) = parse_resize_geometry(core, mode)
                    .map_err(|e| Error::invalid(format!("convert: -resize: {e}")))?;
                ops.push(Op::Resize {
                    width: w,
                    height: h,
                    mode,
                });
                i += 2;
            }
            // `-thumbnail` is IM's "make a small representative image"
            // convenience flag. Same geometry grammar as `-resize`;
            // semantics differ in that IM also strips metadata and
            // (for JPEG inputs) honours EXIF orientation. We unroll it
            // into [`Op::Thumbnail`] which the runners expand to
            // `Resize { mode } + Strip` (auto-orient is documented as
            // a follow-up — needs an EXIF reader on the source side).
            "-thumbnail" => {
                let v = val(i + 1)?;
                let (mode, core) = ResizeMode::split_suffix(v);
                let (w, h) = parse_resize_geometry(core, mode)
                    .map_err(|e| Error::invalid(format!("convert: -thumbnail: {e}")))?;
                ops.push(Op::Thumbnail {
                    width: w,
                    height: h,
                    mode,
                });
                i += 2;
            }
            // `-define KEY[=VALUE]` — opaque codec-specific tunable.
            // Forwarded literally to the sink encoder; the encoder
            // ignores keys it doesn't recognise. IM's grammar uses `:`
            // inside the key as a namespace separator (e.g.
            // `jpeg:dct-method=float`); we don't parse that — the key
            // is preserved verbatim.
            "-define" => {
                let v = val(i + 1)?;
                let (key, value) = match v.split_once('=') {
                    Some((k, val)) => (k.to_string(), Some(val.to_string())),
                    None => (v.to_string(), None),
                };
                if key.is_empty() {
                    return Err(Error::invalid(format!(
                        "convert: -define: '{v}' has an empty KEY (expected KEY[=VALUE])"
                    )));
                }
                ops.push(Op::Define { key, value });
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
            "-ping" => {
                ping = true;
                i += 1;
            }
            // `--probe` is GNU-style double-dash because it's a
            // session-scope mode switch (no value, no per-input
            // semantics) — same shape as `--help` / `--version` would
            // have if the convert verb owned them. Single-dash `-probe`
            // is reserved as a future IM-style probe-OP that might
            // legitimately want an argument.
            "--probe" => {
                probe = true;
                i += 1;
            }
            // `--json` selects a machine-readable output flavour for
            // the probe summary. It's only meaningful paired with
            // `--probe`; the validation runs after the loop so we can
            // emit a clear "needs --probe" error rather than a generic
            // "unknown flag" one.
            "--json" => {
                probe_json = true;
                i += 1;
            }
            // `--watch` re-runs the probe whenever the input file's
            // mtime changes. Like `--json`, it's only meaningful paired
            // with `--probe` (there's no other long-running mode for it
            // to attach to); the post-loop validation surfaces a clear
            // "needs --probe" error rather than a generic "unknown
            // flag" one.
            "--watch" => {
                probe_watch = true;
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
            "-rotate" => {
                let v = val(i + 1)?;
                let n: i32 = v.parse().map_err(|_| {
                    Error::invalid(format!("convert: -rotate: '{v}' is not an integer"))
                })?;
                // Round-1 supports only quarter-turn rotations.
                // Other angles need a real resampler (bilinear / lanczos
                // sample at non-integer coordinates), which we'll wire
                // through oxideav-image-filter once it lands.
                if !matches!(n, 90 | 180 | 270 | -90 | -180 | -270) {
                    return Err(Error::invalid(format!(
                        "convert: -rotate: only multiples of 90 supported (got {n})"
                    )));
                }
                ops.push(Op::Rotate { degrees: n });
                i += 2;
            }
            "-flip" => {
                ops.push(Op::Flip);
                i += 1;
            }
            "-flop" => {
                ops.push(Op::Flop);
                i += 1;
            }
            "-negate" => {
                ops.push(Op::Negate);
                i += 1;
            }
            "-crop" => {
                let v = val(i + 1)?;
                let (w, h, x, y) =
                    parse_crop(v).map_err(|e| Error::invalid(format!("convert: -crop: {e}")))?;
                ops.push(Op::Crop { x, y, w, h });
                i += 2;
            }
            // ---- Round-next: IM-style image-filter flags wired
            // through to the matching `oxideav-image-filter` factory. ----
            "-sharpen" => {
                let v = val(i + 1)?;
                let (radius, sigma) =
                    parse_blur(v).map_err(|e| Error::invalid(format!("convert: -sharpen: {e}")))?;
                ops.push(Op::Sharpen { radius, sigma });
                i += 2;
            }
            "-unsharp" => {
                let v = val(i + 1)?;
                let (radius, sigma, amount, threshold) = parse_unsharp(v)
                    .map_err(|e| Error::invalid(format!("convert: -unsharp: {e}")))?;
                ops.push(Op::Unsharp {
                    radius,
                    sigma,
                    amount,
                    threshold,
                });
                i += 2;
            }
            "-gamma" => {
                let v = val(i + 1)?;
                let g: f32 = v.parse().map_err(|_| {
                    Error::invalid(format!("convert: -gamma: '{v}' is not a finite float"))
                })?;
                if !g.is_finite() || g <= 0.0 {
                    return Err(Error::invalid(format!(
                        "convert: -gamma: value must be > 0 (got {g})"
                    )));
                }
                ops.push(Op::Gamma { value: g });
                i += 2;
            }
            "-brightness-contrast" => {
                let v = val(i + 1)?;
                let (b, c) = parse_brightness_contrast(v)
                    .map_err(|e| Error::invalid(format!("convert: -brightness-contrast: {e}")))?;
                ops.push(Op::BrightnessContrast {
                    brightness: b,
                    contrast: c,
                });
                i += 2;
            }
            "-contrast" => {
                // IM's `-contrast` takes no arg and applies a single
                // small contrast bump. Repeated `-contrast` accumulates.
                ops.push(Op::Contrast { delta: 1 });
                i += 1;
            }
            "-sepia" => {
                let v = val(i + 1)?;
                let t = parse_percent_or_unit(v)
                    .map_err(|e| Error::invalid(format!("convert: -sepia: {e}")))?;
                ops.push(Op::Sepia { threshold: t });
                i += 2;
            }
            "-modulate" => {
                let v = val(i + 1)?;
                let (b, s, h) = parse_modulate(v)
                    .map_err(|e| Error::invalid(format!("convert: -modulate: {e}")))?;
                ops.push(Op::Modulate {
                    brightness: b,
                    saturation: s,
                    hue: h,
                });
                i += 2;
            }
            "-level" => {
                let v = val(i + 1)?;
                let (b, g, w) =
                    parse_level(v).map_err(|e| Error::invalid(format!("convert: -level: {e}")))?;
                ops.push(Op::Level {
                    black: b,
                    gamma: g,
                    white: w,
                });
                i += 2;
            }
            "-normalize" => {
                ops.push(Op::Normalize);
                i += 1;
            }
            "-threshold" => {
                let v = val(i + 1)?;
                let n = parse_threshold_pct(v)
                    .map_err(|e| Error::invalid(format!("convert: -threshold: {e}")))?;
                ops.push(Op::Threshold { value: n });
                i += 2;
            }
            "-posterize" => {
                let v = val(i + 1)?;
                let n: u32 = v.parse().map_err(|_| {
                    Error::invalid(format!(
                        "convert: -posterize: '{v}' is not a non-negative integer"
                    ))
                })?;
                if n < 2 {
                    return Err(Error::invalid(format!(
                        "convert: -posterize: levels must be >= 2 (got {n})"
                    )));
                }
                ops.push(Op::Posterize { levels: n });
                i += 2;
            }
            "-solarize" => {
                let v = val(i + 1)?;
                let n = parse_threshold_pct(v)
                    .map_err(|e| Error::invalid(format!("convert: -solarize: {e}")))?;
                ops.push(Op::Solarize { value: n });
                i += 2;
            }
            "-vignette" => {
                let v = val(i + 1)?;
                let (radius, sigma, x, y) = parse_vignette(v)
                    .map_err(|e| Error::invalid(format!("convert: -vignette: {e}")))?;
                ops.push(Op::Vignette {
                    radius,
                    sigma,
                    x,
                    y,
                });
                i += 2;
            }
            "-colorize" => {
                let v = val(i + 1)?;
                let (color, amount) = parse_colorize(v)
                    .map_err(|e| Error::invalid(format!("convert: -colorize: {e}")))?;
                ops.push(Op::Colorize { color, amount });
                i += 2;
            }
            "-equalize" => {
                ops.push(Op::Equalize);
                i += 1;
            }
            "-auto-gamma" => {
                ops.push(Op::AutoGamma);
                i += 1;
            }
            "-colorspace" => {
                let v = val(i + 1)?;
                let cs = v.trim().to_string();
                // Round-1 only routes `gray`/`grey` to the grayscale
                // factory; other colourspaces become a recorded no-op
                // (the input keeps its native colourspace).
                let lower = cs.to_ascii_lowercase();
                if !matches!(lower.as_str(), "gray" | "grey" | "rgb" | "srgb") {
                    return Err(Error::unsupported(format!(
                        "convert: -colorspace '{cs}' is not yet wired (round-1 supports gray/grey/rgb/srgb)"
                    )));
                }
                ops.push(Op::Colorspace(cs));
                i += 2;
            }
            // ---- Per-format encoder option flags for the 3D
            //      side-channel. Stored on `ConvertPlan::mesh3d_options`,
            //      not as `Op` entries, since they're plan-level
            //      switches that only the mesh3d_runner consumes
            //      (raster ops have no analogous notion). The flags
            //      are accepted on every input shape (PDF / raster /
            //      etc.) and silently ignored when the side-channel
            //      doesn't fire — matches IM's tolerant-of-irrelevant-
            //      flags posture.
            "-stl-format" => {
                let v = val(i + 1)?;
                let choice = StlFormatChoice::parse(v).map_err(Error::invalid)?;
                mesh3d_options.stl_format = Some(choice);
                i += 2;
            }
            "-gltf-format" => {
                let v = val(i + 1)?;
                let choice = GltfFormatChoice::parse(v).map_err(Error::invalid)?;
                mesh3d_options.gltf_format = Some(choice);
                i += 2;
            }
            // `-render flat|wireframe` selects the 3D→raster surface
            // model. Stored on `Mesh3DOptions` (sister to `-stl-format`
            // and `-gltf-format`) so it never leaks into the regular
            // pipeline path. Silently ignored when the input/output
            // pair doesn't trigger the 3D→raster side-channel.
            "-render" => {
                let v = val(i + 1)?;
                let mode = Mesh3DRenderMode::parse(v).map_err(Error::invalid)?;
                mesh3d_options.render_mode = Some(mode);
                i += 2;
            }
            // `-light AZIMUTH,ELEVATION,INTENSITY` — directional light
            // override for the 3D→raster renderer's Gouraud / Phong
            // shading paths. Stored on `Mesh3DOptions::light`; silently
            // ignored when the input/output pair doesn't trigger 3D
            // rendering.
            "-light" => {
                let v = val(i + 1)?;
                let spec = LightSpec::parse(v).map_err(Error::invalid)?;
                mesh3d_options.light = Some(spec);
                i += 2;
            }
            // `-camera ELEVATION,AZIMUTH,DISTANCE` — camera override for
            // the 3D→raster renderer (replaces the auto-framed default).
            "-camera" => {
                let v = val(i + 1)?;
                let spec = CameraSpec::parse(v).map_err(Error::invalid)?;
                mesh3d_options.camera = Some(spec);
                i += 2;
            }
            // `-projection perspective|orthographic` — projection mode
            // for the 3D→raster renderer. Default is perspective.
            "-projection" => {
                let v = val(i + 1)?;
                let mode = ProjectionMode::parse(v).map_err(Error::invalid)?;
                mesh3d_options.projection = Some(mode);
                i += 2;
            }
            // `-fov DEGREES` — vertical field of view for the 3D→raster
            // renderer's perspective projection. Default is 60°. Ignored
            // for orthographic projection.
            "-fov" => {
                let v = val(i + 1)?;
                let fov: f32 = v
                    .parse()
                    .map_err(|_| Error::invalid(format!("convert: -fov: '{v}' is not a number")))?;
                if !fov.is_finite() || fov <= 0.0 || fov >= 180.0 {
                    return Err(Error::invalid(format!(
                        "convert: -fov: {fov} must be in (0, 180) degrees"
                    )));
                }
                mesh3d_options.fov_deg = Some(fov);
                i += 2;
            }
            // `-bg COLOR` — background fill for the 3D render canvas.
            // Default is transparent black. Distinct from `-background`
            // which is the IM canvas-fill for alpha-remove + PDF.
            "-bg" => {
                let v = val(i + 1)?;
                let colour = parse_color(v).map_err(Error::invalid)?;
                mesh3d_options.bg = Some(colour);
                i += 2;
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
    // `--probe` is mutually exclusive with an output positional —
    // probing means "describe, don't write", and the user passing both
    // is almost always a mistake (typo'd a flag, copy-pasted from a
    // real conversion). Surface it as a clear actionable error rather
    // than silently writing or silently dropping the output arg.
    if probe && positionals.len() >= 2 {
        return Err(Error::invalid(format!(
            "convert: --probe cannot be combined with an output file ('{}'); use either --probe OR an output file, not both",
            positionals[1]
        )));
    }
    if positionals.len() < 2 && !ping && !probe {
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
    // `--json` is only meaningful as a formatting modifier on
    // `--probe`. Without `--probe` there's no probe summary to
    // serialise, so we'd otherwise silently swallow the flag.
    if probe_json && !probe {
        return Err(Error::invalid(
            "convert: --json requires --probe (today --json only formats the probe summary)",
        ));
    }
    // `--watch` re-runs the probe on mtime change; only meaningful
    // paired with `--probe`. Without `--probe` there's no long-running
    // mode for it to attach to.
    if probe_watch && !probe {
        return Err(Error::invalid(
            "convert: --watch requires --probe (today --watch only re-runs the probe on input mtime change)",
        ));
    }

    let (raw_input, input_pages) = split_input_selector(&positionals[0])?;
    let input = translate_input_shorthand(raw_input);
    let output = positionals.get(1).cloned().unwrap_or_default();
    let output_template = if output.is_empty() {
        None
    } else {
        parse_printf_template(&output)?
    };

    Ok(ConvertPlan {
        input,
        input_pages,
        ops,
        output,
        output_template,
        ping,
        probe,
        probe_json,
        probe_watch,
        mesh3d_options,
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
            let n: usize = body.parse().map_err(|_| {
                Error::invalid(format!(
                    "convert: input '{s}': '{body}' is not a non-negative integer page index"
                ))
            })?;
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
    let dup = |a: u8| -> Result<u8, String> {
        let d = hex_digit(a)?;
        Ok(d * 16 + d)
    };
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

/// Parse the geometry-without-suffix portion of a `-resize` /
/// `-thumbnail` arg, branching on the scaling mode the suffix already
/// selected.
///
/// Most modes parse exactly like `WxH` (two positive integers). A few
/// modes have looser grammar:
///
/// - [`ResizeMode::Percent`] — `N`, `Nx`, `xN`, `NxN`. A bare `N`
///   applies to both axes; missing axes default to the other one
///   (`200x` → `200x200`; `x200` → `200x200`). Components must be
///   integer percentages > 0.
/// - [`ResizeMode::Area`] — same `WxH` shape; the runtime treats
///   `W*H` as the target pixel area.
/// - Everything else — strict `WxH`, both > 0.
fn parse_resize_geometry(s: &str, mode: ResizeMode) -> Result<(u32, u32), String> {
    if matches!(mode, ResizeMode::Percent) {
        // `N` / `Nx` / `xN` / `NxN`. Single-number form replicates.
        let s = s.trim();
        let (w_str, h_str) = match s.split_once(['x', 'X']) {
            None => (s, s), // bare N → both axes
            Some((w, h)) => {
                let w = if w.is_empty() { h } else { w };
                let h = if h.is_empty() { w } else { h };
                (w, h)
            }
        };
        let w: u32 = w_str
            .parse()
            .map_err(|_| format!("'{w_str}' is not a non-negative integer percent"))?;
        let h: u32 = h_str
            .parse()
            .map_err(|_| format!("'{h_str}' is not a non-negative integer percent"))?;
        if w == 0 || h == 0 {
            return Err("percent values must both be > 0".into());
        }
        return Ok((w, h));
    }
    parse_wxh(s)
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

/// Parse `-crop WxH+X+Y` (and the simpler `WxH+X+Y` permutations IM
/// also accepts). All four numbers are required; IM-style geometry
/// modifiers (`%`, `!`, `^`, `<`, `>`, `@`, `-X-Y` negative offsets)
/// are NOT yet supported in round-1 — they'd need centering / aspect
/// math we don't have. Bbox bounds checking against the actual image
/// dims happens at apply time, not here.
fn parse_crop(s: &str) -> Result<(u32, u32, u32, u32), String> {
    // Reject IM modifiers up front with a clear message, otherwise
    // they'd be silently swallowed by parse() of `0` etc.
    for mark in ['%', '!', '^', '<', '>', '@'] {
        if s.contains(mark) {
            return Err(format!(
                "'{s}' uses IM modifier '{mark}' (round-1 supports plain WxH+X+Y only)"
            ));
        }
    }
    // IM also accepts `-X` for negative offsets; round-1 sticks to
    // `+X+Y` only.
    let mut parts = s.split('+');
    let wh = parts
        .next()
        .ok_or_else(|| format!("'{s}' is missing the WxH component"))?;
    let x_str = parts
        .next()
        .ok_or_else(|| format!("'{s}' is missing the +X offset (expected WxH+X+Y)"))?;
    let y_str = parts
        .next()
        .ok_or_else(|| format!("'{s}' is missing the +Y offset (expected WxH+X+Y)"))?;
    if parts.next().is_some() {
        return Err(format!("'{s}' has more than two `+` separators"));
    }
    let (w, h) = parse_wxh(wh)?;
    let x: u32 = x_str
        .parse()
        .map_err(|_| format!("'{x_str}' is not a non-negative integer X offset"))?;
    let y: u32 = y_str
        .parse()
        .map_err(|_| format!("'{y_str}' is not a non-negative integer Y offset"))?;
    Ok((w, h, x, y))
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

/// Parse `-unsharp RxS+amount+threshold` (any subset accepted in
/// source order: `R`, `RxS`, `RxS+A`, `RxS+A+T`).
///
/// Defaults match `oxideav_image_filter::Unsharp`'s conventions:
/// sigma = radius/2, amount = 1.0, threshold = 0 (out of 255).
fn parse_unsharp(s: &str) -> Result<(u32, f32, f32, u8), String> {
    // `+` separates the three optional tail components. Keep the
    // first chunk (radius / sigma) intact.
    let mut parts = s.splitn(3, '+');
    let head = parts.next().unwrap_or(s);
    let amount_str = parts.next();
    let threshold_str = parts.next();
    let (radius, sigma) = parse_blur(head)?;
    let amount: f32 = match amount_str {
        Some(a) => a
            .parse()
            .map_err(|_| format!("'{a}' is not a finite float"))?,
        None => 1.0,
    };
    let threshold: u8 = match threshold_str {
        // IM lets the threshold be either an integer 0..=255 or a
        // percentage. Accept both shapes; cap to u8.
        Some(t) => parse_threshold_pct(t)?,
        None => 0,
    };
    Ok((radius, sigma, amount, threshold))
}

/// Parse `-brightness-contrast B[xC]` / `B[,C]` / `B`. IM uses both
/// `x` and `,` as separators in different docs; accept either.
/// Range is `[-100..=100]` per IM.
fn parse_brightness_contrast(s: &str) -> Result<(f32, f32), String> {
    let (bs, cs) = match s.split_once(['x', 'X', ',']) {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let b: f32 = bs
        .parse()
        .map_err(|_| format!("'{bs}' is not a finite float"))?;
    let c: f32 = match cs {
        Some(s) if !s.is_empty() => s
            .parse()
            .map_err(|_| format!("'{s}' is not a finite float"))?,
        _ => 0.0,
    };
    if !(-100.0..=100.0).contains(&b) {
        return Err(format!("brightness {b} out of range (-100..=100)"));
    }
    if !(-100.0..=100.0).contains(&c) {
        return Err(format!("contrast {c} out of range (-100..=100)"));
    }
    Ok((b, c))
}

/// Parse a value that's either a percentage (`50%`) or a unit-range
/// scalar (`0.5`). Used by `-sepia`. Returns the scalar in 0..=1.
fn parse_percent_or_unit(s: &str) -> Result<f32, String> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        let v: f32 = p
            .parse()
            .map_err(|_| format!("'{p}' is not a finite float"))?;
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("'{s}' is out of range (0%..=100%)"));
        }
        return Ok(v / 100.0);
    }
    let v: f32 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a finite float"))?;
    if !(0.0..=1.0).contains(&v) {
        return Err(format!("'{s}' is out of range (0.0..=1.0)"));
    }
    Ok(v)
}

/// Parse `-modulate B[,S[,H]]`. Each element is percent-of-base
/// (100 = identity). Missing tail elements default to 100 / 100 / 100.
fn parse_modulate(s: &str) -> Result<(f32, f32, f32), String> {
    let mut parts = s.split(',').map(|p| p.trim());
    let b = parts
        .next()
        .ok_or_else(|| format!("'{s}' is missing the brightness value"))?;
    let s_field = parts.next();
    let h_field = parts.next();
    if parts.next().is_some() {
        return Err(format!(
            "'{s}' has more than 3 components (expected B[,S[,H]])"
        ));
    }
    let bv: f32 = b
        .parse()
        .map_err(|_| format!("'{b}' is not a finite float"))?;
    let sv: f32 = match s_field {
        Some(t) if !t.is_empty() => t
            .parse()
            .map_err(|_| format!("'{t}' is not a finite float"))?,
        _ => 100.0,
    };
    let hv: f32 = match h_field {
        Some(t) if !t.is_empty() => t
            .parse()
            .map_err(|_| format!("'{t}' is not a finite float"))?,
        _ => 100.0,
    };
    Ok((bv, sv, hv))
}

/// Parse `-level B[/G[/W]]` or `-level B,G,W`. IM accepts both
/// separators. Black/white are 0..=255 (or `N%` for a percentage).
/// Gamma is `> 0`.
fn parse_level(s: &str) -> Result<(u8, f32, u8), String> {
    let parts: Vec<&str> = s.split(['/', ',']).map(|p| p.trim()).collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(format!("'{s}' is not in B[/G[/W]] form (1..=3 components)"));
    }
    let black = parse_byte_or_percent(parts[0])?;
    let gamma: f32 = if parts.len() >= 2 && !parts[1].is_empty() {
        let g: f32 = parts[1]
            .parse()
            .map_err(|_| format!("'{}' is not a finite float", parts[1]))?;
        if !g.is_finite() || g <= 0.0 {
            return Err(format!("gamma {g} must be > 0"));
        }
        g
    } else {
        1.0
    };
    let white = if parts.len() == 3 && !parts[2].is_empty() {
        parse_byte_or_percent(parts[2])?
    } else {
        255
    };
    if black > white {
        return Err(format!(
            "black point ({black}) must be <= white point ({white})"
        ));
    }
    Ok((black, gamma, white))
}

/// Parse `0..=255` or `N%` → byte. Used by `-level`.
fn parse_byte_or_percent(s: &str) -> Result<u8, String> {
    if let Some(p) = s.strip_suffix('%') {
        let v: f32 = p
            .parse()
            .map_err(|_| format!("'{p}' is not a finite float"))?;
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("'{s}' is out of range (0%..=100%)"));
        }
        return Ok((v * 2.55).round().clamp(0.0, 255.0) as u8);
    }
    let n: u32 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a non-negative integer"))?;
    if n > 255 {
        return Err(format!("'{s}' is out of range (0..=255)"));
    }
    Ok(n as u8)
}

/// Parse `-threshold N%` / `-threshold N` / `-solarize N%` —
/// returns the resulting 0..=255 byte value.
fn parse_threshold_pct(s: &str) -> Result<u8, String> {
    parse_byte_or_percent(s.trim())
}

/// Parse `-vignette R[+S][+X[+Y]]`.
///
/// Mirrors IM's `-vignette geometry`-style argument. `R+S` are the
/// Gaussian radius and sigma in pixels; `S` defaults to `R / 2.0` when
/// omitted (matching IM and our `-blur RxS` convention). `+X+Y` are the
/// **normalised** image-relative centre offsets in `[0.0, 1.0]`
/// (default `0.5 / 0.5` = image centre). IM's `-vignette R+S+X+Y` uses
/// pixel offsets for `X+Y`, but the image-filter `Vignette` factory is
/// resolution-independent and takes them normalised; document the
/// difference here. To target the centre — by far the common case —
/// pass just `R` (e.g. `-vignette 50`).
fn parse_vignette(s: &str) -> Result<(f32, f32, f32, f32), String> {
    let mut parts = s.split('+');
    let r_str = parts
        .next()
        .ok_or_else(|| format!("'{s}' is missing the radius component"))?;
    let radius: f32 = r_str
        .parse()
        .map_err(|_| format!("'{r_str}' is not a finite float radius"))?;
    if !radius.is_finite() || radius < 0.0 {
        return Err(format!("radius {radius} must be >= 0"));
    }
    let s_part = parts.next();
    let sigma: f32 = match s_part {
        Some(t) if !t.is_empty() => t
            .parse()
            .map_err(|_| format!("'{t}' is not a finite float sigma"))?,
        _ => radius / 2.0,
    };
    if !sigma.is_finite() || sigma < 0.0 {
        return Err(format!("sigma {sigma} must be >= 0"));
    }
    let x: f32 = match parts.next() {
        Some(t) if !t.is_empty() => t
            .parse()
            .map_err(|_| format!("'{t}' is not a finite float x"))?,
        _ => 0.5,
    };
    let y: f32 = match parts.next() {
        Some(t) if !t.is_empty() => t
            .parse()
            .map_err(|_| format!("'{t}' is not a finite float y"))?,
        _ => 0.5,
    };
    if parts.next().is_some() {
        return Err(format!("'{s}' has more than 4 components"));
    }
    if !x.is_finite() || !y.is_finite() {
        return Err(format!("x/y must be finite (got x={x}, y={y})"));
    }
    Ok((radius, sigma, x, y))
}

/// Parse `-colorize C[xC[xC]]/A%`.
///
/// The colour part `C[xC[xC]]` accepts the same grammar as
/// [`parse_color`] — CSS L3 named colours and `#hex` 3/4/6/8 forms —
/// PLUS the IM-style `R[xG[xB]]` per-channel byte triplet. When only
/// `/A%` is given (no colour part), the target defaults to white. The
/// `A%` portion is a percentage in `[0%..=100%]`; we accept either
/// `N%` or a unit scalar `0.0..=1.0`.
fn parse_colorize(s: &str) -> Result<([u8; 4], f32), String> {
    let s = s.trim();
    let (color_part, amount_part) = match s.split_once('/') {
        Some((c, a)) => (c, a),
        // No `/A%` — interpret the whole string as the amount, with a
        // white default colour. IM's rare "single-arg" form.
        None => ("", s),
    };
    let color = if color_part.is_empty() {
        [255, 255, 255, 255]
    } else if color_part.contains('x') || color_part.contains('X') {
        // R[xG[xB]] per-channel byte triplet.
        let mut parts = color_part.split(['x', 'X']);
        let r_str = parts
            .next()
            .ok_or_else(|| format!("'{color_part}' is missing the R component"))?;
        let r: u32 = r_str
            .parse()
            .map_err(|_| format!("'{r_str}' is not a non-negative integer"))?;
        if r > 255 {
            return Err(format!("R component {r} out of range (0..=255)"));
        }
        let g: u32 = match parts.next() {
            Some(t) if !t.is_empty() => t
                .parse()
                .map_err(|_| format!("'{t}' is not a non-negative integer"))?,
            _ => r,
        };
        if g > 255 {
            return Err(format!("G component {g} out of range (0..=255)"));
        }
        let b: u32 = match parts.next() {
            Some(t) if !t.is_empty() => t
                .parse()
                .map_err(|_| format!("'{t}' is not a non-negative integer"))?,
            _ => g,
        };
        if b > 255 {
            return Err(format!("B component {b} out of range (0..=255)"));
        }
        if parts.next().is_some() {
            return Err(format!(
                "'{color_part}' has more than 3 components (expected R[xG[xB]])"
            ));
        }
        [r as u8, g as u8, b as u8, 255]
    } else {
        // CSS named / `#hex`.
        parse_color(color_part)?
    };
    let amount = parse_percent_or_unit(amount_part)?;
    Ok((color, amount))
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
                mode: ResizeMode::Default,
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
                mode: ResizeMode::Force,
            }]
        );
    }

    #[test]
    fn resize_fill_caret_suffix() {
        let p = parse(&to_vec(&["a.png", "-resize", "200x100^", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 200,
                height: 100,
                mode: ResizeMode::Fill,
            }]
        );
    }

    #[test]
    fn resize_shrink_only_suffix() {
        let p = parse(&to_vec(&["a.png", "-resize", "100x100>", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 100,
                height: 100,
                mode: ResizeMode::Shrink,
            }]
        );
    }

    #[test]
    fn resize_grow_only_suffix() {
        let p = parse(&to_vec(&["a.png", "-resize", "1024x1024<", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 1024,
                height: 1024,
                mode: ResizeMode::Grow,
            }]
        );
    }

    #[test]
    fn resize_percent_two_axis() {
        let p = parse(&to_vec(&["a.png", "-resize", "50x200%", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 50,
                height: 200,
                mode: ResizeMode::Percent,
            }]
        );
    }

    #[test]
    fn resize_percent_single_value_replicates() {
        // IM lets a bare `N%` mean "N percent on both axes"; we follow.
        let p = parse(&to_vec(&["a.png", "-resize", "75%", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 75,
                height: 75,
                mode: ResizeMode::Percent,
            }]
        );
    }

    #[test]
    fn resize_area_at_suffix() {
        let p = parse(&to_vec(&["a.png", "-resize", "640x480@", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Resize {
                width: 640,
                height: 480,
                mode: ResizeMode::Area,
            }]
        );
    }

    #[test]
    fn thumbnail_with_geometry_modifier() {
        let p = parse(&to_vec(&["a.png", "-thumbnail", "128x128^", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Thumbnail {
                width: 128,
                height: 128,
                mode: ResizeMode::Fill,
            }]
        );
    }

    #[test]
    fn thumbnail_default_mode() {
        let p = parse(&to_vec(&["a.png", "-thumbnail", "100x100", "b.jpg"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Thumbnail {
                width: 100,
                height: 100,
                mode: ResizeMode::Default,
            }]
        );
    }

    #[test]
    fn define_with_value_round_trips_namespaced_key() {
        let p = parse(&to_vec(&[
            "a.png",
            "-define",
            "jpeg:dct-method=float",
            "b.jpg",
        ]))
        .unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Define {
                key: "jpeg:dct-method".into(),
                value: Some("float".into()),
            }]
        );
    }

    #[test]
    fn define_bare_key_no_value() {
        // `-define KEY` (no `=VALUE`) sets the flag-style key to JSON true.
        let p = parse(&to_vec(&[
            "a.png",
            "-define",
            "png:strip-comments",
            "b.png",
        ]))
        .unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Define {
                key: "png:strip-comments".into(),
                value: None,
            }]
        );
    }

    #[test]
    fn define_empty_key_rejected() {
        let err = parse(&to_vec(&["a.png", "-define", "=value", "b.png"])).unwrap_err();
        assert!(
            format!("{err:?}").contains("empty KEY"),
            "unexpected error: {err:?}"
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
    fn rotate_90_parses() {
        let p = parse(&to_vec(&["a.png", "-rotate", "90", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Rotate { degrees: 90 }]);
    }

    #[test]
    fn rotate_negative_270_parses() {
        let p = parse(&to_vec(&["a.png", "-rotate", "-270", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Rotate { degrees: -270 }]);
    }

    #[test]
    fn rotate_45_rejected() {
        let err = parse(&to_vec(&["a.png", "-rotate", "45", "b.png"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("only multiples of 90 supported"),
            "unexpected message: {msg}"
        );
        assert!(msg.contains("got 45"));
    }

    #[test]
    fn rotate_non_integer_rejected() {
        let err = parse(&to_vec(&["a.png", "-rotate", "abc", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("not an integer"));
    }

    #[test]
    fn flip_and_flop_are_valueless() {
        let p = parse(&to_vec(&["a.png", "-flip", "-flop", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Flip, Op::Flop]);
    }

    #[test]
    fn negate_is_valueless() {
        let p = parse(&to_vec(&["a.png", "-negate", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Negate]);
    }

    #[test]
    fn crop_basic() {
        let p = parse(&to_vec(&["a.png", "-crop", "10x20+3+4", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Crop {
                x: 3,
                y: 4,
                w: 10,
                h: 20
            }]
        );
    }

    #[test]
    fn crop_zero_offset() {
        let p = parse(&to_vec(&["a.png", "-crop", "32x32+0+0", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Crop {
                x: 0,
                y: 0,
                w: 32,
                h: 32
            }]
        );
    }

    #[test]
    fn crop_missing_offset_rejected() {
        let err = parse(&to_vec(&["a.png", "-crop", "32x32", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("missing the +X offset"));
    }

    #[test]
    fn crop_im_modifier_rejected() {
        let err = parse(&to_vec(&["a.png", "-crop", "50%+0+0", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("IM modifier"));
    }

    #[test]
    fn sharpen_basic() {
        let p = parse(&to_vec(&["a.png", "-sharpen", "1x0.5", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Sharpen { radius: 1, sigma }] => assert!((sigma - 0.5).abs() < 1e-6),
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn sharpen_radius_only_defaults_sigma() {
        let p = parse(&to_vec(&["a.png", "-sharpen", "4", "b.png"])).unwrap();
        // Sigma defaults to radius/2 (matches IM and parse_blur).
        assert_eq!(
            p.ops,
            vec![Op::Sharpen {
                radius: 4,
                sigma: 2.0
            }]
        );
    }

    #[test]
    fn unsharp_full_grammar() {
        let p = parse(&to_vec(&["a.png", "-unsharp", "2x1.5+0.8+5", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Unsharp {
                radius: 2,
                sigma,
                amount,
                threshold: 5,
            }] => {
                assert!((sigma - 1.5).abs() < 1e-6);
                assert!((amount - 0.8).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn unsharp_partial_defaults() {
        let p = parse(&to_vec(&["a.png", "-unsharp", "3", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Unsharp {
                radius: 3,
                sigma,
                amount,
                threshold: 0,
            }] => {
                assert!((sigma - 1.5).abs() < 1e-6, "sigma = {sigma}");
                assert!((amount - 1.0).abs() < 1e-6, "amount = {amount}");
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn gamma_basic() {
        let p = parse(&to_vec(&["a.png", "-gamma", "1.8", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Gamma { value }] => assert!((value - 1.8).abs() < 1e-6),
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn gamma_zero_rejected() {
        let err = parse(&to_vec(&["a.png", "-gamma", "0", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("must be > 0"));
    }

    #[test]
    fn gamma_negative_rejected() {
        let err = parse(&to_vec(&["a.png", "-gamma", "-1.5", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("must be > 0"));
    }

    #[test]
    fn brightness_contrast_basic_comma() {
        let p = parse(&to_vec(&["a.png", "-brightness-contrast", "10,5", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::BrightnessContrast {
                brightness: 10.0,
                contrast: 5.0
            }]
        );
    }

    #[test]
    fn brightness_contrast_basic_x_separator() {
        let p = parse(&to_vec(&[
            "a.png",
            "-brightness-contrast",
            "20x-15",
            "b.png",
        ]))
        .unwrap();
        assert_eq!(
            p.ops,
            vec![Op::BrightnessContrast {
                brightness: 20.0,
                contrast: -15.0
            }]
        );
    }

    #[test]
    fn brightness_contrast_brightness_only() {
        let p = parse(&to_vec(&["a.png", "-brightness-contrast", "30", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::BrightnessContrast {
                brightness: 30.0,
                contrast: 0.0
            }]
        );
    }

    #[test]
    fn brightness_contrast_out_of_range_rejected() {
        let err = parse(&to_vec(&[
            "a.png",
            "-brightness-contrast",
            "150,0",
            "b.png",
        ]))
        .unwrap_err();
        assert!(format!("{err:?}").contains("out of range"));
    }

    #[test]
    fn contrast_step_default_one() {
        let p = parse(&to_vec(&["a.png", "-contrast", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Contrast { delta: 1 }]);
    }

    #[test]
    fn contrast_repeated_accumulates_via_chain() {
        // IM accumulates `-contrast` across repeated flags. We capture
        // each as a separate Op; the executor sums them.
        let p = parse(&to_vec(&["a.png", "-contrast", "-contrast", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Contrast { delta: 1 }, Op::Contrast { delta: 1 }]
        );
    }

    #[test]
    fn sepia_percent_form() {
        let p = parse(&to_vec(&["a.png", "-sepia", "80%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Sepia { threshold }] => assert!((threshold - 0.8).abs() < 1e-6),
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn sepia_unit_form() {
        let p = parse(&to_vec(&["a.png", "-sepia", "0.5", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Sepia { threshold }] => assert!((threshold - 0.5).abs() < 1e-6),
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn modulate_full_triplet() {
        let p = parse(&to_vec(&["a.png", "-modulate", "120,80,150", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Modulate {
                brightness: 120.0,
                saturation: 80.0,
                hue: 150.0,
            }]
        );
    }

    #[test]
    fn modulate_brightness_only_defaults_other_components() {
        let p = parse(&to_vec(&["a.png", "-modulate", "150", "b.png"])).unwrap();
        assert_eq!(
            p.ops,
            vec![Op::Modulate {
                brightness: 150.0,
                saturation: 100.0,
                hue: 100.0,
            }]
        );
    }

    #[test]
    fn level_full_triplet() {
        let p = parse(&to_vec(&["a.png", "-level", "10/1.2/250", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Level {
                black: 10,
                gamma,
                white: 250,
            }] => assert!((gamma - 1.2).abs() < 1e-6),
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn level_percent_endpoints() {
        let p = parse(&to_vec(&["a.png", "-level", "10%/1.0/90%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Level {
                black,
                gamma: _,
                white,
            }] => {
                // 10% of 255 ≈ 26; 90% ≈ 230.
                assert_eq!(*black, 26);
                assert_eq!(*white, 230);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn level_inverted_endpoints_rejected() {
        let err = parse(&to_vec(&["a.png", "-level", "200/1.0/100", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("must be <="));
    }

    #[test]
    fn normalize_is_valueless() {
        let p = parse(&to_vec(&["a.png", "-normalize", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Normalize]);
    }

    #[test]
    fn threshold_percent() {
        let p = parse(&to_vec(&["a.png", "-threshold", "50%", "b.png"])).unwrap();
        // 50% of 255 ≈ 128.
        assert_eq!(p.ops, vec![Op::Threshold { value: 128 }]);
    }

    #[test]
    fn threshold_byte() {
        let p = parse(&to_vec(&["a.png", "-threshold", "200", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Threshold { value: 200 }]);
    }

    #[test]
    fn posterize_basic() {
        let p = parse(&to_vec(&["a.png", "-posterize", "4", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Posterize { levels: 4 }]);
    }

    #[test]
    fn posterize_one_rejected() {
        let err = parse(&to_vec(&["a.png", "-posterize", "1", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("levels must be >= 2"));
    }

    #[test]
    fn solarize_percent() {
        let p = parse(&to_vec(&["a.png", "-solarize", "100", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Solarize { value: 100 }]);
    }

    #[test]
    fn colorspace_gray_recognised() {
        let p = parse(&to_vec(&["a.png", "-colorspace", "Gray", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Colorspace("Gray".into())]);
    }

    #[test]
    fn colorspace_rgb_recognised() {
        let p = parse(&to_vec(&["a.png", "-colorspace", "rgb", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Colorspace("rgb".into())]);
    }

    #[test]
    fn colorspace_unsupported_rejected() {
        let err = parse(&to_vec(&["a.png", "-colorspace", "CMYK", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("not yet wired"));
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
        assert!(parse_printf_template("/some/dir/out.jpg")
            .unwrap()
            .is_none());
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
    fn ping_flag_off_by_default() {
        let p = parse(&to_vec(&["in.png", "out.jpg"])).unwrap();
        assert!(!p.ping);
    }

    #[test]
    fn ping_flag_set_when_present() {
        let p = parse(&to_vec(&["-ping", "in.png", "out.jpg"])).unwrap();
        assert!(p.ping);
    }

    #[test]
    fn ping_makes_output_optional() {
        let p = parse(&to_vec(&["-ping", "in.png"])).unwrap();
        assert!(p.ping);
        assert_eq!(p.output, "");
        assert!(p.output_template.is_none());
    }

    #[test]
    fn missing_output_without_ping_still_errors() {
        let err = parse(&to_vec(&["in.png"])).unwrap_err();
        // Should be Invalid, not Unsupported.
        assert!(format!("{err:?}").contains("no output file"));
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

    // ---- Round-after-next: -vignette / -colorize / -equalize / -auto-gamma ----

    #[test]
    fn vignette_radius_only_defaults_sigma_and_centre() {
        let p = parse(&to_vec(&["a.png", "-vignette", "50", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Vignette {
                radius,
                sigma,
                x,
                y,
            }] => {
                assert!((radius - 50.0).abs() < 1e-6);
                // Sigma defaults to radius / 2.
                assert!((sigma - 25.0).abs() < 1e-6);
                // X/Y default to image centre (0.5, 0.5).
                assert!((x - 0.5).abs() < 1e-6);
                assert!((y - 0.5).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn vignette_radius_and_sigma() {
        let p = parse(&to_vec(&["a.png", "-vignette", "50+10", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Vignette {
                radius,
                sigma,
                x,
                y,
            }] => {
                assert!((radius - 50.0).abs() < 1e-6);
                assert!((sigma - 10.0).abs() < 1e-6);
                assert!((x - 0.5).abs() < 1e-6);
                assert!((y - 0.5).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn vignette_full_grammar_with_xy() {
        let p = parse(&to_vec(&["a.png", "-vignette", "50+10+0.25+0.75", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Vignette {
                radius,
                sigma,
                x,
                y,
            }] => {
                assert!((radius - 50.0).abs() < 1e-6);
                assert!((sigma - 10.0).abs() < 1e-6);
                assert!((x - 0.25).abs() < 1e-6);
                assert!((y - 0.75).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn vignette_too_many_components_rejected() {
        let err = parse(&to_vec(&["a.png", "-vignette", "1+2+3+4+5", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("more than 4"));
    }

    #[test]
    fn vignette_negative_radius_rejected() {
        let err = parse(&to_vec(&["a.png", "-vignette", "-5", "b.png"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("not a finite float radius") || msg.contains("must be >= 0"));
    }

    #[test]
    fn colorize_named_color_with_percent_amount() {
        let p = parse(&to_vec(&["a.png", "-colorize", "red/40%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Colorize { color, amount }] => {
                assert_eq!(*color, [255, 0, 0, 255]);
                assert!((amount - 0.4).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colorize_hex_color() {
        let p = parse(&to_vec(&["a.png", "-colorize", "#ff8000/50%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Colorize { color, amount }] => {
                assert_eq!(*color, [255, 128, 0, 255]);
                assert!((amount - 0.5).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colorize_per_channel_triplet() {
        let p = parse(&to_vec(&["a.png", "-colorize", "200x100x50/25%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Colorize { color, amount }] => {
                assert_eq!(*color, [200, 100, 50, 255]);
                assert!((amount - 0.25).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colorize_single_channel_replicated() {
        // `100/50%` → no `x`, but contains `/`. The colour is the
        // string before `/`; "100" lacks `x`, so we fall through to
        // parse_color which won't match (it's not named / hex). Test
        // that we get a clean error.
        let err = parse(&to_vec(&["a.png", "-colorize", "100/50%", "b.png"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("unknown colour") || msg.contains("not a valid hex"));
    }

    #[test]
    fn colorize_per_channel_one_value_replicates() {
        // `100x` (or `100xx`) — single value means R = G = B = 100.
        let p = parse(&to_vec(&["a.png", "-colorize", "100x100x100/30%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Colorize { color, amount: _ }] => {
                assert_eq!(*color, [100, 100, 100, 255]);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colorize_amount_only_defaults_white() {
        // `/40%` form — IM accepts it; default colour is white.
        let p = parse(&to_vec(&["a.png", "-colorize", "/40%", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Colorize { color, amount }] => {
                assert_eq!(*color, [255, 255, 255, 255]);
                assert!((amount - 0.4).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colorize_amount_unit_scalar() {
        // Without `%`, parse_percent_or_unit accepts a unit scalar.
        let p = parse(&to_vec(&["a.png", "-colorize", "red/0.6", "b.png"])).unwrap();
        match p.ops.as_slice() {
            [Op::Colorize { color, amount }] => {
                assert_eq!(*color, [255, 0, 0, 255]);
                assert!((amount - 0.6).abs() < 1e-6);
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }

    #[test]
    fn colorize_out_of_range_channel_rejected() {
        let err = parse(&to_vec(&["a.png", "-colorize", "300x0x0/50%", "b.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("out of range"));
    }

    #[test]
    fn equalize_is_valueless() {
        let p = parse(&to_vec(&["a.png", "-equalize", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::Equalize]);
    }

    #[test]
    fn auto_gamma_is_valueless() {
        let p = parse(&to_vec(&["a.png", "-auto-gamma", "b.png"])).unwrap();
        assert_eq!(p.ops, vec![Op::AutoGamma]);
    }

    #[test]
    fn vignette_rejects_negative_sigma() {
        let err = parse(&to_vec(&["a.png", "-vignette", "5+-1", "b.png"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not a finite float sigma") || msg.contains("must be >= 0"),
            "unexpected message: {msg}"
        );
    }

    // ---- Per-format encoder option flags (`-stl-format`,
    //      `-gltf-format`). End-to-end coverage lives in
    //      `tests/format_flags.rs`; the unit tests here only check
    //      that the parser populates `ConvertPlan::mesh3d_options`
    //      correctly and rejects malformed values up-front. ----

    #[test]
    fn stl_format_default_is_none() {
        let p = parse(&to_vec(&["in.stl", "out.stl"])).unwrap();
        assert!(p.mesh3d_options.stl_format.is_none());
        assert!(p.ops.is_empty(), "no -stl-format → no Op pushed");
    }

    #[test]
    fn stl_format_ascii_parses() {
        let p = parse(&to_vec(&["in.stl", "-stl-format", "ascii", "out.stl"])).unwrap();
        assert_eq!(p.mesh3d_options.stl_format, Some(StlFormatChoice::Ascii));
    }

    #[test]
    fn stl_format_binary_parses() {
        let p = parse(&to_vec(&["in.stl", "-stl-format", "binary", "out.stl"])).unwrap();
        assert_eq!(p.mesh3d_options.stl_format, Some(StlFormatChoice::Binary));
    }

    #[test]
    fn stl_format_aliases_parse() {
        // `bin` → Binary, `text` → Ascii.
        let p = parse(&to_vec(&["in.stl", "-stl-format", "bin", "out.stl"])).unwrap();
        assert_eq!(p.mesh3d_options.stl_format, Some(StlFormatChoice::Binary));
        let p = parse(&to_vec(&["in.stl", "-stl-format", "text", "out.stl"])).unwrap();
        assert_eq!(p.mesh3d_options.stl_format, Some(StlFormatChoice::Ascii));
    }

    #[test]
    fn stl_format_case_insensitive() {
        let p = parse(&to_vec(&["in.stl", "-stl-format", "ASCII", "out.stl"])).unwrap();
        assert_eq!(p.mesh3d_options.stl_format, Some(StlFormatChoice::Ascii));
    }

    #[test]
    fn stl_format_unknown_value_rejected() {
        let err = parse(&to_vec(&["in.stl", "-stl-format", "xml", "out.stl"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("unknown flavour 'xml'") && msg.contains("expected 'binary'"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn stl_format_missing_value_rejected() {
        let err = parse(&to_vec(&["in.stl", "-stl-format"])).unwrap_err();
        assert!(format!("{err:?}").contains("-stl-format: missing value"));
    }

    #[test]
    fn gltf_format_default_is_none() {
        let p = parse(&to_vec(&["in.stl", "out.gltf"])).unwrap();
        assert!(p.mesh3d_options.gltf_format.is_none());
    }

    #[test]
    fn gltf_format_glb_parses() {
        let p = parse(&to_vec(&["in.stl", "-gltf-format", "glb", "out.glb"])).unwrap();
        assert_eq!(p.mesh3d_options.gltf_format, Some(GltfFormatChoice::Glb));
    }

    #[test]
    fn gltf_format_embedded_parses() {
        let p = parse(&to_vec(&["in.stl", "-gltf-format", "embedded", "out.gltf"])).unwrap();
        assert_eq!(
            p.mesh3d_options.gltf_format,
            Some(GltfFormatChoice::JsonEmbedded)
        );
    }

    #[test]
    fn gltf_format_external_parses_even_though_runtime_rejects() {
        // The parser accepts the token; runtime emits the gltf-rN
        // follow-up message. Decouples the user-facing error from the
        // arg parser's "unknown flag" path.
        let p = parse(&to_vec(&["in.stl", "-gltf-format", "external", "out.gltf"])).unwrap();
        assert_eq!(
            p.mesh3d_options.gltf_format,
            Some(GltfFormatChoice::JsonExternal)
        );
    }

    #[test]
    fn gltf_format_aliases_parse() {
        // `binary` → Glb (matches IM-style synonym tolerance).
        let p = parse(&to_vec(&["in.stl", "-gltf-format", "binary", "out.glb"])).unwrap();
        assert_eq!(p.mesh3d_options.gltf_format, Some(GltfFormatChoice::Glb));
        // `json-embedded` → JsonEmbedded.
        let p = parse(&to_vec(&[
            "in.stl",
            "-gltf-format",
            "json-embedded",
            "out.gltf",
        ]))
        .unwrap();
        assert_eq!(
            p.mesh3d_options.gltf_format,
            Some(GltfFormatChoice::JsonEmbedded)
        );
    }

    #[test]
    fn gltf_format_unknown_value_rejected() {
        let err = parse(&to_vec(&["in.stl", "-gltf-format", "obj", "out.gltf"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("unknown flavour 'obj'") && msg.contains("expected 'glb'"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn render_gouraud_parses() {
        let p = parse(&to_vec(&["in.stl", "-render", "gouraud", "out.png"])).unwrap();
        assert_eq!(
            p.mesh3d_options.render_mode,
            Some(Mesh3DRenderMode::Gouraud)
        );
    }

    #[test]
    fn render_phong_parses() {
        let p = parse(&to_vec(&["in.stl", "-render", "phong", "out.png"])).unwrap();
        assert_eq!(p.mesh3d_options.render_mode, Some(Mesh3DRenderMode::Phong));
    }

    #[test]
    fn render_wire_alias_parses() {
        let p = parse(&to_vec(&["in.stl", "-render", "wire", "out.png"])).unwrap();
        assert_eq!(
            p.mesh3d_options.render_mode,
            Some(Mesh3DRenderMode::Wireframe)
        );
    }

    #[test]
    fn render_unknown_mode_rejected_lists_all_options() {
        let err = parse(&to_vec(&["in.stl", "-render", "raytrace", "out.png"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("unknown mode 'raytrace'")
                && msg.contains("'gouraud'")
                && msg.contains("'phong'"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn light_parses_three_components() {
        let p = parse(&to_vec(&["in.stl", "-light", "30,45,1.5", "out.png"])).unwrap();
        let light = p.mesh3d_options.light.expect("-light should populate");
        assert_eq!(light.azimuth_deg, 30.0);
        assert_eq!(light.elevation_deg, 45.0);
        assert_eq!(light.intensity, 1.5);
    }

    #[test]
    fn light_rejects_negative_intensity() {
        let err = parse(&to_vec(&["in.stl", "-light", "0,0,-1", "out.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("intensity"));
    }

    #[test]
    fn light_rejects_two_component_input() {
        let err = parse(&to_vec(&["in.stl", "-light", "30,45", "out.png"])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("AZIMUTH,ELEVATION,INTENSITY"),
            "expected grammar hint, got: {msg}"
        );
    }

    #[test]
    fn camera_parses_three_components() {
        let p = parse(&to_vec(&["in.stl", "-camera", "20,30,2.0", "out.png"])).unwrap();
        let cam = p.mesh3d_options.camera.expect("-camera should populate");
        assert_eq!(cam.elevation_deg, 20.0);
        assert_eq!(cam.azimuth_deg, 30.0);
        assert_eq!(cam.distance, 2.0);
    }

    #[test]
    fn camera_rejects_zero_distance() {
        let err = parse(&to_vec(&["in.stl", "-camera", "0,0,0", "out.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("distance"));
    }

    #[test]
    fn projection_orthographic_parses() {
        let p = parse(&to_vec(&[
            "in.stl",
            "-projection",
            "orthographic",
            "out.png",
        ]))
        .unwrap();
        assert_eq!(
            p.mesh3d_options.projection,
            Some(ProjectionMode::Orthographic)
        );
    }

    #[test]
    fn projection_perspective_default_is_unset() {
        let p = parse(&to_vec(&["in.stl", "out.png"])).unwrap();
        assert!(p.mesh3d_options.projection.is_none());
    }

    #[test]
    fn projection_alias_parses() {
        let p = parse(&to_vec(&["in.stl", "-projection", "ortho", "out.png"])).unwrap();
        assert_eq!(
            p.mesh3d_options.projection,
            Some(ProjectionMode::Orthographic)
        );
    }

    #[test]
    fn projection_unknown_rejected() {
        let err = parse(&to_vec(&["in.stl", "-projection", "isometric", "out.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("unknown mode 'isometric'"));
    }

    #[test]
    fn fov_parses_within_range() {
        let p = parse(&to_vec(&["in.stl", "-fov", "45", "out.png"])).unwrap();
        assert_eq!(p.mesh3d_options.fov_deg, Some(45.0));
    }

    #[test]
    fn fov_rejects_zero() {
        let err = parse(&to_vec(&["in.stl", "-fov", "0", "out.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("(0, 180)"));
    }

    #[test]
    fn fov_rejects_180_or_above() {
        let err = parse(&to_vec(&["in.stl", "-fov", "180", "out.png"])).unwrap_err();
        assert!(format!("{err:?}").contains("(0, 180)"));
    }

    #[test]
    fn bg_named_colour_parses() {
        let p = parse(&to_vec(&["in.stl", "-bg", "black", "out.png"])).unwrap();
        assert_eq!(p.mesh3d_options.bg, Some([0, 0, 0, 255]));
    }

    #[test]
    fn bg_hex_colour_parses() {
        let p = parse(&to_vec(&["in.stl", "-bg", "#abcdef", "out.png"])).unwrap();
        assert_eq!(p.mesh3d_options.bg, Some([0xab, 0xcd, 0xef, 255]));
    }

    #[test]
    fn bg_transparent_parses() {
        let p = parse(&to_vec(&["in.stl", "-bg", "transparent", "out.png"])).unwrap();
        assert_eq!(p.mesh3d_options.bg, Some([0, 0, 0, 0]));
    }

    #[test]
    fn mesh3d_flags_can_appear_before_input() {
        // IM grammar lets ops appear on either side of the positional;
        // confirm the per-format flags follow the same rule.
        let p = parse(&to_vec(&[
            "-stl-format",
            "ascii",
            "-gltf-format",
            "glb",
            "in.stl",
            "out.stl",
        ]))
        .unwrap();
        assert_eq!(p.mesh3d_options.stl_format, Some(StlFormatChoice::Ascii));
        assert_eq!(p.mesh3d_options.gltf_format, Some(GltfFormatChoice::Glb));
        assert_eq!(p.input, "in.stl");
        assert_eq!(p.output, "out.stl");
    }
}
