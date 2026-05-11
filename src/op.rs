//! IM-style operations supported by `oxideav convert`.
//!
//! Each variant carries exactly the data the CLI needs to hand to
//! [`crate::plan_to_job`], nothing more.

/// Dither strategy used when `-colors N` forces a paletted output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dither {
    None,
    Bayer,
    FloydSteinberg,
}

impl Dither {
    /// Parse the value after `-dither`. Accepts ImageMagick-ish names
    /// case-insensitively (`None`, `FloydSteinberg`, `floyd_steinberg`,
    /// `o8x8` / `ordered` → Bayer).
    pub fn parse(s: &str) -> Result<Dither, String> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(Dither::None),
            "floyd_steinberg" | "floyd-steinberg" | "floydsteinberg" | "fs" => {
                Ok(Dither::FloydSteinberg)
            }
            "bayer" | "ordered" | "o8x8" => Ok(Dither::Bayer),
            other => Err(format!(
                "convert: -dither: unknown strategy '{other}' (expected 'none', 'bayer', or 'floyd_steinberg')"
            )),
        }
    }
}

/// `-alpha SUBCOMMAND`. ImageMagick exposes a small grammar of alpha-channel
/// edits — we cover the ones with deterministic semantics on RGBA buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlphaOp {
    /// `on` / `activate` — enable alpha (no-op when output is already RGBA).
    On,
    /// `off` / `deactivate` — strip the alpha channel from the output
    /// (Rgba → Rgb24).
    Off,
    /// `remove` — composite the image over the current `-background`
    /// colour and force every output pixel's alpha to `255`.
    Remove,
    /// `set` / `opaque` — set every alpha sample to `255` without
    /// compositing.
    Set,
    /// `transparent` — set every alpha sample to `0` without touching
    /// the colour channels.
    Transparent,
}

impl AlphaOp {
    pub fn parse(s: &str) -> Result<AlphaOp, String> {
        match s.to_ascii_lowercase().as_str() {
            "on" | "activate" => Ok(AlphaOp::On),
            "off" | "deactivate" => Ok(AlphaOp::Off),
            "remove" => Ok(AlphaOp::Remove),
            "set" | "opaque" => Ok(AlphaOp::Set),
            "transparent" => Ok(AlphaOp::Transparent),
            other => Err(format!(
                "convert: -alpha: unknown subcommand '{other}' (expected 'on'/'off'/'activate'/'deactivate'/'remove'/'set'/'opaque'/'transparent')"
            )),
        }
    }
}

/// `-resize` / `-thumbnail` geometry-modifier suffix.
///
/// ImageMagick's geometry grammar tags the literal `WxH` with a single
/// trailing character that selects the scaling policy. We honour the
/// six common ones; everything else (`%@` combos, gravity flags, …)
/// is a documented follow-up.
///
/// The runtime sees the original `WxH` plus this tag and resolves the
/// final pixel dimensions against the actual source size — most modes
/// can't be eagerly resolved at parse time because we don't yet know
/// the source dims.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResizeMode {
    /// No suffix — preserve aspect ratio and FIT inside the `WxH` box
    /// (the larger of the two scale factors wins, so neither output
    /// dimension exceeds the request). IM's default geometry policy.
    #[default]
    Default,
    /// `WxH!` — IGNORE aspect ratio, force the exact `WxH` dimensions.
    /// Equivalent to the original `bang = true`.
    Force,
    /// `WxH^` — preserve aspect ratio and FILL the `WxH` box (the
    /// smaller of the two scale factors wins, so both output
    /// dimensions are at least the request). Common pairing with a
    /// follow-up `-crop` to land on exact `WxH`.
    Fill,
    /// `WxH>` — only resize when the input is LARGER than `WxH`
    /// (otherwise pass through unchanged). IM's "shrink-only" policy.
    Shrink,
    /// `WxH<` — only resize when the input is SMALLER than `WxH`
    /// (otherwise pass through unchanged). IM's "enlarge-only" policy.
    Grow,
    /// `WxH%` — interpret `W` and `H` as percentages of the source
    /// dimensions (independent X / Y scale). `WxH` are still parsed as
    /// integer percentages by `parse_wxh`; if only one number is given
    /// it applies to both axes (handled at parse time).
    Percent,
    /// `WxH@` — interpret `W*H` as the TARGET pixel area; both output
    /// dims are scaled by `sqrt(target_area / source_area)` so the
    /// aspect ratio is preserved AND the output area matches the
    /// request.
    Area,
}

