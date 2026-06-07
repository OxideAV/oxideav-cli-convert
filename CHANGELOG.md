# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.5](https://github.com/OxideAV/oxideav-cli-convert/compare/v0.0.4...v0.0.5) - 2026-06-07

### Added

- *(convert)* -roll ±X±Y — IM-style circular pixel shift
- *(convert)* -quality validates 0..=100 at parse time
- *(convert)* 'did you mean?' hint on unknown -flag errors
- *(convert)* -trim + -fuzz N[%] — IM auto-crop with tolerance state
- *(convert)* -monochrome — IM 1-bit B/W shorthand (gray + 2 colors + Floyd-Steinberg)
- *(convert)* -extent WxH[±X±Y] — canvas re-window with active background
- *(convert)* comma-list + negative page-selector atoms
- *(convert)* .ico output + -define icon:auto-resize multi-resolution
- *(convert)* debug render modes (normal/depth) + N×N supersampling AA
- *(convert)* gouraud/phong shading + camera/light/projection/fov/bg + FBX wire
- *(convert)* wire USDZ encoder + 3D→raster software renderer
- *(convert)* -resize geometry suffixes + -thumbnail + -define
- *(convert)* --probe --watch + did-you-mean hints for unknown extensions
- *(convert)* --probe surfaces PDF /Info, mesh3d per-mesh detail, container metadata
- *(convert)* --probe dry-run structural inspection (+--json)
- *(convert)* -stl-format and -gltf-format encoder option flags
- *(convert)* 3D-asset side-channel — STL/OBJ/glTF/GLB/USDZ
- *(convert)* -vignette / -colorize / -equalize / -auto-gamma + PDF tonal-op side-channel
- *(convert)* IM tonal / colour-grading flags wired to image-filter
- *(convert)* -rotate / -flip / -flop / -crop / -negate inline ops
- *(convert)* -ping flag for IM-style header-only inspection

### Fixed

- *(convert)* revert pdf_runner BMP API change

### Other

- *(mesh3d)* route renderer through oxideav-render::make_renderer
- wildcard arm for non-exhaustive SourceOutput
- bump oxideav-webp pin 0.1 → 0.2
- *(convert)* pin PDF→raster DPI math + headline-command end-to-end
- stub encode_webp after oxideav-webp clean-room rebuild
- ico_out test: replace 5-tuple with named DirEntry struct for clippy
- absorb oxideav-bmp 0.1.3 BmpImage.palette + encode_bmp tuple return
- drop alias table — registry is the only source of truth
- registry-driven output codec lookup via ContainerRegistry
- rustfmt sweep on round-2 PDF / fan-out additions
- bump oxideav-webp dep to 0.1 (published)
- bump oxideav-pdf dep to 0.1 (published)
- refresh for PDF / page selectors / multi-format fan-out
- round 2: page selector + multi-format raster fan-out + SVG/PDF page extraction
- add PDF input + Scene-aware fan-out for ImageMagick-style PDF rendering

### Added

- `-roll ±X±Y` — IM-style circular pixel shift. `dx` shifts columns
  to the right by `dx` pixels (negative = left), `dy` shifts rows
  down by `dy` pixels (negative = up); pixels that fall off one
  edge wrap around to the opposite edge so the visible image
  translates as a rigid block. Width and height stay unchanged.
  Grammar mirrors `-extent`'s `±X±Y` tail (shared parser): both
  offsets must be present and signed (`-roll +5+10`, `-roll -3+2`,
  `-roll +5-10`); a single offset (`-roll +5`), an unsigned value
  (`-roll 5+10`), or an empty argument is rejected at parse time
  with a clear message. Shifts larger than the dimension are
  reduced mod width / height by the inline implementation so
  `-roll +width+height` is the identity. Wired through both paths:
  the generic pipeline (`video.roll` factory in
  `oxideav-image-filter`) and the inline PDF / mesh3d side-channel
  (new `pixel_xform::roll` working on `RgbaImage` with both RGB-24
  and RGBA stride layouts via two in-place `slice::rotate_right`
  passes — one over each row, one over the row block).

