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
        /// <summary>The slider's accessible name when it differs from the label (two sliders are both called Detail).</summary>
        public string? AutomationName;
        public required double Min, Max, Step;
        /// <summary>What double-clicking resets it to.</summary>
        public double Default;
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
    ComboBox profileBox = null!;
    int profilesKnownFor = -1;
    uint committedProfile;
    bool profilePreviewing;
    // The item the pointer was released on, while the drop-down is open: the choice, as far as a click goes (see AddProfileItem).
    uint? profileClicked;
    // The item a press landed on since the drop-down opened. The release of the click that opened it must not count as a choice.
    uint? profilePressed;
    // True from the drop-down opening until its Closed event, which comes after the key that closes it has reached the window.
    bool profileListOpen;
    /// <summary>The adjustments of the photo in view, as last sent to the engine.</summary>
    Engine.Edit edit = Engine.Edit.None;
    /// <summary>A slider moved since the last commit.</summary>
    bool editPending;
    /// <summary>Sliders are being set from the edit, so their changes aren't edits.</summary>
    bool updatingSliders;

    void BuildEditPanel()
    {
        groupHost = CropControls;
        BuildCropGroup();
        groupHost = EditControls;

        Header("White balance");
        BuildProfileRow();
        whiteBalanceText = new TextBlock { Opacity = 0.6, FontSize = 12, VerticalAlignment = VerticalAlignment.Center };
        pickerButton = new ToggleButton
        {
            Content = new FontIcon { Glyph = ((char)0xEF3C).ToString(), FontSize = 14 },
            Padding = new Thickness(6, 3, 6, 3), MinWidth = 0, MinHeight = 0,
        };
        ToolTipService.SetToolTip(pickerButton, "White balance selector (W): click a spot that should be neutral");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(pickerButton, "White balance selector");
        pickerButton.Click += (_, _) =>
        {
            SetPicking(pickerButton.IsChecked == true);
            FocusKeys();
        };
        var asShot = new HyperlinkButton { Content = "As shot", Padding = new Thickness(4, 0, 4, 0), FontSize = 12 };
        asShot.Click += (_, _) => Change(e => e with { CustomWhiteBalance = 0 }, commit: true);
        var row = new Grid { Margin = new Thickness(0, 0, 0, 2) };
        row.Children.Add(new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Children = { pickerButton, whiteBalanceText } });
        row.Children.Add(asShot);
        asShot.HorizontalAlignment = HorizontalAlignment.Right;
        group.Children.Add(row);

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
        AddPercent("Texture", e => e.Texture, (e, v) => e with { Texture = v });
        AddPercent("Clarity", e => e.Clarity, (e, v) => e with { Clarity = v });
        AddPercent("Vibrance", e => e.Vibrance, (e, v) => e with { Vibrance = v });
        AddPercent("Saturation", e => e.Saturation, (e, v) => e with { Saturation = v });

        BuildColorMixer();

        Header("Detail");
        group.Children.Add(new TextBlock
        {
            Text = "Zoom to 100% (Z) to judge sharpening and noise reduction",
            Opacity = 0.55, FontSize = 12, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(4, 0, 0, 4),
        });
        SubHeader("Sharpening");
        Add(DetailSlider("Amount", 0, 150, 1, 40, e => e.SharpenAmount, (e, v) => e with { SharpenAmount = v }, "{0:0}"));
        Add(DetailSlider("Radius", 0.5, 3.0, 0.1, 1.0, e => e.SharpenRadius, (e, v) => e with { SharpenRadius = v }, "{0:0.0}"));
        Add(DetailSlider("Detail", 0, 100, 1, 25, e => e.SharpenDetail, (e, v) => e with { SharpenDetail = v }, "{0:0}"));
        Add(DetailSlider("Masking", 0, 100, 1, 0, e => e.SharpenMasking, (e, v) => e with { SharpenMasking = v }, "{0:0}"));
        SubHeader("Noise reduction");
        Add(DetailSlider("Luminance", 0, 100, 1, 0, e => e.NoiseLuminance, (e, v) => e with { NoiseLuminance = v }, "{0:0}"));
        Add(DetailSlider("Detail", 0, 100, 1, 50, e => e.NoiseLuminanceDetail, (e, v) => e with { NoiseLuminanceDetail = v }, "{0:0}", "Luminance detail"));
        Add(DetailSlider("Color", 0, 100, 1, 25, e => e.NoiseColor, (e, v) => e with { NoiseColor = v }, "{0:0}"));
        Add(DetailSlider("Detail", 0, 100, 1, 50, e => e.NoiseColorDetail, (e, v) => e with { NoiseColorDetail = v }, "{0:0}", "Color detail"));

        Header("Lens");
        Add(new Adjustment
        {
            Name = "Vignetting", Min = -100, Max = 100, Step = 1,
            Get = e => e.VignetteAmount,
            Set = (e, v) => e with { VignetteAmount = (float)v },
            Format = v => $"{v:+0;-0;0}",
        });
        Add(new Adjustment
        {
            Name = "Midpoint", Min = 0, Max = 100, Step = 1, Default = 50,
            Get = e => 50 + e.VignetteMidpoint,
            Set = (e, v) => e with { VignetteMidpoint = (float)v - 50 },
            Format = v => $"{v:0}",
        });
        distortionSwitch = LensSwitch("Distortion", "Correct the distortion recorded in the raw", on => e => e with { NoDistortion = on ? 0u : 1u });
        aberrationSwitch = LensSwitch("Chromatic aberration", "Correct the colour fringing recorded in the raw", on => e => e with { NoChromaticAberration = on ? 0u : 1u });

        var reset = new Button { Content = "Reset all", Margin = new Thickness(0, 16, 0, 0), HorizontalAlignment = HorizontalAlignment.Stretch };
        reset.Click += (_, _) => Change(_ => Engine.Edit.None, commit: true);
        EditControls.Children.Add(reset);
        EditControls.Children.Add(new TextBlock
        {
            Text = "Double-click a slider to reset it. Ctrl+Z undoes, Ctrl+Y redoes.",
            Opacity = 0.55, FontSize = 12, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 8, 0, 0),
        });
    }

    /// <summary>Where <see cref="Header"/> puts the groups being built: the Edit panel, or the Crop tool's.</summary>
    StackPanel groupHost = null!;

    /// <summary>The panel of the group being built: <see cref="Add"/> puts sliders in it.</summary>
    StackPanel group = null!;

    /// <summary>Starts a group of sliders under a header that opens and closes it.</summary>
    void Header(string text)
    {
        var chevron = new FontIcon { FontSize = 12 };
        var button = new Button
        {
            Background = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent),
            BorderThickness = new Thickness(0),
            HorizontalAlignment = HorizontalAlignment.Stretch,
            HorizontalContentAlignment = HorizontalAlignment.Left,
            Padding = new Thickness(4, 6, 4, 6),
            Margin = new Thickness(0, groupHost.Children.Count == 0 ? 0 : 8, 0, 0),
            Content = new StackPanel
            {
                Orientation = Orientation.Horizontal,
                Spacing = 8,
                Children =
                {
                    chevron,
                    new TextBlock { Text = text, Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"] },
                },
            },
        };
        var body = group = new StackPanel { Spacing = 2 };
        void Show(bool open)
        {
            body.Visibility = open ? Visibility.Visible : Visibility.Collapsed;
            chevron.Glyph = ((char)(open ? 0xE70D : 0xE76C)).ToString(); // down, right
        }
        Show(layout.GetValueOrDefault(text, true));
        button.Click += (_, _) =>
        {
            var open = body.Visibility == Visibility.Collapsed;
            Show(open);
            SetLayout(text, open);
            FocusKeys();
        };
        groupHost.Children.Add(button);
        groupHost.Children.Add(body);
    }

    ToggleSwitch distortionSwitch = null!, aberrationSwitch = null!;

    /// <summary>A labelled on/off switch for one of the automatic lens corrections. It sets the
    /// edit's off-flag, so a photo with no edit has both on.</summary>
    ToggleSwitch LensSwitch(string name, string tip, Func<bool, Func<Engine.Edit, Engine.Edit>> change)
    {
        var toggle = new ToggleSwitch { OnContent = "", OffContent = "", MinWidth = 0, IsOn = true };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(toggle, name);
        ToolTipService.SetToolTip(toggle, tip);
        toggle.Toggled += (_, _) =>
        {
            if (!updatingSliders)
                Change(change(toggle.IsOn), commit: true);
            FocusKeys();
        };
        var row = new Grid { Margin = new Thickness(0, 4, 0, 0) };
        row.Children.Add(new TextBlock { Text = name, FontSize = 13, VerticalAlignment = VerticalAlignment.Center });
        toggle.HorizontalAlignment = HorizontalAlignment.Right;
        row.Children.Add(toggle);
        group.Children.Add(row);
        return toggle;
    }

    void BuildProfileRow()
    {
        var row = new Grid { ColumnSpacing = 10, Margin = new Thickness(0, 2, 0, 8) };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        row.Children.Add(new TextBlock { Text = "Profile", FontSize = 13, VerticalAlignment = VerticalAlignment.Center });
        profileBox = new ComboBox { HorizontalAlignment = HorizontalAlignment.Stretch };
        Grid.SetColumn(profileBox, 1);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(profileBox, "Profile");
        ToolTipService.SetToolTip(profileBox, "Camera profile");
        profileBox.DropDownOpened += (_, _) =>
        {
            profileListOpen = true;
            committedProfile = edit.Profile;
            profilePreviewing = false;
            profileClicked = null;
            profilePressed = null;
        };
        profileBox.SelectionChanged += (_, _) =>
        {
            if (updatingSliders || profileBox.SelectedItem is not ComboBoxItem { Tag: uint chosen })
                return;
            profilePreviewing = false;
            committedProfile = chosen;
            Change(e => e with { Profile = chosen }, commit: true);
            FocusKeys();
        };
        profileBox.DropDownClosed += (_, _) =>
        {
            profileListOpen = false;
            // A click on an entry that the preview had already selected raises no SelectionChanged, so the
            // choice is committed here. Closing any other way (Esc, a click elsewhere) gives the preview up.
            if (profileClicked is { } clicked)
            {
                profileClicked = null;
                profilePreviewing = false;
                if (clicked != committedProfile)
                {
                    committedProfile = clicked;
                    Change(e => e with { Profile = clicked }, commit: true);
                }
            }
            if (profilePreviewing)
            {
                profilePreviewing = false;
                if (CurrentPhoto is { } p && edit.Profile != committedProfile)
                {
                    edit = edit with { Profile = committedProfile };
                    engine.SetEdit(p.Index, edit, commit: false);
                }
            }
            FocusKeys();
        };
        row.Children.Add(profileBox);
        group.Children.Add(row);
    }

    const string BetaProfileTip = "Beta: this profile's brightness doesn't match Lightroom yet (it renders 0.3–0.8 stops darker).";

    /// <summary>The camera-matching and Adobe Standard profiles read from Adobe's files; a profile the user added is not beta.</summary>
    static bool IsBetaProfile(uint id, string name) =>
        id != 0 && (name.StartsWith("Camera ", StringComparison.Ordinal) || name == "Adobe Standard");

    /// <summary>The profile's name with a small "Beta" tag after it.</summary>
    static DataTemplate? betaProfileTemplate;
    static DataTemplate BetaProfileTemplate => betaProfileTemplate ??= (DataTemplate)Microsoft.UI.Xaml.Markup.XamlReader.Load(
        "<DataTemplate xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\">" +
        "<StackPanel Orientation=\"Horizontal\" Spacing=\"8\"><TextBlock Text=\"{Binding}\" VerticalAlignment=\"Center\"/>" +
        "<Border CornerRadius=\"4\" Padding=\"5,1,5,2\" VerticalAlignment=\"Center\" Background=\"{ThemeResource ControlFillColorSecondaryBrush}\">" +
        "<TextBlock Text=\"Beta\" FontSize=\"10\" Foreground=\"{ThemeResource TextFillColorSecondaryBrush}\"/></Border></StackPanel></DataTemplate>");

    void AddProfileItem(uint id, string name)
    {
        var item = new ComboBoxItem { Content = name, Tag = id };
        if (IsBetaProfile(id, name))
        {
            item.ContentTemplate = BetaProfileTemplate;
            ToolTipService.SetToolTip(item, BetaProfileTip);
        }
        item.PointerEntered += (_, _) =>
        {
            if (!profileBox.IsDropDownOpen || CurrentPhoto is not { } p)
                return;
            if (edit.Profile != id)
            {
                profilePreviewing = true;
                edit = edit with { Profile = id };
                engine.SetEdit(p.Index, edit, commit: false);
            }
        };
        item.AddHandler(UIElement.PointerPressedEvent, new PointerEventHandler((_, _) => profilePressed = id), true);
        item.AddHandler(UIElement.PointerReleasedEvent, new PointerEventHandler((_, _) =>
        {
            if (profilePressed == id)
                profileClicked = id;
        }), true);
        profileBox.Items.Add(item);
    }

    void UpdateProfileBox()
    {
        if (CurrentPhoto is not { } p)
        {
            profilesKnownFor = -1;
            return;
        }
        if (profilesKnownFor != p.Index)
        {
            profilesKnownFor = p.Index;
            var list = engine.PhotoProfiles(p.Index);
            profileBox.Items.Clear();
            foreach (var (id, name) in list)
                AddProfileItem(id, name);
        }
        var index = 0;
        for (var i = 0; i < profileBox.Items.Count; i++)
        {
            if (profileBox.Items[i] is ComboBoxItem { Tag: uint id } && id == edit.Profile)
            {
                index = i;
                break;
            }
        }
        if (index == 0 && edit.Profile != 0)
        {
            var extra = Engine.ProfileName(edit.Profile);
            if (extra.Length > 0)
            {
                AddProfileItem(edit.Profile, extra);
                index = profileBox.Items.Count - 1;
            }
        }
        profileBox.SelectedIndex = index;
        var chosenItem = profileBox.Items[index] as ComboBoxItem;
        var beta = chosenItem is { Tag: uint chosenId, Content: string chosenName } && IsBetaProfile(chosenId, chosenName);
        ToolTipService.SetToolTip(profileBox, beta ? "Camera profile (beta). " + BetaProfileTip : "Camera profile");
    }

    /// <summary>The photo whose lens data the switches were set from, or -1 until it is known.</summary>
    int lensKnownFor = -1;

    /// <summary>Greys out a lens switch the photo has no data for, with the reason in its tooltip.
    /// Unknown until the raw is decoded.</summary>
    void UpdateLensSwitches()
    {
        if (CurrentPhoto is { } current && lensKnownFor == current.Index)
            return;
        var info = CurrentPhoto is { } p ? engine.LensInfo(p.Index) : null;
        lensKnownFor = info == null || CurrentPhoto == null ? -1 : CurrentPhoto.Index;
        void Set(ToggleSwitch toggle, bool? has, string what, string available)
        {
            toggle.IsEnabled = has == true;
            ToolTipService.SetToolTip(toggle, has switch
            {
                true => available,
                false => $"This photo has no {what} data to correct.",
                null => "Reading the raw…",
            });
        }
        Set(distortionSwitch, info?.Distortion, "distortion", "Correct the distortion recorded in the raw");
        Set(aberrationSwitch, info?.ChromaticAberration, "chromatic aberration", "Correct the colour fringing recorded in the raw");
    }

    /// <summary>A small title inside a group, for the sliders under it.</summary>
    void SubHeader(string text) => group.Children.Add(new TextBlock
    {
        Text = text, Opacity = 0.75, FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
        Margin = new Thickness(4, 8, 0, 0),
    });

    /// <summary>A detail slider. The edit holds its value as an offset from <paramref name="def"/>, so
    /// a zeroed edit is the default; the slider shows the value itself.</summary>
    static Adjustment DetailSlider(string name, double min, double max, double step, double def,
        Func<Engine.Edit, float> offset, Func<Engine.Edit, float, Engine.Edit> set, string format, string? automationName = null) => new()
    {
        Name = name, Min = min, Max = max, Step = step, Default = def, AutomationName = automationName,
        Get = e => def + offset(e),
        Set = (e, v) => set(e, (float)(v - def)),
        Format = v => string.Format(System.Globalization.CultureInfo.InvariantCulture, format, v),
    };

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
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(a.Slider, a.AutomationName ?? a.Name);
        a.Slider.ValueChanged += (_, e) => Slider_ValueChanged(a, e);
        a.Slider.DoubleTapped += (_, _) => Reset(a);
        // The end of a drag (or of a key press on the slider) commits what it changed.
        a.Slider.AddHandler(UIElement.PointerReleasedEvent, new PointerEventHandler((_, _) => CommitEdit()), true);
        a.Slider.AddHandler(UIElement.PointerCaptureLostEvent, new PointerEventHandler((_, _) => CommitEdit()), true);
        a.Slider.KeyUp += (_, _) => CommitEdit();
        group.Children.Add(label);
        group.Children.Add(a.Slider);
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
            Change(e => a.Set(e, a.Default), commit: true);
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
        {
            Committed();
            AfterCommit(p);
        }
        ShowEdit();
    }

    void CommitEdit()
    {
        if (!editPending || CurrentPhoto is not { } p)
            return;
        editPending = false;
        engine.SetEdit(p.Index, edit, commit: true);
        Committed();
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
        CommitMasks();
        if (engine.Undo(p.Index, redo) is { } now)
        {
            edit = now.Edit;
            masks = now.Masks;
            if (cropping)
                cropCommits += redo ? 1 : -1;
            AfterCommit(p);
            ShowEdit();
            MasksChanged();
        }
    }

    /// <summary>Loads the adjustments of the photo now in view.</summary>
    void LoadEdit()
    {
        CommitEdit();
        profilesKnownFor = -1;
        lensKnownFor = -1;
        edit = CurrentPhoto is { } p ? engine.GetEdit(p.Index) : Engine.Edit.None;
        LoadMasks();
        ShowEdit();
    }

    /// <summary>Sets the sliders from <see cref="edit"/>, showing the as-shot white balance when
    /// there's no custom one (once the raw is decoded and it's known).</summary>
    void ShowEdit()
    {
        SyncIntended();
        updatingSliders = true;
        UpdateProfileBox();
        foreach (var a in adjustments)
        {
            if (a == temperature || a == tint)
                continue;
            a.Slider.Value = a.Get(edit);
            a.Value.Text = a.Format(a.Slider.Value);
        }
        if (cropping)
            UpdateCropOverlay();
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
        distortionSwitch.IsOn = edit.NoDistortion == 0;
        aberrationSwitch.IsOn = edit.NoChromaticAberration == 0;
        UpdateLensSwitches();
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
