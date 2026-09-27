//! Frame tap: the still-image write path `convert` owns itself.
//!
//! The regular pipeline route hands the whole job — decode, filters,
//! conversion, encode, mux — to the executor. Two still-image features
//! need the encode + mux stages on this side instead:
//!
//! * **Encoder options** (`--opt KEY=VALUE`, and the `.avif` ⇒
//!   `codec=av1` inference). The encoder is built here with the
//!   options in its [`CodecParameters::options`] bag, validated
//!   against the encoder's declared schema at plan time.
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
    CodecId, CodecOptions, CodecParameters, Error, Frame, MediaType, Packet, PixelFormat, Result,
    RuntimeContext, StreamInfo,
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
}

/// What the tap wrote.
#[derive(Clone, Copy, Debug, Default)]
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
) -> Result<(oxideav_pipeline::executor::ExecutorStats, TapStats)> {
    let (tx, rx) = sync_channel::<Frame>(2);
    let start = Instant::now();
    std::thread::scope(|s| {
        let writer = s.spawn(move || write_frames(rx, ctx, spec, start));
        let sink = ChannelSink { tx: Some(tx) };
        let exec = Executor::new(job, ctx)
            .with_sink_override(TAP_SINK, Box::new(sink))
            .run();
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(Error::other("convert: frame writer panicked")));
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
                    let mut f = OpenFile::create(ctx, spec, &path, w, h, &mut stats)?;
                    f.push(&frame, &mut stats)?;
                    f.finish(&mut stats)?;
                    stats.files += 1;
                }
                None => {
                    if single.is_none() {
                        single = Some(OpenFile::create(ctx, spec, &spec.output, w, h, &mut stats)?);
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
    fn create(
        ctx: &RuntimeContext,
        spec: &TapSpec,
        path: &str,
        w: u32,
        h: u32,
        stats: &mut TapStats,
    ) -> Result<OpenFile> {
        let mut params = CodecParameters::video(CodecId::new(spec.codec.as_str()));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(spec.frame_format);
        params.frame_rate = spec.stream.params.frame_rate;
        let mut options = CodecOptions::new();
        for (k, v) in &spec.options {
            options.insert(k.as_str(), v.as_str());
        }
        params.options = options;
        let t = Instant::now();
        let encoder = oxideav_pipeline::selection::make_encoder(&ctx.codecs, &params)
            .map_err(|e| Error::invalid(format!("convert: {} encoder: {e}", spec.codec)))?;
        stats.encode += t.elapsed();

        let mut stream = spec.stream.clone();
        stream.index = 0;
        stream.params = encoder.output_params().clone();
        let t = Instant::now();
        let file = File::create(path)
            .map_err(|e| Error::invalid(format!("convert: failed to create {path}: {e}")))?;
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
    fn sub_byte_and_ragged_layouts_are_refused() {
        assert_eq!(frame_dims(&frame(&[(8, 4)]), PixelFormat::MonoBlack), None);
        assert_eq!(frame_dims(&frame(&[(10, 4)]), PixelFormat::Rgb24), None);
        assert_eq!(frame_dims(&frame(&[]), PixelFormat::Rgb24), None);
    }
}
