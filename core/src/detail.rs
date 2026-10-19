//! The detail pass (docs/phase2b.md): noise reduction and sharpening, which look at a pixel's
//! neighbours at full resolution. The develop pass runs per *screen* pixel, so these run before
//! it, on a region of one pyramid level, and write a **detail texture** the develop pass samples
//! instead of the pyramid:
//!
//! - The region is what the view covers (mapped back to the texture through the view's affine
//!   map and the lens warp) plus an apron for the filters' reach, at level `floor(lod)`.
//! - It runs only when its inputs change: the photo, the view, or a detail slider. White
//!   balance, tone and presence come after it in the develop pass, so dragging them costs nothing
//!   here (`DetailState::runs` counts the runs).
//! - Export tiles each get the pass at level 0 over their footprint plus the apron.
//! - All its memory is allocated once (`Resources`) and reused; it only grows if a view needs a
//!   bigger region than any before.
//!
//! Noise reduction is an à trous wavelet decomposition (B3 spline, five scales) of the image in
//! a variance-stabilised opponent space (a square root, then luminance and two colour axes), each
//! scale's detail shrunk by a threshold from a per-photo noise estimate, as the wavelet denoisers
//! of darktable and RawTherapee do (both GPL-3, like this program). Sharpening is an unsharp mask
//! on log2 luminance. The kernels are in `shaders/detail.hlsli` and the files it lists.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Result;

use crate::develop::{Detail, Edit};
use crate::geometry::ViewMap;
use crate::gpu::{Gpu, new_texture};
use crate::lens::{Channel, LensCorrection};
use crate::pipeline::{Image, Zoom};

/// Wavelet scales of the noise reduction.
pub const SCALES: usize = 5;

/// How far the filters reach, in texels of the level they work at, and so how much is worked
/// around a piece of output: the wavelets reach 2·(1+2+4+8+16) = 62, the sharpening blur 3 times
/// the radius (9) and its mask one more.
pub const APRON: u32 = 80;

/// The detail pass works on tiles of this many output texels (plus the apron), which is what
/// keeps its scratch memory small: it does not depend on the view's size.
const TILE: u32 = 768;
const WORK: u32 = TILE + 2 * APRON;

/// The most pixels the detail texture may have (72 MB); a view that would need more works one
/// pyramid level down.
const MAX_PIXELS: u64 = 9_000_000;
const FIT_PIXELS: u64 = 4_500_000;

/// The standard deviation of an à trous B3 detail scale's coefficients for white noise of
/// standard deviation 1 (Starck & Murtagh, 2D, `c_j - c_{j+1}`): finest first.
pub const SCALE_NOISE: [f32; SCALES] = [0.890, 0.201, 0.0856, 0.0413, 0.0203];

/// Camera noise isn't white (the demosaic spreads it), so coarser scales hold more of it than
/// `SCALE_NOISE` says. Measured against white noise on high-ISO photos of both cameras
/// (`bench noisescale`: the quietest tenth of blocks): luminance holds 1.7–2.2× at scale 1 and
/// 2–2.7× beyond; the colour axes hold 3.7–4.7× at scale 1 and 5–8× beyond (colour noise is
/// mostly low-frequency blotches).
const LUMA_CORRELATION: [f32; SCALES] = [1.0, 1.8, 2.3, 2.5, 2.5];
const CHROMA_CORRELATION: [f32; SCALES] = [1.0, 4.0, 5.5, 5.5, 5.0];

/// The noise in the detail at scale `j` of pyramid `level` is that of scale `j + level` of
/// level 0 (the same band of frequencies), which for white noise halves per scale beyond the table.
fn scale_noise(index: usize) -> f32 {
    if index < SCALES { SCALE_NOISE[index] } else { SCALE_NOISE[SCALES - 1] * 0.5f32.powi((index - SCALES + 1) as i32) }
}

/// Thresholds at slider 100 as multiples of the coefficients' noise: luminance linear in the
/// slider; colour grows as its square root, so the default 25 is already strong (colour noise
/// is far uglier than luminance noise and costs little detail).
const LUMINANCE_REACH: f32 = 2.6;
const COLOR_REACH: f32 = 4.5;
/// Luminance Detail shrinks the thresholds of the finest scales, where texture lives, to this
/// fraction at Detail 100 (and half way at the default 50).
const LUMINANCE_PROTECT: [f32; SCALES] = [0.5, 0.7, 0.9, 1.0, 1.0];
/// Colour noise lives at coarse scales, so chroma is shrunk harder there (at Detail 0; Detail
/// 100 shrinks all scales alike).
const COLOR_COARSE: [f32; SCALES] = [1.0, 1.3, 1.9, 2.6, 3.5];

/// Sharpening: the gain on the difference of log2 luminance per 100 of Amount; the soft limit on
/// that difference in stops at Detail 0 and 100.
const SHARPEN_GAIN: f32 = 1.5;
const LIMIT_LOW: f32 = 0.25;
const LIMIT_HIGH: f32 = 4.0;
/// Below this sigma (in texels of the level) the blur is the pixel itself: nothing to sharpen.
const MIN_SIGMA: f32 = 0.3;

// ---- Where ----

/// The part of one pyramid level the detail texture covers, in that level's texels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub level: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The size of a pyramid level.
pub fn level_size(base: (u32, u32), level: u32) -> (u32, u32) {
    ((base.0 >> level).max(1), (base.1 >> level).max(1))
}

