# Milestone 2d: card import

Copying a camera card into the library, the way the user already files photos:
`Pictures\2026\2026-06-06\P1020943.RW2`. Culling can start the moment the card is in, from
the card's embedded JPEGs, while the copy runs.

## Detecting a card

- A **removable drive with a `DCIM` folder** (DriveInfo, DriveType.Removable, polled every
  2 s while the window is open; cheap). Also accept fixed drives with `DCIM` for card readers
  that report themselves as fixed, but only at the top level of the drive.
- When one appears, an **Import** entry appears at the top of the Folders panel: the volume
  label and letter ("OM SYSTEM (E:)"), the photo count, and how many are new (not already in
  the library). It goes when the card is removed.
- For testing: a command-line switch (or environment variable) `--fake-card <folder>` makes a
  folder act as an inserted card. **Tests and manual runs only ever use a scratch copy as the
  card and a scratch folder as the destination; nothing is ever written to the real
  `Pictures\2026`.**

## Viewing the card

- Selecting the Import entry opens the card as a folder: all photos under `DCIM\*\` (every
  subfolder, e.g. `100OMSYS`, `101_PANA`), in capture-time order. `library::scan_folders`
  takes several directories.
- Embedded JPEGs show at once (the cull view already uses them); raws decode as usual if the
  user opens Edit, but editing waits until the photo is copied (Edit is disabled on card photos
  not yet copied, with a tooltip).
- **Stars, labels and picks made on the card** are saved to the photo's **destination**
  sidecar, never to the card: `Ratings::load` takes explicit sidecar paths, and for a card
  those are the planned destination paths. The sidecar can be written before the raw arrives;
  the copier never overwrites an existing sidecar at the destination.

## Copying

- Starts when the user clicks **Import** on the entry (one click; the defaults below are
  remembered). A progress bar on the entry: files, megabytes, time left.
- **Destination**: `<root>\<yyyy>\<yyyy-MM-dd>\<file name>`, root `Pictures` by default (so
  `Pictures\2026\2026-10-18`), the date from EXIF DateTimeOriginal (exif.rs gains it, for both
  ORF and RW2, and for the JPEG when there's no raw), falling back to the file's modified time.
  Raw, JPEG and any sidecar of a photo go together.
- **New only**: a photo whose destination file already exists with the same size is skipped
  (that's how "new" is counted). A name clash with a different size gets `-1`, `-2` added.
- **Backup**: an optional second root (off by default); each file is copied to both.
- **Safety**: copy to `name.tmp` and rename on success; compare sizes after copying (and read
  back a hash of the copy when "Verify" is on, off by default); never delete or change
  anything on the card. A failed file is reported by name and the rest carry on.
- **Speed**: one copy thread per destination, large buffered reads (4 MB), the card read once
  per file even with a backup (write both from the same buffer). The card is usually the
  bottleneck; the copy shouldn't make browsing the card stutter (decode workers stay at
  normal priority, the copier lower).
- **Settings**: a small flyout from a gear on the Import entry: destination root, backup root
  and on/off, Verify. Saved with the app's other settings.
- When done: "Imported 312 photos to 3 folders", and the dated folders appear in the Folders
  panel's tree; selecting one opens it as usual. Optionally (setting, off) eject the card.

## Where the code goes

- core/src/import.rs: planning (which files, their dates and destinations, which are new),
  the copier on its own thread with progress and cancel, tests on a fake card in a temp
  folder (including name clashes, already-imported photos, a missing date, a failed copy).
- core/src/exif.rs: DateTimeOriginal.
- core/src/library.rs: `scan_folders`.
- core/src/ratings.rs: explicit sidecar paths.
- FFI and Engine.cs: plan, start, progress (through the existing event callback, as export's
  progress is), cancel.
- app: MainWindow.Import.cs (the entry, detection, the flyout); the Folders panel only gains a
  slot at its top.
