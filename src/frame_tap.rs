//! Frame tap: the still-image write path `convert` owns itself.
//!
//! The regular pipeline route hands the whole job — decode, filters,
//! conversion, encode, mux — to the executor. Every still-image output
//! whose source layout the planner knows runs the encode + mux stages
//! on this side instead, because the still-image contract the
//! `oxideav-image` gateway documents needs them here:
//!
//! * **Encoder options** (`--opt KEY=VALUE`) are validated against the
//!   encoder's declared schema at plan time and handed over in its
//!   [`CodecParameters::options`] bag; `-quality N` becomes the
//!   `quality` option exactly when the schema declares one (the
//!   gateway's `SaveOptions::quality` rule).
//! * **Metadata blobs** (ICC / Exif / XMP, core 0.1.40) ride through:
//!   the source stream's blobs go on the encoder's parameters and on
//!   the muxer's stream, the frame's own record stays on the frame —
//!   with a single picture's frame blobs promoted ahead of the stream's
//!   (the gateway's single-picture rule). `-strip` withholds them all.
//! * **Thread budget**: the encoder is granted the plan's
//!   [`ExecutionContext`] (the executor grants the decoder the same).
//! * **Per-frame fan-out** (`convert seq.heics frame-%03d.png`): every
//!   frame of the selected stream lands in its own file, each through
//!   a fresh encoder + muxer.
//!
//! The executor still runs demux → decode → filters → conversion; its
//! output is bound to the reserved `@out` sink, which forwards each
//! decoded frame over a bounded channel to a writer thread that owns
//! the encoder and muxer (the registries are borrowed, so the writer
//! lives in a scoped thread next to the synchronous executor call).
//!
//! `OXIDEAV_CONVERT_TIMING=1` prints per-stage wall-clock spans to
//! stderr: plan, first-frame arrival, encode, mux + write.

use std::fs::File;
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::{Duration, Instant};

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Error, ExecutionContext, Frame, MediaType, Packet,
    PixelFormat, Result, RuntimeContext, StreamInfo,
};
use oxideav_pipeline::executor::JobSink;
use oxideav_pipeline::{Executor, Job};
use oxideav_pixfmt::FormatInfo;

use crate::op::PrintfTemplate;

/// Reserved sink name the tap binds to.
pub const TAP_SINK: &str = "@out";

/// Executor-side error text when the writer thread has gone away.
const WRITER_STOPPED: &str = "frame writer stopped";

/// Everything the writer needs, resolved at plan time.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TapSpec {
    /// Encoder codec id (`png`, `heif`, …).
    pub codec: String,
    /// Muxer (container) name for the output.
    pub container: String,
    /// Validated encoder options.
    pub options: Vec<(String, String)>,
    /// Layout of the frames the executor delivers (the planned
    /// encoder input layout).
    pub frame_format: PixelFormat,
    /// Frame dimensions when known at plan time (no filter ops: the
    /// source stream's own); `None` → derived per frame from the
    /// tightly-packed plane geometry the filters produce.
    pub dims: Option<(u32, u32)>,
    /// The selected source stream (time base / frame rate template).
    pub stream: StreamInfo,
    /// Literal output path (single file) …
    pub output: String,
    /// … or a `%d` template: one file per frame.
    pub template: Option<PrintfTemplate>,
    /// Thread budget granted to the encoder
    /// ([`Encoder::set_execution_context`](oxideav_core::Encoder::set_execution_context)):
    /// the job's `threads` key, else the host's parallelism — the same
    /// resolution the pipeline executor applies to its own codecs.
    pub threads: usize,
    /// `-quality N`: the encoder's `quality` option when its schema
    /// declares one and `--opt quality=` did not set it.
    pub quality: Option<u8>,
    /// `-strip`: write no metadata blobs (stream level or per frame).
    pub strip_metadata: bool,
    /// The frames arrive as `Pal8` (the source layout, left unconverted
    /// by the planner) and the writer expands them to
    /// [`frame_format`](Self::frame_format) itself through the
    /// gateway's palette rules: transparent entries become alpha `0`
    /// on an alpha target, and are composited over
    /// [`background`](Self::background) on a target without alpha.
    pub expand_palette: bool,
    /// `-background` (opaque white by default): what palette
    /// transparency is composited over on a target without alpha.
    pub background: [u8; 4],
    /// Container string pairs (`-set KEY=VALUE` / `-comment TEXT`)
    /// the muxer stores (`Muxer::carries_metadata_key` said so at
    /// plan time), handed over with `Muxer::set_metadata` before the
    /// header. Written even under `-strip`.
    pub metadata: Vec<(String, String)>,
}

