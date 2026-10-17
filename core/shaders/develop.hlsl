// The view pass: runs for every slider change, once per screen pixel. It develops a raw, or
// shows one of the camera's JPEGs (mode 1) through the same zoom and rotation.
//
// It samples the cached pyramid at the level matching the zoom, so its cost depends on the
// window size, not the sensor size. White balance, exposure and both color conversions are
// folded into one matrix on the CPU, and the tone sliders into one curve table, so this stays
// a few dozen instructions per pixel.

// Like the other kernels', but with two inputs: the image, then the tone curve.
#define DEVELOP_ROOT_SIGNATURE \
    "RootConstants(num32BitConstants=32, b0)," \
    "DescriptorTable(SRV(t0, numDescriptors=2), UAV(u0))," \
    "StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR," \
    " addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP)"

cbuffer Constants : register(b0)
{
    float4 row0;    // camera RGB → linear sRGB; white balance and exposure included
    float4 row1;
    float4 row2;
    float2 origin;  // image coordinates (0..1) at output pixel (0, 0)
    float2 stepX;   // change in image coordinates per output pixel to the right
    float2 stepY;   // ... and per output pixel down
    float lod;      // pyramid level to sample
    uint mode;      // 0: develop a raw; 1: show a camera JPEG (BGRA, already display-ready)
    uint outWidth;
    uint outHeight;
    float saturation; // -1..1
    float vibrance;   // -1..1
};

// Must match develop.rs.
static const uint CURVE_SIZE = 1024;
static const float CURVE_MIN_EV = -16.0;
static const float CURVE_MAX_EV = 6.0;

Texture2D<float4> image : register(t0);
Buffer<float> curve : register(t1);
SamplerState linearClamp : register(s0);
RWTexture2D<unorm float4> output : register(u0);

// The tone curve for one channel: looked up by stops, linear below the table's range.
float tone(float x)
{
    if (x <= 0.0)
        return 0.0;
    float ev = log2(x);
    if (ev <= CURVE_MIN_EV)
        return curve[0] * x / exp2(CURVE_MIN_EV);
    float t = saturate((ev - CURVE_MIN_EV) / (CURVE_MAX_EV - CURVE_MIN_EV)) * (CURVE_SIZE - 1);
    uint i = min((uint)t, CURVE_SIZE - 2);
    return lerp(curve[i], curve[i + 1], t - i);
}

// OkLab (Björn Ottosson): a perceptual color space where scaling a and b changes how colorful
// something is without shifting its hue or lightness. Works on linear sRGB.
float3 toOkLab(float3 c)
{
    float3 lms = float3(
        0.4122214708 * c.r + 0.5363325363 * c.g + 0.0514459929 * c.b,
        0.2119034982 * c.r + 0.6806995451 * c.g + 0.1073969566 * c.b,
        0.0883024619 * c.r + 0.2817188376 * c.g + 0.6299787005 * c.b);
    lms = sign(lms) * pow(abs(lms), 1.0 / 3.0);
    return float3(
        0.2104542553 * lms.x + 0.7936177850 * lms.y - 0.0040720468 * lms.z,
        1.9779984951 * lms.x - 2.4285922050 * lms.y + 0.4505937099 * lms.z,
        0.0259040371 * lms.x + 0.7827717662 * lms.y - 0.8086757660 * lms.z);
}

float3 fromOkLab(float3 lab)
{
    float3 lms = float3(
        lab.x + 0.3963377774 * lab.y + 0.2158037573 * lab.z,
        lab.x - 0.1055613458 * lab.y - 0.0638541728 * lab.z,
        lab.x - 0.0894841775 * lab.y - 1.2914855480 * lab.z);
    lms = lms * lms * lms;
    return float3(
        4.0767416621 * lms.x - 3.3077115913 * lms.y + 0.2309699292 * lms.z,
        -1.2684380046 * lms.x + 2.6097574011 * lms.y - 0.3413193965 * lms.z,
        -0.0041960863 * lms.x - 0.7034186147 * lms.y + 1.7076147010 * lms.z);
}

// Saturation scales every color's colorfulness alike; vibrance mostly the muted ones, so it
// can enrich a scene without pushing already-strong colors over.
float3 presence(float3 rgb)
{
    if (saturation == 0.0 && vibrance == 0.0)
        return rgb;
    float3 lab = toOkLab(rgb);
    float chroma = length(lab.yz);
    float scale = (1.0 + saturation) * (1.0 + vibrance * (1.0 - smoothstep(0.0, 0.2, chroma)));
    lab.yz *= max(scale, 0.0);
    return fromOkLab(lab);
}

float3 srgbEncode(float3 c)
{
    return select(c <= 0.0031308, 12.92 * c, 1.055 * pow(c, 1.0 / 2.4) - 0.055);
}

[RootSignature(DEVELOP_ROOT_SIGNATURE)]
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
    float4 texel = image.SampleLevel(linearClamp, uv, lod);
    if (mode == 1)
    {
        output[id.xy] = float4(texel.bgr, 1.0);
        return;
    }
    float3 cam = texel.rgb;
    float3 rgb = float3(dot(row0.xyz, cam), dot(row1.xyz, cam), dot(row2.xyz, cam));
    rgb = float3(tone(rgb.r), tone(rgb.g), tone(rgb.b));
    rgb = saturate(presence(rgb));
    output[id.xy] = float4(srgbEncode(rgb), 1.0);
}
