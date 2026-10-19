# Edited thumbnails and previews

Today the filmstrip, the grid and the Cull view show the camera's JPEGs, so an edited photo
looks as shot everywhere but in Edit mode. Crop and rotation already apply to the JPEGs in the
view (they share the view map); the tone, colour, detail, lens and mask edits don't. This adds
**developed previews**: our own render of an edited photo, kept on disk, used wherever the
photo is shown small or fitted, and a clearer **edited badge**.

Unedited photos are unchanged: the camera's JPEG is the fast path and stays it.

## The developed preview

- A JPEG of the photo **as edited**: developed by the same GPU pass as Edit mode and export,
  with crop, rotation, straighten, lens, masks and the detail pass, fitted to **2560 px** on the
  longer side of the crop (enough for a fitted 4K view on the desktop; the laptop's is smaller),
  quality 90, sRGB.
- **Rendered on the GPU thread**, the way export renders, at the preview size: Zoom::Fit to an
  offscreen target, read back, encoded off the GPU thread. The detail pass runs at the level
  that size samples, as the fitted view does. A render should be about the cost of a frame plus
  the readback and encode (target: under 60 ms of GPU-thread time on the desktop, so it never
  stalls the view; measure it on the laptop too).
- **When it's made:**
  - **While editing**: on each committed change (slider let go, crop done, mask stroke ended),
    debounced by about 0.5 s, the photo's thumbnail is re-rendered in memory, so the filmstrip
    follows the edit live. The full preview is rendered and written to disk when the user
    leaves the photo or Edit mode (or after 3 s with no change), not on every commit.
  - **For photos edited before** (opened folder, sidecar has edits, no valid cached preview):
    a low-priority background job. A worker decodes the raw (the existing Raw job), then the GPU
    thread renders when it has nothing else to draw. Visible thumbnails first, then the photos
    near the one in view, then the rest. It must never slow down browsing: the photo in view,
    its neighbours' previews and visible thumbnails always come first, and raws decoded only
    for this are dropped from the cache right after.
  - Undo, a pasted or reset edit, or a sidecar changed elsewhere all just make the cached one
    stale (see the key below), so it's re-rendered by the same rules.
- **Cache**: `%LOCALAPPDATA%\Windedit\Previews\`. File name: a hash of the raw's path, size and
  modified time, plus a fingerprint of everything that changes the render (the `Edit`, the
  `Masks`, the brush strokes, the camera profile when that comes, and a render version number
  bumped whenever the pipeline's look changes). So an entry is valid exactly while its
  name matches, nothing has to be invalidated, and stale entries age out. Capped at 2 GB (a
  setting later), least recently used out first, checked at startup and after writes.
- A thumbnail of an edited photo is made from its developed preview (the same downscale as
  from a camera JPEG today), in memory and, since it's small, also cached on disk beside it
  (`.thumb.jpg`), so a folder of edited photos shows its edits from the first scroll without
  reading 2560 px JPEGs.

## Where it's used

- **Filmstrip and grid**: an edited photo's thumbnail is its developed one once there is one;
  until then the camera's, swapped without a flicker when the developed one arrives (the
  app already updates thumbnails from events).
- **Cull view, fitted**: an edited photo shows its developed preview instead of the camera
  JPEG (it is already cropped, so the view shows it whole: no crop applied a second time),
  falling back to the camera JPEG with crop and rotation, as now, until it exists.
- **Cull view at 100%** of an edited photo: the raw developed live, as Edit mode does (the
  preview is too small and a camera JPEG would be unedited). The raw is already wanted at 100%.
- Edit mode is unchanged: always the live develop.

## The edited badge

- A small badge on the thumbnail itself, bottom right, in both the **grid and the filmstrip**
  (the filmstrip's pencil in its caption row goes; the grid has none today): Segoe Fluent's
  sliders glyph (E9E9) in white on a translucent dark disc, readable on any photo, in both
  themes.
- "Edited" means any non-default `Edit` field or any mask, as `PhotoMeta.edited` already
  computes; it updates as soon as the edit changes (and goes when a reset leaves nothing).
- Tooltip on the badge: "Edited".

## Tests and checks

- Unit: the cache key changes with every kind of edit (a slider, crop, a mask, a brush stroke)
  and not with ratings, labels or picks; LRU eviction; a corrupt or half-written cache file is
  ignored and replaced (write to `.tmp` and rename).
- A developed preview matches an export of the same photo scaled to the same size (mean
  difference under 1 level, no shift: the same crop exactly).
- Bench: render-and-encode time per preview on the desktop (and laptop if possible); a
  950-photo folder with 30 edited photos and no cache: browsing stays as fast as with no edits
  while the backlog renders, and how long the backlog takes.
- By hand: edit a photo, watch the filmstrip thumbnail follow, leave Edit, Cull view shows the
  edit; restart: the thumbnails show edits at once from the disk cache; the badge in both
  views and both themes.