- `-quality` now validates the IM-conventional `0..=100` range at
  parse time. Out-of-band values (`-quality 1000`, `-quality 150`)
  previously fell through the encoder's "drop unknown key" path and
  looked to the user like the flag had been honoured; they now emit
  a clear `convert: -quality: N out of range (must be in 0..=100;
  common values: 75 default, 85 web, 95 archival)` error. Endpoints
  `0` (JPEG's smallest acceptable) and `100` (highest) stay
  accepted; the existing non-numeric / negative-integer path
  (`-quality high`, `-quality -5`) still trips the pre-cap
  `not a non-negative integer` branch with its original message.
  Pinned by three new args-parser tests covering the
  endpoints-accepted, over-range-rejected, and non-numeric-
  unchanged contracts.

- "Did you mean?" hint on unknown-flag errors. A bogus `-`-prefixed
  flag is matched against the full known-flag table (`-resize`,
  `-quality`, `-colorspace`, …, plus the double-dash session modes
  `--probe` / `--json` / `--watch`) via the existing
  `suggest::closest_match` Levenshtein helper. Close edits emit a
  `(did you mean '-quality'?)` clause appended to the bare
  `unknown flag 'X'` error; distant typos (`-fnord`) get the bare
  error unchanged. The lead the user typed is preserved on the
  suggestion — `--prbe` reattaches `--` to suggest `--probe`,
  `-quailty` reattaches `-` to suggest `-quality` — so the hint is
  copy-paste-correct. The known-flag table is a single sorted slice
  near the top of `args.rs` that must stay in sync with the parser's
  match arms; a flag missing from the table just downgrades hint
  quality on its own typos. Adds three unit tests pinning the
  transposition (`-quailty` → `-quality`, `-reszie` → `-resize`)
  and the dash-flavour-preservation (`--prbe` → `--probe`) cases,
  plus a negative-assert on the existing `-fnord` test confirming
  distant tokens still produce no hint.

- `-trim` (valueless) paired with `-fuzz N[%]` — IM's auto-crop op
  that collapses an image down to the bounding box of pixels
  differing from the corner-pixel reference background by more
  than the active `-fuzz` tolerance. The `-fuzz` flag updates a
  parser-state value rather than emitting an op of its own; a
  following `-trim` captures the value at parse time so source-
  order semantics survive into the plan walker (`-fuzz 5 -trim
  -fuzz 20 -trim` lands as two trims with two distinct
  tolerances, not one). Accepts both the raw byte grammar
  (`-fuzz 12`, `0..=255`) and IM's percent grammar (`-fuzz 10%`,
  `0..=100%` → `0..=255` rounded). Wired through three paths:
  the args parser (new `Op::Trim { fuzz }` variant + per-flag
  state plumbing), the generic pipeline path (`video.trim`
  factory in `oxideav-image-filter`), and the inline PDF /
  mesh3d side-channel (new shape-recovering helper in
  `pixel_xform` that inverts the packed-plane stride to read
  back the trimmed dimensions, since the bbox isn't known at
  apply-time). Uniform-background inputs collapse to a
  representable `1x1` frame per the factory contract.

- `-monochrome` — IM's valueless shorthand that emits a 1-bit
  black-and-white image with Floyd-Steinberg error diffusion. Lowers
  to the canonical primitive chain
  `-colorspace gray -colors 2 -dither floyd_steinberg` at args-parse
  time, preserving source-order semantics so any neighbours (e.g.
  a preceding `-resize` and a trailing `-strip`) sit on either side
  of the expansion rather than being pushed around it. The dither
  choice is hardcoded to `FloydSteinberg` regardless of any prior
  `-dither none` to match IM's "monochrome is self-contained"
  contract; users who want the un-dithered cut still write the chain
  by hand. Lives in the args parser only — every downstream runner
  (PDF side-channel, 3D-render side-channel, regular pipeline) sees
  the lowered ops it already knows how to handle, so no plan_to_job
  / pixel_xform / image-filter changes were needed.

