using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Windedit;

/// <summary>A node in the Folders panel: a folder, or one of the headings above them.</summary>
public sealed class FolderEntry(string path, string name, bool isGroup = false)
{
    public string Path { get; } = path;
    public string Name { get; } = name;
    public bool IsGroup { get; } = isGroup;

    /// <summary>What the tree shows for it (the tree can't bind to this class), carrying the entry in its Tag.</summary>
    public UIElement Visual()
    {
        var text = new TextBlock { Text = Name, TextTrimming = TextTrimming.CharacterEllipsis };
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Tag = this };
        if (IsGroup)
            text.FontWeight = Microsoft.UI.Text.FontWeights.SemiBold;
        else
            row.Children.Add(new FontIcon { Glyph = ((char)0xE8B7).ToString(), FontSize = 14, Opacity = 0.8 });
        row.Children.Add(text);
        if (Path != "")
            ToolTipService.SetToolTip(row, Path);
        return row;
    }
}

/// <summary>
/// The Folders panel: Recent, Pictures and This PC as trees whose children load when a node is
/// opened, and a search over Pictures and Recent. Folders are read on a background thread.
/// </summary>
public sealed partial class MainWindow
{
    const int MaxSearchResults = 300;

    static readonly string PicturesPath = Environment.GetFolderPath(Environment.SpecialFolder.MyPictures);
    static readonly EnumerationOptions Readable = new()
    {
        IgnoreInaccessible = true,
        AttributesToSkip = FileAttributes.Hidden | FileAttributes.System,
    };

    TreeViewNode recentNode = null!;
    /// <summary>The node highlighted for the open folder (in Recent, unless it was opened from the tree).</summary>
    TreeViewNode? openNode;
    CancellationTokenSource? searchCancel;

    /// <summary>A folder in a search result, with the matching folders and parents below it.</summary>
    sealed class Hit(string path, string name)
    {
        public string Path = path, Name = name;
        public readonly List<Hit> Children = [];
    }

    void BuildFolderTree()
    {
        recentNode = Node(new FolderEntry("", "Recent", isGroup: true), expanded: true);
        var pictures = Node(new FolderEntry(PicturesPath, "Pictures", isGroup: true), expanded: true);
        var thisPc = Node(new FolderEntry("", "This PC", isGroup: true));
        pictures.HasUnrealizedChildren = thisPc.HasUnrealizedChildren = true;
        FolderTree.RootNodes.Add(recentNode);
        FolderTree.RootNodes.Add(pictures);
        FolderTree.RootNodes.Add(thisPc);
        LoadChildren(pictures);
        _ = RefreshRecent();
    }

    static TreeViewNode Node(FolderEntry entry, bool expanded = false) =>
        new() { Content = entry.Visual(), IsExpanded = expanded };

    static FolderEntry? Entry(TreeViewNode? node) => (node?.Content as FrameworkElement)?.Tag as FolderEntry;

    // ---- Opening branches ----

    void FolderTree_Expanding(TreeView sender, TreeViewExpandingEventArgs args)
    {
        if (args.Node.HasUnrealizedChildren)
            LoadChildren(args.Node);
    }

    void LoadChildren(TreeViewNode node)
    {
        node.HasUnrealizedChildren = false;
        var entry = Entry(node)!;
        var isDrives = entry.IsGroup && entry.Path == "";
        Task.Run(() => isDrives ? ReadDrives() : ReadFolders(entry.Path)).ContinueWith(t =>
            ui.TryEnqueue(() =>
            {
                foreach (var child in t.Result)
                    node.Children.Add(new TreeViewNode { Content = child.Visual(), HasUnrealizedChildren = true });
            }));
    }

