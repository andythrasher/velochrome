using System.Diagnostics;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Storage.Pickers;

namespace Windedit;

/// <summary>
/// Export: the photo in view as a full-size JPEG. Ctrl+Shift+E (handled with the other keys in
/// <c>Root_KeyDown</c>) or the toolbar button opens a small flyout; the engine renders and
/// writes the file in the background, one export after another, and reports back through
/// <see cref="Engine.ExportChanged"/>.
/// </summary>
public sealed partial class MainWindow
{
    // The flyout's choices, as name=value lines.
    static string ExportFile => Path.Combine(AppData, "export.txt");

    /// <summary>Exports asked for and not finished: job id → photo name.</summary>
    readonly Dictionary<int, string> exporting = [];
    readonly Dictionary<int, float> exportProgress = [];
    bool exportToChosen = true, updatingExportFlyout;
    string? exportChosenFolder = DefaultChosenFolder();
    int exportQuality = 92;
    /// <summary>The last file written, for the Open folder link.</summary>
    string? lastExport;
    Microsoft.UI.Xaml.DispatcherTimer? exportHide;

    void InitExport()
    {
        foreach (var line in ReadLines(ExportFile))
        {
            var at = line.IndexOf('=');
            if (at <= 0)
                continue;
            var (name, value) = (line[..at], line[(at + 1)..]);
            switch (name)
            {
                // Only an explicit "choice" counts: older files named "next to the photos" by default.
                case "choice": exportToChosen = value == "folder"; break;
                case "folder" when value.Length > 0: exportChosenFolder = value; break;
                case "quality" when int.TryParse(value, out var q): exportQuality = Math.Clamp(q, 50, 100); break;
            }
        }
        engine.ExportChanged += e => ui.TryEnqueue(() => OnExportChanged(e));
    }

    void SaveExportChoices() => WriteLines(ExportFile,
    [
        $"choice={(exportToChosen ? "folder" : "photos")}",
        $"folder={exportChosenFolder}",
        $"quality={exportQuality}",
    ]);

    /// <summary>Where exports go unless another folder is chosen: Velochrome Exports in Pictures (which may be redirected).</summary>
    static string DefaultChosenFolder()
    {
        var pictures = Environment.GetFolderPath(Environment.SpecialFolder.MyPictures);
        if (string.IsNullOrEmpty(pictures))
            pictures = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Pictures");
        return Path.Combine(pictures, "Velochrome Exports");
    }

    /// <summary>A long path with its middle left out, so the ends (drive and folder) show.</summary>
    static string Shorten(string path) => path.Length <= 44 ? path : path[..10] + "…" + path[^33..];
    /// <summary>Where this folder's exports go by default: an Export folder inside it.</summary>
    // Never inside a card's folder: nothing is written to the card.
    string DefaultExportFolder() => cardOpen ? DefaultChosenFolder() : Path.Combine(folder ?? "", "Export");

    string ExportFolder() => (exportToChosen || cardOpen) && !string.IsNullOrEmpty(exportChosenFolder) ? exportChosenFolder : DefaultExportFolder();

    void ShowExportFlyout()
    {
        if (all.Length > 0)
            ExportFlyout.ShowAt(ExportButton);
    }

    void ExportFlyout_Opening(object? sender, object e)
    {
        updatingExportFlyout = true;
        ExportTitle.Text = CurrentPhoto is { } p ? $"Export {p.Name}.jpg" : "Export";
        ExportNextTo.IsChecked = !exportToChosen;
        ExportChoose.IsChecked = exportToChosen;
        ExportNextToPath.Text = Shorten(DefaultExportFolder());
        ShowChosenPath();
        ExportQuality.Value = exportQuality;
        ExportQualityText.Text = exportQuality.ToString();
        ExportGo.IsEnabled = CurrentPhoto != null;
        updatingExportFlyout = false;
    }

    void ExportFlyout_Closed(object? sender, object e) => FocusKeys();

    async void ExportWhere_Click(object sender, RoutedEventArgs e)
    {
        if (ReferenceEquals(sender, ExportNextTo))
        {
            exportToChosen = false;
        }
        else
        {
            // Clicking it while it is already the choice asks for another folder.
            if (exportToChosen || exportChosenFolder == null)
            {
                var picked = await PickExportFolder();
                if (picked == null)
                {
                    ExportChoose.IsChecked = exportToChosen; // asking was cancelled: nothing changes
                    ExportNextTo.IsChecked = !exportToChosen;
                    return;
                }
                exportChosenFolder = picked;
                ShowChosenPath();
            }
            exportToChosen = true;
        }
        SaveExportChoices();
    }