impl ResizeMode {
    /// Strip an IM geometry-modifier suffix off `WxH`.
    /// Returns `(mode, geometry-without-suffix)`. No suffix means
    /// `Default` plus the input verbatim. Multiple suffix chars
    /// (e.g. `WxH^!`) are rejected by the args parser layer; here we
    /// only peel the LAST one and report what we found.
    pub fn split_suffix(s: &str) -> (ResizeMode, &str) {
        if let Some(core) = s.strip_suffix('!') {
            return (ResizeMode::Force, core);
        }
        if let Some(core) = s.strip_suffix('^') {
            return (ResizeMode::Fill, core);
        }
        if let Some(core) = s.strip_suffix('>') {
            return (ResizeMode::Shrink, core);
        }
        if let Some(core) = s.strip_suffix('<') {
            return (ResizeMode::Grow, core);
        }
        if let Some(core) = s.strip_suffix('%') {
            return (ResizeMode::Percent, core);
        }
        if let Some(core) = s.strip_suffix('@') {
            return (ResizeMode::Area, core);
        }
        (ResizeMode::Default, s)
    }

    /// Lowercase tag string used in JSON params handed to the resize
    /// factory. Stable identifiers — codecs that grow geometry-mode
    /// awareness can branch off these.
    pub fn as_tag(self) -> &'static str {
        match self {
            ResizeMode::Default => "default",
            ResizeMode::Force => "force",
            ResizeMode::Fill => "fill",
            ResizeMode::Shrink => "shrink",
            ResizeMode::Grow => "grow",
            ResizeMode::Percent => "percent",
            ResizeMode::Area => "area",
        }
    }

    /// Resolve the final `(out_w, out_h)` for a request of `(req_w,
    /// req_h)` against a source size of `(src_w, src_h)`.
    ///
    /// Output is always >= 1 on both axes; clipping to source dims for
    /// `Shrink` / `Grow` modes happens here so the caller can blindly
    /// hand the result to the resize filter without re-checking.
    pub fn resolve(self, req_w: u32, req_h: u32, src_w: u32, src_h: u32) -> (u32, u32) {
        let sw = src_w.max(1) as f64;
        let sh = src_h.max(1) as f64;
        let rw = req_w.max(1) as f64;
        let rh = req_h.max(1) as f64;
        let (out_w, out_h) = match self {
            ResizeMode::Default => {
                // Aspect-preserving fit: pick the smaller scale.
                let s = (rw / sw).min(rh / sh);
                ((sw * s).round(), (sh * s).round())
            }
            ResizeMode::Force => (rw, rh),
            ResizeMode::Fill => {
                let s = (rw / sw).max(rh / sh);
                ((sw * s).round(), (sh * s).round())
            }
            ResizeMode::Shrink => {
                if (src_w <= req_w) && (src_h <= req_h) {
                    (sw, sh)
                } else {
                    let s = (rw / sw).min(rh / sh);
                    ((sw * s).round(), (sh * s).round())
                }
            }
            ResizeMode::Grow => {
                if (src_w >= req_w) && (src_h >= req_h) {
                    (sw, sh)
                } else {
                    let s = (rw / sw).min(rh / sh);
                    ((sw * s).round(), (sh * s).round())
                }
            }
            ResizeMode::Percent => {
                // req_w / req_h are percentages: 50 → half, 200 → double.
                ((sw * rw / 100.0).round(), (sh * rh / 100.0).round())
            }
            ResizeMode::Area => {
                let target_area = rw * rh;
                let src_area = sw * sh;
                let s = (target_area / src_area).sqrt();
                ((sw * s).round(), (sh * s).round())
            }
        };
        ((out_w as i64).max(1) as u32, (out_h as i64).max(1) as u32)
    }
}

