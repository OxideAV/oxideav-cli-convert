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
//! ## Per-format encoder options
//!
//! When the caller supplies a per-format option flag (`-stl-format`,
//! `-gltf-format`, …), the encoder lookup bypasses the
//! [`Mesh3DRegistry`]'s parameter-less factory and constructs a
//! typed encoder directly via the format crate (`oxideav_stl`,
//! `oxideav_gltf`). The decoder lookup always goes through the
//! registry — input options aren't a thing yet.
//!
//! Cargo-feature-gated on `mesh3d`. With the feature off, this module
//! disappears and the convert verb falls through to the regular
//! pipeline path for 3D inputs (which then errors out cleanly because
//! no demuxer claims them).

use std::fs;

use oxideav_core::{Error, Result};
use oxideav_mesh3d::{Mesh3DEncoder, Mesh3DRegistry};

use crate::op::{GltfFormatChoice, Mesh3DOptions, StlFormatChoice};

/// File extensions the 3D side-channel claims as inputs. Mirrors the
/// extension lists the four sibling format crates (stl/obj/gltf/usdz)
/// register with the `Mesh3DRegistry`. Centralised here so input
/// recognition stays a single source of truth.
const MESH3D_INPUT_EXTS: &[&str] = &["stl", "obj", "gltf", "glb", "usdz", "mtl"];

/// File extensions the 3D side-channel can emit. Matches the input
/// set: `oxideav-usdz` ships a [`UsdzEncoder`](oxideav_usdz::UsdzEncoder)
/// alongside its decoder, so USDZ is now a round-trip target as
/// well.
const MESH3D_OUTPUT_EXTS: &[&str] = &["stl", "obj", "gltf", "glb", "mtl", "usdz"];

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
/// Output extension → encoder picked from the registry, OR constructed
/// directly via the format crate when [`Mesh3DOptions`] requests a
/// non-default flavour. Input extension → decoder picked from the
/// registry. Decoders that don't accept the bytes (e.g. ASCII OBJ fed
/// to the binary STL decoder via a renamed extension) propagate an
/// `Error::Invalid` from the decoder up.
pub fn run(input_path: &str, output_path: &str, options: &Mesh3DOptions) -> Result<()> {
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

    // Reject mismatched per-format flags up-front (e.g. `-stl-format ascii`
    // paired with a `.glb` output) so the user sees the actual misuse, not
    // a downstream "unknown encoder" message.
    validate_options_against_output(&out_ext, options)?;

    let mut decoder = registry.decoder_for_extension(&in_ext).ok_or_else(|| {
        let hint = crate::suggest::format_hint(&in_ext, MESH3D_INPUT_EXTS);
        Error::unsupported(format!(
            "convert: no 3D decoder registered for input extension '.{in_ext}'{hint} (known: {})",
            joined_known_inputs()
        ))
    })?;
    let mut encoder = pick_encoder(&registry, &out_ext, options)?;

    let bytes = fs::read(input_path)
        .map_err(|e| Error::invalid(format!("convert: failed to read {input_path}: {e}")))?;
    let scene = decoder.decode(&bytes)?;
    let out_bytes = encoder.encode(&scene)?;
    fs::write(output_path, out_bytes)
        .map_err(|e| Error::invalid(format!("convert: failed to write {output_path}: {e}")))?;
    Ok(())
}

/// Reject per-format option flags that don't apply to the chosen
/// output extension — e.g. `-stl-format ascii` with a `.obj` output.
/// Per IM convention we surface this as a clear error rather than
/// silently dropping the flag.
fn validate_options_against_output(out_ext: &str, options: &Mesh3DOptions) -> Result<()> {
    if options.stl_format.is_some() && out_ext != "stl" {
        return Err(Error::invalid(format!(
            "convert: -stl-format set but output extension is '.{out_ext}', not '.stl'"
        )));
    }
    if options.gltf_format.is_some() && out_ext != "gltf" && out_ext != "glb" {
        return Err(Error::invalid(format!(
            "convert: -gltf-format set but output extension is '.{out_ext}', not '.gltf' or '.glb'"
        )));
    }
    Ok(())
}

