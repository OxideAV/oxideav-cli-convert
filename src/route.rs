//! Pure routing layer: decide which execution path a parsed
//! [`ConvertPlan`] takes, without performing any conversion work.
//!
//! [`crate::run`] used to make these decisions inline, which meant the
//! media-kind × media-kind dispatch matrix could only be exercised by
//! actually running conversions. Extracting the decision into
//! [`decide`] keeps `run` a thin dispatcher and lets tests pin every
//! cell of the matrix with nothing but a `ConvertPlan`.
//!
//! Precedence (first match wins), mirroring the historical inline
//! order:
//!
//! 1. `-ping` — header-only inspection, no decode.
//! 2. `--probe` — structural inspection, no output write.
//! 3. PDF input — Scene-aware side-channel (`pdf_runner`).
//! 4. 3D-asset input (`mesh3d` feature) — same-class re-encode,
//!    3D→raster render, or a typed pairing error.
//! 5. `.ico` output (`ico` feature) — multi-resolution icon writer.
//! 6. Everything else — the regular `oxideav-pipeline` path.
//!
//! The precedence order is observable behaviour: `in.pdf out.ico`
//! routes to the PDF side-channel (which then rejects `.ico` as an
//! unsupported output class), NOT to the ICO writer, because input
//! classification outranks output classification.

use crate::op::ConvertPlan;
use oxideav_core::Error;

/// One cell of the routing matrix — the execution path [`crate::run`]
/// dispatches to for a given plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `-ping`: print one IM-format header line per image/stream,
    /// no pixel decode, no output write.
    Ping,
    /// `--probe`: decode far enough for structural metadata, print a
    /// summary, no output write.
    Probe,
    /// PDF input: Scene-aware runner (vector-preserving PDF/SVG
    /// output, or per-page rasterisation).
    PdfSideChannel,
    /// 3D-asset input paired with a 3D-asset output: decode →
    /// re-encode through the `Mesh3DRegistry`.
    #[cfg(feature = "mesh3d")]
    Mesh3dToMesh3d,
    /// 3D-asset input paired with a raster output: software-render
    /// the scene, then apply raster ops and encode.
    #[cfg(feature = "mesh3d")]
    Mesh3dToRaster,
    /// `.ico` output on a non-PDF, non-3D input: multi-resolution
    /// icon writer driven by `-define icon:auto-resize=…`.
    #[cfg(feature = "ico")]
    IcoOutput,
    /// The regular path: [`crate::plan_to_job`] → pipeline executor.
    Pipeline,
}

impl Route {
    /// Stable lowercase tag, handy for log lines and test diagnostics.
    pub fn as_tag(&self) -> &'static str {
        match self {
            Route::Ping => "ping",
            Route::Probe => "probe",
            Route::PdfSideChannel => "pdf",
            #[cfg(feature = "mesh3d")]
            Route::Mesh3dToMesh3d => "mesh3d-to-mesh3d",
            #[cfg(feature = "mesh3d")]
            Route::Mesh3dToRaster => "mesh3d-to-raster",
            #[cfg(feature = "ico")]
            Route::IcoOutput => "ico",
            Route::Pipeline => "pipeline",
        }
    }
}