/// One convert operation.
///
/// Operations apply in source order — same as `imagemagick convert`,
/// even though we don't yet support IM's stack-reset semantics.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// `-resize WxH[!^<>%@]`. The trailing modifier selects the
    /// scaling policy via [`ResizeMode`]; absence of any modifier is
    /// `ResizeMode::Default` (aspect-preserving fit-inside).
    Resize {
        width: u32,
        height: u32,
        mode: ResizeMode,
    },
    /// `-thumbnail WxH[!^<>%@]` — IM convenience flag. Same geometry
    /// grammar as `-resize`; the runner unrolls it into a Resize plus
    /// Strip pair (and, eventually, an auto-orient pass) so the
    /// downstream pipeline doesn't need a dedicated thumbnail node.
    Thumbnail {
        width: u32,
        height: u32,
        mode: ResizeMode,
    },
    /// `-define KEY[=VALUE]` — opaque codec-specific tunable forwarded
    /// to the sink. Keys keep their literal form (e.g.
    /// `jpeg:dct-method`); the sink encoder picks up keys it
    /// recognises and silently ignores the rest, mirroring IM's
    /// "tolerant of irrelevant options" posture. Bare `-define KEY`
    /// (no `=VALUE`) sets the key to JSON `true`.
    Define { key: String, value: Option<String> },
    /// `-blur RxS`. Sigma defaults to `radius / 2.0` when the `xS`
    /// portion is omitted (matches IM's convention).
    Blur { radius: u32, sigma: f32 },
    /// `-edge R`. Radius ignored by the current Sobel impl but parsed
    /// for forward-compatibility.
    Edge { radius: u32 },
    /// `-colors N` paired with an optional `-dither` preceding it.
    /// When present, the output is paletted via
    /// [`oxideav_pixfmt`] before encoding.
    Colors { count: u32, dither: Dither },
    /// `-format FMT` — override the container/codec decision
    /// otherwise derived from the output extension.
    Format(String),
    /// `-quality N` — forwarded to the sink codec (e.g. JPEG quality,
    /// WebP quality). Silently dropped by codecs that don't honour it.
    Quality(u32),
    /// `-strip` — request that metadata (EXIF, XMP, ID3, etc.) be
    /// dropped on write.
    Strip,
    /// `-density N` — DPI for vector→raster conversion. PDF / SVG
    /// pages are measured in PostScript points (1/72 inch); a page at
    /// 300 DPI rasterises to `points × (300 / 72)` pixels per axis.
    /// Ignored on raster-only inputs.
    Density(u32),
    /// `-background COLOR` — canvas fill colour used by the
    /// rasteriser and by `-alpha remove`. CSS L3 named colours and
    /// `#hex` 3/4/6/8 forms accepted (same parser as `xc:`).
    Background([u8; 4]),
    /// `-alpha SUBCOMMAND` — see [`AlphaOp`].
    Alpha(AlphaOp),
    /// `-rotate N` — rotate by N degrees. Round-1 supports only
    /// multiples of 90 (90/180/270 and the negatives). Other angles
    /// are rejected by the args parser with a "only multiples of 90
    /// supported" message.
    Rotate { degrees: i32 },
    /// `-flip` — vertical flip (reverse row order).
    Flip,
    /// `-flop` — horizontal flip (reverse column order within each row).
    Flop,
    /// `-crop WxH+X+Y` — extract a `WxH` bounding box at offset `(X,Y)`.
    /// The args parser accepts the IM grammar; runtime checks that the
    /// bbox fits inside the input dimensions and errors cleanly when
    /// it doesn't.
    Crop { x: u32, y: u32, w: u32, h: u32 },
    /// `-negate` — per-pixel `out = 255 - in` on the colour channels;
    /// alpha (when present) is unchanged.
    Negate,
    /// `-sharpen RxS` — unsharp-mask sharpening with radius / sigma
    /// (amount defaults to 1.0 in the factory). Sigma defaults to
    /// `radius / 2.0` when only `R` is given.
    Sharpen { radius: u32, sigma: f32 },
    /// `-unsharp RxS+amount+threshold` — full unsharp-mask grammar.
    /// Amount and threshold each default to a sensible value when
    /// omitted (`amount = 1.0`, `threshold = 0`).
    Unsharp {
        radius: u32,
        sigma: f32,
        amount: f32,
        threshold: u8,
    },
    /// `-gamma G` — power-law gamma correction. `G > 0`.
    Gamma { value: f32 },
    /// `-brightness-contrast B[,C]` — brightness in `[-100..=100]`,
    /// contrast in `[-100..=100]` (both percent-of-range). Either
    /// argument may be omitted; the IM grammar tolerates `B`,
    /// `Bx`, `BxC`, `B,C`.
    BrightnessContrast { brightness: f32, contrast: f32 },
    /// `-contrast` (no value) — IM applies a tiny per-channel contrast
    /// step. Multiple `-contrast` flags accumulate; we collapse them
    /// into a single delta carried on the op (positive for `-contrast`,
    /// negative for `+contrast`, which IM uses for the inverse). The
    /// factory wires this to `BrightnessContrast::new(0, 5*delta)` —
    /// a 5-percent step matching IM's "single contrast bump" feel.
    Contrast { delta: i32 },
    /// `-sepia THRESHOLD%` — warm-tint mapping. `threshold` is a
    /// scalar in `0.0..=1.0` (IM expresses it as a percentage of the
    /// dynamic range; we accept either form on parse).
    Sepia { threshold: f32 },
    /// `-modulate B,S,H` — IM's HSL-style triplet. Each component is
    /// percent-of-base around 100 (so `100,100,0` is identity, `200`
    /// doubles, `0` zeros, etc.). The hue field is "hue offset
    /// percent" but image-filter takes degrees; we translate.
    Modulate {
        brightness: f32,
        saturation: f32,
        hue: f32,
    },
    /// `-level B/G/W` — input black point, gamma, white point. Black
    /// and white are 0..=255; gamma must be `> 0`.
    Level { black: u8, gamma: f32, white: u8 },
    /// `-normalize` — stretch the histogram to fill 0..=255.
    Normalize,
    /// `-threshold N%` — binarise: pixels below `value` map to 0,
    /// pixels at or above map to 255.
    Threshold { value: u8 },
    /// `-posterize N` — collapse to `N` levels per channel.
    Posterize { levels: u32 },
    /// `-solarize N%` — invert pixels above the threshold.
    Solarize { value: u8 },
    /// `-colorspace gray|grey|rgb|srgb` — round-1 covers only the
    /// grayscale conversion (everything else is treated as a
    /// pass-through, keeping the input colourspace).
    Colorspace(String),
    /// `-vignette R+S+X+Y` — Gaussian radial darkening centred at
    /// `(x, y)`. `radius`/`sigma` are in pixels (`sigma` defaults to
    /// `radius / 2.0` when omitted, matching IM); `x`/`y` are
    /// **normalised** image-relative offsets in `[0.0, 1.0]` (the
    /// image-filter factory takes them this way to stay
    /// resolution-independent), defaulting to `0.5 / 0.5` (image
    /// centre) when omitted.
    Vignette {
        radius: f32,
        sigma: f32,
        x: f32,
        y: f32,
    },
    /// `-colorize C[xC[xC]]/A%` — linear blend toward a target colour
    /// by `amount`. `color` is `[R, G, B, A]` (alpha defaults to
    /// `255`); `amount` is a unit scalar in `[0.0, 1.0]` (IM expresses
    /// it as a percentage of the dynamic range).
    Colorize { color: [u8; 4], amount: f32 },
    /// `-equalize` (no value) — per-channel histogram equalisation via
    /// CDF mapping.
    Equalize,
    /// `-auto-gamma` (no value) — auto-gamma: pick a per-channel gamma
    /// so the geometric mean lands at `0.5`.
    AutoGamma,
}

