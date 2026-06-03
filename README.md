# oxideav-cli-convert

The engine behind `oxideav convert`: an ImageMagick-style CLI that
works on images, video, audio, **PDFs, vector content, and synthetic
sources**, implemented on top of `oxideav-pipeline`.

A one-frame PNG → JPG, a 90-minute MP4 → MKV with `-resize`, and a
multi-page PDF → page-numbered PNG sequence go through related code
paths — convert's job is to translate IM-style args into work the
rest of the workspace already knows how to do.

## Supported ops

| Op | Notes |
|---|---|
| `-resize WxH[!]` | bilinear; `!` skips aspect-ratio preservation |
| `-blur RxS` | Gaussian; `S` = sigma (defaults to `R/2`) |
| `-edge R` | Sobel magnitude |
| `-colors N` + `-dither {none\|bayer\|floyd_steinberg}` | palette quantisation via `pixfmt → Pal8` |
| `-format FMT` | bypass extension-based codec/container detection |
| `-quality N` | `0..=100`; forwarded to the sink codec when supported (JPG, WebP, …); out-of-range values reject at parse time |
| `-strip` | drop metadata on write |
| `-density N` | DPI for vector→raster (default 72; PDF / SVG inputs only) |
| `-background COLOR` | canvas + alpha-flatten background (CSS L3 named + `#hex` 3/4/6/8) |
| `-alpha {on\|off\|activate\|deactivate\|remove\|set\|opaque\|transparent}` | full IM grammar |
| `-rotate N` | quarter-turn only (`N ∈ {±90, ±180, ±270}`); 90/270 swap dims |
| `-flip` | vertical flip (rows reversed) |
| `-flop` | horizontal flip (columns reversed) |
| `-crop WxH+X+Y` | extract bbox; bbox-out-of-range errors cleanly |
| `-extent WxH[±X±Y]` | re-window onto a fixed canvas; padding painted with the active `-background` (defaults to opaque white); negative offsets translate the source toward the upper-left |
| `-negate` | per-pixel `255 - in` on RGB channels (alpha unchanged) |
| `-sharpen RxS` | unsharp-mask sharpening (`S` defaults to `R/2`) |
| `-unsharp RxS+amount+threshold` | full unsharp-mask grammar (`amount`/`threshold` optional) |
| `-gamma G` | power-law gamma (`G > 0`) |
| `-brightness-contrast B[,C]` | both in `[-100..=100]`; `BxC` separator also accepted |
| `-contrast` | bare flag; bumps contrast by 5%; repeats accumulate |
| `-sepia THRESHOLD` | accepts `N%` or `0.0..=1.0` |
| `-modulate B[,S[,H]]` | percent-of-base around 100; hue 0..200 → ±180° |
| `-level B[/G[/W]]` | endpoints accept `N` or `N%`; gamma > 0; black ≤ white |
| `-normalize` | stretch histogram to 0..=255 |
| `-threshold N[%]` | binarise around `N` |
| `-posterize N` | collapse to `N >= 2` levels per channel |
| `-solarize N[%]` | invert above threshold |
| `-colorspace gray\|grey\|rgb\|srgb` | `gray`/`grey` → grayscale factory; `rgb`/`srgb` no-op |
| `-monochrome` | valueless IM shorthand for `-colorspace gray -colors 2 -dither floyd_steinberg` (1-bit B/W with error diffusion); always emits `floyd_steinberg` regardless of any prior `-dither none` |
| `-fuzz N[%]` | tolerance state for the next `-trim`; bytes (`0..=255`) or percent (`0..=100%` → `0..=255` rounded); no op emitted in isolation |
| `-trim` | auto-crop to the bounding box of pixels differing from the corner-pixel reference background by more than the active `-fuzz` tolerance; uniform-background inputs collapse to a `1x1` representable frame |

The geometry / negate / tonal ops above all wire through to the
matching `oxideav-image-filter` factory on the regular pipeline path
(`video.rotate`, `video.sharpen`, `video.gamma`, …). The PDF
side-channel keeps its inline `pixel_xform` implementation for the
five geometry / negate ops; the new tonal ops are pipeline-only on
the PDF path today (a follow-up will route them through the same
inline pre-encode hook).

Anything else reports `unsupported: convert: -<op> is not yet
implemented` and exits cleanly — no silent misbehaviour.

## Arg ordering

Ops can appear **anywhere** — before AND after the input. The first
non-flag positional is the input; the last is the output. Multiple
inputs (`convert in1.pdf in2.pdf out.gif`) is a documented round-3
follow-up; today it errors with a clear message.

## Inputs

