//! Filter ops planned on the filters' own declared layouts.
//!
//! Every `oxideav-image-filter` filter declares the pixel layouts it
//! takes ([`ImageFilter::accepted_pixel_formats`]) and the one it emits
//! for each ([`ImageFilter::output_pixel_format`]); a layout outside
//! the declared set is a typed refusal at run time, never narrowed.
//! The planner asks the same questions at plan time: each filter op is
//! paired with the typed filter it lowers to (built with the op's own
//! parameters where they change the declared set — a resize kernel, a
//! rotation angle) and the running layout is converted through the
//! gateway's views only when the next filter refuses it. A 16-bit PNG
//! through `-resize` stays 16-bit end to end; the same picture through
//! `-sepia` (packed 8-bit RGB only) is narrowed once, ahead of that
//! op, with a note.

use oxideav_core::PixelFormat;
use oxideav_image_filter::{
    AutoGamma, Blur, BrightnessContrast, Colorize, Crop, Edge, Equalize, Extent, Flip, Flop, Gamma,
    Grayscale, ImageFilter, Interpolation, Level, Modulate, Negate, Normalize, Posterize, Resize,
    Roll, Rotate, Sepia, Sharpen, Solarize, Threshold, Trim, Unsharp, Vignette,
};
use oxideav_pixfmt::FormatInfo;

use crate::op::Op;

/// What an op does to the running layout of the filter chain.
pub enum LayoutStep {
    /// A filter with declared layouts: the op lowers to this filter.
    Filter(Box<dyn ImageFilter>),
    /// A conversion node the op lowers to (`-colors` quantises to a
    /// palette): the layout becomes this one.
    Convert(PixelFormat),
    /// A sink-side or no-op setting: the layout is unchanged.
    None,
}

/// The typed filter `op` lowers to on the pipeline route (the same
/// factory `plan_to_job` names, with the parameters that shape its
/// declared layouts), or the layout change of a conversion op.
pub fn layout_step(op: &Op) -> LayoutStep {
    fn f(filter: impl ImageFilter + 'static) -> LayoutStep {
        LayoutStep::Filter(Box::new(filter))
    }
    match op {
        Op::Resize { width, height, .. } | Op::Thumbnail { width, height, .. } => {
            f(Resize::new(*width, *height).with_interpolation(Interpolation::Bilinear))
        }
        Op::Blur { radius, sigma } => f(Blur::new(*radius).with_sigma(*sigma)),
        Op::Edge { .. } => f(Edge::new()),
        Op::Colors { .. } => LayoutStep::Convert(PixelFormat::Pal8),
        Op::Rotate { degrees } => f(Rotate::new(*degrees as f32)),
        Op::Flip => f(Flip::new()),
        Op::Flop => f(Flop::new()),
        Op::Crop { x, y, w, h } => f(Crop::new(*x, *y, *w, *h)),
        Op::Extent { width, height, .. } => f(Extent::new(*width, *height)),
        Op::Negate => f(Negate::new()),
        Op::Sharpen { radius, sigma } => f(Sharpen::new(*radius, *sigma)),
        Op::Unsharp {
            radius,
            sigma,
            amount,
            threshold,
        } => f(Unsharp::new(*radius, *sigma, *amount, *threshold)),
        Op::Gamma { value } => f(Gamma::new(*value)),
        Op::BrightnessContrast {
            brightness,
            contrast,
        } => f(BrightnessContrast::new(*brightness, *contrast)),
        Op::Contrast { delta } => f(BrightnessContrast::new(0.0, *delta as f32 * 5.0)),
        Op::Sepia { threshold } => f(Sepia::new(*threshold)),
        Op::Modulate {
            brightness,
            saturation,
            hue,
        } => f(Modulate::new(
            *brightness,
            *saturation,
            (*hue - 100.0) * 1.8,
        )),
        Op::Level {
            black,
            gamma,
            white,
        } => f(Level::new(*black, *white, *gamma)),
        Op::Normalize => f(Normalize::default()),
        Op::Threshold { value } => f(Threshold::new(*value)),
        Op::Posterize { levels } => f(Posterize::new(*levels)),
        Op::Solarize { value } => f(Solarize::new(*value)),
        Op::Colorspace(cs)
            if cs.eq_ignore_ascii_case("gray") || cs.eq_ignore_ascii_case("grey") =>
        {
            f(Grayscale::new().with_preserve_alpha(true))
        }
        Op::Vignette {
            radius,
            sigma,
            x,
            y,
        } => f(Vignette::new(*x, *y, *radius, *sigma)),
        Op::Colorize { color, amount } => f(Colorize::new(*color, *amount)),
        Op::Equalize => f(Equalize::new()),
        Op::AutoGamma => f(AutoGamma::new()),
        Op::Trim { fuzz } => f(Trim::new().with_fuzz(*fuzz)),
        Op::Roll { dx, dy } => f(Roll::new(*dx, *dy)),
        Op::Colorspace(_)
        | Op::Define { .. }
        | Op::Format(_)
        | Op::Quality(_)
        | Op::Strip
        | Op::Depth(_)
        | Op::EncoderOpt { .. }
        | Op::Threads(_)
        | Op::Density(_)
        | Op::Background(_)
        | Op::Alpha(_)
        | Op::SetMetadata { .. } => LayoutStep::None,
    }
}