/// A printf-style multi-output template. Detected by the args parser
/// when the output filename contains `%[0-9]*d`. Used by Scene-shaped
/// inputs (PDF) to fan out one file per page.
///
/// Examples:
///   `page-%03d.png` → prefix=`page-`, width=3, suffix=`.png` →
///   `page-000.png`, `page-001.png`, …
///   `out%d.jpg`     → prefix=`out`, width=0, suffix=`.jpg` →
///   `out0.jpg`, `out1.jpg`, …
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrintfTemplate {
    pub prefix: String,
    pub width: u8,
    pub suffix: String,
}

impl PrintfTemplate {
    /// Render the template at the given index.
    pub fn expand(&self, n: usize) -> String {
        if self.width == 0 {
            format!("{}{}{}", self.prefix, n, self.suffix)
        } else {
            format!(
                "{}{:0>width$}{}",
                self.prefix,
                n,
                self.suffix,
                width = self.width as usize
            )
        }
    }
}

/// ImageMagick-style `[N]` / `[N-M]` page-selection suffix on an
/// input path. `input.pdf[0]` selects page 0; `input.pdf[2-5]`
/// selects pages 2, 3, 4, 5 (inclusive on both ends, like IM).
///
/// Today the selector only honours numeric pages — IM also supports
/// negative indices (`[-1]` = last page) and comma-separated lists
/// (`[0,2,4]`). Both are documented round-3 follow-ups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageSelector {
    /// Single page index.
    Single(usize),
    /// Inclusive range `start..=end`.
    Range(usize, usize),
}

