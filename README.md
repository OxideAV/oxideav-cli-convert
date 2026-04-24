# oxideav-cli-convert

The engine behind `oxideav convert`: an ImageMagick-style `convert`
CLI that works on images, video, and audio, implemented on top of
`oxideav-pipeline`.

A one-frame PNG → JPG and a 90-minute MP4 → MKV with a resize filter
go through the same code path — convert's job is to translate
IM-style args into an in-memory pipeline job, not to re-implement I/O.

## Supported ops (MVP)

| IM syntax | Pipeline filter | Notes |
|---|---|---|
| `-resize WxH[!]` | `resize` | bilinear interpolation |
| `-blur RxS` | `blur` | Gaussian; `S` = sigma |
| `-edge R` | `edge` | Sobel magnitude |
| `-colors N` + `-dither {none\|bayer\|floyd_steinberg}` | `pixfmt → Pal8` | palette quantisation |
| `-format FMT` | sink override | bypass extension |
| `-quality N` | codec param | forwarded to the sink codec when supported |
| `-strip` | muxer option | drop metadata on write |

Anything else reports `unsupported: convert: -<op> is not yet
implemented` and exits cleanly — no silent misbehaviour. Each new op
is a matching `oxideav-image-filter` primitive + one line in
`plan_to_job.rs` + one line in the pipeline executor dispatcher.

## Usage

```
oxideav convert in.png -resize 800x600 -blur 0x2 out.jpg
oxideav convert movie.mp4 -resize 640x360 movie.mkv
```

Identical flow; the second just happens to have more packets.

## License

MIT. Copyright © 2026 Karpelès Lab Inc.
