// The detail a scale holds is the difference between it and the next, coarser one. Here it is
// shrunk and added to the running sum; the last scale writes the denoised image (the smooth
// remainder plus the shrunk details).
//
// The shrink looks at the detail's neighbourhood, not the coefficient alone: its energy over 3 x 3
// pixels (the centre counts double). A lone large coefficient in a quiet neighbourhood is noise
// that happened to be big, so it is removed; texture and edges are coherent, so they stay.
//   - Luminance: factor = max(0, 1 - (threshold / energy)^p) on the coefficient.
//   - Colour: the two axes shrink together (a vector, in units of their thresholds), so hue
//     isn't skewed. Where luminance has an edge the colour threshold falls, so a saturated edge
//     (a red sign against white) isn't smeared into its surroundings.

#include "detail.hlsli"

Texture2D<float4> scale : register(t0);     // the current scale: the image, or an earlier blur
Texture2D<float4> nextScale : register(t1); // the next scale
Texture2D<float4> sumIn : register(t2);     // the shrunk detail so far
RWTexture2D<float4> sumOut : register(u0);  // the sum, or at the last scale the denoised image
RWTexture2D<float4> output : register(u1);  // the detail texture, when this pass writes it

float3 detailAt(int2 p)
{
    float3 current = loadIn(scale, p);
    if (flags & FLAG_STABILISE)
        current = stabilise(current);
    return current - loadRegion(nextScale, p);
}

[RootSignature(DETAIL_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (any(int2(id.xy) >= regionSize))
        return;
    int2 p = int2(id.xy);
    float3 centre = detailAt(p);
    // Weighted energy: luminance squared, and colour squared in units of the thresholds.
    float eY = 2.0 * centre.x * centre.x;
    float2 n0 = centre.yz / max(thr.yz, 1e-9);
    float eC = 2.0 * dot(n0, n0);
    [unroll] for (int j = -1; j <= 1; j++)
        [unroll] for (int i = -1; i <= 1; i++)
        {
            if (i == 0 && j == 0)
                continue;
            float3 d = detailAt(p + int2(i, j));
            eY += d.x * d.x;
            float2 n = d.yz / max(thr.yz, 1e-9);
            eC += dot(n, n);
        }
    eY = sqrt(eY / 10.0);
    eC = sqrt(eC / 10.0);

    float3 s = centre;
    if (thr.x > 0.0)
        s.x = (eY > 0.0) ? centre.x * max(0.0, 1.0 - pow(thr.x / eY, pw.x)) : 0.0;
    if (thr.y > 0.0)
    {
        // Colour thresholds relax where luminance has structure: down to 30% at a strong edge.
        float guide = 0.3 + 0.7 / (1.0 + pow(eY / max(4.0 * thr.w, 1e-9), 2.0));
        float f = (eC > 0.0) ? max(0.0, 1.0 - pow(guide / eC, pw.y)) : 0.0;
        s.yz = centre.yz * f;
    }

    float3 sum = ((flags & FLAG_ACC) ? loadRegion(sumIn, p) : 0) + s;
    if (flags & FLAG_LAST)
    {
        float3 rgb = unstabilise(loadRegion(nextScale, p) + sum);
        if (flags & FLAG_OUT)
        {
            if (all(p >= outRect.xy) && all(p < outRect.zw))
                output[p + outOffset] = float4(rgb, 1);
        }
        else
        {
            sumOut[p] = float4(rgb, 1);
        }
    }
    else
    {
        sumOut[p] = float4(sum, 0);
    }
}
