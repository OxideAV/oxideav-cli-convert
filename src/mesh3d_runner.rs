//! 3D-asset side-channel for the convert verb.
//!
//! When `convert` sees an input whose extension is `.stl` / `.obj` /
//! `.gltf` / `.glb` / `.usdz`, it bypasses the regular
//! [`oxideav_pipeline`] path (3D scenes don't fit the
//! `Frame::Video` / `Frame::Audio` shape that pipeline expects) and
//! routes the work through a [`oxideav_mesh3d::Mesh3DRegistry`]
//! pre-populated by [`oxideav_meta::populate_mesh3d_registry`]:
//!
//! 1. Read the input bytes.
//! 2. Pick a decoder by input extension; decode → [`Scene3D`].
//! 3. Pick an encoder by output extension; encode → bytes.
//! 4. Write bytes to the output path.
//!
//! The convert verb owns the routing here so the rest of the workspace
//! can stay agnostic of the 3D dispatch contract (which uses a
//! separate registry from `RuntimeContext`'s codec/container path).
//!
//! Cargo-feature-gated on `mesh3d`. With the feature off, this module
//! disappears and the convert verb falls through to the regular
//! pipeline path for 3D inputs (which then errors out cleanly because
//! no demuxer claims them).

use std::fs;

use oxideav_core::{Error, Result};
use oxideav_mesh3d::Mesh3DRegistry;

/// File extensions the 3D side-channel claims as inputs. Mirrors the
/// extension lists the four sibling format crates (stl/obj/gltf/usdz)
/// register with the `Mesh3DRegistry`. Centralised here so input
/// recognition stays a single source of truth.
const MESH3D_INPUT_EXTS: &[&str] = &["stl", "obj", "gltf", "glb", "usdz", "mtl"];

/// File extensions the 3D side-channel can emit. Same set minus
/// `usdz` (read-only today; the `oxideav-usdz` crate ships a decoder
/// but no encoder factory).
const MESH3D_OUTPUT_EXTS: &[&str] = &["stl", "obj", "gltf", "glb", "mtl"];

/// Returns `true` when the input path's extension matches one of the
/// 3D formats wired into the [`Mesh3DRegistry`]. Case-insensitive.
pub fn is_mesh3d_input(input: &str) -> bool {
    matches_known_ext(input, MESH3D_INPUT_EXTS)
}

/// Returns `true` when the output path's extension matches one of the
/// 3D formats the side-channel can emit. Used by `lib.rs::run` to
/// reject a 3D input paired with a non-3D output before the encoder
/// lookup fails further in.
pub fn is_mesh3d_output(output: &str) -> bool {
    matches_known_ext(output, MESH3D_OUTPUT_EXTS)
}

fn matches_known_ext(path: &str, set: &[&str]) -> bool {
    let Some(ext) = ext_of(path) else {
        return false;
    };
    let lc = ext.to_ascii_lowercase();
    set.iter().any(|e| *e == lc)
}

fn ext_of(path: &str) -> Option<&str> {
    // Strip any trailing query string a generator-style URI might
    // tack on; then look at the last `.` of the basename.
    let last = path.rsplit('/').next().unwrap_or(path);
    let last = last.split('?').next().unwrap_or(last);
    let dot = last.rfind('.')?;
    Some(&last[dot + 1..])
}

/// Run the 3D-input convert flow. Side-effect-only: writes one file
/// to disk.
///
/// Output extension → encoder picked from the registry. Input extension
/// → decoder picked from the registry. Decoders that don't accept the
/// bytes (e.g. ASCII OBJ fed to the binary STL decoder via a renamed
/// extension) propagate an `Error::Invalid` from the decoder up.
pub fn run(input_path: &str, output_path: &str) -> Result<()> {
    let in_ext = ext_of(input_path)
        .ok_or_else(|| {
            Error::invalid(format!(
                "convert: input '{input_path}' has no extension — cannot pick a 3D decoder"
            ))
        })?
        .to_ascii_lowercase();
    let out_ext = ext_of(output_path)
        .ok_or_else(|| {
            Error::invalid(format!(
                "convert: output '{output_path}' has no extension — cannot pick a 3D encoder"
            ))
        })?
        .to_ascii_lowercase();

    let mut registry = Mesh3DRegistry::new();
    oxideav_meta::populate_mesh3d_registry(&mut registry);

    let mut decoder = registry.decoder_for_extension(&in_ext).ok_or_else(|| {
        Error::unsupported(format!(
            "convert: no 3D decoder registered for input extension '.{in_ext}' (known: {})",
            joined_known_inputs()
        ))
    })?;
    let mut encoder = registry.encoder_for_extension(&out_ext).ok_or_else(|| {
        Error::unsupported(format!(
            "convert: no 3D encoder registered for output extension '.{out_ext}' (known: {})",
            joined_known_outputs()
        ))
    })?;

    let bytes = fs::read(input_path)
        .map_err(|e| Error::invalid(format!("convert: failed to read {input_path}: {e}")))?;
    let scene = decoder.decode(&bytes)?;
    let out_bytes = encoder.encode(&scene)?;
    fs::write(output_path, out_bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to write {output_path}: {e}")))?;
    Ok(())
}

fn joined_known_inputs() -> String {
    MESH3D_INPUT_EXTS
        .iter()
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn joined_known_outputs() -> String {
    MESH3D_OUTPUT_EXTS
        .iter()
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_mesh3d_extensions_case_insensitively() {
        assert!(is_mesh3d_input("foo.stl"));
        assert!(is_mesh3d_input("FOO.STL"));
        assert!(is_mesh3d_input("path/to/cube.obj"));
        assert!(is_mesh3d_input("scene.gltf"));
        assert!(is_mesh3d_input("scene.GLB"));
        assert!(is_mesh3d_input("archive.usdz"));
        assert!(is_mesh3d_input("materials.mtl"));
        assert!(!is_mesh3d_input("foo.png"));
        assert!(!is_mesh3d_input("foo.pdf"));
        assert!(!is_mesh3d_input("noext"));
    }

    #[test]
    fn detects_mesh3d_output_extensions() {
        assert!(is_mesh3d_output("out.stl"));
        assert!(is_mesh3d_output("out.obj"));
        assert!(is_mesh3d_output("out.gltf"));
        assert!(is_mesh3d_output("out.glb"));
        assert!(is_mesh3d_output("out.mtl"));
        // USDZ is read-only today (decoder only, no encoder).
        assert!(!is_mesh3d_output("out.usdz"));
        assert!(!is_mesh3d_output("out.png"));
    }

    #[test]
    fn unknown_input_extension_errors_with_known_set() {
        // Pick an extension that's neither 3D nor present on disk so
        // we can check the error message without wiring real fixtures.
        let err = run("/tmp/does-not-exist.xyz", "/tmp/out.stl").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no 3D decoder registered for input extension '.xyz'"),
            "message was: {msg}"
        );
    }

    #[test]
    fn unknown_output_extension_errors_with_known_set() {
        let err = run("/tmp/does-not-exist.stl", "/tmp/out.xyz").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no 3D encoder registered for output extension '.xyz'"),
            "message was: {msg}"
        );
    }
}
