# Windedit

A raw photo editor for Windows, built for speed: a Rust engine that develops photos on the GPU,
with a native WinUI app on top. Mac and Linux versions are planned, each with its own native app
on the same engine.

It's at **Phase 1, milestone 1c**: culling and basic adjustments. Open a folder, move through it in
a loupe or a grid, rate, flag and label photos with Lightroom's keys, and adjust white balance,
tone and color in Edit mode; it all goes into XMP sidecars. See
[docs/phase1-plan.md](docs/phase1-plan.md) for the plan, and [docs/phase0.md](docs/phase0.md),
[docs/phase1a.md](docs/phase1a.md), [docs/phase1b.md](docs/phase1b.md) and
[docs/phase1c.md](docs/phase1c.md) for what each step built and measured.

Cameras tested: OM System OM-5 Mark II, Panasonic Lumix S1 and S9. The decoder (rawler)
knows many more, but they're untested.

## Build and run

Requires the .NET 10 SDK, Rust (stable, MSVC toolchain) and the Windows SDK (for `dxc`, the
shader compiler; Visual Studio Build Tools include it).

```
dotnet build app/Windedit
app/Windedit/bin/Debug/net10.0-windows10.0.26100.0/win-x64/Windedit.exe [a folder or photo]
```

In the window: Ctrl+O opens a folder. The keys follow Lightroom: ← → to move, 0–5 for stars,
P / X / U to pick, reject or clear, 6–9 for color labels (Shift or Caps Lock moves on after
rating), Z or a click for 100%, G for the grid, E for the loupe, D for Edit mode (the developed
raw, with the adjustment sliders; Ctrl+Z and Ctrl+Y undo and redo). Ratings, flags, labels and
adjustments are saved to each photo's `.xmp` sidecar. F12 saves the last frame the engine drew to `%TEMP%\windedit-snapshot.png`.

Building the app builds the engine first (`cargo build --release -p windedit-core`).

The benchmark:

```
cargo build --release -p bench
target/release/bench.exe decode|preview|gpu <raw files or folders>
target/release/bench.exe cull <a folder>
```

`gpu` also writes a PNG of each photo's fitted view to `target/release/renders`. `cull` runs
the whole engine the way culling uses it (see [docs/phase1a.md](docs/phase1a.md)); `thumbs <a
folder>` times thumbnails for a grid; `xmpcheck <folder of .xmp>` checks that sidecars would
update cleanly, writing nothing; `look <a raw>` renders it with each slider pushed in turn.
`jpegs`, `previewpng` and `snapshot` are diagnostics.

To run it on another PC (no .NET or Rust needed there):

```
dotnet publish app/Windedit -c Release -r win-x64 --self-contained -o artifacts/windedit-phase0
```

then copy `target/release/bench.exe` into that folder too.

## Layout

- `core/`: the engine (Rust). `src/ffi.rs` is the C interface the apps use; `shaders/` holds
  the GPU kernels.
- `app/Windedit/`: the Windows app (C#, WinUI 3).
- `tools/bench/`: the benchmark.
- `docs/`: plans and findings.

Set `WINDEDIT_D3D_DEBUG=1` to turn on the Direct3D debug layer (needs Windows' Graphics Tools).
`WINDEDIT_NO_ECO=1` runs background decoding at normal priority instead of EcoQoS, for comparing.

## License

Windedit is free software under the [GNU General Public License v3.0 or later](LICENSE):
anyone may use, study, change and share it, and anything built from it must be shared under
the same terms.
