// Tone pre-pass, step 1: the small copy. Each pixel of a ~128-pixel image holds the mean tone
// (log2 brightness after Contrast and Blacks) of its block of the photo, read from a coarse
// pyramid level, so this never touches the full-resolution image. The vignetting gain is in it,
// so the base layer and the percentiles see what the develop pass will draw.

#include "tone.hlsli"

cbuffer Constants : register(b0)
{
    float4 lum;      // camera RGB → brightness (sRGB luminance of the developed pixel)
    float4 lensA;    // lens correction, as in develop.hlsl (needed to find the corrected radius)
    float4 lensK;
    float4 vignette; // manual lens vignetting, as in develop.hlsl
    uint2 smallSize;
    uint2 srcSize;   // the photo, in pixels, before rotation
    float factor;    // photo pixels per small-copy pixel
    float exposure;  // the Exposure slider, taken back out for the as-shot copy
};

Texture2D<float4> image : register(t0);
SamplerState linearClamp : register(s0);
RWTexture2D<float> smallCopy : register(u0);
RWTexture2D<float> scene : register(u5); // the tones as shot, for the percentiles

#include "lens.hlsli"

[RootSignature(TONE_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= smallSize.x || id.y >= smallSize.y)
        return;
    float2 lo = id.xy * factor;
    float2 hi = min(lo + factor, float2(srcSize));
    float sum = 0.0, shot = 0.0;
    [unroll] for (int j = 0; j < 4; j++)
    {
        [unroll] for (int i = 0; i < 4; i++)
        {
            float2 uv = lerp(lo, hi, (float2(i, j) + 0.5) / 4.0) / float2(srcSize);
            float3 cam = image.SampleLevel(linearClamp, uv, 0).rgb;
            float x = log2(max(dot(lum.xyz, cam), 1e-9)) + vignetteStops(correctedRadius(uv, float2(srcSize)));
            sum += toneGlobal(x);
            shot += x;
        }
    }
    smallCopy[id.xy] = sum / 16.0;
    scene[id.xy] = shot / 16.0 - exposure;
}
