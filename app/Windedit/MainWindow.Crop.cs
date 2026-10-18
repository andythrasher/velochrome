using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.Foundation;
using Microsoft.UI.Input;
using Windows.System;

namespace Windedit;

/// <summary>A Canvas that can set its own mouse cursor (WinUI only lets a subclass do that).</summary>
public sealed class CursorCanvas : Canvas
{
    public void SetCursor(Microsoft.UI.Input.InputCursor? cursor) => ProtectedCursor = cursor;
}

/// <summary>
/// Crop mode (docs/crop.md). The engine shows the whole frame at Fit with the photo turned
/// beneath it and reports the frame's rectangle on the view with every frame; this draws the crop
/// box on a Canvas over the swap chain panel and turns drags into edits. Every constraint
/// (the crop staying inside the photo) is the engine's (<see cref="Engine.ConstrainCrop"/>).
/// </summary>
public sealed partial class MainWindow
{
    const int Left = 1, Top = 2, Right = 4, Bottom = 8, Move = 16, Turn = 32;
    const double HandleReach = 12;

    bool cropping;
    /// <summary>The box as the user last set it (resizing, moving, an aspect, a turn). Straightening shows
    /// this shrunk to fit, never the shrunk box itself, so turning back gives it back.</summary>
    float[] intended = [0, 0, 1, 1];
    /// <summary>A quarter turn is under way: the frame the engine last reported is the old one.</summary>
    bool skipSync;
    /// <summary>The photo's edit when crop mode started, for Esc.</summary>
    Engine.Edit cropEntry;
    /// <summary>Changes committed since crop mode started, which Esc steps back.</summary>
    int cropCommits;
    /// <summary>The aspect ratio (width over height) the box keeps, or null for free.</summary>
    double? aspect;
    bool aspectLocked;
    double entryAspect = 1;

    // A drag in progress.
    bool dragging;
    int dragKind;
    Point dragStart;
    float[] dragCrop = new float[4];
    float dragAngle;
    double dragAspect = 1;
    // Dragging the photo: the box stays where it was on screen while the view follows it.
    (float X, float Y) viewCenter = (0.5f, 0.5f);
    (float X, float Y) dragCenter;
    Rect dragFrame, dragBox;
    Microsoft.UI.Xaml.DispatcherTimer? recenter;

    Button cropButton = null!;
    ComboBox aspectBox = null!;
    ToggleButton lockButton = null!;
    readonly Rectangle[] dim = new Rectangle[4];
    readonly Rectangle cropBorder = new();
    readonly Rectangle[] handles = new Rectangle[8];
    readonly Line[] thirds = new Line[4];
    // Shown instead of the thirds while turning: ten divisions each way, and the angle.
    readonly Line[] fine = new Line[18];
    readonly Border angleLabel = new();
    readonly TextBlock angleText = new();
    readonly Border turnGlyph = new();

    static readonly string[] AspectNames = ["Original", "As shot", "1:1", "4:5", "5:7", "2:3", "16:9", "Free"];
    // Landscape ratios of the presets that have one (index 2 on); they turn to suit the box.
    static readonly double[] AspectRatios = [1, 5.0 / 4, 7.0 / 5, 3.0 / 2, 16.0 / 9];

    // ---- The Crop group of the Edit panel ----

