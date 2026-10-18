using Microsoft.UI;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.Foundation;
using Windows.System;

namespace Windedit;

/// <summary>
/// The Masks panel and its handles on the photo (docs/phase2d-masks.md). The engine holds the masks
/// (masks.rs); this edits them: the panel lists them and carries the selected mask's sliders, and
/// a Canvas over the photo draws the selected shape's handles and turns drags into edits. Shapes
/// live in the raw's frame in units of its long edge, so every drag goes through the view's map
/// (<see cref="Engine.Frame.MapOriginU"/>), which follows zoom, crop, straighten and rotation.
/// </summary>
public sealed partial class MainWindow
{
    delegate void MasksEditor(ref Engine.Masks m);

    /// <summary>The masks of the photo in view, as last sent to the engine.</summary>
    Engine.Masks masks;
    bool masking;
    bool overlayOn;
    /// <summary>A mask change since the last commit.</summary>
    bool masksPending;
    bool updatingMasks;
    int selMask = -1, selComp = -1;

    // Creating a shape: the next drag on the photo makes it.
    uint creatingShape;
    uint creatingMode;
    bool creatingNewMask;

    // A drag in progress.
    enum Grab { None, Move, Rotate, LinearFull, LinearGone, RadiusX, RadiusY, Create }
    Grab mkGrab;
    Point mkStart;
    (double X, double Y) mkStartFrame;
    Engine.MaskComponent mkOrig;
    int mkDragMask, mkDragComp;
    bool mkMoved;

    const double MaskReach = 9, PinReach = 14;

    StackPanel maskListBox = null!, maskSliderBox = null!, radialBox = null!;
    ToggleSwitch overlaySwitch = null!, invertSwitch = null!;
    Slider featherSlider = null!;
    TextBlock featherValue = null!;
    readonly Slider[] maskSliders = new Slider[Engine.MaskSliders + 1];
    readonly TextBlock[] maskSliderValues = new TextBlock[Engine.MaskSliders + 1];
    TextBlock maskHint = null!;

    // Slider index 0 is Amount; i + 1 is Engine.Mask.Sliders[i].
    static readonly (string Name, double Min, double Max, double Step, double Default)[] MaskSliderSpecs =
    [
        ("Amount", 0, 100, 1, 100),
        ("Temperature", -100, 100, 1, 0), ("Tint", -100, 100, 1, 0), ("Exposure", -4, 4, 0.01, 0),
        ("Contrast", -100, 100, 1, 0), ("Highlights", -100, 100, 1, 0), ("Shadows", -100, 100, 1, 0),
        ("Whites", -100, 100, 1, 0), ("Blacks", -100, 100, 1, 0), ("Clarity", -100, 100, 1, 0),
        ("Saturation", -100, 100, 1, 0),
    ];

    // ---- The panel ----

