// Tone pre-pass, step 2: the guided filter (He et al.) on the small copy, as base_layer in
// tone.rs does it. Writes the coefficients a and b: a pixel of tone x has base a·x + b.
//
// Run four times, with a pass number in the constants (a box mean along rows, then columns, then
// rows and columns again), one thread per pixel. The scratch textures hold two quantities side
// by side (the second at x + width).

#include "tone.hlsli"

cbuffer Constants : register(b0)
{
    uint2 size;
    uint radius;
    uint pass;
    float eps;   // the guided filter's epsilon, in stops²: how big an edge it keeps
};

RWTexture2D<float> smallCopy : register(u0);
RWTexture2D<float> scratchA : register(u1);
RWTexture2D<float> scratchB : register(u2);
RWTexture2D<float2> coefficients : register(u3);

float2 readPair(RWTexture2D<float> t, uint2 at)
{
    return float2(t[at], t[at + uint2(size.x, 0)]);
}

void writePair(RWTexture2D<float> t, uint2 at, float2 v)
{
    t[at] = v.x;
    t[at + uint2(size.x, 0)] = v.y;
}

// What each pass averages: v and v² of the small copy, then the previous pass's output.
float2 fetch(uint2 at)
{
    if (pass == 0)
    {
        float v = smallCopy[at];
        return float2(v, v * v);
    }
    if (pass == 2)
        return readPair(scratchB, at);
    return readPair(scratchA, at);
}

void store(uint2 at, float2 mean)
{
    if (pass == 1)
    {
        // mean = (mean of v, mean of v²): the filter's a and b.
        float variance = max(mean.y - mean.x * mean.x, 0.0);
        float a = variance / (variance + eps);
        writePair(scratchB, at, float2(a, mean.x - a * mean.x));
    }
    else if (pass == 3)
    {
        coefficients[at] = mean;
    }
    else
    {
        writePair(scratchA, at, mean);
    }
}

[RootSignature(TONE_ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= size.x || id.y >= size.y)
        return;
    // Along rows on the first and third passes, down columns on the others; the window shrinks
    // at the edges.
    bool horizontal = (pass & 1) == 0;
    uint at = horizontal ? id.x : id.y;
    uint len = horizontal ? size.x : size.y;
    uint lo = at > radius ? at - radius : 0;
    uint hi = min(at + radius + 1, len);
    float2 sum = 0.0;
    for (uint i = lo; i < hi; i++)
        sum += fetch(horizontal ? uint2(i, id.y) : uint2(id.x, i));
    store(id.xy, sum / (hi - lo));
}
