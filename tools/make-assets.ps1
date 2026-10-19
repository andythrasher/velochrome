# Generates the app icon (app/Windedit/Assets/Velochrome.ico) and the MSIX logos
# (app/Windedit/Assets/Package/*.png) from "Velochrome Icon.png" in the repo root.
# Usage: powershell -File tools/make-assets.ps1
#
# Every size is an exact area average of the 2048 px original, taken on premultiplied colour,
# so the antialiased edge of the rounded square picks up no light halo from the colour hidden
# under its transparent pixels.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public class Art {
    public int W, H; public float[] P; // premultiplied RGBA, 0..1
    public static Art Load(string path) {
        using (var bmp = new Bitmap(path)) {
            var d = bmp.LockBits(new Rectangle(0, 0, bmp.Width, bmp.Height), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
            var raw = new byte[d.Stride * bmp.Height];
            Marshal.Copy(d.Scan0, raw, 0, raw.Length);
            bmp.UnlockBits(d);
            var a = new Art { W = bmp.Width, H = bmp.Height, P = new float[bmp.Width * bmp.Height * 4] };
            for (int y = 0; y < a.H; y++) for (int x = 0; x < a.W; x++) {
                int s = y * d.Stride + x * 4, o = (y * a.W + x) * 4;
                float al = raw[s + 3] / 255f;
                a.P[o] = raw[s + 2] / 255f * al; a.P[o + 1] = raw[s + 1] / 255f * al; a.P[o + 2] = raw[s] / 255f * al; a.P[o + 3] = al;
            }
            return a;
        }
    }
    // The box around everything that isn't (nearly) transparent.
    public Rectangle Bounds() {
        int x0 = W, y0 = H, x1 = -1, y1 = -1;
        for (int y = 0; y < H; y++) for (int x = 0; x < W; x++)
            if (P[(y * W + x) * 4 + 3] > 0.03f) { if (x < x0) x0 = x; if (x > x1) x1 = x; if (y < y0) y0 = y; if (y > y1) y1 = y; }
        return Rectangle.FromLTRB(x0, y0, x1 + 1, y1 + 1);
    }
    // The square `src` averaged into size x size.
    public Bitmap Square(RectangleF src, int size) {
        var o = new float[size * size * 4];
        double step = src.Width / size;
        for (int y = 0; y < size; y++) for (int x = 0; x < size; x++) {
            double sx0 = src.X + x * step, sx1 = sx0 + step, sy0 = src.Y + y * step, sy1 = sy0 + step;
            double r = 0, g = 0, b = 0, a = 0, wsum = 0;
            for (int iy = (int)Math.Floor(sy0); iy < Math.Ceiling(sy1); iy++) {
                double wy = Math.Min(sy1, iy + 1) - Math.Max(sy0, iy);
                for (int ix = (int)Math.Floor(sx0); ix < Math.Ceiling(sx1); ix++) {
                    double w = wy * (Math.Min(sx1, ix + 1) - Math.Max(sx0, ix));
                    wsum += w;
                    if (ix < 0 || iy < 0 || ix >= W || iy >= H) continue;
                    int i = (iy * W + ix) * 4;
                    r += P[i] * w; g += P[i + 1] * w; b += P[i + 2] * w; a += P[i + 3] * w;
                }
            }
            int j = (y * size + x) * 4;
            o[j] = (float)(r / wsum); o[j + 1] = (float)(g / wsum); o[j + 2] = (float)(b / wsum); o[j + 3] = (float)(a / wsum);
        }
        return ToBitmap(o, size, size);
    }
    public static Bitmap ToBitmap(float[] p, int w, int h) {
        var bmp = new Bitmap(w, h, PixelFormat.Format32bppArgb);
        var d = bmp.LockBits(new Rectangle(0, 0, w, h), ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
        var raw = new byte[d.Stride * h];
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) {
            int i = (y * w + x) * 4, s = y * d.Stride + x * 4;
            float a = p[i + 3];
            raw[s + 3] = (byte)Math.Round(Math.Min(1f, a) * 255);
            if (a > 1e-6f) {
                raw[s + 2] = (byte)Math.Round(Math.Min(1f, p[i] / a) * 255);
                raw[s + 1] = (byte)Math.Round(Math.Min(1f, p[i + 1] / a) * 255);
                raw[s] = (byte)Math.Round(Math.Min(1f, p[i + 2] / a) * 255);
            }
        }
        Marshal.Copy(raw, 0, d.Scan0, raw.Length);
        bmp.UnlockBits(d);
        return bmp;
    }
}
'@