| Form | Resolved by | Examples |
|---|---|---|
| Local path | filesystem | `in.png`, `/abs/path/movie.mp4` |
| `file://` | source registry | `file:///abs/path/x.wav` |
| `http(s)://` | `oxideav-http` | `https://example.com/v.mp4` |
| Generator shorthand | `oxideav-generator` | `xc:red`, `gradient:red-blue`, `pattern:checkerboard`, `plasma:`, `mandelbrot:`, `synth:5,sine,440`, `testsrc:`, `smptebars:`, `noise:perlin`, `label:Hello world` |
| PDF (side-channel) | `oxideav-pdf` | `input.pdf`, `report.pdf[0]`, `report.pdf[2-5]` |
| 3D asset (side-channel) | `oxideav-mesh3d` registry (`mesh3d` feature) | `cube.stl`, `model.obj`, `scene.gltf`, `scene.glb`, `archive.usdz`, `materials.mtl` |

### `[N]` / `[N-M]` / `[A,B,…]` page selectors

ImageMagick-style suffix on PDF inputs:

| Selector | Meaning |
|---|---|
| `input.pdf[0]` | page 0 only |
| `input.pdf[2-5]` | pages 2..=5 inclusive |
| `input.pdf[-1]` | last page |
| `input.pdf[-3]` | third-from-last page |
| `input.pdf[5--1]` | page 5 through the last page |
| `input.pdf[-3--1]` | last three pages |
| `input.pdf[0,2,4]` | comma-separated list (atoms may themselves be ranges) |
| `input.pdf[0-2,5,-1]` | mix of ranges, singles, and negative indices |
| _(no suffix)_ | every page |

Negative atoms `-k` resolve to `total_pages - k` (`-1` = last page).
Comma lists preserve source order, so `[2,0]` writes page 2 first then
page 0; duplicates are retained (IM convention — `[0,0,0]` writes the
same page three times).

Out-of-range indices error with a precise count: `page index 5 out
of range (input has 3 page(s))` (or, for a negative atom that
overshoots the document, `negative page index -5 out of range
(input has 3 page(s); valid: -1..=-3)`).

## Outputs

The output extension chooses the encoder. Three classes drive the
routing:

| Class | Extensions | Behaviour |
|---|---|---|
| **Scene** | `.pdf` | writer consumes the whole `Scene` (vector preserved, multi-page) |
| **Vector** | `.svg` | one `VectorFrame` per file (no rasterisation) |
| **Raster** | `.png` `.jpg` `.bmp` `.webp`, plus any other extension a registered container claims (`qoi`, `dds`, …) | render each page to RGBA, encode |

Codec lookup goes through the caller's `RuntimeContext` —
`ContainerRegistry::container_for_extension` is consulted first, so any
sibling crate that registers itself (e.g. `oxideav_qoi::register`,
`oxideav_dds::register`) extends the supported output set
automatically. A small fallback table inside `plan_to_job` covers the
aliasing cases where the canonical encoder name differs from the
container name (`jpg` → `mjpeg`, `wav` → `pcm_s16le`, `ogg` →
`vorbis`, `avif` → `av1`, `ico`/`cur` → `png`).

### Printf templates

When the output filename contains a single `%[0-9]*d` token (e.g.
`page-%03d.png`), convert fans out to one file per selected page,
substituting the index. Multiple `%d`s, `%s`, `%x`, etc. are
rejected with a precise error.

### Routing matrix (3D-asset input)

The `mesh3d` cargo feature (default-on) wires `oxideav-mesh3d` plus
the format codecs (stl/obj/gltf/usdz) through `oxideav-meta`'s
`populate_mesh3d_registry` helper. With the feature off the side-channel
disappears and 3D inputs fall through to the regular pipeline path.

| Input | Output | Action |
|---|---|---|
| 3D format | matching 3D format (`.stl`/`.obj`/`.gltf`/`.glb`/`.mtl`/`.usdz`) | decode → re-encode through `Mesh3DRegistry` |
| 3D format | raster (`.png`/`.jpg`/`.bmp`/`.webp`) | software-render the scene to RGBA, encode |
| 3D format | other extension (`.svg`, `.mp4`, …) | error with did-you-mean hint |
| 3D format | output with `%d` template | error — 3D scenes are single-document |

USDZ now round-trips both ways (encoder ships in `oxideav-usdz` 0.0.1).
3D→raster rendering uses a tiny built-in software rasteriser with
flat / wireframe / Gouraud / Phong shading, two debug visualisations
(`normal-debug`, `depth-debug`), an N×N supersampling anti-aliasing
pass (`-aa N`), and full camera/light/projection/FOV/background
controls. Texture sampling and PBR are documented follow-ups.

Raster ops (`-rotate`, `-flip`, `-crop`, `-negate`, …) ARE honoured
on the 3D→raster path — they run after rasterisation, sharing the
same pixel-transform chain as the PDF runner. `-resize WxH` seeds
the canvas dimensions before rasterisation (default 1024×1024);
`-background COLOR` paints the canvas (default opaque white).

