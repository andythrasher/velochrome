// The vertical pass of the à trous B3 blur: the next scale from the horizontally blurred rows.

#include "detail.hlsli"

Texture2D<float4> blurred : register(t0);
RWTexture2D<float4> nextScale : register(u0);

[RootSignature(DETAIL_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (any(int2(id.xy) >= regionSize))
        return;
    float3 next = 0;
    [unroll] for (int k = -2; k <= 2; k++)
        next += B3[k + 2] * loadRegion(blurred, int2(id.xy) + int2(0, k * (int)step));
    nextScale[id.xy] = float4(next, 0);
}
