# Milestone 2a: Lens panel and white balance picker

What [the Phase 2 plan](phase2-plan.md) asked of 2a (manual vignetting, switches for the automatic
lens corrections, and an eyedropper for white balance) and how it's built.

## Using it

**Lens**, a new group in the Edit panel after Presence:

- **Vignetting** (−100 to +100) brightens (+) or darkens (−) the corners, as Lightroom's manual lens
  vignetting does. **Midpoint** (0–100, 50 by default) sets how far in from the corners the change
  starts: a lower midpoint reaches further in, a higher one keeps it in the corners. Double-click
  resets a slider (Midpoint to 50).
- **Distortion** and **Chromatic aberration** switches, on by default. They turn the automatic
  corrections from the raw's own data (docs/lens.md) off, to see the uncorrected photo. A switch is
  greyed out when the raw has nothing of that kind to correct, with the reason in its tooltip
  ("This photo has no chromatic aberration data to correct."; OM-5 II files at 300 mm carry only
  chromatic aberration, Panasonic files only distortion). Turning distortion off shows the sensor's
  own geometry, and the frame keeps its size: 100% and export are unchanged in pixels.

**White balance picker.** **W** (Lightroom's key), or the eyedropper button beside "As shot" in the
White balance group, turns it on: the cursor over the photo becomes a crosshair and the status bar
says "Click a spot that should be neutral · Esc cancels". Clicking sets Temperature and Tint so that
spot is neutral (a custom white balance, one undo step, so Ctrl+Z puts the old one back) and the
picker switches itself off. Esc, W again, leaving Edit mode, changing photo, or starting crop mode
also cancel it. A spot that can't be used (clipped, black, off the photo, raw still being read)
leaves the white balance alone, says why in the status bar for a few seconds, and keeps the picker
on for another try.

## Vignetting

Radial gain in stops on scene-linear light, in `develop.hlsl` right after the lens-corrected
sample and before white balance, exposure and tone, so every later stage sees it. With `r` the
distance from the centre of the **lens-corrected full frame** in half-diagonals (the corners are 1;
it ignores the crop, so cropping doesn't move the vignette, and rotation and straightening don't
either):

```
gain_stops = amount/100 · smoothstep(r0, 1, r)^1.5        r0 = 0.9 · (50 + midpoint offset)/100
```

At +100 the corners get +1 stop, and nothing inside `r0` changes. `r0` is 0.45 at the default
midpoint, 0 at midpoint 0 (the whole frame ramps) and 0.9 at 100.

This is the *opposite direction* to the brief's suggested `r0 = 0.9·(1 − midpoint/100)`: Lightroom's
help says a lower midpoint affects a larger area away from the corners and a higher one restricts it
to the corners, which is what the user expects, so `r0` rises with the midpoint.

- **Tone pre-pass.** `tonesmall.hlsl` adds the same gain to its tones (the small copy for the base
  layer, and the as-shot copy the percentiles read), so Highlights, Shadows and Whites see the
  vignetted image. That pass reads the sensor image, not the corrected frame, so it finds each
  sample's corrected radius by inverting the lens warp in three rounds (`correctedRadius` in the new
  `shaders/lens.hlsli`, which also now holds `lensGreenRatio`, shared by both shaders).
- **Constants.** The develop root constants went from 56 to 60 words (a `vignette` float4 at the
  end; the cap in `gpu.rs` is 64). The tone pre-pass constants are 22 of 32 words.
- **CPU reference.** `tone.rs` is unchanged: every tonefit/gpucheck case has vignetting 0, so there is
  nothing to drift. `develop::vignette_stops` is the CPU twin of the shader's gain, and is unit tested.
- **Not applied** to the camera JPEG shown while culling (it is its own picture, already as the
  camera processed it), or to thumbnails.

## The new `Edit` fields

`develop::Edit` (repr(C)) and C# `Engine.Edit` gained four fields at the end, in the same order:

| Field | Meaning | Default |
|---|---|---|
| `vignette_amount` | −100 to 100 | 0 |
| `vignette_midpoint` | the slider's 0–100 **as an offset from 50** | 0 |
| `no_distortion` | 1 turns the distortion correction off | 0 |
| `no_chromatic_aberration` | 1 turns the colour correction off | 0 |

