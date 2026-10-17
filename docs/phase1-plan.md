# Phase 1 plan: culling that never waits

Phase 0 showed that editing is fast and opening is not: a cold raw decode takes 70–190 ms, and
the first photo of each size spends another 50–170 ms allocating GPU memory on the laptop.
Phase 1 builds the part of the app you use most, going through a day's photos, around one
rule: **the photo on screen never waits for a raw decode.** The wait still happens, but in the
background, ahead of you.

Phase 1 also covers the rest of "useful for me" from the original plan (basic adjustments,
crop, export), as later milestones. This document details the first two.

## What the library looks like

From `Pictures` on the desktop:

- **Day folders** (`2026\2026-06-06`): 88 of them, median 123 raws, the largest 1,475.
- **RAW+JPEG pairs:** 25,600 raws (ORF and RW2) next to 29,000 JPEGs. Most raws have a
  camera JPEG beside them with the same name.
- **Three cameras:** OM-5 Mark II, Lumix S1 and Lumix S9.
- **Lightroom sidecars:** 271 `name.xmp` files, all in `Lightroom Imports`, holding Camera Raw
  settings and ratings. The day folders have none yet.

## Design

### 1. Previews first, raws behind

Each photo has up to three images, from cheapest to best:

| Source | Size | Cost | Use |
|---|---|---|---|
| Embedded JPEG in the raw | 1920×1280 (S1, S9) to 3200×2400 (OM-5 II) | 10–30 ms | the loupe while culling, thumbnails |
| Camera JPEG beside the raw | full size | ~30 ms, less when decoded scaled | 100% focus checks before the raw is ready |
| Our raw render | full size | 70–190 ms decode + 15 ms GPU | editing, and 100% when there's no full-size JPEG |

On ←/→, the loupe shows the best image that's ready, at once, and swaps in a better one when
it arrives. Previews are decoded with Windows' WIC codec at the size they'll be shown; JPEG
can skip most of the work when shrinking by 2, 4 or 8. That makes them cheaper than the full
decodes Phase 0 measured.

**Decided: what Cull mode shows.** The camera's JPEG and our raw render won't match exactly
(different tone curves and sharpening), so swapping one for the other is a visible jump.
Cull mode shows camera JPEGs only, which keeps it consistent and instant, as
Photo Mechanic does. The raw render appears at 100% when the JPEG is smaller than the sensor,
and in Edit mode. Prefetching raws still pays off, because Edit and 100% need them.

### 2. Background decoding with priorities

A job system in the engine (`core/src/jobs.rs`) runs decodes on all cores except one, from a
priority queue that's rebuilt every time the selection moves:

1. the current photo's preview
2. the current photo's raw (in Edit mode or at 100%)
3. previews of the next and previous few photos, in the direction you're moving
4. raws of the next and previous 2–4 photos
5. thumbnails for the filmstrip and grid

A decode can't be stopped partway, but nothing is wasted: a finished one goes into the cache
even if you've moved on. Background jobs run as low priority with Windows' EcoQoS, which puts
them on the efficient cores of Lunar Lake and the 14700F, saves battery, and leaves the
performance cores for the photo on screen.

**Caches**, sized from the PC's memory:

- Decoded raws: 48 MB each at 24 MP. Budget about 10% of RAM, which is roughly 30 photos on
  the laptop. The window you're moving through needs about 9.
- Previews: the compressed JPEG bytes (1–3 MB) for many photos, plus decoded pixels for the
  few nearest.
- Thumbnails: in memory to start with. From embedded JPEGs on 7 cores, a 1,475-photo folder
  should take a few seconds. A disk cache is added only if measuring shows it's needed.
- GPU: the existing `ImageMemory` slots, demosaicing on demand (15 ms on the laptop).

### 3. GPU memory allocated before it's needed

Direct3D 12 can create resources from any thread, so allocation moves off the critical path:

- At startup, a background thread allocates slots for the photo sizes used last time (kept in
  settings).
- When a folder opens, the first raw's header gives its size, and a missing slot is allocated
  while the thumbnails load.

The first photo of a session then opens like every other.

### 4. A GPU thread instead of GPU work on the UI thread

