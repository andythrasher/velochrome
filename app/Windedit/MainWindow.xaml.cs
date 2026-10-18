using System.Runtime.InteropServices.WindowsRuntime;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media.Imaging;
using Windows.Storage.Pickers;
using Windows.System;
using Windows.UI.Core;

namespace Windedit;

/// <summary>
/// Culling: a folder's photos in a loupe with a filmstrip, or a grid; stars, pick flags and color
/// labels with Lightroom's keys, saved to XMP sidecars by the engine; filters. The engine decodes
/// and draws on its own threads; this window sends requests and shows what comes back.
/// </summary>
public sealed partial class MainWindow : Window
{
    static readonly string[] LayerNames = ["", "quick preview", "loupe preview", "full-size preview", "raw"];
    static readonly string[] LabelNames = ["", "Red", "Yellow", "Green", "Blue", "Purple", "label"];
    static readonly string AppData = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Windedit");
    // Sensor sizes seen, so the next launch can allocate GPU memory for them before it's needed.
    static readonly string SizesFile = Path.Combine(AppData, "sizes.txt");
    static readonly string RecentFile = Path.Combine(AppData, "recent.txt");
    // Which panels and Edit groups are open, as name=1 or name=0 lines.
    static readonly string LayoutFile = Path.Combine(AppData, "layout.txt");
    // Filmstrip cells fit a 3:2 thumbnail at the strip's height; the same spacing as the XAML layouts.
    const double FilmstripCellWidth = 133, CellSpacing = 6;

    readonly Engine engine;
    readonly Microsoft.UI.Dispatching.DispatcherQueue ui;
    readonly List<(uint, uint)> sizes = [];
    readonly byte[] thumbnailBuffer = new byte[Engine.ThumbnailBytes];

    string? folder;
    /// <summary>Every photo in the folder, by its engine index.</summary>
    PhotoItem[] all = [];
    /// <summary>The photos the filters let through, in order.</summary>
    List<PhotoItem> shown = [];
    /// <summary>Position of the current photo in <see cref="shown"/>, or -1.</summary>
    int position = -1;
    PhotoItem? CurrentPhoto => position >= 0 && position < shown.Count ? shown[position] : null;

    bool gridMode, actualSize, editMode, foldersOpen;
    readonly Dictionary<string, bool> layout = [];
    Engine.Frame last;
    /// <summary>How many repeaters each photo is shown in (filmstrip, grid): its thumbnail is
    /// kept while it's on screen somewhere.</summary>
    readonly Dictionary<int, int> realized = [];
    bool visibleUpdatePending, bringIntoViewPending;

