using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Windows.System;

namespace Windedit;

/// <summary>A Grid that can set its own mouse cursor (WinUI only lets a subclass do that).</summary>
public sealed class CursorGrid : Grid
{
    public void SetCursor(InputCursor? cursor) => ProtectedCursor = cursor;
}

/// <summary>
/// The white balance picker (docs/phase2a.md). While it is on, a transparent layer over the
/// photo shows a crosshair and takes the click; the engine maps the click back to the photo
/// (through zoom, crop, straighten, rotation and lens) and answers with the temperature and
/// tint that make that spot neutral, which becomes one undo step.
/// </summary>
public sealed partial class MainWindow
{
    bool picking;
    ToggleButton pickerButton = null!;
    Microsoft.UI.Xaml.DispatcherTimer? messageTimer;

    /// <summary>W starts and stops the picker, and Esc stops it. Returns whether the key was used.</summary>
    bool PickerShortcut(VirtualKey key, bool ctrl)
    {
        if (ctrl || gridMode || CurrentPhoto == null)
            return false;
        switch (key)
        {
            case VirtualKey.W: SetPicking(!picking); return true;
            case VirtualKey.Escape when picking: SetPicking(false); return true;
            default: return false;
        }
    }

    void SetPicking(bool on)
    {
        if (on && (CurrentPhoto == null || gridMode))
            on = false;
        if (on == picking)
        {
            pickerButton.IsChecked = on;
            return;
        }
        if (on)
        {
            ExitCrop(cancel: false);
            ExitMasks();
            if (!editMode)
                SetEditMode(true);
            CommitEdit();
        }
        picking = on;
        pickerButton.IsChecked = on;
        PickLayer.Visibility = on ? Visibility.Visible : Visibility.Collapsed;
        PickLayer.SetCursor(on ? InputSystemCursor.Create(InputSystemCursorShape.Cross) : null);
        ShowMessage(on ? "Click a spot that should be neutral · Esc cancels" : null, timed: false);
        FocusKeys();
    }

    void PickLayer_PointerPressed(object sender, PointerRoutedEventArgs e)
    {
        var point = e.GetCurrentPoint(Panel);
        if (!picking || CurrentPhoto == null || !point.Properties.IsLeftButtonPressed)
            return;
        e.Handled = true;
        // The engine's view is in physical pixels.
        var scale = Panel.CompositionScaleX;
        try
        {
            var (kelvin, tint) = engine.PickWhiteBalance((float)(point.Position.X * scale), (float)(point.Position.Y * scale));
            Change(e => e with { CustomWhiteBalance = 1, Temperature = kelvin, Tint = tint }, commit: true);
            SetPicking(false);
            ShowMessage($"White balance set to {kelvin:F0} K, tint {tint:+0;-0;0}", timed: true);
        }
        catch (InvalidOperationException ex)
        {
            // Keeps the white balance as it was, and the picker on for another try.
            ShowMessage(ex.Message, timed: true);
        }
        FocusKeys();
    }

    /// <summary>Shows a message where the key hints are, for a few seconds or until replaced.</summary>
    void ShowMessage(string? text, bool timed)
    {
        messageTimer?.Stop();
        PickStatus.Text = text ?? "";
        PickStatus.Visibility = text == null ? Visibility.Collapsed : Visibility.Visible;
        KeysText.Visibility = text == null ? Visibility.Visible : Visibility.Collapsed;
        if (text == null || !timed)
            return;
        messageTimer ??= new Microsoft.UI.Xaml.DispatcherTimer { Interval = TimeSpan.FromSeconds(6) };
        messageTimer.Tick -= MessageExpired;
        messageTimer.Tick += MessageExpired;
        messageTimer.Start();
    }

    void MessageExpired(object? sender, object e)
    {
        messageTimer?.Stop();
        // While the picker is still waiting for a click, its prompt comes back.
        ShowMessage(picking ? "Click a spot that should be neutral · Esc cancels" : null, timed: false);
    }
}
