# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `--probe` dry-run structural-inspection mode. Decodes the input far
  enough to extract metadata (page count, mesh count, sample rate, …),
  prints a compact summary to stdout, and skips any output write.
  Mutually exclusive with an output positional — `--probe in.gltf`
  is the supported shape; `--probe in.gltf out.obj` errors at the
  parser. Fields surfaced per input class:
  - **Raster image / video / audio**: container, codec, width × height,
    bit_depth, color_space, alpha presence, frame_rate, sample_rate_hz,
    channels, channel_layout, sample_format, duration_s, bit_rate
    (best-effort from `StreamInfo` / `CodecParameters`; absent fields
    omitted from the output).
  - **PDF**: file_size_bytes, page_count, per-page width_pt × height_pt
    × orientation_deg (capped at 32 entries; total is always reported
    via `page_count`), embedded_image_count (walked from the
    `VectorFrame` tree). embedded_font_count is reported as `unknown`
    pending an upstream font-resource census.
  - **SVG**: file_size_bytes, single-page width_pt × height_pt,
    embedded_image_count.
  - **3D (STL/OBJ/glTF/GLB/USDZ/MTL)** (mesh3d feature): mesh_count,
    primitive_count, vertex_count, triangle_count (uses
    `Primitive::triangle_count` for triangle topologies; non-triangle
    topologies report 0), material_count, texture_count,
    animation_count, skin_count, node_count, root_count, topologies
    (per-topology histogram), bounding_box (computed from positions
    when not embedded).
- `--json` flag — pair with `--probe` for a single-line machine-
  readable JSON object instead of the default pretty-printed
  `key: value` block. Without `--probe` the parser rejects `--json`
  with a clear "needs --probe" message rather than silently swallowing
  the flag.
- `op::ConvertPlan::probe` + `op::ConvertPlan::probe_json` fields
  carrying the parsed mode switches.
- New `crate::probe` module hosting the structural-inspection runner,
  the input-shape decision tree (PDF / 3D / SVG / container fallback),
  and a hand-rolled JSON serialiser kept in sync field-by-field with
  the pretty-printer so a `diff` between two probes stays meaningful.
- End-to-end coverage in `tests/probe.rs` (8 tests): PDF pretty + JSON,
  SVG pretty, STL pretty + JSON, mutual-exclusion error, `--json`-
  without-`--probe` error, no-input error.
- Per-format encoder option flags for the 3D side-channel
  (`-stl-format ascii|binary` and `-gltf-format glb|embedded|external`)
  with case-insensitive synonyms (`bin`/`text`, `binary`/`json-embedded`/
  `json-external`). When set, the encoder is constructed directly via
  the format crate (`oxideav_stl::encoder::StlEncoder::new(StlFormat::…)`,
  `oxideav_gltf::GltfEncoder::with_output(OutputFlavour::…)`), bypassing
  the parameter-less factory closures stored in `Mesh3DRegistry`. The
  default code path is unchanged — flag-less convert invocations still
  pick up the registry default for every format.
- `op::Mesh3DOptions` field on `ConvertPlan` carrying the parsed
  per-format choices; threaded through to `mesh3d_runner::run` which
  validates the flag↔output-extension pairing up-front (e.g.
  `-stl-format ascii` paired with a `.gltf` output emits a clear
  "set but output extension is '.gltf', not '.stl'" error rather
  than silently dropping).
- `oxideav-stl` / `oxideav-obj` / `oxideav-gltf` as direct optional
  deps (gated on `mesh3d`) so the convert verb can call the typed
  encoder constructors. `oxideav-meta` already pulls them via the
  `3d` feature so this adds no new transitive crates.
- End-to-end coverage in `tests/format_flags.rs` (11 tests):
  STL→STL ASCII / binary on-disk byte signatures, glTF JsonEmbedded
  via `.glb` extension and Glb via `.gltf` extension (override-the-
  extension paths), unknown-value parser rejection, mismatched-output
  rejection, `external` follow-up surfacing.

### Cross-crate follow-ups

- `oxideav-obj` rN — publish 0.0.1 with `ObjEncoder::with_negative_indices`
  + `obj::SerializeOptions::negative_indices` (already on master in this
  workspace); needed to wire `-obj-negative-indices` through convert.
  Per-decimal-precision option also needs to be added to
  `mtl::serialize_mtl` / `obj::SerializeOptions` for `-mtl-precision N`.
- `oxideav-gltf` rN — extend `OutputFlavour` with a `JsonExternal`
  variant that emits a `.gltf` JSON document referencing a sidecar
  `.bin` file (encoder needs to return both `Vec<u8>` blobs or a
  caller-side helper that splits them). Once published, swap the
  `external` arm in `mesh3d_runner::build_gltf_encoder` from
  `Err(unsupported)` to the real flavour.