/// The box the view covers, in texels of the full-size texture (left, top, right, bottom, floats),
/// clipped to the texture; None if it covers none of it. `view` maps output pixels (the picture's
/// part of the `out`-sized target) to texture coordinates; `lens` moves them to the sensor image.
pub fn view_bounds(view: &ViewMap, out: (u32, u32), texture: (u32, u32), lens: Option<&LensCorrection>) -> Option<[f64; 4]> {
    let (w, h) = (texture.0 as f64, texture.1 as f64);
    // The picture's part of the output.
    let (x0, y0) = ((view.clip[0] as f64).max(0.0), (view.clip[1] as f64).max(0.0));
    let (x1, y1) = ((view.clip[2] as f64).min(out.0 as f64), (view.clip[3] as f64).min(out.1 as f64));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let at = |x: f64, y: f64| -> [f64; 2] {
        let u = [0, 1].map(|i| (view.origin[i] as f64 + x * view.step_x[i] as f64 + y * view.step_y[i] as f64).clamp(0.0, 1.0));
        match lens {
            Some(l) => {
                let s = l.source(Channel::Green, [((u[0] - 0.5) * w) as f32, ((u[1] - 0.5) * h) as f32], w as f32, h as f32);
                [w / 2.0 + s[0] as f64, h / 2.0 + s[1] as f64]
            }
            None => [u[0] * w, u[1] * h],
        }
    };
    // The border, finely enough that the lens warp's bulge between points is a few texels at most.
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    const N: usize = 16;
    for i in 0..=N {
        let t = i as f64 / N as f64;
        let (x, y) = (x0 + t * (x1 - x0), y0 + t * (y1 - y0));
        for p in [at(x, y0), at(x, y1), at(x0, y), at(x1, y)] {
            b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
        }
    }
    // The colour channels sit slightly apart (chromatic aberration), and the warp can bulge.
    let margin = 4.0 + 0.006 * 0.5 * w.hypot(h) * lens.is_some() as u32 as f64;
    let b = [(b[0] - margin).max(0.0), (b[1] - margin).max(0.0), (b[2] + margin).min(w), (b[3] + margin).min(h)];
    (b[2] > b[0] && b[3] > b[1]).then_some(b)
}

/// The region of `level` that covers `bounds` (full-size texels) and an apron.
/// The part of one pyramid level the detail texture covers: what the view needs, in texels of
/// that level (it is worked on with an apron round it, which is not kept).
pub fn region_at(bounds: &[f64; 4], texture: (u32, u32), level: u32) -> Region {
    let (lw, lh) = level_size(texture, level);
    let s = (1u64 << level) as f64;
    let edge = |lo: f64, hi: f64, size: u32| {
        let a0 = (lo / s).floor().max(0.0);
        let a1 = (hi / s).ceil().min(size as f64);
        (a0 as u32, (a1 - a0).max(1.0) as u32)
    };
    let ((x, w), (y, h)) = (edge(bounds[0], bounds[2], lw), edge(bounds[1], bounds[3], lh));
    Region { level, x, y, w, h }
}

// ---- Noise ----

/// The stabilised opponent space the noise reduction works in (the CPU twin of `stabilise` in
/// detail.hlsli).
pub fn stabilise(rgb: [f32; 3]) -> [f32; 3] {
    let root = 0.002f32.sqrt();
    let y = rgb.map(|c| if c >= 0.0 { (c + 0.002).sqrt() } else { root + c / (2.0 * root) });
    [(y[0] + y[1] + y[2]) / 3.0, 0.5 * (y[0] - y[2]), 0.25 * (2.0 * y[1] - y[0] - y[2])]
}

/// The photo's noise: the standard deviation of the finest wavelet scale's noise in each of the
/// stabilised channels, at level 0.
pub type Noise = [f32; 3];

/// Estimates the noise of an RGB image (camera RGB, linear) of `w` × `h`: the median absolute
/// value of the finest B3 detail coefficients of each stabilised channel, over the median-to-
/// deviation factor 0.6745 and the scale's own factor. Texture and edges only push it up a little;
/// the median ignores them unless they cover half the crop.
pub fn estimate_noise(rgb: &[[f32; 3]], w: usize, h: usize) -> Noise {
    assert_eq!(rgb.len(), w * h);
    if w < 8 || h < 8 {
        return [0.0; 3];
    }
    const K: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
    let opp: Vec<[f32; 3]> = rgb.iter().map(|&c| stabilise(c)).collect();
    let mut rows = vec![[0.0f32; 3]; w * h];
    for y in 0..h {
        for x in 2..w - 2 {
            let mut s = [0.0; 3];
            for (k, wk) in K.iter().enumerate() {
                let p = opp[y * w + x + k - 2];
                for c in 0..3 {
                    s[c] += wk * p[c];
                }
            }
            rows[y * w + x] = s;
        }
    }
    let mut detail: [Vec<f32>; 3] = [(); 3].map(|_| Vec::with_capacity(w * h));
    for y in 2..h - 2 {
        for x in 2..w - 2 {
            let mut s = [0.0; 3];
            for (k, wk) in K.iter().enumerate() {
                let p = rows[(y + k - 2) * w + x];
                for c in 0..3 {
                    s[c] += wk * p[c];
                }
            }
            let o = opp[y * w + x];
            for c in 0..3 {
                detail[c].push((o[c] - s[c]).abs());
            }
        }
    }
    detail.map(|mut d| {
        let mid = d.len() / 2;
        let (_, median, _) = d.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        *median / 0.6745 / SCALE_NOISE[0]
    })
}

