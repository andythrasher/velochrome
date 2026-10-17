// Tone pre-pass, step 3: the 99.5th and 25th percentiles of the photo's tones as shot, which steer
// Whites and Shadows. A histogram in group shared memory, then a prefix sum to find the bins.
// One thread group.

#include "tone.hlsli"

cbuffer Constants : register(b0)
{
    uint2 size;
};

static const uint BINS = 2048;
static const float LOW = -24.0;   // stops: the range the histogram covers
static const float HIGH = 12.0;

RWTexture2D<float> scene : register(u5); // the tones as shot (tonesmall.hlsl)
RWBuffer<float> stats : register(u4);

groupshared uint histogram[BINS];
groupshared uint running[BINS / 2]; // cumulative count through each pair of bins

// The value at sorted position k, placed within its bin. `before` counts the pairs below t.
float locate(uint k, uint t, uint before)
{
    uint bin = 2 * t;
    uint first = histogram[bin];
    if (before + first <= k)
    {
        before += first;
        bin++;
    }
    float frac = (k - before + 0.5) / histogram[bin];
    return LOW + (bin + frac) * (HIGH - LOW) / BINS;
}

[RootSignature(TONE_ROOT_SIGNATURE)]
[numthreads(BINS / 2, 1, 1)]
void main(uint3 id : SV_GroupThreadID)
{
    uint t = id.x;
    histogram[2 * t] = 0;
    histogram[2 * t + 1] = 0;
    GroupMemoryBarrierWithGroupSync();

    uint n = size.x * size.y;
    for (uint i = t; i < n; i += BINS / 2)
    {
        float x = scene[uint2(i % size.x, i / size.x)];
        uint bin = (uint)clamp((x - LOW) / (HIGH - LOW) * BINS, 0.0, BINS - 1.0);
        InterlockedAdd(histogram[bin], 1);
    }
    GroupMemoryBarrierWithGroupSync();

    // Inclusive prefix sum of the pair totals.
    uint v = histogram[2 * t] + histogram[2 * t + 1];
    running[t] = v;
    GroupMemoryBarrierWithGroupSync();
    for (uint d = 1; d < BINS / 2; d *= 2)
    {
        uint add = t >= d ? running[t - d] : 0;
        GroupMemoryBarrierWithGroupSync();
        v += add;
        running[t] = v;
        GroupMemoryBarrierWithGroupSync();
    }

    // The sorted positions tone.rs picks.
    uint kTop = (uint)((n - 1) * 0.995);
    uint kDark = (uint)((n - 1) * 0.25);
    uint before = t > 0 ? running[t - 1] : 0;
    if (before <= kTop && kTop < running[t])
        stats[0] = locate(kTop, t, before);
    if (before <= kDark && kDark < running[t])
        stats[1] = locate(kDark, t, before);
}