impl PageSelector {
    /// Resolve the selector to a list of zero-based page indices,
    /// validated against `total_pages`.
    pub fn resolve(&self, total_pages: usize) -> Result<Vec<usize>, String> {
        match self {
            PageSelector::Single(n) => {
                if *n >= total_pages {
                    Err(format!(
                        "page index {n} out of range (input has {total_pages} page(s))"
                    ))
                } else {
                    Ok(vec![*n])
                }
            }
            PageSelector::Range(a, b) => {
                if a > b {
                    return Err(format!("page range [{a}-{b}] is inverted"));
                }
                if *b >= total_pages {
                    return Err(format!(
                        "page range [{a}-{b}] out of range (input has {total_pages} page(s))"
                    ));
                }
                Ok((*a..=*b).collect())
            }
        }
    }
}

/// `-stl-format` value. Selects which STL-on-disk flavour the encoder
/// emits. Default is [`Binary`](Self::Binary), matching the registry
/// factory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StlFormatChoice {
    /// 80-byte header + `uint32` triangle count + `N × 50`-byte triangle
    /// records. Compact and binary-stable.
    Binary,
    /// `solid … endsolid` ASCII grammar. Larger and floating-point
    /// formatted but human-readable.
    Ascii,
}

impl StlFormatChoice {
    /// Parse the value after `-stl-format`. Accepts ImageMagick-ish
    /// tokens case-insensitively (`binary`/`bin` → Binary, `ascii`/`text`
    /// → Ascii).
    pub fn parse(s: &str) -> Result<StlFormatChoice, String> {
        match s.to_ascii_lowercase().as_str() {
            "binary" | "bin" => Ok(StlFormatChoice::Binary),
            "ascii" | "text" => Ok(StlFormatChoice::Ascii),
            other => Err(format!(
                "convert: -stl-format: unknown flavour '{other}' (expected 'binary' or 'ascii')"
            )),
        }
    }
}

/// `-gltf-format` value. Selects the glTF on-disk container.
///
/// `JsonExternal` is documented but not yet implementable today
/// (the underlying `OutputFlavour::JsonExternal` is missing in
/// published `oxideav-gltf` 0.0.0); the args parser accepts the
/// token now and the encoder will reject it cleanly until the
/// upstream variant lands. See gltf-rN follow-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GltfFormatChoice {
    /// `.glb` self-contained binary container.
    Glb,
    /// `.gltf` JSON document with the binary buffer inlined as a
    /// base64 `data:application/octet-stream;base64,…` URI.
    JsonEmbedded,
    /// `.gltf` JSON document referencing a sidecar `.bin` file.
    /// Reserved — emits an "unsupported" error today (gltf-rN).
    JsonExternal,
}

impl GltfFormatChoice {
    /// Parse the value after `-gltf-format`. Accepts case-insensitive
    /// `glb`, `embedded`, `external`. `binary` is an alias for `glb`
    /// (mirrors how IM-style flags often accept short-form synonyms).
    pub fn parse(s: &str) -> Result<GltfFormatChoice, String> {
        match s.to_ascii_lowercase().as_str() {
            "glb" | "binary" => Ok(GltfFormatChoice::Glb),
            "embedded" | "json" | "json-embedded" => Ok(GltfFormatChoice::JsonEmbedded),
            "external" | "json-external" => Ok(GltfFormatChoice::JsonExternal),
            other => Err(format!(
                "convert: -gltf-format: unknown flavour '{other}' (expected 'glb', 'embedded', or 'external')"
            )),
        }
    }
}

