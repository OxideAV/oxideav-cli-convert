# oxideav-cli-convert

[![CI](https://github.com/OxideAV/oxideav-cli-convert/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-cli-convert/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-cli-convert.svg)](https://crates.io/crates/oxideav-cli-convert) [![docs.rs](https://docs.rs/oxideav-cli-convert/badge.svg)](https://docs.rs/oxideav-cli-convert) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

The engine behind `oxideav convert`: a flag-driven image/media
conversion CLI that works on images, video, audio, **PDFs, vector
content, and synthetic sources**, implemented on top of
`oxideav-pipeline`.

A one-frame PNG → JPG, a 90-minute MP4 → MKV with `-resize`, and a
multi-page PDF → page-numbered PNG sequence go through related code
paths — convert's job is to translate the conversion flags into work
the rest of the workspace already knows how to do.

## Supported ops

| Op | Notes |
|---|---|
| `-resize WxH[!>^<@]` / `N%` | bilinear; the source-aware modes (`%`, `>` shrink-only, `<` grow-only, `^` fill, `@` area, and the aspect-preserving default) are resolved against the input's geometry at plan time, carried through `-crop` / `-extent` / 90° `-rotate` / earlier resizes; `!` is literal |
| `-blur RxS` | Gaussian; `S` = sigma (defaults to `R/2`) |
| `-edge R` | Sobel magnitude |
| `-colors N` + `-dither {none\|bayer\|floyd_steinberg}` | palette quantisation via `pixfmt → Pal8` |
| `-format FMT` | bypass extension-based codec/container detection |
| `-quality N` | `0..=100`; the encoder's `quality` option exactly when its declared schema has one (JPEG, HEIC, AVIF, TIFF, …; the gateway's `SaveOptions::quality` rule); `--opt quality=` stays explicit; `.avif` defaults to 60 (lossy; `100` = lossless); out-of-range values reject at parse time |
| `-depth 8\|16` | output bit depth per channel for still-image outputs (a 10-bit HEIC → `-depth 8` → 8-bit PNG); without it the source's own depth is kept when the encoder can carry it |
| `-threads N` | the plan's execution budget (`ConvertPlan::execution_context`): the executor's decoders and filters, the still-image writer's encoder, and every decoder / encoder of a gateway-written still (`OpenOptions` / `SaveOptions::with_execution_context`, `oxideav-image` 0.0.5); `0` = auto (the host's parallelism, the default) |
| `--opt KEY=VALUE` | encoder option for still-image outputs, repeatable; validated against the target encoder's declared options (`oxideav info <codec>`), unknown keys / ill-typed values are typed errors |
| `-strip` | write no metadata blobs (ICC / Exif / XMP, stream level or per picture); `-thumbnail` implies it; the caller's own `-set` / `-comment` pairs are still written |
| `-set KEY=VALUE` / `-comment TEXT` | a container string pair for the output (`-comment` is the key `comment`), handed to the writer's muxer before its header exactly for the keys it carries (`Muxer::carries_metadata_key`; `Muxer::set_metadata`, core 0.1.41); a key the container has no field for is dropped with a `convert: note:` naming it (the HEIF sequence muxer stores `copyright` / `copyright:<lang>` only; a HEIF / AVIF still, PNG, JPEG store none today); repeatable, a later value for a key wins; `KEY=VALUE` is one argument (`KEY=` sets an empty value) |
| `--allow-containers L` / `--deny-containers L` | input restriction: comma lists of container names (deny wins, case-insensitive), checked on the header probe before any decoder exists; a refusal is `Error::Unsupported` with a `convert: restricted:` message (`restrict::is_restricted`) |
| `--allow-codecs L` / `--deny-codecs L` | the same over every video stream's codec id (the gateway's `OpenOptions` lists on the side-channels) |
| `-density N` | DPI for vector→raster (default 72; PDF / SVG inputs only) |
| `-background COLOR` | canvas + alpha-flatten background (CSS L3 named + `#hex` 3/4/6/8) |
| `-alpha {on\|off\|activate\|deactivate\|remove\|set\|opaque\|transparent}` | full alpha-op grammar |
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
| `-monochrome` | valueless shorthand for `-colorspace gray -colors 2 -dither floyd_steinberg` (1-bit B/W with error diffusion); always emits `floyd_steinberg` regardless of any prior `-dither none` |
| `-fuzz N[%]` | tolerance state for the next `-trim`; bytes (`0..=255`) or percent (`0..=100%` → `0..=255` rounded); no op emitted in isolation |
| `-trim` | auto-crop to the bounding box of pixels differing from the corner-pixel reference background by more than the active `-fuzz` tolerance; uniform-background inputs collapse to a `1x1` representable frame |
| `-roll ±X±Y` | circular pixel shift; pixels off one edge wrap to the opposite edge; both X and Y signed offsets required (`-roll +5+10`, `-roll -3+2`); width/height unchanged |

