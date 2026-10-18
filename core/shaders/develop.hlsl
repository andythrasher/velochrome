// The view pass: runs for every slider change, once per screen pixel. It develops a raw, or
// shows one of the camera's JPEGs (mode 1) through the same zoom and rotation.
//
// It samples the cached pyramid at the level matching the zoom, so its cost depends on the
// window size, not the sensor size. White balance, exposure, the camera's offset and the color
// conversion to Rec.2020 are folded into one matrix on the CPU. The tone model (tone.hlsli) then
// moves each pixel's brightness using the guided-filter coefficients and statistics the
// pre-pass worked out for the whole image.

#include "tone.hlsli"

// Like the other kernels', but with more inputs: the image, the tone parameters, the guided
// filter's coefficients, the statistics, the detail texture, Clarity's coefficients and the masks.
#define DEVELOP_ROOT_SIGNATURE \
    "RootConstants(num32BitConstants=62, b0)," \
    "DescriptorTable(SRV(t0, numDescriptors=8), UAV(u0))," \
    "StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR," \
    " addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP)"

cbuffer Constants : register(b0)
{
    float3 row0;    // camera RGB → linear Rec.2020; white balance, exposure and camera offset included
    uint detailOrigin; // the detail texture's rectangle in the level's texels: left and top (16 bits each),
    float3 row1;
    uint detailSize;   // ... and width and height
    float3 row2;
    uint detailLevel;  // 0: no detail texture; else the level it was made at, plus one
    float4 lum;     // camera RGB → brightness (the sRGB luminance of the same pixel)
    float2 origin;  // image coordinates (0..1) at output pixel (0, 0)
    float2 stepX;   // change in image coordinates per output pixel to the right
    float2 stepY;   // ... and per output pixel down
    float lod;      // pyramid level to sample
    uint mode;      // 0: develop a raw; 1: show a camera JPEG (BGRA, already display-ready)
    uint outWidth;
    uint outHeight;
    float saturation; // -1..1
    float vibrance;   // -1..1
    float2 baseScale; // image coordinates → pixels of the guided filter's small copy
    float2 baseSize;  // the small copy's size in pixels (the texture is bigger)
    float4 lensA;     // lens correction: model (0 none, 1 Olympus, 2 Panasonic), scale, zoom, has chromatic aberration
    float4 lensK;     // distortion terms
    float4 lensR;     // red chromatic aberration terms
    float4 lensB;     // blue chromatic aberration terms
    float4 clip;      // the picture on the output, in pixels: left, top, right, bottom (outside is canvas)
    float4 canvas;    // the empty canvas around the photo (rgb)
    float4 vignette;  // manual lens vignetting: stops at the corners, and the radius where it starts
    uint maskCount;   // masks in use (0: the loop is skipped entirely)
    uint maskFlags;   // bit 0: Clarity's base is there even if the global Clarity is 0; bit 1: draw mask maskFlags >> 8 as a red overlay
};

Texture2D<float4> image : register(t0);
Texture2D<float2> baseCoefficients : register(t2);
Texture2D<float2> clarityCoefficients : register(t5); // Clarity's base, when lum.w != 0 or maskFlags bit 0
Buffer<float> stats : register(t3); // the 99.5th and 25th percentiles of the tones

SamplerState linearClamp : register(s0);

// ---- Masks (core/src/masks.rs is the reference; docs/phase2d-masks.md) ----

// Must match GpuComponent and GpuMask in masks.rs, word for word.
struct MaskComponent
{
    uint kind;      // 0 none, 1 linear, 2 radial, 3 brush (part B)
    uint flags;     // bit 0: subtract; bit 1: invert
    float inner;
    float offset;
    float2 centre;
    float2 axisX;   // linear: the gradient
    float2 axisY;
    float2 pad;
};

struct MaskData
{
    uint count;
    float opacity;
    float exposure;
    float3 wbLog2;
    float contrast, highlights, shadows, whites, blacks, clarity, saturation;
    float3 pad;
    MaskComponent components[8];
};

// The buffer holds the eight masks (masks.rs `GpuMaskSet::masks`); maskCount says how many are in use.
#ifdef MASKS
StructuredBuffer<MaskData> maskSet : register(t6);
// The brush components' layers (brushgpu.rs), a slice each, over the whole frame: sampled at uv_in.
Texture2DArray<float> brushLayers : register(t7);

