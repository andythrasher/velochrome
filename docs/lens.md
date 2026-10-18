# Lens correction

Micro Four Thirds and Lumix S cameras record distortion (and, for OM System, lateral chromatic
aberration) correction values in the raw file and expect the converter to apply them. Windedit
reads them and applies them in the develop pass. The camera's own JPEG is already corrected, so
it's the reference for checking (`bench lens`). The code is `core/src/lens.rs`; the maths follows
darktable's embedded-metadata lens correction (`src/iop/lens.cc`, `src/common/exif.cc`, GPL-3.0),
which follows what exiftool and exiv2 document.

rawler 0.8 doesn't expose any of these tags, so `lens.rs` reads them with a small TIFF directory
walker (ORF and RW2 are TIFF variants).

## The tags

**Panasonic RW2**: tag `0x0119` (exiv2 `Exif.PanasonicRaw.0x0119`, exiftool DistortionInfo) in the
first directory: 32 bytes, sixteen little-endian `int16`. With `v[i]` as those numbers:

- on if `(v[7] & 0x0f) == 1`
- `s = 1 / (1 + v[5] / 32768)`
- `a = v[8] / 32768`, `b = v[4] / 32768`, `c = v[11] / 32768`

The rest are unused. No chromatic aberration data.

**Olympus / OM System ORF**: the maker note (Exif `0x927c`) starts `OM SYSTEM\0\0\0II\4\0` (OM-5 II) or
`OLYMPUS\0II\3\0`; its directory is 16 or 12 bytes in, and offsets count from the note's start.
Entry `0x2040` is the ImageProcessing directory, which holds (all float32):

- `0x150a`, four values `k2 k4 k6 drs`: distortion. `0 0 0 1` means none. (`0x1510` repeats it.)
- `0x150c`, six values `red0 red2 red4 blue0 blue2 blue4`: lateral chromatic aberration.

Present on every OM-5 II file checked; zero distortion with only CA on the 300 mm lens. Tag
`0x1011` (DistortionCorrection2) reads 0 on files that carry correct data, so it's ignored.
Lightroom likewise applies the correction for these cameras whatever the camera setting.

## The model

Radii are in units of the half-diagonal of the (cropped) image, measured from its centre. For a
position at radius `r` in the corrected image, the sensor image is read at radius `r · f(r)` in the
same direction.

- **Olympus**: `rs = drs·r`, `f(r) = drs · (1 + rs²·(k2 + rs²·(k4 + rs²·k6)))`. Red and blue are read at
  `f(r) · (1 + c0 + c2·rd² + c4·rd⁴)` with `rd = f(r)·r`; green is the reference.
- **Panasonic**: the camera's polynomial goes the other way: a sensor radius `rd` corresponds to the
  corrected radius `rd · (1 + s·(a·rd² + b·rd⁴ + c·rd⁶))`. We need the inverse, so two rounds of
  `rd = r / (1 + s·(a·rd² + b·rd⁴ + c·rd⁶))` start from `rd = r`; `f = rd / r`.

**Auto-scale.** Darktable scales so no empty border shows. Here, `LensCorrection::fit` finds the
largest extra zoom (a factor ≤ 1 on the corrected position, `zoom`) that keeps all four edges of the frame
inside the sensor image for all three channels. It is 1.0 for most photos (the camera's own `drs`/`s`
already crop) and below 1 when the correction would pull in empty space: for example 0.9685 on an S1
60 mm, 0.9976 on an S9 50 mm. It matches the camera JPEG's framing (see below).