/// Pick the encoder for `out_ext`, honouring [`Mesh3DOptions`] when a
/// per-format flag overrides the registry default.
///
/// The default path stays through the registry so adding a new 3D
/// format requires zero churn here.
fn pick_encoder(
    registry: &Mesh3DRegistry,
    out_ext: &str,
    options: &Mesh3DOptions,
) -> Result<Box<dyn Mesh3DEncoder>> {
    match out_ext {
        "stl" => {
            if let Some(choice) = options.stl_format {
                return Ok(build_stl_encoder(choice));
            }
        }
        "gltf" | "glb" => {
            if let Some(choice) = options.gltf_format {
                return build_gltf_encoder(choice);
            }
        }
        _ => {}
    }
    registry.encoder_for_extension(out_ext).ok_or_else(|| {
        let hint = crate::suggest::format_hint(out_ext, MESH3D_OUTPUT_EXTS);
        Error::unsupported(format!(
            "convert: no 3D encoder registered for output extension '.{out_ext}'{hint} (known: {})",
            joined_known_outputs()
        ))
    })
}

/// Construct an STL encoder for the given user choice. Bypasses the
/// registry so we can pass the `StlFormat` constructor argument that
/// the registry's parameter-less factory closure can't.
fn build_stl_encoder(choice: StlFormatChoice) -> Box<dyn Mesh3DEncoder> {
    use oxideav_stl::encoder::{StlEncoder, StlFormat};
    let format = match choice {
        StlFormatChoice::Binary => StlFormat::Binary,
        StlFormatChoice::Ascii => StlFormat::Ascii,
    };
    Box::new(StlEncoder::new(format))
}

/// Construct a glTF encoder for the given user choice. `JsonExternal`
/// is rejected with a clear gltf-rN follow-up message until the
/// upstream `OutputFlavour::JsonExternal` variant lands in published
/// `oxideav-gltf`.
fn build_gltf_encoder(choice: GltfFormatChoice) -> Result<Box<dyn Mesh3DEncoder>> {
    use oxideav_gltf::{GltfEncoder, OutputFlavour};
    let flavour = match choice {
        GltfFormatChoice::Glb => OutputFlavour::Glb,
        GltfFormatChoice::JsonEmbedded => OutputFlavour::JsonEmbedded,
        GltfFormatChoice::JsonExternal => {
            return Err(Error::unsupported(
                "convert: -gltf-format external (JSON + sidecar .bin) is not yet wired (gltf-rN follow-up: OutputFlavour::JsonExternal not in published oxideav-gltf)"
                    .to_string(),
            ));
        }
    };
    Ok(Box::new(GltfEncoder::with_output(flavour)))
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
        assert!(is_mesh3d_output("out.usdz"));
        assert!(is_mesh3d_output("out.USDZ"));
        assert!(!is_mesh3d_output("out.png"));
    }

    #[test]
    fn unknown_input_extension_errors_with_known_set() {
        // Pick an extension that's neither 3D nor present on disk so
        // we can check the error message without wiring real fixtures.
        let err = run(
            "/tmp/does-not-exist.xyz",
            "/tmp/out.stl",
            &Mesh3DOptions::default(),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no 3D decoder registered for input extension '.xyz'"),
            "message was: {msg}"
        );
    }

    #[test]
    fn unknown_output_extension_errors_with_known_set() {
        let err = run(
            "/tmp/does-not-exist.stl",
            "/tmp/out.xyz",
            &Mesh3DOptions::default(),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no 3D encoder registered for output extension '.xyz'"),
            "message was: {msg}"
        );
    }

    #[test]
    fn unknown_input_extension_suggests_close_match() {
        // `.gtlf` is one transposition away from `.gltf` — the
        // did-you-mean clause should fire.
        let err = run(
            "/tmp/does-not-exist.gtlf",
            "/tmp/out.stl",
            &Mesh3DOptions::default(),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("did you mean '.gltf'?"),
            "expected did-you-mean hint, message was: {msg}"
        );
    }

    #[test]
    fn unknown_output_extension_suggests_close_match() {
        // `.glft` is one transposition from `.gltf`.
        let err = run(
            "/tmp/does-not-exist.stl",
            "/tmp/out.glft",
            &Mesh3DOptions::default(),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("did you mean '.gltf'?"),
            "expected did-you-mean hint, message was: {msg}"
        );
    }

    #[test]
    fn unrelated_unknown_extension_no_hint() {
        // `.png` is too far from any 3D extension — no hint should be
        // appended.
        let err = run(
            "/tmp/does-not-exist.png",
            "/tmp/out.stl",
            &Mesh3DOptions::default(),
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("did you mean"),
            "expected no hint, message was: {msg}"
        );
    }
}