// The masks' sliders weighted at one pixel and summed (masks.rs `Local`).
struct MaskLocal
{
    float exposure;
    float3 wbLog2;
    float contrast, highlights, shadows, whites, blacks, clarity, saturation;
};

float componentWeight(MaskComponent c, float2 p, float2 uvIn)
{
    float w = 0.0;
    if (c.kind == 1)
    {
        float t = saturate(dot(p, c.axisX) + c.offset);
        w = 1.0 - t * t * (3.0 - 2.0 * t);
    }
    else if (c.kind == 2)
    {
        float2 d = p - c.centre;
        float2 q = float2(dot(d, c.axisX), dot(d, c.axisY));
        float r2 = dot(q, q);
        // Past the ellipse (most pixels, for a small shape) is 0 without the square root.
        if (r2 < 1.0)
        {
            float t = saturate((sqrt(r2) - c.inner) / (1.0 - c.inner));
            w = 1.0 - t * t * (3.0 - 2.0 * t);
        }
        w = (c.flags & 2) != 0 ? 1.0 - w : w;
    }
    else if (c.kind == 3 && c.offset >= 0.0)
    {
        w = brushLayers.SampleLevel(linearClamp, float3(uvIn, c.offset), 0);
    }
    return w;
}

float maskWeight(uint m, float2 p, float2 uvIn)
{
    float add = 1.0, sub = 1.0;
    uint n = maskSet[m].count;
    [loop] for (uint i = 0; i < n; i++)
    {
        MaskComponent c = maskSet[m].components[i];
        float w = componentWeight(c, p, uvIn);
        if ((c.flags & 1) != 0)
            sub *= 1.0 - w;
        else
            add *= 1.0 - w;
    }
    return (1.0 - add) * sub * maskSet[m].opacity;
}

MaskLocal evalMasks(float2 p, float2 uvIn)
{
    MaskLocal l = (MaskLocal)0;
    uint n = min(maskCount, 8u);
    [loop] for (uint m = 0; m < n; m++)
    {
        float w = maskWeight(m, p, uvIn);
        if (w <= 0.0)
            continue;
        MaskData d = maskSet[m];
        l.exposure += w * d.exposure;
        l.wbLog2 += w * d.wbLog2;
        l.contrast += w * d.contrast;
        l.highlights += w * d.highlights;
        l.shadows += w * d.shadows;
        l.whites += w * d.whites;
        l.blacks += w * d.blacks;
        l.clarity += w * d.clarity;
        l.saturation += w * d.saturation;
    }
    return l;
}
#endif
RWTexture2D<unorm float4> output : register(u0);

static const float3x3 REC2020_TO_SRGB = float3x3(
    1.660491, -0.5876411, -0.0728499,
    -0.1245505, 1.1328999, -0.0083494,
    -0.0181508, -0.1005789, 1.1187297);

// OkLab (Björn Ottosson): a perceptual color space where scaling a and b changes how colorful
// something is without shifting its hue or lightness. Works on linear sRGB.
float3 toOkLab(float3 c)
{
    float3 lms = float3(
        0.4122214708 * c.r + 0.5363325363 * c.g + 0.0514459929 * c.b,
        0.2119034982 * c.r + 0.6806995451 * c.g + 0.1073969566 * c.b,
        0.0883024619 * c.r + 0.2817188376 * c.g + 0.6299787005 * c.b);
    lms = sign(lms) * pow(abs(lms), 1.0 / 3.0);
    return float3(
        0.2104542553 * lms.x + 0.7936177850 * lms.y - 0.0040720468 * lms.z,
        1.9779984951 * lms.x - 2.4285922050 * lms.y + 0.4505937099 * lms.z,
        0.0259040371 * lms.x + 0.7827717662 * lms.y - 0.8086757660 * lms.z);
}