The output keeps the sensor image's pixel size, so **100% zoom and the "raw size" are unchanged**:
there is no separate corrected-and-cropped size. (The zoom changes the magnification by at most
3%, not the frame's dimensions.)

## On the GPU

`develop.hlsl` warps `uv` (after the view transform, in the unrotated texture's space) before
sampling: `lensWarp` does the same as `LensCorrection::source`. With CA, R and B are sampled at their own warped positions
(three samples instead of one). The tone pass's base layer lookup uses the green position. The lens
parameters are 16 extra root constants (48 in all). `Layer::Preview` (the camera JPEG, mode 1) isn't warped.
`WINDEDIT_NO_LENS=1` turns correction off at decode.

## Check against the camera

`bench lens <raw>...` renders at Fit about 1024 px wide (`LENS_WIDTH` changes it), with and
without correction, and finds the scale and shift that best align each with the embedded JPEG
(edge correlation). It then reports the displacement left after that fit in nine windows. The two
PNGs per variant (`LENS_OUT` sets the folder) are the render beside the JPEG, and an edge overlay
(render's edges red, JPEG's cyan, so misalignment shows as a coloured pair). For files with CA it also
reports red and blue against green.
`bench lensscan <folders>` lists focal length and model from the headers.

Residual misalignment, in pixels at 1024 wide (worst corner; scale is the fitted scale versus the camera's JPEG):

| File | Lens | Corrected | Uncorrected |
|---|---|---|---|
| OM-5 II P6060005 | 12 mm | 0.3 px, scale 1.0002 | 14.5 px (and scale 1.037) |
| OM-5 II P6060001 | 39 mm | 0.2 px, scale 1.0001 | 6.0 px |
| OM-5 II P8092665 | 300 mm (CA only) | 0.3 px | 0.1 px |
| S1 P1018493 | 24 mm | 1.1 px, scale 1.0028 | 25 px |
| S1 P1031668 | 60 mm | 0.2 px, scale 1.0023 | 4.2 px |
| S9 P1070842 | 24 mm | 0.2 px, scale 1.0010 | 8.5 px |
| S9 P1070381 | 50 mm | 0.8 px | 0.9 px (first-corner match is an outlier) |

Everything is within a pixel or two of the camera JPEG, and the fitted scale is within 0.3% of 1,
so the framing matches too. The S1 24 mm 1.1 px is at the top-left, in a patch with little detail.

**Chromatic aberration** (OM-5 II, measured at 3072 px wide): the 12 mm file's red fringe goes from
about +1 px to about +0.1 px with the correction. The 39 mm file's blue goes from about −1.0 px to
+0.7 px, an overshoot. A channel-against-channel edge measurement also picks up colour structure
in the scene, so this check is weak; the formula and signs are darktable's, and unscaled gain was
kept. This is the open question.

## Missing or odd data

- OM-5 II at 300 mm: distortion all zero (the lens needs none); only CA is stored.
- No Panasonic CA data is stored in `0x0119` (as in darktable).
- `0x1011` is 0 on OM-5 II files that still carry parameters.
- Untested: other makes, and older Olympus (`OLYMP\0`) maker notes, which aren't read.

## Vignetting (shading)

Looked for embedded shading data; none is usable, and for these photos none is needed.

- **OM-5 II**: the camera's own switch is `Exif.OlympusCs.0x050c` (ShadingCompensation, in the
  CameraSettings directory `0x2020`). It reads 0 on all 50 files sampled across the library, so the camera
  corrected neither the raw nor its JPEG. ImageProcessing has two undocumented blocks, `0x1103` (49152
  bytes, ORF only) and `0x1104` (4608 bytes); exiftool calls them UnknownBlock3?/4?. Neither is
  a plain gain grid: as 16-bit words they run up to 55834 and jump between neighbours, and the same
  file's 300 mm shot holds similar-sized values near 4096. Nothing public decodes them.
- **Panasonic**: `0x011b` (64 bytes) sits next to the distortion tag. The one analysis found (an
  exiftool forum thread) reads it as zone radii for chromatic aberration, not shading. Panasonic bodies
  apply their shading compensation to the raw themselves, when it's on.
- `bench vignette <raw>...` compares the lens-corrected render with the camera JPEG: sixteen spots
  and, more reliably, a median profile by radius, after matching tone curves at r < 0.4. Render minus
  JPEG at the outer ring, in stops: OM-5 II 12 mm +0.05, 39 mm +0.02, 300 mm 0.00; S1 24 mm 0.00,
  60 mm −0.01; S9 24 mm −0.11, 50 mm −0.24 (the last two are the only gaps over 0.1 stop). So our render's falloff
  already matches the camera JPEG's.

### Outcome: no automatic correction

Our lens-corrected render already matches the camera JPEG's falloff on the OM-5 II and the S1, and on the S9
within about 0.1 stop (`bench vignette`, outer ring). A per-photo match was built and tested: at decode time
it compared the raw's linear luminance with the embedded JPEG's on a 384-wide grid in the corrected frame,
matched tone curves at r < 0.4, took the median shortfall per ring of radius, and fitted
`g(r) = 1 + a·r² + b·r⁴` (at least 1 and rising), accepting it only with a corner gain of 0.05 stop or
more, little scatter, enough usable rings, and the same falloff on all four sides. It corrected one of the
seven test files (S9 24 mm, −0.11 → −0.05 stop), about 20% of 70 sampled files would have got 0.05–0.12 stop
corrections near the noise floor, and decode took 35–60 ms longer. So it was left out of the engine in favour
of a manual vignetting slider (Phase 2).

The analysis remains as a diagnostic in `tools/bench/src/vignette.rs`. `bench vignette <raw>...` prints, per
ring of radius, how much brighter the camera's JPEG is than the raw (stops), what the fit would apply (`a`, `b`, the
gain at the corner, scatter) or why it would decline, and the lens-corrected render's falloff against the JPEG
by ring. `bench lenstags <raw>` lists a file's tags (`TAG=0x1103` shows one tag's values; `SUB=2020` picks the Olympus
CameraSettings directory).
