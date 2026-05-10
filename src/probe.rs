//! `--probe` runner — decode the input far enough to extract
//! structural metadata (page count, mesh count, sample rate, …),
//! print a compact summary to stdout, and skip any output write.
//!
//! Output flavours:
//!
//! * Pretty-printed (default): a list of `key: value` lines, with
//!   nested groups (per-page dimensions, per-stream codec) indented.
//! * JSON (`--json`): a single-line object with the same fields,
//!   suitable for machine consumption.
//!
//! ## Routing
//!
//! The probe walks the same input-shape decision tree as
//! [`crate::run`]:
//!
//! 1. PDF inputs → [`probe_pdf`] via `oxideav_pdf::read_pdf_to_scene`.
//! 2. 3D inputs (mesh3d feature on) → [`probe_mesh3d`] via the
//!    [`oxideav_mesh3d::Mesh3DRegistry`] populated by
//!    `oxideav_meta::populate_mesh3d_registry`.
//! 3. SVG inputs → [`probe_svg`] via `oxideav_svg::parse_svg` (a
//!    second Scene-shaped input class).
//! 4. Everything else → [`probe_container`], same path the `-ping`
//!    runner uses (raster / audio / video).
//!
//! Per-input fields are best-effort; when a producer crate doesn't
//! surface a particular field (e.g. PDF embedded font count without a
//! resource walker) the probe leaves it `null` in JSON / `unknown` in
//! pretty form rather than guessing.

use std::fmt::Write as _;
use std::fs;

use oxideav_core::vector::{Group, Node, VectorFrame};
use oxideav_core::{Error, MediaType, PixelFormat, Result, RuntimeContext, SourceOutput};
use oxideav_pdf::read_pdf_to_scene;
use oxideav_scene::Page;
use oxideav_svg::parse_svg;

use crate::op::ConvertPlan;
use crate::pdf_runner::is_pdf_input;

/// One field on the probe summary. Kept as a sum type so the
/// pretty-print and JSON formatters share the same input.
#[derive(Clone, Debug)]
enum Field {
    /// `key: value` (string).
    Str(&'static str, String),
    /// `key: value` (integer).
    Int(&'static str, i64),
    /// `key: value` (float; rendered with 2 decimal places by the
    /// pretty formatter and as a JSON number with the `Display`
    /// impl's default precision).
    Float(&'static str, f64),
    /// `key: value` (string array).
    StrList(&'static str, Vec<String>),
    /// `key: value` (group of nested fields).
    Group(&'static str, Vec<Field>),
    /// `key: value` (array of grouped fields — one group per array
    /// element, keyed by the group's own field set).
    GroupList(&'static str, Vec<Vec<Field>>),
}

/// A complete probe summary.
#[derive(Clone, Debug, Default)]
struct Summary {
    /// Top-level fields. Common fields first (`path`, `kind`); then
    /// kind-specific fields.
    fields: Vec<Field>,
}

impl Summary {
    fn push(&mut self, field: Field) {
        self.fields.push(field);
    }

    /// Pretty-printed text summary — newline-separated `key: value`
    /// lines, two-space indent per nesting level.
    fn to_pretty(&self) -> String {
        let mut out = String::new();
        for f in &self.fields {
            write_field_pretty(&mut out, f, 0);
        }
        out
    }

    /// JSON object on a single line. The serialisation is hand-rolled
    /// (instead of `serde_json::to_string`) so the field order matches
    /// the pretty-print version exactly — useful when the caller is
    /// diffing two probes for regressions.
    fn to_json(&self) -> String {
        let mut out = String::from("{");
        for (i, f) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_field_json(&mut out, f);
        }
        out.push('}');
        out
    }
}

fn write_field_pretty(out: &mut String, field: &Field, depth: usize) {
    let indent = "  ".repeat(depth);
    match field {
        Field::Str(k, v) => {
            let _ = writeln!(out, "{indent}{k}: {v}");
        }
        Field::Int(k, v) => {
            let _ = writeln!(out, "{indent}{k}: {v}");
        }
        Field::Float(k, v) => {
            let _ = writeln!(out, "{indent}{k}: {v:.2}");
        }
        Field::StrList(k, items) => {
            let _ = writeln!(out, "{indent}{k}: [{}]", items.join(", "));
        }
        Field::Group(k, sub) => {
            let _ = writeln!(out, "{indent}{k}:");
            for f in sub {
                write_field_pretty(out, f, depth + 1);
            }
        }
        Field::GroupList(k, items) => {
            let _ = writeln!(out, "{indent}{k}:");
            for (i, sub) in items.iter().enumerate() {
                let _ = writeln!(out, "{indent}  - #{i}:");
                for f in sub {
                    write_field_pretty(out, f, depth + 2);
                }
            }
        }
    }
}

fn write_field_json(out: &mut String, field: &Field) {
    match field {
        Field::Str(k, v) => {
            out.push_str(&json_string(k));
            out.push(':');
            out.push_str(&json_string(v));
        }
        Field::Int(k, v) => {
            out.push_str(&json_string(k));
            out.push(':');
            let _ = write!(out, "{v}");
        }
        Field::Float(k, v) => {
            out.push_str(&json_string(k));
            out.push(':');
            // Use the Display impl which avoids trailing zeros.
            // f64 NaN / Inf would be invalid JSON; we don't produce
            // those (every probe value is a measured count or
            // dimension), but be explicit anyway.
            if v.is_finite() {
                let _ = write!(out, "{v}");
            } else {
                out.push_str("null");
            }
        }
        Field::StrList(k, items) => {
            out.push_str(&json_string(k));
            out.push(':');
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(item));
            }
            out.push(']');
        }
        Field::Group(k, sub) => {
            out.push_str(&json_string(k));
            out.push(':');
            out.push('{');
            for (i, f) in sub.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_field_json(out, f);
            }
            out.push('}');
        }
        Field::GroupList(k, items) => {
            out.push_str(&json_string(k));
            out.push(':');
            out.push('[');
            for (i, sub) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('{');
                for (j, f) in sub.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    write_field_json(out, f);
                }
                out.push('}');
            }
            out.push(']');
        }
    }
}

