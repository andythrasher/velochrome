using System.Diagnostics;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.Storage.Pickers;
using Windows.System;

namespace Windedit;

/// <summary>
/// Phase 0 test window: open a raw file, drag Exposure, and see how long each step takes.
/// Slider moves are coalesced into at most one render per display frame.
/// </summary>
public sealed partial class MainWindow : Window
{
    static readonly string[] RawExtensions = [".orf", ".rw2"];

    readonly Engine engine;
    string? path;
    Engine.OpenInfo opened;
    string openSummary = "";
    bool hasImage, busy, dirty, actualSize;
    // From the first slider move since the last frame to that move being on screen.
    readonly Stopwatch sinceInput = new();
    // Recent frames, for a steady readout while dragging.
    readonly Queue<(double develop, double cpu, double input)> recent = new();

    public MainWindow(string? startPath)
    {
        InitializeComponent();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1600, 1000));
        engine = new Engine();
        Closed += (_, _) => engine.Dispose();
        Content.KeyDown += Content_KeyDown;
        CompositionTarget.Rendering += OnRendering;
        Status.Text = $"{engine.AdapterName} · Ctrl+O opens a raw file";
        if (startPath != null)
            _ = OpenAsync(startPath);
    }

    async void Open_Click(object sender, RoutedEventArgs e)
    {
        var picker = new FileOpenPicker { SuggestedStartLocation = PickerLocationId.PicturesLibrary };
        foreach (var ext in RawExtensions)
            picker.FileTypeFilter.Add(ext);
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var file = await picker.PickSingleFileAsync();
        if (file != null)
            await OpenAsync(file.Path);
    }

    async Task OpenAsync(string newPath)
    {
        if (busy)
            return;
        busy = true;
        Status.Text = $"Opening {Path.GetFileName(newPath)}…";
        var clock = Stopwatch.StartNew();
        try
        {
            // Decoding takes a while; keep the window responsive. Nothing else touches the
            // engine meanwhile (busy blocks rendering).
            opened = await Task.Run(() => engine.Open(newPath));
            path = newPath;
            hasImage = true;
            actualSize = false;
            recent.Clear();
            busy = false;
            Render();
            var o = opened;
            openSummary = $"{engine.AdapterName} · {Path.GetFileName(path)} {o.Width}×{o.Height} · " +
                $"open {o.TotalMs:F0} ms (decode {o.DecodeMs:F0}, upload {o.UploadMs:F1}, copy {o.CopyMs:F1}, " +
                $"demosaic {o.DemosaicMs:F1}, pyramid {o.PyramidMs:F1}) · on screen after {clock.Elapsed.TotalMilliseconds:F0} ms";
            Status.Text = openSummary;
            Title = $"{Path.GetFileName(path)} – Windedit";
        }
        catch (Exception ex)
        {
            Status.Text = $"Couldn't open {Path.GetFileName(newPath)}: {ex.Message}";
        }
        finally
        {
            busy = false;
        }
    }

    void Exposure_ValueChanged(object sender, RangeBaseValueChangedEventArgs e)
    {
        ExposureText.Text = e.NewValue.ToString("+0.00;-0.00;0.00");
        engine.SetExposure((float)e.NewValue);
        Invalidate();
    }

    void Exposure_DoubleTapped(object sender, DoubleTappedRoutedEventArgs e) => ExposureSlider.Value = 0;

    void Invalidate()
    {
        if (!dirty)
            sinceInput.Restart();
        dirty = true;
    }

    void OnRendering(object? sender, object e)
    {
        if (dirty && !busy)
            Render();
    }

    void Render()
    {
        dirty = false;
        if (!hasImage)
            return;
        var t = engine.Render();
        var input = sinceInput.IsRunning ? sinceInput.Elapsed.TotalMilliseconds : t.CpuMs;
        sinceInput.Reset();
        recent.Enqueue((t.DevelopMs, t.CpuMs, input));
        while (recent.Count > 30)
            recent.Dequeue();
        if (recent.Count > 1)
        {
            Status.Text = $"{openSummary}\n" +
                $"Last {recent.Count} frames: " +
                $"develop {recent.Average(r => r.develop):F3} ms on the GPU, render call {recent.Average(r => r.cpu):F2} ms, " +
                $"slider-to-screen {recent.Average(r => r.input):F1} ms · {(actualSize ? "100%" : "fit")}";
        }
    }

    void Panel_SizeChanged(object sender, SizeChangedEventArgs e) => ResizeView();

    void Panel_CompositionScaleChanged(SwapChainPanel sender, object args) => ResizeView();

    void ResizeView()
    {
        if (busy || Panel.ActualWidth < 1 || Panel.ActualHeight < 1)
            return;
        var scale = Panel.CompositionScaleX;
        engine.ResizeView(Panel, (uint)Math.Round(Panel.ActualWidth * scale), (uint)Math.Round(Panel.ActualHeight * scale), scale);
        Render();
    }

    void Panel_PointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (!hasImage || busy)
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
            var fit = Math.Min(w / opened.Width, h / opened.Height);
            var x = (p.X - (w - opened.Width * fit) / 2) / (opened.Width * fit);
            var y = (p.Y - (h - opened.Height * fit) / 2) / (opened.Height * fit);
            engine.ZoomToActual((float)x, (float)y);
        }
        actualSize = !actualSize;
        Invalidate();
    }

    void Content_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (path == null || busy || (e.Key != VirtualKey.Left && e.Key != VirtualKey.Right))
            return;
        var files = Directory.EnumerateFiles(Path.GetDirectoryName(path)!)
            .Where(f => RawExtensions.Contains(Path.GetExtension(f).ToLowerInvariant()))
            .Order(StringComparer.OrdinalIgnoreCase)
            .ToList();
        var next = files.IndexOf(path) + (e.Key == VirtualKey.Right ? 1 : -1);
        if (next >= 0 && next < files.Count)
            _ = OpenAsync(files[next]);
        e.Handled = true;
    }
}
