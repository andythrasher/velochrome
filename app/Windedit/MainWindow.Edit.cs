using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;

namespace Windedit;

/// <summary>
/// The Edit panel: the basic adjustments as sliders. Dragging shows the change at once without
/// saving it; letting go saves it, as one step for Ctrl+Z. Double-clicking a slider resets it.
/// </summary>
public sealed partial class MainWindow
{
    /// <summary>One slider: how it reads and writes the edit, and how it shows its value.</summary>
    sealed class Adjustment
    {
        public required string Name;
        public required double Min, Max, Step;
        public required Func<Engine.Edit, double> Get;
        public required Func<Engine.Edit, double, Engine.Edit> Set;
        public required Func<double, string> Format;
        public Slider Slider = null!;
        public TextBlock Value = null!;
    }

    // Temperature on the slider is logarithmic (2,000–50,000 K), so each step is a similar
    // change in color across the range, as in Lightroom.
    const double MinKelvin = 2000, MaxKelvin = 50000;
    static double KelvinToSlider(double k) => 100 * Math.Log(k / MinKelvin) / Math.Log(MaxKelvin / MinKelvin);
    static double SliderToKelvin(double p) => MinKelvin * Math.Pow(MaxKelvin / MinKelvin, p / 100);

    readonly List<Adjustment> adjustments = [];
    Adjustment temperature = null!, tint = null!;
    TextBlock whiteBalanceText = null!;
    /// <summary>The adjustments of the photo in view, as last sent to the engine.</summary>
    Engine.Edit edit;
    /// <summary>A slider moved since the last commit.</summary>
    bool editPending;
    /// <summary>Sliders are being set from the edit, so their changes aren't edits.</summary>
    bool updatingSliders;

