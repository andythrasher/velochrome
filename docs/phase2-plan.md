# Phase 2 plan (draft)

What comes after Phase 1 (culling, basic adjustments, crop and export), as agreed so far with
the user. The order is a proposal; each milestone gets its own design note before it's built,
as Phase 1's did.

## 2a. Lens panel and white balance picker

- **Lens panel**, a new group in the Edit panel:
  - **Vignetting**, a manual slider (−100 to +100) with a midpoint control, as in Lightroom's
    Manual lens corrections. It's a radial gain on scene-linear light in the develop pass, in
    the same lens-corrected radius as the distortion, before tone. Automatic correction was
    measured and left out (docs/lens.md): our render already matches the camera JPEG's corners,
    and a per-photo match bought too little for its decode time.
  - **Distortion and chromatic aberration** stay automatic from the raw's own data, with a
    switch to turn them off (Lightroom forces distortion on for Micro Four Thirds; we can allow
    off, say for checking).
- **White balance picker**: an eyedropper (W) that sets temperature and tint from a clicked
  area, averaged over a few pixels in scene-linear light.

## 2b. Detail

- **Sharpening** (amount, radius, detail, masking), and **noise reduction**, luminance and
  color, as a classic GPU denoiser first.
- **Texture and Clarity**: local contrast at fine and medium scales. Clarity shares the
  edge-preserving base layer the tone model already builds.
- These work at full resolution, so the 100% view and export are what they're tuned against;
  the fitted view uses a cheaper approximation that matches its look.

## 2c. Color

- **HSL / Color Mixer**: hue, saturation and luminance for eight color ranges.
- Later, color grading (shadows, midtones, highlights).

## 2d. Local edits and import

- **Masks**: linear and radial gradients and a brush, each with its own set of the basic
  sliders.
- **Card import**: when a camera card is inserted, an Import entry in the Folders panel copies
  it into dated folders, with an optional second copy for backup, while its embedded JPEGs
  can already be culled (docs/ui-layout.md).

## Later

- AI denoise and AI masks (subject, sky), on ONNX models through DirectML, for both GPUs.
- An Edit panel filter box, once HSL and Detail make the panel long.
- Cropped and edited thumbnails; reading Lightroom's `crs:Crop*`; crop-handle cursors.
- "Remove from Recent" in the Folders panel.
- Signing the app (Smart App Control blocks unsigned builds copied to other PCs).
