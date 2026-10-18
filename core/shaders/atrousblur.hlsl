// One horizontal pass of the à trous B3 blur: five taps `step` texels apart. Scale 0 reads the
// image itself (FLAG_STABILISE); later scales read the previous scale's blur.

#include "detail.hlsli"

Texture2D<float4> input : register(t0);
RWTexture2D<float4> output : register(u0);

[RootSignature(DETAIL_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (any(int2(id.xy) >= regionSize))
        return;
    float3 sum = 0;
    [unroll] for (int k = -2; k <= 2; k++)
    {
        float3 c = loadIn(input, int2(id.xy) + int2(k * (int)step, 0));
        sum += B3[k + 2] * ((flags & FLAG_STABILISE) ? stabilise(c) : c);
    }
    output[id.xy] = float4(sum, 0);
}