    void BuildCropGroup()
    {
        Header("Crop");
        cropButton = new Button { Content = "Crop  (R)", HorizontalAlignment = HorizontalAlignment.Stretch };
        cropButton.Click += (_, _) =>
        {
            ToggleCrop();
            FocusKeys();
        };
        group.Children.Add(cropButton);

        aspectBox = new ComboBox { HorizontalAlignment = HorizontalAlignment.Stretch, Margin = new Thickness(0, 4, 0, 0) };
        foreach (var name in AspectNames)
            aspectBox.Items.Add(name);
        aspectBox.SelectedIndex = AspectNames.Length - 1;
        aspectBox.SelectionChanged += (_, _) =>
        {
            AspectChosen(aspectBox.SelectedIndex);
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(aspectBox, "Aspect ratio");
        group.Children.Add(aspectBox);

        lockButton = new ToggleButton { Content = "Lock" };
        ToolTipService.SetToolTip(lockButton, "Keep the aspect ratio");
        lockButton.Click += (_, _) =>
        {
            aspectLocked = lockButton.IsChecked == true;
            if (aspectLocked && aspect == null)
                aspect = CropAspect(edit);
            FocusKeys();
        };
        var swap = Tool("⇄", "Swap portrait and landscape (X)", () => SwapAspect());
        var left = Tool("↺", "Rotate left (Ctrl+[)", () => RotateBy(-1));
        var right = Tool("↻", "Rotate right (Ctrl+])", () => RotateBy(1));
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, Margin = new Thickness(0, 4, 0, 0) };
        foreach (var b in new ButtonBase[] { lockButton, swap, left, right })
            row.Children.Add(b);
        group.Children.Add(row);

        Add(new Adjustment
        {
            Name = "Straighten", Min = -45, Max = 45, Step = 0.1,
            Get = e => e.Angle,
            Set = (e, v) => WithAngle(e, (float)Math.Round(v, 2)),
            Format = v => $"{v:+0.0;-0.0;0.0}°",
        });

        var reset = new HyperlinkButton { Content = "Reset crop", Padding = new Thickness(4, 0, 4, 0), FontSize = 12 };
        reset.Click += (_, _) =>
        {
            Change(e => WithCrop(e with { Rotation = 0, Angle = 0 }, [0, 0, 1, 1]), commit: true);
            FocusKeys();
        };
        group.Children.Add(reset);
    }

    Button Tool(string glyph, string tip, Action click)
    {
        var button = new Button { Content = glyph, MinWidth = 40 };
        ToolTipService.SetToolTip(button, tip);
        button.Click += (_, _) =>
        {
            click();
            FocusKeys();
        };
        return button;
    }

    // ---- Keys ----

    /// <summary>R, Enter, Esc, X and Ctrl+[ / Ctrl+] for crop mode. Returns whether the key was used.</summary>
    bool CropShortcut(VirtualKey key, bool ctrl)
    {
        if (gridMode || CurrentPhoto == null)
            return false;
        if (ctrl)
        {
            // Oem4 and Oem6: the bracket keys.
            if (key == (VirtualKey)219)
                RotateBy(-1);
            else if (key == (VirtualKey)221)
                RotateBy(1);
            else
                return false;
            return true;
        }
        switch (key)
        {
            case VirtualKey.R: ToggleCrop(); return true;
            case VirtualKey.Enter when cropping: ExitCrop(cancel: false); return true;
            case VirtualKey.Escape when cropping: ExitCrop(cancel: true); return true;
            case VirtualKey.X when cropping: SwapAspect(); return true;
            case VirtualKey.Left when cropping: NudgePhoto(-1, 0, Shifted()); return true;
            case VirtualKey.Right when cropping: NudgePhoto(1, 0, Shifted()); return true;
            case VirtualKey.Up when cropping: NudgePhoto(0, -1, Shifted()); return true;
            case VirtualKey.Down when cropping: NudgePhoto(0, 1, Shifted()); return true;
            default: return false;
        }
    }

