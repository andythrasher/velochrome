# Milestone 2b: detail (design)

Sharpening, noise reduction, texture and clarity, designed 2026-10-18 and built in two parts:
**A** (the detail pass, noise reduction, sharpening), then **B** (texture, clarity). Notes on what
was actually built go at the end, as in the other milestone docs.

## The problem

These are neighbourhood operations: each pixel depends on the pixels around it, at full
resolution. The develop pass runs once per *screen* pixel, sampling the demosaiced image's
pyramid at the level the view needs (`lod`), through the view's affine map, the lens warp and the
crop geometry. A screen pixel at Fit stands for many photo pixels, so it can't sharpen at
full-resolution radius by itself.

## The detail pass

A new pass runs before the develop pass and writes a **detail texture**: a processed copy of the
part of the image the view needs.
- **Space:** the texture is in image (texture) space, the unrotated demosaiced photo in camera
  RGB, linear: the same space and format as the pyramid. So geometry, the lens warp, white
  balance and tone all still happen in the develop pass, unchanged; the develop pass simply
  samples the detail texture where it used to sample the pyramid.
- **Level:** the pass works at the pyramid level the view samples, L = floor(lod), reading that
  level. Radii are given in full-resolution pixels and divided by 2^L. At 100% (L = 0) it is the
  real thing; at Fit the finest effects shrink below a pixel, which is also how a downscaled
  sharpened photo looks. As in Lightroom, 100% is where sharpening and noise reduction are judged.
- **Region:** only what the view covers: map the output rectangle's corners and edge midpoints
  back to texture space (`geometry::view_map` and the lens warp), take the bounding box, and add
  an apron for the filters' reach. At Fit this is about the whole level-L image, which is
  screen-sized. At 100% it's a screen-sized window of level 0.
- **When it runs:** only when its inputs change: the photo, the view (pan, zoom, resize), or a
  detail slider. Not for white balance, tone, presence, crop overlays or the picker, since its
  input is before all of those. So most slider drags stay as cheap as now.
- **Off:** with every detail control at its neutral value, the develop pass samples the pyramid
  directly, so nothing costs anything.
- **Export:** each tile gets its own detail pass at level 0 over the tile's texture footprint
  plus the apron. Tiles join without seams because the apron covers the filters' reach.
  Statistics the detail pass needs per image (the noise level) are computed once per image, not
  per tile.
- **Memory:** the texture is allocated once at the largest view size (and once for the export
  tile), like the other preallocated GPU memory: no allocation per frame.

## Noise reduction (part A)

- **Where:** first in the detail pass, on linear camera RGB, in a variance-stabilised space
  (a square-root, Anscombe-style transform), so shot noise is roughly even across tones.
- **How:** an à trous wavelet decomposition (B3 spline, five scales), as darktable's and
  RawTherapee's wavelet denoisers do (GPL-3, so their maths can be ported, with credit):
  - **Luminance:** soft-threshold each scale's luminance detail. The thresholds come from a
    noise estimate (the median absolute deviation of the finest scale, measured once per photo
    at level 0, on a central crop), times the slider.
  - **Color:** convert to an opponent space (luminance plus two chroma axes), and shrink chroma
    detail more strongly at coarse scales, where colour blotches live.