- `-extent WxH[±X±Y]` — IM's canvas re-window op. Places the source at
  signed offset `(x, y)` (default `(0, 0)`) on a fresh `WxH` canvas,
  painting pixels outside the source rectangle with the active
  `-background` colour (defaulting to opaque white when none was set).
  Negative offsets translate the source toward the upper-left so its
  right / bottom edge can fit inside the window; sources fully outside
  the canvas yield an all-background image (no error, matching IM).
  Source-order semantics are preserved: each `-extent` captures the
  `-background` that preceded it, so a later `-background` does NOT
  retroactively repaint earlier extents. Wired through three paths:
  the args parser (`parse_extent` with full `±X±Y` grammar), the
  generic pipeline path (`video.extent` factory in
  `oxideav-image-filter`), and the inline PDF / mesh3d side-channel
  (new `pixel_xform::extent` working on `RgbaImage` with both RGB-24
  and RGBA stride layouts).

- Direct unit tests for the PDF-page → RGBA rasteriser
  (`pdf_runner::render_page_to_rgba`) pinning the
  point-to-pixel scaling at multiple DPIs: 72-DPI US Letter
  (612 pt → 612 px), 300-DPI US Letter (612 pt → 2550 px,
  792 pt → 3300 px), 150-DPI A4 (595 pt → 1240 px, 842 pt
  → 1754 px), last-density-wins on a chained `-density`
  sequence, and an opaque-colour `-background` paints every
  pixel of an empty page. Round-trips the spec's claim that
  PDF page sizes are in PostScript points (1/72 inch).
- End-to-end pdf_to_png test `headline_command_letter_at_300_dpi_emits_2550x3300_rgb_pngs`
  exercising the literal round-90 invocation
  `-density 300 input.pdf -background white -alpha remove -alpha off page-%03d.png`
  on a US-Letter-sized fixture. Asserts both output PNGs are
  exactly 2550×3300 and that the PNG IHDR colour-type field
  is `2` (Truecolor RGB), confirming `-alpha off` actually
  drops the alpha channel from the encode rather than
  emitting an RGBA buffer with `A=255` everywhere.

- Comma-separated and negative page-selector atoms on PDF inputs.
  `convert input.pdf[-1] cover.png` now writes the last page of the
  document; `convert input.pdf[0,2,4] page-%d.png` fans out the
  user-listed pages in source order; `convert input.pdf[0-2,5,-1]
  ...` mixes ranges, singles, and negative offsets in one selector;
  `convert input.pdf[5--1] ...` runs page 5 through the last page.
  Atomic specs (`PageAtom::Single(isize)` / `::Range(isize, isize)`)
  carry the signed indices; `PageSelector::resolve(total_pages)`
  maps `-k` to `total_pages - k` and propagates a precise "negative
  index out of range" error otherwise.
- `op::PageAtom` enum + a new `PageSelector::List(Vec<PageAtom>)`
  variant. Single-atom selectors continue to land as
  `PageSelector::Single` / `::Range` so existing pattern-match
  callers don't have to walk a one-element list.
- Source order is preserved on resolve (so `[2,0]` writes page 2
  before page 0); duplicates are retained matching IM
  (`[0,0,0]` writes the same page three times).
- 13 new unit tests in `args::tests` (negative-index parse, negative
  range endpoint(s), comma-list parse, single-atom-list collapse to
  `Single`, empty-atom / bare-dash rejection, resolver coverage for
  negatives / lists / duplicates / out-of-range propagation) plus 4
  new end-to-end tests in `tests/pdf_to_png.rs` (`pdf[-1]` → PNG,
  `pdf[0,1]` → PNG fan-out, `pdf[0,-1]` → PNG fan-out, `pdf[-5]`
  out-of-range error).

- `.ico` (Windows icon) output target. `oxideav convert src.png out.ico`
  writes a 1-entry ICO at the source dimensions; pair with
  `-define icon:auto-resize=W1,W2,…` (IM convention) for a
  multi-resolution icon — each comma-separated `W` becomes an `W × W`
  sub-image bilinear-downscaled from the source through
  `oxideav-image-filter`'s `Resize` factory. The writer follows the
  `oxideav-ico` `WriteOptions::default()` policy (PNG for sub-images
  with `min(w,h) >= 64`, BMP otherwise — what Windows 10+ tooling
  produces). ICO format limits per-axis dimensions to `1..=256`;
  out-of-range sizes (`0`, `257`, …) are rejected up front.
