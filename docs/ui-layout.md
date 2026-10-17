# The window's layout

How the main window is arranged after Phase 1c, decided with the user on 2026-10-17. The aim is
Lightroom's Develop flow, browsing and tweaking as you go, without its catalog or its modes.

## The shape

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ [▤]  [loupe][grid]   Show [All photos▾] [Any rating▾] [Any label▾]   …  [✎] │  toolbar
├────────────┬───────────────────────────────────────────────┬─────────────────┤
│ 🔍 search  │                                               │ ▾ White balance │
│ Recent     │                                               │ ▾ Tone          │
│  2026-10-15│               loupe or grid                   │ ▾ Presence      │
│ Pictures   │                                               │                 │
│  ▸ 2026    │                                               │                 │
│ This PC    │                                               │                 │
│  ▸ C:      │                                               │                 │
├────────────┴───────────────────────────────────────────────┴─────────────────┤
│ filmstrip                                                                     │
├──────────────────────────────────────────────────────────────────────────────┤
│ status line                                                                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Folders panel (left)

- A toggle at the toolbar's left end opens and closes it. The toggle shows a folder-tree
  glyph and the tooltip "Folders (F7)". The panel replaces the Open folder split button and
  its Recent drop-down. Ctrl+O still opens the system folder picker.
- **Recent** lists the folders opened lately (the existing `recent.txt`).
- **Pictures** shows the user's Pictures library as a tree, and **This PC** shows the drives.
  Children load when a node is expanded, never ahead, so a big drive costs nothing until it's
  opened.
- Clicking a folder opens it, as Open folder does today. The open folder is highlighted.
- **Search** at the top filters the tree to folders whose names contain the text, searching
  under Pictures and Recent. It runs in the background as you type, and only shows matches
  and their parents.
- About 260 pixels wide. Whether it's open is remembered between runs.

## Edit panel (right)

- Edit is a panel, not a mode. A toggle pinned to the toolbar's right end (like Windshot's
  Layers button) opens it; D does the same. While it's open, the loupe shows the developed
  raw and its sliders apply to the photo in view, as you move from photo to photo. With it
  closed, the loupe shows the camera's JPEG for fast culling, as now.
- Its groups (White balance, Tone, Presence) collapse with a click on their header, and
  remember whether they're open. A filter box comes later, once HSL and Detail make the
  panel long.
- In the grid, the panel hides, as now, and comes back with the loupe.

## Keys

- F7 toggles the left panel, F8 the right one, and Tab both (Tab shows both if either is
  hidden, otherwise hides both). These are Lightroom's keys.
- D toggles the Edit panel; G, E, Z and the rating keys are unchanged.

## Later

- **Import:** when a card is inserted, an Import entry at the top of the Folders panel copies
  it into dated folders (`2026-10-15`), with an optional second copy for backup. Culling
  the card's embedded JPEGs works while the copy runs.
- **Searching photos by name** across folders, and later a small cache rebuilt from the
  sidecars, for filters across folders such as "every 5-star photo this year".