    static List<FolderEntry> ReadFolders(string path)
    {
        try
        {
            return Directory.EnumerateDirectories(path, "*", Readable)
                .Select(d => new FolderEntry(d, Path.GetFileName(d)))
                .OrderBy(f => f.Name, StringComparer.OrdinalIgnoreCase)
                .ToList();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    static List<FolderEntry> ReadDrives()
    {
        try
        {
            return DriveInfo.GetDrives().Where(d => d.DriveType != DriveType.NoRootDirectory && d.IsReady)
                .Select(d => new FolderEntry(d.RootDirectory.FullName,
                    string.IsNullOrEmpty(d.VolumeLabel) ? d.Name.TrimEnd('\\') : $"{d.VolumeLabel} ({d.Name.TrimEnd('\\')})"))
                .ToList();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    void FolderTree_ItemInvoked(TreeView sender, TreeViewItemInvokedEventArgs args)
    {
        if (args.InvokedItem is not TreeViewNode node || Entry(node) is not { } entry)
            return;
        if (entry.IsGroup)
        {
            // A heading opens and closes; the highlight stays on the open folder.
            node.IsExpanded = !node.IsExpanded;
            sender.SelectedNode = sender == FolderTree && openNode is { Parent: not null } ? openNode : null;
        }
        else
        {
            if (sender == FolderTree)
                openNode = node;
            OpenFolder(entry.Path, null);
        }
        FocusKeys();
    }

    // ---- Recent ----

    /// <summary>Fills the Recent branch from recent.txt, then highlights the open folder.</summary>
    async Task RefreshRecent()
    {
        var found = await Task.Run(() => ReadLines(RecentFile).Where(Directory.Exists).ToList());
        recentNode.Children.Clear();
        foreach (var path in found)
            recentNode.Children.Add(Node(new FolderEntry(path, Path.GetFileName(path.TrimEnd('\\')))));
        recentNode.IsExpanded = true;
        HighlightOpenFolder();
    }

    /// <summary>Keeps the highlight on the open folder: where it is if that's it, else its place
    /// in Recent (the folder just opened is first there).</summary>
    void HighlightOpenFolder()
    {
        if (folder == null)
            return;
        bool Open(TreeViewNode? n) => n is { Parent: not null }
            && string.Equals(Entry(n)?.Path, folder, StringComparison.OrdinalIgnoreCase);
        if (!Open(openNode))
            openNode = recentNode.Children.FirstOrDefault(Open);
        FolderTree.SelectedNode = openNode;
    }

    // ---- Search ----

    async void FolderSearch_TextChanged(object sender, TextChangedEventArgs e)
    {
        searchCancel?.Cancel();
        var text = FolderSearch.Text.Trim();
        if (text.Length == 0)
        {
            SearchTree.Visibility = Visibility.Collapsed;
            FolderTree.Visibility = Visibility.Visible;
            SearchStatus.Text = "";
            return;
        }
        var cancel = searchCancel = new CancellationTokenSource();
        FolderTree.Visibility = SearchTree.Visibility = Visibility.Collapsed;
        SearchStatus.Text = "Searching…";
        List<Hit> found;
        try
        {
            // A short wait, so typing doesn't start a search per letter.
            await Task.Delay(200, cancel.Token);
            var recent = ReadLines(RecentFile);
            found = await Task.Run(() => Search(text, recent, cancel.Token), cancel.Token);
        }
        catch (OperationCanceledException)
        {
            return;
        }
        SearchTree.RootNodes.Clear();
        foreach (var group in found)
            SearchTree.RootNodes.Add(ToNode(group, isGroup: true));
        SearchTree.Visibility = Visibility.Visible;
        FolderTree.Visibility = Visibility.Collapsed;
        SearchStatus.Text = found.Count == 0 ? "No folders match." : "";
    }

    /// <summary>The Recent and Pictures folders whose names contain <paramref name="text"/>, each
    /// under the folders that lead to it. Only branches with a match are returned.</summary>
    static List<Hit> Search(string text, List<string> recent, CancellationToken cancel)
    {
        bool Matches(string name) => name.Contains(text, StringComparison.OrdinalIgnoreCase);
        var groups = new List<Hit>();

        var recentGroup = new Hit("", "Recent");
        foreach (var path in recent.Where(p => Matches(Path.GetFileName(p.TrimEnd('\\')))))
            recentGroup.Children.Add(new Hit(path, Path.GetFileName(path.TrimEnd('\\'))));
        if (recentGroup.Children.Count > 0)
            groups.Add(recentGroup);

        var root = new Hit(PicturesPath, "Pictures");
        var byPath = new Dictionary<string, Hit>(StringComparer.OrdinalIgnoreCase) { [PicturesPath] = root };
        var count = 0;
        try
        {
            var options = new EnumerationOptions
            {
                IgnoreInaccessible = true,
                AttributesToSkip = Readable.AttributesToSkip,
                RecurseSubdirectories = true,
            };
            foreach (var dir in Directory.EnumerateDirectories(PicturesPath, "*", options))
            {
                cancel.ThrowIfCancellationRequested();
                if (!Matches(Path.GetFileName(dir)))
                    continue;
                // Add the folder under its parents, creating whichever aren't there yet.
                var chain = new Stack<string>();
                for (var p = dir; p != null && !byPath.ContainsKey(p); p = Path.GetDirectoryName(p))
                    chain.Push(p);
                while (chain.Count > 0)
                {
                    var p = chain.Pop();
                    var hit = new Hit(p, Path.GetFileName(p));
                    byPath[Path.GetDirectoryName(p)!].Children.Add(hit);
                    byPath[p] = hit;
                }
                if (++count >= MaxSearchResults)
                    break;
            }
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { }
        if (root.Children.Count > 0)
            groups.Add(root);
        return groups;
    }

    static TreeViewNode ToNode(Hit hit, bool isGroup = false)
    {
        var node = Node(new FolderEntry(hit.Path, hit.Name, isGroup), expanded: true);
        foreach (var child in hit.Children.OrderBy(c => c.Name, StringComparer.OrdinalIgnoreCase))
            node.Children.Add(ToNode(child));
        return node;
    }
}
