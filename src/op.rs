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
}

/// The parsed result of one `oxideav convert` invocation.
#[derive(Clone, Debug)]
pub struct ConvertPlan {
    /// Input URI. Currently exactly one — IM's multi-input stack is a
    /// documented follow-up.
    pub input: String,
    /// Chain of operations in source order.
    pub ops: Vec<Op>,
    /// Output path.
    pub output: String,
}
