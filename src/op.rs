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