/// `-render MODE` value. Selects which surface model the 3D→raster
/// renderer uses when the user pairs a 3D input (`.stl`/`.obj`/
/// `.gltf`/`.glb`/`.usdz`) with a raster output (`.png`/`.jpg`/
/// `.bmp`/`.webp`).
///
/// Default is [`Flat`](Self::Flat) — one constant colour per primitive
/// derived from the material's `base_color` (or a fallback grey when
/// no material is attached). [`Gouraud`](Self::Gouraud) and
/// [`Phong`](Self::Phong) add per-vertex / per-pixel lighting using
/// either the primitive's stored vertex normals or face normals
/// computed from the triangle vertices when none are provided.
/// [`NormalDebug`](Self::NormalDebug) and [`DepthDebug`](Self::DepthDebug)
/// are diagnostic visualisations that bypass the lighting equation
/// entirely — useful when normals look wrong, a Z-fighting glitch
/// shows up, or the camera framing seems off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mesh3DRenderMode {
    /// Flat-shaded triangles, one colour per primitive (no per-pixel
    /// lighting). Cheapest mode and the default.
    #[default]
    Flat,
    /// Wireframe — only triangle edges are drawn, interior pixels stay
    /// at the background. Same colour-per-primitive as [`Flat`](Self::Flat).
    Wireframe,
    /// Gouraud — light is evaluated at each vertex using the per-vertex
    /// normal, then the resulting RGB is bilinearly interpolated across
    /// the triangle. Requires (or computes) one normal per vertex.
    Gouraud,
    /// Phong — the per-vertex normal is interpolated across the
    /// triangle and the lighting equation is evaluated per pixel.
    /// Smoother than Gouraud at the cost of one normalise + dot per
    /// fragment.
    Phong,
    /// NormalDebug — paint each pixel with the world-space interpolated
    /// normal mapped into RGB (`[r,g,b] = (n + 1) / 2 * 255`). Light /
    /// camera / material settings are ignored. Useful for inspecting
    /// whether per-vertex normals were loaded correctly or whether the
    /// face-normal fallback is firing.
    NormalDebug,
    /// DepthDebug — paint each pixel a grayscale value derived from the
    /// interpolated NDC z (near = white, far = black). Light / camera
    /// orbit / material settings are ignored; useful for spotting
    /// Z-fighting and for picking sane near/far planes.
    DepthDebug,
}

impl Mesh3DRenderMode {
    /// Parse the value after `-render`. Accepts case-insensitive
    /// `flat` / `wireframe` / `gouraud` / `phong` / `normal-debug` /
    /// `depth-debug`. `wire` is a short alias for `wireframe`;
    /// `shaded` aliases `flat`; `normal` / `normals` alias
    /// `normal-debug`; `depth` / `z` alias `depth-debug`.
    pub fn parse(s: &str) -> Result<Mesh3DRenderMode, String> {
        match s.to_ascii_lowercase().as_str() {
            "flat" | "shaded" => Ok(Mesh3DRenderMode::Flat),
            "wireframe" | "wire" => Ok(Mesh3DRenderMode::Wireframe),
            "gouraud" => Ok(Mesh3DRenderMode::Gouraud),
            "phong" => Ok(Mesh3DRenderMode::Phong),
            "normal-debug" | "normaldebug" | "normal" | "normals" => {
                Ok(Mesh3DRenderMode::NormalDebug)
            }
            "depth-debug" | "depthdebug" | "depth" | "z" => Ok(Mesh3DRenderMode::DepthDebug),
            other => Err(format!(
                "convert: -render: unknown mode '{other}' (expected 'flat', 'wireframe', 'gouraud', 'phong', 'normal-debug', or 'depth-debug')"
            )),
        }
    }
}

/// `-projection MODE` value. Selects perspective vs. orthographic
/// projection for the 3D→raster renderer. Default is
/// [`Perspective`](Self::Perspective) — the IM convention for vector
/// rasterisation matches what most 3D viewers ship.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectionMode {
    /// Perspective projection — objects shrink with distance.
    #[default]
    Perspective,
    /// Orthographic projection — parallel rays, no foreshortening.
    /// Useful for technical drawings / part diagrams.
    Orthographic,
}

impl ProjectionMode {
    /// Parse the value after `-projection`. Case-insensitive synonyms:
    /// `perspective` / `persp` / `p`, `orthographic` / `ortho` / `o`.
    pub fn parse(s: &str) -> Result<ProjectionMode, String> {
        match s.to_ascii_lowercase().as_str() {
            "perspective" | "persp" | "p" => Ok(ProjectionMode::Perspective),
            "orthographic" | "ortho" | "o" => Ok(ProjectionMode::Orthographic),
            other => Err(format!(
                "convert: -projection: unknown mode '{other}' (expected 'perspective' or 'orthographic')"
            )),
        }
    }
}

/// `-light AZIMUTH,ELEVATION,INTENSITY` — directional light override
/// for the 3D→raster renderer.
///
/// `azimuth` and `elevation` are in degrees; `intensity` is a unit
/// scalar in `[0.0, ~]` multiplied into the diffuse term. The
/// renderer always also applies a small constant ambient term so
/// back-faces stay visible.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightSpec {
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    pub intensity: f32,
}

