using System.Runtime.InteropServices;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace Windedit;

/// <summary>
/// Importing a camera card. A removable drive with a DCIM folder (found by polling) gets an entry at
/// the top of the Folders panel: click it to browse the card's photos like a folder, and Import
/// to copy the new ones into Pictures\yyyy\yyyy-MM-dd. The engine plans and copies
/// (core/src/import.rs) and reports through <see cref="Engine.ImportChanged"/>; nothing is ever
/// written to the card, and stars given to its photos are saved beside the copies, not on it.
/// </summary>
public sealed partial class MainWindow
{
    // The flyout's choices, as name=value lines.
    static string ImportFile => Path.Combine(AppData, "import.txt");

    string? cardRoot;
    string cardLabel = "Camera card";
    bool cardIsFake, cardIsRemovable;
    int cardTotal, cardNew;
    bool cardScanned;
    /// <summary>The card's photos are what's open (so they can't be edited yet).</summary>
    bool cardOpen;
    bool importing, polling;

    string? fakeCardFolder;
    /// <summary>The destination was named on the command line, so it isn't saved.</summary>
    bool importDestPinned;
    string importDest = DefaultImportRoot();
    string? importBackup;
    bool importBackupOn, importVerify, importEject, updatingImportFlyout;
    Brush? importEntryBorder;

    static string DefaultImportRoot() =>
        string.IsNullOrEmpty(PicturesPath) ? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Pictures") : PicturesPath;

    void InitImport(string? fakeCard, string? fakeDest)
    {
        foreach (var line in ReadLines(ImportFile))
        {
            var at = line.IndexOf('=');
            if (at <= 0)
                continue;
            var (name, value) = (line[..at], line[(at + 1)..]);
            switch (name)
            {
                case "dest" when value.Length > 0: importDest = value; break;
                case "backup" when value.Length > 0: importBackup = value; break;
                case "backupOn": importBackupOn = value == "1"; break;
                case "verify": importVerify = value == "1"; break;
                case "eject": importEject = value == "1"; break;
            }
        }
        // A test card files into a folder of its own unless told otherwise, never the real library.
        if (fakeCard != null)
        {
            fakeCardFolder = Path.GetFullPath(fakeCard).TrimEnd('\\');
            importDest = Path.GetFullPath(fakeDest ?? fakeCardFolder + "-library");
            importDestPinned = true;
        }
        else if (fakeDest != null)
        {
            importDest = Path.GetFullPath(fakeDest);
            importDestPinned = true;
        }
        importEntryBorder = ImportEntry.BorderBrush;
        engine.ImportChanged += e => ui.TryEnqueue(() => OnImportChanged(e));

        var timer = new DispatcherTimer { Interval = TimeSpan.FromSeconds(2) };
        timer.Tick += (_, _) => PollCard();
        timer.Start();
        PollCard();
    }

    void SaveImportSettings()
    {
        var lines = new List<string>();
        if (!importDestPinned)
            lines.Add($"dest={importDest}");
        lines.AddRange([$"backup={importBackup}", $"backupOn={(importBackupOn ? 1 : 0)}", $"verify={(importVerify ? 1 : 0)}", $"eject={(importEject ? 1 : 0)}"]);
        WriteLines(ImportFile, lines);
    }

    // ---- Finding the card ----

    /// <summary>The inserted card: a removable drive with DCIM (or a fixed one, for readers that call themselves that), else the test card.</summary>
    static (string Root, string Label, bool Removable)? FindCard(string? fake)
    {
        if (fake != null)
            return Directory.Exists(Path.Combine(fake, "DCIM")) ? (fake, $"Test card ({Path.GetFileName(fake)})", false) : null;
        var system = Path.GetPathRoot(Environment.SystemDirectory);
        foreach (var d in DriveInfo.GetDrives())
        {
            try
            {
                if (d.DriveType is not (DriveType.Removable or DriveType.Fixed) || !d.IsReady)
                    continue;
                if (d.DriveType == DriveType.Fixed && string.Equals(d.Name, system, StringComparison.OrdinalIgnoreCase))
                    continue;
                if (!Directory.Exists(Path.Combine(d.Name, "DCIM")))
                    continue;
                var letter = d.Name.TrimEnd('\\');
                return (d.Name, string.IsNullOrEmpty(d.VolumeLabel) ? letter : $"{d.VolumeLabel} ({letter})", d.DriveType == DriveType.Removable);
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                // A reader with no card, or one that is going away.
            }
        }
        return null;
    }