- **Sliders:**
  - Luminance (0–100, default 0), with Detail (0–100, default 50).
  - Color (0–100, default 25, as in Lightroom's default for raws), with Detail (0–100, default 50).

## Sharpening (part A)

- **Where:** after noise reduction in the detail pass, on log2 luminance. The pixel's colour is
  scaled by 2^(sharpening), so hues don't shift and highlight and shadow halos are symmetric.
- **How:** an unsharp mask on log luminance.
  - **Radius** sets the blur.
  - **Detail** suppresses halos at low values (it limits large differences) and lets fine
    texture through at high ones.
  - **Masking** weights the effect by edge strength (the gradient magnitude of a lightly blurred
    luminance), so at high values flat areas such as sky and skin aren't sharpened.
- **Sliders, as in Lightroom:**
  - Amount (0–150, default 40);
  - Radius (0.5–3.0, default 1.0);
  - Detail (0–100, default 25);
  - Masking (0–100, default 0).
  Alt-dragging Masking shows the mask (white = sharpened), as Lightroom does. That's optional in
  part A.

## Texture and Clarity (part B)

- **Texture** (−100 to +100): boosts or smooths fine-to-medium detail, a band of the log
  luminance between roughly 2 and 8 full-resolution pixels. It leaves the finest scale (noise) and
  large shapes alone. It's in the detail pass, after sharpening.
- **Clarity** (−100 to +100): local contrast at a medium-large scale.
  - It builds an edge-preserving base of log luminance at about a 512-pixel copy (a guided
    filter, as the tone model's base is built at 128 px), in the tone pre-pass, and samples it
    in the develop pass. The pixel moves by clarity × (x − base_mid), weighted toward midtones,
    so it doesn't clip shadows and highlights: a midtone contrast boost, as Lightroom's is.
  - This works at any zoom without the detail pass, so Fit shows it properly.

## Defaults and the Edit

Sharpening 40 and Color noise reduction 25 are on by default, as in Lightroom for raws. The
`Edit` struct must still mean "default" when zeroed (C# `default`, `Edit::default()`). So these
fields are stored as offsets from their defaults (as the vignetting midpoint is), and written to
the sidecar only when not default.

## Panel

- A **Detail** group in the Edit panel, after Presence and before Lens:
  - Sharpening (Amount, Radius, Detail, Masking);
  - Noise reduction (Luminance with its Detail, Color with its Detail).
- **Texture and Clarity** go in Presence, above Vibrance, as in Lightroom.
- A note under the Detail heading: "Zoom to 100% (Z) to judge sharpening and noise reduction".

## Checks

- **Test photos:** high-ISO raws from the user's library (an OM-5 II at ISO 3200 or more, an S1
  at 6400 or more; `bench lenstags` or a quick tag read finds ISO), plus the three reference
  photos.
- **Before and after** at 100%: noise measured on a flat patch (the standard deviation of a
  smooth region), and side-by-side crops for checking by eye.
- **No seams in export:** two overlapping tiles agree.
- **Fit matches export:** the fitted view matches the export downscaled, within a few levels.
- **Timing:**
  - The detail pass at 1440p (Fit and 100%) under about 4 ms on the desktop (RTX 5060 Ti).
    Report it; it must also be usable on the laptop's Arc 140V.
  - Tone and white balance drags don't rerun it.
  - The develop pass is unchanged when every detail control is neutral.

## As built, part A

Detail pass, noise reduction and sharpening (`core/src/detail.rs`, `core/shaders/detail.hlsli` and
`atrousblur`, `atrousv`, `atroushrink`, `sharpblur`, `sharpen`). Reviewed once and tuned; the second
round (colour, noise-aware sharpening, speckle, memory) is folded in below.

### How it works

- **Level and region.** The level is `floor(lod)`, one coarser if the region would pass 4.5 megapixels
  while zoomed out (Fit of a 24 MP photo on a 1440p view), or 9 MP at any zoom. Export tiles are level 0.
  The detail texture covers only what the view needs: the bounding box of the view's border (16 points
  per side, through the lens warp) plus a few texels. It is worked in **tiles of 768 × 768 output
  texels with an 80-texel apron** (NR reach 62, sharpening 10), so scratch memory does not depend on
  the view. The develop pass takes four taps from it when a pixel covers more than 1.2 texels and falls
  back to the pyramid outside it.
- **Memory.** Five 928 × 928 RGBA16F scratch textures (34 MB, made on first use) plus the detail texture
  itself, sized to the view's region (8 bytes a texel). **62–64 MB at 1440p, 97 MB at 4K** (it was
  300 MB). The luminance blur for sharpening reuses two NR textures. The warm-up compiles the kernels
  and frees its memory. Descriptor heap: 4096.
- **Reruns.** Only when the photo, the view or a detail slider changes (key: image id, region, detail
  values, noise). Dragging exposure, contrast or shadows (30 frames) and white balance (10) ran it 0
  times; a sharpening drag runs it every frame.
- **Noise estimate:** the median of the finest B3 detail of a 512 × 512 central crop at level 0, per
  stabilised channel, once per photo (3 ms readback). Moving it to the GPU was not done.

### Noise reduction

À trous B3 wavelets, five scales, in a square-root-stabilised opponent space (luminance, and two
colour axes). Below zero the root continues as its tangent line, which keeps black unlifted.

- **Thresholds** come from the estimate and the white-noise ratios, scaled by the per-scale factors
  measured on the four high-ISO raws (`bench noisescale`): camera noise is not white, so luminance holds
  1.7–2.7× the white prediction at coarse scales and colour 4–8×. The noise at scale j of level L is that
  of scale j+L of level 0.
