# Contributing to Velochrome

Thanks for helping. The code's internal name is "windedit"; see the [README](README.md).

## Build and test

You need the .NET 10 SDK, Rust (stable, MSVC) and the Windows SDK (for `dxc`).

```
cargo test --release                      # in core/; all tests must pass
cargo build --release --workspace
dotnet build app/Windedit -c Release -r win-x64
```

Tests that read real raw files are skipped unless `VELOCHROME_TEST_RAWS` names a folder of raws.
Test on copies of your photos, never the originals. [AGENTS.md](AGENTS.md) has the working
rules and the architecture in brief, and `docs/` has a design note for each milestone.

## Style

- Match the code around you. Comments are plain prose saying *why*, not *what*.
- Engine `Edit` fields are default-safe when zeroed, have a matching C# struct in
  `app/Windedit/Engine.cs`, and are written to the sidecar only when they differ from the default.
- A photo with no edits must render exactly as before and just as fast; check develop-pass changes
  with `bench`. Browsing must never stall.
- Check UI changes by building and running the app, in both the light and dark themes.

## Pull requests

Keep each change to one feature or fix, with a short title and a plain description of what changed
and why. No sign-off or contributor agreement is required: by contributing you agree your work is
released under the project's GPL-3.0 licence. The name and icon are not part of that licence
([TRADEMARKS.md](TRADEMARKS.md)).
