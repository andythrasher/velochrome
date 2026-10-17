# Windedit

A raw photo editor for Windows, built for speed: a Rust engine that develops photos on the GPU,
with a native WinUI app on top. Mac and Linux versions are planned, each with its own native app
on the same engine.

It's at **Phase 1, milestone 1a**: the engine for culling. Previews show at once, raws are
decoded ahead in the background, and the test window moves through a folder. See
[docs/phase0.md](docs/phase0.md) for the engine test, [docs/phase1-plan.md](docs/phase1-plan.md)
for the plan and [docs/phase1a.md](docs/phase1a.md) for what 1a measured.

Cameras tested: OM System OM-5 Mark II, Panasonic Lumix S1 and S9. The decoder (rawler)
knows many more, but they're untested.

## Build and run

Requires the .NET 10 SDK, Rust (stable, MSVC toolchain) and the Windows SDK (for `dxc`, the
shader compiler; Visual Studio Build Tools include it).

```
dotnet build app/Windedit
app/Windedit/bin/Debug/net10.0-windows10.0.26100.0/win-x64/Windedit.exe [a photo]
```

In the window: Ctrl+O opens a photo and its folder, ←/→/Home/End move through it, a click
zooms to 100% there (and back), E switches between Cull mode (the camera's JPEGs) and Edit mode
(the developed raw), and F12 saves the last frame the engine drew to `%TEMP%\windedit-snapshot.png`.

Building the app builds the engine first (`cargo build --release -p windedit-core`).

The benchmark:

```
cargo build --release -p bench
target/release/bench.exe decode|preview|gpu <raw files or folders>
target/release/bench.exe cull <a folder>
```

`gpu` also writes a PNG of each photo's fitted view to `target/release/renders`. `cull` runs
the whole engine the way culling uses it (see [docs/phase1a.md](docs/phase1a.md)). `jpegs`,
`previewpng` and `snapshot` are diagnostics.

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

## License

Windedit is free software under the [GNU General Public License v3.0 or later](LICENSE):
anyone may use, study, change and share it, and anything built from it must be shared under
the same terms.