    void ShowChosenPath()
    {
        ExportChosenPath.Text = Shorten(exportChosenFolder ?? "");
        ToolTipService.SetToolTip(ExportChosenPath, exportChosenFolder);
    }

    async Task<string?> PickExportFolder()
    {
        var picker = new FolderPicker { SuggestedStartLocation = PickerLocationId.PicturesLibrary };
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        return (await picker.PickSingleFolderAsync())?.Path;
    }

    void ExportQuality_ValueChanged(object sender, Microsoft.UI.Xaml.Controls.Primitives.RangeBaseValueChangedEventArgs e)
    {
        // Also fires while the XAML loads, before the text beside it exists.
        if (updatingExportFlyout || ExportQualityText == null)
            return;
        exportQuality = (int)e.NewValue;
        ExportQualityText.Text = exportQuality.ToString();
        SaveExportChoices();
    }

    void ExportGo_Click(object sender, RoutedEventArgs e)
    {
        ExportFlyout.Hide();
        ExportCurrent();
    }

    /// <summary>Queues the photo in view (in the grid, its current photo) for export.</summary>
    void ExportCurrent()
    {
        if (CurrentPhoto is not { } p || folder == null)
            return;
        CommitEdit(); // a slider still being dragged is part of what's exported
        try
        {
            var id = engine.Export(p.Index, Path.Combine(ExportFolder(), p.Name + ".jpg"), exportQuality);
            exporting[id] = p.Name;
            exportProgress[id] = 0;
            ShowExportStatus();
        }
        catch (Exception ex)
        {
            ShowExportMessage($"Couldn't export {p.Name}: {ex.Message}", folderPath: null);
        }
    }

    void OnExportChanged(Engine.ExportUpdate u)
    {
        switch (u.State)
        {
            case Engine.ExportWorking:
                if (exporting.ContainsKey(u.Id))
                {
                    exportProgress[u.Id] = u.Progress;
                    ShowExportStatus();
                }
                break;
            case Engine.ExportDone:
                exporting.Remove(u.Id);
                exportProgress.Remove(u.Id);
                lastExport = u.Text;
                if (exporting.Count > 0)
                    ShowExportStatus();
                else
                    ShowExportMessage($"Exported {Path.GetFileName(u.Text)}  ({u.TotalMs / 1000:F1} s)", Path.GetDirectoryName(u.Text));
                break;
            default:
                var name = exporting.GetValueOrDefault(u.Id, "the photo");
                exporting.Remove(u.Id);
                exportProgress.Remove(u.Id);
                ShowExportMessage($"Couldn't export {name}: {u.Text}", folderPath: null);
                break;
        }
    }

    /// <summary>The status line while exports are under way: the oldest one's progress, and how many wait.</summary>
    void ShowExportStatus()
    {
        exportHide?.Stop();
        var id = exporting.Keys.Min();
        var waiting = exporting.Count - 1;
        ExportProgress.Visibility = Visibility.Visible;
        ExportProgress.Value = exportProgress.GetValueOrDefault(id);
        ExportText.Text = $"Exporting {exporting[id]}…" + (waiting > 0 ? $"  (+{waiting} waiting)" : "");
        ExportOpen.Visibility = Visibility.Collapsed;
        ExportStatus.Visibility = Visibility.Visible;
        KeysText.Visibility = Visibility.Collapsed;
    }

    /// <summary>A result in the status line for a few seconds, with a link to the folder when there is one.</summary>
    void ShowExportMessage(string text, string? folderPath)
    {
        ExportProgress.Visibility = Visibility.Collapsed;
        ExportText.Text = text;
        ExportOpen.Visibility = folderPath == null ? Visibility.Collapsed : Visibility.Visible;
        ExportStatus.Visibility = Visibility.Visible;
        KeysText.Visibility = Visibility.Collapsed;
        exportHide ??= new Microsoft.UI.Xaml.DispatcherTimer { Interval = TimeSpan.FromSeconds(12) };
        exportHide.Tick -= HideExportStatus;
        exportHide.Tick += HideExportStatus;
        exportHide.Stop();
        exportHide.Start();
    }

    void HideExportStatus(object? sender, object e)
    {
        exportHide?.Stop();
        if (exporting.Count > 0)
            return;
        ExportStatus.Visibility = Visibility.Collapsed;
        KeysText.Visibility = Visibility.Visible;
    }

    void ExportOpen_Click(object sender, RoutedEventArgs e)
    {
        if (lastExport == null)
            return;
        // Explorer with the new file selected.
        Process.Start(new ProcessStartInfo("explorer.exe", $"/select,\"{lastExport}\"") { UseShellExecute = true });
        FocusKeys();
    }
}
