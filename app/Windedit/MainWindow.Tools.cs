using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Input;

namespace Windedit;

/// <summary>
/// The tool strip at the top of the Edit panel (Edit, Crop, Masks) and dragging a zoomed photo.
/// The tools themselves are in MainWindow.Crop.cs and MainWindow.Masks.cs, which enter and leave
/// them from the strip, the keys and anywhere else; they all end by calling <see cref="UpdateTools"/>.
/// </summary>
public sealed partial class MainWindow
{
    // ---- The tool strip ----

    /// <summary>Shows which tool is active (Edit when neither of the others is) and the panel under the strip.</summary>
    void UpdateTools()
    {
        EditTool.IsChecked = !cropping && !masking;
        CropTool.IsChecked = cropping;
        MaskTool.IsChecked = masking;
        CropHelp.Visibility = cropping ? Visibility.Visible : Visibility.Collapsed;
        EditControls.Visibility = !cropping && !masking ? Visibility.Visible : Visibility.Collapsed;
        CropControls.Visibility = cropping ? Visibility.Visible : Visibility.Collapsed;
        UpdatePanCursor();
    }

    void EditTool_Click(object sender, RoutedEventArgs e)
    {
        // Whatever tool is on goes off; Edit itself stays on, so clicking it again does nothing.
        ExitCrop(cancel: false);
        ExitMasks();
        UpdateTools();
        FocusKeys();
    }

    void CropTool_Click(object sender, RoutedEventArgs e)
    {
        ToggleCrop();
        UpdateTools();
        FocusKeys();
    }

    void MaskTool_Click(object sender, RoutedEventArgs e)
    {
        if (masking)
            ExitMasks();
        else
            EnterMasks();
        UpdateTools();
        FocusKeys();
    }

    // ---- Dragging a zoomed photo ----

    /// <summary>The 100% view's centre as fractions of the crop, which the pan moves.</summary>
    (double X, double Y) zoomCenter = (0.5, 0.5);
    bool panning;
    Windows.Foundation.Point panStart;
    (double X, double Y) panFrom;
    /// <summary>The pointer has gone further than a click: letting go leaves the zoom alone.</summary>
    bool panMoved;
    // A press that moves no further than this (in view-independent pixels) is a click.
    const double ClickSlop = 4;

    /// <summary>The cropped photo's size in pixels: what the 100% view's centre is a fraction of. (The frame event's own size is of what the view shows, which at 100% is only part of it.)</summary>
    (double W, double H) CropPixels() =>
        ((edit.CropRight - edit.CropLeft) * last.FrameWidth, (edit.CropBottom - edit.CropTop) * last.FrameHeight);

    /// <summary>Keeps the view's edges inside the photo's: the centre can't go so near an edge that blank canvas shows.</summary>
    (double X, double Y) ClampCenter(double x, double y)
    {
        var scale = Panel.CompositionScaleX;
        double Clamp(double c, double photo, double view)
        {
            if (photo <= view || photo <= 0)
                return 0.5;
            var half = view / 2 / photo;
            return Math.Clamp(c, half, 1 - half);
        }
        var (w, h) = CropPixels();
        return (Clamp(x, w, Panel.ActualWidth * scale), Clamp(y, h, Panel.ActualHeight * scale));
    }

    /// <summary>The press that starts a pan, or a click that will zoom out if the pointer stays put.</summary>
    void BeginPan(PointerRoutedEventArgs e)
    {
        panning = true;
        panMoved = false;
        panStart = e.GetCurrentPoint(Panel).Position;
        panFrom = zoomCenter;
        Panel.CapturePointer(e.Pointer);
        UpdatePanCursor();
    }

    void Panel_PointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (!panning || !actualSize)
            return;
        var p = e.GetCurrentPoint(Panel).Position;
        double dx = p.X - panStart.X, dy = p.Y - panStart.Y;
        if (!panMoved && Math.Abs(dx) <= ClickSlop && Math.Abs(dy) <= ClickSlop)
            return;
        panMoved = true;
        // At 100% a photo pixel is a physical pixel, so the photo follows the pointer 1:1 when the
        // centre moves by the pointer's move in photo pixels, the other way.
        var scale = Panel.CompositionScaleX;
        var c = ClampCenter(panFrom.X - dx * scale / Math.Max(1, CropPixels().W), panFrom.Y - dy * scale / Math.Max(1, CropPixels().H));
        zoomCenter = c;
        engine.ZoomToActual((float)c.X, (float)c.Y);
    }

    void Panel_PointerReleased(object sender, PointerRoutedEventArgs e)
    {
        if (!panning)
            return;
        // Ending first: releasing the capture raises PointerCaptureLost, which would end the pan as not a click.
        EndPan(click: !panMoved);
        Panel.ReleasePointerCapture(e.Pointer);
    }

    void Panel_PointerCaptureLost(object sender, PointerRoutedEventArgs e) => EndPan(click: false);

    void EndPan(bool click)
    {
        if (!panning)
            return;
        panning = false;
        if (click && actualSize)
            ToggleZoom(0.5, 0.5);
        UpdatePanCursor();
    }

    /// <summary>A hand while the photo can be dragged, and a four-way arrow while it is (WinUI has no closed hand).</summary>
    void UpdatePanCursor()
    {
        var can = actualSize && !cropping && !masking && !picking;
        CanvasHost.SetCursor(can ? InputSystemCursor.Create(panning ? InputSystemCursorShape.SizeAll : InputSystemCursorShape.Hand) : null);
    }
}
