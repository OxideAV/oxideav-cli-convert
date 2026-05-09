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

/// One convert operation.
///
/// Operations apply in source order — same as `imagemagick convert`,
/// even though we don't yet support IM's stack-reset semantics.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// `-resize WxH[!]`. `bang = true` means the `!` was present:
    /// force exact dimensions without preserving aspect ratio.
    Resize { width: u32, height: u32, bang: bool },
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
}
