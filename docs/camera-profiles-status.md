# Camera profiles, step 1: status (2026-10-19)

Where step 1 of [camera-profiles.md](camera-profiles.md) stands after one implementer session.
The work is **uncommitted** on `master` (on top of 4ceded4) and **not yet reviewed**. Most of it
is built and the engine side is tested. The app hasn't been built or driven, there are no
screenshots, and there is no real comparison against Lightroom yet. One benchmark in the new
bench tool is broken (see "Problems found"), and the colour match looks wrong in places.

## Files touched

New: `core/src/profile.rs`, `tools/bench/src/profiles.rs`.
Modified: `core/build.rs`, `core/shaders/develop.hlsl`, `core/shaders/tone.hlsli`,
`core/src/{color,develop,developed,engine,exif,ffi,gpu,lib,pipeline,xmp}.rs`,
`tools/bench/src/{main,previews}.rs`, `app/Windedit/Engine.cs`, `app/Windedit/MainWindow.Edit.cs`.
About 700 lines added to existing files, plus the two new files.

## What's built

- **DCP reader** (`profile.rs`): `Profile::from_file` / `from_bytes` parse the `IIRC` TIFF-like
  format: matrices, illuminants, `HueSatMap` 1/2 with dims and encoding, `LookTable`,
  `ProfileToneCurve` (turned into a 4096-entry LUT with a monotone cubic Hermite spline),
  `BaselineExposureOffset`, `ProfileName`, `UniqueCameraModel`. Test
  `parses_real_adobe_profiles` reads the real files under
  `C:\ProgramData\Adobe\CameraRaw\CameraProfiles`.
- **CPU reference** in `profile.rs`: `cam_to_prophoto` interpolates ForwardMatrix (or
  ColorMatrix) by inverse colour temperature between the two illuminants. Then
  `interpolated_hue_sat`, `apply_hsv_table` (with the spec's hue wrap-around), `eval_curve`,
  `tone_rgb` (hue-keeping, as `tone::tone_rgb`) and `eval_pixel` (the whole chain to linear
  sRGB).
- **Colour** (`color.rs`): ProPhoto ↔ XYZ D50, ProPhoto → Rec.2020 / sRGB, `forward_to_prophoto`,
  `camera_to_prophoto`; test `prophoto_round_trips`.
- **Registry** (`profile.rs`): scans Adobe's `Camera\<model>\` and `Adobe Standard\` folders once
  and parses lazily. IDs are global per profile *name* (`id_for_name`, `name_of`), and
  `get(make, model, id)` resolves a name for a camera. `id_for_lightroom_name` maps Lightroom's
  names ("Adobe Color" → 0). `profiles_for_camera` maps EXIF make/model to Adobe's naming. It
  finds 11 profiles for the OM-5 Mark II and 13 for the DC-S1. Test
  `registry_discovers_camera_profiles`. Windedit's own folders (`<app>\Profiles`,
  `%APPDATA%\Windedit\Profiles`) are **not** scanned yet.
