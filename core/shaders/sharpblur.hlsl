// Gaussian blurs of log luminance, one direction per pass: up to three at once (sharpening's, and
// the two of Texture's band), each in its own channel. The horizontal pass reads the colour input
// and writes the blurred rows; the vertical pass reads those. A sigma of 0 leaves its channel 0.

#include "detail.hlsli"

Texture2D<float4> input : register(t0);
Texture2D<float4> rows : register(t1); // (the three blurs in xyz)
RWTexture2D<float4> output : register(u0);

[RootSignature(DETAIL_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (any(int2(id.xy) >= regionSize))
        return;
    int2 p = int2(id.xy);
    float3 sigma = float3(sharp.x, thr.x, thr.y);
    int3 taps = int3(sharp.w, thr.z, thr.w);
    int reach = max(taps.x, max(taps.y, taps.z));
    float3 inv = -0.5 / max(sigma * sigma, 1e-6);
    float3 sum = 0, weight = 0;
    for (int k = -reach; k <= reach; k++)
    {
        float3 v;
        if (flags & FLAG_VERTICAL)
            v = rows.Load(int3(clamp(p + int2(0, k), 0, regionSize - 1), 0)).xyz;
        else
            v = logLuminance(loadIn(input, p + int2(k, 0)));
        float3 w = select(and(abs(k) <= taps, sigma > 0.0), exp(inv * k * k), 0.0);
        sum += w * v;
        weight += w;
    }
    output[id.xy] = float4(sum / max(weight, 1e-9), 0);
}
