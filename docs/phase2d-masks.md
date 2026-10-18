# Milestone 2d: masks

Local adjustments, as in Lightroom's Masking panel: a photo has a list of masks, each built
from shapes (linear gradient, radial gradient, brush) and each with its own set of sliders.
Built in two parts:

- **Part A**: the mask framework, linear and radial gradients, the per-mask sliders, the
  overlay and the on-photo handles, the sidecar and undo.
- **Part B**: the brush.

## What a mask is

- A photo has up to **8 masks** (`MAX_MASKS`), drawn in order. Each has up to **8 components**
  (`MAX_COMPONENTS`), each a shape and a mode: **add** or **subtract** (Lightroom also has
  intersect; later).
- A mask's weight at a pixel: `w = union(add) × (1 − union(subtract))`, where `union` is
  `1 − Π(1 − wᵢ)` (a soft "or" that never exceeds 1). Then `w × opacity` (the mask's Amount,
  0–100%, default 100).
- **Linear gradient**: two points, where the effect is full (`a`) and where it is gone (`b`).
  The weight is `smoothstep` across the band from `a` to `b`, constant along the band. Three
  lines on screen as in Lightroom: full, centre, gone.
- **Radial gradient**: centre, two radii, rotation, feather 0–100 (default 50) and an
  **invert** flag. The weight is 1 inside the ellipse shrunk by the feather and falls to 0 at
  the ellipse, with `smoothstep`; inverted it is `1 − w` (the default in Lightroom is not
  inverted: the effect is inside).
- **Brush** (part B): see below.

## Coordinates

