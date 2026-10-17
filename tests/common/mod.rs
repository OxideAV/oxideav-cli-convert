//! A test-only still-image format, `tprobe`, whose encoder *reports*
//! what `convert` handed it instead of coding pixels: the thread
//! budget it was granted, the options it received, the layout and
//! geometry of each frame, the metadata blobs at both levels and the
//! palette records. The matching demuxer / decoder synthesise a source
//! with exactly the records a test wants (a `Pal8` picture with a
//! transparent entry, stream-level ICC / Exif blobs, per-frame blobs),
//! so every behaviour of the still-image write path is pinned
//! deterministically, independent of which registered format crates
//! carry which record today.
//!
//! The file layouts are this test suite's own invention:
//!
//! * input: `TPRB\n`, a big-endian u32 header length, a JSON header
//!   (`width`, `height`, `format`, `palette`, `stream_blobs`,
//!   `frame_blobs`), then the packed pixel bytes;
//! * output: one JSON line for the muxer's stream (what the muxer was
//!   opened with), then one JSON line per encoded frame (the encoder's
//!   report).

#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use oxideav_core::{
    BlobKind, CodecCapabilities, CodecId, CodecInfo, CodecOptionsStruct, CodecParameters,
    CodecResolver, Decoder, Demuxer, Encoder, Error, ExecutionContext, Frame, MediaType,
    MetadataBlob, Muxer, OptionField, OptionKind, OptionValue, Packet, PixelFormat, ProbeData,
    ProbeScore, ReadSeek, Result, RuntimeContext, StreamInfo, TimeBase, VideoFrame, VideoPlane,
    WriteSeek,
};
use serde_json::{json, Value};

/// Container, codec and extension name.
pub const NAME: &str = "tprobe";
const MAGIC: &[u8] = b"TPRB\n";

/// A second target, `.tprgb`: the same encoder declaring `Rgb24`
/// only (no alpha, no palette) — what a JPEG-like sink looks like to
/// the planner.
pub const NAME_RGB: &str = "tprgb";
/// A third, `.tprgba`: `Rgba` only.
pub const NAME_RGBA: &str = "tprgba";

/// Register the container (demuxer, muxer, probe, extension) and the
/// codec (decoder, encoder with a `quality` / `tag` schema) for the
/// three targets.
pub fn register(ctx: &mut RuntimeContext) {
    register_one(ctx, NAME, ACCEPTED, make_encoder);
    register_one(ctx, NAME_RGB, &[PixelFormat::Rgb24], make_encoder_rgb);
    register_one(ctx, NAME_RGBA, &[PixelFormat::Rgba], make_encoder_rgba);
}

fn register_one(
    ctx: &mut RuntimeContext,
    name: &str,
    accepted: &[PixelFormat],
    encoder: oxideav_core::EncoderFactory,
) {
    ctx.containers.register_demuxer(name, open_demuxer);
    ctx.containers.register_muxer(name, open_muxer);
    ctx.containers.register_probe(name, probe);
    ctx.containers.register_extension(name, name);
    let caps = CodecCapabilities::video(format!("{name}_test"))
        .with_decode()
        .with_encode()
        .with_lossy(true)
        .with_lossless(true)
        .with_intra_only(true)
        .with_pixel_formats(accepted.to_vec());
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(name))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(encoder)
            .encoder_options::<ProbeOptions>(),
    );
}

/// The `tprobe` encoder's declared input layouts (the planner's pick
/// is among these): packed RGB first, then the palette and grey
/// layouts.
pub const ACCEPTED: &[PixelFormat] = &[
    PixelFormat::Rgb24,
    PixelFormat::Rgba,
    PixelFormat::Pal8,
    PixelFormat::Gray8,
];

/// Pixel layouts the source side can carry (packed, one plane).
const PACKED: &[(PixelFormat, usize)] = &[
    (PixelFormat::Rgb24, 3),
    (PixelFormat::Rgba, 4),
    (PixelFormat::Gray8, 1),
    (PixelFormat::Pal8, 1),
];