    async void PollCard()
    {
        // The copy needs the card where it is, even if Windows loses it for a moment.
        if (polling || importing)
            return;
        polling = true;
        try
        {
            var found = await Task.Run(() => FindCard(fakeCardFolder));
            if (found is not { } card)
            {
                if (cardRoot != null)
                {
                    cardRoot = null;
                    cardScanned = false;
                    ImportEntry.Visibility = Visibility.Collapsed;
                }
                return;
            }
            if (card.Root == cardRoot)
                return;
            (cardRoot, cardLabel, cardIsRemovable) = card;
            cardIsFake = fakeCardFolder != null;
            cardScanned = false;
            ImportTitle.Text = cardLabel;
            ImportMessage.Visibility = Visibility.Collapsed;
            ImportEntry.Visibility = Visibility.Visible;
            ShowCardState();
            await ScanCard();
        }
        finally
        {
            polling = false;
        }
    }

    /// <summary>Counts the card's photos and how many are new. Reads every photo's date, so it runs off the UI thread.</summary>
    async Task ScanCard()
    {
        if (cardRoot is not { } root)
            return;
        var dest = importDest;
        try
        {
            var (total, fresh) = await Task.Run(() => engine.ImportScan(root, dest));
            if (root != cardRoot)
                return;
            (cardTotal, cardNew, cardScanned) = (total, fresh, true);
            // The destination changed under an open card: its photos now belong to other sidecars.
            if (cardOpen && !importing)
                OpenCard();
        }
        catch (Exception ex)
        {
            if (root == cardRoot)
            {
                cardScanned = false;
                ImportCounts.Text = $"Couldn't read the card: {ex.Message}";
            }
            return;
        }
        ShowCardState();
    }

    void ShowCardState()
    {
        ImportSettings.IsEnabled = !importing;
        ImportProgressBox.Visibility = importing ? Visibility.Visible : Visibility.Collapsed;
        ImportGo.Visibility = importing ? Visibility.Collapsed : Visibility.Visible;
        if (!cardScanned)
        {
            ImportCounts.Text = "Reading the card…";
            ImportGo.Content = "Import";
            ImportGo.IsEnabled = false;
            return;
        }
        ImportCounts.Text = cardTotal == 0 ? "No photos" : $"{Plural(cardTotal, "photo")} · {cardNew} new";
        ImportGo.IsEnabled = cardNew > 0;
        ImportGo.Content = cardNew > 0 ? $"Import {Plural(cardNew, "new photo")}" : cardTotal > 0 ? "All imported" : "Nothing to import";
        ToolTipService.SetToolTip(ImportGo, $"Copy to {importDest}");
    }

    static string Plural(int n, string noun) => n == 1 ? $"1 {noun}" : $"{n:N0} {noun}s";

    // ---- Browsing the card ----

    void ImportHeader_Tapped(object sender, TappedRoutedEventArgs e)
    {
        // The gear opens its flyout and nothing else.
        for (var o = e.OriginalSource as DependencyObject; o != null; o = VisualTreeHelper.GetParent(o))
            if (ReferenceEquals(o, ImportSettings))
                return;
        OpenCard();
        FocusKeys();
    }

    /// <summary>Opens the card's photos like a folder, in the order they were shot.</summary>
    void OpenCard()
    {
        if (cardRoot is not { } root || !cardScanned)
            return;
        int count;
        try
        {
            count = engine.ImportOpenCard();
        }
        catch (Exception ex)
        {
            ShowImportMessage($"Couldn't open the card: {ex.Message}");
            return;
        }
        openEntry = null;
        FolderTree.SelectedItem = null;
        ShowOpened(count, root, null, isCard: true);
    }

    /// <summary>Called whenever photos are opened: the card's can't be edited until they're copied.</summary>
    void SetCardOpen(bool open)
    {
        cardOpen = open;
        if (open && editMode)
            SetEditMode(false);
        EditToggle.IsEnabled = !open;
        ToolTipService.SetToolTip(EditToggle, open ? "Import these photos to edit them" : "Edit: the developed raw (D, F8)");
        try
        {
            ImportEntry.BorderBrush = open ? new SolidColorBrush((Windows.UI.Color)Application.Current.Resources["SystemAccentColor"]) : importEntryBorder;
            ImportEntry.BorderThickness = new Thickness(open ? 2 : 1);
        }
        catch (Exception ex) when (ex is KeyNotFoundException or InvalidCastException) { }
    }