The geometry / negate / tonal ops above all wire through to the
matching `oxideav-image-filter` factory on the regular pipeline path
(`video.rotate`, `video.sharpen`, `video.gamma`, …). The PDF
side-channel keeps its inline `pixel_xform` implementation for the
five geometry / negate ops; the new tonal ops are pipeline-only on
the PDF path today (a follow-up will route them through the same
inline pre-encode hook).

Anything else reports `convert: unknown flag '-<op>'` (with a "did
you mean?" hint when the token is a close edit of a real flag) and
exits cleanly — no silent misbehaviour.

## Arg ordering

Ops can appear **anywhere** — before AND after the input. The first
non-flag positional is the input; the last is the output. Multiple
inputs (`convert in1.pdf in2.pdf out.gif`) is a documented
follow-up; today it errors with a clear message.

A `--` marker ends flag parsing: everything after it is a positional
even when it starts with a dash, so files literally named
`-weird.png` are expressible (`convert -- -weird.png out.jpg`).
Empty arguments (usually an unset shell variable) are rejected at
parse time with an actionable message.

`--help` (or IM-style `-help`) prints a usage synopsis whose flag
list is generated from the parser's own flag table — the same table
that powers "did you mean?" hints and is sync-tested against the
parser — so the help text cannot drift from what actually parses.

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

Bracket-suffix page selectors on PDF inputs:

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
page 0; duplicates are retained (`[0,0,0]` writes the
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
| **Raster** | `.png` `.jpg` `.bmp` `.webp`, plus every extension the caller's registry has a still-image encoder for (`tif`, `gif`, `qoi`, `tga`, `heic`, …) | render each page to RGBA, encode through `oxideav-image` |

Codec lookup goes through the caller's `RuntimeContext` —
`ContainerRegistry::container_for_extension` is the single source of
truth, so any sibling crate that registers itself (e.g.
`oxideav_qoi::register`, `oxideav_dds::register`) extends the
supported output set automatically, aliasing included (each
registering crate owns its extension aliases, e.g. `oxideav-mjpeg`
claims both `jpg` and `jpeg`). Extensions nobody registered
resolve to no codec: the pipeline then infers per-track (the right
answer for video containers like MP4/MKV that don't pin a single
codec) or prompts for an explicit `-format CODEC`.

### Side-channel stills ride `oxideav-image`

The PDF runner, the 3D renderer and the icon writer do not carry
format code of their own. A rendered page or scene is handed to the
[`oxideav-image`](https://github.com/OxideAV/oxideav-image) gateway
(`oxideav_image::encode` — encoder + muxer resolved from the output
extension through the caller's registries); the icon writer's input
is `oxideav_image::open` (any registered still-image format, primary
picture). What this crate adds on top:

- `-quality N` → the gateway's schema-gated `quality` (forwarded only
  to encoders that declare it: JPEG, lossy WebP, TIFF-JPEG, …; silently
  nothing for PNG / BMP / lossless encoders). Without `-quality` the
  encoder's own declared default applies — JPEG writes at 75, where the
  pre-gateway side-channel hard-coded 85. `-quality 85` reproduces the
  former pictures sample for sample (same quantisation tables; the
  header segment order differs because the registry encoder writes it
  differently from the crate's direct API).
- `--opt KEY=VALUE` is validated against the encoder's schema and
  forwarded, exactly as on the pipeline route (`.avif` implies
  `codec=av1` here too).
- The encoder input layout is the pipeline route's pick
  (`image_sink::pick_sink_format`, honouring `-depth`): RGB rather than
  RGBA after `-alpha off`, 16-bit with `-depth 16`. An encoder that
  declares no accepted layouts (JPEG) is probed in ImageMagick's order
  — the render's own non-RGB layout, then 4:2:0 YCbCr, 4:2:2, 4:4:4,
  RGB, gray — so an RGB render is a 4:2:0 JPEG as before and never a
  grayscale one. PNG / BMP / ICO / SVG / PDF outputs are byte-identical
  to the pre-gateway side-channel.
- `.ico` is never a raster frame: it stays the icon side-channel's.

The direct `oxideav-png` / `oxideav-bmp` / `oxideav-mjpeg` /
`oxideav-webp` dependencies are gone; `oxideav-ico` stays because an
icon bundles sub-images of *different* sizes, which the gateway's
one-geometry-per-sequence `encode_frames` cannot express.

### Icons (`.ico` output)

`convert src.X -define icon:auto-resize=16,32,48,256 out.ico` writes a
multi-resolution Windows icon (one entry per listed size, bilinear
downscale, entries from 64 px up as PNG payloads); without the
`-define` a single entry at the source size (1..=256 per axis). The
source is any still-image format the registry opens — PNG, BMP, JPEG,
GIF, TIFF, WebP, QOI, HEIC, … (primary picture of a multi-image file).

### Printf templates

When the output filename contains a single `%[0-9]*d` token (e.g.
`page-%03d.png`), convert fans out to one file per selected page
(PDF inputs) or per frame (everything else — image sequences,
animations, video; indices are 0-based), substituting the index.
Multiple `%d`s, `%s`, `%x`, etc. are rejected with a precise error.

## Still-image outputs

When the output resolves to an image encoder (`png`, `ppm`/`pgm`/
`pbm`/`pam`, `heic`, `avif`, `tga`, `jpg`, …) the planner reads the
input's container header (no decode) and plans what the pipeline's
generic defaults get wrong, then hands the decoded frames to
`convert`'s own encode + mux stage (`frame_tap`), which adopts the
still-image contract the `oxideav-image` gateway documents:

- **Which stream.** HEIF / AVIF files with an image-sequence track
  open as one still stream (the composed primary) plus one stream per
  track, in container order. `convert seq.heics out.png` writes the
  primary still (with a stderr note); `convert seq.heics frame-%03d.png`
  writes every frame of the first image-sequence track; an animation
  output (`.gif`, `.apng`) and a video output (`.mp4`, …) take the
  track too. Streams are picked by shape (a still is one tick of a 1/1
  time base), never by index. Single-video inputs are unaffected.
- **Which codec.** The gateway's `resolve_target`: the requested format
  name picks the codec on a multi-brand container (`.avif` is the
  `avif` encoder, `.heic` the `heif` one, `.jpg` → `mjpeg`); `--opt
  KEY=VALUE` sets any option the encoder declares, e.g. `convert in.png
  --opt qp=30 --opt tiles=2x2 out.heic`. A sequence-brand output
  (`.heics` / `.heifs`, `.avifs`) fed by a timed source codes the
  source's frames as an HEVC (AV1) track for the sequence muxer:
  `convert anim.gif out.heics`.
- **Which encoder input layout.** For a plain conversion the encoder
  gets the accepted layout that loses nothing the source carries
  (colour, alpha, bit depth) at the fewest bits per pixel — YUV → PNG
  is RGB, 10-bit YUV → PNG is 16-bit RGB, YUV → Netpbm a real pixmap;
  a packed-RGB source → JPEG is 4:2:0 YCbCr (4:4:4 from `-quality 90`;
  `--opt chroma=` / `sampling=` choose). The Netpbm extensions pick
  their family; `-depth 8|16` caps or raises the depth. Filter ops
  are planned on the filters' own declared layouts (`oxideav-image-
  filter` 0.1.3, `accepted_pixel_formats` / `output_pixel_format`):
  the running layout starts at what the stream decodes to and is
  converted ahead of an op exactly when that op's filter refuses it
  — to the declared layout that keeps what the source carries (grey
  stays grey, alpha and palette transparency keep an alpha channel,
  a deep source stays deep where the filter has a deep layout), with
  a `convert: note:` naming the op. Every 8-bit layout (JPEG-range
  YCbCr, palette, packed BGR, …) reaches every filter untouched
  through the filter's own routes; the sink pick follows the layout
  the chain emits. See *Filter layouts* below.
- **Quality.** `-quality N` is the encoder's `quality` option exactly
  when its schema declares one (`SaveOptions::quality`); without it
  every encoder's own schema default applies — `.avif` is quality 60
  (a photo-sized file, the `avif` encoder's default since 0.0.16,
  pinned by a test that reads the schema); `-quality 100` is lossless
  there and on HEIC.
- **Metadata.** The source stream's blobs (ICC / Exif / XMP, core
  0.1.40) go on the encoder's parameters and on the muxer's stream; a
  picture's own record stays on the frame, promoted ahead of the
  stream's for the one-picture file it becomes (the gateway's
  precedence). Whether the format embeds them is the encoder's
  (`oxideav-heif` writes all three; see its README's survey for the
  rest). `-strip` / `-thumbnail` withhold them all. Container string
  pairs (`-set KEY=VALUE` / `-comment TEXT`) go to the writer's muxer
  before its header for the keys it carries (`-set` row above); on
  the routes that do not open the muxer here — the executor's (a
  timed source into a sequence brand, an output the writer cannot
  size) and the gateway side-channels (PDF / 3D renders, `.ico`) —
  they are dropped with a note (follow-ups below).
- **Palette alpha.** A `Pal8` source reaches a palette-accepting
  encoder with its palette and alpha records untouched; an encoder
  without `Pal8` gets the picture expanded by the writer — alpha `0`
  for transparent entries on an alpha target, composited over
  `-background` (opaque white by default) on a target without alpha.
- **Budget.** The plan's execution context (`-threads N`, else the
  host's parallelism) reaches the executor's decoder and the writer's
  encoder (`Encoder::set_execution_context`), and every decoder and
  encoder the gateway creates for a side-channel still (PDF / 3D
  renders, the icon writer's input) through `OpenOptions` /
  `SaveOptions::with_execution_context`; the bytes never depend on
  it.
- **Restrictions.** `--allow-containers` / `--deny-containers` /
  `--allow-codecs` / `--deny-codecs` refuse an input on its header
  probe (container name, then every video stream's codec id) before
  any decoder exists — the gateway's `OpenOptions` lists on the
  side-channels, the same rules on the pipeline route.

Outputs the writer cannot size (an undeclared source layout, a
bit-packed `pbm` after filter ops) keep the executor's encode.
`OXIDEAV_CONVERT_TIMING=1` prints per-stage timings. Library callers
get the same knobs on `ConvertPlan` (`threads()`,
`execution_context()`, `quality()`, `strip_metadata()`,
`restrictions` / `with_restrictions`) and `plan_to_job` refuses a
restricted input the same way.

### Filter layouts: which ops keep a deep source

Planned from the filters' declarations (pinned by
`filter_layouts::tests::deep_layout_table`): *kept* = the op runs on
the deep layout and emits it; `A → B` = the planner converts to `A`
ahead of the op (the filter's declared 8-bit route for that source)
and the op emits `B`. `-resize` on a 16-bit PNG / TIFF stays 16-bit
end to end; the pointwise colour ops narrow once and the rest of the
chain stays 8-bit.

| op | `Rgb48Le` | `Rgba64Le` | `Gray16Le` | `Yuv420P10Le` |
|---|---|---|---|---|
| `-resize` / `-thumbnail` | kept | kept | kept | kept |
| `-crop` | kept | kept | kept | kept |
| `-flip` / `-flop` | kept | kept | kept | kept |
| `-roll` | kept | kept | kept | kept |
| `-rotate 90` (quarter turns) | kept | kept | kept | kept |
| `-rotate 30` (other angles) | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-extent` / `-trim` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-blur` / `-sharpen` / `-unsharp` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-negate` / `-gamma` / `-level` / `-threshold` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-normalize` / `-equalize` / `-auto-gamma` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-posterize` / `-solarize` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-brightness-contrast` / `-contrast` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-edge` | `Rgb24` → `Gray8` | `Rgba` → `Gray8` | `Gray8` → `Gray8` | `Rgb24` → `Gray8` |
| `-sepia` / `-modulate` / `-colorize` / `-vignette` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-colorspace gray` | `Rgb24` → `Rgb24` | `Rgba` → `Rgba` | `Gray8` → `Gray8` | `Rgb24` → `Rgb24` |
| `-colors` | → `Pal8` | → `Pal8` | → `Pal8` | → `Pal8` |

### Budgets: 12 MP at `-threads 1 / 4 / 8` (round 479)

`examples/convert_cli.rs` (this crate's `run` as a process over the
formats the tests register; Apple M4 Max, 18 logical CPUs, macOS
26.6, release build): `/usr/bin/time -l` wall and peak RSS, best of
two, over the 4032×3024 iPhone 13 Pro Max device sample (`src.heic`,
8-bit 4:2:0, 48 tiles), its PNG decode (`src.png`, 14.6 MB) and that
PNG's AVIF (`src.avif`, quality 60). *Before* is the round-477 crate
(b53c244), *after* this round; every cell is the same bytes at every
budget. The stage columns are `OXIDEAV_CONVERT_TIMING=1` at 1 → 8
workers: a stage whose time does not move is **serial** in the crate
that owns it.

| direction | before 1 / 4 / 8 | after 1 / 4 / 8 | peak RSS (8) | output | decode 1 → 8 | encode 1 → 8 |
|---|---|---|---|---|---|---|
| HEIC → PNG | 0.84 / 0.26 / 0.18 s | 0.93 / 0.30 / 0.20 s | 162 MiB | 14 618 082 B | 0.49 → 0.14 s | 0.41 → 0.07 s |
| HEIC → AVIF | 7.35 / 2.01 / 1.22 s | 7.81 / 2.37 / 1.34 s | 155 MiB | 778 684 B | 0.49 → 0.14 s | 6.9 → 1.2 s |
| HEIC → HEIC | 4.21 / 1.13 / 0.68 s | 4.26 / 1.32 / 0.88 s | 124 MiB | 2 249 399 B | 0.49 → 0.14 s | 3.7 → 0.7 s |
| HEIC → JPEG | 0.51 / 0.19 / 0.14 s | 0.51 / 0.22 / 0.19 s | 74 MiB | 1 709 867 B | 0.51 → 0.11 s | 0.05 → 0.05 s (**mjpeg encoder serial**, 50 ms) |
| HEIC → TIFF | 0.47 / 0.16 / 0.12 s | 0.48 / 0.19 / 0.15 s | 212 MiB | 36 581 254 B | 0.50 → 0.12 s | 6 → 6 ms (**tiff encoder serial**, trivial) |
| HEIC → WebP (`-quality 80`) | refused (stream copied into the webp muxer) | 2.69 / 2.47 / 2.57 s | 127 MiB | 905 164 B | 0.50 → 0.11 s | 2.38 → 2.35 s (**webp_vp8 encoder serial**) |
| PNG → HEIC | 4.12 / 1.30 / 0.90 s | 4.07 / 1.73 / 0.98 s | 196 MiB | 2 251 762 B | 0.28 → 0.27 s (**png decoder serial**) | 4.2 → 0.7 s |
| PNG → AVIF | 6.96 / 2.17 / 1.42 s | 7.39 / 2.54 / 1.56 s | 201 MiB | 775 510 B | 0.28 → 0.27 s (**png decoder serial**) | 7.5 → 1.3 s |
| AVIF → HEIC | 3.93 / 1.09 / 0.65 s | 4.09 / 1.25 / 0.70 s | 140 MiB | 1 566 485 B | 0.48 → 0.09 s | 3.8 → 0.6 s |

Before and after agree within run-to-run noise on the pipeline route:
the executor's decoder and the writer's encoder were already granted
the budget in round 477. What moved this round is the **gateway
side-channel**, which decoded serially whatever the budget: `convert
src.heic -define icon:auto-resize=256,128 out.ico` (the icon writer
reads through `oxideav-image`) is 0.51 s at `-threads 1` and 0.15 s
at `-threads 8` (344 → 367 MiB), where round 477 was ~0.6 s at every
budget. `convert src.heic -resize 50% out.png` is 0.66 → 0.17 s (1 →
8; decode + resize 0.55 → 0.15 s, PNG encode 0.12 → 0.02 s).

Stages that stay serial under the budget, by owner: the `webp_vp8`
encoder (2.35 s for 12 MP; its lossless sibling `webp_vp8l` needs
2.3 GiB and panics after 140 s on this picture — `oxideav-webp`
findings, reported), the `png` decoder (0.27 s; inflate), the `mjpeg`
encoder (50 ms) and the `tiff` encoder (6 ms). Every other stage
scales with the budget.

### Where the time goes: 12 MP HEIC → PNG

Measured 2026-09-28 on an M4 Max (release build, `sips`-produced
4032×3024 HEIC from a plasma-fractal PNG; CPU seconds via `getrusage`
so a loaded machine does not skew the split; each stage timed in
isolation through the crates' public APIs, then cross-checked with
`sample` on the CLI and `OXIDEAV_CONVERT_TIMING=1`).

| stage (owner) | 8-bit 4:2:0 source | 10-bit 4:2:0 source |
|---|---|---|
| process start + registries + plan incl. input header probe (`convert`) | 0.03 s | 0.03 s |
| demux — one `heif` packet (`oxideav-heif`) | < 1 ms | < 1 ms |
| decode — HEVC item (`oxideav-heif` → `oxideav-h265`) | 0.67 s | 1.58 s |
| pixfmt to the *old* target, RGBA (`oxideav-pixfmt`) | 0.007 s | 0.010 s |
| pixfmt to the planned target — RGB24 / RGB48 (`oxideav-pixfmt`) | 0.006 s | 0.028 s |
| PNG encode of RGBA, 48.8 MB (`oxideav-png` → `compcol` deflate ≈ 95 %) | 4.20 s | 3.82 s |
| PNG encode of the planned target — RGB24 36.6 MB / RGB48 73 MB | 3.12 s | 3.29 s |
| PNG encode with `-depth 8` (RGB24) | — | 1.99 s |
| PNG encode with `--opt filter=none` (RGB24, +80 % file size) | 0.9 s | — |
| mux + file write (`oxideav-png` container) | 3 ms | 6 ms |
| frame copies / allocator / executor plumbing (`oxideav-pipeline` + encoder-side buffering) | ≈ 0.1 s | ≈ 0.1 s |
| Netpbm encode, old target 1-bit `MonoBlack` (`oxideav-pbm`) | 0.010 s | 0.012 s |
| Netpbm encode, planned target RGB24 / RGB48 P6 | 0.004 s | 0.004 s |

So the "≈ 4.6 s the PNG route spends that the PPM route does not" was
the PNG encoder's deflate on a 48.8 MB RGBA buffer, and the PPM route
was fast because it wrote a **1-bit thresholded bitmap** (the pipeline's
default target for the Netpbm encoder), which is also why re-encoding
that PPM to PNG took 0.15 s. Nothing in this crate's own code was on
the hot path; what this crate could fix was the target layout.

End-to-end CLI CPU time, same sibling-crate snapshot, before → after
this round's planning changes:

| route | before | after |
|---|---|---|
| 8-bit HEIC → PNG | 5.72 s (RGBA) | 4.40 s (RGB) |
| 10-bit HEIC → PNG | 6.24 s (8-bit RGBA, 2 bits dropped) | 5.45 s (16-bit RGB, lossless); 3.57 s with `-depth 8` |
| 8-bit HEIC → PPM | 0.82 s (1-bit P4) | 0.81 s (P6, 36.6 MB) |
| 10-bit HEIC → PPM | 1.91 s (1-bit P4) | 1.94 s (16-bit P6, 73 MB) |

With the sibling crates at their 2026-09-28 tips (multi-threaded HEVC
item decode) the 8-bit HEIC → PNG conversion takes 2.9 s wall / 3.9 s
CPU, the 10-bit one 3.8 s wall (2.2 s with `-depth 8`), and either
PPM route well under a second wall.

Remaining cost is owned elsewhere: deflate speed / compression level
in `oxideav-png` (no level option is declared; `filter=none` is the
only knob), the encoder's two full-frame copies (`send_frame`
buffering + `video_frame_to_png_image`), and the pipeline's own copy
into the convert stage.

### Routing matrix (3D-asset input)

The `mesh3d` cargo feature (default-on) wires `oxideav-mesh3d` plus
the format codecs (stl/obj/gltf/usdz) through `oxideav-meta`'s
`populate_mesh3d_registry` helper. With the feature off the side-channel
disappears and 3D inputs fall through to the regular pipeline path.

| Input | Output | Action |
|---|---|---|
| 3D format | matching 3D format (`.stl`/`.obj`/`.gltf`/`.glb`/`.mtl`/`.usdz`) | decode → re-encode through `Mesh3DRegistry` |
| 3D format | raster (`.png`/`.jpg`/`.bmp`/`.webp`, or any extension the registry has a still-image encoder for) | software-render the scene to RGBA, encode through `oxideav-image` |
| 3D format | other extension (`.svg`, `.mp4`, `.ico`, …) | error with did-you-mean hint |
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
| `-bg` | CSS L3 named or `#hex` colour (3 / 4 / 6 / 8 hex digits) | transparent black | Render-canvas background fill. Distinct from `-background` so callers can keep canvas-fill and renderer-clear separate. |
| `-aa` | integer `1..=8` | `1` (off) | Supersampling factor for the 3D→raster renderer. The scene is rasterised at `N × output` and box-filtered back down. `1` is off; `2`/`4` are typical. Capped at `8` so a 1024² render at max-aa stays under ~80 MB framebuffer + z-buffer. |

### Routing matrix (PDF input)

| Input | Output | Template? | Action |
|---|---|:---:|---|
| PDF (any pages) | `.pdf` | no | pass selected pages through (vector preserved) |
| PDF (any pages) | `.pdf` | yes | error — `%d` + Scene-aware output is bogus |
| PDF (1 page) | `.svg` | no | write that page's VectorFrame |
| PDF (N pages) | `.svg` | no | error — suggest `%d` template OR `[N]` selector |
| PDF (any pages) | `.svg` | yes | one SVG per page |
| PDF (1 page) | `.png` / `.jpg` / `.bmp` / `.webp` / any registered still-image extension | no | render + encode |
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

## Planning guarantees

`convert` separates *deciding* from *doing*, and the deciding half
carries test-pinned guarantees:

- **Routing is pure.** `route::decide(&plan)` classifies every
  invocation (help / ping / probe / PDF side-channel / 3D→3D /
  3D→raster / ico / pipeline) without doing any work; the
  media-kind × media-kind dispatch matrix is pinned by IO-free unit
  tests, including precedence (input classification outranks output
  classification — `in.pdf out.ico` is PDF-runner business).
- **Every emitted job validates.** `plan_to_job` and
  `plan_to_render3d_job` run `oxideav_pipeline::Job::validate()`
  before returning, so a planner bug is a typed error at plan time.
- **Every emitted job is JSON-stable.** Serialise → re-parse →
  re-serialise is byte-identical, and f32 flag values serialise as
  the decimal the user typed (no f64-widening noise), so planned
  jobs are storable / replayable / snapshot-testable `oxideav run`
  documents. Golden snapshots pin representative argv lines.
- **Unplannable input is a typed error, never silence or a panic** —
  3D input × non-3D/non-raster output, `%d` templates or `--opt` on
  outputs that are not still images, unknown / ill-typed encoder
  options, empty arguments, malformed value grammars (pinned by an
  adversarial-argv corpus).

## Known follow-ups

- Multi-input (`convert in1.pdf in2.pdf out.gif`).
- `-density` applied to raster inputs (silently dropped today).
- `-define` on the pipeline route lands in the job's `codec_params`,
  which the pipeline does not forward to encoders; `--opt KEY=VALUE`
  is the channel for declared options.
- Metadata through the registry is carried by `oxideav-heif` today;
  the `avif` encoder, `png`, `mjpeg`, `webp`, `tiff`, `gif` do not
  embed `params.blobs` yet (per-crate adoption), so HEIC → AVIF drops
  the profile until `oxideav-avif` reads them.
- `-set` / `-comment` ride only the writer's muxer: the pipeline's
  `Job` has no container-metadata field (so a timed source into
  `.heics` cannot carry `copyright`), and the `oxideav-image` gateway's
  `SaveOptions` carries blobs, not string pairs (so the side-channels
  drop them). Both are asks on those crates.
- Filters without a declared deep layout (the colour and tonal
  families) narrow a 16-bit / 10-bit source to 8 bits ahead of the
  op (*Filter layouts* above); a deep path there is the filter
  crate's to add — the planner follows the declarations.
- `oxideav-webp`'s registry encoder declares no `quality` option, so
  `-quality` does not reach a `.webp` output.
- Registering PDF as a real `Demuxer` so `oxideav probe` /
  `transcode` / `run` see it. Today this side-channels through
  `convert` only.

## License

MIT. Copyright © 2026 Karpelès Lab Inc.