All four are zero by default, so a zeroed struct (a C# `default`) is the default edit, as with the
crop; `Edit::clean_lens` (called by `we_set_edit` and the sidecar reader) clamps them, and repairs a
damaged sidecar. Sidecar names: `velochrome:VignetteAmount`, `VignetteMidpoint` (written as the slider
shows it, 50 default), `LensDistortion` and `LensChromaticAberration` (written as `0` when off);
each only when it differs from the default. Round trips are tested in `xmp.rs`.

`LensCorrection::with_switches(distortion, ca)` gives what is left of a correction: without
distortion, scale and zoom are 1 and the distortion terms 0 (the identity geometry, so the frame
keeps its size); without CA, the colour terms are zeroed; `None` if nothing is left. The develop pass,
the tone pre-pass and the picker all go through it.

## The picker's engine side

`we_pick_white_balance(handle, view_x, view_y, &kelvin, &tint)` (physical view pixels) returns 1 and
writes the answer, or fails with a message in `we_last_error` that the app shows. The GPU thread
handles it (`Command::Pick`), because it knows the view as the last frame drew it; the work in
`core/src/pick.rs` is CPU only and takes a fraction of a millisecond.

1. **View → photo** (`pick::sensor_point`): the same `geometry::view_map` the develop pass uses,
   with the viewer's edit and zoom (crop, straighten, quarter turns, camera orientation, Fit or
   100% and its pan), then the lens warp through `LensCorrection::source` with the edit's switches.
   Clicks outside the picture (the canvas around a fitted crop) are refused.
2. **Reading the colour** (`pick::average`): from the raw **mosaic** on the CPU rather than the
   demosaiced image: each of R, G and B averaged over its own samples in a window, black subtracted
   and scaled to 0..1, before any white balance (so it needs no GPU readback and nothing stalls the
   UI). The window is 5 sensor pixels across at 100%, and 3 screen pixels across when zoomed out
   (12 sensor pixels at Fit on a 24-megapixel photo), at most 63. Samples at 98% of the white level
   or more are left out; more than a tenth clipped fails ("clipped"), as does a channel mean under
   0.5% ("too dark").
3. **To temperature and tint**: the multipliers that make the average neutral (`G/R, 1, G/B`) go
   through `develop::multipliers_to_temperature` (clamped to the sliders' ranges).

Because the answer comes from the raw and not from the rendered picture, **picking the same spot
again with the new white balance on gives the identical answer** by construction; the check that
matters (below) is that the render of the spot really is neutral.

**Not built:** the optional magnified loupe beside the cursor.

## Keys

W and Esc (while picking) are registered in one place, `PickerShortcut` in `MainWindow.Picker.cs`,
called from `Root_KeyDown` next to the crop keys. W is used nowhere else; Esc is taken by the crop
and the photo/grid keys only when the picker isn't on (the picker's check runs first, and crop mode
and the picker exclude each other). The picker's click layer is a transparent `CursorGrid` over the
loupe (a Grid that can set its cursor), so the existing click-to-zoom isn't reached while picking.

## Checks

- **Unit tests** (all of `cargo test` passes, 60 tests here): the vignetting gain's shape
  (`develop.rs`), the lens switches (`lens.rs`), XMP round trips and damaged values (`xmp.rs`),
  and in `pick.rs`: a grey patch in a synthetic mosaic gives the as-shot white balance back at
  any spot, at Fit and 100%, and through a quarter turn, 7° and a crop; a coloured square is
  picked in its own place; clicks map to the right sensor pixel through rotation, crop, zoom and
  the lens warp (and the switch); clipped, black and off-photo spots are refused; the window grows
  when zoomed out.