/// What the tap wrote.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct TapStats {
    /// Frames received from the executor.
    pub frames: u64,
    /// Files written.
    pub files: u64,
    /// Time spent in the encoder (send + flush + drain).
    pub encode: Duration,
    /// Time spent in the muxer (header + packets + trailer, incl. I/O).
    pub mux: Duration,
    /// Wall-clock arrival of the first frame, from the run's start.
    pub first_frame: Option<Duration>,
}

/// Run `job` (whose single output is [`TAP_SINK`]) and write its frames
/// per `spec`. Returns the executor's stats and the tap's.
pub fn run(
    job: &Job,
    ctx: &RuntimeContext,
    spec: &TapSpec,
    prefs: &oxideav_pipeline::CodecPreferences,
) -> Result<(oxideav_pipeline::executor::ExecutorStats, TapStats)> {
    let (tx, rx) = sync_channel::<Frame>(2);
    let start = Instant::now();
    std::thread::scope(|s| {
        let writer = s.spawn(move || write_frames(rx, ctx, spec, start));
        let sink = ChannelSink { tx: Some(tx) };
        let exec = Executor::new(job, ctx)
            .with_codec_preferences(prefs.clone())
            .with_sink_override(TAP_SINK, Box::new(sink))
            .run();
        let written = writer.join().unwrap_or_else(|payload| {
            // An encoder or muxer panic: the in-flight single output
            // is removed (the writer's own error paths do the same;
            // unwinding skipped them), and the panic text travels
            // with the error. `Other` — the umbrella CLI's "anything
            // else" exit — because a panic is no typed class.
            if spec.template.is_none() {
                let _ = std::fs::remove_file(&spec.output);
            }
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(Error::other(format!(
                "convert: frame writer panicked in the {} encoder / {} muxer: {what}",
                spec.codec, spec.container
            )))
        });
        // A writer failure surfaces on the executor side as a closed
        // channel ([`WRITER_STOPPED`]); report the writer's own, more
        // specific error then. Any other executor failure is the root
        // cause (the writer merely saw the channel close early).
        match (exec, written) {
            (Ok(es), Ok(ts)) => Ok((es, ts)),
            (Err(e), Err(w)) if e.to_string().contains(WRITER_STOPPED) => Err(w),
            (Err(e), _) => Err(e),
            (Ok(_), Err(w)) => Err(w),
        }
    })
}

/// `@out` sink: forwards each decoded video frame to the writer.
struct ChannelSink {
    tx: Option<SyncSender<Frame>>,
}

impl JobSink for ChannelSink {
    fn start(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        Ok(())
    }

    fn write_packet(&mut self, _kind: MediaType, _pkt: &Packet) -> Result<()> {
        Err(Error::unsupported(
            "convert: frame tap received a packet (stream copy) instead of a decoded frame",
        ))
    }

    fn write_frame(&mut self, kind: MediaType, frm: &Frame) -> Result<()> {
        if kind != MediaType::Video {
            return Ok(());
        }
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| Error::other("convert: frame tap already finished"))?;
        tx.send(frm.clone())
            .map_err(|_| Error::other(format!("convert: {WRITER_STOPPED}")))
    }

    fn finish(&mut self) -> Result<()> {
        self.tx = None;
        Ok(())
    }
}

/// Writer thread body: encode + mux every received frame.
fn write_frames(
    rx: Receiver<Frame>,
    ctx: &RuntimeContext,
    spec: &TapSpec,
    start: Instant,
) -> Result<TapStats> {
    let mut stats = TapStats::default();
    let mut single: Option<OpenFile> = None;
    let result = (|| {
        for frame in rx.iter() {
            stats.first_frame.get_or_insert_with(|| start.elapsed());
            let frame = if spec.strip_metadata {
                strip_frame(frame)
            } else {
                frame
            };
            let frame = if spec.expand_palette {
                expand_palette(frame, spec)?
            } else {
                frame
            };
            let Frame::Video(ref vf) = frame else {
                continue;
            };
            let (w, h) = match spec.dims {
                Some(d) => d,
                None => frame_dims(vf, spec.frame_format).ok_or_else(|| {
                    Error::unsupported(format!(
                        "convert: cannot infer the frame geometry of a {:?} frame after the filter ops",
                        spec.frame_format
                    ))
                })?,
            };
            match &spec.template {
                Some(t) => {
                    let path = t.expand(stats.frames as usize);
                    let mut f = OpenFile::create(ctx, spec, &path, w, h, vf, &mut stats)?;
                    f.push(&frame, &mut stats)?;
                    f.finish(&mut stats)?;
                    stats.files += 1;
                }
                None => {
                    if single.is_none() {
                        single = Some(OpenFile::create(
                            ctx,
                            spec,
                            &spec.output,
                            w,
                            h,
                            vf,
                            &mut stats,
                        )?);
                    }
                    single
                        .as_mut()
                        .expect("opened above")
                        .push(&frame, &mut stats)?;
                }
            }
            stats.frames += 1;
        }
        if let Some(f) = single.take() {
            f.finish(&mut stats)?;
            stats.files += 1;
        }
        if stats.frames == 0 {
            return Err(Error::invalid(
                "convert: the selected stream produced no frames; nothing written",
            ));
        }
        Ok(())
    })();
    if let Err(e) = result {
        // Drop the half-written single output (fan-out files that were
        // completed stay; the one in flight is removed by `create`'s
        // guard on its own failure paths).
        if let Some(f) = single.take() {
            f.abandon();
        }
        return Err(e);
    }
    Ok(stats)
}

