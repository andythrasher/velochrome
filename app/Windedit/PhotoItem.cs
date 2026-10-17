using System.ComponentModel;
using System.Runtime.CompilerServices;
using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

namespace Windedit;

/// <summary>One photo in the filmstrip and grid: its thumbnail and what its badges show.</summary>
public sealed partial class PhotoItem(int index, string name) : INotifyPropertyChanged
{
    // Lightroom's label colors, in label order (Red, Yellow, Green, Blue, Purple).
    static readonly SolidColorBrush[] LabelBrushes =
    [
        new(ColorHelper.FromArgb(255, 0xE0, 0x45, 0x45)),
        new(ColorHelper.FromArgb(255, 0xE6, 0xC2, 0x29)),
        new(ColorHelper.FromArgb(255, 0x4C, 0xB0, 0x50)),
        new(ColorHelper.FromArgb(255, 0x40, 0x84, 0xE0)),
        new(ColorHelper.FromArgb(255, 0x9C, 0x5C, 0xD0)),
        new(ColorHelper.FromArgb(255, 0x90, 0x90, 0x90)), // a label name of some other app's
    ];
    static readonly SolidColorBrush Transparent = new(Colors.Transparent);

    /// <summary>Index in the folder (the engine's numbering).</summary>
    public int Index { get; } = index;
    public string Name { get; } = name;

    public event PropertyChangedEventHandler? PropertyChanged;

    ImageSource? thumbnail;
    public ImageSource? Thumbnail
    {
        get => thumbnail;
        set => Set(ref thumbnail, value);
    }

    Engine.Metadata metadata;
    public Engine.Metadata Metadata
    {
        get => metadata;
        set
        {
            metadata = value;
            Changed(nameof(Stars));
            Changed(nameof(FlagGlyph));
            Changed(nameof(LabelBrush));
            Changed(nameof(Dimming));
        }
    }

    bool isCurrent;
    public bool IsCurrent
    {
        get => isCurrent;
        set
        {
            isCurrent = value;
            Changed(nameof(CurrentBrush));
        }
    }

    public string Stars => metadata.Rating > 0 ? new string('★', metadata.Rating) : "";

    /// <summary>Segoe Fluent Icons: a flag for picks, a cross for rejects.</summary>
    public string FlagGlyph => metadata.Pick switch { 1 => "", -1 => "", _ => "" };

    public Brush LabelBrush => metadata.Label is >= 1 and <= 6 ? LabelBrushes[metadata.Label - 1] : Transparent;

    /// <summary>Rejected photos fade back, as in Lightroom.</summary>
    public double Dimming => metadata.Pick == -1 ? 0.35 : 1.0;

    public Brush CurrentBrush => isCurrent
        ? (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"]
        : Transparent;

    void Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        field = value;
        Changed(name!);
    }

    void Changed(string name) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}