Shapes live in **image space before crop and straighten** (the lens-corrected frame in the
raw's own orientation: `uv_in` in develop.hlsl), so cropping, straightening and rotating never
move a mask relative to the photo. To keep ellipses round, positions are stored in units of the
frame's **long edge** (`p = uv × dims / max(dims)`), not 0..1 per axis. Angles are in that
same space; the UI converts through the view's map, the same one crop mode uses for its
handles.

## Per-mask sliders

Each mask has: **Temperature, Tint, Exposure, Contrast, Highlights, Shadows, Whites, Blacks,
Clarity, Saturation** (−100 to +100; Exposure −4 to +4 stops; Temperature and Tint relative,
−100 to +100, as in Lightroom's local sliders). Texture, Sharpness, Noise and Dehaze need the
detail pass to be mask-aware: later.

How they combine with the global sliders, per pixel:

- **Exposure**: `cam × exp2(Σ wᵢ × evᵢ)`, applied with the global exposure (before tone).
- **Temperature and Tint**: a white balance gain per mask, worked out on the CPU as a ratio of
  camera-space multipliers (+100 Temperature ≈ the global slider moved +2000 K at 5500 K;
  match Lightroom's direction and roughly its strength), applied as `cam × exp2(Σ wᵢ ×
  log2 gainᵢ)`.
- **Contrast, Highlights, Shadows, Whites, Blacks, Clarity, Saturation**: the per-pixel slider
  is `global + Σ wᵢ × localᵢ`, clamped to −1..1 (Lightroom lets local and global add up; the
  clamp keeps the tone model inside the range it was fitted on). This means `toneGlobal`,
  `toneLocal`, `clarityShift` and `presence` take their sliders as an argument instead of
  reading `params[0]` directly; with no masks the result must be bit-identical to now.
- **Clarity**: the clarity base must be built when any mask has Clarity, even if the global
  slider is 0 (today it's skipped then).

Statistics (the 99.5th and 25th percentiles) stay global: a mask doesn't change what the
adaptive terms see. That's how Lightroom behaves near enough, and it keeps the pre-pass
unchanged.

## Data, FFI and storage

- `Edit` stays as it is (a flat `repr(C)` value). Masks are a second flat `repr(C)` value,
  `Masks` (core/src/masks.rs), with fixed-size arrays (`[Mask; MAX_MASKS]`, `count`), default
  zeroed = no masks, and a matching C# struct. Brush strokes don't fit a fixed struct; part B
  keeps them engine-side (see below).
- Engine/FFI: `set_masks(index, &Masks, commit)` alongside `set_edit`; the undo history keeps
  `(Edit, Masks)` pairs so undo covers both; `show` sends both.
- **Sidecar**: written only when there are masks, in Windedit's own namespace (as other
  non-Lightroom fields are), one element per mask with its components and non-default
  sliders. Reading Lightroom's `crs:MaskGroupBasedCorrections` is later.
- **GPU**: the masks go in their own structured buffer (a new SRV, t6), not the tone
  parameters, so `PARAMS_BYTES` and the root constants aren't touched. One root constant bit
  (or an existing flag word) says "no masks" so the develop pass skips the loop entirely.
- **Export** renders the masks: check that the export tiles use the same develop parameters
  and that the mask coordinates are tile-independent (they are, being in frame space).

## UI

- A **Masks** button in the toolbar (and **Shift+W**), next to Crop. It opens the Masks panel
  in place of the Edit panel's groups, as Lightroom does:
  - the list of masks (Mask 1, Mask 2…; renamable later), each with its components and an
    add/subtract and delete menu, and a **+ New mask** menu: Linear gradient (**M**), Radial
    gradient (**Shift+M**), and (part B) Brush (**K**);
  - **Add** and **Subtract** buttons under the selected mask, adding a component of a chosen
    shape;
  - the selected mask's sliders, with Amount at the top, and a **Reset** for the mask;
  - **Show overlay** (**O**): the selected mask's weight drawn as a translucent red over the
    photo by the develop pass (a flag and the mask's index in the constants), shown
    automatically while a handle is dragged, as in Lightroom.
- **Creating** a linear gradient: drag on the photo from where it's full to where it's gone. A
  radial: drag out from the centre (Shift for a circle).
- **Handles** in a XAML overlay over the swap chain, like crop's: linear: drag the centre line
  to move, the pin to move, the outer lines to change the band, and outside the lines to
  rotate; radial: the pin to move, four handles on the ellipse to resize, outside it to rotate;
  Feather and Invert in the panel. Delete removes the selected component.
- Leaving the Masks panel (Shift+W, Escape, or another tool) hides the handles; masks still
  apply.

## Part B: brush

- Brush settings: **Size**, **Feather**, **Flow**, **Density** (Lightroom's), an **Erase** mode
  (Alt held, or a toggle), and **[** and **]** for size, as in Lightroom. Auto Mask (edge-aware)
  is later.
- Strokes are stored as dab lists (centre in long-edge units, radius, feather, flow, density,
  erase) per component, engine-side, as `Arc<Vec<Stroke>>` so undo snapshots are cheap; FFI
  `add_stroke(index, mask, component, dabs, count, erase)` appends one stroke (one undo step).
  The sidecar stores them compactly (a delta-encoded list per stroke).
- Rasterised on the CPU into a **R16 layer per brush component** at 2048 px on the long edge
  (`rasterize` in masks.rs, with tests), as each dab arrives during a drag, uploading the dirty
  rectangle; the develop pass samples the layer (a Texture2DArray, t7) with bilinear filtering.
  The accumulation per dab is Lightroom's: `m += (density − m) × flow × falloff(r)` (erase
  moves toward 0).
- Cursor: two circles (size and feather) in the overlay.

## Performance

- No masks: the develop pass costs what it does today (checked with `bench`).
- Eight radial masks with two components each: under 0.3 ms extra on the RTX 5060 Ti at 4K;
  report the laptop number too if possible.
- Brush dabs during a drag: the view keeps its frame rate; rasterising a dab is microseconds.

## Tests

- masks.rs: weights for each shape (inside, edge, outside, feather, invert), union and
  subtract, the coordinate conversion, and (part B) rasterising.
- The develop pass with masks matches a CPU reference on a few pixels (as `tonefit gpucheck`
  does for tone), and with no masks it matches today's output exactly.
- Sidecar round trip; undo across `Edit` and `Masks`.
