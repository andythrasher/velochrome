# Handoff (2026-10-18)

Where Velochrome stands, how the work has been run, and what comes next. Conventions and build
commands are in [AGENTS.md](../AGENTS.md).

## The user and their setup

- Shoots an **OM System OM-5 Mark II** (ORF) and a **Lumix S1** (RW2); an **S9** too. Library in  the user's photo library, in dated folders (read-only for us). Uses Lightroom today and wants
  Velochrome to match its look and beat its speed.
- Desktop: i7-14700F, RTX 5060 Ti, 4K monitor at 150%. Laptop: Core Ultra 7 256V, Arc 140V
  (16 GB shared), which runs the published build in `artifacts\windedit-phase0`.
- Unsigned builds copied to other PCs are blocked by Smart App Control (signing is a later item).

## How the work has been run

- One senior model designs (a short design note in `docs/` per feature), briefs implementer
  sessions, reviews their diffs and screenshots, and commits on the user's OK. Implementers work
  on a written brief: the files, what done looks like, the hard requirements (tests, bench
  numbers, screenshots in both themes, scratch copies only, don't commit).
- Reviews have caught real problems; keep doing them. Two lessons from the crop work: check the
  geometry itself, not just that tests pass (the drag bugs were in `core/src/geometry.rs`), and
  when the user sends a screen recording, extract frames and look (with no ffmpeg here, a small
  local HTTP server plus a browser canvas worked).

## What's built (all on `master`, latest 3f64cbc)

- Culling: folders panel, filmstrip and grid, Lightroom keys for stars, flags and labels, XMP
  sidecars, card import (copy into dated folders, optional backup, cull while copying).
- Develop (GPU): a tone model fitted to Lightroom (docs/tone.md), white balance with a picker,
  lens correction from the raw's own data, manual vignetting, noise reduction, sharpening,
  texture, clarity, the HSL colour mixer, crop / straighten / rotate (docs/crop.md), masks
  (linear, radial, brush) with per-mask sliders.
- Export to full-size JPEG; edited thumbnails and previews with an on-disk cache and an edited
  badge (docs/edited-previews.md).
- UI: Edit panel with a tool strip (Edit, Crop, Masks), light and dark themes with Windshot's
  colour sets, double-click in the filmstrip to edit, drag to pan at 100%.

## Next: camera profiles

Design in [docs/camera-profiles.md](camera-profiles.md). In short: read Adobe's DCP profiles
already on this PC (Camera Matching for the OM-5 Mark II, S1 and S9), render them in the develop
pass per the DNG spec, add a profile drop-down, and later fit our own shippable profiles from
calibration shots the user will take. Build step 1 first and check it against Lightroom.

**Step 1 is built, reviewed and committed**, except that the camera profiles render 0.3–0.8 stops
too dark. That is waiting on Lightroom exports of reference photos (Adobe Color, Camera Natural,
Camera Vivid). See [camera-profiles-status.md](camera-profiles-status.md), "Review", for the
findings and what the exports will settle.

## Known gaps (worth picking up)

- Not yet tested on hardware: card import from a real card (and eject); masks' and previews'
  speed on the laptop; live thumbnail updates while dragging sliders in the running app.
- No GPU noise estimate for noise reduction (a 3 ms readback remains); no Alt-drag mask view
  for sharpening; no HSL targeted-adjustment tool; export tile seams up to 6 levels on dark,
  noisy photos.
- Masks: no Texture, Sharpness, Noise or Dehaze per mask; no intersect mode; no renaming; no
  auto-mask for the brush; the brush strip is clipped in a very narrow window.
- Later list (docs/phase2-plan.md): AI denoise and AI masks (ONNX via DirectML), an Edit panel
  filter box, reading Lightroom's `crs:Crop*`, "Remove from Recent", app signing.

## Prompt for the next session

Paste this to start:

> You're picking up Velochrome, a GPL-3.0 Lightroom-style raw photo editor for Windows (Rust +
> Direct3D 12 engine in `core/`, C# WinUI 3 app in `app/Windedit/`) at `C:\dev\windedit`.
> Read `AGENTS.md` and `docs/handoff.md` first, then `docs/camera-profiles.md`, which is the
> next milestone. Before writing code, read the parts of the develop pass the design touches
> (`core/shaders/develop.hlsl`, `core/shaders/tone.hlsli`, `core/src/pipeline.rs`,
> `core/src/develop.rs`, `core/src/color.rs`, `core/src/xmp.rs`, `core/src/developed.rs`) and
> the DNG specification's chapter on camera profiles (DCP tags, HueSatMap, LookTable,
> ProfileToneCurve, ForwardMatrix interpolation). Then build step 1 of the design: a DCP reader
> with tests on the real files under `C:\ProgramData\Adobe\CameraRaw\CameraProfiles`, the
> renderer in the develop pass (default profile bit-identical and no slower, checked with
> `bench`), the profile registry, the `Edit` field and sidecar, the drop-down in the Edit panel,
> and the previews' fingerprint. Never write into the user's photo library; use scratch
> copies. Don't commit until I say so. When done, report what you built, how closely Camera
> Natural and Camera Vivid match Lightroom on a few OM-5 Mark II and S1 photos, timings, and
> screenshots in both themes.
