# Camera profiles (design)

The next milestone. A **profile** sets a photo's base look: how the camera's colours map to
the working space, a colour table, and the base tone curve, before the user's sliders. Lightroom
calls them Adobe Color, Camera Natural, Camera Vivid and so on. The user wants the camera makers'
looks first (OM System's especially, Lumix's too), and later film and "manufacturer" looks of
our own (a Leica-style "German red dot", for instance).

## Where profiles come from

- **Not from the raw.** An ORF or RW2 carries the colour matrices (which we already use), the
  picture mode the photo was shot in, and an embedded JPEG rendered in that mode, but not the
  mode's recipe. Adobe's Camera Matching profiles are Adobe's own fits, made by measuring the
  camera's JPEGs against its raws; we can use them on the user's PC but can't ship them.
- **Adobe's DCPs on this PC** (read-only, never shipped):
  `C:\ProgramData\Adobe\CameraRaw\CameraProfiles\Camera\<model>\*.dcp` (Camera Matching, e.g.
  `OM Digital Solutions OM-5 Mark II Camera Natural.dcp`, `Panasonic DC-S1 Camera L Monochrome.dcp`)
  and `...\CameraProfiles\Adobe Standard\<model> Adobe Standard.dcp`. Plain DCP files (magic
  `IIRC`, little-endian TIFF-like tags), format documented in the DNG specification.
- **Our own fitted profiles** (shippable): fitted by tonefit from raw + camera-JPEG pairs (see
  "Fitting our own" below), written as DCP files too, so there's one format and one renderer.
- **Film and creative looks**, later: our own DCPs (or a 3D LUT with an amount slider), designed
  rather than measured.

## The renderer (DNG spec order)

Per the DNG specification's camera-to-display chain, fitted into our develop pass:

1. White balance, then the profile's **ForwardMatrix** (interpolated between its two
   illuminants by the white balance's colour temperature, as the spec says) to XYZ D50, then to
   linear ProPhoto (the space DCP tables are defined in). Profiles without ForwardMatrix use
   ColorMatrix inversion, as the spec describes.
2. **HueSatMap** (3D table in HSV: hue, saturation, and value divisions; with its encoding
   tag, linear or sRGB-gamma value axis), before exposure.
3. Exposure (and the profile's BaselineExposureOffset).
4. **LookTable** (same structure), after exposure.
5. **ProfileToneCurve**, replacing our fitted base curve (`baseCurve` in tone.hlsli) when the
   profile has one, applied the same hue-keeping way (`toneRgb`). Our slider model (Contrast,
   Highlights, Shadows, Whites, Blacks, Clarity) stays on top unchanged: it was fitted against
   Lightroom with Adobe Color, and Lightroom applies the same sliders over any profile.
6. Then everything after tone as now (mixer, presence, sRGB), converting ProPhoto to our
   Rec.2020 working space where the pipeline needs it.

GPU: the tables go in a 3D texture (or a structured buffer, with trilinear interpolation done
by hand to follow the spec's hue wrap-around exactly); the tone curve in a 1D LUT (4096
entries). A photo with the default profile must render bit-identically to now and cost the same
(as with masks: a separate kernel variant if registers suffer). Check against Lightroom: render
a few photos with Camera Natural / Vivid / Monotone in both and compare (the user can export
reference JPEGs from Lightroom; tonefit already compares renders with screenshots).

## Data, storage, UI

- **Registry**: on startup, scan the Adobe folders (if present) and Windedit's own profile
  folders (`<app>\Profiles`, `%APPDATA%\Windedit\Profiles`), reading each DCP's
  `UniqueCameraModel` and `ProfileName`. Match a photo by its camera model (the EXIF make/model
  mapped to Adobe's naming: "OM Digital Solutions OM-5 Mark II", "Panasonic DC-S1"). Parse
  lazily, cache parsed tables.
- **Edit**: a `profile` field that is default-safe when zeroed (0 = Windedit's default look),
  holding an id from the registry; the sidecar stores the profile's **name** (as Lightroom's
  `crs:CameraProfile`, e.g. "Camera Natural", plus our own `windedit:Profile`), so it survives
  a different registry order and reads Lightroom's choice when present.
- **Developed previews**: the fingerprint (`developed.rs`) must include the profile, and
  `RENDER_VERSION` bumps when the renderer's look changes.
- **UI**: a Profile drop-down at the top of the Edit panel's first group, as Lightroom's:
  Windedit default, then Camera Matching (this camera's), then Adobe Standard, then our own
  looks; hovering an entry previews it live (the GPU makes this cheap). Later an Amount slider
  for creative looks.
- **Auto camera match** (step 2): the raw records the picture mode (Olympus maker notes
  PictureMode; Panasonic PhotoStyle). An option to open each photo with the matching profile,
  as Lightroom's "Camera Matching" default does.

## Fitting our own (step 3, shippable)

- tonefit gains `fit-profile`: inputs are raw + camera-JPEG pairs of one camera and picture
  mode; it fits a HueSatMap/LookTable (e.g. 36 hue × 8 saturation × 8 value) and a tone curve so
  our render matches the camera's JPEG, regularised to be smooth, and writes a DCP.
- Best data: the user shoots, per camera and per picture mode, RAW+JPEG of a colour chart
  (ColorChecker) and a few colourful scenes, in daylight and indoor light, with the camera's
  extras off (OM Gradation Normal, Panasonic i.Dynamic off, no art filters). About an hour per
  camera. Fallback: the embedded JPEGs already in the user's library (every raw is a pair), with
  care for the camera's own dynamic-range and lens processing.
- Validate a fit against Adobe's DCP for the same mode (mean ΔE over the chart and the scenes).
- Names: makers' mode names are trademarks; ship descriptive names ("Camera Natural (OM)")
  pending a proper check.

## Order

1. DCP reader and renderer, Adobe's profiles from this PC, the registry, the Edit field, the
   sidecar, the drop-down, previews' fingerprint. Verify against Lightroom.
2. Auto camera match from the maker notes.
3. tonefit `fit-profile` and our own shippable profiles from the user's calibration shots.
4. Creative looks (film, "German red dot"), with an Amount slider.