- 3D-asset side-channel — `convert cube.stl cube.obj`,
  `convert model.obj model.gltf`, `convert scene.gltf scene.glb`,
  `convert archive.usdz extracted.gltf`, etc. work end-to-end.
  Inputs sniffed by extension (`.stl` / `.obj` / `.gltf` / `.glb` /
  `.usdz` / `.mtl`); decode → re-encode runs through a
  `oxideav_mesh3d::Mesh3DRegistry` populated by
  `oxideav_meta::populate_mesh3d_registry`. USDZ is read-only today
  (decoder registered, no encoder factory). Raster ops (`-resize`,
  `-blur`, …) are silently ignored for 3D-asset conversions — they
  have no pixel grid.
- `mesh3d_runner` module with `is_mesh3d_input` / `is_mesh3d_output`
  recognisers and a `run(input, output)` driver that picks decoder /
  encoder by file extension and surfaces friendly error messages
  (`"no 3D decoder registered for input extension '.xyz' (known:
  .stl, .obj, .gltf, .glb, .usdz, .mtl)"`).
- `mesh3d` cargo feature (default-on). Pulls `oxideav-mesh3d` plus
  `oxideav-meta = { version = "0.0", default-features = false,
  features = ["3d"] }` so the convert verb only drags the four
  format codecs (stl/obj/gltf/usdz) into its dep tree, not every
  audio/video/image sibling that meta's default `all` preset would
  bring. Slim builds (`--no-default-features --features generator`)
  drop the side-channel and 3D inputs fall through cleanly.
- 3D→non-3D output and 3D + `%d`-template inputs are rejected
  before decode with a clear, actionable error message.
- End-to-end integration tests in `tests/mesh3d_convert.rs` covering
  STL→OBJ, STL→glTF (JSON envelope check), STL→GLB (`glTF` magic
  check), OBJ→glTF round-trip, plus the two error paths above.

- Four more IM colour-grading flags wired to the matching
  `oxideav-image-filter` factory (registers as `vignette` / `colorize` /
  `equalize` / `auto-gamma` once the consumer pulls a published
  image-filter that includes them; the CLI emits the JSON dialect
  unconditionally):
  - `-vignette R[+S][+X[+Y]]` → `video.vignette { x, y, radius, sigma }`
    (`S` defaults to `R/2`; `X`/`Y` default to image centre — passed
    as normalised `[0.0, 1.0]` offsets to stay resolution-independent).
  - `-colorize C[xC[xC]]/A%` → `video.colorize { color: [R,G,B,A],
    amount }`. Colour part accepts CSS L3 named, `#hex` 3/4/6/8, and
    IM's per-channel `R[xG[xB]]` triplet (single-component value
    replicates); `/A%` accepts a percentage or unit-scalar amount.
  - `-equalize` (no value) → `video.equalize {}`.
  - `-auto-gamma` (no value) → `video.auto-gamma {}`.
- PDF→raster side-channel now honours the round-37 tonal /
  colour-grading ops (`-sharpen`, `-unsharp`, `-gamma`,
  `-brightness-contrast`, `-contrast`, `-sepia`, `-modulate`, `-level`,
  `-normalize`, `-threshold`, `-posterize`, `-solarize`,
  `-colorspace gray|grey`). `pixel_xform::apply_pixel_transform_chain`
  dispatches to the matching `oxideav-image-filter` constructor on the
  rendered RGBA buffer before alpha-grammar / encode, keeping PDF
  inputs pixel-identical to non-PDF inputs at the same op chain.
  Locked down by an integration test that decodes a `-sharpen 1x0.5`
  PDF render and asserts byte-for-byte match against the standalone
  `Sharpen` filter applied to the plain render.
- `RgbaImage::is_rgb` is now `pub` (was private to `pdf_runner`) so the
  `pixel_xform` tonal dispatch can pick the right `PixelFormat` tag.

- IM-style tonal / colour-grading flags wired through to the matching
  `oxideav-image-filter` factory via a `wrap(chain, "video.<name>",
  json!(…))` step on the regular pipeline path:
  - `-sharpen RxS` → `video.sharpen { radius, sigma }`.
  - `-unsharp RxS+amount+threshold` →
    `video.unsharp { radius, sigma, amount, threshold }`.
  - `-gamma G` → `video.gamma { value }` (`G > 0` enforced).
  - `-brightness-contrast B[,C]` (also `BxC`) →
    `video.brightness-contrast { brightness, contrast }` (each in
    `[-100..=100]`).
  - `-contrast` (no value) → `video.contrast { value: 5.0 }` per
    flag — repeated `-contrast` chains accumulate.
  - `-sepia N%` / `-sepia 0.5` → `video.sepia { threshold }`.
  - `-modulate B[,S[,H]]` → `video.modulate
    { brightness, saturation, hue_degrees }` with IM's
    `0..200`-around-`100` hue translated to `±180°`.
  - `-level B[/G[/W]]` (also `B,G,W`; black/white accept `N` or `N%`)
    → `video.level { black, gamma, white }` (gamma > 0; black ≤ white).
  - `-normalize` → `video.normalize {}`.
  - `-threshold N` / `-threshold N%` → `video.threshold { value }`.
  - `-posterize N` (`N >= 2`) → `video.posterize { levels }`.
  - `-solarize N` / `-solarize N%` → `video.solarize { value }`.
  - `-colorspace gray|grey` → `video.grayscale { preserve_alpha: true }`;
    `-colorspace rgb|srgb` is recorded as a no-op (input keeps its
    colourspace). Other colourspaces continue to error cleanly.
  - The previously inline-only round-1 ops (`-rotate`, `-flip`, `-flop`,
    `-crop`, `-negate`) now also wire through to the matching
    `video.rotate / .flip / .flop / .crop / .negate` factories on the
    regular pipeline path so non-PDF inputs honour them too. The PDF
    side-channel still applies them via `pixel_xform` for parity.
