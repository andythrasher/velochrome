# Velochrome

A fast raw photo editor for Windows. A Rust engine develops photos on the GPU (Direct3D 12) under
a native WinUI 3 app, with Lightroom's keys and layout so culling and editing feel familiar, and
with the aim of being much quicker than it: photos open, scroll and respond to sliders without
waiting.

> The code's internal name is **windedit**. You'll see it in the source: the `Windedit` C#
> namespace and `app/Windedit` folder, the `windedit-core` crate and `windedit_core.dll`, the `we_`
> prefix of the engine's C interface, and the repository folder. The product, the window, the
> installed program and what is written to your files are all Velochrome.

Velochrome collects nothing and never connects to the internet: see [PRIVACY.md](PRIVACY.md).

## What it does

- **Culling:** open a folder; move through it in a loupe, a filmstrip or a grid; rate, flag and
  colour-label with Lightroom's keys. Ratings, flags and labels live in standard XMP sidecars
  (`photo.xmp`) that Lightroom, Bridge and Photo Mechanic also read.
- **Import from a card:** copy into dated folders, optionally to a backup too, and cull while it
  copies.
- **Develop, on the GPU:** white balance with a picker; exposure, contrast, highlights, shadows,
  whites, blacks, vibrance and saturation (a tone model fitted to Lightroom's); texture and
  clarity; sharpening and noise reduction; the HSL colour mixer; lens correction from the data
  in the raw file itself, plus manual vignetting; crop, straighten and rotate; local adjustments
  with linear, radial and brush masks, each with its own sliders.
- **Camera profiles (beta):** Camera Matching and Adobe Standard profiles, read from Adobe's DCP
  files if Adobe software has installed them. They are marked **Beta** because they currently
  render about 0.3–0.8 stops darker than Lightroom does; that is being worked on.
- **Export** to full-size JPEG with the camera's EXIF.
- **Edited thumbnails and previews** are cached on disk, with an "edited" badge, so a folder of
  edited photos browses as fast as an unedited one.
- Light and dark themes, in several colour sets.

Edits are non-destructive: the raw file is never touched, and adjustments are saved in the
photo's sidecar. See [docs/](docs/) for design notes on every milestone and
[docs/handoff.md](docs/handoff.md) for where the project stands.

## Cameras

Tested: **OM System OM-5 Mark II** (ORF), **Panasonic Lumix S1** and **S9** (RW2). The decoder
([rawler](https://github.com/dnglab/dnglab)) reads many more makes and models, so others may
work, but they are untested; lens correction in particular is only implemented for Olympus and
Panasonic files. Bayer sensors only for now.

## Requirements

Windows 11 and a Direct3D 12 GPU.

## Build and run

You need the .NET 10 SDK, Rust (stable, MSVC toolchain) and the Windows SDK (for `dxc`, the shader
compiler; Visual Studio Build Tools include it).

```
dotnet build app/Windedit -c Release -r win-x64
app/Windedit/bin/Release/net10.0-windows10.0.26100.0/win-x64/Velochrome.exe [a folder or photo]
```

Building the app builds the engine first (`cargo build --release -p windedit-core`).

Tests and tools:

```
cargo test --release            # in core/
cargo build --release --workspace
target/release/bench.exe decode|preview|gpu <raw files or folders>
```

Some engine tests read real raw files; set `VELOCHROME_TEST_RAWS` to a folder of raws (with
`P6060001.ORF` and `P1070381.RW2` in it, or in subfolders) to run them. Without it they are skipped.

`tools/bench` holds benchmarks and checks (see [docs/phase1a.md](docs/phase1a.md)); `tools/tonefit`
fits the tone model to Lightroom.

### Using it

Ctrl+O opens a folder. The keys follow Lightroom: ← → to move, 0–5 for stars, P / X / U to pick,
reject or clear, 6–9 for colour labels (Shift or Caps Lock moves on after rating), Z or a click for
100%, G for the grid, E for the loupe, D for Edit mode (Ctrl+Z and Ctrl+Y undo and redo).

### The Microsoft Store package

`tools/make-assets.ps1` makes the icon and logos from `Velochrome Icon.png`. `tools/package.ps1`
builds the MSIX for the Store (and a test-signed copy for sideloading); see the notes at the top of
that script and of `app/Windedit/Package.appxmanifest`.

### Settings, caches and the old name

Settings, the preview cache and the crash log live in `%LOCALAPPDATA%\Velochrome`, and custom camera
profiles go in `%APPDATA%\Velochrome\Profiles`. The program was called Windedit during development:
on first launch it copies the old `Windedit` folders across (leaving them in place), and sidecars
written under the old name (`windedit:` fields) are still read and are rewritten as `velochrome:`
the next time the photo is saved.

## Layout

- `core/`: the engine (Rust). `src/ffi.rs` is the C interface the app uses; `shaders/` holds the
  GPU kernels.
- `app/Windedit/`: the Windows app (C#, WinUI 3).
- `tools/`: `bench` (benchmarks and checks), `tonefit`, and the packaging scripts.
- `docs/`: plans and findings.

## Licence and trademarks

Velochrome is free software under the [GNU General Public License v3.0](LICENSE): anyone may use,
study, change and share it, and anything built from it must be shared under the same terms. Third-party
components and their licences are listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

The **name "Velochrome" and the Velochrome icon are trademarks of the author and are not covered by
the GPL**: a fork or modified version must use a different name and icon. See
[TRADEMARKS.md](TRADEMARKS.md).

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).
