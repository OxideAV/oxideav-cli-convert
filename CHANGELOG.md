# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