/// One conversion the planner inserted ahead of a filter op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Narrowing {
    /// Index of the op (in the plan's op list) the conversion precedes.
    pub before_op: usize,
    /// The layout the frames had.
    pub from: PixelFormat,
    /// The layout the filter takes instead.
    pub to: PixelFormat,
}

/// Walk `ops` from `src`, the layout the source decodes to, and
/// return the layout the chain emits plus every conversion a filter's
/// declared set forces. A filter that declares no layouts (an
/// implementation outside `oxideav-image-filter`) is assumed to take
/// anything.
pub fn plan_layouts(src: PixelFormat, ops: &[Op]) -> (PixelFormat, Vec<Narrowing>) {
    let mut cur = src;
    let mut narrowings = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        match layout_step(op) {
            LayoutStep::Filter(filter) => {
                let accepted = filter.accepted_pixel_formats();
                if !accepted.is_empty() && !accepted.contains(&cur) {
                    if let Some(to) = filter_input_layout_for(cur, &accepted) {
                        narrowings.push(Narrowing {
                            before_op: i,
                            from: cur,
                            to,
                        });
                        cur = to;
                    }
                }
                if let Some(out) = filter.output_pixel_format(cur) {
                    cur = out;
                }
            }
            LayoutStep::Convert(to) => cur = to,
            LayoutStep::None => {}
        }
    }
    (cur, narrowings)
}

