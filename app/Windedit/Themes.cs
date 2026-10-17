using Windows.UI;

namespace Windedit;

/// <summary>
/// How the window looks in light or dark mode, as in Windshot (Editing/Themes.cs). Null values
/// keep the standard look: plain Mica, the Windows accent color and the standard title bar.
/// </summary>
/// <param name="Tint">The window's color, laid nearly opaque over Mica, or fully opaque when <paramref name="Solid"/>.</param>
/// <param name="Canvas">Around the photo, where the window would otherwise show through.</param>
/// <param name="OnAccent">Icons and text on the accent; by default black or white, whichever reads.</param>
/// <param name="Panel">The Folders and Edit panels' and the filmstrip's background.</param>
internal sealed record WindowLook(
    Color? Tint = null,
    Color? Canvas = null,
    Color? Accent = null,
    Color? OnAccent = null,
    bool Solid = false,
    Color? TitleBar = null,
    Color? TitleText = null,
    Color? Panel = null);

/// <summary>A theme: a look for the window in light mode and one in dark mode.</summary>
internal sealed record Theme(string Name, WindowLook Light, WindowLook Dark)
{
    /// <summary>The standard look, which changes nothing.</summary>
    public bool IsStandard => Light == new WindowLook() && Dark == new WindowLook();

    /// <summary>A few colors that stand for the window look in a picker: background, canvas, accent.</summary>
    public Color[] WindowSwatches(bool dark)
    {
        var look = dark ? Dark : Light;
        return
        [
            look.Tint ?? Rgb(dark ? 0x202020u : 0xF3F3F3u),
            look.Canvas ?? Rgb(dark ? 0x1C1C1Cu : 0xE8E8E8u),
            look.Accent ?? Rgb(dark ? 0x60CDFFu : 0x0067C0u),
        ];
    }

    internal static Color Rgb(uint rgb) => Color.FromArgb(255, (byte)(rgb >> 16), (byte)(rgb >> 8), (byte)rgb);
}

internal static class Themes
{
    private static Color Rgb(uint rgb) => Theme.Rgb(rgb);

    private static WindowLook Tinted(uint tint, uint canvas, uint accent) => new(Rgb(tint), Rgb(canvas), Rgb(accent));

    // The same names and values as Windshot's, so the two apps match.
    public static readonly Theme[] All =
    [
        new("Windshot", new(), new()),

        new("Ember",
            Tinted(0xF8F0EB, 0xF0E2D9, 0xC2410C),
            Tinted(0x241B18, 0x1B1412, 0xFB923C)),

        new("Moss",
            Tinted(0xEFF2EC, 0xE3E8DE, 0x3A7D44),
            Tinted(0x1D221D, 0x161A16, 0x8BCB8F)),

        new("Fjord",
            Tinted(0xEEF2F6, 0xE1E7EE, 0x3B6E99),
            Tinted(0x1E242D, 0x171C23, 0x8DB8E0)),

        new("Sorbet",
            Tinted(0xFBF0F4, 0xF4E2EA, 0xB03A7A),
            Tinted(0x261E24, 0x1E171C, 0xF2A0CB)),

        new("Arcade",
            Tinted(0xF2EEF8, 0xE7E0F2, 0x8B2FC9),
            Tinted(0x16121E, 0x100D17, 0xD58CFF)),

        new("Graphite",
            Tinted(0xEDEDED, 0xE1E1E1, 0x3D4148),
            Tinted(0x1B1B1C, 0x141415, 0xC9CDD3)),

        // A tribute to the Windows 3.1 color scheme of the same name. Garish on purpose.
        new("Hot Dog Stand",
            new(Tint: Rgb(0xFFFF00), Canvas: Rgb(0xFF0000), Accent: Rgb(0x000000), OnAccent: Rgb(0xFFFF00), Solid: true,
                TitleBar: Rgb(0xFF0000), TitleText: Rgb(0xFFFFFF), Panel: Rgb(0xFFFF00)),
            new(Tint: Rgb(0x000000), Canvas: Rgb(0x800000), Accent: Rgb(0xFFFF00), OnAccent: Rgb(0x000000), Solid: true,
                TitleBar: Rgb(0xFF0000), TitleText: Rgb(0xFFFF00), Panel: Rgb(0x000000))),
    ];

    /// <summary>The theme with this name, or the standard one.</summary>
    public static Theme Find(string? name) =>
        All.FirstOrDefault(t => t.Name.Equals(name, StringComparison.OrdinalIgnoreCase)) ?? All[0];
}

internal static class ColorExtensions
{
    public static bool IsLight(this Color c) =>
        (0.2126 * c.R + 0.7152 * c.G + 0.0722 * c.B) / 255 > 0.6;
}