/// One output file: an encoder feeding a muxer.
struct OpenFile {
    path: String,
    encoder: Box<dyn oxideav_core::Encoder>,
    muxer: Box<dyn oxideav_core::Muxer>,
}

impl OpenFile {
    /// `first` is the file's first frame: its own blobs record is the
    /// file's metadata ahead of the stream's (every file this writer
    /// produces holds one picture per encoder stream).
    fn create(
        ctx: &RuntimeContext,
        spec: &TapSpec,
        path: &str,
        w: u32,
        h: u32,
        first: &oxideav_core::VideoFrame,
        stats: &mut TapStats,
    ) -> Result<OpenFile> {
        let codec_id = CodecId::new(spec.codec.as_str());
        let mut params = CodecParameters::video(codec_id.clone());
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(spec.frame_format);
        params.frame_rate = spec.stream.params.frame_rate;
        let mut options = CodecOptions::new();
        let quality = spec
            .quality
            .or_else(|| crate::image_sink::default_quality(&spec.codec));
        if let Some(q) = quality {
            let declared = ctx
                .codecs
                .encoder_options_schema(&codec_id)
                .is_some_and(|schema| schema.iter().any(|f| f.name == "quality"));
            if declared && !spec.options.iter().any(|(k, _)| k == "quality") {
                options.insert("quality", q.to_string());
            }
        }
        for (k, v) in &spec.options {
            options.insert(k.as_str(), v.as_str());
        }
        params.options = options;
        let blobs = if spec.strip_metadata {
            Vec::new()
        } else {
            file_blobs(first, spec.stream.params.blobs())
        };
        params.blobs = blobs.clone();
        let t = Instant::now();
        let mut encoder = oxideav_pipeline::selection::make_encoder(&ctx.codecs, &params)
            .map_err(|e| Error::invalid(format!("convert: {} encoder: {e}", spec.codec)))?;
        encoder.set_execution_context(&ExecutionContext::with_threads(spec.threads));
        stats.encode += t.elapsed();

        let mut stream = spec.stream.clone();
        stream.index = 0;
        stream.params = encoder.output_params().clone();
        // A muxer that embeds metadata itself (HEIF) reads the stream's
        // blobs; an encoder that does not echo them on its output
        // parameters would otherwise leave the muxer without them.
        if stream.params.blobs().is_empty() && !blobs.is_empty() {
            stream.params.blobs = blobs;
        } else if spec.strip_metadata {
            stream.params.blobs = Vec::new();
        }
        let t = Instant::now();
        // The I/O class survives (permission denied, missing directory):
        // the CLI maps `Error::Io` kinds to its typed exit codes.
        let file = File::create(path).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("convert: failed to create {path}: {e}"),
            ))
        })?;
        let muxer = ctx
            .containers
            .open_muxer(&spec.container, Box::new(file), &[stream]);
        let mut muxer = match muxer {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
        };
        // Container string pairs go in before the header (most formats
        // write their text block there). `Unsupported` is the
        // contract's "not carried", never a failure of the write; any
        // other refusal is the muxer's own and keeps its class.
        if !spec.metadata.is_empty() {
            match muxer.set_metadata(&spec.metadata) {
                Ok(()) | Err(Error::Unsupported(_)) => {}
                Err(e) => {
                    drop(muxer);
                    let _ = std::fs::remove_file(path);
                    return Err(e);
                }
            }
        }
        if let Err(e) = muxer.write_header() {
            drop(muxer);
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        stats.mux += t.elapsed();
        Ok(OpenFile {
            path: path.to_string(),
            encoder,
            muxer,
        })
    }

    fn push(&mut self, frame: &Frame, stats: &mut TapStats) -> Result<()> {
        let t = Instant::now();
        let sent = self.encoder.send_frame(frame);
        stats.encode += t.elapsed();
        sent?;
        self.drain(stats)
    }

    fn drain(&mut self, stats: &mut TapStats) -> Result<()> {
        loop {
            let t = Instant::now();
            let pkt = self.encoder.receive_packet();
            stats.encode += t.elapsed();
            match pkt {
                Ok(p) => {
                    let t = Instant::now();
                    self.muxer.write_packet(&p)?;
                    stats.mux += t.elapsed();
                }
                Err(Error::NeedMore) | Err(Error::Eof) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn finish(mut self, stats: &mut TapStats) -> Result<()> {
        let path = self.path.clone();
        let done = (|| {
            let t = Instant::now();
            self.encoder.flush()?;
            stats.encode += t.elapsed();
            self.drain(stats)?;
            let t = Instant::now();
            self.muxer.write_trailer()?;
            stats.mux += t.elapsed();
            Ok(())
        })();
        if done.is_err() {
            drop(self);
            let _ = std::fs::remove_file(&path);
        }
        done
    }

    fn abandon(self) {
        let path = self.path.clone();
        drop(self);
        let _ = std::fs::remove_file(Path::new(&path));
    }
}

/// The blobs a one-picture file carries: the picture's own record
/// first, then the stream's blobs of kinds the picture does not carry
/// (a frame blob overrides a stream blob of its kind — the gateway's
/// precedence and single-picture promotion rules).
pub fn file_blobs(
    frame: &oxideav_core::VideoFrame,
    stream: &[oxideav_core::MetadataBlob],
) -> Vec<oxideav_core::MetadataBlob> {
    let mut out = frame.blobs();
    for b in stream {
        if !out.iter().any(|f| f.kind == b.kind) {
            out.push(b.clone());
        }
    }
    out
}

/// A `Pal8` frame expanded to `spec.frame_format` through the
/// gateway's palette rules ([`oxideav_image::Image::to_format`]): the
/// palette-alpha record yields alpha on an alpha target; on a target
/// without alpha, transparent entries are composited over
/// `spec.background` first (the gateway itself drops the alpha — a
/// written picture should not). Blobs and the colour record ride
/// along; a frame that is not `Pal8` is returned unchanged.
fn expand_palette(frame: Frame, spec: &TapSpec) -> Result<Frame> {
    use oxideav_image::Image;
    let Frame::Video(vf) = frame else {
        return Ok(frame);
    };
    if vf.palette().is_none() {
        return Ok(Frame::Video(vf));
    }
    let Some((w, h)) = spec.dims.or_else(|| frame_dims(&vf, PixelFormat::Pal8)) else {
        return Err(Error::unsupported(
            "convert: cannot size the palette frame to expand it",
        ));
    };
    let mut params = CodecParameters::video(CodecId::new(spec.stream.params.codec_id.as_str()));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(PixelFormat::Pal8);
    let img = Image::from_video_frame(vf, &params)
        .map_err(|e| Error::invalid(format!("convert: palette frame: {e}")))?;
    let target = spec.frame_format;
    let blobs = img.frame_blobs().to_vec();
    let has_alpha = FormatInfo::of(target).has_alpha;
    let out = if img.has_palette_alpha() && !has_alpha {
        let mut rgba = img
            .to_rgba8()
            .map_err(|e| Error::invalid(format!("convert: palette expansion: {e}")))?;
        composite_over(&mut rgba, spec.background);
        let flat = Image::from_rgba8(w, h, rgba)
            .map_err(|e| Error::invalid(format!("convert: composited frame: {e}")))?;
        flat.to_format(target)
    } else {
        img.to_format(target)
    }
    .map_err(|e| Error::invalid(format!("convert: palette → {target:?}: {e}")))?;
    let (mut out, _) = out.into_video_frame();
    if out.blobs().is_empty() && !blobs.is_empty() {
        out.set_blobs(blobs);
    }
    Ok(Frame::Video(out))
}

/// Composite packed RGBA over an opaque `bg` colour in place.
fn composite_over(rgba: &mut [u8], bg: [u8; 4]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a == 255 {
            continue;
        }
        let inv = 255 - a;
        for c in 0..3 {
            px[c] = ((u32::from(px[c]) * a + u32::from(bg[c]) * inv + 127) / 255) as u8;
        }
        px[3] = 255;
    }
}

/// `frame` without its metadata-blobs record.
fn strip_frame(frame: Frame) -> Frame {
    match frame {
        Frame::Video(mut vf) => {
            let _ = vf.take_blobs();
            Frame::Video(vf)
        }
        other => other,
    }
}

/// Frame dimensions from plane 0's geometry, for the tightly-packed
/// frames the filter ops and the converter produce: width = stride ÷
/// bytes per plane-0 sample group, height = bytes ÷ stride. `None`
/// for sub-byte layouts (1-bit mono), where the stride does not pin
/// the width.
pub fn frame_dims(frame: &oxideav_core::VideoFrame, fmt: PixelFormat) -> Option<(u32, u32)> {
    let plane = frame.image_planes().first()?;
    if plane.stride == 0 {
        return None;
    }
    let info = FormatInfo::of(fmt);
    let bytes_per_px = if info.is_planar {
        usize::from(info.bit_depth).div_ceil(8)
    } else {
        let bits = fmt.bits_per_pixel_approx() as usize;
        if bits % 8 != 0 {
            return None;
        }
        bits / 8
    };
    if bytes_per_px == 0 || plane.stride % bytes_per_px != 0 {
        return None;
    }
    let w = plane.stride / bytes_per_px;
    let h = plane.data.len() / plane.stride;
    Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{VideoFrame, VideoPlane};

    fn frame(planes: &[(usize, usize)]) -> VideoFrame {
        VideoFrame {
            pts: None,
            planes: planes
                .iter()
                .map(|&(stride, rows)| VideoPlane {
                    stride,
                    data: vec![0; stride * rows],
                })
                .collect(),
        }
    }

    #[test]
    fn packed_dims_come_from_the_stride() {
        assert_eq!(
            frame_dims(&frame(&[(150, 40)]), PixelFormat::Rgb24),
            Some((50, 40))
        );
        assert_eq!(
            frame_dims(&frame(&[(200, 40)]), PixelFormat::Rgba),
            Some((50, 40))
        );
        assert_eq!(
            frame_dims(&frame(&[(300, 40)]), PixelFormat::Rgb48Le),
            Some((50, 40))
        );
        assert_eq!(
            frame_dims(&frame(&[(50, 40)]), PixelFormat::Gray8),
            Some((50, 40))
        );
    }

    #[test]
    fn planar_dims_come_from_the_luma_plane() {
        assert_eq!(
            frame_dims(
                &frame(&[(64, 48), (32, 24), (32, 24)]),
                PixelFormat::Yuv420P
            ),
            Some((64, 48))
        );
        assert_eq!(
            frame_dims(
                &frame(&[(128, 48), (64, 24), (64, 24)]),
                PixelFormat::Yuv420P10Le
            ),
            Some((64, 48))
        );
    }

    #[test]
    fn frame_blobs_override_stream_blobs_of_their_kind() {
        use oxideav_core::{BlobKind, MetadataBlob};
        let mut f = frame(&[(3, 1)]);
        f.set_blobs(vec![MetadataBlob::new(BlobKind::EXIF, vec![1])]);
        let stream = vec![
            MetadataBlob::new(BlobKind::ICC, vec![2]),
            MetadataBlob::new(BlobKind::EXIF, vec![3]),
        ];
        let got = file_blobs(&f, &stream);
        assert_eq!(got.len(), 2);
        assert!(got[0].is(&BlobKind::EXIF) && got[0].data == [1]);
        assert!(got[1].is(&BlobKind::ICC) && got[1].data == [2]);
        assert!(file_blobs(&frame(&[(3, 1)]), &stream)
            .iter()
            .map(|b| b.data[0])
            .eq([2, 3]));
        let stripped = strip_frame(Frame::Video(f));
        let Frame::Video(v) = stripped else {
            panic!("video")
        };
        assert!(v.blobs().is_empty());
    }

    #[test]
    fn composite_over_blends_toward_the_background() {
        let mut px = vec![255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0, 128];
        composite_over(&mut px, [255, 255, 255, 255]);
        assert_eq!(&px[..4], &[255, 0, 0, 255], "opaque pixels untouched");
        assert_eq!(
            &px[4..8],
            &[255, 255, 255, 255],
            "fully transparent = background"
        );
        assert_eq!(&px[8..], &[127, 127, 127, 255], "half over white");
    }

    #[test]
    fn sub_byte_and_ragged_layouts_are_refused() {
        assert_eq!(frame_dims(&frame(&[(8, 4)]), PixelFormat::MonoBlack), None);
        assert_eq!(frame_dims(&frame(&[(10, 4)]), PixelFormat::Rgb24), None);
        assert_eq!(frame_dims(&frame(&[]), PixelFormat::Rgb24), None);
    }
}
