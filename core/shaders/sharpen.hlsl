// Sharpening and Texture, both on log2 luminance, written as a change of stops that scales the
// pixel's colour (so hues don't shift and a halo is as strong in the shadows as in the highlights).
//
// Sharpening (an unsharp mask):
//   - Coring: the difference from the blur is soft-thresholded at what sensor noise would make of
//     it (the photo's noise estimate, in log units at this brightness), so flat areas aren't
//     grainier than without sharpening while edges still sharpen. Detail sets how far it goes.
//   - Detail also limits the difference (a soft limit, in stops) against halos.
//   - Masking weights the effect by the edge strength (the gradient of the blurred luminance).
//
// Texture: the band between two blurs (about 2 and 8 pixels), boosted or removed.
//   - Edge-aware: weighted by 1 / (1 + (band / tau)^2), so strong edges (a roof against the sky)
//     get no halo, and a negative Texture smooths skin-like detail without blurring edges.
//   - A positive Texture is cored at the band's noise, so it doesn't bring grain back.

#include "detail.hlsli"

Texture2D<float4> input : register(t0);
Texture2D<float4> blurred : register(t1); // (sharpening's blur, and the band's two, in xyz)
RWTexture2D<float4> output : register(u0);

float noiseInStops(float blurredLog, float k)
{
    float y = max(exp2(blurredLog) - LOG_FLOOR, 0.0);
    return k * 2.0 * sqrt(y + STAB) / ((y + LOG_FLOOR) * 0.6931472);
}

[RootSignature(DETAIL_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (any(int2(id.xy) >= regionSize))
        return;
    int2 p = int2(id.xy);
    if (any(p < outRect.xy) || any(p >= outRect.zw))
        return;
    float3 rgb = loadIn(input, p);
    float l = logLuminance(rgb);
    float3 bl = blurred.Load(int3(clamp(p, 0, regionSize - 1), 0)).xyz;

    float sharpTerm = 0.0;
    if (sharp.x > 0.0)
    {
        float gx = blurred.Load(int3(clamp(p + int2(1, 0), 0, regionSize - 1), 0)).x - blurred.Load(int3(clamp(p - int2(1, 0), 0, regionSize - 1), 0)).x;
        float gy = blurred.Load(int3(clamp(p + int2(0, 1), 0, regionSize - 1), 0)).x - blurred.Load(int3(clamp(p - int2(0, 1), 0, regionSize - 1), 0)).x;
        float edge = 0.5 * length(float2(gx, gy));
        float weight = lerp(1.0, smoothstep(0.0, mask.x, edge), mask.y);
        if (flags & FLAG_MASK)
        {
            output[p + outOffset] = float4(weight, weight, weight, 1);
            return;
        }
        float diff = l - bl.x;
        diff = sign(diff) * max(abs(diff) - noiseInStops(bl.x, mask.z), 0.0);
        float limit = sharp.z;
        sharpTerm = sharp.y * weight * limit * tanh(diff / limit);
    }

    float textureTerm = 0.0;
    if (pw.x != 0.0)
    {
        float band = bl.y - bl.z;
        float shown = band;
        if (pw.x > 0.0)
            shown = sign(band) * max(abs(band) - noiseInStops(bl.z, mask.w), 0.0);
        float edgeWeight = 1.0 / (1.0 + pow(abs(band) / pw.y, 2.0));
        textureTerm = pw.x * edgeWeight * shown;
    }
    float gain = exp2(clamp(sharpTerm + textureTerm, -3.0, 3.0));
    output[p + outOffset] = float4(rgb * gain, 1);
}