    // ---- Copying ----

    void ImportGo_Click(object sender, RoutedEventArgs e)
    {
        if (cardRoot == null || !cardScanned || importing || cardNew == 0)
            return;
        string? backup = null;
        if (importBackupOn)
        {
            if (string.IsNullOrEmpty(importBackup))
            {
                ShowImportMessage("Choose a backup folder in the import settings first.");
                return;
            }
            backup = importBackup;
        }
        try
        {
            engine.ImportStart(backup, importVerify);
        }
        catch (Exception ex)
        {
            ShowImportMessage($"Couldn't start the import: {ex.Message}");
            return;
        }
        importing = true;
        ImportMessage.Visibility = Visibility.Collapsed;
        ImportBar.Value = 0;
        ImportProgressText.Text = "Starting…";
        ImportCancel.IsEnabled = true;
        ImportCancel.Content = "Stop";
        ShowCardState();
        FocusKeys();
    }

    void ImportCancel_Click(object sender, RoutedEventArgs e)
    {
        engine.ImportCancel();
        ImportCancel.IsEnabled = false;
        ImportCancel.Content = "Stopping…";
        FocusKeys();
    }

    static string Megabytes(long bytes) => bytes >= 1L << 30 ? $"{bytes / (double)(1L << 30):F1} GB" : $"{bytes >> 20:N0} MB";

    void OnImportChanged(Engine.ImportUpdate u)
    {
        if (u.State == 0)
        {
            if (!importing)
                return;
            ImportBar.Value = u.BytesTotal > 0 ? (double)u.BytesDone / u.BytesTotal : 1;
            var line = $"{Megabytes(u.BytesDone)} of {Megabytes(u.BytesTotal)}";
            // Not worth guessing in the first second or two.
            if (u.Seconds >= 2 && u.BytesDone > 0)
            {
                var left = TimeSpan.FromSeconds(u.Seconds * (u.BytesTotal - u.BytesDone) / u.BytesDone);
                line += $" · {u.BytesDone / u.Seconds / 1e6:F0} MB/s · " + (left.TotalMinutes >= 1 ? $"{Math.Ceiling(left.TotalMinutes):F0} min left" : $"{Math.Ceiling(left.TotalSeconds):F0} s left");
            }
            ImportProgressText.Text = $"{u.Text}\n{u.FilesDone} of {u.FilesTotal} files · {line}";
            return;
        }

        importing = false;
        var lines = u.Text.Split('\n');
        var failures = lines.Where(l => l.StartsWith('!')).Select(l => l[1..]).ToList();
        var folders = lines.Where(l => l.StartsWith('>')).Select(l => l[1..]).ToList();
        ShowImportMessage($"{lines[0]}.", failures);
        AddImportedFolders(folders);
        ShowCardState();
        _ = ScanCard();
        if (u.State == 1 && failures.Count == 0 && importEject)
            _ = EjectCard();
    }

    void ShowImportMessage(string text, List<string>? details = null)
    {
        ImportMessage.Text = text;
        ToolTipService.SetToolTip(ImportMessage, details is { Count: > 0 } ? string.Join("\n", details) : null);
        ImportMessage.Visibility = Visibility.Visible;
    }

    // ---- New folders in the tree ----

    /// <summary>Makes the folders just filled show in the Folders panel: inside Pictures, in the tree (where it has been read; an unread
    /// branch will include them when it is); elsewhere, in Recent.</summary>
    void AddImportedFolders(List<string> dirs)
    {
        foreach (var dir in dirs)
        {
            var relative = Path.GetRelativePath(PicturesPath, dir);
            if (relative.StartsWith("..") || Path.IsPathRooted(relative))
            {
                RememberRecent(dir);
                continue;
            }
            var node = picturesEntry;
            foreach (var part in relative.Split('\\', StringSplitOptions.RemoveEmptyEntries))
            {
                if (!node.Loaded)
                    break;
                var child = node.Children.FirstOrDefault(c => string.Equals(c.Name, part, StringComparison.OrdinalIgnoreCase));
                if (child == null)
                {
                    child = new FolderEntry(Path.Combine(node.Path, part), part);
                    var at = node.Children.TakeWhile(c => string.Compare(c.Name, part, StringComparison.OrdinalIgnoreCase) < 0).Count();
                    node.Children.Insert(at, child);
                }
                node = child;
            }
        }
    }