// ---- What to run ----

// ---- What to run ----

/// The shrink applied to one wavelet scale's detail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaleShrink {
    /// Thresholds for luminance and the two colour axes, and the luminance noise at this scale
    /// (which tells the colour shrink what counts as a luminance edge).
    pub threshold: [f32; 4],
    /// Exponents of the shrink function for luminance and colour (1 soft; higher keeps more of
    /// the large coefficients).
    pub power: [f32; 2],
}

/// The noise reduction's shrink per scale at pyramid `level`; None if both sliders are 0.
pub fn shrink_plan(d: &Detail, noise: Noise, level: u32) -> Option<[ScaleShrink; SCALES]> {
    if d.luminance <= 0.0 && d.color <= 0.0 {
        return None;
    }
    let luminance = d.luminance / 100.0 * LUMINANCE_REACH;
    let color = (d.color / 100.0).sqrt() * COLOR_REACH;
    let (ld, cd) = (d.luminance_detail / 100.0, d.color_detail / 100.0);
    let power = [1.0 + 3.0 * ld, 1.0 + 3.0 * cd];
    Some(std::array::from_fn(|j| {
        let k = j + level as usize;
        let c = k.min(SCALES - 1);
        let (luma_unit, chroma_unit) = (scale_noise(k) * LUMA_CORRELATION[c], scale_noise(k) * CHROMA_CORRELATION[c]);
        let protect = 1.0 + (LUMINANCE_PROTECT[j] - 1.0) * ld;
        // Detail 100 spreads colour smoothing evenly over the scales.
        let boost = COLOR_COARSE[j].powf(1.0 - cd);
        ScaleShrink {
            threshold: [
                luminance * noise[0] * luma_unit * protect,
                color * noise[1] * chroma_unit * boost,
                color * noise[2] * chroma_unit * boost,
                noise[0] * luma_unit,
            ],
            power,
        }
    }))
}

/// The sharpening's numbers at pyramid `level`; None if it does nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sharpen {
    /// Gaussian sigma in the level's texels, and how many taps each side reaches.
    pub sigma: f32,
    pub taps: u32,
    pub gain: f32,
    /// The soft limit on the difference from the blur, in stops.
    pub limit: f32,
    /// The edge mask: threshold in stops per texel and how much it applies (0..1).
    pub mask_threshold: f32,
    pub mask_amount: f32,
    /// The coring: the noise of a pixel's difference from its blur in the stabilised luminance
    /// space, times k (the shader turns it into stops at each brightness). 0 without a noise estimate.
    pub coring: f32,
}

pub fn sharpen_plan(d: &Detail, level: u32, noise: Noise) -> Option<Sharpen> {
    let sigma = d.radius / (1u32 << level) as f32;
    if d.amount <= 0.0 || sigma < MIN_SIGMA {
        return None;
    }
    let m = d.masking / 100.0;
    let t = d.detail / 100.0;
    // A pixel minus a Gaussian blur of it: white noise of 1 leaves sqrt(1 - 3/(8π s²)).
    let difference = (1.0 - 3.0 / (8.0 * std::f32::consts::PI * sigma * sigma)).max(0.1).sqrt();
    // Detail 25 cores at 1.5 noise sigmas, Detail 100 not at all; noise reduction that has already
    // run leaves less to core.
    let k = 2.0 * (1.0 - t);
    let remaining = 1.0 - 0.6 * (d.luminance / 100.0).min(1.0);
    Some(Sharpen {
        sigma,
        taps: (3.0 * sigma).ceil().clamp(1.0, 9.0) as u32,
        gain: d.amount / 100.0 * SHARPEN_GAIN,
        limit: LIMIT_LOW + (LIMIT_HIGH - LIMIT_LOW) * t * t,
        mask_threshold: 0.02 + 0.25 * m,
        mask_amount: (2.0 * m).min(1.0),
        coring: k * noise[0] * 0.5f32.powi(level as i32) * difference * remaining,
    })
}

/// Texture's numbers at pyramid `level`; None if it does nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Texture {
    /// The band is the difference of two Gaussians, in the level's texels.
    pub sigma_a: f32,
    pub sigma_b: f32,
    pub taps_a: u32,
    pub taps_b: u32,
    /// Signed: the fraction of the band added (positive) or removed (negative).
    pub gain: f32,
    /// The band amplitude (stops) at which the edge weight halves.
    pub edge: f32,
    /// The coring of a positive Texture: the band's noise in the stabilised luminance space.
    pub coring: f32,
}

/// The band's two blurs, in full-resolution pixels: it holds detail of about 2 to 8 pixels, and
/// leaves the finest scale (noise) and large shapes alone.
const TEXTURE_SIGMA: (f32, f32) = (1.4, 4.0);
const TEXTURE_EDGE: f32 = 0.4;