- **`exif.rs`**: `camera_make_model(path)` reads Make/Model from the first 64 KB.
- **Edit field**: `Edit::profile: u32` (0 = Windedit's look, so a zeroed field is safe), mirrored
  in `Engine.cs`.
- **Sidecar** (`xmp.rs`): writes `windedit:Profile="<name>"` only when the profile isn't the
  default. On reading it falls back to Lightroom's `crs:CameraProfile`. Test
  `profile_round_trips_and_reads_lightroom_crs`. It does **not** write `crs:CameraProfile`. The
  design mentions both; decide whether we should write Lightroom's attribute too.
- **Fingerprint** (`developed.rs`): includes the profile name. `RENDER_VERSION` is not bumped,
  because the default look hasn't changed.
- **GPU** (`develop.hlsl` under `#ifdef PROFILE`, `tone.hlsli`, `gpu.rs`, `pipeline.rs`):
  - Two new kernel variants, `developprofile.dxil` and `developprofilemasks.dxil`, are built in
    `build.rs`. `develop.dxil` and `developmasks.dxil` are byte-for-byte untouched.
  - The profile kernel's root signature has 11 SRVs. `t8` holds HueSatMap, `t9` LookTable and
    `t10` the tone-curve LUT, all as typed buffers with trilinear interpolation done by hand.
  - `ToneProfileParams` appends `ProfileParams` (dims, encodings, has-curve, exposure gain) to
    `ToneParams`.
  - The CPU folds WB × ForwardMatrix → ProPhoto into `row0..2`. Exposure is applied between
    HueSatMap and LookTable. The tone sliders' scale goes in before the profile's curve, and
    ProPhoto → sRGB comes last, followed by the mixer and presence as before.
  - With masks, the mask's exposure is applied at the same place as the global exposure, not
    folded into the camera RGB.
  - `gpu.srv_profile` uploads the tables. It re-uploads the HueSatMap only when the profile or
    the temperature (in 0.1 K steps) changes.
  - The Clarity cache key includes the profile.
- **FFI / app**:
  - `Engine::profiles(index)`, `we_photo_profiles`, `we_profile_name`, and C# wrappers
    `PhotoProfiles` / `ProfileName`.
  - `MainWindow.Edit.cs`: `BuildProfileRow` / `UpdateProfileBox` put a Profile `ComboBox` at the
    top of the first group. Hovering an entry previews it (`SetEdit(..., commit: false)`), and
    selecting one commits it.
- **`bench profiles <raw files…>`** (`tools/bench/src/profiles.rs`), in three parts:
  1. A GPU-vs-CPU check on a synthetic frame.
  2. 4K timings.
  3. Renders compared against camera JPEGs and the tonefit Lightroom crops. Side-by-side strips go
     to `target/release/renders/profiles/<stem>-profiles.png`.

## What was verified

- `cargo test --release` in `core/`: 132 tests pass. This was run before the last bench-only
  edits; re-run it anyway.
- **Default path bit-identical**: `bench masks` on `PA150285.ORF` and `P1020780.RW2` gives all
  12 output hashes equal to the baseline. Its 4K timings were unchanged (0.346 / 0.334 ms).
- **GPU matches the CPU reference**: Adobe Standard, Camera Natural, Vivid and Portrait were each
  tested as shot, at 3200 K with all tone sliders, and with a radial mask. The mean difference is
  0.03 to 0.05 levels, with p99 at 1 and the maximum at 2 levels out of 255. So the shader does
  what `profile.rs` says. Whether `profile.rs` is *right* is the open question below.

## Problems found (fix or check first)

1. **The profile timings in `bench profiles` are invalid.** `time_4k` re-dispatches the kernel
   with zeroed constants (`&[0u32; 62]`) at descriptor slot 100, so `outWidth` is 0 and every
   thread returns at once. The ~0.03 ms it prints for every case is just overhead. Time the real
   `render_image_masked` dispatch between timestamps instead, the way `bench masks` does it. The
   cost of the profile path at 4K is therefore **unmeasured**.
2. **Camera Natural and Vivid probably don't match yet**, especially Vivid on the S1.
   - Numbers below: brightness RMS in stops over the 5th–98th percentiles, then mean OkLab ΔE.

   | Photo | vs | Windedit | Adobe Std | Natural | Vivid |
   |---|---|---|---|---|---|
   | PA150285 (OM) | camera JPEG | 0.58 / .043 | 0.55 / .036 | 0.64 / .055 | 0.62 / .054 |
   | P6140518 (OM) | camera JPEG | 0.29 / .029 | 0.29 / .027 | 0.29 / .035 | 0.26 / .034 |
   | P5280801 (OM, LR used Natural) | camera JPEG | 0.07 / .020 | 0.08 / .020 | 0.40 / .050 | 0.37 / .049 |
   | P1020780 (S1) | camera JPEG | 0.21 / .039 | 0.20 / .038 | 0.32 / .048 | **0.82 / .082** |
   | P1018088 (S1, LR used Natural) | camera JPEG | 0.15 / .068 | 0.14 / .069 | 0.31 / .075 | **0.75 / .108** |
   | P1018129 (S1, LR used Vivid) | camera JPEG | 0.25 / .037 | 0.22 / .036 | 0.33 / .046 | **0.80 / .094** |

   - Adobe Standard comes out about as close as Windedit's own look, which is a good sign for the
     matrices and the HueSatMap.
   - Natural and Vivid should land *closer* to the camera JPEG than Adobe Standard, since that's
     what Adobe fitted them to. Here they're further away. Vivid on the S1 is off by about
     0.8 stops in every channel, which looks like a brightness or scale error, not a colour one.
   - Things to check:
     - **Curve input scale.** Our base curve puts middle grey at input 2^−3.72 ≈ 0.076. The
       values fed to `ProfileToneCurve` use that same scale, but Adobe's curve may expect ACR's
       scale (its own default exposure and BaselineExposure). The offset could also differ per
       profile.
     - **Double offset.** Whether `tone::camera_offset` (−0.47 for Panasonic) should still apply
       on top of the profile's `BaselineExposureOffset`.
     - **LookTable.** Whether its value axis is encoded as the code assumes (`look_encoding`).
     - **Curve LUT range.** Whether the spline LUT covers the curve's full input range.
   - Look at the strips in `target/release/renders/profiles/` first.
3. **The "vs LR default" rows in the bench aren't a Lightroom comparison for these profiles.** The
   crops in `target/tonefit/crops/` are Lightroom renders with *Adobe Color*. A real check needs
   the user to export JPEGs from Lightroom with Camera Natural and with Camera Vivid at default
   sliders (OM-5 II: PA150285, P6140518, P5280801; S1: P1020780, P1018088, P1018129). The bench
   then needs a small change to compare against those exports. The Lightroom sidecars that use
   these profiles are in `C:\Users\Andy\Pictures\Lightroom Imports\`; treat that folder as
   read-only too.

## Review (2026-10-19)

- **Against the DNG spec**:
  - The reader and the tables look right. The hue shift is converted from degrees, and the table
    order (value, then hue, then saturation) matches the reference. The sRGB value encoding only
    picks where the table is read, and the value scale works in the encoded space, as the
    reference does.
  - Illuminant interpolation is linear in inverse temperature (mireds).
- **Timing**: fixed. `bench profiles` now times the whole develop the way `bench masks` does.
  At 4K Fit on the RTX 5060 Ti:

  | | PA150285 (OM) | P1018129 (S1) |
  |---|---|---|
  | Default | 0.35 ms | 0.34 ms |
  | Adobe Standard | 0.63 ms | 0.66 ms |
  | Camera Natural / Vivid | 0.58 ms | 0.67 ms |
  | Natural + 1 mask | 0.70 ms | 0.78 ms |

  A profile roughly doubles the pass. It's acceptable, but there's room to make it faster: fold
  the two HSV tables and the curve into one 3D LUT texture and let the hardware sample it.
- **Default bit-identical**: confirmed. The 12 `bench masks` hashes (PA150285 and P1020780)
  equal those of a build of 4ceded4.
- **Why Natural and Vivid are off**: `bench profiles` now prints the signed brightness
  difference (render minus camera JPEG, stops) at p10, p50 and p90. Every profile render is
  *darker*:

  | Photo | Natural (p50) | Vivid (p50) |
  |---|---|---|
  | OM | −0.43 | −0.42 |
  | S1 | −0.33 | −0.8 to −1.0 |

  - **The tone curves are not at fault.** Adobe's OM-5 II Natural and Vivid curves are almost
    identical to our base curve: both send 0.076 to 0.18, so our input scale is ACR's. Adobe
    Color is Adobe Standard plus a look table and a slight S point-curve, with no exposure
    offset, so our base look is ACR's default.
  - **On the OM-5 II the offset explains it.** Its Camera profiles carry
    `BaselineExposureOffset = −0.32`, and their look tables leave grey unchanged. So we render
    0.32 stops darker than Adobe Color, almost exactly the error. Whether Lightroom applies that
    offset net, or cancels it against the camera's own baseline exposure in its camera
    database (a value we don't have, which our fitted `camera_offset` stands in for), can only
    be settled against Lightroom.
  - **On the S1 the cause is different.** Its profiles have no offset, but Adobe's Vivid curve
    is about 0.5 stops darker than ours at middle grey, and Standard about 0.3. That fits ACR
    feeding these curves a brighter input than we do for Panasonic: our Panasonic
    `camera_offset` of −0.47 was fitted to Adobe Color and may not belong under a camera
    profile.
  - **The camera JPEGs are a weak reference.** We don't know which mode each was shot in (the
    `crs:CameraProfile` in the Lightroom sidecars is the user's later choice), and the cameras
    add their own processing.
  - **Needed to settle it**: Lightroom exports of a few photos at reset settings (Exposure 0),
    once each with Adobe Color, Camera Natural and Camera Vivid. Then fit one exposure term per
    camera for "Camera" profiles, or confirm the rule for applying `BaselineExposureOffset`.

- **App** (driven on scratch copies, both themes):
  - The drop-down lists Windedit, then the camera's profiles, then Adobe Standard, correctly per
    camera.
  - Hover previews and Esc reverts. A click commits, with undo; the sidecar gets
    `windedit:Profile` and loses it again at the default.
  - Edited previews regenerate.
  - Three UI bugs were fixed in `MainWindow.Edit.cs` and `MainWindow.xaml.cs`: clicking an entry
    never committed, Esc also left the loupe, and the default entry was blank. For the blank
    entry, `we_profile_name(0)` now returns "Windedit".
  - Keyboard selection in the open list is untested.

## Not done (before the review)

- App: not built since the changes and not run. Build it with
  `dotnet build app/Windedit -c Release -r win-x64` and copy over `windedit_core.dll` if it is
  stale. Drive it on **scratch copies**, e.g. under
  `%USERPROFILE%\.gemini\antigravity\brain\deb79ab7-…\scratch\photos`, never the library. Check:
  - the drop-down's contents and order;
  - hover preview, and that it reverts when the drop-down closes without a selection;
  - that the sidecar is written;
  - that edited previews regenerate when the profile changes;
  - screenshots in the light and dark themes.
- The drop-down order the design asks for is Windedit, then Camera Matching, then Adobe Standard.
  Check that it does this.
- Windedit's own profile folders in the registry.
- A real 4K timing for the profile kernels (problem 1).

## Prompt for the next session

> You're reviewing and finishing step 1 of Windedit's camera profiles milestone at
> `C:\dev\windedit`. Read `AGENTS.md`, `docs/handoff.md`, `docs/camera-profiles.md` and then
> `docs/camera-profiles-status.md`, which describes the uncommitted work from the previous session
> and its known problems. Review the diff (`git diff`, plus the new `core/src/profile.rs` and
> `tools/bench/src/profiles.rs`) against the DNG spec's camera profile chapter. Fix the 4K timing
> in `bench profiles` and measure the profile path honestly. Find out why Camera Natural and
> especially Camera Vivid (S1) render about 0.3–0.8 stops off the camera JPEGs. Keep the default
> profile bit-identical (`bench masks` hashes). Then build and drive the app on scratch copies and
> take screenshots in both themes. Never write into `C:\Users\Andy\Pictures\2026` or
> `Lightroom Imports`. Don't commit until I say so.