    static bool Shifted() => InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);

    // ---- Entering and leaving ----

    void ToggleCrop()
    {
        if (cropping)
            ExitCrop(cancel: false);
        else
            EnterCrop();
    }

    void EnterCrop()
    {
        if (CurrentPhoto == null)
            return;
        SetPicking(false);
        if (gridMode)
            SetGridMode(false);
        if (!editMode)
            SetEditMode(true);
        if (actualSize)
        {
            actualSize = false;
            engine.ZoomToFit();
            UpdatePhotoText();
        }
        CommitEdit();
        cropping = true;
        cropCommits = 0;
        cropEntry = edit;
        intended = CropOf(edit);
        entryAspect = CropAspect(edit);
        engine.SetCropMode(true);
        CropCanvas.Visibility = Visibility.Visible;
        cropButton.Content = "Done  (Enter)";
        UpdateCropOverlay();
        FocusKeys();
    }

    /// <summary>Leaves crop mode. Everything was committed as it was dragged, so applying only
    /// leaves; cancelling steps the changes made in it back.</summary>
    void ExitCrop(bool cancel)
    {
        if (!cropping)
            return;
        CommitEdit();
        if (cancel && CurrentPhoto is { } p)
        {
            for (var i = 0; i < cropCommits && !SameGeometry(edit, cropEntry); i++)
            {
                if (engine.Undo(p.Index, false) is not { } now)
                    break;
                edit = now;
            }
            if (!SameGeometry(edit, cropEntry))
                Change(e => WithCrop(e with { Rotation = cropEntry.Rotation, Angle = cropEntry.Angle }, CropOf(cropEntry)), commit: true);
            AfterCommit(p);
        }
        StopRecenter();
        viewCenter = (0.5f, 0.5f);
        cropping = false;
        dragging = false;
        engine.SetCropMode(false);
        CropCanvas.Visibility = Visibility.Collapsed;
        cropButton.Content = "Crop  (R)";
        ShowEdit();
        FocusKeys();
    }

    /// <summary>Counts a commit made while cropping.</summary>
    void Committed()
    {
        if (cropping)
            cropCommits++;
    }

    static bool SameGeometry(Engine.Edit a, Engine.Edit b) =>
        a.Rotation == b.Rotation && a.Angle == b.Angle && CropOf(a).SequenceEqual(CropOf(b));

    // ---- Edits ----

    static float[] CropOf(Engine.Edit e) => [e.CropLeft, e.CropTop, e.CropRight, e.CropBottom];

    static Engine.Edit WithCrop(Engine.Edit e, float[] c) =>
        e with { CropLeft = c[0], CropTop = c[1], CropRight = c[2], CropBottom = c[3] };

    /// <summary>The crop's width over height in pixels.</summary>
    double CropAspect(Engine.Edit e)
    {
        var c = CropOf(e);
        var w = (c[2] - c[0]) * Math.Max(1, last.FrameWidth);
        var h = (c[3] - c[1]) * Math.Max(1, last.FrameHeight);
        return h > 0 ? w / h : 1;
    }

    /// <summary>A new angle, with the intended box made smaller where the photo no longer reaches it.</summary>
    Engine.Edit WithAngle(Engine.Edit e, float angle)
    {
        e = e with { Angle = angle };
        if (last.FrameWidth == 0)
            return e;
        return WithCrop(e, Engine.ConstrainCrop(last.FrameWidth, last.FrameHeight, angle, Engine.Constrain.Shrink, intended, intended));
    }

    /// <summary>Keeps <see cref="intended"/> in step with the edit: it stays while the edit's box is what
    /// straightening makes of it (so undo and redo of an angle keep it), and follows the box when
    /// the box was changed some other way (a resize, a move, an aspect, a turn, another photo).</summary>
    void SyncIntended()
    {
        var c = CropOf(edit);
        if (skipSync || last.FrameWidth == 0)
        {
            if (!skipSync)
                intended = c;
            return;
        }
        var shown = Engine.ConstrainCrop(last.FrameWidth, last.FrameHeight, edit.Angle, Engine.Constrain.Shrink, intended, intended);
        for (var i = 0; i < 4; i++)
            if (Math.Abs(shown[i] - c[i]) > 1e-4)
            {
                intended = c;
                return;
            }
    }

    /// <summary>Turns the photo a quarter turn; the crop turns with it.</summary>
    void RotateBy(int direction)
    {
        if (CurrentPhoto == null || gridMode)
            return;
        float[] Turned(float[] c) => direction > 0 ? [1 - c[3], c[0], 1 - c[1], c[2]] : [c[1], 1 - c[2], c[3], 1 - c[0]];
        // The intended box turns with the frame, and the shown one follows from it.
        intended = Turned(intended);
        skipSync = true;
        Change(e => WithCrop(e with { Rotation = (e.Rotation + (direction > 0 ? 1u : 3u)) % 4 }, Turned(CropOf(e))), commit: true);
        skipSync = false;
        if (aspect is { } a)
            aspect = 1 / a;
    }

    void AspectChosen(int index)
    {
        if (CurrentPhoto == null || last.FrameWidth == 0)
            return;
        if (index == AspectNames.Length - 1)
        {
            aspect = null;
            aspectLocked = false;
            lockButton.IsChecked = false;
            return;
        }
        var portrait = CropAspect(edit) < 1;
        var ratio = index switch
        {
            0 => (double)last.FrameWidth / last.FrameHeight,
            1 => entryAspect,
            _ => portrait ? 1 / AspectRatios[index - 2] : AspectRatios[index - 2],
        };
        aspect = ratio;
        aspectLocked = true;
        lockButton.IsChecked = true;
        FitAspect(ratio);
    }

    /// <summary>The largest box of this shape inside the current one.</summary>
    void FitAspect(double ratio)
    {
        var current = CropOf(edit);
        var fitted = Engine.ConstrainCrop(last.FrameWidth, last.FrameHeight, edit.Angle, Engine.Constrain.Aspect,
            current, current, (float)ratio);
        Change(e => WithCrop(e, fitted), commit: true);
    }

    void SwapAspect()
    {
        if (CurrentPhoto == null || last.FrameWidth == 0)
            return;
        var ratio = 1 / (aspect ?? CropAspect(edit));
        if (aspect != null)
            aspect = ratio;
        FitAspect(ratio);
    }

    // ---- The overlay ----

    void InitCrop()
    {
        var shade = new SolidColorBrush(Windows.UI.Color.FromArgb(0xA0, 0, 0, 0));
        var white = new SolidColorBrush(Colors.White);
        for (var i = 0; i < dim.Length; i++)
            CropCanvas.Children.Add(dim[i] = new Rectangle { Fill = shade, IsHitTestVisible = false });
        cropBorder.Stroke = white;
        cropBorder.StrokeThickness = 1.5;
        cropBorder.IsHitTestVisible = false;
        CropCanvas.Children.Add(cropBorder);
        for (var i = 0; i < thirds.Length; i++)
        {
            thirds[i] = new Line { Stroke = new SolidColorBrush(Windows.UI.Color.FromArgb(0xB0, 255, 255, 255)), StrokeThickness = 1, Visibility = Visibility.Collapsed, IsHitTestVisible = false };
            CropCanvas.Children.Add(thirds[i]);
        }
        for (var i = 0; i < handles.Length; i++)
        {
            handles[i] = new Rectangle { Width = 9, Height = 9, Fill = white, Stroke = new SolidColorBrush(Colors.Black), StrokeThickness = 1, IsHitTestVisible = false };
            CropCanvas.Children.Add(handles[i]);
        }
        var faint = new SolidColorBrush(Windows.UI.Color.FromArgb(0x70, 255, 255, 255));
        for (var i = 0; i < fine.Length; i++)
        {
            fine[i] = new Line { Stroke = faint, StrokeThickness = 1, Visibility = Visibility.Collapsed, IsHitTestVisible = false };
            CropCanvas.Children.Add(fine[i]);
        }
        angleText.FontSize = 13;
        angleText.Foreground = white;
        angleLabel.Child = angleText;
        angleLabel.Background = new SolidColorBrush(Windows.UI.Color.FromArgb(0xC0, 0, 0, 0));
        angleLabel.CornerRadius = new CornerRadius(4);
        angleLabel.Padding = new Thickness(6, 2, 6, 2);
        angleLabel.Visibility = Visibility.Collapsed;
        angleLabel.IsHitTestVisible = false;
        CropCanvas.Children.Add(angleLabel);
        // Beside the pointer outside the box: turning is what a drag here does.
        turnGlyph.Child = new FontIcon { Glyph = "", FontSize = 16, Foreground = white };
        turnGlyph.Background = new SolidColorBrush(Windows.UI.Color.FromArgb(0xC0, 0, 0, 0));
        turnGlyph.CornerRadius = new CornerRadius(12);
        turnGlyph.Padding = new Thickness(4);
        turnGlyph.Visibility = Visibility.Collapsed;
        turnGlyph.IsHitTestVisible = false;
        CropCanvas.Children.Add(turnGlyph);
        CropCanvas.PointerExited += (_, _) => turnGlyph.Visibility = Visibility.Collapsed;
        CropCanvas.PointerPressed += CropPointerPressed;
        CropCanvas.PointerMoved += CropPointerMoved;
        CropCanvas.PointerReleased += CropPointerReleased;
        CropCanvas.PointerCaptureLost += (_, _) => EndDrag();
        CropCanvas.SizeChanged += (_, _) => UpdateCropOverlay();
    }

    /// <summary>The frame, in the canvas's units, once a crop-mode frame has been drawn.</summary>
    bool FrameRect(out Rect frame)
    {
        var scale = Panel.CompositionScaleX;
        frame = new Rect(last.ViewX / scale, last.ViewY / scale, last.ViewW / scale, last.ViewH / scale);
        return cropping && CurrentPhoto?.Index == last.Photo && last.ViewW > 0 && last.FrameWidth > 0
            && last.Width == last.FrameWidth && last.Height == last.FrameHeight;
    }

    /// <summary>The crop box in the canvas's units.</summary>
    Rect CropRect(Rect frame, float[] c) =>
        new(frame.X + c[0] * frame.Width, frame.Y + c[1] * frame.Height, (c[2] - c[0]) * frame.Width, (c[3] - c[1]) * frame.Height);

    void UpdateCropOverlay()
    {
        var ready = FrameRect(out var f);
        CropCanvas.Opacity = ready ? 1 : 0;
        if (!ready)
            return;
        var r = CropRect(f, CropOf(edit));
        if (dragging && dragKind == Move)
        {
            // Exactly where the drag puts them, not where the last drawn frame had them.
            f = new Rect(dragFrame.X - (viewCenter.X - dragCenter.X) * f.Width, dragFrame.Y - (viewCenter.Y - dragCenter.Y) * f.Height, f.Width, f.Height);
            r = dragBox;
        }
        void Place(UIElement e, double x, double y, double w, double h)
        {
            Canvas.SetLeft(e, x);
            Canvas.SetTop(e, y);
            if (e is FrameworkElement fe)
            {
                fe.Width = Math.Max(0, w);
                fe.Height = Math.Max(0, h);
            }
        }
        // Darken the frame around the box: above and below it, then left and right.
        Place(dim[0], f.X, f.Y, f.Width, r.Y - f.Y);
        Place(dim[1], f.X, r.Bottom, f.Width, f.Bottom - r.Bottom);
        Place(dim[2], f.X, r.Y, r.X - f.X, r.Height);
        Place(dim[3], r.Right, r.Y, f.Right - r.Right, r.Height);
        Place(cropBorder, r.X, r.Y, r.Width, r.Height);
        double[] xs = [r.X, r.X + r.Width / 2, r.Right], ys = [r.Y, r.Y + r.Height / 2, r.Bottom];
        var n = 0;
        foreach (var y in ys)
            foreach (var x in xs)
            {
                if (x == xs[1] && y == ys[1])
                    continue;
                Canvas.SetLeft(handles[n], x - 4.5);
                Canvas.SetTop(handles[n], y - 4.5);
                n++;
            }
        for (var i = 0; i < 2; i++)
        {
            var (v, h) = (thirds[i], thirds[i + 2]);
            (v.X1, v.X2, v.Y1, v.Y2) = (r.X + r.Width * (i + 1) / 3, r.X + r.Width * (i + 1) / 3, r.Y, r.Bottom);
            (h.X1, h.X2, h.Y1, h.Y2) = (r.X, r.Right, r.Y + r.Height * (i + 1) / 3, r.Y + r.Height * (i + 1) / 3);
        }
        for (var i = 0; i < 9; i++)
        {
            var (v, h) = (fine[i], fine[i + 9]);
            var x = r.X + r.Width * (i + 1) / 10;
            var y = r.Y + r.Height * (i + 1) / 10;
            (v.X1, v.X2, v.Y1, v.Y2) = (x, x, r.Y, r.Bottom);
            (h.X1, h.X2, h.Y1, h.Y2) = (r.X, r.Right, y, y);
        }
    }

    // ---- Dragging ----

    int HitTest(Point p, Rect r)
    {
        var inX = p.X >= r.X - HandleReach && p.X <= r.Right + HandleReach;
        var inY = p.Y >= r.Y - HandleReach && p.Y <= r.Bottom + HandleReach;
        var flags = 0;
        if (inY && Math.Abs(p.X - r.X) <= HandleReach) flags |= Left;
        if (inY && Math.Abs(p.X - r.Right) <= HandleReach) flags |= Right;
        if (inX && Math.Abs(p.Y - r.Y) <= HandleReach) flags |= Top;
        if (inX && Math.Abs(p.Y - r.Bottom) <= HandleReach) flags |= Bottom;
        // A box too small to tell its edges apart: the nearer one.
        if ((flags & (Left | Right)) == (Left | Right))
            flags &= Math.Abs(p.X - r.X) < Math.Abs(p.X - r.Right) ? ~Right : ~Left;
        if ((flags & (Top | Bottom)) == (Top | Bottom))
            flags &= Math.Abs(p.Y - r.Y) < Math.Abs(p.Y - r.Bottom) ? ~Bottom : ~Top;
        if (flags != 0)
            return flags;
        return r.Contains(p) ? Move : Turn;
    }

    void CropPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (!FrameRect(out var f))
            return;
        var p = e.GetCurrentPoint(CropCanvas).Position;
        dragStart = p;
        dragCrop = CropOf(edit);
        dragAngle = edit.Angle;
        dragAspect = CropAspect(edit);
        dragKind = HitTest(p, CropRect(f, dragCrop));
        StopRecenter();
        dragCenter = viewCenter;
        dragFrame = f;
        dragBox = CropRect(f, dragCrop);
        dragging = true;
        CropCanvas.CapturePointer(e.Pointer);
        SetGrid(dragKind == Turn);
        UpdateCropOverlay();
        e.Handled = true;
    }

    void CropPointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (!FrameRect(out var f))
            return;
        var p = e.GetCurrentPoint(CropCanvas).Position;
        if (!dragging)
        {
            ShowHover(p, f);
            return;
        }
        float fw = last.FrameWidth, fh = last.FrameHeight, angle = edit.Angle;
        var current = CropOf(edit);
        var s = dragCrop;
        // The pointer's move, in fractions of the frame.
        var dx = (float)((p.X - dragStart.X) / f.Width);
        var dy = (float)((p.Y - dragStart.Y) / f.Height);

        if (dragKind == Turn)
        {
            var c = new Point(f.X + f.Width / 2, f.Y + f.Height / 2);
            var turn = Math.Atan2(p.Y - c.Y, p.X - c.X) - Math.Atan2(dragStart.Y - c.Y, dragStart.X - c.X);
            var degrees = turn * 180 / Math.PI;
            degrees -= 360 * Math.Round(degrees / 360);
            var total = Math.Clamp(dragAngle + degrees, -45, 45);
            var shiftNow = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
            var a = (float)(shiftNow ? Math.Round(total) : Math.Round(total, 1));
            // From the box as the user set it, so turning back gives it back.
            var box = Engine.ConstrainCrop(fw, fh, a, Engine.Constrain.Shrink, intended, intended);
            Change(ed => WithCrop(ed with { Angle = a }, box), commit: false);
            angleText.Text = $"{a:+0.0;-0.0;0.0}°";
            angleLabel.Visibility = Visibility.Visible;
            Canvas.SetLeft(angleLabel, p.X + 16);
            Canvas.SetTop(angleLabel, p.Y - 34);
            return;
        }

        var shift = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift)
            .HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        float[] result;
        if (dragKind == Move)
        {
            // The photo follows the pointer under a box that stays where it is: the box moves the
            // other way in the frame, and the view follows the box.
            float[] want = [s[0] - dx, s[1] - dy, s[2] - dx, s[3] - dy];
            result = Engine.ConstrainCrop(fw, fh, angle, Engine.Constrain.Slide, current, want);
            viewCenter = (dragCenter.X + (result[0] + result[2] - s[0] - s[2]) / 2, dragCenter.Y + (result[1] + result[3] - s[1] - s[3]) / 2);
            engine.SetCropView(viewCenter.X, viewCenter.Y);
        }
        else if (aspectLocked || shift)
        {
            var ratio = (float)(aspectLocked ? aspect ?? dragAspect : dragAspect);
            result = Scale(s, dx, dy, ratio, fw, fh, angle, current);
        }
        else
        {
            float[] want = [..s];
            if ((dragKind & Left) != 0) want[0] += dx;
            if ((dragKind & Right) != 0) want[2] += dx;
            if ((dragKind & Top) != 0) want[1] += dy;
            if ((dragKind & Bottom) != 0) want[3] += dy;
            result = Engine.ConstrainCrop(fw, fh, angle, Engine.Constrain.Edges, current, want);
        }
        Change(ed => WithCrop(ed, result), commit: false);
    }

    /// <summary>A resize that keeps the box's shape: the opposite corner (or edge) stays put.</summary>
    float[] Scale(float[] s, float dx, float dy, float ratio, float fw, float fh, float angle, float[] current)
    {
        const float minPixels = 16;
        var left = (dragKind & Left) != 0;
        var right = (dragKind & Right) != 0;
        var top = (dragKind & Top) != 0;
        var bottom = (dragKind & Bottom) != 0;
        float wpx, hpx, ax, ay;
        float[] want;
        if ((left || right) && (top || bottom))
        {
            // A corner: the opposite one is the anchor.
            ax = left ? s[2] : s[0];
            ay = top ? s[3] : s[1];
            var cx = (left ? s[0] : s[2]) + dx;
            var cy = (top ? s[1] : s[3]) + dy;
            wpx = Math.Max(minPixels, Math.Abs(cx - ax) * fw);
            hpx = Math.Max(minPixels, Math.Abs(cy - ay) * fh);
            if (wpx / hpx > ratio) hpx = wpx / ratio; else wpx = hpx * ratio;
            var sx = cx == ax ? (left ? -1 : 1) : Math.Sign(cx - ax);
            var sy = cy == ay ? (top ? -1 : 1) : Math.Sign(cy - ay);
            float x2 = ax + sx * wpx / fw, y2 = ay + sy * hpx / fh;
            want = [Math.Min(ax, x2), Math.Min(ay, y2), Math.Max(ax, x2), Math.Max(ay, y2)];
        }
        else if (left || right)
        {
            // A side: the other side is the anchor, and the height follows about the middle.
            ax = left ? s[2] : s[0];
            ay = (s[1] + s[3]) / 2;
            var edge = (left ? s[0] : s[2]) + dx;
            wpx = Math.Max(minPixels, Math.Abs(edge - ax) * fw);
            hpx = wpx / ratio;
            var side = (left ? -1 : 1) * wpx / fw;
            want = [Math.Min(ax, ax + side), ay - hpx / fh / 2, Math.Max(ax, ax + side), ay + hpx / fh / 2];
        }
        else
        {
            ax = (s[0] + s[2]) / 2;
            ay = top ? s[3] : s[1];
            var edge = (top ? s[1] : s[3]) + dy;
            hpx = Math.Max(minPixels, Math.Abs(edge - ay) * fh);
            wpx = hpx * ratio;
            var side = (top ? -1 : 1) * hpx / fh;
            want = [ax - wpx / fw / 2, Math.Min(ay, ay + side), ax + wpx / fw / 2, Math.Max(ay, ay + side)];
        }
        return Engine.ConstrainCrop(fw, fh, angle, Engine.Constrain.Scale, current, want, ax, ay);
    }

    /// <summary>The grid shown while dragging: dense while turning, thirds otherwise, none when null.</summary>
    void SetGrid(bool? turning)
    {
        foreach (var t in thirds)
            t.Visibility = turning == false ? Visibility.Visible : Visibility.Collapsed;
        foreach (var t in fine)
            t.Visibility = turning == true ? Visibility.Visible : Visibility.Collapsed;
        if (turning != true)
            angleLabel.Visibility = Visibility.Collapsed;
    }

    /// <summary>The cursor and the turn glyph for the pointer over the overlay.</summary>
    void ShowHover(Point p, Rect f)
    {
        var kind = HitTest(p, CropRect(f, CropOf(edit)));
        var shape = kind switch
        {
            Left | Top or Right | Bottom => InputSystemCursorShape.SizeNorthwestSoutheast,
            Right | Top or Left | Bottom => InputSystemCursorShape.SizeNortheastSouthwest,
            Left or Right => InputSystemCursorShape.SizeWestEast,
            Top or Bottom => InputSystemCursorShape.SizeNorthSouth,
            Move => InputSystemCursorShape.Hand,
            _ => InputSystemCursorShape.Cross,
        };
        CropCanvas.SetCursor(InputSystemCursor.Create(shape));
        var turning = kind == Turn;
        turnGlyph.Visibility = turning && !dragging ? Visibility.Visible : Visibility.Collapsed;
        if (turning)
        {
            Canvas.SetLeft(turnGlyph, p.X + 14);
            Canvas.SetTop(turnGlyph, p.Y + 14);
        }
    }

    void CropPointerReleased(object sender, PointerRoutedEventArgs e)
    {
        CropCanvas.ReleasePointerCapture(e.Pointer);
        EndDrag();
    }

    /// <summary>Eases the view back to the frame's middle after the photo was dragged.</summary>
    void GlideBack()
    {
        StopRecenter();
        var from = viewCenter;
        var step = 0;
        recenter = new Microsoft.UI.Xaml.DispatcherTimer { Interval = TimeSpan.FromMilliseconds(16) };
        recenter.Tick += (_, _) =>
        {
            step++;
            var t = Math.Min(1.0, step / 10.0);
            var ease = (float)(1 - (1 - t) * (1 - t));
            viewCenter = (from.X + (0.5f - from.X) * ease, from.Y + (0.5f - from.Y) * ease);
            engine.SetCropView(viewCenter.X, viewCenter.Y);
            if (t >= 1)
                StopRecenter();
        };
        recenter.Start();
    }

    void StopRecenter()
    {
        recenter?.Stop();
        recenter = null;
    }

    /// <summary>Arrow keys move the photo under the box by a screen pixel, ten with Shift.</summary>
    void NudgePhoto(int dx, int dy, bool big)
    {
        if (!cropping || last.ViewW <= 0 || last.FrameWidth == 0)
            return;
        var px = big ? 10 : 1;
        var current = CropOf(edit);
        float mx = -dx * px / last.ViewW, my = -dy * px / last.ViewH;
        float[] want = [current[0] + mx, current[1] + my, current[2] + mx, current[3] + my];
        var result = Engine.ConstrainCrop(last.FrameWidth, last.FrameHeight, edit.Angle, Engine.Constrain.Slide, current, want);
        Change(ed => WithCrop(ed, result), commit: true);
    }

    void EndDrag()
    {
        if (!dragging)
            return;
        dragging = false;
        if (dragKind == Move)
            GlideBack();
        SetGrid(null);
        CommitEdit(); // one undo step for the whole drag
    }
}
