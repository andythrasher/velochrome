// The per-edit pass: runs for every slider change, once per screen pixel.
//
// It samples the cached pyramid at the level matching the zoom, so its cost depends on the
// window size, not the sensor size. White balance, exposure and both color conversions are
// folded into one matrix on the CPU, so this stays a handful of instructions per pixel.

#include "common.hlsli"

cbuffer Constants : register(b0)
{
    float4 row0;    // camera RGB → linear sRGB, exposure and white balance included
    float4 row1;
    float4 row2;
    float2 origin;  // image coordinates (0..1) at output pixel (0, 0)
    float2 stepX;   // change in image coordinates per output pixel to the right
    float2 stepY;   // ... and per output pixel down
    float lod;      // pyramid level to sample
    float pad0;
    uint outWidth;
    uint outHeight;
};

Texture2D<float4> image : register(t0);
SamplerState linearClamp : register(s0);
RWTexture2D<unorm float4> output : register(u0);

// Filmic shoulder (Narkowicz's fit of the ACES curve): gentle toe, highlights roll off instead
// of clipping. A placeholder for the real tone controls.
float3 tone(float3 x)
{
    return saturate((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14));
}

float3 srgbEncode(float3 c)
{
    return select(c <= 0.0031308, 12.92 * c, 1.055 * pow(c, 1.0 / 2.4) - 0.055);
}

[RootSignature(ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= outWidth || id.y >= outHeight)
        return;
    float2 uv = origin + (id.x + 0.5) * stepX + (id.y + 0.5) * stepY;
    if (any(uv < 0.0) || any(uv > 1.0))
    {
        output[id.xy] = float4(0.11, 0.11, 0.11, 1.0); // the empty canvas around the photo
        return;
    }
    float3 cam = image.SampleLevel(linearClamp, uv, lod).rgb;
    float3 rgb = float3(dot(row0.xyz, cam), dot(row1.xyz, cam), dot(row2.xyz, cam));
    rgb = tone(max(rgb, 0.0));
    output[id.xy] = float4(srgbEncode(rgb), 1.0);
}