/// The layout to hand a filter that refuses `src`, chosen from its
/// declared `accepted` set: the first candidate that keeps what the
/// source carries (grey stays grey, alpha and palette transparency
/// keep an alpha channel, a deep source stays deep where the filter
/// has a deep layout) and that `oxideav-pixfmt` converts to. `None`
/// when no declared layout is reachable (the filter then reports its
/// refusal at run time).
pub fn filter_input_layout_for(src: PixelFormat, accepted: &[PixelFormat]) -> Option<PixelFormat> {
    use PixelFormat::*;
    let info = FormatInfo::of(src);
    let deep = info.bit_depth > 8;
    let gray = matches!(
        src,
        MonoBlack | MonoWhite | Gray8 | Gray10Le | Gray12Le | Gray16Le | GrayF32Le | Ya8 | Ya16Le
    );
    let alpha = info.has_alpha || info.is_palette || matches!(src, Ya8 | Ya16Le);
    let candidates: &[PixelFormat] = match (gray, alpha, deep) {
        (true, true, true) => &[Ya16Le, Ya8, Rgba64Le, Rgba, Gray16Le, Gray8, Rgb48Le, Rgb24],
        (true, true, false) => &[Ya8, Rgba, Gray8, Rgb24],
        (true, false, true) => &[Gray16Le, Gray8, Rgb48Le, Rgba64Le, Rgb24, Rgba],
        (true, false, false) => &[Gray8, Rgb24, Rgba],
        (false, true, true) => &[Rgba64Le, Rgba, Rgb48Le, Rgb24],
        (false, true, false) => &[Rgba, Rgb24],
        (false, false, true) => &[Rgb48Le, Rgb24, Rgba],
        (false, false, false) => &[Rgb24, Rgba],
    };
    candidates
        .iter()
        .copied()
        .find(|&c| c != src && accepted.contains(&c) && oxideav_pixfmt::supports(src, c))
        .or_else(|| {
            // Nothing from the preferred list: any declared layout the
            // source converts to, the filter's own order.
            accepted
                .iter()
                .copied()
                .find(|&c| c != src && oxideav_pixfmt::supports(src, c))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::ResizeMode;

    fn resize() -> Op {
        Op::Resize {
            width: 4,
            height: 4,
            mode: ResizeMode::Force,
        }
    }

    #[test]
    fn resize_keeps_a_deep_layout_and_sepia_narrows_it() {
        let (out, n) = plan_layouts(PixelFormat::Rgb48Le, &[resize()]);
        assert_eq!(out, PixelFormat::Rgb48Le);
        assert!(n.is_empty());
        let (out, n) = plan_layouts(PixelFormat::Rgb48Le, &[Op::Sepia { threshold: 80.0 }]);
        assert_eq!(out, PixelFormat::Rgb24);
        assert_eq!(
            n,
            vec![Narrowing {
                before_op: 0,
                from: PixelFormat::Rgb48Le,
                to: PixelFormat::Rgb24
            }]
        );
    }

    #[test]
    fn one_conversion_ahead_of_the_first_refusing_op_only() {
        let ops = [
            resize(),
            Op::Sepia { threshold: 80.0 },
            Op::Negate,
            resize(),
        ];
        let (out, n) = plan_layouts(PixelFormat::Gray16Le, &ops);
        // Gray16 resizes natively; sepia takes no 16-bit word, so the
        // grey picture goes to Gray8 (its declared 8-bit grey route)
        // there and stays for the rest.
        assert_eq!(out, PixelFormat::Gray8);
        assert_eq!(n.len(), 1);
        assert_eq!(
            (n[0].before_op, n[0].from, n[0].to),
            (1, PixelFormat::Gray16Le, PixelFormat::Gray8)
        );
    }

    #[test]
    fn classic_filters_take_the_deep_grey_through_gray8() {
        let (out, n) = plan_layouts(PixelFormat::Gray16Le, &[Op::Negate]);
        assert_eq!(out, PixelFormat::Gray8);
        assert_eq!(n[0].to, PixelFormat::Gray8);
    }

    #[test]
    fn eight_bit_layouts_are_never_converted_ahead_of_a_filter() {
        // Every filter declares the 8-bit layouts it takes through its
        // own routes (the palette route for a pointwise filter, the
        // packed-RGB hop otherwise): the planner inserts nothing and
        // the chain emits what the filter declares.
        let (out, n) = plan_layouts(PixelFormat::Pal8, &[Op::Sepia { threshold: 80.0 }]);
        assert_eq!(out, PixelFormat::Pal8, "pointwise: the palette route");
        assert!(n.is_empty());
        let (out, n) = plan_layouts(
            PixelFormat::Pal8,
            &[Op::Blur {
                radius: 1,
                sigma: 0.5,
            }],
        );
        assert_eq!(out, PixelFormat::Rgba, "not pointwise: the hop's layout");
        assert!(n.is_empty());
        let (out, n) = plan_layouts(PixelFormat::YuvJ420P, &[Op::Negate]);
        assert_eq!(out, PixelFormat::YuvJ420P);
        assert!(n.is_empty());
        let (_, n) = plan_layouts(PixelFormat::Yuv420P, &[Op::Negate]);
        assert!(n.is_empty(), "the classic set takes Yuv420P natively");
    }

    /// The README's table: which ops keep a deep layout and which
    /// narrow it (printed under `--nocapture`, rows pinned below).
    #[test]
    fn deep_layout_table() {
        use crate::op::Dither;
        let ops: Vec<(&str, Op)> = vec![
            ("-resize / -thumbnail", resize()),
            (
                "-crop",
                Op::Crop {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2,
                },
            ),
            ("-flip / -flop", Op::Flip),
            ("-roll", Op::Roll { dx: 1, dy: 1 }),
            ("-rotate 90", Op::Rotate { degrees: 90 }),
            ("-rotate 30", Op::Rotate { degrees: 30 }),
            (
                "-extent",
                Op::Extent {
                    width: 4,
                    height: 4,
                    x: 0,
                    y: 0,
                    bg: [0; 4],
                },
            ),
            ("-trim", Op::Trim { fuzz: 0 }),
            (
                "-blur / -sharpen / -unsharp",
                Op::Blur {
                    radius: 1,
                    sigma: 0.5,
                },
            ),
            ("-negate / -gamma / -level / -threshold", Op::Negate),
            ("-normalize / -equalize / -auto-gamma", Op::Normalize),
            ("-posterize / -solarize", Op::Posterize { levels: 4 }),
            (
                "-brightness-contrast / -contrast",
                Op::Contrast { delta: 1 },
            ),
            ("-edge", Op::Edge { radius: 1 }),
            ("-sepia / -modulate / -tint", Op::Sepia { threshold: 80.0 }),
            (
                "-colorize / -vignette",
                Op::Colorize {
                    color: [1, 2, 3, 255],
                    amount: 0.5,
                },
            ),
            ("-colorspace gray", Op::Colorspace("gray".into())),
            (
                "-colors",
                Op::Colors {
                    count: 16,
                    dither: Dither::None,
                },
            ),
        ];
        let sources = [
            PixelFormat::Rgb48Le,
            PixelFormat::Rgba64Le,
            PixelFormat::Gray16Le,
            PixelFormat::Yuv420P10Le,
        ];
        let mut rows = Vec::new();
        for (flag, op) in &ops {
            let mut cells = Vec::new();
            for &src in &sources {
                let (out, n) = plan_layouts(src, std::slice::from_ref(op));
                cells.push(match n.first() {
                    Some(n) => format!("{:?} → {:?}", n.to, out),
                    None if out == src => "kept".to_string(),
                    None => format!("→ {out:?}"),
                });
            }
            println!("| `{flag}` | {} |", cells.join(" | "));
            rows.push((*flag, cells));
        }
        let keeps = |flag: &str| {
            rows.iter()
                .find(|(f, _)| *f == flag)
                .map(|(_, c)| c.iter().all(|c| c == "kept"))
                .unwrap()
        };
        assert!(keeps("-resize / -thumbnail"));
        assert!(keeps("-crop"));
        assert!(keeps("-flip / -flop"));
        assert!(keeps("-roll"));
        assert!(keeps("-rotate 90"));
        assert!(!keeps("-sepia / -modulate / -tint"));
        assert!(!keeps("-blur / -sharpen / -unsharp"));
    }

    #[test]
    fn edge_emits_gray8_and_colors_quantises_to_pal8() {
        let (out, _) = plan_layouts(PixelFormat::Rgb24, &[Op::Edge { radius: 1 }]);
        assert_eq!(out, PixelFormat::Gray8);
        let (out, n) = plan_layouts(
            PixelFormat::Rgb24,
            &[Op::Colors {
                count: 16,
                dither: crate::op::Dither::None,
            }],
        );
        assert_eq!(out, PixelFormat::Pal8);
        assert!(n.is_empty());
    }

    #[test]
    fn sink_side_ops_leave_the_layout_alone() {
        let ops = [Op::Quality(80), Op::Strip, Op::Threads(2), Op::Depth(16)];
        let (out, n) = plan_layouts(PixelFormat::Yuv420P10Le, &ops);
        assert_eq!(out, PixelFormat::Yuv420P10Le);
        assert!(n.is_empty());
    }
}
