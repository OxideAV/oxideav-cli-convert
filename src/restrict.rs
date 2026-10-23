//! Input restrictions: allow / deny lists over container names and
//! codec ids, for converting untrusted files inside a sandbox.
//!
//! The lists are the ones the `oxideav-image` gateway enforces
//! (`OpenOptions::with_allow_containers` and friends — names compare
//! case-insensitively, a deny list wins over an allow list, `None`
//! allows everything): [`Restrictions::apply`] hands them to a gateway
//! open, and [`Restrictions::check_container`] /
//! [`Restrictions::check_streams`] apply the same rules to the
//! pipeline route's own header probe, before any decoder exists. A
//! refused input is `Error::Unsupported` whose message starts with
//! `convert: restricted:` ([`is_restricted`]), the pipeline-side
//! counterpart of the gateway's `ImageError::Restricted`.

use oxideav_core::{Error, MediaType, Result, StreamInfo};
use oxideav_image::OpenOptions;

/// Allow / deny lists for the input (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Restrictions {
    /// Containers the input may be; `None` = any.
    pub allow_containers: Option<Vec<String>>,
    /// Containers refused even when allowed above.
    pub deny_containers: Vec<String>,
    /// Codec ids every video stream must be among; `None` = any.
    pub allow_codecs: Option<Vec<String>>,
    /// Codec ids refused even when allowed above.
    pub deny_codecs: Vec<String>,
}

/// Message prefix of a refusal.
const PREFIX: &str = "convert: restricted:";

fn lower<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    names
        .into_iter()
        .map(|s| s.into().trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Restrictions {
    /// No restriction.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when nothing is restricted.
    pub fn is_empty(&self) -> bool {
        self.allow_containers.is_none()
            && self.deny_containers.is_empty()
            && self.allow_codecs.is_none()
            && self.deny_codecs.is_empty()
    }

    /// Only these containers (added to an existing allow list).
    pub fn with_allow_containers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_containers
            .get_or_insert_with(Vec::new)
            .extend(lower(names));
        self
    }

    /// Never these containers.
    pub fn with_deny_containers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_containers.extend(lower(names));
        self
    }

    /// Only these codec ids (every video stream).
    pub fn with_allow_codecs<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_codecs
            .get_or_insert_with(Vec::new)
            .extend(lower(ids));
        self
    }

    /// Never these codec ids.
    pub fn with_deny_codecs<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_codecs.extend(lower(ids));
        self
    }

    /// The gateway open options with these lists set.
    pub fn apply(&self, mut opts: OpenOptions) -> OpenOptions {
        if let Some(a) = &self.allow_containers {
            opts = opts.with_allow_containers(a.iter().cloned());
        }
        if !self.deny_containers.is_empty() {
            opts = opts.with_deny_containers(self.deny_containers.iter().cloned());
        }
        if let Some(a) = &self.allow_codecs {
            opts = opts.with_allow_codecs(a.iter().cloned());
        }
        if !self.deny_codecs.is_empty() {
            opts = opts.with_deny_codecs(self.deny_codecs.iter().cloned());
        }
        opts
    }

    /// Refuse `container` (a registered container name) when the lists
    /// say so.
    pub fn check_container(&self, container: &str) -> Result<()> {
        let name = container.to_ascii_lowercase();
        if self.deny_containers.contains(&name) {
            return Err(Error::unsupported(format!(
                "{PREFIX} container '{container}' is denied"
            )));
        }
        if let Some(allow) = &self.allow_containers
            && !allow.contains(&name)
        {
            return Err(Error::unsupported(format!(
                "{PREFIX} container '{container}' is not in the allow list ({})",
                allow.join(", ")
            )));
        }
        Ok(())
    }

    /// Refuse the input when any video stream's codec id is denied or
    /// outside the allow list (audio and data streams are not
    /// decoded by `convert`'s still-image paths and are not checked).
    pub fn check_streams(&self, streams: &[StreamInfo]) -> Result<()> {
        if self.allow_codecs.is_none() && self.deny_codecs.is_empty() {
            return Ok(());
        }
        for s in streams
            .iter()
            .filter(|s| s.params.media_type == MediaType::Video)
        {
            let id = s.params.codec_id.as_str().to_ascii_lowercase();
            if self.deny_codecs.contains(&id) {
                return Err(Error::unsupported(format!(
                    "{PREFIX} codec '{id}' of video stream #{} is denied",
                    s.index
                )));
            }
            if let Some(allow) = &self.allow_codecs
                && !allow.contains(&id)
            {
                return Err(Error::unsupported(format!(
                    "{PREFIX} codec '{id}' of video stream #{} is not in the allow list ({})",
                    s.index,
                    allow.join(", ")
                )));
            }
        }
        Ok(())
    }
}

