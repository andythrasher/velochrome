# Milestone 2c: the colour mixer (HSL)

Hue, Saturation and Luminance for eight colour ranges, like Lightroom's Color Mixer in HSL mode. Built 2026-10-18.
Reference code and tests: `core/src/mixer.rs`; GPU: `mixer()` in `core/shaders/develop.hlsl`; panel:
`app/Windedit/MainWindow.Mixer.cs`; check tool: `bench hsl`.

## What it does

Eight ranges (Red, Orange, Yellow, Green, Aqua, Blue, Purple, Magenta), each with Hue, Saturation and Luminance, −100…+100,
default 0: 24 sliders. It runs in the develop pass **after the tone step and before Vibrance/Saturation**, in OkLCh on
display-linear sRGB (the space `presence()` already uses, so a hue change leaves lightness alone).

### Range centres (OkLCh hue, degrees)

Each is the OkLab hue of the sRGB colour at Lightroom's HSL hue (full saturation and value). Measured by the
`centres_follow_lightroom_hues` test, which fails if the table drifts from them:

| | Red | Orange | Yellow | Green | Aqua | Blue | Purple | Magenta |
|---|---|---|---|---|---|---|---|---|
| Lightroom HSL hue | 0 | 30 | 60 | 120 | 180 | 240 | 270 | 300 |
| **OkLCh hue** | **29.23** | **52.78** | **109.77** | **142.50** | **194.77** | **264.05** | **293.77** | **328.36** |

Red, Green and Blue are the sRGB primaries' hues. The gaps are uneven (24° Red→Orange, 57° Orange→Yellow, 33°, 52°, 69°, 30°,
35°, 61° Magenta→Red); that is where Lightroom's hues land in a perceptual space.

### Weights

A smooth partition of unity over hue: between neighbouring centres the two ranges share the pixel with raised-cosine
weights (1 − w, w; w = ½ − ½cos πt), so at most two ranges are active, they sum to 1, and the weights have zero slope at
every centre (no seams, no kink). Saturation and Luminance use these weights of the pixel's **original** hue.

**Chroma factor** g(C) = smoothstep(0.005, 0.06, C): 0 for greys (so neutrals and noise in near-greys are never touched),
1 from a modest chroma (muted skin is about 0.05–0.1). Hue, Saturation and Luminance are all scaled by it.

### Effects

