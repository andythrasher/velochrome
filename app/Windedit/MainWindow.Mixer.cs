using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;

namespace Windedit;

/// <summary>
/// The Color Mixer group of the Edit panel (docs/phase2c.md): Hue, Saturation and Luminance tabs of
/// eight sliders, one per colour range. Each slider's track shows what it does to its range's
/// colour, redrawn for the theme.
/// </summary>
public sealed partial class MainWindow
{
    static readonly string[] MixerRanges = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];
    static readonly string[] MixerKinds = ["Hue", "Saturation", "Luminance"];

    /// <summary>Each range's centre in OkLCh hue, degrees (core/src/mixer.rs CENTRES).</summary>
    static readonly double[] MixerCentres = [29.23, 52.78, 109.77, 142.50, 194.77, 264.05, 293.77, 328.36];

    /// <summary>The slider and which track it paints, by index (kind × 8 + range).</summary>
    readonly List<Slider> mixerSliders = [];
    readonly StackPanel[] mixerTabs = new StackPanel[3];
    readonly ToggleButton[] mixerTabButtons = new ToggleButton[3];

    void BuildColorMixer()
    {
        Header("Color Mixer");
        var body = group;
        var tabRow = new Grid { Margin = new Thickness(0, 2, 0, 4) };
        var tabs = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        for (var t = 0; t < 3; t++)
        {
            var tab = t;
            var button = new ToggleButton
            {
                Content = MixerKinds[t], FontSize = 12, Padding = new Thickness(8, 3, 8, 3), MinWidth = 0, MinHeight = 0,
                IsChecked = t == 0,
            };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, MixerKinds[t] + " tab");
            button.Click += (_, _) =>
            {
                ShowMixerTab(tab);
                FocusKeys();
            };
            mixerTabButtons[t] = button;
            tabs.Children.Add(button);
        }
        var reset = new HyperlinkButton { Content = "Reset", Padding = new Thickness(4, 0, 4, 0), FontSize = 12, HorizontalAlignment = HorizontalAlignment.Right };
        reset.Click += (_, _) => Change(Engine.Edit.WithoutMixer, commit: true);
        tabRow.Children.Add(tabs);
        tabRow.Children.Add(reset);
        body.Children.Add(tabRow);

        for (var t = 0; t < 3; t++)
        {
            var panel = mixerTabs[t] = new StackPanel { Spacing = 2, Visibility = t == 0 ? Visibility.Visible : Visibility.Collapsed };
            body.Children.Add(panel);
            group = panel;
            for (var r = 0; r < 8; r++)
            {
                var index = t * 8 + r;
                var kind = t;
                var range = r;
                var a = Add(new Adjustment
                {
                    Name = MixerRanges[r], AutomationName = $"{MixerKinds[t]} {MixerRanges[r]}",
                    Min = -100, Max = 100, Step = 1,
                    Get = e => Engine.Edit.GetMixer(e, index),
                    Set = (e, v) => Engine.Edit.WithMixer(e, index, (float)v),
                    Format = v => $"{v:+0;-0;0}",
                });
                mixerSliders.Add(a.Slider);
                a.Slider.Loaded += (_, _) => PaintMixerTrack(kind, range);
            }
        }
        group = body;
        EditControls.ActualThemeChanged += (_, _) =>
        {
            for (var t = 0; t < 3; t++)
                for (var r = 0; r < 8; r++)
                    PaintMixerTrack(t, r);
        };
    }

    void ShowMixerTab(int tab)
    {
        for (var t = 0; t < 3; t++)
        {
            mixerTabs[t].Visibility = t == tab ? Visibility.Visible : Visibility.Collapsed;
            mixerTabButtons[t].IsChecked = t == tab;
        }
        for (var r = 0; r < 8; r++)
        {
            mixerSliders[tab * 8 + r].UpdateLayout();
            PaintMixerTrack(tab, r);
        }
    }

    /// <summary>Colours a mixer slider's track: the hue gradient the slider moves the range through
    /// (Hue), grey to the range's colour (Saturation), dark to light (Luminance). The ends' lightness
    /// follows the theme so the track stays visible against the panel.</summary>
    void PaintMixerTrack(int kind, int range)
    {
        var slider = mixerSliders[kind * 8 + range];
        slider.ApplyTemplate();
        var track = FindPart(slider, "HorizontalTrackRect") as Microsoft.UI.Xaml.Shapes.Rectangle;
        if (track == null)
            return;
        var dark = EditControls.ActualTheme == ElementTheme.Dark;
        var centre = MixerCentres[range];
        var brush = new LinearGradientBrush { StartPoint = new Windows.Foundation.Point(0, 0.5), EndPoint = new Windows.Foundation.Point(1, 0.5) };
        void Stop(double at, Windows.UI.Color c) => brush.GradientStops.Add(new GradientStop { Offset = at, Color = c });
        switch (kind)
        {
            case 0:
                {
                    // From the colour the slider's minimum moves the centre to, to its maximum's.
                    var before = MixerCentres[(range + 7) % 8];
                    var after = MixerCentres[(range + 1) % 8];
                    var from = centre - 0.5 * (centre - before + (before > centre ? 360 : 0));
                    var to = centre + 0.5 * (after - centre + (after < centre ? 360 : 0));
                    for (var i = 0; i <= 8; i++)
                        Stop(i / 8.0, OkLchColour(0.70, 0.13, from + (to - from) * i / 8));
                    break;
                }
            case 1:
                Stop(0, OkLchColour(dark ? 0.62 : 0.72, 0.0, centre));
                Stop(1, OkLchColour(dark ? 0.62 : 0.72, 0.20, centre));
                break;
            default:
                Stop(0, OkLchColour(dark ? 0.30 : 0.38, 0.09, centre));
                Stop(0.5, OkLchColour(0.65, 0.14, centre));
                Stop(1, OkLchColour(dark ? 0.95 : 0.88, 0.07, centre));
                break;
        }
        track.Fill = brush;
        track.Height = 6;
        track.RadiusX = track.RadiusY = 3;
        if (FindPart(slider, "HorizontalDecreaseRect") is Microsoft.UI.Xaml.Shapes.Rectangle decrease)
            decrease.Fill = new SolidColorBrush(Colors.Transparent);
    }

    static FrameworkElement? FindPart(DependencyObject parent, string name)
    {
        for (var i = 0; i < VisualTreeHelper.GetChildrenCount(parent); i++)
        {
            var child = VisualTreeHelper.GetChild(parent, i);
            if (child is FrameworkElement fe && fe.Name == name)
                return fe;
            if (FindPart(child, name) is { } found)
                return found;
        }
        return null;
    }

    /// <summary>An sRGB colour from OkLCh (lightness, chroma, hue in degrees); chroma is cut back
    /// until it fits the gamut.</summary>
    static Windows.UI.Color OkLchColour(double l, double chroma, double hueDegrees)
    {
        var h = hueDegrees * Math.PI / 180;
        static bool Fits(double[] c) => c.All(v => v is >= -1e-4 and <= 1 + 1e-4);
        double[] At(double c)
        {
            var a = c * Math.Cos(h);
            var b = c * Math.Sin(h);
            var lp = l + 0.3963377774 * a + 0.2158037573 * b;
            var mp = l - 0.1055613458 * a - 0.0638541728 * b;
            var sp = l - 0.0894841775 * a - 1.2914855480 * b;
            lp = lp * lp * lp; mp = mp * mp * mp; sp = sp * sp * sp;
            return
            [
                4.0767416621 * lp - 3.3077115913 * mp + 0.2309699292 * sp,
                -1.2684380046 * lp + 2.6097574011 * mp - 0.3413193965 * sp,
                -0.0041960863 * lp - 0.7034186147 * mp + 1.7076147010 * sp,
            ];
        }
        var rgb = At(chroma);
        if (!Fits(rgb))
        {
            double lo = 0, hi = chroma;
            for (var i = 0; i < 16; i++)
            {
                var mid = (lo + hi) / 2;
                if (Fits(At(mid))) lo = mid; else hi = mid;
            }
            rgb = At(lo);
        }
        static byte Encode(double v)
        {
            v = Math.Clamp(v, 0, 1);
            return (byte)Math.Round(255 * (v <= 0.0031308 ? 12.92 * v : 1.055 * Math.Pow(v, 1 / 2.4) - 0.055));
        }
        return Windows.UI.Color.FromArgb(255, Encode(rgb[0]), Encode(rgb[1]), Encode(rgb[2]));
    }
}