fn fmt_name(f: PixelFormat) -> String {
    format!("{f:?}")
}

fn parse_fmt(s: &str) -> Option<(PixelFormat, usize)> {
    PACKED.iter().copied().find(|(f, _)| fmt_name(*f) == s)
}

fn bytes_per_pixel(f: PixelFormat) -> usize {
    PACKED
        .iter()
        .find(|(x, _)| *x == f)
        .map(|(_, n)| *n)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Source side: a test writes a `.tprobe` file with the records it wants.
// ---------------------------------------------------------------------

/// What a `.tprobe` input carries.
#[derive(Clone, Debug)]
pub struct Source {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Packed pixels, `width × height × bytes-per-pixel` (indices for
    /// `Pal8`).
    pub pixels: Vec<u8>,
    /// RGBA palette entries for a `Pal8` picture (alpha `255` =
    /// opaque); empty otherwise.
    pub palette: Vec<[u8; 4]>,
    /// Blobs the demuxer declares on the stream.
    pub stream_blobs: Vec<MetadataBlob>,
    /// Blobs the decoder attaches to the frame's record.
    pub frame_blobs: Vec<MetadataBlob>,
}

impl Source {
    /// A `width × height` packed picture in `format` from a pixel
    /// closure (`x`, `y` → channel bytes).
    pub fn packed(
        width: u32,
        height: u32,
        format: PixelFormat,
        px: impl Fn(u32, u32) -> Vec<u8>,
    ) -> Source {
        let bpp = bytes_per_pixel(format);
        let mut pixels = Vec::with_capacity((width * height) as usize * bpp);
        for y in 0..height {
            for x in 0..width {
                let p = px(x, y);
                assert_eq!(p.len(), bpp, "{format:?} takes {bpp} bytes per pixel");
                pixels.extend_from_slice(&p);
            }
        }
        Source {
            width,
            height,
            format,
            pixels,
            palette: Vec::new(),
            stream_blobs: Vec::new(),
            frame_blobs: Vec::new(),
        }
    }

    pub fn with_palette(mut self, palette: Vec<[u8; 4]>) -> Source {
        self.palette = palette;
        self
    }

    pub fn with_stream_blobs(mut self, blobs: Vec<MetadataBlob>) -> Source {
        self.stream_blobs = blobs;
        self
    }

    pub fn with_frame_blobs(mut self, blobs: Vec<MetadataBlob>) -> Source {
        self.frame_blobs = blobs;
        self
    }

    /// The file bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let header = json!({
            "width": self.width,
            "height": self.height,
            "format": fmt_name(self.format),
            "palette": self.palette.iter().map(|e| json!(e.to_vec())).collect::<Vec<_>>(),
            "stream_blobs": blobs_json(&self.stream_blobs),
            "frame_blobs": blobs_json(&self.frame_blobs),
        })
        .to_string();
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&(header.len() as u32).to_be_bytes());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.pixels);
        out
    }

    pub fn write(&self, path: &Path) {
        fs::write(path, self.to_bytes()).expect("write tprobe source");
    }
}

fn blobs_json(blobs: &[MetadataBlob]) -> Vec<Value> {
    blobs
        .iter()
        .map(|b| {
            json!({
                "kind": b.kind.as_str(),
                "mime": b.mime,
                "data": b.data,
            })
        })
        .collect()
}

fn blobs_from_json(v: &Value) -> Vec<MetadataBlob> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    let kind = b.get("kind")?.as_str()?;
                    let data: Vec<u8> = b
                        .get("data")?
                        .as_array()?
                        .iter()
                        .filter_map(|x| x.as_u64().map(|n| n as u8))
                        .collect();
                    let mut blob = MetadataBlob::new(BlobKind::custom(kind), data);
                    if let Some(m) = b.get("mime").and_then(Value::as_str) {
                        blob = blob.with_mime(m);
                    }
                    Some(blob)
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------
// Container: probe, demuxer, muxer.
// ---------------------------------------------------------------------