    void BuildMaskPanel()
    {
        MaskControls.Children.Add(new TextBlock { Text = "Masks", Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"], Margin = new Thickness(4, 0, 0, 6) });
        var create = new Button { Content = "+ New mask", HorizontalAlignment = HorizontalAlignment.Stretch };
        create.Flyout = ShapeMenu(shape => StartCreating(shape, 0, newMask: true), ("Linear gradient  (M)", Engine.LinearShape), ("Radial gradient  (Shift+M)", Engine.RadialShape));
        MaskControls.Children.Add(create);
        maskListBox = new StackPanel { Spacing = 2, Margin = new Thickness(0, 6, 0, 0) };
        MaskControls.Children.Add(maskListBox);
        maskHint = new TextBlock { Opacity = 0.6, FontSize = 12, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(4, 6, 0, 0) };
        MaskControls.Children.Add(maskHint);

        overlaySwitch = new ToggleSwitch { OnContent = "", OffContent = "", MinWidth = 0 };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(overlaySwitch, "Show overlay");
        overlaySwitch.Toggled += (_, _) =>
        {
            if (!updatingMasks)
                SetOverlay(overlaySwitch.IsOn);
            FocusKeys();
        };
        var overlayRow = new Grid { Margin = new Thickness(0, 8, 0, 0) };
        overlayRow.Children.Add(new TextBlock { Text = "Show overlay  (O)", FontSize = 13, VerticalAlignment = VerticalAlignment.Center });
        overlaySwitch.HorizontalAlignment = HorizontalAlignment.Right;
        overlayRow.Children.Add(overlaySwitch);
        MaskControls.Children.Add(overlayRow);

        maskSliderBox = new StackPanel { Spacing = 2 };
        for (var i = 0; i < maskSliders.Length; i++)
            AddMaskSlider(i);

        // The selected radial shape's own settings.
        radialBox = new StackPanel { Spacing = 2 };
        featherSlider = new Slider { Minimum = 0, Maximum = 100, StepFrequency = 1, SmallChange = 1, LargeChange = 5, IsThumbToolTipEnabled = false };
        featherValue = new TextBlock { HorizontalAlignment = HorizontalAlignment.Right, FontSize = 13, Opacity = 0.75 };
        var featherLabel = new Grid { Margin = new Thickness(0, 4, 0, -6) };
        featherLabel.Children.Add(new TextBlock { Text = "Feather", FontSize = 13 });
        featherLabel.Children.Add(featherValue);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(featherSlider, "Feather");
        featherSlider.ValueChanged += (_, e) =>
        {
            featherValue.Text = $"{e.NewValue:0}";
            if (!updatingMasks)
                ChangeMasks((ref Engine.Masks m) => m.Items[selMask].Components[selComp].P[5] = (float)e.NewValue, commit: false);
        };
        featherSlider.DoubleTapped += (_, _) => ChangeMasks((ref Engine.Masks m) => m.Items[selMask].Components[selComp].P[5] = 50, commit: true);
        WireMaskCommit(featherSlider);
        invertSwitch = new ToggleSwitch { OnContent = "", OffContent = "", MinWidth = 0 };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(invertSwitch, "Invert");
        invertSwitch.Toggled += (_, _) =>
        {
            if (!updatingMasks && selMask >= 0 && selComp >= 0)
                ChangeMasks((ref Engine.Masks m) => m.Items[selMask].Components[selComp].P[6] = invertSwitch.IsOn ? 1 : 0, commit: true);
            FocusKeys();
        };
        var invertRow = new Grid { Margin = new Thickness(0, 4, 0, 0) };
        invertRow.Children.Add(new TextBlock { Text = "Invert", FontSize = 13, VerticalAlignment = VerticalAlignment.Center });
        invertSwitch.HorizontalAlignment = HorizontalAlignment.Right;
        invertRow.Children.Add(invertSwitch);
        radialBox.Children.Add(featherLabel);
        radialBox.Children.Add(featherSlider);
        radialBox.Children.Add(invertRow);
        // The shape's settings come before the mask's sliders, as the shape is what was just made.
        MaskControls.Children.Add(radialBox);
        MaskControls.Children.Add(maskSliderBox);

        var reset = new Button { Content = "Reset mask", Margin = new Thickness(0, 12, 0, 0), HorizontalAlignment = HorizontalAlignment.Stretch };
        reset.Click += (_, _) =>
        {
            if (selMask >= 0)
                ChangeMasks((ref Engine.Masks m) =>
                {
                    m.Items[selMask].Amount = 100;
                    for (var i = 0; i < Engine.MaskSliders; i++)
                        m.Items[selMask].Sliders[i] = 0;
                }, commit: true);
            FocusKeys();
        };
        maskSliderBox.Children.Add(reset);
        var done = new Button { Content = "Done  (Shift+W)", Margin = new Thickness(0, 8, 0, 0), HorizontalAlignment = HorizontalAlignment.Stretch };
        done.Click += (_, _) =>
        {
            ExitMasks();
            FocusKeys();
        };
        MaskControls.Children.Add(done);
        MaskControls.Children.Add(new TextBlock
        {
            Text = "Drag on the photo to make a shape. Delete removes the selected shape. Double-click a slider to reset it.",
            Opacity = 0.55, FontSize = 12, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 8, 0, 0),
        });
    }

    /// <summary>A menu of shapes; choosing one calls <paramref name="chosen"/>.</summary>
    static MenuFlyout ShapeMenu(Action<uint> chosen, params (string Name, uint Shape)[] shapes)
    {
        var menu = new MenuFlyout();
        foreach (var (name, shape) in shapes)
        {
            var item = new MenuFlyoutItem { Text = name };
            item.Click += (_, _) => chosen(shape);
            menu.Items.Add(item);
        }
        return menu;
    }

    void AddMaskSlider(int i)
    {
        var (name, min, max, step, def) = MaskSliderSpecs[i];
        var label = new Grid { Margin = new Thickness(0, 4, 0, -6) };
        label.Children.Add(new TextBlock { Text = name, FontSize = 13 });
        var value = maskSliderValues[i] = new TextBlock { HorizontalAlignment = HorizontalAlignment.Right, FontSize = 13, Opacity = 0.75 };
        label.Children.Add(value);
        var slider = maskSliders[i] = new Slider
        {
            Minimum = min, Maximum = max, StepFrequency = step, SmallChange = step, LargeChange = (max - min) / 20, IsThumbToolTipEnabled = false,
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(slider, name);
        slider.ValueChanged += (_, e) =>
        {
            value.Text = FormatMaskValue(i, e.NewValue);
            if (!updatingMasks && selMask >= 0)
                ChangeMasks((ref Engine.Masks m) => SetMaskValue(ref m, selMask, i, (float)e.NewValue), commit: false);
        };
        slider.DoubleTapped += (_, _) =>
        {
            if (selMask >= 0)
                ChangeMasks((ref Engine.Masks m) => SetMaskValue(ref m, selMask, i, (float)def), commit: true);
        };
        WireMaskCommit(slider);
        maskSliderBox.Children.Add(label);
        maskSliderBox.Children.Add(slider);
    }

    static string FormatMaskValue(int i, double v) => i switch
    {
        0 => $"{v:0}%",
        3 => $"{v:+0.00;-0.00;0.00}",
        _ => $"{v:+0;-0;0}",
    };

    static float GetMaskValue(in Engine.Masks m, int mask, int i) => i == 0 ? m.Items[mask].Amount : m.Items[mask].Sliders[i - 1];

    static void SetMaskValue(ref Engine.Masks m, int mask, int i, float v)
    {
        if (i == 0)
            m.Items[mask].Amount = v;
        else
            m.Items[mask].Sliders[i - 1] = v;
    }

    /// <summary>The end of a drag (or a key press) on a slider commits what it changed.</summary>
    void WireMaskCommit(Slider slider)
    {
        slider.AddHandler(UIElement.PointerReleasedEvent, new PointerEventHandler((_, _) => CommitMasks()), true);
        slider.AddHandler(UIElement.PointerCaptureLostEvent, new PointerEventHandler((_, _) => CommitMasks()), true);
        slider.KeyUp += (_, _) => CommitMasks();
    }

    /// <summary>The mask list and the controls under it, from <see cref="masks"/> and the selection.</summary>
    void RebuildMaskPanel()
    {
        updatingMasks = true;
        maskListBox.Children.Clear();
        for (var i = 0; i < masks.Count; i++)
        {
            var index = i;
            var selected = i == selMask;
            var row = new Grid { ColumnSpacing = 2 };
            row.ColumnDefinitions.Add(new ColumnDefinition());
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var button = RowButton($"Mask {i + 1}", selected, bold: true);
            button.Click += (_, _) => SelectMask(index, masks.Items[index].Count > 0 ? 0 : -1);
            var delete = IconButton("\uE74D", "Delete the mask");
            delete.Click += (_, _) => DeleteMask(index);
            row.Children.Add(button);
            Grid.SetColumn(delete, 1);
            row.Children.Add(delete);
            maskListBox.Children.Add(row);
            if (!selected)
                continue;
            for (var j = 0; j < masks.Items[i].Count; j++)
            {
                var comp = j;
                var c = masks.Items[i].Components[j];
                var crow = new Grid { ColumnSpacing = 2, Margin = new Thickness(14, 0, 0, 0) };
                crow.ColumnDefinitions.Add(new ColumnDefinition());
                crow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                crow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                var shape = c.Kind == Engine.LinearShape ? "Linear gradient" : "Radial gradient";
                var pick = RowButton(shape, j == selComp, bold: false);
                pick.Click += (_, _) => SelectMask(index, comp);
                var mode = new Button { Content = c.Mode == Engine.SubtractMode ? "Subtract" : "Add", MinWidth = 0, Padding = new Thickness(8, 3, 8, 3), FontSize = 12 };
                ToolTipService.SetToolTip(mode, "Switch between adding to and subtracting from the mask");
                mode.Click += (_, _) => ChangeMasks((ref Engine.Masks m) => m.Items[index].Components[comp].Mode ^= 1, commit: true);
                var remove = IconButton("\uE74D", "Delete the shape (Delete)");
                remove.Click += (_, _) => DeleteComponent(index, comp);
                crow.Children.Add(pick);
                Grid.SetColumn(mode, 1);
                crow.Children.Add(mode);
                Grid.SetColumn(remove, 2);
                crow.Children.Add(remove);
                maskListBox.Children.Add(crow);
            }
            var adds = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, Margin = new Thickness(14, 2, 0, 4) };
            foreach (var (text, subtract) in new[] { ("Add", false), ("Subtract", true) })
            {
                var button2 = new Button { Content = text + " ▾", FontSize = 12, Padding = new Thickness(10, 3, 10, 3) };
                button2.IsEnabled = masks.Items[i].Count < Engine.MaxComponents;
                button2.Flyout = ShapeMenu(shape => StartCreating(shape, subtract ? Engine.SubtractMode : 0, newMask: false),
                    ("Linear gradient", Engine.LinearShape), ("Radial gradient", Engine.RadialShape));
                adds.Children.Add(button2);
            }
            maskListBox.Children.Add(adds);
        }
        maskHint.Text = creatingShape != 0 ? "Drag on the photo to make the shape · Esc cancels"
            : masks.Count == 0 ? "No masks. Choose + New mask, or press M (linear) or Shift+M (radial)." : "";

        var has = selMask >= 0 && selMask < masks.Count;
        maskSliderBox.Visibility = has ? Visibility.Visible : Visibility.Collapsed;
        var radial = has && selComp >= 0 && selComp < masks.Items[selMask].Count && masks.Items[selMask].Components[selComp].Kind == Engine.RadialShape;
        radialBox.Visibility = radial ? Visibility.Visible : Visibility.Collapsed;
        if (radial)
        {
            var c = masks.Items[selMask].Components[selComp];
            featherSlider.Value = c.P[5];
            featherValue.Text = $"{c.P[5]:0}";
            invertSwitch.IsOn = c.P[6] != 0;
        }
        if (has)
            for (var i = 0; i < maskSliders.Length; i++)
            {
                var v = GetMaskValue(in masks, selMask, i);
                maskSliders[i].Value = v;
                maskSliderValues[i].Text = FormatMaskValue(i, maskSliders[i].Value);
            }
        overlaySwitch.IsOn = overlayOn;
        updatingMasks = false;
    }

    static Button RowButton(string text, bool selected, bool bold)
    {
        var b = new Button
        {
            Content = text, HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Left,
            Padding = new Thickness(8, 4, 8, 4), FontWeight = bold ? Microsoft.UI.Text.FontWeights.SemiBold : Microsoft.UI.Text.FontWeights.Normal,
            BorderThickness = new Thickness(selected ? 1.5 : 0),
        };
        if (selected)
            b.BorderBrush = (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"];
        else
            b.Background = new SolidColorBrush(Colors.Transparent);
        return b;
    }

    static Button IconButton(string glyph, string tip)
    {
        var b = new Button { Content = new FontIcon { Glyph = glyph, FontSize = 12 }, Padding = new Thickness(7, 5, 7, 5), MinWidth = 0, MinHeight = 0, Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0) };
        ToolTipService.SetToolTip(b, tip);
        return b;
    }

    // ---- Entering and leaving ----

    void Masks_Click(object sender, RoutedEventArgs e)
    {
        if (MasksToggle.IsChecked == true)
            EnterMasks();
        else
            ExitMasks();
        FocusKeys();
    }

    void EnterMasks()
    {
        if (CurrentPhoto == null || gridMode)
        {
            MasksToggle.IsChecked = false;
            return;
        }
        if (masking)
            return;
        SetPicking(false);
        ExitCrop(cancel: false);
        if (!editMode)
            SetEditMode(true);
        CommitEdit();
        masking = true;
        MasksToggle.IsChecked = true;
        EditControls.Visibility = Visibility.Collapsed;
        MaskControls.Visibility = Visibility.Visible;
        MaskCanvas.Visibility = Visibility.Visible;
        selMask = masks.Count > 0 ? Math.Clamp(selMask, 0, (int)masks.Count - 1) : -1;
        selComp = selMask >= 0 && masks.Items[selMask].Count > 0 ? 0 : -1;
        RebuildMaskPanel();
        UpdateMaskOverlay();
        SyncMaskOverlay();
        FocusKeys();
    }

    /// <summary>Leaves the Masks panel: the handles go, the masks still apply.</summary>
    void ExitMasks()
    {
        if (!masking)
            return;
        CommitMasks();
        creatingShape = 0;
        mkGrab = Grab.None;
        masking = false;
        MasksToggle.IsChecked = false;
        MaskControls.Visibility = Visibility.Collapsed;
        EditControls.Visibility = Visibility.Visible;
        MaskCanvas.Visibility = Visibility.Collapsed;
        MaskCanvas.Children.Clear();
        engine.SetMaskOverlay(-1);
    }

    /// <summary>The masks of the photo now in view.</summary>
    void LoadMasks()
    {
        masks = CurrentPhoto is { } p ? engine.GetMasks(p.Index) : default;
        selMask = masks.Count > 0 ? 0 : -1;
        selComp = selMask >= 0 && masks.Items[selMask].Count > 0 ? 0 : -1;
        creatingShape = 0;
        if (masking)
        {
            RebuildMaskPanel();
            UpdateMaskOverlay();
            SyncMaskOverlay();
        }
    }

    /// <summary>The masks changed under the panel (undo and redo): fix the selection and redraw.</summary>
    void MasksChanged()
    {
        if (selMask >= masks.Count)
            selMask = (int)masks.Count - 1;
        if (selMask >= 0 && selComp >= masks.Items[selMask].Count)
            selComp = (int)masks.Items[selMask].Count - 1;
        if (selMask >= 0 && selComp < 0 && masks.Items[selMask].Count > 0)
            selComp = 0;
        if (!masking)
            return;
        RebuildMaskPanel();
        UpdateMaskOverlay();
        SyncMaskOverlay();
    }

    // ---- Keys ----

    /// <summary>Shift+W, M, Shift+M, O, Delete and Esc for masks. Returns whether the key was used.</summary>
    bool MaskShortcut(VirtualKey key, bool ctrl, bool shift)
    {
        if (ctrl || gridMode || CurrentPhoto == null)
            return false;
        switch (key)
        {
            case VirtualKey.W when shift:
                if (masking) ExitMasks(); else EnterMasks();
                return true;
            case VirtualKey.M:
                EnterMasks();
                if (masking)
                    StartCreating(shift ? Engine.RadialShape : Engine.LinearShape, 0, newMask: true);
                return true;
            case VirtualKey.O when masking:
                SetOverlay(!overlayOn);
                return true;
            case VirtualKey.Delete or VirtualKey.Back when masking && selMask >= 0 && selComp >= 0:
                DeleteComponent(selMask, selComp);
                return true;
            case VirtualKey.Escape when masking:
                if (creatingShape != 0)
                {
                    creatingShape = 0;
                    RebuildMaskPanel();
                    UpdateMaskOverlay();
                }
                else
                    ExitMasks();
                return true;
            default:
                return false;
        }
    }

    // ---- Editing ----

    /// <summary>Applies a change to the photo's masks: shown at once, saved (one undo step) when
    /// <paramref name="commit"/> is true or <see cref="CommitMasks"/> is called.</summary>
    void ChangeMasks(MasksEditor change, bool commit)
    {
        if (CurrentPhoto is not { } p)
            return;
        CommitEdit();
        change(ref masks);
        engine.SetMasks(p.Index, in masks, commit);
        masksPending = !commit;
        if (commit)
            AfterCommit(p);
        if (masking)
        {
            // The sliders being dragged already show their values; the rest follows the data.
            if (commit)
                RebuildMaskPanel();
            UpdateMaskOverlay();
        }
    }

    void CommitMasks()
    {
        if (!masksPending || CurrentPhoto is not { } p)
            return;
        masksPending = false;
        engine.SetMasks(p.Index, in masks, commit: true);
        AfterCommit(p);
        if (masking)
            RebuildMaskPanel();
    }

    void SelectMask(int mask, int comp)
    {
        selMask = mask;
        selComp = comp;
        RebuildMaskPanel();
        UpdateMaskOverlay();
        SyncMaskOverlay();
        FocusKeys();
    }

    void DeleteMask(int mask)
    {
        ChangeMasks((ref Engine.Masks m) =>
        {
            for (var i = mask; i < Engine.MaxMasks - 1; i++)
                m.Items[i] = m.Items[i + 1];
            m.Items[Engine.MaxMasks - 1] = default;
            m.Count--;
        }, commit: true);
        selMask = masks.Count == 0 ? -1 : Math.Min(mask, (int)masks.Count - 1);
        selComp = selMask >= 0 && masks.Items[selMask].Count > 0 ? 0 : -1;
        RebuildMaskPanel();
        UpdateMaskOverlay();
        SyncMaskOverlay();
        FocusKeys();
    }

    /// <summary>Removes a shape; the last one takes the whole mask with it.</summary>
    void DeleteComponent(int mask, int comp)
    {
        if (masks.Items[mask].Count <= 1)
        {
            DeleteMask(mask);
            return;
        }
        ChangeMasks((ref Engine.Masks m) =>
        {
            ref var target = ref m.Items[mask];
            for (var j = comp; j < Engine.MaxComponents - 1; j++)
                target.Components[j] = target.Components[j + 1];
            target.Components[Engine.MaxComponents - 1] = default;
            target.Count--;
        }, commit: true);
        selComp = Math.Min(comp, (int)masks.Items[mask].Count - 1);
        RebuildMaskPanel();
        UpdateMaskOverlay();
        FocusKeys();
    }

    void SetOverlay(bool on)
    {
        overlayOn = on;
        updatingMasks = true;
        overlaySwitch.IsOn = on;
        updatingMasks = false;
        SyncMaskOverlay();
    }

    /// <summary>Tells the engine which mask to draw as the red overlay: the selected one, while the
    /// overlay is on or a handle is being dragged.</summary>
    void SyncMaskOverlay() =>
        engine.SetMaskOverlay(masking && selMask >= 0 && (overlayOn || mkGrab != Grab.None) ? selMask : -1);

    void StartCreating(uint shape, uint mode, bool newMask)
    {
        EnterMasks();
        if (!masking)
            return;
        if (newMask ? masks.Count >= Engine.MaxMasks : selMask < 0 || masks.Items[selMask].Count >= Engine.MaxComponents)
        {
            ShowMessage("There is no room for another " + (newMask ? "mask" : "shape") + ".", timed: true);
            return;
        }
        creatingShape = shape;
        creatingMode = mode;
        creatingNewMask = newMask;
        MaskCanvas.SetCursor(InputSystemCursor.Create(InputSystemCursorShape.Cross));
        RebuildMaskPanel();
        FocusKeys();
    }

    // ---- The view's map ----

    /// <summary>Whether the handles can be placed: the raw is decoded and its frame has been drawn.</summary>
    bool MapReady(out Rect picture)
    {
        var scale = Panel.CompositionScaleX;
        picture = new Rect(last.ViewX / scale, last.ViewY / scale, last.ViewW / scale, last.ViewH / scale);
        return masking && !cropping && CurrentPhoto?.Index == last.Photo && last.RawWidth > 0 && last.ViewW > 0
            && Math.Abs(last.MapStepXU * last.MapStepYV - last.MapStepXV * last.MapStepYU) > 1e-18;
    }

    /// <summary>A frame position (units of the raw's long edge) on the canvas.</summary>
    Point FrameToCanvas(double x, double y)
    {
        double w = last.RawWidth, h = last.RawHeight, edge = Math.Max(w, h);
        double du = x * edge / w - last.MapOriginU, dv = y * edge / h - last.MapOriginV;
        double sxu = last.MapStepXU, sxv = last.MapStepXV, syu = last.MapStepYU, syv = last.MapStepYV;
        var det = sxu * syv - syu * sxv;
        var a = (du * syv - dv * syu) / det;
        var b = (sxu * dv - sxv * du) / det;
        var scale = Panel.CompositionScaleX;
        return new Point(a / scale, b / scale);
    }

    (double X, double Y) CanvasToFrame(Point c)
    {
        double w = last.RawWidth, h = last.RawHeight, edge = Math.Max(w, h);
        var scale = Panel.CompositionScaleX;
        double px = c.X * scale, py = c.Y * scale;
        var u = last.MapOriginU + px * last.MapStepXU + py * last.MapStepYU;
        var v = last.MapOriginV + px * last.MapStepXV + py * last.MapStepYV;
        return (u * w / edge, v * h / edge);
    }

    Point FrameToCanvas((double X, double Y) p) => FrameToCanvas(p.X, p.Y);

    /// <summary>The angle (frame radians) at which the view's rightwards points: a new radial shape's
    /// own x axis follows the screen, whatever way the photo is turned.</summary>
    double ScreenAngle()
    {
        var a = CanvasToFrame(new Point(0, 0));
        var b = CanvasToFrame(new Point(100, 0));
        return Math.Atan2(b.Y - a.Y, b.X - a.X);
    }

    // ---- The overlay ----

    static readonly SolidColorBrush HandleWhite = new(Colors.White);
    static readonly SolidColorBrush HandleShadow = new(Windows.UI.Color.FromArgb(0xC0, 0, 0, 0));
    static readonly SolidColorBrush HandleAccent = new(Windows.UI.Color.FromArgb(0xFF, 0xFF, 0xC8, 0x40));

    void InitMasks()
    {
        MaskCanvas.PointerPressed += MaskPointerPressed;
        MaskCanvas.PointerMoved += MaskPointerMoved;
        MaskCanvas.PointerReleased += MaskPointerReleased;
        MaskCanvas.PointerCaptureLost += (_, _) => EndMaskDrag();
        MaskCanvas.SizeChanged += (_, _) => UpdateMaskOverlay();
    }

    /// <summary>Draws the pins of every shape and the handles of the selected one.</summary>
    void UpdateMaskOverlay()
    {
        if (!masking)
            return;
        MaskCanvas.Children.Clear();
        var ready = MapReady(out var picture);
        MaskCanvas.Opacity = ready ? 1 : 0;
        if (!ready)
            return;
        MaskCanvas.Clip = new RectangleGeometry { Rect = picture };
        for (var i = 0; i < masks.Count; i++)
            for (var j = 0; j < masks.Items[i].Count; j++)
            {
                var c = masks.Items[i].Components[j];
                var selected = i == selMask && j == selComp;
                if (selected)
                    DrawHandles(c, picture);
                var pin = PinOf(c);
                var at = FrameToCanvas(pin);
                var r = selected ? 7 : i == selMask ? 5 : 4;
                AddShape(new Ellipse { Width = 2 * r, Height = 2 * r, Fill = selected ? HandleWhite : HandleShadow, Stroke = selected ? HandleShadow : HandleWhite, StrokeThickness = 1.5 }, at.X - r, at.Y - r);
            }
    }

    void AddShape(UIElement e, double x, double y)
    {
        e.IsHitTestVisible = false;
        Canvas.SetLeft(e, x);
        Canvas.SetTop(e, y);
        MaskCanvas.Children.Add(e);
    }

    void AddLine(Point a, Point b, Brush brush, double width, DoubleCollection? dashes = null)
    {
        var l = new Line { X1 = a.X, Y1 = a.Y, X2 = b.X, Y2 = b.Y, Stroke = brush, StrokeThickness = width, IsHitTestVisible = false };
        if (dashes != null)
            l.StrokeDashArray = dashes;
        MaskCanvas.Children.Add(l);
    }

    /// <summary>Where a shape's pin sits, in frame coordinates: a gradient's middle, an ellipse's centre.</summary>
    static (double X, double Y) PinOf(Engine.MaskComponent c) =>
        c.Kind == Engine.LinearShape ? ((c.P[0] + c.P[2]) / 2, (c.P[1] + c.P[3]) / 2) : (c.P[0], c.P[1]);

    void DrawHandles(Engine.MaskComponent c, Rect picture)
    {
        if (c.Kind == Engine.LinearShape)
        {
            var a = FrameToCanvas(c.P[0], c.P[1]);
            var b = FrameToCanvas(c.P[2], c.P[3]);
            var mid = new Point((a.X + b.X) / 2, (a.Y + b.Y) / 2);
            var (nx, ny) = Unit(b.X - a.X, b.Y - a.Y);
            var (tx, ty) = (-ny, nx);
            var reach = picture.Width + picture.Height;
            foreach (var (k, width, dashed) in new[] { (a, 1.5, false), (mid, 1.0, true), (b, 1.5, false) })
            {
                var p = new Point(k.X - tx * reach, k.Y - ty * reach);
                var q = new Point(k.X + tx * reach, k.Y + ty * reach);
                AddLine(p, q, HandleShadow, width + 2);
                AddLine(p, q, HandleWhite, width, dashed ? new DoubleCollection { 4, 4 } : null);
            }
            return;
        }
        // Radial: the ellipse, the feather's, and a handle on each axis.
        double cx = c.P[0], cy = c.P[1], rx = c.P[2], ry = c.P[3], theta = c.P[4];
        var (sin, cos) = Math.SinCos(theta);
        Point On(double fx, double fy) => FrameToCanvas(cx + fx * cos - fy * sin, cy + fx * sin + fy * cos);
        void Ring(double scale, Brush brush, double width, DoubleCollection? dashes)
        {
            var points = new PointCollection();
            for (var i = 0; i <= 72; i++)
            {
                var t = i * Math.PI / 36;
                points.Add(On(rx * scale * Math.Cos(t), ry * scale * Math.Sin(t)));
            }
            var poly = new Polyline { Points = points, Stroke = brush, StrokeThickness = width, IsHitTestVisible = false };
            if (dashes != null)
                poly.StrokeDashArray = dashes;
            MaskCanvas.Children.Add(poly);
        }
        Ring(1, HandleShadow, 3.5, null);
        Ring(1, HandleWhite, 1.5, null);
        var inner = 1 - Math.Max(c.P[5] / 100, 0.005);
        if (inner > 0.02)
            Ring(inner, HandleWhite, 1, new DoubleCollection { 4, 4 });
        foreach (var (hx, hy) in new[] { (rx, 0.0), (-rx, 0.0), (0.0, ry), (0.0, -ry) })
        {
            var at = On(hx, hy);
            AddShape(new Rectangle { Width = 9, Height = 9, Fill = HandleWhite, Stroke = HandleShadow, StrokeThickness = 1 }, at.X - 4.5, at.Y - 4.5);
        }
    }

    static (double X, double Y) Unit(double x, double y)
    {
        var len = Math.Sqrt(x * x + y * y);
        return len < 1e-9 ? (0, 1) : (x / len, y / len);
    }

    // ---- Dragging ----

    static double Dist(Point a, Point b) => Math.Sqrt((a.X - b.X) * (a.X - b.X) + (a.Y - b.Y) * (a.Y - b.Y));

    /// <summary>What the pointer is on: another shape's pin selects it; the selected shape's handles,
    /// body and outside move, resize and turn it.</summary>
    (Grab Grab, int Mask, int Comp) HitTest(Point p)
    {
        if (selMask >= 0 && selComp >= 0 && selMask < masks.Count && selComp < masks.Items[selMask].Count)
        {
            var c = masks.Items[selMask].Components[selComp];
            if (Dist(p, FrameToCanvas(PinOf(c))) <= PinReach)
                return (Grab.Move, selMask, selComp);
            if (c.Kind == Engine.LinearShape)
            {
                var a = FrameToCanvas(c.P[0], c.P[1]);
                var b = FrameToCanvas(c.P[2], c.P[3]);
                var mid = new Point((a.X + b.X) / 2, (a.Y + b.Y) / 2);
                var (nx, ny) = Unit(b.X - a.X, b.Y - a.Y);
                var s = (p.X - mid.X) * nx + (p.Y - mid.Y) * ny;
                var half = Dist(a, b) / 2;
                if (Math.Abs(s + half) <= MaskReach) return (Grab.LinearFull, selMask, selComp);
                if (Math.Abs(s - half) <= MaskReach) return (Grab.LinearGone, selMask, selComp);
                if (Math.Abs(s) <= half + MaskReach) return (Grab.Move, selMask, selComp);
                return (Grab.Rotate, selMask, selComp);
            }
            else
            {
                double cx = c.P[0], cy = c.P[1], rx = c.P[2], ry = c.P[3];
                var (sin, cos) = Math.SinCos(c.P[4]);
                Point On(double fx, double fy) => FrameToCanvas(cx + fx * cos - fy * sin, cy + fx * sin + fy * cos);
                if (Dist(p, On(rx, 0)) <= MaskReach + 3 || Dist(p, On(-rx, 0)) <= MaskReach + 3) return (Grab.RadiusX, selMask, selComp);
                if (Dist(p, On(0, ry)) <= MaskReach + 3 || Dist(p, On(0, -ry)) <= MaskReach + 3) return (Grab.RadiusY, selMask, selComp);
                var f = CanvasToFrame(p);
                double dx = f.X - cx, dy = f.Y - cy;
                var u = (dx * cos + dy * sin) / rx;
                var v = (-dx * sin + dy * cos) / ry;
                return (u * u + v * v <= 1 ? Grab.Move : Grab.Rotate, selMask, selComp);
            }
        }
        return (Grab.None, -1, -1);
    }

    /// <summary>Another shape's pin under the pointer, if any.</summary>
    (int Mask, int Comp)? PinAt(Point p)
    {
        for (var i = 0; i < masks.Count; i++)
            for (var j = 0; j < masks.Items[i].Count; j++)
                if (!(i == selMask && j == selComp) && Dist(p, FrameToCanvas(PinOf(masks.Items[i].Components[j]))) <= PinReach)
                    return (i, j);
        return null;
    }

    void MaskPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (!MapReady(out _) || !e.GetCurrentPoint(MaskCanvas).Properties.IsLeftButtonPressed)
            return;
        var p = e.GetCurrentPoint(MaskCanvas).Position;
        e.Handled = true;
        FocusKeys();
        if (creatingShape != 0)
        {
            BeginCreate(p);
        }
        else
        {
            // The selected shape's own pin and handles win over a neighbour's pin.
            var hit = HitTest(p);
            if (PinAt(p) is { } pin && !OnOwnPin(p) && hit.Grab is Grab.None or Grab.Rotate or Grab.Move)
            {
                SelectMask(pin.Mask, pin.Comp);
                return;
            }
            if (hit.Grab == Grab.None)
                return;
            mkGrab = hit.Grab;
            mkDragMask = hit.Mask;
            mkDragComp = hit.Comp;
            mkOrig = masks.Items[hit.Mask].Components[hit.Comp];
        }
        mkStart = p;
        mkStartFrame = CanvasToFrame(p);
        mkMoved = false;
        MaskCanvas.CapturePointer(e.Pointer);
        SyncMaskOverlay();
    }