    void BuildEditPanel()
    {
        Header("White balance");
        whiteBalanceText = new TextBlock { Opacity = 0.6, FontSize = 12, Margin = new Thickness(0, 0, 0, 2) };
        var asShot = new HyperlinkButton { Content = "As shot", Padding = new Thickness(4, 0, 4, 0), FontSize = 12 };
        asShot.Click += (_, _) => Change(e => e with { CustomWhiteBalance = 0 }, commit: true);
        var row = new Grid();
        row.Children.Add(whiteBalanceText);
        row.Children.Add(asShot);
        asShot.HorizontalAlignment = HorizontalAlignment.Right;
        EditControls.Children.Add(row);

        temperature = Add(new Adjustment
        {
            Name = "Temperature", Min = 0, Max = 100, Step = 0.1,
            Get = e => KelvinToSlider(e.Temperature),
            Set = (e, v) => CustomWhiteBalance(e) with { Temperature = (float)SliderToKelvin(v) },
            Format = v => $"{SliderToKelvin(v):F0} K",
        });
        tint = Add(new Adjustment
        {
            Name = "Tint", Min = -150, Max = 150, Step = 1,
            Get = e => e.Tint,
            Set = (e, v) => CustomWhiteBalance(e) with { Tint = (float)v },
            Format = v => $"{v:+0;-0;0}",
        });

        Header("Tone");
        Add(new Adjustment
        {
            Name = "Exposure", Min = -5, Max = 5, Step = 0.01,
            Get = e => e.Exposure,
            Set = (e, v) => e with { Exposure = (float)v },
            Format = v => $"{v:+0.00;-0.00;0.00}",
        });
        AddPercent("Contrast", e => e.Contrast, (e, v) => e with { Contrast = v });
        AddPercent("Highlights", e => e.Highlights, (e, v) => e with { Highlights = v });
        AddPercent("Shadows", e => e.Shadows, (e, v) => e with { Shadows = v });
        AddPercent("Whites", e => e.Whites, (e, v) => e with { Whites = v });
        AddPercent("Blacks", e => e.Blacks, (e, v) => e with { Blacks = v });

        Header("Presence");
        AddPercent("Vibrance", e => e.Vibrance, (e, v) => e with { Vibrance = v });
        AddPercent("Saturation", e => e.Saturation, (e, v) => e with { Saturation = v });

        var reset = new Button { Content = "Reset all", Margin = new Thickness(0, 16, 0, 0), HorizontalAlignment = HorizontalAlignment.Stretch };
        reset.Click += (_, _) => Change(_ => default, commit: true);
        EditControls.Children.Add(reset);
        EditControls.Children.Add(new TextBlock
        {
            Text = "Double-click a slider to reset it. Ctrl+Z undoes, Ctrl+Y redoes.",
            Opacity = 0.55, FontSize = 12, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 8, 0, 0),
        });
    }

    void Header(string text) => EditControls.Children.Add(new TextBlock
    {
        Text = text,
        Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"],
        Margin = new Thickness(0, EditControls.Children.Count == 0 ? 0 : 14, 0, 2),
    });

    void AddPercent(string name, Func<Engine.Edit, float> get, Func<Engine.Edit, float, Engine.Edit> set) => Add(new Adjustment
    {
        Name = name, Min = -100, Max = 100, Step = 1,
        Get = e => get(e),
        Set = (e, v) => set(e, (float)v),
        Format = v => $"{v:+0;-0;0}",
    });

    Adjustment Add(Adjustment a)
    {
        var label = new Grid { Margin = new Thickness(0, 4, 0, -6) };
        label.Children.Add(new TextBlock { Text = a.Name, FontSize = 13 });
        a.Value = new TextBlock { HorizontalAlignment = HorizontalAlignment.Right, FontSize = 13, Opacity = 0.75 };
        label.Children.Add(a.Value);
        a.Slider = new Slider
        {
            Minimum = a.Min, Maximum = a.Max, StepFrequency = a.Step, SmallChange = a.Step,
            LargeChange = (a.Max - a.Min) / 20, IsThumbToolTipEnabled = false,
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(a.Slider, a.Name);
        a.Slider.ValueChanged += (_, e) => Slider_ValueChanged(a, e);
        a.Slider.DoubleTapped += (_, _) => Reset(a);
        // The end of a drag (or of a key press on the slider) commits what it changed.
        a.Slider.AddHandler(UIElement.PointerReleasedEvent, new PointerEventHandler((_, _) => CommitEdit()), true);
        a.Slider.AddHandler(UIElement.PointerCaptureLostEvent, new PointerEventHandler((_, _) => CommitEdit()), true);
        a.Slider.KeyUp += (_, _) => CommitEdit();
        EditControls.Children.Add(label);
        EditControls.Children.Add(a.Slider);
        adjustments.Add(a);
        return a;
    }

    /// <summary>Starting a custom white balance from the one as shot, so the other slider
    /// keeps its value.</summary>
    Engine.Edit CustomWhiteBalance(Engine.Edit e)
    {
        if (e.CustomWhiteBalance != 0)
            return e;
        var asShot = CurrentPhoto is { } p ? engine.AsShotWhiteBalance(p.Index) : null;
        return e with { CustomWhiteBalance = 1, Temperature = asShot?.Kelvin ?? 5500, Tint = asShot?.Tint ?? 0 };
    }

    void Slider_ValueChanged(Adjustment a, RangeBaseValueChangedEventArgs e)
    {
        a.Value.Text = a.Format(e.NewValue);
        if (!updatingSliders)
            Change(edit => a.Set(edit, e.NewValue), commit: false);
    }

    void Reset(Adjustment a)
    {
        if (a == temperature || a == tint)
            Change(e => e with { CustomWhiteBalance = 0 }, commit: true);
        else
            Change(e => a.Set(e, 0), commit: true);
    }

    /// <summary>Applies a change to the photo in view and shows it on the sliders.</summary>
    void Change(Func<Engine.Edit, Engine.Edit> change, bool commit)
    {
        if (CurrentPhoto is not { } p)
            return;
        edit = change(edit);
        engine.SetEdit(p.Index, edit, commit);
        editPending = !commit;
        if (commit)
            AfterCommit(p);
        ShowEdit();
    }

    void CommitEdit()
    {
        if (!editPending || CurrentPhoto is not { } p)
            return;
        editPending = false;
        engine.SetEdit(p.Index, edit, commit: true);
        AfterCommit(p);
    }

    void AfterCommit(PhotoItem p)
    {
        p.Metadata = engine.GetMetadata(all.Length)[p.Index]; // for its "edited" badge
    }

    void UndoEdit(bool redo)
    {
        if (CurrentPhoto is not { } p)
            return;
        CommitEdit();
        if (engine.Undo(p.Index, redo) is { } now)
        {
            edit = now;
            AfterCommit(p);
            ShowEdit();
        }
    }

    /// <summary>Loads the adjustments of the photo now in view.</summary>
    void LoadEdit()
    {
        CommitEdit();
        edit = CurrentPhoto is { } p ? engine.GetEdit(p.Index) : default;
        ShowEdit();
    }

    /// <summary>Sets the sliders from <see cref="edit"/>, showing the as-shot white balance when
    /// there's no custom one (once the raw is decoded and it's known).</summary>
    void ShowEdit()
    {
        updatingSliders = true;
        foreach (var a in adjustments)
        {
            if (a == temperature || a == tint)
                continue;
            a.Slider.Value = a.Get(edit);
            a.Value.Text = a.Format(a.Slider.Value);
        }
        var shown = edit;
        var asShot = CurrentPhoto is { } p && edit.CustomWhiteBalance == 0 ? engine.AsShotWhiteBalance(p.Index) : null;
        if (asShot is { } s)
            shown = shown with { Temperature = s.Kelvin, Tint = s.Tint };
        var known = edit.CustomWhiteBalance != 0 || asShot != null;
        temperature.Slider.Value = known ? temperature.Get(shown) : KelvinToSlider(5500);
        tint.Slider.Value = known ? tint.Get(shown) : 0;
        temperature.Value.Text = known ? temperature.Format(temperature.Slider.Value) : "";
        tint.Value.Text = known ? tint.Format(tint.Slider.Value) : "";
        whiteBalanceText.Text = edit.CustomWhiteBalance != 0 ? "Custom" : known ? "As shot" : "As shot (reading the raw…)";
        updatingSliders = false;
    }

    /// <summary>Ctrl+Z / Ctrl+Y while editing. Returns whether the key was one of them.</summary>
    bool EditShortcut(VirtualKeyWithModifiers key)
    {
        switch (key)
        {
            case { Key: Windows.System.VirtualKey.Z, Shift: false }:
                UndoEdit(redo: false);
                return true;
            case { Key: Windows.System.VirtualKey.Y }:
            case { Key: Windows.System.VirtualKey.Z, Shift: true }:
                UndoEdit(redo: true);
                return true;
            default:
                return false;
        }
    }

    readonly record struct VirtualKeyWithModifiers(Windows.System.VirtualKey Key, bool Shift);
}