- **Hue:** a cubic Hermite hue map through the moved centres. Centre *i* moves by slider × ½ × (the distance to the neighbour
  it moves toward: the next range for +, the previous for −). Between centres the map is a Hermite spline whose slopes at
  the centres are 1 (so it is the identity when nothing moves), limited to min(1, 3 × the mean slope of each side's span)
  (the Fritsch–Carlson condition): **monotonic for any combination of sliders**, and two neighbours moved toward each other
  can at most meet half way. This is a change from "weights times shifts", which can run backwards: with weights
  h' = h + (1−w)s_i + w·s_{i+1}, the slope is 1 + (s_{i+1} − s_i)·w′ and w′ reaches π/2L, so a half-gap shift on each side
  overturns it (Red +100 with Orange −100). The Hermite map keeps the half-gap reach and the monotonicity. The actual
  shift is g(C) × the map's.
- **Saturation:** chroma × (1 + g × Σ w·s). −100 reaches grey for the range's centre colour, +100 doubles it.
- **Luminance:** OkLab L × 2^(g × Σ w·l / 3), i.e. linear luminance × 2^(g × l): ±1 stop at ±100 for a colour with
  C ≥ 0.06 (L ≈ Y^⅓). Multiplicative, so near-black barely moves.
- **Gamut:** after the three changes, a result outside sRGB loses chroma (8 steps of bisection, in (L, h) of the result)
  rather than clipping. In-gamut results cost nothing extra. Input is clamped to 0..1 first, and the whole step returns the
  pixel untouched when nothing applies (all-zero for its hue, or a grey).

### No NaN, no out-of-range

`no_nan_and_always_in_range` runs eight extreme settings over an 11³ grid of values from 0 to 1 (including 1e-6, 1e-4,
0.999) and over out-of-range and NaN inputs: every output is finite and in 0..1. Other tests: greys and a near-grey
(chroma under 0.005) are returned bit-exact at ±100 on all 24; weights sum to 1, ≤ 2 non-zero, continuous at centres and
0/360; the hue map is monotonic and sums to one trip round the circle for all 16 single-slider extremes, each neighbouring
pair pushed together, and 300 random combinations; a centre moves by ½ gap in the slider's direction; Saturation −100 gives
chroma < 0.001 on each range's centre colour with L unchanged (and the sRGB primaries); +100 doubles chroma, keeping hue
(±0.1°) and lightness, and a strong colour stays in gamut with hue ±0.3°; Blue Luminance ±100 is ±1.0 stop (±0.2) and leaves
Red alone; the effect on a hue sweep has no jump.

## Parameters and cost

`ToneParams` (the structured buffer at t1) grew by 32 floats (`mixer::Params`: hue shift ×8, hue slope ×8, saturation ×8,
luminance ×8, all worked out on the CPU), and the buffer's room went from 256 to 512 bytes. The root constants stay at 60 words:
`canvas.w`, spare until now, carries "the mixer has work" (0 or 1), so with all 24 sliders at zero the shader skips it.

`Edit::mixer: [f32; 24]` (hue, saturation, luminance, each Red … Magenta; zeroed = default). The C# struct has 24 named fields
in that order (`HueRed` … `LuminanceMagenta`), with `GetMixer`/`WithMixer` by index (kind × 8 + range).

## Sidecar

`velochrome:HueRed` … `velochrome:HueMagenta`, `velochrome:SaturationRed` …, `velochrome:LuminanceRed` …, written only when not
zero; damaged values are put in range (`clean_mixer`). Tests: each of the 24 alone, all together into a new sidecar and into
the Lightroom sample, zeros write nothing, damage, and back-to-zero removes them. `bench xmpcheck` on the Lightroom
Imports sidecars: 271 update cleanly, 0 don't.

## Panel

**Color Mixer** group after Presence and before Detail, collapsible like the others. Three tabs (Hue, Saturation, Luminance;
toggle buttons), each with the eight sliders; a **Reset** link clears all 24. Double-click a slider resets it. Tracks:
Hue = the gradient the slider sweeps its range through (OkLCh at the range's centre minus half the gap to plus half the
gap), Saturation = grey to the range's colour, Luminance = dark to light through the colour. The track is the slider
template's `HorizontalTrackRect` with a gradient, redrawn when the theme changes; the light end is a little darker in
Light mode and the dark end lighter in Dark mode so the track stays visible on the panel. Undo/redo, sidecar and export need
nothing else, since they go through `Edit`.

**Not built:** Lightroom's targeted adjustment tool (drag on the photo). It was marked optional-if-cheap; it needs
picking a pixel's colour back from the rendered frame and a drag gesture on the canvas, which is not cheap in the middle of the
crop work.

## Checks

`cargo test` (the mixer's 10 tests and the sidecar test pass; two `geometry::tests` fail in the other session's crop work
in progress, not here), `cargo clippy --all-targets` clean, `cargo build --release` clean, `dotnet build app/Windedit`
0 errors.

### `bench hsl [raw...]`

Chart: 1440 columns of OkLCh hue (0–360°), six bands of lightness 0.30 … 0.92 at chroma 0.11 (or 90% of the gamut), and a
grey ramp; built as a synthetic raw, so the real pipeline (tone curve and all) renders it. Output goes to
`renders/hsl/` (original | edited). Measured at mid lightness at the column whose *rendered original* hue is nearest each
centre. Hue shift in °, chroma ratio, luminance in stops:

| setting | at its own centre | at all seven other centres | grey ramp | hues touched (>2 levels) |
|---|---|---|---|---|
| Blue Luminance −100 | −0.10°, ×1.008, **−1.01 stops** | 0.00° ×1.000 0.00 | 0 levels | 24.5% (202°–290°) |
| Orange Saturation +100 | −0.18°, **×1.258** (at that lightness, the gamut limits it), −0.02 | 0 | 0 | 16.6% (33°–107°) |
| Green Hue +100 | **+25.98°** (½ of Green→Aqua = 26.14°), ×0.993, +0.02 | 0 | 0 | 21.0% (113°–189°) |
| Red hue/sat/lum all −100 | chroma ×0.000 (grey), −0.91 stops | 0 | 0 | 21.0% |

So at the neighbouring centres the change is exactly zero (the weights are zero there) and the effect covers the half-gaps
either side of the range. (Red all −100 is a grey at its centre, so its hue shift is meaningless there; the table prints it
for completeness.) The chart's 8-bit render is a little under the ideal at +100 saturation because the chart's chroma is
already large in the bright bands.

Photos: P6060001 (green trees, blue floor line, blue roof, pink clothes), P6100369, and P7100199: pixels changed by
more than 3 levels (Fit): Blue Lum −100 5.9 / 8.3 / 7.7%; Orange Sat +100 1.1 / 18.1 / 6.7%; Green Hue +100 11.4 / 1.4 /
0.8%; Red all −100 1.6 / 14.8 / 1.5%. Side-by-side PNGs are in the notes folder (`phase2c/`): the foliage turns toward
aqua with Green Hue +100, the blue floor line goes dark with Blue Luminance −100, the portrait's skin goes strongly orange
with Orange Saturation +100.

### Timing (RTX 5060 Ti, whole `render_image`, median of 80 frames with Exposure changing, 1440p; includes the tone pre-pass)

| | mixer off | Orange Sat +100 | all 24 at ±100 |
|---|---|---|---|
| P6060001, Fit | 0.106 ms | 0.136 (+0.030) | 0.141 (+0.035) |
| P6100369, Fit | 0.206 | 0.252 (+0.046) | 0.264 (+0.058) |
| P7100199, Fit | 0.192 | 0.228 (+0.036) | 0.230 (+0.038) |
| P6060001, 100% | 0.275 | 0.324 (+0.050) | 0.340 (+0.065) |
| P6100369, 100% | 0.293 | 0.371 (+0.078) | 0.396 (+0.103) |
| P7100199, 100% | 0.268 | 0.322 (+0.054) | 0.324 (+0.056) |

All zeros costs nothing (the shader branch is skipped); active costs 0.03–0.10 ms at 1440p (it is a per-pixel
OkLab round trip and, for the out-of-gamut pixels, a bisection). Not measured on the Arc 140V.

### The app (scratch copy `C:\scratch2c`, nothing written under Pictures)

Driven by UI Automation, synthesized mouse and keys, screenshots in the notes folder (`phase2c/`): the group in Light (`mixer-light-*.png`) and Dark
(`mixer-dark-*.png`), full windows (`app-light-edited.png`, `app-dark-edited.png`), the reopened state (`app-6-reopened.png`) and the export
(`export-P6060001.jpg`, 3888 × 5184 with the mixer applied; the blue line and the jeans are grey, the blue roof dark).
Confirmed: the three tabs and their sliders; Hue +75 Green and −64 Red, Saturation +100 Orange and −97 Blue, Luminance −100 Blue
and +86 Orange; the tracks in both themes; double-click and the Reset link (the sidecar loses every mixer attribute);
Ctrl+Z / Ctrl+Y step a slider's commit back and forward (+100 → +1 → −100 on Saturation Orange); the sidecar
(`HueGreen="75"` … `LuminanceOrange="86"`, nothing for zeros); closing and reopening the folder loads all values back;
export from the Export popup (the photo is exported with the edit).

## Notes and shortcomings

- The Hue slider's reach is half the gap, and the gaps are uneven, so Aqua/Blue (the widest) move the most in degrees; at
  ±100 Blue → Purple moves 15° and Blue → Aqua 35°.
- At high lightness (L > ~0.85) and strong Luminance or Saturation the gamut limiter takes the chroma back, so a light colour
  gains less saturation than a dark one: the same trade-off as in any OkLCh mixer.
- Luminance of a colour with lightness already near 1 (a pale sky) saturates at L = 1; a chroma cut follows.
- The thresholds (0.005 / 0.06 chroma) were chosen so greys and sensor noise are inert but skin and muted colours respond;
  they can be tuned without touching anything else.
- The C# slider tracks duplicate the centres (`MixerCentres`); if `mixer::CENTRES` changes, change both.
- Keyboard focus of UI-Automation's `SetFocus` on a slider was unreliable for synthesized keys; mouse clicks on the
  track and then keys work.