    /// <summary>Whether the pointer is on the selected shape's own pin.</summary>
    bool OnOwnPin(Point p) =>
        selMask >= 0 && selComp >= 0 && selMask < masks.Count && selComp < masks.Items[selMask].Count
        && Dist(p, FrameToCanvas(PinOf(masks.Items[selMask].Components[selComp]))) <= PinReach;

    void BeginCreate(Point p)
    {
        var shape = creatingShape;
        var mode = creatingMode;
        var frame = CanvasToFrame(p);
        var comp = new Engine.MaskComponent { Kind = shape, Mode = mode };
        if (shape == Engine.LinearShape)
        {
            (comp.P[0], comp.P[1], comp.P[2], comp.P[3]) = ((float)frame.X, (float)frame.Y, (float)frame.X, (float)frame.Y);
        }
        else
        {
            comp.P[0] = (float)frame.X;
            comp.P[1] = (float)frame.Y;
            comp.P[2] = comp.P[3] = 0.001f;
            comp.P[4] = (float)ScreenAngle();
            comp.P[5] = 50;
        }
        int mask, index;
        if (creatingNewMask)
        {
            mask = (int)masks.Count;
            index = 0;
        }
        else
        {
            mask = selMask;
            index = (int)masks.Items[mask].Count;
        }
        ChangeMasks((ref Engine.Masks m) =>
        {
            if (creatingNewMask)
            {
                m.Items[mask] = default;
                m.Items[mask].Amount = 100;
                m.Count++;
            }
            m.Items[mask].Components[index] = comp;
            m.Items[mask].Count++;
        }, commit: false);
        selMask = mask;
        selComp = index;
        mkGrab = Grab.Create;
        mkDragMask = mask;
        mkDragComp = index;
        mkOrig = comp;
        creatingShape = 0;
        UpdateMaskOverlay();
    }