- `crate::ico_runner` module hosts the new side-channel runner.
  Decode covers PNG (`oxideav_png::decode_png_to_rgba`) and BMP
  (`oxideav_bmp::decode_bmp`); other inputs surface a clean
  "not yet supported on the ICO writer path — convert to PNG first"
  error rather than silently bailing.
- `ico` cargo feature (default-on, sibling to `mesh3d`). Pulls
  `oxideav-ico` as an optional dep so slim builds (`--no-default-features
  --features generator`) drop the side-channel and `.ico` falls through
  to the regular pipeline path (which then errors out as an unknown
  sink extension — exactly what it should).

- 3D → raster renderer learns two diagnostic visualisation modes
  plus N×N supersampling anti-aliasing.
  - `-render normal-debug` (aliases `normal` / `normals`) paints each
    pixel `(n + 1) / 2 * 255` per channel — the classic "normal map"
    colour-key (positive +X is red-ish, +Y green, +Z blue). Lighting
    and material are ignored; useful for verifying the geometry
    pipeline / normal-loading path.
  - `-render depth-debug` (aliases `depth` / `z`) paints each pixel
    a grayscale value derived from the interpolated NDC z (near =
    white, far = black). Lighting / material / camera-orbit ignored;
    useful for spotting Z-fighting and picking sane near/far planes.
  - `-aa N` for `N ∈ 1..=8` rasterises the scene at `N × output_w`
    by `N × output_h` then box-filters back down. `N = 1` is the
    round-44 baseline (off); `2` / `4` typical; `8` capped so a
    1024² render stays under ~80 MB framebuffer + z-buffer. Stored
    on `Mesh3DOptions::aa`; works with every `-render` mode (the
    debug modes also benefit from cleaner triangle edges).
- `op::Mesh3DRenderMode::NormalDebug` / `::DepthDebug` variants;
  `op::Mesh3DOptions` gains an `aa: Option<u32>` field.
- 11 new arg-parser tests (`-render normal-debug` / `depth-debug` +
  short aliases; updated unknown-mode error mentions both new modes;
  `-aa` factor parses, `1` allowed, `0` / `>=9` rejected, non-integer
  rejected), 9 new renderer unit tests (`normal_to_byte` / `depth_to_byte`
  endpoint mappings; normal-debug + depth-debug rasterisers paint
  expected pixel patterns; AA default / 1× / 2× output dimensions and
  edge softening; `downsample_box` averaging on uniform + split fields),
  and 4 new end-to-end `mesh3d_convert` tests (`-render normal-debug`
  PNG, `-render depth-debug` PNG, `-aa 4` keeps output dims at 64×64,
  `-aa 1` no-op succeeds).

- 3D → raster renderer learns Gouraud and Phong shading.
  - `-render gouraud` evaluates a Lambert+ambient term at every vertex
    (using `Primitive::normals` when present, else the face normal
    computed from the three triangle positions in world space) and
    bilinearly interpolates the resulting RGB across the triangle.
  - `-render phong` interpolates the per-vertex normal across the
    triangle and evaluates the lighting equation at every pixel —
    smoother surface contour at the cost of one normalise + dot per
    fragment.
  - `-render flat` and `-render wireframe` continue to behave as
    they did in round 43; the default stays Flat.
