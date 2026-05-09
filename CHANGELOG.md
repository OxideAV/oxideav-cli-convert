# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
