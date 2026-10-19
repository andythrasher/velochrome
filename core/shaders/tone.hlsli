// The tone model on the GPU: the same functions as core/src/tone.rs, which is the reference.
// Every number comes from the parameter buffer (t1), written from tone::MODEL, so there is
// nothing to keep in step here but the structure.

// Must match ToneParams in pipeline.rs, field for field.
struct Model
{
    float contrast;
    float blacksDown, blacksUp, blacksUpGrowth, blacksPower;
    float highlightsFloor, highlightsAmount, highlightsFrom, highlightsTo, highlightsUp;
    float shadowsSlope, shadowsKnee, shadowsSoft, shadowsReach, shadowsTarget;
    float whitesDown, whitesDownFrom, whitesDownTo, whitesDownPower;
    float whitesUp, whitesUpFrom, whitesUpTo, whitesUpPower;
    float whitesStretchFloor, whitesStretchPower;
    float baseRadius, baseEps;
};

struct Sliders
{
    float contrast, highlights, shadows, whites, blacks; // -1..1
};

// The colour mixer (mixer.rs Params): per range, Red to Magenta.
struct MixerParams
{
    float hueShift[8];  // degrees the range centre moves
    float hueSlope[8];  // the hue map's slope at each centre
    float saturation[8]; // -1..1
    float luminance[8];  // -1..1
};

#ifdef PROFILE
struct ProfileParams
{
    uint3 hueSatDims;
    uint hueSatEncoding;
    uint3 lookDims;
    uint lookEncoding;
    uint hasToneCurve;
    float exposureGain;
};
#endif

struct ToneParams
{
    Model m;
    Sliders s;
    float n, log2Sigma, ymax, middle, nearWhite; // the base curve's fixed numbers
    MixerParams mixer;
#ifdef PROFILE
    ProfileParams profile;
#endif
};

StructuredBuffer<ToneParams> params : register(t1);

// The tone pre-pass kernels share one table: the image (t0) and parameters (t1), then the small
// copy (u0), two scratch textures (u1, u2), the guided filter's coefficients (u3) and the
// statistics (u4).
#define TONE_ROOT_SIGNATURE \
    "RootConstants(num32BitConstants=32, b0)," \
    "DescriptorTable(SRV(t0, numDescriptors=2), UAV(u0, numDescriptors=6))," \
    "StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR," \
    " addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP)"

// Contrast then Blacks, on log2 brightness, for the sliders sl: the global ones, or with a mask's
// share added (develop.hlsl).
float toneGlobal(float x, Sliders sl)
{
    ToneParams p = params[0];
    x = p.middle + (x - p.middle) * exp2(p.m.contrast * sl.contrast);
    float lin = exp2(x);
    float amount = pow(abs(sl.blacks), p.m.blacksPower);
    if (sl.blacks < 0.0)
    {
        float b = p.m.blacksDown * amount;
        lin = max(lin - b, 1e-9) / (1.0 - b);
    }
    else
    {
        lin += p.m.blacksUp * amount * pow(lin / exp2(p.middle), p.m.blacksUpGrowth);
    }
    return log2(lin);
}

float toneGlobal(float x)
{
    return toneGlobal(x, params[0].s);
}

// Highlights, Shadows and Whites: the change in stops for a pixel at x whose base is base, for
// the sliders s. stats = (99.5th percentile, 25th percentile) of the image's tones.
float toneLocal(float x, float base, float2 stats, Sliders s)
{
    ToneParams p = params[0];
    Model m = p.m;
    float dx = 0.0;
    float d = base - p.middle;
    float h = m.highlightsFloor + m.highlightsAmount * smoothstep(m.highlightsFrom, m.highlightsTo, d);
    dx += s.highlights * (s.highlights > 0.0 ? m.highlightsUp * h : h);
    float w = m.shadowsSoft;
    float depth = s.shadows > 0.0 ? clamp(m.shadowsTarget - (stats.y - p.middle), 0.0, m.shadowsReach) : 0.0;
    // The exponent is capped: beyond it the result is far over the 3.5 stop limit anyway.
    float soft = min(m.shadowsSlope, 0.75) * w * log(1.0 + exp(min((m.shadowsKnee + depth - d) / w, 40.0)));
    dx += s.shadows * min(soft, 3.5);
    d = x - p.middle;
    if (s.whites < 0.0)
    {
        dx -= pow(-s.whites, m.whitesDownPower) * m.whitesDown * smoothstep(m.whitesDownFrom, m.whitesDownTo, d);
    }
    else if (s.whites > 0.0)
    {
        dx += pow(s.whites, m.whitesUpPower) * m.whitesUp * smoothstep(m.whitesUpFrom, m.whitesUpTo, d);
        float headroom = max(p.nearWhite - stats.x, 0.0);
        float reach = m.whitesStretchFloor + (1.0 - m.whitesStretchFloor) * smoothstep(-6.0, 0.5, x - stats.x);
        dx += pow(s.whites, m.whitesStretchPower) * headroom * reach;
    }
    return dx;
}

// Display light for linear curve input s.
float baseCurve(float s)
{
    if (s <= 0.0)
        return 0.0;
    ToneParams p = params[0];
    float r = (log2(s) - p.log2Sigma) * p.n;
    return min(p.ymax / (1.0 + exp2(-r)), 1.0);
}

// The base curve with hue kept: the largest and smallest channels go through it, and the middle
// one keeps its place between them.
float3 toneRgb(float3 rgb)
{
    float3 c = max(rgb, 0.0);
    float hi = max(c.r, max(c.g, c.b));
    float lo = min(c.r, min(c.g, c.b));
    if (hi <= lo)
        return baseCurve(hi);
    float hiOut = baseCurve(hi);
    float loOut = baseCurve(lo);
    return loOut + (hiOut - loOut) * (c - lo) / (hi - lo);
}
