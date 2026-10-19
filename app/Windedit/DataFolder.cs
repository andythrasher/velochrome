namespace Windedit;

/// <summary>
/// Where the app keeps its settings, preview cache and crash log (%LOCALAPPDATA%\Velochrome), and
/// where custom camera profiles go (%APPDATA%\Velochrome\Profiles, which the engine scans). The app
/// was called Windedit before: its folders are copied across the first time.
/// </summary>
static class DataFolder
{
    public static readonly string Local = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Velochrome");

    static readonly string Roaming = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "Velochrome");

    /// <summary>
    /// Copies the Windedit folders to the Velochrome ones if the latter don't exist yet; the old ones are
    /// left alone. In an MSIX package writes to these folders go to the package's private copy, but the
    /// real Windedit folder can still be read, so this works there too. The preview cache can be large, so
    /// it is copied on a background thread; the engine copes with a cache that fills up as it runs.
    /// </summary>
    public static void MigrateFromWindedit()
    {
        try
        {
            Migrate(Local, Path.Combine(Path.GetDirectoryName(Local)!, "Windedit"), skip: "Previews", background: "Previews");
            Migrate(Roaming, Path.Combine(Path.GetDirectoryName(Roaming)!, "Windedit"));
        }
        catch
        {
            // Starting fresh is acceptable; failing to start is not.
        }
    }

    static void Migrate(string to, string from, string? skip = null, string? background = null)
    {
        if (Directory.Exists(to) || !Directory.Exists(from))
            return;
        CopyTree(from, to, skip);
        if (background != null && Directory.Exists(Path.Combine(from, background)))
        {
            var thread = new Thread(() =>
            {
                try { CopyTree(Path.Combine(from, background), Path.Combine(to, background), null); }
                catch { }
            })
            { IsBackground = true, Priority = ThreadPriority.BelowNormal };
            thread.Start();
        }
    }

    // A file that can't be copied is left behind (the app starts fresh for it); the reason goes in migration.log.
    static void Log(string folder, string text)
    {
        try { File.AppendAllText(Path.Combine(folder, "migration.log"), text + Environment.NewLine); }
        catch { }
    }

    static void CopyTree(string from, string to, string? skip)
    {
        Directory.CreateDirectory(to);
        foreach (var file in Directory.EnumerateFiles(from))
        {
            var target = Path.Combine(to, Path.GetFileName(file));
            if (!File.Exists(target))
            {
                // Opened for sharing, so a file another running copy of the app is writing can still be read.
                try
                {
                    using var source = new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
                    using var copy = new FileStream(target, FileMode.CreateNew, FileAccess.Write);
                    source.CopyTo(copy);
                }
                catch (Exception e)
                {
                    Log(to, $"{Path.GetFileName(file)}: {e.GetType().Name} {e.Message}");
                }
            }
        }
        foreach (var dir in Directory.EnumerateDirectories(from))
        {
            if (skip != null && Path.GetFileName(dir).Equals(skip, StringComparison.OrdinalIgnoreCase))
                continue;
            CopyTree(dir, Path.Combine(to, Path.GetFileName(dir)), null);
        }
    }
}
