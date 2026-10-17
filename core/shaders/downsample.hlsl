// One level of the image pyramid: each pixel averages a 2×2 block of the level above.
// The pyramid is what lets edits run at screen size instead of sensor size.

#include "common.hlsli"

cbuffer Constants : register(b0)
{
    uint dstWidth;
    uint dstHeight;
    uint srcWidth;
    uint srcHeight;
};

Texture2D<float4> source : register(t0);
RWTexture2D<float4> output : register(u0);

[RootSignature(ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= dstWidth || id.y >= dstHeight)
        return;
    uint2 s = id.xy * 2;
    uint2 last = uint2(srcWidth - 1, srcHeight - 1);
    float4 sum = source[s]
               + source[min(s + uint2(1, 0), last)]
               + source[min(s + uint2(0, 1), last)]
               + source[min(s + uint2(1, 1), last)];
    output[id.xy] = sum * 0.25;
}
