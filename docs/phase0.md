# Phase 0: the engine test

The question: can a raw editor built on a Rust engine plus DirectX 12 compute feel instant on
the photos from my cameras? Phase 0 built just enough to find out: decoding, a GPU develop
pipeline with one adjustment (Exposure), and a WinUI window to drag it in.

**Answer: yes, with lots of headroom.** The develop pass costs fractions of a millisecond even
at 4K. The time that matters now is CPU decoding when a photo is opened, and the fixes for that
are known (see *What it found*).

## What was built

| Piece | Where | What it does |
|---|---|---|
| Decode | `core/src/decode.rs` | rawler for the sensor data and color matrices; our own code for orientation and the Olympus preview JPEG, which rawler misses |
| Color | `core/src/color.rs` | camera RGB → linear Rec.2020 working space, from Adobe's published matrices |
| GPU | `core/src/gpu.rs` | Direct3D 12 device, compute kernels, timestamps |
| Pipeline | `core/src/pipeline.rs`, `core/shaders/` | once per photo: upload, demosaic (Malvar-He-Cutler), half-size pyramid. Every slider move: one pass over the *screen* pixels |
| Present | `core/src/present.rs` | composition swap chain for WinUI's SwapChainPanel |
| C interface | `core/src/ffi.rs` | what the Windows app (and later Mac/Linux apps) call |
| Test app | `app/Windedit` | WinUI 3: open, Exposure slider, click for 100%, ←/→ through the folder, live timings |
| Benchmark | `tools/bench` | `bench decode / preview / gpu <files or folders>` |

## Results on the desktop

i7-14700F (20 cores / 28 threads), RTX 5060 Ti, 32 GB. Median of 10 files (decode, preview)
or 60 frames (develop); files already in the OS file cache.

**Opening a photo (CPU)**

| Camera | Sensor | Decode, one file | All cores, many files | Embedded preview |
|---|---|---|---|---|
| OM-5 Mark II (ORF) | 5184×3888 | 231 ms | 61 files/s | 28 ms (3200×2400) |
| Lumix S1 (RW2) | 6000×4000 | 80 ms | 66 files/s | 12 ms (1920×1280) |
| Lumix S9 (RW2) | 6000×4000 | 129 ms | 56 files/s | 9 ms (1920×1280) |

**Getting it onto the GPU (once per photo)**

| | ORF | S1 | S9 |
|---|---|---|---|
| Upload (CPU copy) | 3.8 ms | 4.7 ms | 4.6 ms |
| Copy to video memory | 3.9 ms | 3.8 ms | 4.4 ms |
| Demosaic | 1.4 ms | 2.0 ms | 1.8 ms |
| Pyramid | 1.1 ms | 1.8 ms | 1.4 ms |

**Every slider move (GPU time of the develop pass)**

| View | ORF | S1 | S9 |
|---|---|---|---|
| Fit, 1920×1080 | 0.02 ms | 0.02 ms | 0.02 ms |
| Fit, 2560×1440 | 0.10 ms | 0.15 ms | 0.11 ms |
| Fit, 3840×2160 | 0.14 ms | 0.16 ms | 0.14 ms |
| 100%, 2560×1440 | 0.06 ms | 0.08 ms | 0.05 ms |
| Full size (export) | 0.37 ms | 0.51 ms | 0.42 ms |

In the app, dragging Exposure on the ORF at fit: 0.04 ms develop, 0.6 ms for the whole render
call, and about 4 ms from a slider move to the frame being handed to Windows (mostly waiting for
the next display refresh; Windows' compositor adds up to one more refresh before it's lit).
From pressing → to the next ORF being on screen takes about 300 ms, nearly all of it decoding.

Against the targets set at the start (slider < 16 ms here, < 33 ms on the laptop; next photo
< 50 ms; open-to-edit < 300 ms): the slider target is beaten by a wide margin. Open-to-edit is
right at the line for ORF and well under it for both Lumix bodies. Next-photo needs the preview
and prefetch work below.

## What it found

1. **The per-edit cost is negligible, so the budget can go to quality.** At 0.1 ms per frame
   there is room for 100× more work per slider move before it shows. Real highlight recovery,
   noise reduction, local contrast and masks can run live rather than in the background.
2. **Decoding is the bottleneck, and the OM-5 II's is the slowest.** Olympus's compression is
   one long sequential stream (no restart points), so a single file can't be split across cores.
   The fix is in how the app uses it, not in the decoder:
   - show the embedded JPEG first (12–28 ms), then swap in the real render when it's ready;
   - decode the next and previous few photos in the background on the other cores (60+ files/s
     on this PC), so ←/→ lands on an already-decoded photo.
3. **The first upload is slower than later ones** (25 ms in the app vs 4 ms in the benchmark):
   Windows maps fresh memory in on first touch. Keep a reusable upload buffer, and later decode
   straight into it.
4. **GPU memory per open photo is about 256 MB** (16-bit RGBA, full size plus pyramid). That's
   fine for one or two photos on a 16 GB laptop; the cache policy needs this in mind.
5. **rawler covers all three bodies**, including the OM-5 II and the original S1. Two gaps,
   now filled in our code: it doesn't read orientation from ORF or RW2, and it can't find the
   Olympus preview JPEG (it's in the maker notes; we scan for the largest embedded JPEG).
6. **Your library has a third camera:** about half the RW2 files are from a Lumix S9. It works
   with no extra effort.

## Shortcuts taken (on purpose)

- Demosaic is Malvar-He-Cutler: fast and decent. RCD or AMaZE (from RawTherapee, GPLv3) come
  later; it runs once per photo, so it can afford to be much heavier.
- Highlights are clipped to neutral; no reconstruction yet.
- The tone curve is a fixed filmic placeholder.
- Shaders are plain HLSL compiled by the Windows SDK's dxc. Slang compiles HLSL nearly as-is,
  so moving to it for Mac and Linux later is cheap.
- Every GPU submission waits for completion, which keeps the timings honest but leaves
  overlap on the table.
- No lens corrections, no edits saved anywhere.

## Not yet measured: the laptop

The Core Ultra 7 256V (Arc 140V, shared memory) is the other half of the target. The engine
detects shared memory and skips the copy to video memory there. To measure, copy
`artifacts\windedit-phase0` (made by the publish command in the README) to the laptop, along
with a few raw files, and run:

```
bench.exe decode <folder of raws>
bench.exe gpu <folder of raws>
Windedit.exe <a raw file>
```