float3 fromOkLab(float3 lab)
{
    float3 lms = float3(
        lab.x + 0.3963377774 * lab.y + 0.2158037573 * lab.z,
        lab.x - 0.1055613458 * lab.y - 0.0638541728 * lab.z,
        lab.x - 0.0894841775 * lab.y - 1.2914855480 * lab.z);
    lms = lms * lms * lms;
    return float3(
        4.0767416621 * lms.x - 3.3077115913 * lms.y + 0.2309699292 * lms.z,
        -1.2684380046 * lms.x + 2.6097574011 * lms.y - 0.3413193965 * lms.z,
        -0.0041960863 * lms.x - 0.7034186147 * lms.y + 1.7076147010 * lms.z);
}

// Saturation scales every color's colorfulness alike; vibrance mostly the muted ones, so it
// can enrich a scene without pushing already-strong colors over.
float3 presence(float3 rgb, float saturation)
{
    if (saturation == 0.0 && vibrance == 0.0)
        return rgb;
    float3 lab = toOkLab(rgb);
    float chroma = length(lab.yz);
    float scale = (1.0 + saturation) * (1.0 + vibrance * (1.0 - smoothstep(0.0, 0.2, chroma)));
    lab.yz *= max(scale, 0.0);
    return fromOkLab(lab);
}


// ---- The colour mixer (core/src/mixer.rs is the reference; docs/phase2c.md) ----

// Each range's centre in OkLCh hue (degrees): Red, Orange, Yellow, Green, Aqua, Blue, Purple, Magenta.
static const float MIXER_CENTRES[8] = { 29.23, 52.78, 109.77, 142.50, 194.77, 264.05, 293.77, 328.36 };

float mixerGap(uint i)
{
    return (i == 7 ? MIXER_CENTRES[0] + 360.0 : MIXER_CENTRES[i + 1]) - MIXER_CENTRES[i];
}

bool inGamut(float3 c)
{
    return all(c >= -1e-5) && all(c <= 1.00001);
}

float3 lchToRgb(float l, float c, float h)
{
    return fromOkLab(float3(l, c * cos(h), c * sin(h)));
}

// Hue, saturation and luminance for eight colour ranges, in OkLCh. A pixel's hue picks its two
// nearest ranges (raised-cosine weights summing to 1); chroma decides how much applies at all.
float3 mixer(float3 rgb)
{
    MixerParams p = params[0].mixer;
    rgb = saturate(rgb);
    float3 lab = toOkLab(rgb);
    float chroma = length(lab.yz);
    float g = smoothstep(0.005, 0.06, chroma);
    if (g <= 0.0)
        return rgb;
    float hue = degrees(atan2(lab.z, lab.y));
    hue += hue < 0.0 ? 360.0 : 0.0;
    float h = hue < MIXER_CENTRES[0] ? hue + 360.0 : hue;
    uint i = 7;
    [unroll] for (int k = 6; k >= 0; k--)
        i = h < MIXER_CENTRES[k + 1] ? (uint)k : i;
    uint j = (i + 1) & 7;
    float span = mixerGap(i);
    float t = (h - MIXER_CENTRES[i]) / span;
    float up = 0.5 - 0.5 * cos(3.14159265 * t);
    float sat = lerp(p.saturation[i], p.saturation[j], up);
    float lum = lerp(p.luminance[i], p.luminance[j], up);
    // A cubic Hermite through the moved centres (monotonic: the CPU limits the slopes).
    float t2 = t * t, t3 = t2 * t;
    float moved = (2.0 * t3 - 3.0 * t2 + 1.0) * (MIXER_CENTRES[i] + p.hueShift[i])
                + (t3 - 2.0 * t2 + t) * span * p.hueSlope[i]
                + (-2.0 * t3 + 3.0 * t2) * (MIXER_CENTRES[i] + span + p.hueShift[j])
                + (t3 - t2) * span * p.hueSlope[j] - h;
    if (abs(moved) < 1e-3 && sat == 0.0 && lum == 0.0)
        return rgb;
    float hueTo = radians(hue + g * moved);
    float chromaTo = chroma * max(1.0 + g * sat, 0.0);
    // Linear luminance is about the cube of OkLab's lightness: one stop at +-100 for a saturated colour.
    float lightTo = saturate(lab.x * exp2(g * lum / 3.0));
    float3 outRgb = lchToRgb(lightTo, chromaTo, hueTo);
    if (!inGamut(outRgb))
    {
        // Lose chroma rather than clip, keeping hue and lightness.
        float lo = 0.0, hi = chromaTo;
        [unroll] for (int s = 0; s < 8; s++)
        {
            float mid = 0.5 * (lo + hi);
            if (inGamut(lchToRgb(lightTo, mid, hueTo))) lo = mid; else hi = mid;
        }
        outRgb = lchToRgb(lightTo, lo, hueTo);
    }
    return saturate(outRgb);
}

