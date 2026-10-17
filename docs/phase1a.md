# Milestone 1a: the engine for culling

What [the Phase 1 plan](phase1-plan.md) asked of 1a, and what was built and measured.

## What was built

| Piece | Where | What it does |
|---|---|---|
| Preview loader | `core/src/preview.rs`, `jpeg.rs` | reads only the raw's first 1 MB to find its JPEGs and orientation, picks the cheapest JPEG for the job, and decodes it with WIC at about the size needed |
| Folder scan | `core/src/library.rs` | a folder's photos, with RAW+JPEG pairs as one photo |
| Jobs | `core/src/jobs.rs`, `sys.rs` | decode workers on all cores but one, a priority list rebuilt on every move, background work at EcoQoS, caches sized from installed memory |
| Viewer | `core/src/viewer.rs` | the GPU thread: draws the best layer that's ready, swaps in better ones as they arrive, allocates GPU memory on a helper thread |
| Engine | `core/src/engine.rs`, `ffi.rs` | the API apps use: open a folder, show, zoom, edit mode. Calls return at once and every frame drawn is reported back |
| Test window | `app/Windedit` | ←/→/Home/End through the folder, click for 100%, E for Edit mode, F12 saves what the engine drew |
| `bench cull` | `tools/bench` | the whole engine, drawing offscreen at 2560×1440: a cold start, stepping, holding a key, 100% checks |

### Three preview tiers

Every photo can be shown three ways, and the jobs fill them in order:

| Tier | Source | Desktop cost |
|---|---|---|
| Quick | the smallest usable JPEG in the raw (1920 px for the Lumix bodies, 3200 px for the OM-5 II) | 6–18 ms |
| Loupe | the cheapest JPEG that covers the view, decoded at about view size | 10–76 ms |
| Full | the biggest JPEG at full size, for 100% | 26–95 ms |

A bigger decode fills a smaller tier only if that tier is still empty, so Cull mode keeps
drawing the right-sized preview. The raw is decoded in the background too, for Edit mode and
for 100% on cameras whose JPEG is smaller than the sensor.

## Results on the desktop

i7-14700F (27 decode threads), RTX 5060 Ti. Files in the OS cache; a 2560×1440 view. Times
are from asking for a photo until a frame of it is drawn.

| | Lumix S1 | OM-5 Mark II | Lumix S9 |
|---|---|---|---|
| Next photo, 2 a second | 5.8 ms | 6.3 ms | 5.7 ms |
| Next photo, 5 a second | 4.5 ms | 6.3 ms | 5.8 ms |
| Holding the arrow key (30 a second) | 4.5 ms, every photo | 5.7 ms, every photo | 4.4 ms, every photo |
| 100% check after a second on the photo | 0.9 ms (21 before the follow-up below) | 0.7 ms (20) | 0.7 ms (21) |
| Raw decoded before moving on, at 5 a second | 39 of 39 | 39 of 39 | 39 of 39 |
| Cold start straight into Edit: first preview / raw | 19 / 155 ms | 37 / 404 ms | 19 / 231 ms |

Against the plan's targets (on the laptop, still to measure): next preview under 50 ms and
keeping up with a held key, met with room to spare here; 100% check under 100 ms, met; first
photo without waiting for allocation, met when the photo size is known from the last session
(the app remembers sensor sizes and allocates them at startup) or once prefetching has seen a
raw of that size. Only a cold start straight into Edit on a never-seen size still pays for both
the raw decode and the allocation.

## Results on the laptop

Core Ultra 7 256V (7 decode threads), Arc 140V, 16 GB. A folder of 293 photos.

| | Laptop | Target |
|---|---|---|
| Next photo, 2 a second | 11.5 ms, 95% within 24.3 ms | < 50 ms |
| Next photo, 5 a second | 10.5 ms, 95% within 23.2 ms | < 50 ms |
| Holding the arrow key (30 a second) | every photo, 10.0 ms, 95% within 22.0 ms | keeps up |
| 100% check after a second on the photo | 47.6 ms, **95% within 119 ms** | < 100 ms |
| Raw decoded before moving on, at 5 a second | 37 of 39 | |
| Cold start straight into Edit: first preview / raw | 49 / 595 ms (raw open 172 ms) | |

Everything meets its target except the slowest 100% checks. The likely cause: the viewer kept
one preview texture and reused it only for a preview of the same size, so zooming swapped the
loupe-size texture for a full-size one (about 100 MB) and back, allocating each time, which
costs 50–170 ms on this laptop.

**Follow-up (done, awaiting a laptop re-run):** the viewer now keeps a texture per tier (quick,
loupe, full), each reused for the next photo of the same size, and uploads the full-size
preview once a photo has been on screen, fitted, for 150 ms with nothing else happening (so
never while moving through photos). On the desktop, 100% checks went from about 21 ms to
0.7–0.9 ms, with stepping unchanged.

The cold start's 172 ms raw open is the GPU memory allocation: the benchmark starts with no
remembered sizes, unlike the app from its second launch.

## Things found along the way

- **High-quality camera JPEGs barely benefit from scaled decoding.** The S1's 9.7 MB camera
  JPEG takes 72 ms at half size against 90 ms at full size, because most of the time is spent
  unpacking the compressed data, which scaling can't skip. That's why previews come in tiers:
  the quick one is up in 6–18 ms and the expensive ones are decoded ahead in the background. A
  faster decoder could split the work across cores, since these JPEGs have restart markers.
- **Lumix S9 raws contain a stub JPEG inside their first preview** whose header claims
  6000×4000 but which holds almost no data. A plain scan took it for a full-size image and drew
  it half grey. The scan now skips over each JPEG's data, which leaves the real full-size one.
- **Screenshots taken with GDI can show near-black pixels as white specks.** Windows' screen
  capture records some of them with alpha 0, and image viewers then draw them transparent. It's
  not on the screen. Save captures without alpha, or use F12 to save what the engine drew.

## Measuring on the laptop

From a terminal in the copied `artifacts\windedit-phase0` folder:

```
.\bench.exe cull "<a folder of raws>"
```

The laptop has 8 cores, so the job system will run 7 decode threads instead of 27; the held-key
and stepping numbers show whether previews still keep up.
