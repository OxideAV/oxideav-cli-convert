//! Per-container default codecs for multi-codec outputs.
//!
//! An output extension that names a container rather than a codec
//! (`.webm`, `.mkv`, `.mp4`, `.wav`, `.y4m`, `.ogg`) leaves the codec choice to the
//! planner. Streams whose codec the container can store are
//! stream-copied (the historical behaviour); a stream it cannot store
//! (raw video into WebM, PCM into Ogg, H.264 into WebM, …) is
//! re-encoded with the container's default codec for that media type,
//! so `convert in.y4m out.webm` works without naming a codec.
//!
//! The table is deliberately small: a container is listed only where
//! the right default is unambiguous for this framework's encoders.

use oxideav_core::{CodecId, MediaType, RuntimeContext, StreamInfo};

/// Which source codecs a container stores as-is for one media type.
enum Storable {
    /// Exactly these codec ids.
    Only(&'static [&'static str]),
    /// Every codec except these (uncompressed payloads the container
    /// has no mapping for).
    AllBut(&'static [&'static str]),
}

impl Storable {
    fn accepts(&self, codec: &str) -> bool {
        match self {
            Storable::Only(ids) => ids.contains(&codec),
            Storable::AllBut(ids) => !ids.contains(&codec),
        }
    }
}

/// `(container, media type, storable source codecs, default codec)`.
const DEFAULTS: &[(&str, MediaType, Storable, &str)] = &[
    // WebM: VP8 / VP9 / AV1 video, Opus / Vorbis audio only.
    (
        "webm",
        MediaType::Video,
        Storable::Only(&["vp8", "vp9", "av1"]),
        "vp9",
    ),
    (
        "webm",
        MediaType::Audio,
        Storable::Only(&["opus", "vorbis"]),
        "opus",
    ),
    // Matroska stores nearly any codec; raw pictures get VP9.
    (
        "matroska",
        MediaType::Video,
        Storable::AllBut(&["rawvideo"]),
        "vp9",
    ),
    // Ogg audio: the codecs with an Ogg mapping; others get Vorbis.
    (
        "ogg",
        MediaType::Audio,
        Storable::Only(&["vorbis", "opus", "flac", "speex"]),
        "vorbis",
    ),
    // WAV (this framework's muxer) stores integer / float PCM only.
    (
        "wav",
        MediaType::Audio,
        Storable::Only(&[
            "pcm_u8",
            "pcm_s16le",
            "pcm_s24le",
            "pcm_s32le",
            "pcm_f32le",
            "pcm_f64le",
        ]),
        "pcm_s16le",
    ),
    // MP4: raw pictures get H.264.
    (
        "mp4",
        MediaType::Video,
        Storable::AllBut(&["rawvideo"]),
        "h264",
    ),
    // YUV4MPEG2 stores raw pictures only.
    (
        "y4m",
        MediaType::Video,
        Storable::Only(&["rawvideo"]),
        "rawvideo",
    ),
];

/// The codec a `kind` stream of `source_codec` should be encoded with
/// for `container`: `Some(Some(codec))` = re-encode, `Some(None)` =
/// stream-copy, `None` = no table entry (keep the caller's default).
/// A default codec with no registered encoder counts as "no entry".
pub fn track_codec(
    ctx: &RuntimeContext,
    container: &str,
    kind: MediaType,
    source_codec: &str,
) -> Option<Option<String>> {
    let (_, _, storable, default) = DEFAULTS
        .iter()
        .find(|(c, k, _, _)| *c == container && *k == kind)?;
    if storable.accepts(source_codec) {
        return Some(None);
    }
    ctx.codecs
        .has_encoder(&CodecId::new(*default))
        .then(|| Some((*default).to_string()))
}

/// Per-stream codec plan for `streams` written to `container`, or
/// `None` when every stream is stored as-is (or the container has no
/// table entry) — the caller then keeps its stream-copy job.
pub fn plan_tracks(
    ctx: &RuntimeContext,
    container: &str,
    streams: &[StreamInfo],
) -> Option<Vec<(MediaType, u32, Option<String>)>> {
    let mut any_encode = false;
    let mut ordinals: [u32; 2] = [0, 0];
    let mut out = Vec::new();
    for s in streams {
        let kind = s.params.media_type;
        let slot = match kind {
            MediaType::Video => 0,
            MediaType::Audio => 1,
            _ => continue,
        };
        let ordinal = ordinals[slot];
        ordinals[slot] += 1;
        let codec = track_codec(ctx, container, kind, s.params.codec_id.as_str()).unwrap_or(None);
        any_encode |= codec.is_some();
        out.push((kind, ordinal, codec));
    }
    any_encode.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecCapabilities, CodecInfo, CodecParameters, TimeBase};

    fn ctx_with_encoders(ids: &[&str]) -> RuntimeContext {
        let mut ctx = RuntimeContext::new();
        for id in ids {
            ctx.codecs.register(
                CodecInfo::new(CodecId::new(*id))
                    .capabilities(CodecCapabilities::video(*id).with_encode())
                    .encoder(|_| Err(oxideav_core::Error::unsupported("stub"))),
            );
        }
        ctx
    }

    fn stream(index: u32, kind: MediaType, codec: &str) -> StreamInfo {
        let mut params = match kind {
            MediaType::Audio => CodecParameters::audio(CodecId::new(codec)),
            _ => CodecParameters::video(CodecId::new(codec)),
        };
        params.media_type = kind;
        StreamInfo {
            index,
            time_base: TimeBase::new(1, 25),
            duration: None,
            start_time: Some(0),
            params,
        }
    }

    #[test]
    fn webm_reencodes_what_it_cannot_store() {
        let ctx = ctx_with_encoders(&["vp9", "opus"]);
        let streams = [
            stream(0, MediaType::Video, "rawvideo"),
            stream(1, MediaType::Audio, "pcm_s16le"),
        ];
        let plan = plan_tracks(&ctx, "webm", &streams).expect("needs encoding");
        assert_eq!(
            plan,
            vec![
                (MediaType::Video, 0, Some("vp9".to_string())),
                (MediaType::Audio, 0, Some("opus".to_string())),
            ]
        );
    }

    #[test]
    fn storable_streams_keep_the_copy_job() {
        let ctx = ctx_with_encoders(&["vp9", "opus"]);
        let streams = [
            stream(0, MediaType::Video, "vp9"),
            stream(1, MediaType::Audio, "opus"),
        ];
        assert_eq!(plan_tracks(&ctx, "webm", &streams), None);
        // Matroska copies everything but raw pictures.
        let streams = [stream(0, MediaType::Video, "h264")];
        assert_eq!(plan_tracks(&ctx, "matroska", &streams), None);
        let streams = [stream(0, MediaType::Video, "rawvideo")];
        assert_eq!(
            plan_tracks(&ctx, "matroska", &streams),
            Some(vec![(MediaType::Video, 0, Some("vp9".to_string()))])
        );
    }

    #[test]
    fn missing_default_encoder_or_unknown_container_is_no_entry() {
        let ctx = ctx_with_encoders(&[]);
        assert_eq!(
            track_codec(&ctx, "webm", MediaType::Video, "rawvideo"),
            None
        );
        let ctx = ctx_with_encoders(&["vp9"]);
        assert_eq!(track_codec(&ctx, "avi", MediaType::Video, "rawvideo"), None);
        assert_eq!(
            track_codec(&ctx, "webm", MediaType::Video, "vp9"),
            Some(None)
        );
    }
}