$root = Resolve-Path (Join-Path $PSScriptRoot '..')
$art = [Art]::Load((Join-Path $root 'Velochrome Icon.png'))
$b = $art.Bounds()
$side = [Math]::Max($b.Width, $b.Height)
# The icon's own square, centred on what's drawn.
$square = New-Object Drawing.RectangleF ($b.X + ($b.Width - $side) / 2.0), ($b.Y + ($b.Height - $side) / 2.0), $side, $side
"Icon bounds $($b.X),$($b.Y) $($b.Width)x$($b.Height)"

$assets = Join-Path $root 'app\Windedit\Assets'
$pkg = Join-Path $assets 'Package'
New-Item -ItemType Directory -Force $pkg | Out-Null
$splashBackground = [Drawing.Color]::FromArgb(255, 27, 32, 48)

# The icon at `fraction` of the canvas's shorter side, centred on a transparent (or `background`) canvas.
function Save-Logo([string]$name, [int]$width, [int]$height, [double]$fraction, $background = $null) {
    $size = [int][Math]::Round([Math]::Min($width, $height) * $fraction)
    $icon = $art.Square($square, $size)
    $out = New-Object Drawing.Bitmap $width, $height, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($out)
    if ($background) { $g.Clear($background) }
    $g.DrawImageUnscaled($icon, [int](($width - $size) / 2), [int](($height - $size) / 2))
    $g.Dispose(); $icon.Dispose()
    $out.Save((Join-Path $pkg $name), [Drawing.Imaging.ImageFormat]::Png); $out.Dispose()
}

$scales = @{ 100 = 1.0; 125 = 1.25; 150 = 1.5; 200 = 2.0; 400 = 4.0 }
foreach ($scale in $scales.Keys) {
    $f = $scales[$scale]
    Save-Logo "Square44x44Logo.scale-$scale.png" ([int](44 * $f)) ([int](44 * $f)) 1.0
    Save-Logo "Square150x150Logo.scale-$scale.png" ([int](150 * $f)) ([int](150 * $f)) 0.8
    Save-Logo "Wide310x150Logo.scale-$scale.png" ([int](310 * $f)) ([int](150 * $f)) 0.8
    Save-Logo "LargeTile.scale-$scale.png" ([int](310 * $f)) ([int](310 * $f)) 0.8
    Save-Logo "SmallTile.scale-$scale.png" ([int](71 * $f)) ([int](71 * $f)) 0.9
    Save-Logo "StoreLogo.scale-$scale.png" ([int](50 * $f)) ([int](50 * $f)) 1.0
    Save-Logo "SplashScreen.scale-$scale.png" ([int](620 * $f)) ([int](300 * $f)) 0.6 $splashBackground
}
# Exact pixel sizes for the taskbar, Alt+Tab and file lists. The icon has its own backplate, so
# the "unplated" variants are the same drawing.
foreach ($size in 16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256) {
    Save-Logo "Square44x44Logo.targetsize-$size.png" $size $size 1.0
    Save-Logo "Square44x44Logo.altform-unplated_targetsize-$size.png" $size $size 1.0
    Save-Logo "Square44x44Logo.altform-lightunplated_targetsize-$size.png" $size $size 1.0
}

# The .ico: 16, 24, 32, 48, 64 and 256 px, each a PNG entry.
$sizes = 16, 24, 32, 48, 64, 256
$images = foreach ($s in $sizes) {
    $bmp = $art.Square($square, $s)
    $ms = New-Object IO.MemoryStream
    $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , @($s, $ms.ToArray())
}
$ico = Join-Path $assets 'Velochrome.ico'
$fs = [IO.File]::Create($ico)
$w = New-Object IO.BinaryWriter $fs
$w.Write([int16]0); $w.Write([int16]1); $w.Write([int16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($img in $images) {
    $s, $bytes = $img
    $dim = if ($s -ge 256) { 0 } else { $s }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([int16]1); $w.Write([int16]32); $w.Write([int]$bytes.Length); $w.Write([int]$offset)
    $offset += $bytes.Length
}
foreach ($img in $images) { $w.Write($img[1]) }
$w.Close()
"Wrote $ico (sizes $($sizes -join ', ')) and $((Get-ChildItem $pkg -Filter *.png).Count) logos in $pkg"
