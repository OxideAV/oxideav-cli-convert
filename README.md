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
| `-quality N` | forwarded to the sink codec when supported (JPG, WebP, …) |
| `-strip` | drop metadata on write |
| `-density N` | DPI for vector→raster (default 72; PDF / SVG inputs only) |
| `-background COLOR` | canvas + alpha-flatten background (CSS L3 named + `#hex` 3/4/6/8) |
| `-alpha {on\|off\|activate\|deactivate\|remove\|set\|opaque\|transparent}` | full IM grammar |
| `-rotate N` | quarter-turn only (`N ∈ {±90, ±180, ±270}`); 90/270 swap dims |
| `-flip` | vertical flip (rows reversed) |
| `-flop` | horizontal flip (columns reversed) |
| `-crop WxH+X+Y` | extract bbox; bbox-out-of-range errors cleanly |
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

### `[N]` / `[N-M]` page selectors

ImageMagick-style suffix on PDF inputs:

| Selector | Meaning |
|---|---|
| `input.pdf[0]` | page 0 only |
| `input.pdf[2-5]` | pages 2..=5 inclusive |
| _(no suffix)_ | every page |

Out-of-range indices error with a precise count: `page index 5 out
of range (input has 3 page(s))`.

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
| 3D format | matching 3D format (`.stl`/`.obj`/`.gltf`/`.glb`/`.mtl`) | decode → re-encode through `Mesh3DRegistry` |
| 3D format | non-3D extension (`.png`, `.mp4`, …) | error — 3D→raster rendering is a documented follow-up |
| 3D format | output with `%d` template | error — 3D scenes are single-document |

USDZ is read-only today (decoder registered, no encoder). Raster ops
(`-resize`, `-blur`, …) are silently ignored for 3D-asset conversions —
they have no pixel grid.

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
```

## Round-3 follow-ups

- Multi-input (`convert in1.pdf in2.pdf out.gif`) — IM allows it.
- Comma-separated and negative page selectors (`[0,2,4]`, `[-1]`).
- GIF + TIFF raster outputs (need palette quantisation / multi-image
  TIFF encoder respectively; both are crate-side work).
- `-density` applied to raster inputs (silently dropped today).
- Registering PDF as a real `Demuxer` so `oxideav probe` /
  `transcode` / `run` see it. Today this side-channels through
  `convert` only.

## License

MIT. Copyright © 2026 Karpelès Lab Inc.
