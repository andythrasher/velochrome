# Working on Windedit

Notes for AI coding agents (and people) picking up this repository. The current state and the
next steps are in [docs/handoff.md](docs/handoff.md); read it first.

## What it is

A GPL-3.0 raw photo editor for Windows, modelled on Lightroom and built to be much faster than
it: a Rust engine (`core/`, Direct3D 12 compute, HLSL shaders in `core/shaders/`) under a C#
WinUI 3 app (`app/Windedit/`). `core/src/ffi.rs` is the C interface; `app/Windedit/Engine.cs`
mirrors it. Tools: `tools/bench` (benchmarks and checks), `tools/tonefit` (fits the tone model
to Lightroom). Design notes for every milestone are in `docs/`.

## Build, test, run

- `cargo test --release` in `core/` (about 130 tests; all must pass).
- `cargo build --release --workspace` builds the engine and tools.
- The app: `dotnet build app/Windedit -c Release -r win-x64` (output in
  `app/Windedit/bin/Release/net10.0-windows10.0.26100.0/win-x64/`). The app build runs the engine
  build first; if `windedit_core.dll` beside the app is older than `target/release/`'s, copy it.
- The laptop build the user tests with, only when asked (close any Windedit running from there
  first):
  `dotnet publish app/Windedit -c Release -r win-x64 --self-contained -o artifacts/windedit-phase0`,
  then copy `target/release/bench.exe` and `target/release/windedit_core.dll` into it.
- The machine is Windows 11 with PowerShell and Git Bash; there is no Python or ffmpeg.

## Rules

- **Never write anything into `C:\Users\Andy\Pictures\2026`** (the user's library), sidecars
  included. Read from it if needed; test on scratch copies.
- Commit only when the user says so; never push. One commit per feature, messages in the repo's
  style (look at `git log`): a short title, then plain prose on what changed and why.
- Engine `Edit` fields are default-safe when zeroed (stored as offsets from defaults, or "off"
  flags), have a matching C# struct in `Engine.cs`, and are written to the sidecar only when
  not default (`xmp.rs`, namespace `windedit:`).
- A photo with no edits must render exactly as before and just as fast; check new develop-pass
  features with `bench` and keep the default path bit-identical.
- Browsing must never stall: background work runs at low priority and yields to the photo in
  view.
- Match the surrounding code: comments are plain prose saying why, not what; British-leaning
  spelling in prose is fine ("colour") but code names follow what's there.
- Verify UI changes by building and driving the app (UI Automation or screenshots), in both the
  light and dark themes.