- Argument parsers for each new flag with friendly error messages
  (`out of range`, `must be > 0`, `levels must be >= 2`,
  `not yet wired`, …).
- Round-trip tests in `plan_to_job::tests`:
  - JSON-shape coverage for every newly-wired op (per-key value
    assertions on the emitted `FilterNode.params`).
  - End-to-end registry-build coverage that hands the CLI-emitted JSON
    back to a `RuntimeContext` carrying `oxideav_image_filter::register`
    — proves the JSON dialect matches the factory's parameter schema.
  - Pixel-exact match: synthesise a 4×4 RGBA edge fixture, run it
    through both `Sharpen` directly and the registry-built filter, and
    assert byte-for-byte equality. The registry-build assertions
    skip silently when the linked image-filter pre-dates the
    round-next factories (published `0.1.1` only registers
    `blur` / `edge` / `resize`), so the test suite stays green
    through the producer-publishes-consumer-lands cycle.
- Inline geometry / negate ops on the PDF→raster path:
  - `-rotate N` for `N ∈ {±90, ±180, ±270}` (other angles rejected
    cleanly with "only multiples of 90 supported (got N)"). 90/270
    swap output width and height.
  - `-flip` — vertical flip (rows reversed).
  - `-flop` — horizontal flip (columns reversed within each row).
  - `-crop WxH+X+Y` — extract a `WxH` bbox at offset `(X,Y)`. Bbox
    that overruns the input dims errors with "bbox WxH+X+Y exceeds
    input W'xH'". IM geometry modifiers (`%`, `!`, `^`, `<`, `>`, `@`)
    are rejected with a clear "round-1 supports plain WxH+X+Y only"
    message.
  - `-negate` — per-pixel `out = 255 - in` on the colour channels;
    alpha (when present) is unchanged.
- `pixel_xform` module exposing the underlying `flip`, `flop`,
  `negate`, `rotate`, `crop`, and `apply_pixel_transform_chain`
  primitives so callers can drive the same transforms directly on an
  `RgbaImage`.
- `RgbaImage` is now `pub` (was `pub(crate)`) so external callers can
  build inputs for the pixel-transform primitives.
- `-ping` flag — read only the headers of the input and print one
  IM-format line per "image" (page / video stream) to stdout, then
  exit without decoding pixels or writing any output. Output line
  shape: `<path> <FORMAT> <W>x<H> <W>x<H>+0+0 <DEPTH>-bit
  <COLORSPACE> <BYTES>B`. Multi-page PDFs emit one line per selected
  page with `[N]` suffix. Output positional becomes optional when
  `-ping` is on.
- `ping` module with a `PixelFormat` → (depth, colorspace) mapping
  table covering the common cases (8/10/12/16-bit YUV, RGB, Gray,
  CMYK, palette, mono).
- `ConvertPlan::ping: bool`.

## [0.0.4](https://github.com/OxideAV/oxideav-cli-convert/compare/v0.0.3...v0.0.4) - 2026-05-03

### Other

- bump oxideav-image-filter 0.0 -> 0.1 (sibling promoted to 0.1 series)
- loosen oxideav-image-filter pin 0.0.4 -> 0.0
- require oxideav-image-filter >= 0.0.4 for new VideoFrame API
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- add generator-shorthand translator hook
- pin release-plz to patch-only bumps

## [0.0.3](https://github.com/OxideAV/oxideav-cli-convert/compare/v0.0.2...v0.0.3) - 2026-04-25

### Other

- release v0.0.2

## [0.0.2](https://github.com/OxideAV/oxideav-cli-convert/releases/tag/v0.0.2) - 2026-04-25

### Other

- use char-array form in split_once predicates
- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- bump version to 0.0.2 for RuntimeContext API change
- take RuntimeContext + drop image_filter feature on pipeline dep
- Initial oxideav-cli-convert: IM-style convert engine on top of oxideav-pipeline