float3 srgbEncode(float3 c)
{
    return select(c <= 0.0031308, 12.92 * c, 1.055 * pow(c, 1.0 / 2.4) - 0.055);
}

#include "lens.hlsli"

// The sensor-image position for a corrected-image position; ca is the colour's chromatic
// aberration terms (zero for green).
float2 lensWarp(float2 uv, float2 dims, float3 ca, bool useCa)
{
    float2 q = (uv - 0.5) * dims * lensA.z;
    float r = length(q) / (0.5 * length(dims));
    float g = lensGreenRatio(r);
    if (useCa)
    {
        float rd2 = g * r * g * r;
        g *= 1.0 + ca.x + rd2 * (ca.y + rd2 * ca.z);
    }
    return 0.5 + q * g / dims;
}

Texture2D<float4> detailTex : register(t4); // the detail pass's output, when detailLevel != 0

// The image at a sensor position: from the detail texture (sharpened and denoised) where it
// covers, else from the pyramid. The detail texture is one pyramid level; where a screen pixel
// covers more than a texel, four taps average it. Positions past the covered rectangle's edge
// (the image's own edge, for a rectangle that reaches it) clamp to its last texel, as the
// pyramid does.
float3 sampleImage(float2 uv, float2 dims)
{
    if (detailLevel != 0)
    {
        uint level = detailLevel - 1;
        float2 p = uv * max(floor(dims / (float)(1u << level)), 1.0);
        float2 origin = float2(detailOrigin & 0xffff, detailOrigin >> 16);
        float2 size = float2(detailSize & 0xffff, detailSize >> 16);
        float2 q = p - origin;
        if (all(q >= 0.0) && all(q <= size))
        {
            float2 cap;
            detailTex.GetDimensions(cap.x, cap.y);
            float footprint = exp2(lod - (float)level);
            if (footprint < 1.2)
                return detailTex.SampleLevel(linearClamp, clamp(q, 0.5, size - 0.5) / cap, 0).rgb;
            float o = 0.25 * footprint;
            return 0.25 * (detailTex.SampleLevel(linearClamp, clamp(q + float2(-o, -o), 0.5, size - 0.5) / cap, 0).rgb
                         + detailTex.SampleLevel(linearClamp, clamp(q + float2(o, -o), 0.5, size - 0.5) / cap, 0).rgb
                         + detailTex.SampleLevel(linearClamp, clamp(q + float2(-o, o), 0.5, size - 0.5) / cap, 0).rgb
                         + detailTex.SampleLevel(linearClamp, clamp(q + float2(o, o), 0.5, size - 0.5) / cap, 0).rgb);
        }
    }
    return image.SampleLevel(linearClamp, uv, lod).rgb;
}

// Clarity (tone.rs `clarity_shift`): the change in stops for a pixel at x whose edge-preserving
// base is base, for c from -1 to 1.
float clarityShift(float x, float base, float c)
{
    float d = x - params[0].middle;
    float weight = smoothstep(-10.0, -4.0, d) * (1.0 - smoothstep(1.0, 5.5, d));
    float deviation = 2.0 * tanh((x - base) / 2.0);
    return c * (c > 0.0 ? 1.3 : 0.55) * weight * deviation;
}

