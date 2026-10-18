// Lens geometry shared by the develop pass and the tone pre-pass (see docs/lens.md and
// docs/phase2a.md; core/src/lens.rs and develop.rs do the same on the CPU). The including shader
// declares lensA (model, scale, zoom, chromatic aberration), lensK (distortion terms) and
// vignette (stops at the corners, radius where the change starts) first.

// The ratio of source radius to corrected radius for green, at corrected radius r (in
// half-diagonals).
float lensGreenRatio(float r)
{
    float scale = lensA.y;
    if (lensA.x == 1.0)
    {
        float rs2 = r * scale * r * scale;
        return scale * (1.0 + rs2 * (lensK.x + rs2 * (lensK.y + rs2 * lensK.z)));
    }
    // Panasonic: two rounds of rd = r / f(rd) invert the polynomial closely enough.
    float rd = r;
    [unroll] for (int i = 0; i < 2; i++)
    {
        float rd2 = rd * rd;
        float f = 1.0 + scale * rd2 * (lensK.x + rd2 * (lensK.y + rd2 * lensK.z));
        rd = f > 1e-6 ? r / f : r;
    }
    return r > 0.0 ? rd / r : 1.0;
}

// Manual vignetting: a gain in stops that grows from nothing at radius vignette.y to
// vignette.x at the corners (radius 1 of the lens-corrected frame, whatever the crop).
float vignetteStops(float r)
{
    if (vignette.x == 0.0)
        return 0.0;
    float t = smoothstep(vignette.y, 1.0, r);
    return vignette.x * pow(t, 1.5);
}

// The corrected-frame radius of a position in the sensor image (radius 0 at the centre, 1 at the
// corners), by inverting the lens warp with a few rounds of r = rs / ratio. For passes that
// read the sensor image directly.
float correctedRadius(float2 sensorUv, float2 dims)
{
    float rs = length((sensorUv - 0.5) * dims) / (0.5 * length(dims));
    if (lensA.x == 0.0)
        return rs;
    float r = rs;
    [unroll] for (int i = 0; i < 3; i++)
        r = rs / (lensA.z * lensGreenRatio(r * lensA.z));
    return r;
}