/// True for an error produced by a refusal — this module's, or the
/// gateway's `ImageError::Restricted` as `crate::raster_io::gateway_error`
/// maps it.
pub fn is_restricted(e: &Error) -> bool {
    matches!(e, Error::Unsupported(m) if m.starts_with(PREFIX) || m.contains(": restricted: "))
}

/// Parse one `--allow-containers` / `--deny-*` value: a comma- or
/// whitespace-separated list of names.
pub fn parse_list(v: &str) -> Vec<String> {
    v.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters, TimeBase};

    fn stream(index: u32, codec: &str, kind: MediaType) -> StreamInfo {
        let mut params = CodecParameters::video(CodecId::new(codec));
        params.media_type = kind;
        StreamInfo {
            index,
            time_base: TimeBase::new(1, 1),
            duration: Some(1),
            start_time: Some(0),
            params,
        }
    }

    #[test]
    fn deny_wins_and_names_are_case_insensitive() {
        let r = Restrictions::new()
            .with_allow_containers(["PNG", "heif"])
            .with_deny_containers(["Heif"]);
        assert!(r.check_container("png").is_ok());
        assert!(is_restricted(&r.check_container("heif").unwrap_err()));
        assert!(is_restricted(&r.check_container("gif").unwrap_err()));
        assert!(!r.is_empty());
        assert!(Restrictions::new().is_empty());
        assert!(Restrictions::new().check_container("anything").is_ok());
    }

    #[test]
    fn codecs_are_checked_on_video_streams_only() {
        let r = Restrictions::new().with_deny_codecs(["hevc"]);
        let ok = vec![
            stream(0, "png", MediaType::Video),
            stream(1, "hevc", MediaType::Audio),
        ];
        assert!(r.check_streams(&ok).is_ok());
        let bad = vec![stream(0, "HEVC", MediaType::Video)];
        let e = r.check_streams(&bad).unwrap_err();
        assert!(is_restricted(&e), "{e}");
        let allow = Restrictions::new().with_allow_codecs(["png"]);
        assert!(allow.check_streams(&ok).is_ok());
        assert!(
            allow
                .check_streams(&[stream(0, "mjpeg", MediaType::Video)])
                .is_err()
        );
    }

    #[test]
    fn apply_sets_the_gateway_lists() {
        let r = Restrictions::new()
            .with_allow_containers(["png"])
            .with_deny_codecs(["hevc", "av1"]);
        let o = r.apply(OpenOptions::new());
        assert_eq!(
            o.allow_containers.as_deref(),
            Some(&["png".to_string()][..])
        );
        assert!(o.deny_containers.is_empty());
        assert_eq!(o.allow_codecs, None);
        assert_eq!(o.deny_codecs, ["hevc", "av1"]);
    }

    #[test]
    fn list_values_split_on_commas_and_spaces() {
        assert_eq!(parse_list("png, heif gif,,"), ["png", "heif", "gif"]);
        assert!(parse_list(" , ").is_empty());
    }

    #[test]
    fn other_errors_are_not_restrictions() {
        assert!(!is_restricted(&Error::unsupported("convert: no encoder")));
        assert!(!is_restricted(&Error::invalid(
            "convert: restricted: not this variant"
        )));
    }
}