#### Per-format encoder option flags

These flags override the encoder choice the registry would make from the
output extension alone. They're accepted on every input shape but only
honoured when the 3D side-channel fires; the runner errors cleanly if a
flag is paired with an extension it doesn't apply to (e.g.
`-stl-format ascii out.obj`).

| Flag | Values | Default | Notes |
|---|---|---|---|
| `-stl-format` | `binary` / `bin` / `ascii` / `text` | `binary` | Selects STL on-disk flavour |
| `-gltf-format` | `glb` / `binary` / `embedded` / `json-embedded` / `external` / `json-external` | infer from `.glb` vs `.gltf` extension | `external` errors with a `gltf-rN` follow-up message until upstream `OutputFlavour::JsonExternal` lands |
| `-render` | `flat` / `shaded` / `wireframe` / `wire` / `gouraud` / `phong` / `normal-debug` (`normal` / `normals`) / `depth-debug` (`depth` / `z`) | `flat` | Selects 3D→raster surface model. `normal-debug` paints `(n+1)/2*255` per channel, `depth-debug` paints near=white, far=black grayscale; both ignore lighting / material settings. |
| `-light` | `AZIMUTH,ELEVATION,INTENSITY` (each a number) | `45,45,1.0` | Directional-light override for Gouraud / Phong (Flat / Wireframe / debug ignore it). |
| `-camera` | `ELEVATION,AZIMUTH,DISTANCE` (each a number; distance > 0) | bbox-fit auto-frame | Orbit camera override; `distance` is a multiplier of the auto-frame radius. |
| `-projection` | `perspective` / `persp` / `p` / `orthographic` / `ortho` / `o` | `perspective` | Picks projection matrix; ortho frames the scene on the smaller axis with a 1.2× margin. |
| `-fov` | degrees in `(0, 180)` | `60` | Vertical FOV for perspective; ignored for orthographic. |
| `-bg` | CSS L3 named or `#hex` colour (3 / 4 / 6 / 8 hex digits) | transparent black | Render-canvas background fill. Distinct from `-background` so callers can keep IM canvas-fill and renderer-clear separate. |
| `-aa` | integer `1..=8` | `1` (off) | Supersampling factor for the 3D→raster renderer. The scene is rasterised at `N × output` and box-filtered back down. `1` keeps the round-44 baseline; `2`/`4` are typical. Capped at `8` so a 1024² render at max-aa stays under ~80 MB framebuffer + z-buffer. |

### Routing matrix (PDF input)

| Input | Output | Template? | Action |
|---|---|:---:|---|
| PDF (any pages) | `.pdf` | no | pass selected pages through (vector preserved) |
| PDF (any pages) | `.pdf` | yes | error — `%d` + Scene-aware output is bogus |
| PDF (1 page) | `.svg` | no | write that page's VectorFrame |
| PDF (N pages) | `.svg` | no | error — suggest `%d` template OR `[N]` selector |
| PDF (any pages) | `.svg` | yes | one SVG per page |
| PDF (1 page) | `.png` / `.jpg` / `.bmp` / `.webp` | no | render + encode |
| PDF (N pages) | raster | no | error — suggest `%d` or `[N]` |
| PDF (any pages) | raster | yes | render + encode per page |

## Examples

```
# Image transcode (works exactly like before)
oxideav convert in.png -resize 800x600 -blur 0x2 out.jpg
oxideav convert movie.mp4 -resize 640x360 movie.mkv

# Synthetic sources (xc / gradient / pattern / synth / label / …)
oxideav convert "xc:red" red.png
oxideav convert "label:Hello world" greeting.png
oxideav convert "gradient:red-blue" gradient.png

# PDF rendering — the user's original ask
oxideav convert -density 300 input.pdf -background white \
                -alpha remove -alpha off page-%03d.png

# Single page extraction
oxideav convert input.pdf[0] cover.png
oxideav convert input.pdf[2-5] -density 150 page-%02d.jpg

# PDF → other vector formats (vector preserved, no rasterisation)
oxideav convert input.pdf[0] cover.svg
oxideav convert input.pdf       page-%d.svg

# PDF → smaller PDF (page extraction, vector preserved)
oxideav convert input.pdf[0] just-the-cover.pdf
oxideav convert input.pdf[10-20] excerpt.pdf

# 3D-asset format conversion (mesh3d feature; default-on)
oxideav convert cube.stl cube.obj
oxideav convert model.obj model.gltf
oxideav convert scene.gltf scene.glb
oxideav convert archive.usdz extracted.gltf

# 3D-asset per-format encoder options
oxideav convert cube.stl  -stl-format ascii  cube-readable.stl
oxideav convert model.obj -gltf-format glb   model.gltf  # binary container, .gltf extension
oxideav convert scene.gltf -gltf-format embedded scene.glb # JSON+data URI, .glb extension
```

