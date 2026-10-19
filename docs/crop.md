# Crop, straighten and rotate

Milestone 1d's geometry, designed 2026-10-17. Lightroom's crop tool is the model for how it
feels; the engine's existing affine view transform does the work.

## What's stored

Geometry is part of the photo's `Edit`, so it gets undo/redo, the sidecar and export the way the
sliders do:

- `rotation`: quarter turns clockwise (0–3), on top of the camera's orientation.
- `angle`: straighten, in degrees (−45 to 45, positive clockwise).
- `crop`: left, top, right, bottom, as fractions of the frame (defined below). The default,
  [0, 0, 1, 1], is no crop.

In the sidecar: `velochrome:Rotation`, `velochrome:CropAngle`, `velochrome:CropLeft`, `CropTop`,
`CropRight` and `CropBottom`, written only when they differ from the default, like the
sliders.

## The frame

Take the photo after the camera's orientation and `rotation`: W × H pixels. The *frame* has the
same size and centre, with the photo turned by `angle` beneath it. A point p of the frame shows
the photo at c + R(−angle)·(p − c), where c is the centre and R a clockwise turn: a positive angle turns the picture clockwise, as in Lightroom. `crop` is a rectangle in the frame, so
it stays axis-aligned on screen while the photo turns under it, as in Lightroom.

The crop must stay inside the turned photo, with no empty corners. When the angle changes, the
crop shrinks just enough to stay inside, keeping its aspect ratio and centre. When the user drags
a handle past the photo, it stops at the edge. The maths lives in Rust (`geometry.rs`, which also holds the view's affine map) and is exposed to the app
(`we_constrain_crop`), so there's one implementation. It has five modes: shrink about the centre (angle or
aspect change), slide (moving the box, which slides along an edge), move the edges that changed
(free resize), scale about an anchor (locked-aspect resize) and fit an aspect ratio.

Frame → texture is still affine: rotation and straighten compose with the existing pan, zoom
and camera rotation into the same origin, stepX and stepY. Lens correction still warps in texture
space after that, so the crop works on the corrected photo.

## Where it applies

- **Loupe and Edit view:** the cropped photo is "the photo". Fit fits the crop, and 100% means
  one pixel of the crop. The camera's JPEG gets the same transform while culling, so a cropped
  photo looks cropped there too: the JPEG is already lens-corrected, and the geometry matches.
- **Export:** the request takes the crop and angle from the Edit. The output is the crop's size
  in pixels, with the angle applied.
- **Thumbnails:** stay uncropped for now (a known shortcoming, like edits).

## Crop mode

- **R** (Lightroom's key) or a Crop button at the top of the Edit panel enters crop mode. The
  loupe shows the whole frame at Fit, with the turned photo, and an overlay drawn by the app on
  a Canvas over the swap chain panel:
  - outside the crop is darkened;
  - a border, with eight handles;
  - a rule-of-thirds grid while dragging.
- **Dragging:** a handle resizes, and dragging inside moves. Dragging outside the box rotates
  (straightens), the way Lightroom's does. Shift keeps the aspect ratio while resizing.
- **The Edit panel shows a Crop group:**
  - an aspect ratio menu: Original, As shot, 1:1, 4:5, 5:7, 2:3, 16:9, and Free;
  - a lock toggle;
  - a swap for portrait/landscape (X, as in Lightroom);
  - a Straighten slider (−45° to 45°, 0.1° steps; double-click resets);
  - rotate left/right buttons (Ctrl+[ and Ctrl+]);
  - Reset.
- **Leaving:** Enter or R applies; Esc puts back the geometry the photo had on entering. Each
  drag commits on release, as the sliders do, so Ctrl+Z steps back through it.
- **Mapping the overlay:** the app has to know where the frame sits on screen. In crop mode
  the engine reports the frame's screen rectangle with each frame drawn, so the overlay maps
  frame fractions to pixels without guessing the layout.

## Later

- Reading Lightroom's `crs:Crop*` when there's no Velochrome crop.
- Cropped and rotated thumbnails.
- A straighten ruler (draw along a horizon).
- Auto-straighten.

## As built (differences from the design above)

- **No `Request.crop` / `angle`.** The export takes everything from the photo's `Edit`;
  `export::window(photo, &edit)` is the crop in whole frame pixels, so the output is the crop's
  size (width and height each rounded from the crop's edges, within a pixel of width × W).
- **`Zoom::Frame`** is crop mode's view (the whole frame at Fit). `Zoom::Actual` fractions are
  relative to the crop, and `Zoom::Region` is in frame pixels.
- **The shader clips to the picture.** `develop.hlsl` takes a `clip` rectangle (the crop's box on the
  output, so the rest of the photo past a crop at Fit isn't shown); root constants went from 48 to 52
  words. A pixel less than one output pixel past the photo's edge is clamped, not dark.
- **`FrameEvent`** carries `view_x/y/w/h` (the picture's rectangle on the view, in view pixels) and
  `frame_width/height`; `width/height` are of what is shown (the crop, or the frame in crop mode).
- **A zeroed `Edit`** (a C# `default`) reads as no crop: `we_set_edit` runs `geometry::clean`, which
  also repairs damaged sidecars. The app uses `Engine.Edit.None`.
- **Esc** steps the commits made in crop mode back with undo (so the redo stack keeps them);
  quarter turns (Ctrl+[ and Ctrl+]) also work outside crop mode, in the loupe.
- Crop edges are written to the sidecar with five decimals, the angle with two.
- **Intended crop.** Straightening never shrinks the box for good. The app keeps the box as the user
  last set it (resize, move, aspect, swap, reset, quarter turn) and every angle change shows that box
  shrunk to fit (`Shrink` from the intended box, not from the shown one), so turning back to 0° gives it
  back exactly. After any edit change the app (`SyncIntended`) keeps the intended box while the edit's box is
  what straightening makes of it, so undo and redo of an angle step keep it. Commits still save the
  shown box.
- **Margin.** In crop mode the frame is fitted with `geometry::FRAME_MARGIN` (9%) free on every side,
  leaving room outside the box to grab and turn. Pointer over the box shows resize, move and turn
  cursors (a crosshair with a ↻ glyph beside it outside the box). Turning shows a ten-by-ten grid and the angle;
  Shift snaps to whole degrees, otherwise 0.1° steps.
- **Moving the photo.** A drag inside the box moves the photo under a box that stays where it is on
  screen, as in Lightroom. The data is the same (the box slides the opposite way in the frame,
  constrained by `Slide`); the view follows the box while dragging (`we_set_crop_view`, the point of
  the frame at the view's centre, `Zoom::Frame { x, y }`) and eases back to the frame's middle on
  release. Arrow keys nudge the photo a screen pixel (Shift: ten) by moving the box the other way;
  unlike the drag, the frame stays put. Cursor: a hand inside the box.
- **A bug found on the way:** a box held as `f32` (as the app and the sidecar hold it) is a hundredth of
  a pixel outside the turned photo, `constrain_crop` took it for invalid after straightening, and Move
  collapsed it to a sliver. The base box now only has to be valid to 0.05 px (test
  `a_box_kept_in_f32_still_fits_and_slides_without_shrinking`).
