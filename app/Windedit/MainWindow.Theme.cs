using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.UI;

namespace Windedit;

/// <summary>
/// Light and dark mode (Auto follows Windows) and the colour themes shared with Windshot. The
/// theme's colours go to the window, the panels, the area around the photo and the engine.
/// </summary>
public sealed partial class MainWindow
{
    // The mode and theme picked, as mode=auto|light|dark and theme=Name lines.
    static string AppearanceFile => Path.Combine(AppData, "appearance.txt");

    ThemedWindow theming = null!;
    /// <summary>The theme list is being rebuilt, so its selection changes aren't picks.</summary>
    bool updatingAppearance;
    /// <summary>Which mode the theme list's swatches show, so it's only rebuilt when that changes.</summary>
    bool? swatchesDark;

    void InitAppearance()
    {
        var saved = ReadLines(AppearanceFile).Select(l => l.Split('=', 2)).Where(p => p.Length == 2)
            .ToDictionary(p => p[0], p => p[1]);
        var mode = saved.GetValueOrDefault("mode") switch
        {
            "light" => ElementTheme.Light,
            "dark" => ElementTheme.Dark,
            _ => ElementTheme.Default,
        };

        ExportIcon.Glyph = PhotoItem.Glyph(0xE72D); // Share
        theming = new ThemedWindow(this, Root, ApplyLook);
        // Flyouts are shown outside the window's content, so they get the accent separately.
        theming.Include(ExportContent);
        theming.Include(AppearanceContent);
        theming.Apply(Themes.Find(saved.GetValueOrDefault("theme")), mode);
    }

    /// <summary>Everything the theme colours apart from the accent resources.</summary>
    void ApplyLook(WindowLook look, bool dark)
    {
        var panel = new SolidColorBrush(look.Panel ?? (dark ? Color.FromArgb(0x4C, 0x3A, 0x3A, 0x3A) : Color.FromArgb(0x80, 0xFF, 0xFF, 0xFF)));
        FoldersPanel.Background = panel;
        EditPanel.Background = panel;
        FilmstripScroller.Background = look.Panel is null ? null : panel;

        // Photo judgment wants a neutral surround, so around the photo is grey in every theme: a
        // tinted one (Ember's warm brown, say) would shift how its colors look.
        var canvas = Theme.Rgb(dark ? 0x1C1C1Cu : 0xE8E8E8u);
        CanvasHost.Background = new SolidColorBrush(canvas);
        engine.SetCanvasColor(canvas.R / 255f, canvas.G / 255f, canvas.B / 255f);

        // The current photo's border is the accent, which for the standard theme is Windows'.
        var accent = look.Accent ?? new Windows.UI.ViewManagement.UISettings().GetColorValue(
            dark ? Windows.UI.ViewManagement.UIColorType.AccentLight2 : Windows.UI.ViewManagement.UIColorType.AccentDark1);
        PhotoItem.Accent = new SolidColorBrush(accent);
        foreach (var p in all)
            p.RefreshAccent();

        ShowAppearance(dark);
    }

    /// <summary>Sets the toolbar icon, the mode choice and the theme list to match what's picked.</summary>
    void ShowAppearance(bool dark)
    {
        updatingAppearance = true;
        // Sun, moon, and a half-filled circle for Auto.
        AppearanceIcon.Glyph = PhotoItem.Glyph(theming.Mode switch
        {
            ElementTheme.Light => 0xE706,
            ElementTheme.Dark => 0xE708,
            _ => 0xF08C,
        });
        ModeAuto.IsChecked = theming.Mode == ElementTheme.Default;
        ModeLight.IsChecked = theming.Mode == ElementTheme.Light;
        ModeDark.IsChecked = theming.Mode == ElementTheme.Dark;

        if (swatchesDark != dark)
        {
            swatchesDark = dark;
            BuildThemeList(dark);
        }
        ThemeList.SelectedIndex = Array.IndexOf(Themes.All, theming.Theme);
        updatingAppearance = false;
    }

    /// <summary>One row per theme: a few swatches of its look in this mode, then its name.</summary>
    void BuildThemeList(bool dark)
    {
        ThemeList.Items.Clear();
        foreach (var theme in Themes.All)
        {
            var swatches = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
            foreach (var color in theme.WindowSwatches(dark))
            {
                swatches.Children.Add(new Border
                {
                    Width = 18, Height = 18, CornerRadius = new CornerRadius(3),
                    Background = new SolidColorBrush(color),
                    BorderBrush = (Brush)Application.Current.Resources["ControlStrokeColorDefaultBrush"],
                    BorderThickness = new Thickness(1),
                });
            }
            var row = new Grid { ColumnSpacing = 10 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition());
            row.Children.Add(swatches);
            var name = new TextBlock { Text = theme.Name, VerticalAlignment = VerticalAlignment.Center };
            Grid.SetColumn(name, 1);
            row.Children.Add(name);
            ThemeList.Items.Add(new ListViewItem { Content = row, Tag = theme });
        }
    }

    void Mode_Checked(object sender, RoutedEventArgs e)
    {
        if (updatingAppearance || sender is not RadioButton { Tag: string tag })
            return;
        Pick(theming.Theme, Enum.Parse<ElementTheme>(tag));
    }

    void Theme_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (updatingAppearance || ThemeList.SelectedItem is not ListViewItem { Tag: Theme theme })
            return;
        Pick(theme, theming.Mode);
    }

    void Pick(Theme theme, ElementTheme mode)
    {
        theming.Apply(theme, mode);
        WriteLines(AppearanceFile,
        [
            $"mode={mode switch { ElementTheme.Light => "light", ElementTheme.Dark => "dark", _ => "auto" }}",
            $"theme={theme.Name}",
        ]);
    }
}
