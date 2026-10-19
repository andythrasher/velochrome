# Milestone 1c: basic adjustments

What [the Phase 1 plan](phase1-plan.md) asked of 1c (white balance, exposure, contrast,
highlights, shadows, whites, blacks, vibrance and saturation, with undo, saved per photo) and
how it's built.

## Using it

D (or the Edit button) switches to Edit mode: the loupe shows the developed raw, with the Edit
panel beside it. Its groups follow Lightroom's:

- **White balance:** Temperature (kelvin, logarithmic from 2,000 to 50,000 K) and Tint (−150
  green to +150 magenta), starting from the camera's as-shot values; "As shot" goes back to
  them.
- **Tone:** Exposure (±5 stops), Contrast, Highlights, Shadows, Whites, Blacks.
- **Presence:** Vibrance, Saturation.

Dragging shows the change live; letting go saves it, as one undo step. Double-clicking a slider
resets it; "Reset all" resets the photo. Ctrl+Z undoes and Ctrl+Y (or Ctrl+Shift+Z) redoes,
per photo, for the session. A pencil on a thumbnail marks a photo with adjustments.

## How it works

- **One pass per frame, as before.** The white balance change, exposure and the two color
  conversions fold into one 3×3 matrix on the CPU; the five tone sliders fold into one
  1,024-entry curve table; vibrance and saturation are two numbers. The develop pass still
  costs 0.04–0.15 ms at 1440p on the desktop.
- **White balance as temperature and tint** (`core/src/develop.rs`). The as-shot multipliers
  are turned into a white point through the camera's color matrix and placed on the Planckian
  locus (Krystek's approximation): the nearest temperature, and the distance from the locus as
  tint, with Lightroom's sign (positive corrects greenish light, so the photo turns magenta).
  A new temperature and tint go back the same way to multipliers, applied relative to the
  as-shot ones the demosaic used.
- **The tone curve** keeps Phase 0's filmic base and reshapes the input in stops: Contrast
  steepens around middle grey; Highlights and Shadows shift the tones above and below it (up to
  2 stops, more the further away); Whites and Blacks move the ends. The result is kept rising, so
  tones never swap places.
- **Vibrance and saturation in OkLab**, where scaling colorfulness doesn't shift hue or
  lightness. Vibrance mostly lifts muted colors.
- `bench look <raw>` renders a photo with each slider pushed in turn, side by side, for checking
  by eye.

## Saved in the sidecar

Adjustments go in the same `.xmp` as the ratings, in our namespace: `velochrome:Exposure`,
`velochrome:Temperature` and `velochrome:Tint` (only for a custom white balance), and so on. Only
values that differ from as shot are written, rounded to two decimals; one file per photo. All
271 Lightroom sidecars still update cleanly with them.

**A risk to know about:** if Lightroom later rewrites the sidecar of a photo edited here (say
after a metadata change there, with automatic XMP writing on), it may not keep our values. The
day folders have no Lightroom sidecars today, so it can't happen there yet.

## Also in this round: memory

Before 1c, the decode caches were capped, taking browsing an S1 folder from 5.2 GB to 1.7 GB
with no change in speed (see [phase1b.md](phase1b.md)).

## Known shortcomings, for later

- **Highlights and Shadows are global**, through the curve, not local like Lightroom's, which
  work on regions. Strong settings flatten the photo where Lightroom's keep local contrast; the
  local version (an edge-aware filter, which Clarity in Phase 2 needs anyway) is the upgrade.
- **The curve is applied per color channel**, which shifts hues at extremes (Shadows +100 turns
  orange flowers yellowish). A hue-preserving curve fixes this.
- **Cull mode and thumbnails still show the camera's JPEG**, even for edited photos (the pencil
  badge says so). Showing edited photos' own renders there is a decision for a later step.
- White balance uses only the camera's D65 matrix; blending in its tungsten matrix by
  temperature would be more accurate far from daylight.