impl LightSpec {
    /// Default directional light: from the upper-right-front quadrant
    /// at unit intensity. Matches the renderer baseline so explicit
    /// `-light` is never required.
    pub fn default_light() -> Self {
        Self {
            azimuth_deg: 45.0,
            elevation_deg: 45.0,
            intensity: 1.0,
        }
    }

    /// Parse `AZIMUTH,ELEVATION,INTENSITY`. All three components are
    /// required; intensity is clamped to `>= 0.0`.
    pub fn parse(s: &str) -> Result<LightSpec, String> {
        let parts: Vec<&str> = s.split([',', 'x']).collect();
        if parts.len() != 3 {
            return Err(format!(
                "convert: -light: '{s}' must be 'AZIMUTH,ELEVATION,INTENSITY' (e.g. '45,30,1.0')"
            ));
        }
        let az: f32 = parts[0]
            .trim()
            .parse()
            .map_err(|_| format!("convert: -light: azimuth '{}' is not a number", parts[0]))?;
        let el: f32 = parts[1]
            .trim()
            .parse()
            .map_err(|_| format!("convert: -light: elevation '{}' is not a number", parts[1]))?;
        let intensity: f32 = parts[2]
            .trim()
            .parse()
            .map_err(|_| format!("convert: -light: intensity '{}' is not a number", parts[2]))?;
        if !intensity.is_finite() || intensity < 0.0 {
            return Err(format!(
                "convert: -light: intensity {intensity} must be >= 0.0 and finite"
            ));
        }
        Ok(LightSpec {
            azimuth_deg: az,
            elevation_deg: el,
            intensity,
        })
    }
}

/// `-camera ELEVATION,AZIMUTH,DISTANCE` — camera override for the
/// 3D→raster renderer.
///
/// `elevation` and `azimuth` are in degrees; `distance` is a positive
/// multiplier of the scene bounding-sphere radius (`1.0` ≈ scene
/// touches the framebuffer edge; the auto-framing default is `~1.2`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraSpec {
    pub elevation_deg: f32,
    pub azimuth_deg: f32,
    pub distance: f32,
}

impl CameraSpec {
    /// Parse `ELEVATION,AZIMUTH,DISTANCE`. All three components are
    /// required; distance must be `> 0`.
    pub fn parse(s: &str) -> Result<CameraSpec, String> {
        let parts: Vec<&str> = s.split([',', 'x']).collect();
        if parts.len() != 3 {
            return Err(format!(
                "convert: -camera: '{s}' must be 'ELEVATION,AZIMUTH,DISTANCE' (e.g. '20,30,1.5')"
            ));
        }
        let el: f32 = parts[0]
            .trim()
            .parse()
            .map_err(|_| format!("convert: -camera: elevation '{}' is not a number", parts[0]))?;
        let az: f32 = parts[1]
            .trim()
            .parse()
            .map_err(|_| format!("convert: -camera: azimuth '{}' is not a number", parts[1]))?;
        let dist: f32 = parts[2]
            .trim()
            .parse()
            .map_err(|_| format!("convert: -camera: distance '{}' is not a number", parts[2]))?;
        if !dist.is_finite() || dist <= 0.0 {
            return Err(format!(
                "convert: -camera: distance {dist} must be > 0.0 and finite"
            ));
        }
        Ok(CameraSpec {
            elevation_deg: el,
            azimuth_deg: az,
            distance: dist,
        })
    }
}