## `--probe` (structural inspection)

`--probe INPUT` decodes the input far enough to extract structural
metadata, prints a compact summary to stdout, and skips any output
write. Pair with `--json` for a single-line machine-readable object
instead of the default pretty-printed `key: value` block.

The flag is mutually exclusive with an output positional:
`oxideav convert --probe in.gltf` is the supported shape; passing
both `--probe` and an output errors with a clear message rather than
silently picking one.

| Input class | Fields surfaced |
|---|---|
| Raster image | container, codec, width, height, bit_depth, color_space, alpha, frame_rate (animated), container metadata (title, artist, etc.) |
| PDF | is_encrypted, page_count, per-page width_pt × height_pt × orientation_deg, embedded_image_count, document-info dictionary (title, author, subject, keywords, creator, producer, creation_date, modification_date) |
| SVG | width_pt, height_pt, embedded_image_count |
| 3D (STL/OBJ/glTF/GLB/USDZ/MTL) | mesh_count, primitive_count, vertex_count, triangle_count, material_count, texture_count, animation_count, skin_count, node_count, root_count, topologies, bounding_box (computed from positions), per-mesh detail (name + counts + bbox), per-material name, per-animation name + duration_s + channel_count |
| Audio | container, codec, sample_rate_hz, channels, channel_layout, bit_depth, sample_format, duration_s, bit_rate, container metadata (title, artist, album, etc. via `Demuxer::metadata()`) |
| Video | container, codec, per-stream width × height × bit_depth × color_space × alpha × frame_rate_fps × duration_s, container metadata |

Embedded font count for PDF / SVG is reported as `unknown` today —
the producer crates don't surface a font-resource census, and
counting unique font names from the text-extraction layer would
over-count synthetic encoding splits. Documented as a cross-crate
follow-up.

`--watch` paired with `--probe` re-runs the probe whenever the input
file's mtime changes, polling once a second. Each fresh report goes
to stdout in the same format the one-shot mode would have used; in
`--json` form each report is its own line so the output is well-formed
JSON-lines (`convert --probe --json --watch in.png | jq` works). The
loop runs forever and exits only on Ctrl+C / SIGINT — useful for
live-monitoring a render-in-progress.

```
# Pretty-printed summary (default)
oxideav convert --probe input.pdf
oxideav convert --probe scene.gltf
oxideav convert --probe sound.mp3

# Single-line JSON (machine-readable)
oxideav convert --probe --json input.pdf
oxideav convert --probe --json movie.mp4 | jq .streams[0].codec_id

# Live-monitor a render-in-progress (re-probes on mtime change)
oxideav convert --probe --watch  rendering.png
oxideav convert --probe --json --watch rendering.png | jq -c .file_size_bytes
```

## "Did you mean?" suggestions

Unrecognised extensions on 3D-asset inputs / outputs trigger a
Levenshtein-distance hint:

```
$ oxideav convert model.gtlf out.obj
convert: no 3D decoder registered for input extension '.gtlf' (did you mean '.gltf'?) (known: .stl, .obj, .gltf, .glb, .usdz, .mtl)
```

The same helper now also fires on unknown-flag errors, so a
mistyped op (`-quailty`, `-reszie`, `--prbe`) gets a copy-paste
correct suggestion instead of a bare "unknown flag" message:

```
$ oxideav convert a.png -quailty 90 b.jpg
convert: unknown flag '-quailty' (did you mean '-quality'?)

$ oxideav convert --prbe in.png
convert: unknown flag '--prbe' (did you mean '--probe'?)
```

The lead the user typed (single vs double dash) is preserved on the
suggestion so it works without further edit. Distant tokens
(`-fnord`, `--xyzzy`) get the bare unknown-flag error with no
misleading hint.

The hint fires when the bad extension / flag is within `max(2, len/3)`
edits of one of the supported set; unrelated typos (`.png` vs the
3D set) get the base error with no misleading suggestion.

## Round-3 follow-ups

- Multi-input (`convert in1.pdf in2.pdf out.gif`) — IM allows it.
- GIF + TIFF raster outputs (need palette quantisation / multi-image
  TIFF encoder respectively; both are crate-side work).
- `-density` applied to raster inputs (silently dropped today).
- Registering PDF as a real `Demuxer` so `oxideav probe` /
  `transcode` / `run` see it. Today this side-channels through
  `convert` only.

Completed previously: `[0,2,4]` comma-separated and `[-1]` negative
page selectors (round 77).

## License

MIT. Copyright © 2026 Karpelès Lab Inc.