- Camera and lighting control on the 3D → raster path:
  - `-light AZIMUTH,ELEVATION,INTENSITY` — directional light
    override. Default is `45°,45°,1.0` from the upper-right-front
    quadrant; only Gouraud / Phong consult the light (Flat and
    Wireframe ignore it).
  - `-camera ELEVATION,AZIMUTH,DISTANCE` — orbit camera override
    (`distance` is a multiplier of the auto-framed default). Without
    `-camera` the renderer keeps the bbox-fit auto-frame.
  - `-projection perspective|orthographic` — projection mode. Default
    is perspective; `ortho` / `o` aliases supported.
  - `-fov DEGREES` — vertical FOV for perspective projection. Default
    60°, valid range `(0, 180)`. Ignored for orthographic.
  - `-bg COLOR` — render-canvas background fill (CSS L3 named or
    `#hex`). Defaults to transparent black. Kept distinct from
    `-background` (which keeps its IM canvas-fill semantics for
    `-alpha remove` and the PDF runner).
- FBX (Filmbox) wired as a 3D input. `oxideav convert in.fbx out.gltf`
  now decodes through `oxideav-fbx` 0.0.1 and re-encodes through the
  existing glTF / OBJ / GLB / USDZ encoders. FBX is decode-only —
  no encoder ships yet, so `.fbx` doesn't appear in the
  `MESH3D_OUTPUT_EXTS` allow-list.