- **Neighbourhood shrink (new).** Each scale's detail is shrunk by `max(0, 1 − (t / E)^p)`, where E is the
  energy over 3 × 3 pixels (centre doubled), not the coefficient alone. A lone large coefficient in a quiet
  neighbourhood (a speck) is removed; coherent texture and edges stay. p = 1 + 3 × Detail (1 soft, higher
  keeps strong coefficients).
- **Luminance.** Threshold 2.6 σ × slider; Detail 100 lowers the fine-scale thresholds to 50/70/90 % at
  scales 0–2 (half way at the default 50). Luminance 30 is clean but natural; 60 is smooth.
- **Colour (new).** The two colour axes shrink together as a vector, threshold 4.5 σ × √slider (so 25 is
  already strong), coarse scales harder (×1.3 … 3.5, evened out by Colour Detail). **Luminance guides it:**
  where luminance has structure the colour threshold falls to 30 %, so saturated edges (the red sign) keep
  their colour instead of bleeding.

### Sharpening

Unsharp mask on log2(luminance + 0.01), Gaussian sigma = Radius ÷ 2^level (skipped under 0.3 texels), gain
1.5 × Amount/100, edge mask from the blurred luminance's gradient. **Noise-aware (new):** the difference
from the blur is soft-thresholded ("cored") at k × the photo's luminance noise in stops at that brightness,
k = 2 × (1 − Detail), so 1.5 σ at Detail 25 and none at 100, less after Luminance NR. Detail also soft-limits
the difference (0.25–4 stops) against halos. At the default, flat dark areas are no noisier than with
sharpening off (`P6100369-sharpen-centre.png`).
**Not built:** Alt-drag to show the mask (`FLAG_MASK` and `DetailState::show_mask` exist; the app side
doesn't), Texture and Clarity (a band-pass step would slot in before `sharpen`).

### Edit fields

Eight, as offsets from the defaults (C# struct matching): `velochrome:SharpenAmount/Radius/Detail/Masking`,
`NoiseLuminance`, `NoiseLuminanceDetail`, `NoiseColor`, `NoiseColorDetail`, written only when not default;
`Edit::detail()` / `set_detail()`. The develop constants stay 60 words (the three spare words in the matrix
rows carry the detail rectangle). Panel: a Detail group after Presence with the zoom note.

### Numbers (RTX 5060 Ti, `bench detail`, full output `phase2b-a/bench-detail.txt` in the notes folder)

| | OM-5 II P6100369 (6400) | OM-5 II P6140475 (6400) | S1 P1018701 (12800) | S9 P1070453 (12800) |
|---|---|---|---|---|
| Pass, 1440p, 100%, defaults | 3.1 ms | 3.1 | 3.2 | 3.2 |
| Pass, 1440p, Fit, defaults | 1.0 | 1.0 | 1.1 | 1.1 |
| Pass, 100%, sharpening only | 0.42 | 0.41 | 0.43 | 0.44 |
| Develop pass, detail neutral | 0.21 | | 0.19 | |
| Memory, 1440p | 62 MB | 62 | 64 | 64 |
| Chroma noise at Color 25 / 50 / 100, % of off | 37 / 35 / 35 | 40 / 38 / 38 | 18 / 8 / 6 | 24 / 21 / 21 |
| Luminance noise, finest scale, Lum 25 / 50 / 100 | 82 / 55 / 56 % | 84 / 56 / 55 | 88 / 59 / 49 | 86 / 55 / 48 |

The finest-scale figures level off because the lowest-variance block still has real texture. Strong NR costs the
same as the defaults (the passes are the same). At 4K and 100% the pass takes about 7 ms and 97 MB. On the
Arc 140V it is bandwidth-bound, so expect 3–4 × the time; not measured.
Overlapping tiles agree to 1–6 levels (3 with the detail pass off on the noisiest, darkest photo, so mostly
the develop pass's own rounding on shadow noise), straightened 7° the same; Fit against the box-filtered 1:1
render: mean 1.7–4.6 levels. Export of 24 MP (S1): about 140 ms.

Crops, 100%, in the notes folder: `*-compare-flat.png` and `*-compare-busy.png` (off | default | Luminance 30 |
Luminance 60) for all four files, `*-nr.png` (off | default | Lum 30 + Color 25 | strong | strong + sharpen),
`*-sharpen.png`, `*-sharpen-centre.png`, and the app screenshots `01`–`06`.

## As built, part B

Texture and Clarity (`bench partb <raw>` runs the checks; crops and screenshots are in the notes folder's `phase2b-b/`).

### Edit fields and panel

`Edit.texture` and `Edit.clarity`, −100 to 100, default 0, so no offset is needed (zeroed `Edit` is the default); the C#
struct has them after `NoiseColorDetail`. Sidecar: `velochrome:Texture` and `velochrome:Clarity`, written only when not
zero (round trips, damaged values and "defaults leave nothing" tested in `xmp.rs` and `develop.rs`). The panel has
Texture and Clarity in Presence, above Vibrance. `Detail::texture` makes a non-zero Texture count as "not neutral", so
Texture alone starts the detail pass.

### Texture (detail pass)

The band is a difference of two Gaussians on log2 luminance, σ 1.4 and 4 px at level 0 (features of about 2–8 px;
σ halves per level, and it is skipped below 0.3 texels). It is added to sharpening's change in the same final kernel, so
the passes are the same three as before: the **blur kernel now makes three Gaussians at once** (sharpening's and the
band's two, one per channel of one texture, with 2.5σ taps), and `sharpen.hlsl` combines them. No new memory.

- **Edge-aware:** weight 1 / (1 + (|band| / 0.4 stops)²). Strong edges (a roof line against sky, the white sign)
  have a large band, so they get nothing: no halo at +100 and no blur at −100. Skin-scale detail has a small band and
  gets all of it.
- **Noise-aware:** a positive Texture is cored at 1.5 × the band's noise (the noise estimate in stops at the pixel's
  brightness, half of a white band's sigma allowing for demosaic correlation), so it does not bring grain back. On
  P6100369's flat block at +100 the finest-scale noise is 2.92 against 2.91 at 0.
- **Gain:** +100 adds 1.8 × the band; −100 removes all of it (skin and fabric go smooth, hair and edges stay).
- Same rules as the rest of the pass: recomputed only when the detail inputs change, radii scale with the level.

### Clarity (tone pre-pass and develop pass)

- **Base:** the tone model's small copy and guided filter, generalised (`guided_base` in `pipeline.rs`) and run again at a
  **512-pixel copy, radius 6% of the long edge, ε 0.8 stops²** (see "Stronger Clarity" below for why it is this wide).
  It writes its own coefficients texture (`Gpu::clarity`, 8 MB). It reads the same tones as the existing base (after
  Exposure, Contrast and Blacks, vignetting included). It is rebuilt only when the photo, white balance, Exposure,
  Contrast, Blacks, vignetting or lens switches change: dragging Clarity itself, or Highlights, Shadows, presence,
  and so on, reuses it. `toneguide.hlsl` takes ε as a constant now.
- **Develop pass:** `shifted += clarityShift(xg, a·xg + b, clarity)`. The shift is `clarity × k × weight × deviation`:
  deviation = 2 · tanh((x − base) / 2) stops (a soft limit, slope ≤ 1); weight fades in over −10…−4 stops from middle grey
  and out over 1…5.5 (so shadows and bright walls take part, only true black and clipped white taper off); k = 1.3 for +, 0.55 for −. At every zoom, since it doesn't use the detail pass.
  Constants live in `tone::clarity_shift` (CPU) and `clarityShift` (develop.hlsl). It rides in spare constant words
  (`lum.w` the amount, `vignette.zw` the copy's scale), so the develop constants are still 60 words; the table has a
  sixth SRV for the coefficients.
- **No swapped tones:** the weight changes slowly and the deviation's slope is at most 1, so output is increasing in
  the pixel's tone for any base. `tone::tests::clarity_shift_properties` checks that over a grid of slopes, and
  `less_exposure_is_never_brighter` now also runs with Clarity ±100 (and mixed with Shadows, Whites, Highlights).
- **CPU reference:** `tone::render` has Clarity, and `tonefit gpucheck` has three Clarity cases (it now turns the detail pass
  off, since the CPU reference has none): they agree with the CPU like the other tone settings do (median 1.9–2.1
  levels, 95th percentile under 5).

### Numbers (RTX 5060 Ti, 1440p)

| | OM-5 II P6100369 | OM-5 II P7100199 |
|---|---|---|
| Detail pass at 100%, defaults | 3.2 ms | 3.3 ms |
| ... with Texture +60 / −60 | 4.0 / 4.0 | 4.1 / 4.1 |
| Clarity, base rebuilt: extra frame time | +0.20 ms | +0.19 ms |
| Clarity, base reused (a Clarity drag) | +0.015 ms | +0.015 ms |
| Develop at 100% with / without Clarity | 0.296 / 0.274 ms | 0.276 / 0.255 ms |
| Fit with Texture 40 + Clarity 40 vs the 1:1 render box-filtered down | mean 3.6 levels | 3.6 |
| Tile vs whole, both on | 1 level | 1 level |

Texture's cost is the wider blur: 0.75 ms over the defaults, right at the 4 ms target (it was 4.2 before the taps were
cut to 2.5σ). Fit against export is 3.25 on P7100199 with both off, so the two settings add 0.35 or less.

### What it looks like

- Texture at 100% on skin (`*-texture-flat.png`, −100 | 0 | +100): −100 smooths the skin and the shirt weave and leaves the
  hair and edges; +100 brings out fabric and hair texture, no halo around the sign or the hair against the background
  (`*-texture-busy.png`).
- Clarity at Fit (`*-clarity-topleft.png`, `*-clarity-centre.png`, full frames `*-clarity±100.png`): +100 deepens the
  mid-tone contrast of columns, foliage and the people; −100 is flat and soft. No halos along the roof/sky and
  column/sky lines. (The first version looked modest on this photo; see "Stronger Clarity" below.)
  Clarity is now clearly visible on both photos.
- App: Texture and Clarity in the panel, undo/redo step them one at a time, the sidecar holds `Clarity="80"` and
  `Texture="80"` and they read back on reopening, and the export (5184 × 3888) carries both
  (`phase2b-b/01`–`06`).

### Stronger Clarity (second round)

The first version moved midtone local contrast by only a few percent. Two reasons. **The base tracked the detail:** with ε 0.02 and a
guided filter whose window variance is almost always far above ε, the base followed the pixel (a → 1), so x − base was tiny. The
base is now much smoother (ε 0.8, radius 6% of the long edge, a 512-pixel copy) and still preserves the big edges, where the
window variance is several stops². **The weight was narrow:** it now reaches the shadows and bright walls.

Measured (`bench partb`): the standard deviation of log2(Y + 0.01) minus the *unadjusted* picture's guided base (the same
copy, radius and ε), over pixels of display luma 0.1–0.6, on the 1500-px Fit render (measuring against the picture's own
base would be circular, since the base follows what Clarity does):

| | 0 | −100 | +100 |
|---|---|---|---|
| P7100199 (street) | 0.283 stops | 0.130 (**−54 %**) | 0.629 (**+123 %**) |
| P6100369 (portrait) | 0.315 | 0.131 (**−59 %**) | 0.727 (**+130 %**) |

(with the first, narrower base the same measure read 0.20 → 0.08 / 0.40 at k 1.0 / 0.5, about −55 % and +100 %; the wider base
is what makes the effect visible, since it makes x − base larger to begin with). The full frames
(`P7100199-clarity100.png` against `clarity0.png`) now show an obvious punch: the shadows under the awning and the columns
deepen, the surfaces gain texture, and −100 is soft and flat.

- **Monotonic:** the weight is wider (so slower), the deviation is limited by `2·tanh(·/2)`, and k stays at 1.3: the properties
  test (and `less_exposure_is_never_brighter` with Clarity ±100, alone and mixed) pass. The test also asserts that shadows and bright
  tones keep 20–90 % and 30–90 % of the midtone strength, and true black and clipped white none.
- **Halos:** 100% crops along the column against sky, the awning's edge and the railing/leaf edges at +100 show none
  (`P7100199-clarity-edge-sky-100pct.png`, `-edge-column-`, `-edge-roof-`: −100 | 0 | +100).
- **Cost:** the base rebuild is +0.20 ms at 1440p (it was 0.17); a Clarity drag reuses it (+0.015). The detail pass is unchanged
  (3.3 ms; Texture ±60 4.1–4.3 ms, a little over the 4 ms target on this photo; 4.0 on the portrait).
- **gpucheck:** the Clarity cases still follow the CPU reference, a little looser than before with the stronger setting
  (P7100199 median 2.9, 95th percentile 5.8 levels at +100 against 2.0 / 4.6 at the default; P6100369 0.9 / 2.6 against 0.8 / 1.6),
  inside the existing worst case of 7.0.
