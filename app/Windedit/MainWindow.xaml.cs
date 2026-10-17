using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Windows.Storage.Pickers;
using Windows.System;

namespace Windedit;

/// <summary>
/// Phase 1a test window: open a photo, move through its folder, and see what the engine drew
/// and how soon. The engine does the work on its own threads; this window only sends requests
/// and shows the frame reports it gets back.
/// </summary>
public sealed partial class MainWindow : Window
{
    static readonly string[] Extensions = [".orf", ".rw2", ".dng", ".jpg", ".jpeg"];
    static readonly string[] LayerNames = ["", "quick preview", "loupe preview", "full-size preview", "raw"];
    // Sensor sizes seen, so the next launch can allocate GPU memory for them before it's needed.
    static readonly string SizesFile = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Windedit", "sizes.txt");

    readonly Engine engine;
    readonly Microsoft.UI.Dispatching.DispatcherQueue ui;
    int count, index = -1;
    bool actualSize;
    Engine.Frame last;
    readonly List<(uint, uint)> sizes = [];

    public MainWindow(string? startPath)
    {
        InitializeComponent();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1600, 1000));
        ui = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        engine = new Engine();
        engine.FrameDrawn += frame => ui.TryEnqueue(() => OnFrame(frame));
        Closed += (_, _) => engine.Dispose();
        Content.KeyDown += Content_KeyDown;
        PrepareRememberedSizes();
        Status.Text = $"{engine.AdapterName} · GPU warm-up {engine.WarmUpMs:F1} ms · Ctrl+O opens a photo";
        if (startPath != null)
            OpenPhoto(startPath);
    }

    async void Open_Click(object sender, RoutedEventArgs e)
    {
        var picker = new FileOpenPicker { SuggestedStartLocation = PickerLocationId.PicturesLibrary };
        foreach (var ext in Extensions)
            picker.FileTypeFilter.Add(ext);
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var file = await picker.PickSingleFileAsync();
        if (file != null)
            OpenPhoto(file.Path);
    }

    /// <summary>Opens the photo's folder and shows it.</summary>
    void OpenPhoto(string path)
    {
        try
        {
            count = engine.OpenFolder(Path.GetDirectoryName(path)!);
            Show(Math.Max(0, engine.FindPhoto(path)));
        }
        catch (Exception ex)
        {
            Status.Text = $"Couldn't open {Path.GetFileName(path)}: {ex.Message}";
        }
    }

    void Show(int newIndex)
    {
        if (newIndex < 0 || newIndex >= count)
            return;
        index = newIndex;
        engine.Show(index);
        Title = $"{engine.PhotoName(index)} ({index + 1} of {count}) – Windedit";
    }

    void OnFrame(Engine.Frame f)
    {
        last = f;
        RememberSize(f.RawWidth, f.RawHeight);
        if (f.Photo != index)
            return;
        var open = f.OpenMs > 0 ? $", raw opened on the GPU in {f.OpenMs:F1} ms" : "";
        Status.Text = $"{engine.AdapterName} · {engine.PhotoName(index)} ({index + 1} of {count}) " +
            $"{f.Width}×{f.Height} · {(actualSize ? "100%" : "fit")}{(EditToggle.IsChecked == true ? " · Edit" : "")}\n" +
            $"Showing the {LayerNames[f.Layer]}, {f.SinceShowMs:F1} ms after it was asked for{open} · " +
            $"frame: GPU {f.GpuMs:F2} ms, total {f.CpuMs:F2} ms";
    }

    void Edit_Click(object sender, RoutedEventArgs e) => engine.SetEditMode(EditToggle.IsChecked == true);

    void Exposure_ValueChanged(object sender, RangeBaseValueChangedEventArgs e)
    {
        ExposureText.Text = e.NewValue.ToString("+0.00;-0.00;0.00");
        engine.SetExposure((float)e.NewValue);
    }

    void Exposure_DoubleTapped(object sender, DoubleTappedRoutedEventArgs e) => ExposureSlider.Value = 0;

    void Panel_SizeChanged(object sender, SizeChangedEventArgs e) => ResizeView();

    void Panel_CompositionScaleChanged(SwapChainPanel sender, object args) => ResizeView();

    void ResizeView()
    {
        if (Panel.ActualWidth < 1 || Panel.ActualHeight < 1)
            return;
        var scale = Panel.CompositionScaleX;
        engine.ResizeView(Panel, (uint)Math.Round(Panel.ActualWidth * scale), (uint)Math.Round(Panel.ActualHeight * scale), scale);
    }

    void Panel_PointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (index < 0 || last.Width == 0)
            return;
        if (actualSize)
        {
            engine.ZoomToFit();
        }
        else
        {
            // Where the click landed on the fitted photo, as fractions of its size.
            var p = e.GetCurrentPoint(Panel).Position;
            double w = Panel.ActualWidth, h = Panel.ActualHeight;
            var fit = Math.Min(w / last.Width, h / last.Height);
            var x = (p.X - (w - last.Width * fit) / 2) / (last.Width * fit);
            var y = (p.Y - (h - last.Height * fit) / 2) / (last.Height * fit);
            engine.ZoomToActual((float)x, (float)y);
        }
        actualSize = !actualSize;
    }

    void Content_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (index < 0)
            return;
        switch (e.Key)
        {
            case VirtualKey.Right: Show(index + 1); break;
            case VirtualKey.Left: Show(index - 1); break;
            case VirtualKey.Home: Show(0); break;
            case VirtualKey.End: Show(count - 1); break;
            case VirtualKey.F12:
                // Diagnostic: what the engine drew, to compare with the screen.
                engine.SaveSnapshot(Path.Combine(Path.GetTempPath(), "windedit-snapshot.png"));
                break;
            case VirtualKey.E:
                EditToggle.IsChecked = EditToggle.IsChecked != true;
                engine.SetEditMode(EditToggle.IsChecked == true);
                break;
            default: return;
        }
        e.Handled = true;
    }

    void PrepareRememberedSizes()
    {
        try
        {
            foreach (var line in File.ReadAllLines(SizesFile))
            {
                var parts = line.Split(' ');
                if (parts.Length == 2 && uint.TryParse(parts[0], out var w) && uint.TryParse(parts[1], out var h))
                {
                    sizes.Add((w, h));
                    engine.Prepare(w, h);
                }
            }
        }
        catch (IOException) { }
        catch (UnauthorizedAccessException) { }
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
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(SizesFile)!);
            File.WriteAllLines(SizesFile, sizes.Select(s => $"{s.Item1} {s.Item2}"));
        }
        catch (IOException) { }
        catch (UnauthorizedAccessException) { }
    }
}