    public MainWindow(string? startPath, string? fakeCard = null, string? fakeDest = null)
    {
        InitializeComponent();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1700, 1100));
        ui = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        engine = new Engine();
        engine.FrameDrawn += e => ui.TryEnqueue(() => OnEngineEvent(e));
        Closed += (_, _) => engine.Dispose();
        // Every key goes through here, even ones a control already used, so the culling keys work
        // wherever focus is (except while choosing from a list).
        Root.AddHandler(UIElement.KeyDownEvent, new KeyEventHandler(Root_KeyDown), true);
        Root.Loaded += (_, _) => FocusKeys();
        FlagFilter.SelectedIndex = RatingFilter.SelectedIndex = LabelFilter.SelectedIndex = 0;
        LoupeToggle.IsChecked = true;
        PrepareRememberedSizes();
        LoadLayout();
        BuildEditPanel();
        InitCrop();
        InitExport();
        InitAppearance();
        BuildFolderTree();
        InitImport(fakeCard, fakeDest);
        SetFoldersPanel(layout.GetValueOrDefault("folders", true));
        EmptyText.Text = $"Open a folder of photos (Ctrl+O).\n\n{engine.AdapterName}";
        TimingText.Text = $"GPU warm-up {engine.WarmUpMs:F1} ms";

        if (startPath != null)
        {
            if (Directory.Exists(startPath))
                OpenFolder(startPath, null);
            else if (File.Exists(startPath))
                OpenFolder(Path.GetDirectoryName(startPath)!, startPath);
        }
    }

    // ---- Opening ----

    async void PickFolder()
    {
        var picker = new FolderPicker { SuggestedStartLocation = PickerLocationId.PicturesLibrary };
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var picked = await picker.PickSingleFolderAsync();
        if (picked != null)
            OpenFolder(picked.Path, null);
    }

    /// <summary>Opens a folder and shows <paramref name="photoPath"/> in it, or its first photo.</summary>
    void OpenFolder(string path, string? photoPath)
    {
        int count;
        try
        {
            count = engine.OpenFolder(path);
        }
        catch (Exception ex)
        {
            EmptyText.Text = $"Couldn't open {path}: {ex.Message}";
            return;
        }
        ShowOpened(count, path, photoPath, isCard: false);
    }

    /// <summary>Shows the photos the engine has just opened: a folder, or a camera card (which is not remembered, and whose photos can't be edited until they're copied).</summary>
    void ShowOpened(int count, string path, string? photoPath, bool isCard)
    {
        folder = path;
        SetCardOpen(isCard);
        ExitCrop(cancel: false);
        SetPicking(false);
        realized.Clear();
        var metadata = engine.GetMetadata(count);
        all = new PhotoItem[count];
        for (var i = 0; i < count; i++)
            all[i] = new PhotoItem(i, engine.PhotoName(i)) { Metadata = metadata[i] };
        if (!isCard)
            RememberRecent(path);
        Title = $"{(isCard ? cardLabel : Path.GetFileName(path))} – Windedit";
        position = -1;
        ApplyFilters(photoPath != null ? engine.FindPhoto(photoPath) : -1);
    }

    // ---- Filters ----

    void Filter_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (all.Length > 0)
            ApplyFilters(CurrentPhoto?.Index ?? -1);
        FocusKeys();
    }

    bool Passes(Engine.Metadata m)
    {
        var flag = FlagFilter.SelectedIndex switch
        {
            1 => m.Pick != -1,
            2 => m.Pick == 1,
            3 => m.Pick == -1,
            _ => true,
        };
        var label = LabelFilter.SelectedIndex <= 0 || m.Label == LabelFilter.SelectedIndex;
        return flag && m.Rating >= Math.Max(0, RatingFilter.SelectedIndex) && label;
    }

    /// <summary>
    /// Rebuilds the shown list and tells the engine the new order (so it prefetches the photos
    /// that are next in it). Keeps <paramref name="keep"/> current if it still passes, else the
    /// next photo that does.
    /// </summary>
    void ApplyFilters(int keep)
    {
        shown = all.Where(p => Passes(p.Metadata)).ToList();
        engine.SetOrder(shown.Select(p => p.Index).ToArray());
        realized.Clear();
        Filmstrip.ItemsSource = shown;
        GridRepeater.ItemsSource = shown;

        foreach (var p in all)
            p.IsCurrent = false;
        position = -1;
        if (shown.Count > 0)
        {
            var at = shown.FindIndex(p => p.Index >= Math.Max(0, keep));
            ShowAt(at >= 0 ? at : shown.Count - 1);
        }
        UpdateEmptyText();
        UpdateCounts();
    }

    void UpdateEmptyText()
    {
        EmptyText.Visibility = shown.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        if (all.Length > 0 && shown.Count == 0)
            EmptyText.Text = "No photos match these filters.";
        else if (all.Length == 0 && folder != null)
            EmptyText.Text = "There are no photos in this folder.";
    }

    void UpdateCounts()
    {
        var picks = all.Count(p => p.Metadata.Pick == 1);
        var rejects = all.Count(p => p.Metadata.Pick == -1);
        CountText.Text = shown.Count == all.Length
            ? $"{all.Length} photos · {picks} picks · {rejects} rejects"
            : $"{shown.Count} of {all.Length} shown · {picks} picks · {rejects} rejects";
    }

    // ---- Moving ----

    void ShowAt(int newPosition)
    {
        if (newPosition < 0 || newPosition >= shown.Count)
            return;
        ExitCrop(cancel: false);
        SetPicking(false);
        CommitEdit(); // a slider change still being dragged belongs to the photo it started on
        if (CurrentPhoto is { } old)
            old.IsCurrent = false;
        position = newPosition;
        var item = shown[position];
        item.IsCurrent = true;
        if (actualSize)
        {
            actualSize = false;
            engine.ZoomToFit();
        }
        engine.Show(item.Index);
        LoadEdit();
        ScheduleBringIntoView();
        UpdatePhotoText();
    }

    /// <summary>
    /// Scrolls the current photo into view (centered), once per batch of moves. The cells are a
    /// fixed size, so where a photo is follows from its position: no element has to be realized
    /// or laid out first (forcing that on every keypress, while thumbnails arrive, can crash XAML).
    /// </summary>
    void ScheduleBringIntoView()
    {
        if (bringIntoViewPending)
            return;
        bringIntoViewPending = true;
        ui.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, BringIntoView);
    }

    void BringIntoView()
    {
        bringIntoViewPending = false;
        if (position < 0)
            return;
        var scroller = gridMode ? GridScroller : FilmstripScroller;
        // Not laid out yet (just shown, or new photos): try again after the next frame.
        if (scroller.ViewportWidth == 0 || (gridMode ? GridRepeater : Filmstrip).ActualWidth == 0)
        {
            bringIntoViewPending = true;
            AfterFrame(BringIntoView);
            return;
        }
        if (gridMode)
        {
            // Every grid cell is the same size; the rows share the repeater's height.
            var columns = GridColumns();
            var rows = (shown.Count + columns - 1) / columns;
            var pitch = (GridRepeater.ActualHeight + CellSpacing) / rows;
            var center = GridScroller.Padding.Top + position / columns * pitch + (pitch - CellSpacing) / 2;
            GridScroller.ChangeView(null, center - GridScroller.ViewportHeight / 2, null, disableAnimation: true);
        }
        else
        {
            var center = FilmstripScroller.Padding.Left + position * (FilmstripCellWidth + CellSpacing) + FilmstripCellWidth / 2;
            FilmstripScroller.ChangeView(center - FilmstripScroller.ViewportWidth / 2, null, null, disableAnimation: true);
        }
    }

    static void AfterFrame(Action action)
    {
        EventHandler<object>? tick = null;
        tick = (_, _) =>
        {
            Microsoft.UI.Xaml.Media.CompositionTarget.Rendering -= tick;
            action();
        };
        Microsoft.UI.Xaml.Media.CompositionTarget.Rendering += tick;
    }

    void Scroller_SizeChanged(object sender, SizeChangedEventArgs e) => ScheduleBringIntoView();

    void UpdatePhotoText()
    {
        if (CurrentPhoto is not { } p)
        {
            PhotoText.Text = "";
            return;
        }
        var m = p.Metadata;
        var parts = new List<string>
        {
            $"{p.Name}  ({position + 1} of {shown.Count})",
            new string('★', m.Rating) + new string('☆', 5 - m.Rating),
        };
        if (m.Pick != 0)
            parts.Add(m.Pick == 1 ? "Picked" : "Rejected");
        if (m.Label != 0)
            parts.Add(LabelNames[m.Label]);
        if (actualSize)
            parts.Add("100%");
        PhotoText.Text = string.Join("   ", parts);
    }

    // ---- Rating ----

    void Rate(Action<int> apply, bool advance)
    {
        if (CurrentPhoto is not { } p)
            return;
        apply(p.Index);
        p.Metadata = engine.GetMetadata(all.Length)[p.Index];
        UpdatePhotoText();
        UpdateCounts();
        if (advance)
            ShowAt(position + 1);
    }

    /// <summary>Lightroom's auto-advance: with Caps Lock on, or Shift held, rating moves on.</summary>
    static bool AutoAdvance()
    {
        var caps = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.CapitalLock);
        var shift = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift);
        return caps.HasFlag(CoreVirtualKeyStates.Locked) || shift.HasFlag(CoreVirtualKeyStates.Down);
    }

    // ---- Keys ----

    void Root_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        var focused = FocusManager.GetFocusedElement(Content.XamlRoot);
        // The panels' keys work wherever focus is. Tab is ours too: it doesn't move focus.
        var alt = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Menu).HasFlag(CoreVirtualKeyStates.Down);
        if (!alt && e.Key is VirtualKey.F7 or VirtualKey.F8 or VirtualKey.Tab)
        {
            switch (e.Key)
            {
                case VirtualKey.F7: SetFoldersPanel(!foldersOpen); break;
                case VirtualKey.F8: SetEditMode(!editMode); break;
                default:
                    var both = !foldersOpen || !editMode;
                    SetFoldersPanel(both);
                    SetEditMode(both);
                    break;
            }
            e.Handled = true;
            return;
        }
        // Leave keys alone while a filter list or the folder search has focus.
        if (focused is ComboBox or TextBox)
            return;
        // A focused slider keeps the keys that move it.
        if (focused is Slider && e.Key is VirtualKey.Left or VirtualKey.Right or VirtualKey.Up or VirtualKey.Down
            or VirtualKey.Home or VirtualKey.End or VirtualKey.PageUp or VirtualKey.PageDown)
            return;
        var ctrl = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control).HasFlag(CoreVirtualKeyStates.Down);
        var shift = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(CoreVirtualKeyStates.Down);
        if (all.Length > 0 && (PickerShortcut(e.Key, ctrl) || CropShortcut(e.Key, ctrl)))
        {
            e.Handled = true;
            return;
        }
        if (ctrl && shift && e.Key == VirtualKey.E)
        {
            ShowExportFlyout();
            e.Handled = true;
            return;
        }
        if (ctrl && all.Length > 0 && EditShortcut(new VirtualKeyWithModifiers(e.Key, shift)))
        {
            e.Handled = true;
            return;
        }
        if (ctrl && e.Key == VirtualKey.O)
        {
            PickFolder();
            e.Handled = true;
            return;
        }
        if (ctrl || all.Length == 0)
            return;

        var advance = AutoAdvance();
        var key = (int)e.Key;
        switch (e.Key)
        {
            case VirtualKey.Right: ShowAt(position + 1); break;
            case VirtualKey.Left: ShowAt(position - 1); break;
            case VirtualKey.Down when gridMode: ShowAt(Math.Min(position + GridColumns(), shown.Count - 1)); break;
            case VirtualKey.Up when gridMode: ShowAt(Math.Max(position - GridColumns(), 0)); break;
            case VirtualKey.Home: ShowAt(0); break;
            case VirtualKey.End: ShowAt(shown.Count - 1); break;
            case VirtualKey.PageDown when gridMode: ShowAt(Math.Min(position + GridColumns() * GridRows(), shown.Count - 1)); break;
            case VirtualKey.PageUp when gridMode: ShowAt(Math.Max(position - GridColumns() * GridRows(), 0)); break;

            case >= VirtualKey.Number0 and <= VirtualKey.Number5:
                Rate(i => engine.SetRating(i, key - (int)VirtualKey.Number0), advance);
                break;
            case >= VirtualKey.NumberPad0 and <= VirtualKey.NumberPad5:
                Rate(i => engine.SetRating(i, key - (int)VirtualKey.NumberPad0), advance);
                break;
            case >= VirtualKey.Number6 and <= VirtualKey.Number9:
                // The same label again clears it, as in Lightroom.
                var label = key - (int)VirtualKey.Number6 + 1;
                Rate(i => engine.SetLabel(i, all[i].Metadata.Label == label ? 0 : label), advance);
                break;
            case VirtualKey.P: Rate(i => engine.SetPick(i, 1), advance); break;
            case VirtualKey.X: Rate(i => engine.SetPick(i, -1), advance); break;
            case VirtualKey.U: Rate(i => engine.SetPick(i, 0), advance); break;

            case VirtualKey.G: SetGridMode(true); break;
            case VirtualKey.E: SetGridMode(false); break;
            case VirtualKey.Enter when gridMode: SetGridMode(false); break;
            case VirtualKey.Escape when actualSize: ToggleZoom(0.5, 0.5); break;
            case VirtualKey.Escape when !gridMode: SetGridMode(true); break;
            case VirtualKey.D: SetEditMode(!editMode); break;
            case VirtualKey.Z:
            case VirtualKey.Space when !gridMode:
                ToggleZoom(0.5, 0.5);
                break;
            case VirtualKey.F12:
                // Diagnostic: what the engine drew, to compare with the screen.
                engine.SaveSnapshot(Path.Combine(Path.GetTempPath(), "windedit-snapshot.png"));
                break;
            default:
                return;
        }
        e.Handled = true;
    }

    int GridColumns()
    {
        var width = GridScroller.ViewportWidth - 20;
        return Math.Max(1, (int)((width + 6) / (190 + 6)));
    }

    int GridRows() => Math.Max(1, (int)(GridScroller.ViewportHeight / (150 + 6)));

    /// <summary>Keeps keyboard focus off the filter lists, so arrow keys move through photos.</summary>
    void FocusKeys() => KeyFocus.Focus(FocusState.Programmatic);

    // ---- Views ----

    void Loupe_Click(object sender, RoutedEventArgs e) => SetGridMode(false);

    void Grid_Click(object sender, RoutedEventArgs e) => SetGridMode(true);

    void SetGridMode(bool on)
    {
        if (on)
        {
            ExitCrop(cancel: false);
            SetPicking(false);
        }
        gridMode = on;
        LoupeToggle.IsChecked = !on;
        GridToggle.IsChecked = on;
        GridScroller.Visibility = on ? Visibility.Visible : Visibility.Collapsed;
        Panel.Visibility = on ? Visibility.Collapsed : Visibility.Visible;
        FilmstripScroller.Visibility = on ? Visibility.Collapsed : Visibility.Visible;
        EditPanel.Visibility = editMode && !on ? Visibility.Visible : Visibility.Collapsed;
        ScheduleBringIntoView();
        FocusKeys();
    }

    void Edit_Click(object sender, RoutedEventArgs e)
    {
        SetEditMode(EditToggle.IsChecked == true);
        FocusKeys();
    }

    /// <summary>Opening the Edit panel is what shows the developed raw.</summary>
    void SetEditMode(bool on)
    {
        if (on && cardOpen)
        {
            EditToggle.IsChecked = false; // the raws are still on the card: copy them first
            return;
        }
        if (!on)
        {
            ExitCrop(cancel: false);
            SetPicking(false);
        }
        editMode = on;
        EditToggle.IsChecked = on;
        engine.SetEditMode(on);
        EditPanel.Visibility = on && !gridMode ? Visibility.Visible : Visibility.Collapsed;
        if (on)
            ShowEdit();
    }

    void Folders_Click(object sender, RoutedEventArgs e)
    {
        SetFoldersPanel(FoldersToggle.IsChecked == true);
        FocusKeys();
    }

    void SetFoldersPanel(bool on)
    {
        foldersOpen = on;
        FoldersToggle.IsChecked = on;
        FoldersPanel.Visibility = on ? Visibility.Visible : Visibility.Collapsed;
        SetLayout("folders", on);
        if (!on)
            FocusKeys(); // focus inside the hidden panel would swallow the keys
    }

    void Thumbnail_Tapped(object sender, TappedRoutedEventArgs e)
    {
        // The repeaters don't set DataContext (the template uses x:Bind), so ask them for the position.
        var element = (UIElement)sender;
        var at = gridMode ? GridRepeater.GetElementIndex(element) : Filmstrip.GetElementIndex(element);
        if (at != position)
            ShowAt(at);
        FocusKeys();
    }

    void Thumbnail_DoubleTapped(object sender, DoubleTappedRoutedEventArgs e)
    {
        if (gridMode)
            SetGridMode(false);
    }

    // ---- Zoom ----

    void Panel_PointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (CurrentPhoto == null || last.Width == 0)
            return;
        // Where the click landed on the fitted photo, as fractions of its size.
        var p = e.GetCurrentPoint(Panel).Position;
        double w = Panel.ActualWidth, h = Panel.ActualHeight;
        var fit = Math.Min(w / last.Width, h / last.Height);
        ToggleZoom((p.X - (w - last.Width * fit) / 2) / (last.Width * fit), (p.Y - (h - last.Height * fit) / 2) / (last.Height * fit));
        FocusKeys();
    }

    void ToggleZoom(double x, double y)
    {
        if (CurrentPhoto == null || gridMode)
            return;
        actualSize = !actualSize;
        if (actualSize)
            engine.ZoomToActual((float)Math.Clamp(x, 0, 1), (float)Math.Clamp(y, 0, 1));
        else
            engine.ZoomToFit();
        UpdatePhotoText();
    }

    void Panel_SizeChanged(object sender, SizeChangedEventArgs e) => ResizeView();

    void Panel_CompositionScaleChanged(SwapChainPanel sender, object args) => ResizeView();

    void ResizeView()
    {
        if (Panel.ActualWidth < 1 || Panel.ActualHeight < 1)
            return;
        var scale = Panel.CompositionScaleX;
        engine.ResizeView(Panel, (uint)Math.Round(Panel.ActualWidth * scale), (uint)Math.Round(Panel.ActualHeight * scale), scale);
    }

    // ---- Thumbnails ----

    void Repeater_ElementPrepared(ItemsRepeater sender, ItemsRepeaterElementPreparedEventArgs args)
    {
        if (args.Index >= shown.Count)
            return;
        var item = shown[args.Index];
        if (sender == Filmstrip)
            ((FrameworkElement)args.Element).Width = FilmstripCellWidth;
        realized[item.Index] = realized.GetValueOrDefault(item.Index) + 1;
        if (item.Thumbnail == null)
            LoadThumbnail(item);
        ScheduleVisibleUpdate();
    }

    void Repeater_ElementClearing(ItemsRepeater sender, ItemsRepeaterElementClearingEventArgs args)
    {
        if ((args.Element as FrameworkElement)?.DataContext is not PhotoItem item)
            return;
        var count = realized.GetValueOrDefault(item.Index) - 1;
        if (count <= 0)
        {
            realized.Remove(item.Index);
            item.Thumbnail = null; // the engine keeps the pixels; this frees the bitmap
        }
        else
        {
            realized[item.Index] = count;
        }
        ScheduleVisibleUpdate();
    }

    void Scroller_ViewChanged(object? sender, ScrollViewerViewChangedEventArgs e) => ScheduleVisibleUpdate();

    /// <summary>Tells the engine which thumbnails are on screen, once per batch of changes.</summary>
    void ScheduleVisibleUpdate()
    {
        if (visibleUpdatePending)
            return;
        visibleUpdatePending = true;
        ui.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, () =>
        {
            visibleUpdatePending = false;
            var center = CurrentPhoto?.Index ?? 0;
            var visible = realized.Keys.Where(i => all[i].Thumbnail == null)
                .OrderBy(i => Math.Abs(i - center)).ToArray();
            engine.SetVisible(visible);
        });
    }

    void LoadThumbnail(PhotoItem item)
    {
        if (engine.GetThumbnail(item.Index, thumbnailBuffer, out var w, out var h) != 1)
            return;
        var bitmap = new WriteableBitmap(w, h);
        using (var stream = bitmap.PixelBuffer.AsStream())
            stream.Write(thumbnailBuffer, 0, w * h * 4);
        bitmap.Invalidate();
        item.Thumbnail = bitmap;
    }

    // ---- Engine events ----

    void OnEngineEvent(Engine.Frame e)
    {
        if (e.Photo < 0 || e.Photo >= all.Length)
            return;
        if (e.Kind == Engine.ThumbnailEvent)
        {
            if (realized.ContainsKey(e.Photo))
                LoadThumbnail(all[e.Photo]);
            return;
        }
        last = e;
        if (cropping && e.Photo == CurrentPhoto?.Index)
            UpdateCropOverlay();
        // The as-shot white balance is known once the raw is decoded: show it.
        if (e.Layer == 4 && e.Photo == CurrentPhoto?.Index && !editPending)
        {
            if (edit.CustomWhiteBalance == 0)
                ShowEdit();
            else
                UpdateLensSwitches();
        }
        RememberSize(e.RawWidth, e.RawHeight);
        if (e.Photo != CurrentPhoto?.Index)
            return;
        var open = e.OpenMs > 0 ? $", raw opened in {e.OpenMs:F0} ms" : "";
        TimingText.Text = $"{LayerNames[e.Layer]} {e.SinceShowMs:F0} ms after asking{open} · GPU {e.GpuMs:F2} ms";
    }

    // ---- Remembered things ----

    void PrepareRememberedSizes()
    {
        foreach (var line in ReadLines(SizesFile))
        {
            var parts = line.Split(' ');
            if (parts.Length == 2 && uint.TryParse(parts[0], out var w) && uint.TryParse(parts[1], out var h))
            {
                sizes.Add((w, h));
                engine.Prepare(w, h);
            }
        }
    }

    /// <summary>Keeps the two most recent sensor sizes (the engine keeps memory for two).</summary>
    void RememberSize(uint width, uint height)
    {
        if (width == 0 || (sizes.Count > 0 && sizes[0] == (width, height)))
            return;
        sizes.Remove((width, height));
        sizes.Insert(0, (width, height));
        if (sizes.Count > 2)
            sizes.RemoveAt(2);
        WriteLines(SizesFile, sizes.Select(s => $"{s.Item1} {s.Item2}"));
    }

    void RememberRecent(string path)
    {
        var recent = ReadLines(RecentFile).Where(p => !string.Equals(p, path, StringComparison.OrdinalIgnoreCase)).Take(9).ToList();
        recent.Insert(0, path);
        WriteLines(RecentFile, recent);
        _ = RefreshRecent();
    }

    void LoadLayout()
    {
        foreach (var line in ReadLines(LayoutFile))
        {
            var at = line.LastIndexOf('=');
            if (at > 0)
                layout[line[..at]] = line[(at + 1)..] == "1";
        }
    }

    /// <summary>Remembers whether a panel or Edit group is open.</summary>
    void SetLayout(string name, bool open)
    {
        if (layout.TryGetValue(name, out var was) && was == open)
            return;
        layout[name] = open;
        WriteLines(LayoutFile, layout.Select(l => $"{l.Key}={(l.Value ? 1 : 0)}"));
    }

    static List<string> ReadLines(string path)
    {
        try
        {
            return File.ReadAllLines(path).Where(l => l.Length > 0).ToList();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    static void WriteLines(string path, IEnumerable<string> lines)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            File.WriteAllLines(path, lines);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { }
    }
}