pub fn texture_plan(d: &Detail, level: u32, noise: Noise) -> Option<Texture> {
    let scale = (1u32 << level) as f32;
    let (a, b) = (TEXTURE_SIGMA.0 / scale, TEXTURE_SIGMA.1 / scale);
    if d.texture == 0.0 || a < MIN_SIGMA {
        return None;
    }
    let t = d.texture / 100.0;
    // 2.5 sigmas: the Gaussian's tail past that is under 2%, which a band signal doesn't miss.
    let taps = |s: f32| (2.5 * s).ceil().clamp(1.0, 10.0) as u32;
    Some(Texture {
        sigma_a: a,
        sigma_b: b,
        taps_a: taps(a),
        taps_b: taps(b),
        gain: if t > 0.0 { 1.8 * t } else { t },
        edge: TEXTURE_EDGE,
        // The band of white noise is a fifth of its sigma; demosaicing correlates it, so half; cored at 1.5.
        coring: 1.5 * 0.5 * noise[0] * 0.5f32.powi(level as i32),
    })
}


// ---- The GPU side ----

/// The kernels' constants (30 words), field for field as in detail.hlsli.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DetailConstants {
    in_origin: [i32; 2],
    in_size: [i32; 2],
    region_size: [i32; 2],
    step: u32,
    flags: u32,
    thr: [f32; 4],
    pw: [f32; 4],
    sharp: [f32; 4],
    mask: [f32; 4],
    out_rect: [i32; 4],
    out_offset: [i32; 2],
}

const _: () = assert!(size_of::<DetailConstants>() == 30 * 4);

const FLAG_STABILISE: u32 = 1;
const FLAG_LAST: u32 = 2;
const FLAG_ACC: u32 = 4;
const FLAG_VERTICAL: u32 = 8;
const FLAG_MASK: u32 = 16;
const FLAG_OUT: u32 = 32;

pub const SLOT_DETAIL: u32 = 300;
const SLOTS_PER_PASS: u32 = 5;
const SLOTS: u32 = 4096;

const FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

// The scratch textures, each WORK × WORK.
const C0: usize = 0; // wavelet scales alternate between C0 and C1; later C0 also holds the blurred log luminance
const A0: usize = 2; // the running sum of shrunk detail, alternating with A1; A1 holds the denoised image
const A1: usize = 3;
const T: usize = 4; // the horizontally blurred scale; later the horizontally blurred log luminance
const SCRATCH: usize = 5;
// The output (the detail texture) is the sixth, tracked with the same state flags.
const D: usize = 5;

/// What the detail texture holds, so a frame with the same inputs can reuse it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Key {
    image: u64,
    region: Region,
    detail: Detail,
    noise: Noise,
    show_mask: bool,
}

/// The detail pass's state, kept in the `Gpu`.
pub struct DetailState {
    /// Five scratch textures of WORK × WORK, made on first use, and the output texture, made as
    /// big as the biggest view needed (rounded up), 8 bytes a texel.
    scratch: Option<[ID3D12Resource; SCRATCH]>,
    out: Option<((u32, u32), ID3D12Resource)>,
    key: Option<Key>,
    /// Whether each texture is in the shader-resource state (else unordered access).
    srv: [bool; SCRATCH + 1],
    /// How many times the pass has run (for tests and the benchmark).
    pub runs: u64,
    /// Show the sharpening mask in place of the picture (alt-drag on Masking, and the benchmark).
    pub show_mask: bool,
}

impl Default for DetailState {
    fn default() -> Self {
        DetailState { scratch: None, out: None, key: None, srv: [false; SCRATCH + 1], runs: 0, show_mask: false }
    }
}

impl DetailState {
    /// Forget the texture's contents, so the next frame runs the pass again.
    pub fn invalidate(&mut self) {
        self.key = None;
    }

    /// Frees the memory (the warm-up's small allocation, say).
    pub fn release(&mut self) {
        *self = DetailState { show_mask: self.show_mask, runs: self.runs, ..Default::default() };
    }

    /// Bytes of GPU memory held now.
    pub fn bytes(&self) -> u64 {
        let scratch = if self.scratch.is_some() { SCRATCH as u64 * (WORK as u64).pow(2) * 8 } else { 0 };
        scratch + self.out.as_ref().map_or(0, |((w, h), _)| *w as u64 * *h as u64 * 8)
    }
}

/// What the develop pass needs to sample the detail texture.
pub struct Active {
    pub region: Region,
    pub texture: ID3D12Resource,
}

impl Active {
    /// The develop pass's constants: the region's left/top and width/height (16 bits each), and the
    /// level plus one.
    pub fn constants(&self) -> (u32, u32, u32) {
        let r = &self.region;
        (r.x | r.y << 16, r.w | r.h << 16, r.level + 1)
    }
}

impl Image {
    /// Measures the photo's noise, once (reads a crop of level 0 back, about 3 ms). Needed only
    /// when noise reduction is on.
    pub fn ensure_noise(&self, gpu: &mut Gpu) -> Result<Noise> {
        if let Some(n) = self.noise.get() {
            return Ok(n);
        }
        const CROP: u32 = 512;
        let (w, h) = (CROP.min(self.width), CROP.min(self.height));
        let (x, y) = ((self.width - w) / 2, (self.height - h) / 2);
        let px = crate::pipeline::read_rgba16f(gpu, &self.linear, 0, (x, y, w, h))?;
        let rgb: Vec<[f32; 3]> = px.iter().map(|p| [p[0], p[1], p[2]]).collect();
        let noise = estimate_noise(&rgb, w as usize, h as usize).map(|s| s.clamp(1e-5, 0.5));
        self.noise.set(Some(noise));
        Ok(noise)
    }
}

