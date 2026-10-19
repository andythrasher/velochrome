// The detail pass (docs/phase2b.md): noise reduction and sharpening on a region of one level of
// the demosaiced image, in the image's own (texture) space. Five kernels share these constants
// and one root signature:
//
//   atrousblur.hlsl    one à trous B3 blur, horizontal, of the image or of the last scale
//   atrousv.hlsl       the vertical blur, which finishes the next scale
//   atroushrink.hlsl   the detail the scale holds (the difference of two scales) is shrunk, by its
//                      neighbourhood's energy, and added to the running sum; the last scale
//                      writes the denoised image
//   sharpblur.hlsl     a Gaussian blur of log luminance, horizontal then vertical
//   sharpen.hlsl       the unsharp mask on log luminance, written to the detail texture
//
// The noise reduction follows the wavelet denoisers of darktable (denoise (profiled)) and
// RawTherapee (wavelet noise reduction), both GPL-3: an à trous decomposition with the B3
// spline, thresholds from a noise estimate, in a variance-stabilised space. This code is
// written for Velochrome's pipeline.

#define DETAIL_ROOT_SIGNATURE \
    "RootConstants(num32BitConstants=32, b0)," \
    "DescriptorTable(SRV(t0, numDescriptors=3), UAV(u0, numDescriptors=2))," \
    "StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR," \
    " addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP)"

// Must match DetailConstants in detail.rs.
cbuffer Constants : register(b0)
{
    int2 inOrigin;    // the colour input: texel (inOrigin + p) of the input texture, clamped to
    int2 inSize;      //   [0, inSize), for p in region pixels (the level's own texels for the image;
                      //   origin 0 and the region's size for the pass's own textures)
    int2 regionSize;
    uint step;        // à trous: spacing of the filter's taps
    uint flags;       // FLAG_*
    float4 thr;       // shrink thresholds for luminance, A and B (in the stabilised space), and the luminance noise at this scale
    float4 pw;        // shrink exponents: luminance, colour
    float4 sharp;     // Gaussian sigma (pixels), gain, difference limit (stops), tap count
    float4 mask;      // sharpen: edge mask threshold (stops per pixel), amount (0..1), coring (stabilised luminance sigma x k)
    int4 outRect;     // the pixels (of the working rectangle) that belong to this tile's output
    int2 outOffset;   // ... and where the first of them goes in the output texture (the detail texture)
};

static const uint FLAG_STABILISE = 1; // the input is camera RGB: convert it to the stabilised space
static const uint FLAG_LAST = 2;      // the last scale: write the denoised image
static const uint FLAG_ACC = 4;       // there is a running sum to add to
static const uint FLAG_VERTICAL = 8;  // sharpblur: the vertical pass
static const uint FLAG_MASK = 16;     // sharpen: show the mask instead of the picture
static const uint FLAG_OUT = 32;      // atroushrink, last scale: write the detail texture rather than a scratch texture

static const float B3[5] = { 1.0 / 16, 4.0 / 16, 6.0 / 16, 4.0 / 16, 1.0 / 16 };

// Added before the square root so the dark end, where noise goes below zero, stays smooth.
static const float STAB = 0.002;

// Camera RGB to the stabilised opponent space: the square root evens out shot noise across
// tones; then luminance Y, and two colour axes A (red - blue) and B (green against the others).
// Below zero (noise around black) the square root continues as its tangent line, so the dark end
// has no rectification, which would lift the blacks.
static const float STAB_ROOT = 0.0447214; // sqrt(STAB)

float stabiliseChannel(float c)
{
    return c >= 0.0 ? sqrt(c + STAB) : STAB_ROOT + c / (2.0 * STAB_ROOT);
}

float unstabiliseChannel(float y)
{
    return y >= STAB_ROOT ? y * y - STAB : 2.0 * STAB_ROOT * (y - STAB_ROOT);
}

float3 stabilise(float3 c)
{
    float3 y = float3(stabiliseChannel(c.r), stabiliseChannel(c.g), stabiliseChannel(c.b));
    return float3((y.r + y.g + y.b) / 3.0, 0.5 * (y.r - y.b), 0.25 * (2.0 * y.g - y.r - y.b));
}

float3 unstabilise(float3 o)
{
    float3 y = float3(o.x + o.y - 2.0 * o.z / 3.0, o.x + 4.0 * o.z / 3.0, o.x - o.y - 2.0 * o.z / 3.0);
    return float3(unstabiliseChannel(y.r), unstabiliseChannel(y.g), unstabiliseChannel(y.b));
}
float3 loadIn(Texture2D<float4> tex, int2 p)
{
    return tex.Load(int3(clamp(inOrigin + p, 0, inSize - 1), 0)).rgb;
}

float3 loadRegion(Texture2D<float4> tex, int2 p)
{
    return tex.Load(int3(clamp(p, 0, regionSize - 1), 0)).rgb;
}

// Soft thresholding at exponent 1; higher exponents (the garrote at 2) shrink large coefficients
// less, so edges and texture survive while the small, noisy ones still go.
float shrink(float c, float t, float p)
{
    float a = abs(c);
    if (t <= 0.0 || a == 0.0)
        return c;
    return c * max(0.0, 1.0 - pow(t / a, p));
}

// Log luminance with a floor added: pure log in the mid-tones and highlights, linear in the
// deepest shadows, where log would turn sensor noise into stops of fake detail to sharpen.
static const float LOG_FLOOR = 0.01;

float logLuminance(float3 rgb)
{
    return log2(clamp(dot(float3(0.299, 0.587, 0.114), rgb), 0.0, 64.0) + LOG_FLOOR);
}