Today the UI thread calls into the engine and waits for the GPU. In Phase 1 the engine owns a
GPU thread: the app sends it commands (show this photo, set this edit, resize, zoom), and
slider moves are merged so only the latest is drawn. Uploads use fences instead of waiting for
the GPU to go idle, so the next photo's upload overlaps with drawing the current one. The swap
chain's frame-latency waitable object keeps input-to-screen at one refresh. The UI thread only
handles input and layout, so it can't stutter while a photo opens.

### 5. Culling

- **Views:** loupe with a filmstrip (built first: this is where speed shows), then a grid.
  The loupe draws through our swap chain. The filmstrip and grid are virtualized WinUI
  `ItemsRepeater` lists of cached thumbnails; if that can't keep 60 fps with 1,475 items,
  the grid is drawn by the engine instead.
- **RAW+JPEG as one photo**, shown once, with the rating applying to both, as Lightroom does.
- **Keys** follow Lightroom and Photo Mechanic, so muscle memory carries over: ←/→ and
  Home/End to move, 0–5 for stars, P / X / U for pick, reject and clear, 6–9 for color labels,
  Z or a click for 100% at that point, G for the grid, Enter for the loupe, and Caps Lock for
  auto-advance after rating.
- **Filters:** picks only, unrejected, rating at least N, label.
- **Opening a folder:** Ctrl+O for a folder, plus a recent-folders list. Importing from a card
  is out of scope here.

### 6. Ratings in XMP sidecars

- Adobe's convention, `P1020943.xmp`, one per RAW+JPEG pair. Lightroom, Bridge and Photo
  Mechanic read the same file.
- Writes `xmp:Rating` and `xmp:Label`. Pick and reject go in our own namespace, since there's
  no standard field for them (Bridge's "rating −1 means reject" would lose the star rating).
- An existing sidecar is updated in place: only those fields change, and everything else
  (Lightroom's Camera Raw settings, keywords) is kept byte-for-byte. The update is written to a
  temporary file and swapped in, so a crash can't leave half a file. Ratings already in a
  Lightroom sidecar are read.
- Writes happen in the background and never block a keypress. Nothing is written for photos
  you only look at.

How *edits* are stored, in the same XMP file or a separate one, is decided in milestone 1c.
One file per photo would address the clutter worry.

## Targets (measured on the laptop)

| What | Target |
|---|---|
| ←/→ to the next photo's preview on screen | < 50 ms, and holding the key (~30 photos/s) keeps up |
| 100% focus check, photo already prefetched | < 100 ms |
| First photo of the session | same as any other (no allocation while you wait) |
| Folder of 1,475 raws: first screen of thumbnails | < 500 ms |
| ... all thumbnails | < 5 s |
| Rating key to saved in the sidecar | < 50 ms, never blocking |
| UI thread | never waits on decoding or the GPU |

`bench cull <folder>` will step through a folder at a set pace with prefetching on, and report
time-to-preview and time-to-raw for every step, so the laptop and desktop can be compared like
in Phase 0.

## Milestones

**1a. Engine for culling.** Job system with priorities and EcoQoS, raw and preview caches,
WIC preview decoding, GPU thread with fenced uploads, background allocation, and `bench cull`.
The Phase 0 test window shows previews instantly on ←/→ and swaps in the raw at 100%. Done when
the bench meets the first three targets on the laptop.

**1b. Cull UI.** Folder opening, loupe and filmstrip, grid, ratings, flags and labels with XMP
sidecars, filters, RAW+JPEG pairs. Done when a real day folder can be culled start to finish,
and Lightroom shows the same ratings.

**1c. Basic adjustments.** White balance (temperature and tint), exposure, contrast, highlights,
shadows, whites, blacks, vibrance and saturation, with undo. Edits saved per photo.

**1d. Crop and export.** Crop, straighten and rotate; JPEG export at full or reduced size,
rendered on the GPU in the background.

## Out of scope for Phase 1

Card import and backup, keywords and captions, lens corrections, noise reduction, masks,
deleting rejected photos (it needs its own care: Recycle Bin, both files of a pair), and the
Mac and Linux apps.