fn probe(p: &ProbeData) -> ProbeScore {
    if p.buf.starts_with(MAGIC) {
        100
    } else {
        0
    }
}

struct ProbeDemuxer {
    streams: Vec<StreamInfo>,
    pixels: Option<Vec<u8>>,
}

fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    let rest = bytes
        .strip_prefix(MAGIC)
        .ok_or_else(|| Error::invalid("tprobe: bad magic"))?;
    if rest.len() < 4 {
        return Err(Error::invalid("tprobe: truncated header length"));
    }
    let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let header = rest
        .get(4..4 + len)
        .ok_or_else(|| Error::invalid("tprobe: truncated header"))?;
    let header: Value = serde_json::from_slice(header)
        .map_err(|e| Error::invalid(format!("tprobe: header: {e}")))?;
    let pixels = rest[4 + len..].to_vec();
    let width = header["width"].as_u64().unwrap_or(0) as u32;
    let height = header["height"].as_u64().unwrap_or(0) as u32;
    let (format, _) = parse_fmt(header["format"].as_str().unwrap_or(""))
        .ok_or_else(|| Error::invalid("tprobe: unknown layout"))?;
    let mut params = CodecParameters::video(CodecId::new(NAME));
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(format);
    params.extradata = header.to_string().into_bytes();
    let params = params.with_blobs(blobs_from_json(&header["stream_blobs"]));
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    };
    Ok(Box::new(ProbeDemuxer {
        streams: vec![stream],
        pixels: Some(pixels),
    }))
}

impl Demuxer for ProbeDemuxer {
    fn format_name(&self) -> &str {
        NAME
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let data = self.pixels.take().ok_or(Error::Eof)?;
        let mut pkt = Packet::new(0, TimeBase::new(1, 1), data);
        pkt.flags.keyframe = true;
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.duration = Some(1);
        Ok(pkt)
    }
}

struct ProbeMuxer {
    out: Box<dyn WriteSeek>,
    streams: Vec<StreamInfo>,
}

fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    if streams.len() != 1 {
        return Err(Error::unsupported(format!(
            "tprobe: one stream per file, got {}",
            streams.len()
        )));
    }
    Ok(Box::new(ProbeMuxer {
        out: output,
        streams: streams.to_vec(),
    }))
}

impl Muxer for ProbeMuxer {
    fn format_name(&self) -> &str {
        NAME
    }

    fn write_header(&mut self) -> Result<()> {
        let s = &self.streams[0];
        let line = json!({
            "stream": {
                "codec": s.params.codec_id.as_str(),
                "width": s.params.width,
                "height": s.params.height,
                "format": s.params.pixel_format.map(fmt_name),
                "blobs": blobs_json(s.params.blobs()),
                "time_base": [s.time_base.num(), s.time_base.den()],
                "duration": s.duration,
            }
        });
        writeln!(self.out, "{line}")?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        writeln!(self.out)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Codec: decoder (synthesises the frame + records), encoder (reports).
// ---------------------------------------------------------------------

struct ProbeDecoder {
    id: CodecId,
    params: CodecParameters,
    header: Value,
    pending: Option<Vec<u8>>,
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let header: Value = serde_json::from_slice(&params.extradata)
        .map_err(|e| Error::invalid(format!("tprobe: decoder extradata: {e}")))?;
    Ok(Box::new(ProbeDecoder {
        id: CodecId::new(NAME),
        params: params.clone(),
        header,
        pending: None,
    }))
}

impl Decoder for ProbeDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.pending = Some(packet.data.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let data = self.pending.take().ok_or(Error::NeedMore)?;
        let fmt = self.params.pixel_format.unwrap_or(PixelFormat::Rgb24);
        let w = self.params.width.unwrap_or(0) as usize;
        let mut frame = VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane {
                stride: w * bytes_per_pixel(fmt),
                data,
            }],
        };
        if fmt == PixelFormat::Pal8 {
            let palette: Vec<[u8; 4]> = self.header["palette"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let e = e.as_array()?;
                            Some([
                                e[0].as_u64()? as u8,
                                e[1].as_u64()? as u8,
                                e[2].as_u64()? as u8,
                                e[3].as_u64()? as u8,
                            ])
                        })
                        .collect()
                })
                .unwrap_or_default();
            frame.set_palette_rgba(&palette);
        }
        let blobs = blobs_from_json(&self.header["frame_blobs"]);
        if !blobs.is_empty() {
            frame.set_blobs(blobs);
        }
        Ok(Frame::Video(frame))
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The encoder's declared options: a `quality` (so the planner's
/// schema-gated forwarding can be observed) and a free-form `tag`.
#[derive(Debug, Default)]
pub struct ProbeOptions {
    pub quality: Option<u32>,
    pub tag: Option<String>,
}