[RootSignature(DEVELOP_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= outWidth || id.y >= outHeight)
        return;
    float2 at = id.xy + 0.5;
    float2 uv = origin + at.x * stepX + at.y * stepY;
    float2 uv_in = uv;
    // Outside the picture is canvas; a pixel less than one output pixel past the photo's edge is
    // still the photo (clamped), so cropping right up to a turned edge shows no dark fringe.
    float2 slack = abs(stepX) + abs(stepY);
    if (any(at < clip.xy) || any(at > clip.zw) || any(uv < -slack) || any(uv > 1.0 + slack))
    {
        output[id.xy] = float4(canvas.rgb, 1.0); // the empty canvas around the photo
        return;
    }
    if (mode == 1)
    {
        output[id.xy] = float4(image.SampleLevel(linearClamp, uv, lod).bgr, 1.0);
        return;
    }
    float3 texel;
    float2 dims;
    image.GetDimensions(dims.x, dims.y);
    if (lensA.x != 0.0)
    {
        uv = lensWarp(uv, dims, 0.0, false); // the tone base layer follows the sensor image too
        texel = sampleImage(uv, dims);
        if (lensA.w != 0.0)
        {
            texel.r = sampleImage(lensWarp(uv_in, dims, lensR.xyz, true), dims).r;
            texel.b = sampleImage(lensWarp(uv_in, dims, lensB.xyz, true), dims).b;
        }
    }
    else
    {
        texel = sampleImage(uv, dims);
    }
    // Vignetting works on scene light, before white balance and tone, at the radius of the
    // corrected frame (uv_in), so cropping doesn't move it.
    float3 cam = texel * exp2(vignetteStops(length((uv_in - 0.5) * dims) / (0.5 * length(dims))));
    // The masks' share of each slider at this pixel, in the lens-corrected frame (uv_in), in units of
    // the long edge. The sliders are the global ones plus it, held in range.
    Sliders sl = params[0].s;
    float satNow = saturation;
    float clarityNow = lum.w;
    float overlay = 0.0;
#ifdef MASKS
    if (maskCount != 0)
    {
        float2 pm = uv_in * dims / max(dims.x, dims.y);
        MaskLocal ml = evalMasks(pm, uv_in);
        cam *= exp2(ml.exposure + ml.wbLog2);
        sl.contrast = clamp(sl.contrast + ml.contrast, -1.0, 1.0);
        sl.highlights = clamp(sl.highlights + ml.highlights, -1.0, 1.0);
        sl.shadows = clamp(sl.shadows + ml.shadows, -1.0, 1.0);
        sl.whites = clamp(sl.whites + ml.whites, -1.0, 1.0);
        sl.blacks = clamp(sl.blacks + ml.blacks, -1.0, 1.0);
        clarityNow = clamp(clarityNow + ml.clarity, -1.0, 1.0);
        satNow = clamp(satNow + ml.saturation, -1.0, 1.0);
        if ((maskFlags & 2) != 0)
            overlay = maskWeight(maskFlags >> 8, pm, uv_in);
    }
#endif
    float3 wide = float3(dot(row0, cam), dot(row1, cam), dot(row2, cam));
    // Brightness in stops, moved by the sliders: the pixel's own for Contrast, Blacks and
    // Whites, and its surroundings' (the base) for Highlights and Shadows.
    float x = log2(max(dot(lum.xyz, cam), 1e-9));
    float xg = toneGlobal(x, sl);
    float2 baseDims;
    baseCoefficients.GetDimensions(baseDims.x, baseDims.y);
    float2 ab = baseCoefficients.SampleLevel(linearClamp, clamp(uv * baseScale, 0.5, baseSize - 0.5) / baseDims, 0);
    float base = ab.x * xg + ab.y;
    float shifted = xg + toneLocal(xg, base, float2(stats[0], stats[1]), sl);
    if (lum.w != 0.0 || (maskFlags & 1) != 0)
    {
        // Clarity's base: its copy's size is the scale rounded up (vignette.zw).
        float2 claritySize = ceil(vignette.zw);
        float2 clarityDims;
        clarityCoefficients.GetDimensions(clarityDims.x, clarityDims.y);
        float2 ab2 = clarityCoefficients.SampleLevel(linearClamp, clamp(uv * vignette.zw, 0.5, claritySize - 0.5) / clarityDims, 0);
        shifted += clarityShift(xg, ab2.x * xg + ab2.y, clarityNow);
    }
    float3 rgb = mul(REC2020_TO_SRGB, toneRgb(wide * exp2(shifted - x)));
    if (canvas.w != 0.0) // set when any colour mixer slider is not zero
        rgb = mixer(rgb);
    rgb = saturate(presence(rgb, satNow));
    float3 display = srgbEncode(rgb);
    if (overlay > 0.0)
        display = lerp(display, float3(0.9, 0.05, 0.05), 0.55 * overlay); // the mask being edited, in red
    output[id.xy] = float4(display, 1.0);
}