/// Encode a Rust string as a JSON string literal. Escapes the
/// minimum set required by RFC 8259 §7 — quotes, backslash, and the
/// C0 control bytes — without pulling in a JSON dep.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Print a probe summary for `plan.input` to stdout. Routing mirrors
/// [`crate::run`] — PDF / 3D side-channels, then container fallback.
pub fn run(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<()> {
    let summary = build_summary(plan, ctx)?;
    let line = if plan.probe_json {
        summary.to_json()
    } else {
        summary.to_pretty()
    };
    if plan.probe_json {
        println!("{line}");
    } else {
        // Pretty form already trails newlines per row; print as-is
        // (no extra newline) so `convert --probe in.png | wc -l` is
        // the row count.
        print!("{line}");
    }
    Ok(())
}

fn build_summary(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<Summary> {
    if is_pdf_input(&plan.input) {
        return probe_pdf(plan);
    }
    #[cfg(feature = "mesh3d")]
    if crate::mesh3d_runner::is_mesh3d_input(&plan.input) {
        return probe_mesh3d(plan);
    }
    if has_extension(&plan.input, &["svg"]) {
        return probe_svg(plan);
    }
    probe_container(plan, ctx)
}

/// Best-effort extension check — same rule as the routing helpers in
/// `mesh3d_runner` / `pdf_runner`.
fn has_extension(path: &str, exts: &[&str]) -> bool {
    let lc = path.to_ascii_lowercase();
    exts.iter().any(|e| lc.ends_with(&format!(".{e}")))
}

// ─────────────────────────── PDF probe ───────────────────────────

fn probe_pdf(plan: &ConvertPlan) -> Result<Summary> {
    let bytes = fs::read(&plan.input)
        .map_err(|e| Error::invalid(format!("convert: failed to read {}: {e}", plan.input)))?;
    let scene = read_pdf_to_scene(&bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to parse PDF: {e:?}")))?;
    let pages = scene.pages.as_deref().unwrap_or(&[]);

    let mut s = Summary::default();
    s.push(Field::Str("path", plan.input.clone()));
    s.push(Field::Str("kind", "pdf".into()));
    s.push(Field::Int("file_size_bytes", bytes.len() as i64));
    s.push(Field::Int("page_count", pages.len() as i64));

    // Per-page dimensions (PostScript points). Capped at 32 entries so
    // a 10 000-page PDF doesn't blow up the summary; the count is
    // always available via `page_count`.
    let cap = 32usize;
    let pages_for_summary: Vec<Vec<Field>> = pages
        .iter()
        .take(cap)
        .map(|p| {
            vec![
                Field::Float("width_pt", p.width as f64),
                Field::Float("height_pt", p.height as f64),
                Field::Int("orientation_deg", p.orientation as i64),
            ]
        })
        .collect();
    s.push(Field::GroupList("pages", pages_for_summary));
    if pages.len() > cap {
        s.push(Field::Int("pages_truncated_at", cap as i64));
    }

    // Walk every page's VectorFrame and tally embedded raster images +
    // groups (a rough proxy for nesting depth). Embedded font count is
    // a follow-up: the PDF reader doesn't surface a /Font resource
    // census today, and counting unique `font_name`s from
    // PdfTextExtraction would over-count synthetic encoding splits.
    let mut image_total = 0usize;
    for page in pages {
        image_total += count_images_in_frame(&page.content);
    }
    s.push(Field::Int("embedded_image_count", image_total as i64));
    // Document-wide embedded font count is not surfaced by
    // `read_pdf_to_scene` today; flag the gap rather than report 0
    // which would imply "no fonts".
    s.push(Field::Str("embedded_font_count", "unknown".into()));

    Ok(s)
}

/// Count `Node::Image` occurrences anywhere in the vector tree. Used
/// by the PDF probe to surface "how many embedded raster images is
/// this page?". Cheap walk — every node visited once.
fn count_images_in_frame(frame: &VectorFrame) -> usize {
    let mut n = 0;
    count_images_in_group(&frame.root, &mut n);
    n
}

fn count_images_in_group(group: &Group, out: &mut usize) {
    for child in &group.children {
        count_images_in_node(child, out);
    }
}

fn count_images_in_node(node: &Node, out: &mut usize) {
    match node {
        Node::Image(_) => *out += 1,
        Node::Group(g) => count_images_in_group(g, out),
        Node::SoftMask { mask, content, .. } => {
            count_images_in_node(mask, out);
            count_images_in_node(content, out);
        }
        Node::Path(_) => {}
        // `Node` is `#[non_exhaustive]` upstream — any future variant
        // (text-on-path, lottie animation node, …) just doesn't count
        // as an embedded raster, which is the conservative answer.
        _ => {}
    }
}

// ─────────────────────────── SVG probe ───────────────────────────

fn probe_svg(plan: &ConvertPlan) -> Result<Summary> {
    let bytes = fs::read(&plan.input)
        .map_err(|e| Error::invalid(format!("convert: failed to read {}: {e}", plan.input)))?;
    let frame = parse_svg(&bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to parse SVG: {e:?}")))?;

    let mut s = Summary::default();
    s.push(Field::Str("path", plan.input.clone()));
    s.push(Field::Str("kind", "svg".into()));
    s.push(Field::Int("file_size_bytes", bytes.len() as i64));
    // SVG is single-page by definition.
    s.push(Field::Int("page_count", 1));

    let pseudo_page = vec![Field::GroupList(
        "pages",
        vec![vec![
            Field::Float("width_pt", frame.width as f64),
            Field::Float("height_pt", frame.height as f64),
            Field::Int("orientation_deg", 0),
        ]],
    )];
    s.fields.extend(pseudo_page);

    let mut image_total = 0usize;
    count_images_in_group(&frame.root, &mut image_total);
    s.push(Field::Int("embedded_image_count", image_total as i64));
    s.push(Field::Str("embedded_font_count", "unknown".into()));
    Ok(s)
}

// ─────────────────────────── 3D probe ───────────────────────────

#[cfg(feature = "mesh3d")]
fn probe_mesh3d(plan: &ConvertPlan) -> Result<Summary> {
    use oxideav_mesh3d::{Mesh3DRegistry, Topology};

    let in_ext = ext_of(&plan.input)
        .ok_or_else(|| {
            Error::invalid(format!(
                "convert: input '{}' has no extension — cannot pick a 3D decoder",
                plan.input
            ))
        })?
        .to_ascii_lowercase();

    let bytes = fs::read(&plan.input)
        .map_err(|e| Error::invalid(format!("convert: failed to read {}: {e}", plan.input)))?;
    let mut registry = Mesh3DRegistry::new();
    oxideav_meta::populate_mesh3d_registry(&mut registry);
    let mut decoder = registry.decoder_for_extension(&in_ext).ok_or_else(|| {
        Error::unsupported(format!(
            "convert: no 3D decoder registered for input extension '.{in_ext}'"
        ))
    })?;
    let scene = decoder.decode(&bytes)?;

    // Tally primitive / vertex counts across every primitive on every
    // mesh. Vertex count is `positions.len()` — even when an index
    // buffer is present, the buffer length describes the *index* count
    // not the unique-vertex count; the spec model (matching glTF) says
    // unique vertices are `positions.len()`.
    let mut prim_total = 0usize;
    let mut vert_total = 0usize;
    let mut tri_total = 0usize;
    for mesh in &scene.meshes {
        for prim in &mesh.primitives {
            prim_total += 1;
            vert_total += prim.positions.len();
            // Use the spec helper for triangle topologies; non-triangle
            // primitives (Points / Lines) report 0, which is what we
            // want.
            tri_total += prim.triangle_count();
        }
    }

    let bbox = compute_bbox(&scene);

    let mut s = Summary::default();
    s.push(Field::Str("path", plan.input.clone()));
    s.push(Field::Str("kind", "mesh3d".into()));
    s.push(Field::Str("format", in_ext));
    s.push(Field::Int("file_size_bytes", bytes.len() as i64));
    s.push(Field::Int("mesh_count", scene.meshes.len() as i64));
    s.push(Field::Int("primitive_count", prim_total as i64));
    s.push(Field::Int("vertex_count", vert_total as i64));
    s.push(Field::Int("triangle_count", tri_total as i64));
    s.push(Field::Int("material_count", scene.materials.len() as i64));
    s.push(Field::Int("texture_count", scene.textures.len() as i64));
    s.push(Field::Int("animation_count", scene.animations.len() as i64));
    s.push(Field::Int("skin_count", scene.skins.len() as i64));
    s.push(Field::Int("node_count", scene.nodes.len() as i64));
    s.push(Field::Int("root_count", scene.roots.len() as i64));
    // The Scene3D model is single-document (no separate "scene"
    // collection like glTF's `scenes` array); surface the implicit
    // index-zero so the field is present and answerable.
    s.push(Field::Int("active_scene_index", 0));
    s.push(Field::Str("scene_name", "(unnamed)".into()));

    // Per-topology tally — useful when "vertex_count: 9" needs context
    // (3 line segments? 3 triangles?). Build a small stable list.
    let mut topo_counts: Vec<(Topology, usize)> = Vec::new();
    for mesh in &scene.meshes {
        for prim in &mesh.primitives {
            if let Some(entry) = topo_counts.iter_mut().find(|(t, _)| *t == prim.topology) {
                entry.1 += 1;
            } else {
                topo_counts.push((prim.topology, 1));
            }
        }
    }
    let topo_strs: Vec<String> = topo_counts
        .iter()
        .map(|(t, n)| format!("{}={n}", topology_label(*t)))
        .collect();
    s.push(Field::StrList("topologies", topo_strs));

    if let Some(b) = bbox {
        s.push(Field::Group(
            "bounding_box",
            vec![
                Field::Float("min_x", b.0[0] as f64),
                Field::Float("min_y", b.0[1] as f64),
                Field::Float("min_z", b.0[2] as f64),
                Field::Float("max_x", b.1[0] as f64),
                Field::Float("max_y", b.1[1] as f64),
                Field::Float("max_z", b.1[2] as f64),
            ],
        ));
    } else {
        s.push(Field::Str("bounding_box", "empty".into()));
    }

    Ok(s)
}

#[cfg(feature = "mesh3d")]
fn topology_label(t: oxideav_mesh3d::Topology) -> &'static str {
    use oxideav_mesh3d::Topology::*;
    match t {
        Triangles => "Triangles",
        TriangleStrip => "TriangleStrip",
        TriangleFan => "TriangleFan",
        Lines => "Lines",
        LineStrip => "LineStrip",
        LineLoop => "LineLoop",
        Points => "Points",
    }
}

/// Compute the AABB of every position buffer across every mesh's
/// every primitive. `None` when the scene has no positions at all.
/// Positions are in the scene's own coordinate space (no node
/// transforms applied) — matching what the spec model exposes.
#[cfg(feature = "mesh3d")]
fn compute_bbox(scene: &oxideav_mesh3d::Scene3D) -> Option<([f32; 3], [f32; 3])> {
    let mut bbox: Option<([f32; 3], [f32; 3])> = None;
    for mesh in &scene.meshes {
        for prim in &mesh.primitives {
            for p in &prim.positions {
                bbox = Some(match bbox {
                    None => (*p, *p),
                    Some((mn, mx)) => (
                        [mn[0].min(p[0]), mn[1].min(p[1]), mn[2].min(p[2])],
                        [mx[0].max(p[0]), mx[1].max(p[1]), mx[2].max(p[2])],
                    ),
                });
            }
        }
    }
    bbox
}

#[cfg(feature = "mesh3d")]
fn ext_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

// ─────────────────────────── Container probe ───────────────────────────

fn probe_container(plan: &ConvertPlan, ctx: &RuntimeContext) -> Result<Summary> {
    let raw = match ctx.sources.open(&plan.input)? {
        SourceOutput::Bytes(b) => b,
        SourceOutput::Packets(_) => {
            return Err(Error::unsupported(format!(
                "convert --probe: {}: packet-shape source not supported",
                plan.input
            )));
        }
        SourceOutput::Frames(_) => {
            return Err(Error::unsupported(format!(
                "convert --probe: {}: frame-shape source not supported",
                plan.input
            )));
        }
    };
    let file_size = fs::metadata(&plan.input).map(|m| m.len()).unwrap_or(0);
    let mut handle: Box<dyn oxideav_core::ReadSeek> = Box::new(raw);
    let ext = ext_from_uri(&plan.input);
    let format = ctx
        .containers
        .probe_input(&mut *handle, ext.as_deref())
        .map_err(|e| Error::invalid(format!("convert --probe: {}: {e}", plan.input)))?;
    let demuxer = ctx
        .containers
        .open_demuxer(&format, handle, &ctx.codecs)
        .map_err(|e| Error::invalid(format!("convert --probe: {}: {e}", plan.input)))?;

    let format_label = format_label_for(&format, demuxer.format_name());
    let streams = demuxer.streams();

    let mut s = Summary::default();
    s.push(Field::Str("path", plan.input.clone()));
    // The stream classification picks the highest-information bucket
    // available: video > audio > other. A movie file with both an
    // audio and a video track reports `kind: video`; an MP3 reports
    // `kind: audio`.
    let kind = classify_kind(streams);
    s.push(Field::Str("kind", kind.into()));
    s.push(Field::Str("container", format_label));
    s.push(Field::Int("file_size_bytes", file_size as i64));
    s.push(Field::Int("stream_count", streams.len() as i64));

    // Per-stream details: codec, media type, dimensions / sample rate
    // / channel layout / duration, etc. The stream array is always
    // present (even when empty) so callers iterating it don't have
    // to special-case missing fields.
    let stream_groups: Vec<Vec<Field>> = streams.iter().map(stream_summary_fields).collect();
    s.push(Field::GroupList("streams", stream_groups));
    Ok(s)
}

fn classify_kind(streams: &[oxideav_core::StreamInfo]) -> &'static str {
    let mut has_video = false;
    let mut has_audio = false;
    for st in streams {
        match st.params.media_type {
            MediaType::Video => has_video = true,
            MediaType::Audio => has_audio = true,
            _ => {}
        }
    }
    if has_video {
        // Single-frame "video" containers (PNG / JPEG / WebP single
        // image / BMP) report a single video stream of n_frames=1; we
        // call those `image` so the field communicates the user's
        // mental model ("this PNG is an image, not a movie").
        if streams.len() == 1 && is_single_frame(&streams[0]) {
            return "image";
        }
        "video"
    } else if has_audio {
        "audio"
    } else {
        "other"
    }
}

/// Heuristic: a container with a single video stream and no `frame_rate`
/// / `duration` set is almost always a still image (PNG / JPEG / WebP /
/// BMP). The real test would be the demuxer reporting `nb_frames == 1`,
/// but `StreamInfo` doesn't carry that today; this proxy is correct for
/// every still-image container in the workspace.
fn is_single_frame(stream: &oxideav_core::StreamInfo) -> bool {
    let p = &stream.params;
    p.frame_rate.is_none() && stream.duration.is_none()
}

fn stream_summary_fields(stream: &oxideav_core::StreamInfo) -> Vec<Field> {
    let p = &stream.params;
    let mut out: Vec<Field> = Vec::new();
    out.push(Field::Int("index", stream.index as i64));
    out.push(Field::Str(
        "media_type",
        media_type_label(p.media_type).into(),
    ));
    out.push(Field::Str("codec_id", p.codec_id.as_str().into()));

    match p.media_type {
        MediaType::Video => {
            let w = p.width.unwrap_or(0);
            let h = p.height.unwrap_or(0);
            out.push(Field::Int("width", w as i64));
            out.push(Field::Int("height", h as i64));
            let (depth, colorspace) = match p.pixel_format {
                Some(pf) => describe_pixel_format(pf),
                None => (0, "unknown"),
            };
            out.push(Field::Int("bit_depth", depth as i64));
            out.push(Field::Str("color_space", colorspace.into()));
            out.push(Field::Str(
                "alpha",
                if p.pixel_format.map(has_alpha).unwrap_or(false) {
                    "yes".into()
                } else {
                    "no".into()
                },
            ));
            if let Some(fr) = p.frame_rate {
                out.push(Field::Float("frame_rate_fps", rational_as_f64(fr)));
            }
            if let Some(d) = stream.duration {
                let secs = stream.time_base.seconds_of(d);
                out.push(Field::Float("duration_s", secs));
            }
        }
        MediaType::Audio => {
            if let Some(sr) = p.sample_rate {
                out.push(Field::Int("sample_rate_hz", sr as i64));
            }
            if let Some(ch) = p.resolved_channels() {
                out.push(Field::Int("channels", ch as i64));
            }
            if let Some(layout) = p.channel_layout {
                out.push(Field::Str("channel_layout", layout.to_string()));
            }
            if let Some(sf) = p.sample_format {
                out.push(Field::Int("bit_depth", sample_format_bits(sf) as i64));
                out.push(Field::Str("sample_format", sample_format_label(sf).into()));
            }
            if let Some(d) = stream.duration {
                let secs = stream.time_base.seconds_of(d);
                out.push(Field::Float("duration_s", secs));
            }
            if let Some(br) = p.bit_rate {
                out.push(Field::Int("bit_rate", br as i64));
            }
        }
        _ => {}
    }
    out
}

fn media_type_label(m: MediaType) -> &'static str {
    match m {
        MediaType::Video => "video",
        MediaType::Audio => "audio",
        MediaType::Subtitle => "subtitle",
        MediaType::Data => "data",
        MediaType::Unknown => "unknown",
    }
}

fn rational_as_f64(r: oxideav_core::Rational) -> f64 {
    r.as_f64()
}

/// Map a [`PixelFormat`] to (per-channel bit depth, color-space label).
/// Mirrors the equivalent helper in `ping.rs` — kept duplicated rather
/// than crossing module boundaries because the probe surface is
/// expected to grow more colour-model variants over time.
fn describe_pixel_format(pf: PixelFormat) -> (u8, &'static str) {
    use PixelFormat::*;
    match pf {
        Rgb24 | Rgba | Bgr24 | Bgra | Argb | Abgr => (8, "sRGB"),
        Rgb48Le | Rgba64Le => (16, "sRGB"),
        Yuv420P | Yuv422P | Yuv444P | YuvJ420P | YuvJ422P | YuvJ444P | Nv12 | Nv21 | Yuyv422
        | Uyvy422 | Yuva420P => (8, "sRGB"),
        Yuv420P10Le | Yuv422P10Le | Yuv444P10Le => (10, "sRGB"),
        Yuv420P12Le | Yuv422P12Le | Yuv444P12Le => (12, "sRGB"),
        Gray8 | Ya8 => (8, "Gray"),
        Gray10Le => (10, "Gray"),
        Gray12Le => (12, "Gray"),
        Gray16Le => (16, "Gray"),
        MonoBlack | MonoWhite => (1, "Gray"),
        Pal8 => (8, "sRGB"),
        Cmyk => (8, "CMYK"),
        _ => (0, "unknown"),
    }
}

/// Whether the named pixel format carries an explicit alpha channel.
/// Conservative — only known-alpha variants return `true`.
fn has_alpha(pf: PixelFormat) -> bool {
    use PixelFormat::*;
    matches!(pf, Rgba | Bgra | Argb | Abgr | Rgba64Le | Yuva420P | Ya8)
}

fn sample_format_bits(sf: oxideav_core::SampleFormat) -> u8 {
    use oxideav_core::SampleFormat::*;
    match sf {
        U8 | U8P | S8 => 8,
        S16 | S16P => 16,
        S24 => 24,
        S32 | S32P | F32 | F32P => 32,
        F64 | F64P => 64,
        // `SampleFormat` is `#[non_exhaustive]`; any new variant lands
        // here as 0 ("unknown") until the table is updated.
        _ => 0,
    }
}

fn sample_format_label(sf: oxideav_core::SampleFormat) -> &'static str {
    use oxideav_core::SampleFormat::*;
    match sf {
        U8 => "u8",
        S8 => "s8",
        S16 => "s16",
        S24 => "s24",
        S32 => "s32",
        F32 => "f32",
        F64 => "f64",
        U8P => "u8p",
        S16P => "s16p",
        S32P => "s32p",
        F32P => "f32p",
        F64P => "f64p",
        _ => "unknown",
    }
}