impl CodecOptionsStruct for ProbeOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "quality",
            kind: OptionKind::U32,
            default: OptionValue::U32(75),
            help: "advisory quality 0..=100 (reported, not applied)",
        },
        OptionField {
            name: "tag",
            kind: OptionKind::String,
            default: OptionValue::String(String::new()),
            help: "free-form marker echoed in the report",
        },
    ];

    fn apply(&mut self, key: &str, value: &OptionValue) -> Result<()> {
        match (key, value) {
            ("quality", OptionValue::U32(q)) => self.quality = Some(*q),
            ("tag", OptionValue::String(s)) => self.tag = Some(s.clone()),
            _ => return Err(Error::invalid(format!("tprobe: option {key}"))),
        }
        Ok(())
    }
}

struct ProbeEncoder {
    id: CodecId,
    params: CodecParameters,
    out_params: CodecParameters,
    options: ProbeOptions,
    threads: usize,
    queue: Vec<Packet>,
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_for(params, NAME, ACCEPTED)
}

fn make_encoder_rgb(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_for(params, NAME_RGB, &[PixelFormat::Rgb24])
}

fn make_encoder_rgba(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_for(params, NAME_RGBA, &[PixelFormat::Rgba])
}

fn make_encoder_for(
    params: &CodecParameters,
    name: &str,
    accepted: &[PixelFormat],
) -> Result<Box<dyn Encoder>> {
    let options: ProbeOptions = oxideav_core::parse_options(&params.options)?;
    let fmt = params
        .pixel_format
        .ok_or_else(|| Error::invalid("tprobe: encoder needs a pixel format"))?;
    if !accepted.contains(&fmt) {
        return Err(Error::unsupported(format!(
            "{name}: encoder does not take {fmt:?}"
        )));
    }
    // The output parameters carry no blobs of their own: what the
    // muxer sees at the stream level is what `convert` attached.
    let mut out_params = params.clone();
    out_params.codec_id = CodecId::new(name);
    out_params.blobs = Vec::new();
    Ok(Box::new(ProbeEncoder {
        id: CodecId::new(name),
        params: params.clone(),
        out_params,
        options,
        threads: 1,
        queue: Vec::new(),
    }))
}

