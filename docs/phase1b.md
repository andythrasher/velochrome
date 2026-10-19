# Milestone 1b: the cull window

What [the Phase 1 plan](phase1-plan.md) asked of 1b, and what was built and measured.

## What it does

- **Open a folder** (Ctrl+O, or the arrow beside Open folder for the last ten), or start the app
  with a folder or photo on the command line. A RAW+JPEG pair is one photo.
- **Loupe with a filmstrip, or a grid.** Thumbnails show the stars, pick flag and color label;
  rejected photos fade back, as in Lightroom.
- **Lightroom's keys:** ← → (and ↑ ↓ PgUp PgDn in the grid), Home and End; 0–5 for stars; P, X
  and U to pick, reject and clear; 6–9 for the Red, Yellow, Green and Blue labels (the same key
  again clears it); Z, Space or a click for 100%; G for the grid, E or Enter for the loupe, Esc to
  step back out; D for Edit mode. With **Shift held or Caps Lock on**, rating moves on to the next
  photo.
- **Filters:** all, not rejected, picks or rejects; a minimum rating; a label. The engine is told
  the filtered order, so it decodes ahead the photos that are actually next.
- **Sidecars:** every change is written in the background to the photo's `.xmp`.

## Sidecars and Lightroom

- **Adobe's convention:** `P1020943.xmp` beside `P1020943.RW2` (and its JPEG). Lightroom,
  Bridge and Photo Mechanic read the same file.
- **Stars and labels** use the standard `xmp:Rating` and `xmp:Label`, with Lightroom's label
  names.
- **Picks and rejects** go in our own namespace, `urn:velochrome:1.0` (`velochrome:Pick` = 1 or −1).
  There's no standard field: Lightroom keeps flags only in its catalog. It's a URN so it claims
  no web domain while the app's name is provisional. Bridge's "rating −1 means rejected" is read,
  but never written, since it would lose the stars.
- **Existing sidecars are edited in place.** `core/src/xmp.rs` has its own small XML scanner that
  records where each tag and attribute is, so only the text of our three values changes;
  Lightroom's develop settings, keywords and layout stay byte for byte. It edits only the
  top-level `rdf:Description`, not the ones nested inside Lightroom's presets, resolves namespaces
  (older files use `xap:`), and handles element-form values (`<xmp:Rating>3</xmp:Rating>`).
- **Checked against the real thing:** all 271 Lightroom sidecars in `Pictures\Lightroom Imports`
  update cleanly in memory, with only our values changing (`bench xmpcheck <folder>` repeats the
  check; it writes nothing).
- **Written safely:** to a temporary file first and swapped in, on a background thread that
  merges bursts of changes to one photo. The engine finishes pending writes before it shuts
  down.

Still to confirm by hand: that Lightroom shows the same stars and labels after reading metadata
from these files (Metadata > Read Metadata from Files).

## Measured on the desktop

The largest day folder (1,478 photos, OM-5 II with camera JPEGs):

| | |
|---|---|
| Open the folder, including reading every sidecar | 14 ms |
| First screen of thumbnails (48) | 70 ms (target < 500 ms) |
| All 1,478 thumbnails | 2.1 s, about 720 a second (target < 5 s) |

`bench thumbs <folder>` measures this; run it on the laptop to compare.

## Found along the way

- **A chance `FF D8 FF` inside an OM-5 II raw** (one in 1,478) looked enough like a JPEG that the
  scan stopped there and never found the real preview behind it. The scan now tells a JPEG that
  runs past the bytes read (stop: it's real) from one whose segments don't chain together (not a
  JPEG: keep looking). Previews also fall back to the next candidate if one won't decode.
- **A thumbnail that can't be made is reported**, so its grid cell doesn't wait forever.
- **Memory:** the decode caches were sized from installed memory (10% for raws, 5% for
  previews), which on a 32 GB PC let browsing an S1 folder reach 5.2 GB. Culling moves through
  photos once, so the caches only need the photos around the one in view. They're now capped:
  raws at 768 MB (16 at 24 MP; about 680 MB on a 16 GB laptop), fitted previews at 256–512 MB,
  thumbnails at 128 MB, and full-size previews (96 MB each at 24 MP) at just two, the photo in
  view and the one before; neighbours' full-size previews are no longer decoded ahead. The same
  browsing run (40 photos stepped through, five 100% checks, twelve pages of grid) now peaks at
  1.7 GB, and `bench cull` is unchanged: next photo in 5–7 ms, 100% checks under 1 ms.

## Not in 1b

Deleting rejected photos, sorting other than by file name, selecting several photos at once,
keywords and captions, and card import: later milestones.