/// Makes sure the detail texture holds what the develop pass should sample for this view,
/// recording the passes if it doesn't. None when the detail controls are neutral (the develop pass
/// then samples the pyramid), the picture is empty, or this is not a raw.
pub fn prepare(gpu: &Gpu, image: &Image, edit: &Edit, view: &ViewMap, out: (u32, u32), zoom: Zoom) -> Option<Active> {
    let detail = edit.detail();
    if detail.is_neutral() {
        return None;
    }
    let texture = (image.width, image.height);
    let lens = image.lens.and_then(|l| l.with_switches(edit.no_distortion == 0, edit.no_chromatic_aberration == 0));
    let bounds = view_bounds(view, out, texture, lens.as_ref())?;

    // The level the view samples; an export tile always works at level 0.
    // A developed preview is a Region too, but a shrunk one, and is made like the fitted view.
    let tile = matches!(zoom, Zoom::Region { scale, .. } if scale >= 1.0);
    let mut level = if tile { 0 } else { (view.lod.floor() as u32).min(image.levels - 1) };
    // A view near 100% needs its level exactly; a zoomed-out one is fine a level coarser if the
    // region would be much bigger than the screen (a fitted 24-megapixel photo on a 1440p view).
    let budget = if view.lod < 0.5 { MAX_PIXELS } else { FIT_PIXELS };
    let region = loop {
        let r = region_at(&bounds, texture, level);
        if tile || r.w as u64 * r.h as u64 <= budget || level + 1 >= image.levels {
            break r;
        }
        level += 1;
    };

    // Noise reduction needs the photo's noise; until it is measured, only the sharpening runs.
    let noise = image.noise.get();
    let detail_eff = if noise.is_none() { Detail { luminance: 0.0, color: 0.0, ..detail } } else { detail };
    let noise = noise.unwrap_or([0.0; 3]);
    let sharp = sharpen_plan(&detail_eff, level, noise);
    let shrink = shrink_plan(&detail_eff, noise, level);
    let texture = texture_plan(&detail_eff, level, noise);
    if sharp.is_none() && shrink.is_none() && texture.is_none() {
        return None;
    }

    let mut st = gpu.detail.borrow_mut();
    let key = Key { image: image.id, region, detail: detail_eff, noise, show_mask: st.show_mask };
    // Memory: the scratch on first use, the output as big as it has needed to be.
    if st.scratch.is_none() {
        let make = || new_texture(&gpu.device, WORK, WORK, 1, FORMAT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        let tex = [make().ok()?, make().ok()?, make().ok()?, make().ok()?, make().ok()?];
        st.scratch = Some(tex);
        st.srv = [false; SCRATCH + 1];
        st.key = None;
    }
    let need = (region.w.div_ceil(64) * 64, region.h.div_ceil(64) * 64);
    let have = st.out.as_ref().map(|o| o.0);
    if have.is_none_or(|c| c.0 < need.0 || c.1 < need.1) {
        let cap = have.map_or(need, |c| (c.0.max(need.0), c.1.max(need.1)));
        // The old texture is idle (every submission waits), so it can go.
        st.out = None;
        let texture = new_texture(&gpu.device, cap.0, cap.1, 1, FORMAT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS).ok()?;
        st.out = Some((cap, texture));
        st.srv[D] = false;
        st.key = None;
    }
    if st.key != Some(key) {
        run(gpu, &mut st, image, &region, shrink.as_ref(), sharp.as_ref(), texture.as_ref());
        st.key = Some(key);
        st.runs += 1;
    }
    Some(Active { region, texture: st.out.as_ref().unwrap().1.clone() })
}

/// One input of a pass: the image's level, or one of the textures.
#[derive(Clone, Copy)]
enum In {
    Level,
    Tex(usize),
    Null,
}

#[derive(Clone, Copy)]
enum Out {
    Tex(usize),
    Null,
}

/// Records the passes, a tile of the output at a time. The texture states are tracked in `st.srv`.
fn run(gpu: &Gpu, st: &mut DetailState, image: &Image, r: &Region, shrink: Option<&[ScaleShrink; SCALES]>, sharp: Option<&Sharpen>, texture: Option<&Texture>) {
    // Sharpening and Texture share the blur passes and the final one.
    let finish = sharp.is_some() || texture.is_some();
    let (lw, lh) = level_size((image.width, image.height), r.level);
    let level_size = [lw as i32, lh as i32];
    let show_mask = st.show_mask;
    let out_tex = st.out.as_ref().unwrap().1.clone();
    let scratch = st.scratch.as_ref().unwrap();
    let tex = |i: usize| if i == D { &out_tex } else { &scratch[i] };
    let srv_state = &mut st.srv;
    let mut pass = 0u32;

    // Moves a texture to the shader-resource (true) or unordered-access state, if it isn't.
    let mut want = |gpu: &Gpu, i: usize, srv: bool| {
        if srv_state[i] != srv {
            let (uav, read) = (D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
            let (before, after) = if srv { (uav, read) } else { (read, uav) };
            gpu.transition(tex(i), 0, before, after);
            srv_state[i] = srv;
        }
    };

    let mut dispatch = |gpu: &Gpu, kernel: &crate::gpu::Kernel, constants: &DetailConstants, ins: [In; 3], outs: [Out; 2]| {
        let base = SLOT_DETAIL + pass * SLOTS_PER_PASS;
        assert!(base + SLOTS_PER_PASS <= SLOTS, "the detail pass needs more descriptors than the heap has");
        pass += 1;
        for (i, input) in ins.iter().enumerate() {
            match *input {
                In::Level => gpu.srv_texture(&image.linear, FORMAT, r.level, 1, base + i as u32),
                In::Tex(t) => {
                    want(gpu, t, true);
                    gpu.srv_texture(tex(t), FORMAT, 0, 1, base + i as u32);
                }
                In::Null => gpu.srv_null(base + i as u32),
            }
        }
        for (i, output) in outs.iter().enumerate() {
            match *output {
                Out::Tex(t) => {
                    want(gpu, t, false);
                    gpu.uav_texture(tex(t), FORMAT, 0, base + 3 + i as u32);
                }
                Out::Null => gpu.uav_null(base + 3 + i as u32),
            }
        }
        gpu.dispatch(kernel, constants, base, (constants.region_size[0] as u32, constants.region_size[1] as u32));
        gpu.uav_barrier();
    };

    let k = &gpu.kernels;
    let a = APRON as i32;
    for ty in (0..r.h).step_by(TILE as usize) {
        for tx in (0..r.w).step_by(TILE as usize) {
            let (iw, ih) = (TILE.min(r.w - tx), TILE.min(r.h - ty));
            // The tile's working rectangle, in the level's texels: the output plus the apron
            // (which may reach past the image; the kernels clamp).
            let origin = [(r.x + tx) as i32 - a, (r.y + ty) as i32 - a];
            let work = [(iw + 2 * APRON) as i32, (ih + 2 * APRON) as i32];
            let base = DetailConstants {
                in_origin: origin,
                in_size: level_size,
                region_size: work,
                out_rect: [a, a, a + iw as i32, a + ih as i32],
                out_offset: [tx as i32 - a, ty as i32 - a],
                ..Default::default()
            };
            // What sharpening reads: the denoised image (A1) or the image's level.
            let mut sharp_input = (In::Level, origin, level_size);

            if let Some(shrink) = shrink {
                for (j, s) in shrink.iter().enumerate() {
                    let step = 1u32 << j;
                    // c_j: the image for scale 0, else C[j % 2] (the next scale goes to the other).
                    let (current, current_origin, current_size) =
                        if j == 0 { (In::Level, origin, level_size) } else { (In::Tex(C0 + j % 2), [0, 0], work) };
                    let next = C0 + (j + 1) % 2;
                    let flags = if j == 0 { FLAG_STABILISE } else { 0 };
                    let at_scale = DetailConstants { in_origin: current_origin, in_size: current_size, step, flags, ..base };
                    dispatch(gpu, &k.atrous_blur, &at_scale, [current, In::Null, In::Null], [Out::Tex(T), Out::Null]);
                    dispatch(gpu, &k.atrous_v, &at_scale, [In::Tex(T), In::Null, In::Null], [Out::Tex(next), Out::Null]);
                    let last = j == SCALES - 1;
                    let acc_in = if j > 0 { In::Tex(A0 + j % 2) } else { In::Null };
                    let flags = flags | if last { FLAG_LAST } else { 0 } | if j > 0 { FLAG_ACC } else { 0 };
                    // The last scale's result is the denoised image (A1), or the output if there is no sharpening.
                    let (flags, outs) = match (last, finish) {
                        (false, _) => (flags, [Out::Tex(A0 + (j + 1) % 2), Out::Null]),
                        (true, true) => (flags, [Out::Tex(A1), Out::Null]),
                        (true, false) => (flags | FLAG_OUT, [Out::Null, Out::Tex(D)]),
                    };
                    dispatch(
                        gpu,
                        &k.atrous_shrink,
                        &DetailConstants { flags, thr: s.threshold, pw: [s.power[0], s.power[1], 0.0, 0.0], ..at_scale },
                        [current, In::Tex(next), acc_in],
                        outs,
                    );
                }
                sharp_input = (In::Tex(A1), [0, 0], work);
            }

            if finish {
                let (input, in_origin, in_size) = sharp_input;
                let (sigma, gain, limit, taps, mask_t, mask_a, coring) =
                    sharp.map_or((0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0), |s| (s.sigma, s.gain, s.limit, s.taps as f32, s.mask_threshold, s.mask_amount, s.coring));
                let t = texture.copied().unwrap_or(Texture { sigma_a: 0.0, sigma_b: 0.0, taps_a: 0, taps_b: 0, gain: 0.0, edge: 1.0, coring: 0.0 });
                let common = DetailConstants {
                    in_origin,
                    in_size,
                    thr: [t.sigma_a, t.sigma_b, t.taps_a as f32, t.taps_b as f32],
                    pw: [t.gain, t.edge, 0.0, 0.0],
                    sharp: [sigma, gain, limit, taps],
                    mask: [mask_t, mask_a, coring, t.coring],
                    ..base
                };
                // The blurred log luminance (sharpening's and the band's, in xyz) lives in T (rows) and C0
                // (the result), which the noise reduction is done with.
                dispatch(gpu, &k.sharp_blur, &common, [input, In::Null, In::Null], [Out::Tex(T), Out::Null]);
                dispatch(gpu, &k.sharp_blur, &DetailConstants { flags: FLAG_VERTICAL, ..common }, [In::Null, In::Tex(T), In::Null], [Out::Tex(C0), Out::Null]);
                let flags = if show_mask { FLAG_MASK } else { 0 };
                dispatch(gpu, &k.sharpen, &DetailConstants { flags, ..common }, [input, In::Tex(C0), In::Null], [Out::Tex(D), Out::Null]);
            }
        }
    }
    // The develop pass reads the result.
    want(gpu, D, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::DETAIL_DEFAULTS;
    use crate::geometry;

    /// A deterministic noise source (xorshift and a sum of uniforms, close to Gaussian).
    struct Noise32(u32);
    impl Noise32 {
        fn uniform(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32
        }
        fn gauss(&mut self) -> f32 {
            (0..12).map(|_| self.uniform()).sum::<f32>() - 6.0
        }
    }

    #[test]
    fn estimates_white_noise_in_the_stabilised_space() {
        // Grey with noise whose standard deviation in the square-root space is about 0.01.
        let (w, h) = (256, 256);
        let mut rng = Noise32(0x1234_5678);
        let sigma = 0.01f32;
        let rgb: Vec<[f32; 3]> = (0..w * h)
            .map(|_| {
                // y = sqrt(c + 0.002) has noise sigma, so c = (y0 + n)² - 0.002 with y0 = 0.5.
                [(); 3].map(|_| {
                    let y = 0.5 + sigma * rng.gauss();
                    y * y - 0.002
                })
            })
            .collect();
        let n = estimate_noise(&rgb, w, h);
        // Each stabilised channel is a mix of the three independent colours; its noise follows.
        let expect = [sigma / 3.0f32.sqrt(), sigma * 0.5 * 2.0f32.sqrt(), sigma * 0.25 * 6.0f32.sqrt()];
        for c in 0..3 {
            assert!((n[c] / expect[c] - 1.0).abs() < 0.08, "channel {c}: {} vs {}", n[c], expect[c]);
        }
    }

    #[test]
    fn scale_noise_table_matches_white_noise() {
        // The à trous B3 decomposition of white noise: the first scale's standard deviation,
        // and the later ones from the same filters with the holes.
        let (w, h) = (192usize, 192usize);
        let mut rng = Noise32(99);
        let mut c: Vec<f32> = (0..w * h).map(|_| rng.gauss()).collect();
        let k = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
        let at = |v: usize, i: usize, step: usize, n: usize| (v as isize + (i as isize - 2) * step as isize).clamp(0, n as isize - 1) as usize;
        let blur = |src: &[f32], step: usize| {
            let mut t = vec![0.0; w * h];
            for y in 0..h {
                for x in 0..w {
                    t[y * w + x] = (0..5).map(|i| k[i] * src[y * w + at(x, i, step, w)]).sum();
                }
            }
            let mut o = vec![0.0; w * h];
            for y in 0..h {
                for x in 0..w {
                    o[y * w + x] = (0..5).map(|i| k[i] * t[at(y, i, step, h) * w + x]).sum();
                }
            }
            o
        };
        for (j, expect) in SCALE_NOISE.iter().enumerate() {
            let next = blur(&c, 1 << j);
            let m = 40usize;
            let d: Vec<f32> = (m..h - m).flat_map(|y| (m..w - m).map(move |x| (x, y))).map(|(x, y)| c[y * w + x] - next[y * w + x]).collect();
            let sd = (d.iter().map(|v| v * v).sum::<f32>() / d.len() as f32).sqrt();
            assert!((sd / expect - 1.0).abs() < 0.12, "scale {j}: {sd} vs {expect}");
            c = next;
        }
    }

    #[test]
    fn plans_follow_the_sliders() {
        let noise = [0.01, 0.005, 0.005];
        // Defaults: colour noise reduction only; sharpening at its defaults.
        let p = shrink_plan(&DETAIL_DEFAULTS, noise, 0).unwrap();
        assert_eq!(p[0].threshold[0], 0.0);
        assert!(p[0].threshold[1] > 0.0 && p[4].threshold[1] > p[0].threshold[1] * 0.1);
        assert!(shrink_plan(&Detail { color: 0.0, ..DETAIL_DEFAULTS }, noise, 0).is_none());
        // Luminance thresholds grow with the slider and shrink with the scale and the level.
        let a = shrink_plan(&Detail { luminance: 30.0, ..DETAIL_DEFAULTS }, noise, 0).unwrap();
        let b = shrink_plan(&Detail { luminance: 60.0, ..DETAIL_DEFAULTS }, noise, 0).unwrap();
        assert!((b[0].threshold[0] / a[0].threshold[0] - 2.0).abs() < 1e-5);
        assert!(a[1].threshold[0] < a[0].threshold[0]);
        let at1 = shrink_plan(&Detail { luminance: 30.0, ..DETAIL_DEFAULTS }, noise, 1).unwrap();
        assert!(at1[0].threshold[0] < a[0].threshold[0]);
        // Detail raises the exponent (the shrink keeps more of the large coefficients).
        let d0 = shrink_plan(&Detail { luminance: 30.0, luminance_detail: 0.0, ..DETAIL_DEFAULTS }, noise, 0).unwrap();
        let d100 = shrink_plan(&Detail { luminance: 30.0, luminance_detail: 100.0, ..DETAIL_DEFAULTS }, noise, 0).unwrap();
        assert!(d0[0].power[0] < d100[0].power[0]);
        // Sharpening: off at Amount 0, and a radius under a third of a texel does nothing.
        assert!(sharpen_plan(&Detail { amount: 0.0, ..DETAIL_DEFAULTS }, 0, noise).is_none());
        let s = sharpen_plan(&DETAIL_DEFAULTS, 0, noise).unwrap();
        assert!((s.sigma - 1.0).abs() < 1e-6 && s.taps == 3);
        assert!(sharpen_plan(&DETAIL_DEFAULTS, 2, noise).is_none());
        let wide = sharpen_plan(&Detail { radius: 3.0, ..DETAIL_DEFAULTS }, 0, noise).unwrap();
        assert_eq!(wide.taps, 9);
        // Coring follows the noise and Detail: none at Detail 100 or without a noise estimate, and less once luminance noise reduction has run.
        assert!(s.coring > 0.0);
        assert_eq!(sharpen_plan(&Detail { detail: 100.0, ..DETAIL_DEFAULTS }, 0, noise).unwrap().coring, 0.0);
        assert_eq!(sharpen_plan(&DETAIL_DEFAULTS, 0, [0.0; 3]).unwrap().coring, 0.0);
        assert!(sharpen_plan(&Detail { luminance: 100.0, ..DETAIL_DEFAULTS }, 0, noise).unwrap().coring < s.coring);
        // Masking 0 leaves the mask off.
        assert_eq!(s.mask_amount, 0.0);
        assert!(sharpen_plan(&Detail { masking: 100.0, ..DETAIL_DEFAULTS }, 0, noise).unwrap().mask_amount == 1.0);
    }

    fn fit_map(texture: (u32, u32), out: (u32, u32), angle: f32) -> ViewMap {
        geometry::view_map(texture, 0, texture, angle, crate::develop::FULL_CROP, Zoom::Fit, out)
    }

    #[test]
    fn region_covers_the_view_with_an_apron() {
        // 100% of a 6000 × 4000 photo in a 1000 × 800 view, centred: the middle 1000 × 800.
        let m = geometry::view_map((6000, 4000), 0, (6000, 4000), 0.0, crate::develop::FULL_CROP, Zoom::Actual { x: 0.5, y: 0.5 }, (1000, 800));
        let b = view_bounds(&m, (1000, 800), (6000, 4000), None).unwrap();
        assert!((b[0] - (2500.0 - 4.0)).abs() < 1.0 && (b[2] - (3500.0 + 4.0)).abs() < 1.0, "{b:?}");
        let r = region_at(&b, (6000, 4000), 0);
        let near = |a: u32, b: u32| a.abs_diff(b) <= 1;
        assert!(near(r.x, 2496) && near(r.y, 1596), "{r:?}");
        assert!(near(r.w, 1008) && near(r.h, 808), "{r:?}");
        // Level 1 halves everything (and the apron stays 80 texels).
        let r1 = region_at(&b, (6000, 4000), 1);
        assert!(r1.w >= 504 && r1.w <= 506, "{r1:?}");
        // Fitted: the whole texture, clipped.
        let m = fit_map((6000, 4000), (1500, 1000), 0.0);
        let b = view_bounds(&m, (1500, 1000), (6000, 4000), None).unwrap();
        let r = region_at(&b, (6000, 4000), 2);
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 1500, 1000));
        // A view with no picture in it has no region.
        let mut m = fit_map((6000, 4000), (1500, 1000), 0.0);
        m.clip = [10.0, 10.0, 5.0, 5.0];
        assert!(view_bounds(&m, (1500, 1000), (6000, 4000), None).is_none());
    }

    #[test]
    fn a_turned_view_needs_a_bigger_region() {
        let m = geometry::view_map((6000, 4000), 0, (6000, 4000), 10.0, crate::develop::FULL_CROP, Zoom::Actual { x: 0.5, y: 0.5 }, (1000, 800));
        let straight = geometry::view_map((6000, 4000), 0, (6000, 4000), 0.0, crate::develop::FULL_CROP, Zoom::Actual { x: 0.5, y: 0.5 }, (1000, 800));
        let a = view_bounds(&m, (1000, 800), (6000, 4000), None).unwrap();
        let b = view_bounds(&straight, (1000, 800), (6000, 4000), None).unwrap();
        assert!(a[2] - a[0] > b[2] - b[0] + 100.0 && a[3] - a[1] > b[3] - b[1] + 100.0);
    }

    #[test]
    fn export_tiles_at_one_to_one_are_level_zero_windows() {
        let m = geometry::view_map((6000, 4000), 0, (6000, 4000), 0.0, crate::develop::FULL_CROP, Zoom::Region { x: 2048.0, y: 0.0, scale: 1.0 }, (2048, 2048));
        assert_eq!(m.lod, 0.0);
        let b = view_bounds(&m, (2048, 2048), (6000, 4000), None).unwrap();
        let r = region_at(&b, (6000, 4000), 0);
        // Reaches the apron beyond the tile on the left and the right, and the top edge is the photo's.
        assert!(r.x.abs_diff(2048 - 4) <= 1 && r.y == 0, "{r:?}");
        assert!(r.w.abs_diff(2048 + 8) <= 2, "{r:?}");
    }
}