impl Encoder for ProbeEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Video(vf) = frame else {
            return Err(Error::unsupported("tprobe: video frames only"));
        };
        let planes = vf.image_planes();
        let first: Vec<u8> = planes
            .first()
            .map(|p| p.data.iter().take(8).copied().collect())
            .unwrap_or_default();
        let palette_len = vf.palette().map(|p| p.len() / 3);
        let palette_alpha = vf.palette_alpha().map(|a| a.to_vec());
        let report = json!({
            "threads": self.threads,
            "format": self.params.pixel_format.map(fmt_name),
            "width": self.params.width,
            "height": self.params.height,
            "quality": self.options.quality,
            "tag": self.options.tag,
            "options": self.params.options.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "stream_blobs": blobs_json(self.params.blobs()),
            "frame_blobs": blobs_json(&vf.blobs()),
            "planes": planes.len(),
            "stride": planes.first().map(|p| p.stride),
            "first_bytes": first,
            "palette_len": palette_len,
            "palette_alpha": palette_alpha,
        });
        let mut pkt = Packet::new(0, TimeBase::new(1, 1), report.to_string().into_bytes());
        pkt.flags.keyframe = true;
        pkt.pts = vf.pts;
        self.queue.push(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if self.queue.is_empty() {
            Err(Error::NeedMore)
        } else {
            Ok(self.queue.remove(0))
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_execution_context(&mut self, ctx: &ExecutionContext) {
        self.threads = ctx.threads;
    }
}

// ---------------------------------------------------------------------
// Reading an output back.
// ---------------------------------------------------------------------

/// A written `.tprobe` output: the muxer's stream line and the
/// encoder's per-frame reports.
#[derive(Clone, Debug)]
pub struct Report {
    pub stream: Value,
    pub frames: Vec<Value>,
}

impl Report {
    pub fn read(path: &Path) -> Report {
        let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let stream: Value = serde_json::from_str(lines.next().expect("stream line")).unwrap();
        let frames = lines
            .map(|l| serde_json::from_str(l).expect("frame report"))
            .collect();
        Report {
            stream: stream["stream"].clone(),
            frames,
        }
    }

    /// The single frame's report.
    pub fn frame(&self) -> &Value {
        assert_eq!(
            self.frames.len(),
            1,
            "one frame expected: {:?}",
            self.frames
        );
        &self.frames[0]
    }

    /// The thread budget the encoder was granted.
    pub fn threads(&self) -> usize {
        self.frame()["threads"].as_u64().unwrap() as usize
    }

    /// The frame's layout name (`"Rgb24"`, …).
    pub fn format(&self) -> String {
        self.frame()["format"].as_str().unwrap().to_string()
    }

    /// `quality` as the encoder parsed it (`None` when not forwarded).
    pub fn quality(&self) -> Option<u32> {
        self.frame()["quality"].as_u64().map(|q| q as u32)
    }

    /// Blob kinds the encoder's parameters carried (stream level).
    pub fn stream_blob_kinds(&self) -> Vec<String> {
        kinds(&self.frame()["stream_blobs"])
    }

    /// Blob kinds the frame's record carried.
    pub fn frame_blob_kinds(&self) -> Vec<String> {
        kinds(&self.frame()["frame_blobs"])
    }

    /// Blob kinds the muxer's stream carried.
    pub fn muxer_blob_kinds(&self) -> Vec<String> {
        kinds(&self.stream["blobs"])
    }

    /// The bytes of the first blob of `kind` at the encoder's stream
    /// level.
    pub fn stream_blob(&self, kind: &str) -> Option<Vec<u8>> {
        self.frame()["stream_blobs"]
            .as_array()?
            .iter()
            .find(|b| b["kind"] == kind)
            .map(|b| {
                b["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_u64().unwrap() as u8)
                    .collect()
            })
    }

    /// The first bytes of plane 0.
    pub fn first_bytes(&self) -> Vec<u8> {
        self.frame()["first_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap() as u8)
            .collect()
    }

    /// The palette-alpha record, when the frame carried one.
    pub fn palette_alpha(&self) -> Option<Vec<u8>> {
        self.frame()["palette_alpha"]
            .as_array()
            .map(|a| a.iter().map(|x| x.as_u64().unwrap() as u8).collect())
    }

    pub fn palette_len(&self) -> Option<usize> {
        self.frame()["palette_len"].as_u64().map(|n| n as usize)
    }
}

fn kinds(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|b| b["kind"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `MediaType` is re-exported so test files need not import it for
/// the one place that matches on it.
pub fn is_video(s: &StreamInfo) -> bool {
    s.params.media_type == MediaType::Video
}
