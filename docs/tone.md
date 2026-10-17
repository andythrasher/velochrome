# The tone model

Milestone 1c's tone sliders didn't behave like Lightroom's. The default look kept skies blue
that Lightroom renders near white, Highlights −100 turned skies grey, and Shadows +100 lifted
far less. This round measured Lightroom Classic and fitted a new model to it.
`core/src/tone.rs` is the model (the CPU reference); the GPU develop pass implements the same
functions.

## How it was measured

- **The reference shots.** The user took screenshots of three photos (an S1 RW2, two OM-5 II
  ORFs) in Lightroom and in Windedit, at each tone slider's ±50 and ±100 and Exposure ±2: 118
  screenshots, listed in `slider training photos/manifest.csv` (the images themselves stay
  out of git).
- **`tools/tonefit`** does the measuring:
  - `extract` crops the photo out of each screenshot.
  - `linear` writes our engine's scene-linear image of each raw.
  - `score` renders every Lightroom setting with the model and compares brightness
    distributions (quantiles, so the photos needn't line up pixel for pixel; Lightroom's lens
    correction moves them a little).
  - `fit` adjusts the model's numbers by coordinate descent, each slider on its own shots.
  - `pairs` writes each Lightroom shot beside the model's render, for checking by eye.
  - `tools/tonefit/run.ps1` redoes the extraction from the screenshots.

## What Lightroom does

- **One base curve for every camera**, `y = 1.05 · s^1.65 / (s^1.65 + σ^1.65)` (with
  log2 σ = −2.34). It fits all three photos within 0.07 stops, from 9 stops below middle grey
  to white.
- **A brightness offset per camera.** Relative to the sensor's clipping point, the S1 is
  rendered 0.47 stops darker than the OM-5 II (Adobe's profiles carry such an offset).
  Without it, we rendered OM-5 II midtones about a stop too dark and kept highlight room
  Lightroom doesn't.
- **Exposure is a plain shift** along the curve.
- **The curve keeps hue**, in a wide color space: Adobe's published method puts the largest and
  smallest channels through the curve and keeps the middle one in proportion. Applied per
  channel, as we did before, pulling down a blue sky drained its color. In sRGB rather than a
  wide space, it darkened strong colors (a fifth of a stop on a deep blue sky).
- **Contrast** scales distances from middle grey (±100 ≈ ×1.5 and ×0.66).
- **Blacks** subtracts a little light (−100 removes about 0.4× middle grey's input), or adds
  some that grows slowly with brightness.
- **Highlights** moves the upper tones by up to about 1.1 stops near white, and a little even
  below middle grey.
- **Shadows** acts on regions, not pixels, and adapts to the photo. A photo whose shadows sit
  deep (the starling's beams) is lifted much further, by up to about 3 stops, than one whose
  dark tones are small details in brighter surroundings.
- **Whites** adapts too. A photo whose brightest parts sit well below white is stretched toward
  it, so Whites +100 can lift a dull photo's midtones by two stops, and barely changes one
  that already reaches white.

## The model

Each pixel's brightness (Rec.709 luminance, in stops) goes through:
1. the camera offset and Exposure;
2. Contrast and Blacks (`global`);
3. Highlights and Shadows, chosen from the pixel's *base*: an edge-preserving average of its
   surroundings (a guided filter of log luminance on a copy about 128 pixels across, radius
   10% of the long edge, ε 0.5 stops²). Also Whites, from the pixel itself (`local`).

The pixel's color is scaled by the resulting change, so ratios between channels are kept, and
the base curve is applied the hue-keeping way in Rec.2020. Two numbers from the whole image
steer the adaptive parts: its 99.5th percentile (Whites) and 25th percentile (Shadows), measured
as shot, before Exposure and the tone sliders.

**No slider may swap tones.** The adaptive parts react to the photo's own brightness, so they
could outrun it: in the first version, lowering Exposure with Shadows up deepened the shadows,
Shadows lifted them further, and the photo got brighter. Two rules prevent it: the percentiles
are measured as shot, so no slider moves them, and Shadows' adaptivity moves where it starts
acting (its knee) rather than adding lift, with its slope held under 0.75 stop per stop. A test
(`less_exposure_is_never_brighter`) sweeps Exposure under extreme settings.

## How close it is

The rms difference from Lightroom, in stops, over the shots of each slider:

| Default | Exposure | Contrast | Highlights | Shadows | Whites | Blacks |
|---|---|---|---|---|---|---|
| 0.13 | 0.17 | 0.17 | 0.15 | 0.34 | 0.13 | 0.13 |

Shadows gave up some accuracy (0.25 before) for the rule above: Lightroom lifts the starling's
deep shadows further than a slider that never swaps tones can. Before, with the old curve and
sliders, Shadows was 0.76 and most others around 0.2. By eye,
the `pairs` images match Lightroom's closely, without halos.

## Known gaps

- Three photos from two cameras is a small sample: the adaptive terms (Shadows' and Whites'
  stretch) are the least certain, and other makes than OM System and Panasonic get Panasonic's
  offset untested.
- On the starling, the worst fits: Shadows +100 lifts its deep beams 0.5 stop less than
  Lightroom, and Shadows −100 crushes them less.
- Colors are matched only through brightness. Lightroom's camera profiles also adjust hues
  and saturation, which this doesn't attempt.

## On the GPU

`core/shaders/tone.hlsli` holds the same functions; every number reaches it through a parameter
buffer written from `tone::MODEL`. Each time a raw is drawn, three small dispatches first reduce
the photo (never the full-resolution image):

1. `tonesmall`: a small copy at most 128 pixels across (block size = ceil(long edge / 128)), each
   pixel the mean tone of its block, read from the pyramid level whose long edge is the smallest
   still ≥ 256 (4×4 bilinear taps per block). In the texture's orientation.
2. `toneguide`: the guided filter on it, four box-mean passes (rows, columns, rows, columns), one
   thread per pixel, writing a and b.
3. `tonestats`: the 99.5th and 25th percentiles of the small copy, from a 2048-bin histogram over
   −24…+12 stops.

The develop pass then does `base = a·xg + b`, `local()`, and the hue-keeping base curve in
Rec.2020, as `render()` does. `tonefit gpucheck <raw>` compares the two. Differences from the CPU
reference: the GPU demosaics at full resolution, and its percentiles are of the small copy's
block means, so the 99.5th percentile sits a little lower than over every pixel (0.14 stop on a
photo with small bright spots), which moves Whites +100 slightly.