fn format_label_for(format: &str, format_name: &str) -> String {
    let chosen = if !format_name.is_empty() {
        format_name
    } else {
        format
    };
    chosen.to_ascii_uppercase()
}

fn ext_from_uri(uri: &str) -> Option<String> {
    let last = uri.rsplit('/').next().unwrap_or(uri);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(last[dot + 1..].to_ascii_lowercase())
}

/// Take the page list (PDF) and an optional [`PageSelector`] and
/// resolve to the indices the probe should report. Currently unused
/// — full document by default — but kept here as an attachment point
/// for a future `--probe input.pdf[2-5]` invocation.
#[allow(dead_code)]
fn _selected_pages_from_plan(plan: &ConvertPlan, pages: &[Page]) -> Result<Vec<usize>> {
    match &plan.input_pages {
        Some(sel) => sel
            .resolve(pages.len())
            .map_err(|e| Error::invalid(format!("convert: input '{}': {e}", plan.input))),
        None => Ok((0..pages.len()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_escapes_control_chars() {
        assert_eq!(json_string("hello"), "\"hello\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("a\tb"), "\"a\\tb\"");
        assert_eq!(json_string("\""), "\"\\\"\"");
        assert_eq!(json_string("\\"), "\"\\\\\"");
        assert_eq!(json_string("\x01"), "\"\\u0001\"");
    }

    #[test]
    fn pretty_writer_indents_groups() {
        let mut s = Summary::default();
        s.push(Field::Str("kind", "test".into()));
        s.push(Field::Group(
            "inner",
            vec![Field::Int("count", 3), Field::Str("name", "foo".into())],
        ));
        let txt = s.to_pretty();
        assert!(txt.contains("kind: test\n"));
        assert!(txt.contains("inner:\n"));
        assert!(txt.contains("  count: 3\n"));
        assert!(txt.contains("  name: foo\n"));
    }

    #[test]
    fn json_writer_quotes_fields() {
        let mut s = Summary::default();
        s.push(Field::Str("kind", "test".into()));
        s.push(Field::Int("count", 7));
        let json = s.to_json();
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
        assert!(json.contains("\"kind\":\"test\""));
        assert!(json.contains("\"count\":7"));
    }

    #[test]
    fn json_writer_handles_group_list() {
        let mut s = Summary::default();
        s.push(Field::GroupList(
            "streams",
            vec![
                vec![
                    Field::Int("index", 0),
                    Field::Str("media_type", "video".into()),
                ],
                vec![
                    Field::Int("index", 1),
                    Field::Str("media_type", "audio".into()),
                ],
            ],
        ));
        let json = s.to_json();
        assert!(
            json.contains("\"streams\":[{\"index\":0,\"media_type\":\"video\"},{\"index\":1,\"media_type\":\"audio\"}]"),
            "unexpected json: {json}"
        );
    }

    #[test]
    fn json_writer_str_list() {
        let mut s = Summary::default();
        s.push(Field::StrList(
            "topologies",
            vec!["Triangles=1".into(), "Lines=2".into()],
        ));
        let json = s.to_json();
        assert!(
            json.contains("\"topologies\":[\"Triangles=1\",\"Lines=2\"]"),
            "got: {json}"
        );
    }

    #[test]
    fn float_pretty_uses_two_decimals() {
        let mut s = Summary::default();
        s.push(Field::Float("width_pt", 612.5));
        let txt = s.to_pretty();
        assert!(txt.contains("width_pt: 612.50\n"), "got: {txt:?}");
    }

    #[test]
    fn ext_helper_matches_case_insensitively() {
        assert!(has_extension("foo.svg", &["svg"]));
        assert!(has_extension("foo.SVG", &["svg"]));
        assert!(has_extension("/abs/path/file.SvG", &["svg"]));
        assert!(!has_extension("foo.png", &["svg"]));
    }
}