- **`bench wb <raw>...`** runs, per photo, against the real GPU render:
  - *Picks.* At Fit, on the most even, neutral-looking mid-tone patch and four fixed spots: the pick,
    the re-pick, and the render of the spot with the pick applied (R, G and B within about 1 level on
    even ground, against up to 38 levels as shot).
  - *Mapping.* Through quarter turn + crop, 4° + crop at Fit and at 100%, and three turns with −3° and
    an off-centre 100%: the render of the edited view at a view point against the plain frame's render
    at the spot `sensor_point` says it is. Mean difference at the mapped spot 0.7–5.8 levels (the
    spread is texture and resampling) against 8–49 levels for a spot 14,9 pixels off, so it hits the
    same photo spot to within a pixel or two. Portrait (camera orientation Rotate90)
    photos are checked too.
  - *Vignetting.* −100, 0, +100 corner/centre brightness, the corner gain against Exposure +1, the
    midpoint, the crop not moving it, and a full-size export.
- **Numbers** (full output in `bench-wb.txt` in the notes folder):

| Photo | As shot | Spot picked (view 650,500; even sky and net) | Re-pick |
|---|---|---|---|
| OM-5 II PA150285 (starling) | 6406 K, +2.5 | 33081 K, +19.5 (render R/G/B 238.9/239.0/239.0) | identical |
| same, mid-grey feather area (800,500) | | 6792 K, −8.7 (59.5/57.8/57.0; as shot 4.2 apart) | identical |
| S9 P1070381, wall (1200,750) | 5518 K, +8.4 | 5521 K, +2.9 (109.5/109.4/109.6) | identical |
| OM-5 II P6060001, portrait (700,650) | 5909 K, +12.1 | 5916 K, +10.4 (136.5/135.2/135.2) | identical |

  Spots that are too dark are refused with the message (the starling's top left, the S9's dark
  left). Corner/centre brightness of the render (display light, not stops of scene light, which the
  tone curve compresses): starling −100: 0.438, 0: 0.650, +100: 0.863; S9: 0.065, 0.148, 0.322;
  portrait OM-5 II: 0.194, 0.447, 0.935. **The scene-light gain at the very corner matches Exposure
  +1:** brightening the last 2% by ×1.314 (Exposure +1 ×1.316) on the starling, ×3.061 (×3.081) on the S9.
  The full-size export has the same corner/centre ratio as the screen (0.860 against 0.863; S9 0.327 against 0.322).
- **`bench gpu`, develop pass** (median of 60, RTX 5060 Ti), before → after: fit 1920×1080
  0.086 → 0.089 ms, 2560×1440 0.187 → 0.189, 3840×2160 0.322 → 0.330, 100% 2560×1440 0.182 → 0.188,
  full-size 5184×3888 0.89 → 0.90. A few percent, the cost of one more constant block and a `pow`
  that is skipped when the amount is 0.
- **Driving the app** on a scratch copy of three raws (never the Pictures folders), by UI Automation
  and synthesized keys and clicks. Screenshots are in the notes folder (`phase2a/`): the Lens group
  (01), vignetting ±100 on the S9 (02, 03), the picker on (04), picked at Fit (05), a too-dark
  spot refused (06), picked at 100% (07), distortion and chromatic aberration off (08), reopened
  with the sidecar (09) and the exported JPEG (10). Confirmed: the sliders and switches; W,
  Esc and W-W; picking at Fit and at 100%; Ctrl+Z back to 6406 K "As shot" and Ctrl+Y again;
  the refusal message with the white balance and the picker kept; the S9's CA switch greyed
  out; the sidecar (`VignetteAmount="-40"`, `VignetteMidpoint="30"`, `LensDistortion="0"`) and
  the state after reopening; a 5184×3888 export carrying the vignette and the uncorrected geometry.

## Notes and shortcomings

- The picker averages a window of *raw* samples, so on texture (feathers, foliage) the answer
  depends on the window and so on the zoom: pick on an even area, or zoom in.
- A pick where the sensor clips one channel but not the others is refused only when more than a
  tenth of the window is clipped; a few clipped samples are left out.
- Auto-dismiss: the picker turns itself off after a successful click, as Lightroom's does by default.
- The camera's JPEG (cull mode, thumbnails) doesn't show vignetting or the switches.
- The tone model's "Whites" stretch reads the vignetted tones, so heavy vignetting moves where the
  adaptive terms think the image's top is, as any exposure change in the corners would.
- Tonefit's `gpucheck` wasn't extended; it passes the zero values the CPU reference knows.