    // ---- Settings ----

    void ImportFlyout_Opening(object? sender, object e)
    {
        updatingImportFlyout = true;
        ImportDestPath.Text = Shorten(importDest);
        ToolTipService.SetToolTip(ImportDestPath, importDest);
        ImportBackupOn.IsChecked = importBackupOn;
        ImportBackupPath.Text = string.IsNullOrEmpty(importBackup) ? "No folder chosen" : Shorten(importBackup);
        ImportVerify.IsChecked = importVerify;
        ImportEject.IsChecked = importEject;
        ImportEject.IsEnabled = !cardIsFake;
        updatingImportFlyout = false;
    }

    void ImportFlyout_Closed(object? sender, object e) => FocusKeys();

    async void ImportDest_Click(object sender, RoutedEventArgs e)
    {
        var picked = await PickExportFolder();
        if (picked == null)
            return;
        importDest = picked;
        ImportDestPath.Text = Shorten(picked);
        ToolTipService.SetToolTip(ImportDestPath, picked);
        SaveImportSettings();
        cardScanned = false;
        ShowCardState();
        await ScanCard();
    }

    async void ImportBackupOn_Click(object sender, RoutedEventArgs e)
    {
        if (updatingImportFlyout)
            return;
        importBackupOn = ImportBackupOn.IsChecked == true;
        if (importBackupOn && string.IsNullOrEmpty(importBackup))
            await ChooseBackup();
        SaveImportSettings();
    }

    async void ImportBackup_Click(object sender, RoutedEventArgs e)
    {
        await ChooseBackup();
        SaveImportSettings();
    }

    async Task ChooseBackup()
    {
        var picked = await PickExportFolder();
        if (picked != null)
        {
            importBackup = picked;
            ImportBackupPath.Text = Shorten(picked);
            ToolTipService.SetToolTip(ImportBackupPath, picked);
        }
        // Asked to back up but nowhere chosen: not a backup after all.
        importBackupOn = !string.IsNullOrEmpty(importBackup) && importBackupOn;
        ImportBackupOn.IsChecked = importBackupOn;
    }

    void ImportVerify_Click(object sender, RoutedEventArgs e)
    {
        importVerify = ImportVerify.IsChecked == true;
        SaveImportSettings();
    }

    void ImportEject_Click(object sender, RoutedEventArgs e)
    {
        importEject = ImportEject.IsChecked == true;
        SaveImportSettings();
    }

    // ---- Ejecting ----

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(IntPtr handle, uint code, byte[]? input, uint inputSize, IntPtr output, uint outputSize, out uint returned, IntPtr overlapped);

    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    /// <summary>Asks Windows to eject the card, as Safely Remove does: lock the volume, dismount it, then eject the media.</summary>
    async Task EjectCard()
    {
        if (cardRoot is not { } root || cardIsFake || !cardIsRemovable)
            return;
        var ok = await Task.Run(() =>
        {
            var handle = CreateFileW($@"\\.\{root.TrimEnd('\\')}", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
            if (handle == new IntPtr(-1))
                return false;
            try
            {
                return DeviceIoControl(handle, 0x00090018, null, 0, IntPtr.Zero, 0, out _, IntPtr.Zero) // FSCTL_LOCK_VOLUME
                    && DeviceIoControl(handle, 0x00090020, null, 0, IntPtr.Zero, 0, out _, IntPtr.Zero) // FSCTL_DISMOUNT_VOLUME
                    && DeviceIoControl(handle, 0x002D4804, [0], 1, IntPtr.Zero, 0, out _, IntPtr.Zero) // IOCTL_STORAGE_MEDIA_REMOVAL: allow
                    && DeviceIoControl(handle, 0x002D4808, null, 0, IntPtr.Zero, 0, out _, IntPtr.Zero); // IOCTL_STORAGE_EJECT_MEDIA
            }
            finally
            {
                CloseHandle(handle);
            }
        });
        ShowImportMessage(ok ? $"{ImportMessage.Text} You can take the card out." : $"{ImportMessage.Text} Windows wouldn't eject the card: use Safely Remove.",
            ToolTipService.GetToolTip(ImportMessage) is string t ? t.Split('\n').ToList() : null);
    }
}