/// Classify a plan into its execution [`Route`], or return the typed
/// error for unroutable pairings (3D input × non-3D/non-raster
/// output, `%d` template on a single-document 3D scene).
///
/// Pure with one caveat: PDF-input detection falls back to sniffing
/// the file's magic bytes when the extension isn't `.pdf`, so an
/// extensionless path that exists on disk may be classified by
/// content. Paths that don't exist are classified by extension alone.
pub fn decide(plan: &ConvertPlan) -> Result<Route, Error> {
    // `-ping` short-circuits everything, including `--probe`.
    if plan.ping {
        return Ok(Route::Ping);
    }
    if plan.probe {
        return Ok(Route::Probe);
    }

    // Input classification outranks output classification: a PDF
    // input always takes the Scene-aware side-channel, whatever the
    // output extension says.
    if crate::pdf_runner::is_pdf_input(&plan.input) {
        return Ok(Route::PdfSideChannel);
    }

    #[cfg(feature = "mesh3d")]
    if crate::mesh3d_runner::is_mesh3d_input(&plan.input) {
        if plan.output_template.is_some() {
            return Err(Error::invalid(format!(
                "convert: output '{}' has a `%d` template but 3D scenes are single-document; remove the template",
                plan.output
            )));
        }
        if crate::mesh3d_runner::is_mesh3d_output(&plan.output) {
            return Ok(Route::Mesh3dToMesh3d);
        }
        if matches!(
            crate::raster_io::classify_output(&plan.output),
            Ok(crate::raster_io::OutputClass::Raster(_))
        ) {
            return Ok(Route::Mesh3dToRaster);
        }
        // Anything else (e.g. .pdf, .svg, .mp4, …): pairing-mismatch
        // error with a did-you-mean hint over the 3D-output set.
        let out_ext = plan.output.rsplit('.').next().unwrap_or("");
        let hint =
            crate::suggest::format_hint(out_ext, &["stl", "obj", "gltf", "glb", "mtl", "usdz"]);
        return Err(Error::invalid(format!(
            "convert: 3D input '{}' must pair with a 3D output (.stl/.obj/.gltf/.glb/.mtl/.usdz) OR a raster output (.png/.jpg/.bmp/.webp); got '{}'{hint}",
            plan.input, plan.output
        )));
    }

    #[cfg(feature = "ico")]
    if crate::ico_runner::is_ico_output(&plan.output) {
        return Ok(Route::IcoOutput);
    }

    Ok(Route::Pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{ConvertPlan, PrintfTemplate};

    /// Build a minimal plan for routing tests. Input paths here never
    /// exist on disk, so classification is extension-only.
    fn plan(input: &str, output: &str) -> ConvertPlan {
        ConvertPlan {
            input: input.into(),
            input_pages: None,
            ops: vec![],
            output: output.into(),
            output_template: None,
            ping: false,
            probe: false,
            probe_json: false,
            probe_watch: false,
            mesh3d_options: Default::default(),
        }
    }

    fn route_of(input: &str, output: &str) -> Route {
        decide(&plan(input, output)).expect("routable")
    }

    fn err_of(input: &str, output: &str) -> String {
        let p = plan(input, output);
        format!("{}", decide(&p).expect_err("unroutable"))
    }

    // ---- mode switches ----

    #[test]
    fn ping_short_circuits_everything() {
        let mut p = plan("in.pdf", "out.png");
        p.ping = true;
        assert_eq!(decide(&p).unwrap(), Route::Ping);
    }

    #[test]
    fn ping_outranks_probe() {
        let mut p = plan("in.png", "");
        p.ping = true;
        p.probe = true;
        assert_eq!(decide(&p).unwrap(), Route::Ping);
    }

    #[test]
    fn probe_outranks_input_classification() {
        // A probe on a PDF must NOT take the PDF conversion
        // side-channel — probe owns its own PDF handling.
        let mut p = plan("in.pdf", "");
        p.probe = true;
        assert_eq!(decide(&p).unwrap(), Route::Probe);

        let mut p = plan("cube.stl", "");
        p.probe = true;
        assert_eq!(decide(&p).unwrap(), Route::Probe);
    }

    // ---- PDF input ----

    #[test]
    fn pdf_input_routes_to_side_channel_for_every_output_class() {
        // Scene, vector, raster, template — all PDF-runner business.
        for out in [
            "out.pdf",
            "out.svg",
            "out.png",
            "out.jpg",
            "out.bmp",
            "out.webp",
            "page-%03d.png",
        ] {
            assert_eq!(route_of("in.pdf", out), Route::PdfSideChannel, "out={out}");
        }
    }

    #[test]
    fn pdf_extension_is_case_insensitive() {
        assert_eq!(route_of("REPORT.PDF", "out.png"), Route::PdfSideChannel);
        assert_eq!(route_of("Report.Pdf", "out.png"), Route::PdfSideChannel);
    }

    #[test]
    fn pdf_input_outranks_ico_output() {
        // Input classification wins: the PDF side-channel gets first
        // refusal (and will reject .ico itself as an unsupported
        // output class) — the ICO writer never sees a PDF input.
        assert_eq!(route_of("in.pdf", "icon.ico"), Route::PdfSideChannel);
    }

    // ---- 3D-asset input (mesh3d feature) ----

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_to_mesh3d_covers_the_full_output_set() {
        for out in [
            "out.stl", "out.obj", "out.gltf", "out.glb", "out.mtl", "out.usdz",
        ] {
            assert_eq!(
                route_of("cube.stl", out),
                Route::Mesh3dToMesh3d,
                "out={out}"
            );
        }
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_to_raster_covers_the_raster_set() {
        for input in [
            "cube.stl",
            "model.obj",
            "scene.gltf",
            "scene.glb",
            "archive.usdz",
            "materials.mtl",
            "rig.fbx",
        ] {
            for out in ["out.png", "out.jpg", "out.jpeg", "out.bmp", "out.webp"] {
                assert_eq!(
                    route_of(input, out),
                    Route::Mesh3dToRaster,
                    "input={input} out={out}"
                );
            }
        }
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_input_extension_is_case_insensitive() {
        assert_eq!(route_of("CUBE.STL", "out.obj"), Route::Mesh3dToMesh3d);
        assert_eq!(route_of("Scene.GlTF", "out.png"), Route::Mesh3dToRaster);
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn fbx_is_input_only_no_same_class_reencode() {
        // FBX has a decoder but no encoder in the workspace; `.fbx`
        // output is NOT in the 3D-output set and classify_output
        // rejects it, so the pairing falls through to the typed error.
        let msg = err_of("rig.fbx", "out.fbx");
        assert!(msg.contains("must pair with"), "got: {msg}");
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_to_unroutable_output_errors_with_pairing_message() {
        for out in ["out.mp4", "out.pdf", "out.wav", "out.ico"] {
            let msg = err_of("cube.stl", out);
            assert!(
                msg.contains("must pair with a 3D output"),
                "out={out} got: {msg}"
            );
            assert!(msg.contains("cube.stl"), "out={out} got: {msg}");
            assert!(msg.contains(out), "out={out} got: {msg}");
        }
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_to_close_typo_output_carries_did_you_mean_hint() {
        // `.slt` is one transposition away from `.stl`.
        let msg = err_of("cube.stl", "out.slt");
        assert!(msg.contains("did you mean"), "got: {msg}");
        assert!(msg.contains("stl"), "got: {msg}");
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_to_svg_errors_rather_than_vector_rendering() {
        // `.svg` is a vector class the PDF path supports, but the 3D
        // renderer only produces raster canvases today.
        let msg = err_of("cube.stl", "out.svg");
        assert!(msg.contains("must pair with"), "got: {msg}");
    }

    #[cfg(feature = "mesh3d")]
    #[test]
    fn mesh3d_with_printf_template_errors() {
        let mut p = plan("cube.stl", "frame-%03d.png");
        p.output_template = Some(PrintfTemplate {
            prefix: "frame-".into(),
            width: 3,
            suffix: ".png".into(),
        });
        let msg = format!("{}", decide(&p).expect_err("template must be rejected"));
        assert!(msg.contains("single-document"), "got: {msg}");
        assert!(msg.contains("frame-%03d.png"), "got: {msg}");
    }

    // ---- .ico output (ico feature) ----

    #[cfg(feature = "ico")]
    #[test]
    fn raster_input_to_ico_routes_to_icon_writer() {
        assert_eq!(route_of("logo.png", "logo.ico"), Route::IcoOutput);
        assert_eq!(route_of("logo.bmp", "LOGO.ICO"), Route::IcoOutput);
    }

    #[cfg(feature = "ico")]
    #[test]
    fn cur_output_does_not_route_to_icon_writer() {
        // `.cur` needs a hotspot grammar first — documented in
        // ico_runner. It stays on the regular pipeline path.
        assert_eq!(route_of("logo.png", "pointer.cur"), Route::Pipeline);
    }

    // ---- regular pipeline path ----

    #[test]
    fn media_pairs_route_to_pipeline() {
        // image → image, image → video-ish, video → video,
        // audio → audio, generator → image: all regular pipeline.
        for (input, output) in [
            ("in.png", "out.jpg"),
            ("in.jpg", "out.webp"),
            ("in.bmp", "out.png"),
            ("movie.mp4", "movie.mkv"),
            ("movie.mkv", "movie.webm"),
            ("in.png", "out.gif"),
            ("sound.wav", "sound.flac"),
            ("sound.mp3", "sound.ogg"),
            ("sound.flac", "sound.wav"),
            ("xc:red", "red.png"),
            ("gradient:red-blue", "g.png"),
            ("subs.srt", "subs.vtt"),
        ] {
            assert_eq!(
                route_of(input, output),
                Route::Pipeline,
                "input={input} output={output}"
            );
        }
    }

    #[test]
    fn unknown_extensions_still_route_to_pipeline() {
        // Routing doesn't gatekeep unknown formats — the pipeline
        // (codec/container registries) owns that error so new sibling
        // crates extend the set without touching the router.
        assert_eq!(route_of("in.xyz", "out.abc"), Route::Pipeline);
        assert_eq!(route_of("noext", "out"), Route::Pipeline);
    }

    #[test]
    fn route_tags_are_stable() {
        assert_eq!(Route::Ping.as_tag(), "ping");
        assert_eq!(Route::Probe.as_tag(), "probe");
        assert_eq!(Route::PdfSideChannel.as_tag(), "pdf");
        assert_eq!(Route::Pipeline.as_tag(), "pipeline");
        #[cfg(feature = "mesh3d")]
        {
            assert_eq!(Route::Mesh3dToMesh3d.as_tag(), "mesh3d-to-mesh3d");
            assert_eq!(Route::Mesh3dToRaster.as_tag(), "mesh3d-to-raster");
        }
        #[cfg(feature = "ico")]
        assert_eq!(Route::IcoOutput.as_tag(), "ico");
    }
}