/// Per-format encoder option flags forwarded to the 3D side-channel.
///
/// All fields default to `None` meaning "use the format crate's
/// registry default" — i.e. the existing convert behaviour is
/// unchanged when no `-foo-format` flag is supplied.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh3DOptions {
    /// `-stl-format ascii|binary`. `None` → registry default (binary).
    pub stl_format: Option<StlFormatChoice>,
    /// `-gltf-format glb|embedded|external`. `None` → infer from
    /// output extension (`.glb` → Glb, `.gltf` → JsonEmbedded), which
    /// is exactly what the registry's by-extension lookup already does.
    pub gltf_format: Option<GltfFormatChoice>,
    /// `-render flat|wireframe|gouraud|phong`. `None` → default
    /// ([`Mesh3DRenderMode::Flat`]). Only consulted by the 3D→raster
    /// side-channel; ignored on every other input/output combination.
    pub render_mode: Option<Mesh3DRenderMode>,
    /// `-light AZIMUTH,ELEVATION,INTENSITY`. `None` → renderer
    /// default ([`LightSpec::default_light`]). Used by Gouraud / Phong
    /// shading; Flat/Wireframe ignore the light entirely.
    pub light: Option<LightSpec>,
    /// `-camera ELEVATION,AZIMUTH,DISTANCE`. `None` → bbox-fit
    /// auto-frame.
    pub camera: Option<CameraSpec>,
    /// `-projection orthographic|perspective`. `None` →
    /// [`ProjectionMode::Perspective`].
    pub projection: Option<ProjectionMode>,
    /// `-fov DEGREES` — vertical field of view for perspective
    /// projection. `None` → 60°. Ignored for orthographic projection.
    /// Must be in `(0, 180)`.
    pub fov_deg: Option<f32>,
    /// `-bg COLOR` — background fill for the render canvas. `None` →
    /// transparent black `[0, 0, 0, 0]`. Distinct from `-background`
    /// (which is the canvas-fill colour for `-alpha remove` and the
    /// PDF runner) so callers can keep the two paint colours separate.
    pub bg: Option<[u8; 4]>,
    /// `-aa N` — supersampling-anti-aliasing factor. The renderer
    /// rasterises at `N × output_w` by `N × output_h` then box-filters
    /// the framebuffer back down to the requested output. `None` or
    /// `Some(1)` → no supersampling (every pixel is point-sampled,
    /// matching the round-44 baseline). Values >= 2 trade memory and
    /// CPU for cleaner triangle edges; 2× and 4× are typical. Hard
    /// cap of 8 because at 8× a 1024² render is a 16M-pixel
    /// framebuffer + an 8M f32 z-buffer (~80 MB) and that's already
    /// further than most users want to push a software renderer.
    pub aa: Option<u32>,
}

/// The parsed result of one `oxideav convert` invocation.
#[derive(Clone, Debug)]
pub struct ConvertPlan {
    /// Input URI WITH any `[N]`/`[N-M]` page selector stripped.
    /// Currently exactly one — IM's multi-input stack is a documented
    /// follow-up.
    pub input: String,
    /// Page selector parsed from the input arg's `[…]` suffix, when
    /// present. `None` means "all pages" for Scene-shaped inputs and
    /// is ignored for raster inputs.
    pub input_pages: Option<PageSelector>,
    /// Chain of operations in source order.
    pub ops: Vec<Op>,
    /// Output path (literal). Empty string when `ping` is on and the
    /// caller omitted it.
    pub output: String,
    /// Parsed printf template if `output` contains `%[0-9]*d`. Used by
    /// the convert runner to fan out per-page when the input is a
    /// Scene-shaped source (PDF) and the output codec doesn't natively
    /// accept Scenes.
    pub output_template: Option<PrintfTemplate>,
    /// `-ping` mode — read only the headers, print one IM-format line
    /// per "image" (page / video stream) to stdout, skip pixel decode
    /// and any output write. Output positional becomes optional.
    pub ping: bool,
    /// `--probe` mode — decode the input far enough to extract
    /// structural metadata (page count, mesh count, sample rate, …),
    /// print a compact summary to stdout, and skip any output write.
    /// Mutually exclusive with an output positional: `--probe in.gltf`
    /// is the supported shape; `--probe in.gltf out.obj` errors at
    /// the parser. The summary defaults to a pretty-printed
    /// `key: value` block; pair with [`probe_json`](Self::probe_json)
    /// for a single-line JSON object instead.
    pub probe: bool,
    /// `--json` mode — emit machine-readable JSON instead of the
    /// pretty-printed summary. Today this only applies to `--probe`;
    /// passing `--json` without `--probe` is a parser-level error
    /// since there's nothing else for it to format.
    pub probe_json: bool,
    /// `--watch` mode (paired with `--probe`) — re-run the probe
    /// whenever the input file's mtime changes. Each re-probe emits a
    /// fresh summary; in JSON mode each summary is on its own line so
    /// `convert --probe --json --watch in.png | jq` is well-formed
    /// JSON-lines. Polls the input mtime once per second; the loop
    /// runs forever and exits only on Ctrl+C / signal. Useful for
    /// live-monitoring a render-in-progress.
    pub probe_watch: bool,
    /// Per-format encoder options for the 3D side-channel
    /// (`-stl-format`, `-gltf-format`, …). `Default` = "use the
    /// registry's default factory for each format" so callers who
    /// don't pass any of these flags see no behaviour change.
    pub mesh3d_options: Mesh3DOptions,
}