- `crate::mesh3d_runner::populate_registry(&mut Mesh3DRegistry)` —
  workspace-aware registry builder that wraps
  `oxideav_meta::populate_mesh3d_registry` and tacks on
  `oxideav_fbx::register` (meta 0.0.1 predates the FBX publish so its
  generated populator doesn't know about it). Once a meta release
  knows about FBX, this layer collapses back to a one-liner.
- `op::Mesh3DRenderMode::Gouraud` / `::Phong` variants;
  `op::ProjectionMode`; `op::LightSpec`; `op::CameraSpec`.
- `op::Mesh3DOptions` gains `light`, `camera`, `projection`,
  `fov_deg`, `bg` fields (all `Option<…>`, defaulting to renderer
  baseline behaviour when unset).
- 18 new arg-parser tests (`-render gouraud|phong|wire`; `-light` /
  `-camera` three-component grammar + invalid-input rejection;
  `-projection orthographic` + aliases; `-fov` in-range + out-of-range
  rejection; `-bg` named / hex / transparent), 4 new renderer unit
  tests (gouraud / phong pixel-drawn, render-bg priority, camera +
  ortho-projection matrix construction), and 4 new end-to-end
  `mesh3d_convert` tests (`-render gouraud` PNG, `-render phong` PNG,
  the full `-projection`/`-camera`/`-fov`/`-light`/`-bg` stack, FBX
  extension recognition + decoder hand-off).

- USDZ wired as a 3D-output target. `oxideav convert in.obj out.usdz`
  now produces a STORED-only PKZip carrying a `scene.usda` Default
  Layer — the matching encoder ships in `oxideav-usdz` 0.0.1 (the
  workspace populator already pointed `Mesh3DRegistry` at it; only
  the runner's allow-list `MESH3D_OUTPUT_EXTS` was gating it out).
- 3D → raster software renderer. `oxideav convert in.gltf out.png`
  decodes the 3D asset through the same `Mesh3DRegistry` the
  STL/OBJ/glTF/USDZ round-trip flow uses, then rasterises the
  `Scene3D` through a tiny built-in software pipeline:
  - **Camera** auto-frames the scene's axis-aligned bounding box at
    a 60° vertical FOV with a 20% margin, looking down `-Z`.
  - **Walk** every `Node`, composing the world matrix top-down; each
    `Mesh::primitive` is projected, back-face-culled, and rasterised
    with a half-space edge-function pipeline + per-pixel z-buffer.
  - **Triangle / strip / fan / line / line-strip / line-loop / point
    topologies** all dispatch through one tessellation step into the
    same triangle-list inner loop.
  - **Flat shading** — one constant colour per `Primitive`, pulled
    from `material.base_color` (linear → sRGB) or fallback grey when
    no material is bound.
  - `-render flat` / `-render wireframe` selects shaded vs.
    edge-only output. Wireframe draws Bresenham lines on each
    triangle's three edges.
  - `-resize WxH` seeds the canvas dimensions (otherwise 1024×1024);
    `-background COLOR` paints the canvas before rasterisation;
    `-alpha …` and the inline pixel-transform chain
    (`-rotate` / `-flip` / `-flop` / `-crop` / `-negate`) all run
    post-rasterisation, sharing the same plumbing as the PDF
    side-channel.
  - Output codec follows the extension: `.png` / `.jpg` / `.bmp` /
    `.webp` (the same set the PDF runner emits); `-quality N`
    forwards into the JPEG encoder.
- New `mesh3d_render` module hosts the renderer; new `raster_io`
  module centralises the `RgbaImage` handoff buffer + per-format
  encoders so the PDF runner and the 3D-render runner share one
  pixel-encode codepath.
- `Op::Mesh3DRenderMode` enum + `-render flat|wireframe|wire` arg
  (lives on `Mesh3DOptions::render_mode`, parallel to
  `-stl-format` / `-gltf-format`; silently ignored when the
  input/output pair doesn't trigger the 3D→raster path).
- IM-style geometry-modifier suffixes on `-resize` (and on the new
  `-thumbnail`):
  - `WxH!` — force exact dimensions, ignore aspect ratio (was
    `bang = true`; now `ResizeMode::Force`).
  - `WxH^` — fill the box: scale both axes by the LARGER of
    `(req_w/src_w, req_h/src_h)` so neither output dim is below the
    request. Pairs naturally with a follow-up `-crop` to land on
    exactly `WxH`.
  - `WxH>` — only resize when the input is LARGER than `WxH`;
    otherwise pass through unchanged.
  - `WxH<` — only resize when the input is SMALLER than `WxH`;
    otherwise pass through unchanged.
  - `WxH%` — interpret `W` and `H` as integer percentages of the
    source dimensions (`50%` halves both axes; `200x100%` doubles
    width and leaves height alone). The bare `N%` form replicates,
    matching IM.
  - `WxH@` — interpret `W*H` as the TARGET pixel area; both output
    dims scale by `sqrt(target_area / source_area)` so the aspect
    ratio is preserved AND the output area matches the request.
- `Op::Resize { width, height, mode: ResizeMode }` (was `bang: bool`).
  The PDF side-channel resolves every mode against the actual source
  dims; the regular pipeline path forwards a stable lowercase tag
  (`default` / `force` / `fill` / `shrink` / `grow` / `percent` /
  `area`) on the resize filter's JSON params so a future executor
  pass can resolve the source-aware variants too.
- `-thumbnail WxH[!^<>%@]` — IM convenience flag. Same geometry
  grammar as `-resize`. Unrolls into a `Resize` plus `Strip` pair
  (auto-orient is a documented follow-up — needs an EXIF reader on
  the source side).
- `-define KEY[=VALUE]` — opaque codec-specific tunable forwarded
  literally onto the sink track's `params` bag. Keys keep their `:`
  namespace separator (e.g. `jpeg:dct-method=float` lands as
  `params["jpeg:dct-method"] = "float"`); bare `-define KEY` (no
  `=VALUE`) becomes `params[KEY] = true`. Multiple `-define` flags
  all stack onto the same params object. Sink encoders that don't
  recognise a key silently ignore it, mirroring IM's
  tolerant-of-irrelevant-options posture.
- `pixel_xform` learns to honour `Op::Resize` / `Op::Thumbnail`
  end-to-end (it already knew the source dims, so the `Fill` /
  `Shrink` / `Grow` / `Percent` / `Area` modes work today on the
  PDF side-channel — no waiting on the regular-pipeline executor
  pass).
- New `run_image_filter_resize` helper in `pixel_xform` that lets
  shape-changing filters (Resize) declare the output dimensions
  directly, fixing a latent bug where `run_image_filter` always
  inherited the input width/height.
- `op::ResizeMode` enum + helpers (`split_suffix`, `as_tag`,
  `resolve(req_w, req_h, src_w, src_h)`) re-exported from the
  crate root.
- 16 new unit tests covering the geometry parsing (`-resize 200x100^`
  / `100x100>` / `1024x1024<` / `50x200%` / `75%` / `640x480@` /
  `-thumbnail 128x128^` / etc.), the `-define` grammar
  (`KEY=VALUE` / bare `KEY` / empty-`KEY` rejection), and the
  pixel-transform chain dispatch (`apply_chain_resize_*`).
- 7 new end-to-end tests in `tests/pdf_to_png.rs` proving the
  geometry modes, `-thumbnail`, and `-define` all work on the PDF
  → PNG / JPG path with byte-level checks on the output dimensions.

- `--probe` extensions surfacing more upstream metadata that the
  round-40 baseline left on the table:
  - **PDF**: `is_encrypted` (always present), and the `/Info` dictionary
    fields lifted onto `Scene::metadata` by `oxideav-pdf` —
    `title`, `author`, `subject`, `keywords`, `creator`, `producer`,
    `creation_date`, `modification_date`. Each is reported only when
    populated (the absence of `producer` is a real signal — "this PDF
    wasn't tagged" — distinct from the empty string). `is_encrypted`
    is read via `DocumentReader::open()`+`is_encrypted()`; if `open()`
    itself fails (encrypted PDF with non-empty password we don't have)
    the field reports `yes` rather than failing the whole probe.
  - **3D (mesh3d feature)**: per-mesh, per-material, per-animation
    detail (capped at 64 entries each — totals always reported via
    `mesh_count` / `material_count` / `animation_count`).
    - `meshes[i]`: `index`, `name` (or `(unnamed)`), `primitive_count`,
      `vertex_count`, `triangle_count`, `bounding_box` (per-mesh AABB
      computed from `Primitive::positions`).
    - `materials[i]`: `index`, `name`.
    - `animations[i]`: `index`, `name`, `channel_count`, `duration_s`
      (max keyframe time across every channel's sampler).
  - **Container fallback**: container-level `metadata` group
    (`title`, `artist`, `album`, etc. via `Demuxer::metadata()`).
    Demuxers that carry no metadata produce an empty group so the
    field stays predictable.
- `probe::render(plan, ctx) -> Result<String>` — same data
  `probe::run` would print, returned as a `String` for in-process
  callers (tests, embedders) that don't want to fight stdout capture.
- `tests/probe.rs` coverage for the new fields (+2 tests, total 10):
  populated `/Info` dict round-trips through the JSON formatter; STL
  per-mesh detail surfaces `vertex_count` / `triangle_count` /
  `bounding_box` and the empty `materials` / `animations` arrays are
  emitted (not omitted).

- `--watch` flag (paired with `--probe`) — re-runs the probe whenever
  the input file's mtime changes, polling once per second via
  `std::fs::metadata`+`modified()`. Each fresh report is emitted in
  the same format the one-shot mode would have used; in `--json` form
  each report is its own line so the output is JSON-lines-compatible.
  The loop runs forever and exits only on Ctrl+C / SIGINT. Soft-
  failing re-probes (file truncated mid-write, transient I/O failure)
  print a diagnostic to stderr and the loop continues — losing one
  frame of output is strictly better than tearing down the watch
  session. `--watch` without `--probe` is a parser-level error
  ("--watch requires --probe").
- `op::ConvertPlan::probe_watch` field carrying the parsed mode switch.
- `probe::run` routes through `run_watch` when `probe_watch` is set;
  the one-shot path is factored into `print_one`.

- "Did you mean?" hint helper — `crate::suggest` provides
  `closest_match` and `format_hint` over Levenshtein edit distance,
  cutoff `max(2, len/3)`. Wired into `mesh3d_runner` (unknown input /
  output extension) and `lib.rs` (3D-input → non-3D-output mismatch)
  so e.g. `.gtlf` typo'd from `.gltf` produces
  `(did you mean '.gltf'?)` rather than just listing the supported
  set. Among ties the candidate with the closest length wins, so
  4-letter typos prefer 4-letter candidates.
- `+5 unit tests` in `crate::suggest` (Levenshtein basics, case
  insensitivity, distance threshold, length-tiebreak); `+3
  integration tests` in `mesh3d_runner` (input typo / output typo /
  unrelated extension); `+3 tests` in `tests/probe.rs` (`--watch`
  parses; `--watch` without `--probe` errors; `--watch --json`
  parses).

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
