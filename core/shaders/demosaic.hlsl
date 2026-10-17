// Bayer sensor data → white-balanced camera RGB, full resolution.
//
// Malvar-He-Cutler (2004): bilinear interpolation corrected by the local gradient of the color
// that was measured. Cheap (one 5×5 neighborhood per pixel) and far better than bilinear; a
// higher-quality demosaic (RCD, AMaZE) can replace it later without touching anything else,
// because this runs once per image and its result is cached.

#include "common.hlsli"

cbuffer Constants : register(b0)
{
    uint width;
    uint height;
    uint cfa;     // 2 bits per 2×2 position (row-major): 0 red, 1 green, 2 blue
    float white;
    float4 black; // per 2×2 position
    float4 wb;    // R, G, B multipliers, G = 1
};

Buffer<uint> raw : register(t0);
RWTexture2D<float4> output : register(u0);

uint colorAt(int x, int y)
{
    return (cfa >> (((y & 1) * 2 + (x & 1)) * 2)) & 3;
}

// One sensor value, black subtracted, scaled to 0..1 and white balanced. Edges mirror, which
// keeps the Bayer pattern intact.
float sensor(int x, int y)
{
    x = x < 0 ? -x : (x >= (int)width ? 2 * (int)width - 2 - x : x);
    y = y < 0 ? -y : (y >= (int)height ? 2 * (int)height - 2 - y : y);
    uint pos = (y & 1) * 2 + (x & 1);
    float v = (float)raw[y * width + x];
    return max(v - black[pos], 0.0) / (white - black[pos]) * wb[colorAt(x, y)];
}

[RootSignature(ROOT_SIGNATURE)]
[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= width || id.y >= height)
        return;
    int x = id.x, y = id.y;

    float c = sensor(x, y);
    // Same-row and same-column neighbors one and two pixels away, and diagonals.
    float n1 = sensor(x, y - 1) + sensor(x, y + 1);         // vertical, distance 1
    float w1 = sensor(x - 1, y) + sensor(x + 1, y);         // horizontal, distance 1
    float n2 = sensor(x, y - 2) + sensor(x, y + 2);         // vertical, distance 2
    float w2 = sensor(x - 2, y) + sensor(x + 2, y);         // horizontal, distance 2
    float d1 = sensor(x - 1, y - 1) + sensor(x + 1, y - 1)
             + sensor(x - 1, y + 1) + sensor(x + 1, y + 1); // diagonals

    uint here = colorAt(x, y);
    float3 rgb;
    if (here == 1)
    {
        // Green site: one of red/blue is left-right, the other up-down.
        float horiz = (5 * c + 4 * w1 - d1 - w2 + 0.5 * n2) / 8; // color on this row
        float vert = (5 * c + 4 * n1 - d1 - n2 + 0.5 * w2) / 8;  // color on this column
        uint rowColor = colorAt(x + 1, y);
        rgb = rowColor == 0 ? float3(horiz, c, vert) : float3(vert, c, horiz);
    }
    else
    {
        // Red or blue site: interpolate green, and the opposite color from the diagonals.
        float g = (4 * c + 2 * (n1 + w1) - (n2 + w2)) / 8;
        float other = (6 * c + 2 * d1 - 1.5 * (n2 + w2)) / 8;
        rgb = here == 0 ? float3(c, g, other) : float3(other, g, c);
    }

    // Clip to the level where every channel is still measured, so blown highlights stay neutral
    // instead of turning magenta. (Highlight reconstruction comes later.)
    output[id.xy] = float4(clamp(rgb, 0.0, 1.0), 1.0);
}