    void MaskPointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (!MapReady(out _))
            return;
        var p = e.GetCurrentPoint(MaskCanvas).Position;
        if (mkGrab == Grab.None)
        {
            ShowMaskHover(p);
            return;
        }
        if (Dist(p, mkStart) > 3)
            mkMoved = true;
        var shift = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        var o = mkOrig;
        var now = CanvasToFrame(p);
        double dx = now.X - mkStartFrame.X, dy = now.Y - mkStartFrame.Y;
        var c = o;
        switch (mkGrab)
        {
            case Grab.Create:
                if (o.Kind == Engine.LinearShape)
                {
                    c.P[2] = (float)now.X;
                    c.P[3] = (float)now.Y;
                }
                else
                {
                    var theta = o.P[4];
                    var (sin, cos) = Math.SinCos(theta);
                    double ex = now.X - o.P[0], ey = now.Y - o.P[1];
                    double along = Math.Abs(ex * cos + ey * sin), across = Math.Abs(-ex * sin + ey * cos);
                    var round = Math.Sqrt(ex * ex + ey * ey);
                    c.P[2] = (float)Math.Max(shift ? round : along, 0.002);
                    c.P[3] = (float)Math.Max(shift ? round : across, 0.002);
                }
                break;
            case Grab.Move:
                if (o.Kind == Engine.LinearShape)
                {
                    c.P[0] += (float)dx; c.P[1] += (float)dy; c.P[2] += (float)dx; c.P[3] += (float)dy;
                }
                else
                {
                    c.P[0] += (float)dx; c.P[1] += (float)dy;
                }
                break;
            case Grab.Rotate:
                {
                    var centre = FrameToCanvas(PinOf(o));
                    var turn = Math.Atan2(p.Y - centre.Y, p.X - centre.X) - Math.Atan2(mkStart.Y - centre.Y, mkStart.X - centre.X);
                    if (o.Kind == Engine.LinearShape)
                    {
                        var (sin, cos) = Math.SinCos(turn);
                        Point Turned(float fx, float fy)
                        {
                            var k = FrameToCanvas(fx, fy);
                            double rx = k.X - centre.X, ry = k.Y - centre.Y;
                            return new Point(centre.X + rx * cos - ry * sin, centre.Y + rx * sin + ry * cos);
                        }
                        var a = CanvasToFrame(Turned(o.P[0], o.P[1]));
                        var b = CanvasToFrame(Turned(o.P[2], o.P[3]));
                        (c.P[0], c.P[1], c.P[2], c.P[3]) = ((float)a.X, (float)a.Y, (float)b.X, (float)b.Y);
                    }
                    else
                    {
                        c.P[4] = (float)(o.P[4] + turn);
                    }
                    break;
                }
            case Grab.LinearFull:
            case Grab.LinearGone:
                {
                    // Along the gradient only: the other line stays, so the band widens or narrows.
                    var a = FrameToCanvas(o.P[0], o.P[1]);
                    var b = FrameToCanvas(o.P[2], o.P[3]);
                    var (nx, ny) = Unit(b.X - a.X, b.Y - a.Y);
                    var along = (p.X - mkStart.X) * nx + (p.Y - mkStart.Y) * ny;
                    if (mkGrab == Grab.LinearFull)
                    {
                        var moved = CanvasToFrame(new Point(a.X + nx * along, a.Y + ny * along));
                        (c.P[0], c.P[1]) = ((float)moved.X, (float)moved.Y);
                    }
                    else
                    {
                        var moved = CanvasToFrame(new Point(b.X + nx * along, b.Y + ny * along));
                        (c.P[2], c.P[3]) = ((float)moved.X, (float)moved.Y);
                    }
                    break;
                }
            case Grab.RadiusX:
            case Grab.RadiusY:
                {
                    var (sin, cos) = Math.SinCos(o.P[4]);
                    double ex = now.X - o.P[0], ey = now.Y - o.P[1];
                    if (mkGrab == Grab.RadiusX)
                        c.P[2] = (float)Math.Clamp(Math.Abs(ex * cos + ey * sin), 0.002, 4);
                    else
                        c.P[3] = (float)Math.Clamp(Math.Abs(-ex * sin + ey * cos), 0.002, 4);
                    if (shift)
                        c.P[2] = c.P[3] = mkGrab == Grab.RadiusX ? c.P[2] : c.P[3];
                    break;
                }
        }
        var (mask, comp) = (mkDragMask, mkDragComp);
        ChangeMasks((ref Engine.Masks m) => m.Items[mask].Components[comp] = c, commit: false);
    }

    void MaskPointerReleased(object sender, PointerRoutedEventArgs e)
    {
        MaskCanvas.ReleasePointerCapture(e.Pointer);
        EndMaskDrag();
    }

    void EndMaskDrag()
    {
        if (mkGrab == Grab.None)
            return;
        var grab = mkGrab;
        mkGrab = Grab.None;
        if (grab == Grab.Create && !mkMoved && masking)
        {
            // A click without a drag makes a shape of a usual size, as big as the view allows.
            var o = mkOrig;
            var c = o;
            if (o.Kind == Engine.LinearShape)
            {
                var screenDown = CanvasToFrame(new Point(0, 100));
                var origin = CanvasToFrame(new Point(0, 0));
                var (ux, uy) = Unit(screenDown.X - origin.X, screenDown.Y - origin.Y);
                c.P[2] = (float)(o.P[0] + ux * 0.3);
                c.P[3] = (float)(o.P[1] + uy * 0.3);
            }
            else
            {
                c.P[2] = c.P[3] = 0.12f;
            }
            var (mask, comp) = (mkDragMask, mkDragComp);
            ChangeMasks((ref Engine.Masks m) => m.Items[mask].Components[comp] = c, commit: false);
        }
        MaskCanvas.SetCursor(null);
        CommitMasks();
        SyncMaskOverlay();
        UpdateMaskOverlay();
    }

    void ShowMaskHover(Point p)
    {
        InputSystemCursorShape shape;
        if (creatingShape != 0)
            shape = InputSystemCursorShape.Cross;
        else if (PinAt(p) != null && !OnOwnPin(p))
            shape = InputSystemCursorShape.Hand;
        else
            shape = HitTest(p).Grab switch
            {
                Grab.Move => InputSystemCursorShape.SizeAll,
                Grab.Rotate => InputSystemCursorShape.Cross,
                Grab.LinearFull or Grab.LinearGone or Grab.RadiusX or Grab.RadiusY => InputSystemCursorShape.SizeNorthwestSoutheast,
                _ => InputSystemCursorShape.Arrow,
            };
        MaskCanvas.SetCursor(InputSystemCursor.Create(shape));
    }
}
