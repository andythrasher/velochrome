//! Local adjustments (docs/phase2d-masks.md): a photo has up to `MAX_MASKS` masks, each built from
//! up to `MAX_COMPONENTS` shapes (linear and radial gradients so far; the brush is part B) that
//! add to or subtract from the mask, and each with its own sliders. The develop pass weights every
//! mask's sliders by the mask's weight at the pixel.
//!
//! `Masks` is what the app edits and the sidecar stores: a flat `repr(C)` value, zeroed = no masks,
//! mirrored by a C# struct. `GpuMaskSet` is the same masks worked out for the shader (directions
//! and offsets instead of angles and radii, white balance as per-channel gains), and this file's
//! functions on it are the CPU reference for `evalMasks` in develop.hlsl.
//!
//! Shapes live in the lens-corrected frame in the raw's own orientation (`uv_in` in develop.hlsl),
//! so cropping, straightening and rotating never move a mask over the photo. To keep ellipses
//! round, positions are in units of the frame's long edge: `p = uv × dims / max(dims)`.

use crate::color::Mat3;
use crate::develop::{temperature_to_multipliers};

pub const MAX_MASKS: usize = 8;
pub const MAX_COMPONENTS: usize = 8;
/// Sliders per mask, in this order: Temperature, Tint, Exposure, Contrast, Highlights, Shadows,
/// Whites, Blacks, Clarity, Saturation.
pub const SLIDERS: usize = 10;

pub mod slider {
    pub const TEMPERATURE: usize = 0;
    pub const TINT: usize = 1;
    pub const EXPOSURE: usize = 2;
    pub const CONTRAST: usize = 3;
    pub const HIGHLIGHTS: usize = 4;
    pub const SHADOWS: usize = 5;
    pub const WHITES: usize = 6;
    pub const BLACKS: usize = 7;
    pub const CLARITY: usize = 8;
    pub const SATURATION: usize = 9;
    pub const NAMES: [&str; super::SLIDERS] =
        ["temperature", "tint", "exposure", "contrast", "highlights", "shadows", "whites", "blacks", "clarity", "saturation"];
}

/// `Component::kind`.
pub const KIND_NONE: u32 = 0;
pub const KIND_LINEAR: u32 = 1;
pub const KIND_RADIAL: u32 = 2;
/// Part B. Reserved so the layout and the shader's switch already have a place for it: a brush
/// component's `p[0]` will name its raster layer. Until then it has no weight.
pub const KIND_BRUSH: u32 = 3;

/// `Component::mode`.
pub const ADD: u32 = 0;
pub const SUBTRACT: u32 = 1;

/// One shape of a mask.
///
/// - Linear: `p` = x, y of where the effect is full, x, y of where it is gone.
/// - Radial: `p` = centre x, y; radii x, y (before rotation); rotation in radians; feather 0–100;
///   invert (0 or 1).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Component {
    pub kind: u32,
    pub mode: u32,
    pub p: [f32; 8],
}

/// A mask: its shapes, its Amount, and its sliders.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Mask {
    /// How many of `components` are in use.
    pub count: u32,
    /// 0–100 (the mask's opacity). A new mask is made with 100.
    pub amount: f32,
    /// Temperature, Tint, Contrast, … in `slider` order: −100 to 100, Exposure −4 to 4 stops.
    pub sliders: [f32; SLIDERS],
    pub components: [Component; MAX_COMPONENTS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Masks {
    pub count: u32,
    pub masks: [Mask; MAX_MASKS],
}

impl Component {
    pub const NONE: Component = Component { kind: KIND_NONE, mode: ADD, p: [0.0; 8] };
}

impl Mask {
    pub const NONE: Mask = Mask { count: 0, amount: 0.0, sliders: [0.0; SLIDERS], components: [Component::NONE; MAX_COMPONENTS] };

    pub fn new() -> Mask {
        Mask { amount: 100.0, ..Default::default() }
    }

    pub fn components(&self) -> &[Component] {
        &self.components[..(self.count as usize).min(MAX_COMPONENTS)]
    }

    /// Whether any slider is away from zero.
    pub fn has_sliders(&self) -> bool {
        self.sliders.iter().any(|&v| v != 0.0)
    }
}

impl Masks {
    /// No masks, as a constant.
    pub const NONE: Masks = Masks { count: 0, masks: [Mask::NONE; MAX_MASKS] };

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn masks(&self) -> &[Mask] {
        &self.masks[..(self.count as usize).min(MAX_MASKS)]
    }

    /// Whether any mask moves Clarity (the develop pass then needs Clarity's base layer even with
    /// the global slider at 0).
    pub fn any_clarity(&self) -> bool {
        self.masks().iter().any(|m| m.sliders[slider::CLARITY] != 0.0 && m.amount > 0.0)
    }

    /// Puts everything in range and drops what can't be drawn: a damaged sidecar, or a wrapper's
    /// wild value. Components of unknown kind go; masks keep their order.
    pub fn clean(&mut self) {
        let fix = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { 0.0 };
        self.count = self.count.min(MAX_MASKS as u32);
        for mask in &mut self.masks[..self.count as usize] {
            mask.count = mask.count.min(MAX_COMPONENTS as u32);
            mask.amount = fix(mask.amount, 0.0, 100.0);
            for (i, v) in mask.sliders.iter_mut().enumerate() {
                let limit = if i == slider::EXPOSURE { 4.0 } else { 100.0 };
                *v = fix(*v, -limit, limit);
            }
            let mut kept = 0;
            for i in 0..mask.count as usize {
                let mut c = mask.components[i];
                if c.kind != KIND_LINEAR && c.kind != KIND_RADIAL {
                    continue;
                }
                c.mode = (c.mode != 0) as u32;
                if c.kind == KIND_LINEAR {
                    for v in &mut c.p[..4] {
                        *v = fix(*v, -4.0, 4.0);
                    }
                    c.p[4..].fill(0.0);
                } else {
                    for v in &mut c.p[..2] {
                        *v = fix(*v, -4.0, 4.0);
                    }
                    c.p[2] = fix(c.p[2], 0.0, 4.0).max(1e-4);
                    c.p[3] = fix(c.p[3], 0.0, 4.0).max(1e-4);
                    c.p[4] = fix(c.p[4], -7.0, 7.0);
                    c.p[5] = fix(c.p[5], 0.0, 100.0);
                    c.p[6] = (c.p[6] != 0.0) as u32 as f32;
                    c.p[7] = 0.0;
                }
                mask.components[kept] = c;
                kept += 1;
            }
            for c in &mut mask.components[kept..] {
                *c = Component::default();
            }
            mask.count = kept as u32;
        }
        for mask in &mut self.masks[self.count as usize..] {
            *mask = Mask::default();
        }
    }
}

// ---- Coordinates ----

/// A frame position given as fractions (0–1) of the raw's width and height, in the units shapes
/// are stored in (the long edge's).
pub fn to_frame(uv: [f32; 2], dims: (u32, u32)) -> [f32; 2] {
    let edge = dims.0.max(dims.1).max(1) as f32;
    [uv[0] * dims.0 as f32 / edge, uv[1] * dims.1 as f32 / edge]
}

/// The inverse of [`to_frame`].
pub fn from_frame(p: [f32; 2], dims: (u32, u32)) -> [f32; 2] {
    let edge = dims.0.max(dims.1).max(1) as f32;
    [p[0] * edge / dims.0.max(1) as f32, p[1] * edge / dims.1.max(1) as f32]
}

// ---- What the shader reads ----

/// One shape, worked out for the shader (develop.hlsl `GpuComponent`, field for field).
/// Linear: `axis_x` is the gradient `g` and `offset` its constant, so the weight is
/// `1 − smoothstep(0, 1, p·g + offset)`. Radial: the ellipse's two axes scaled by the inverse
/// radii, so `|((p − centre)·axis_x, (p − centre)·axis_y)|` is 1 on the ellipse.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GpuComponent {
    pub kind: u32,
    /// Bit 0: subtract. Bit 1: invert (radial).
    pub flags: u32,
    /// Radial: where the falloff starts (as a fraction of the ellipse), 1 − feather.
    pub inner: f32,
    pub offset: f32,
    pub centre: [f32; 2],
    pub axis_x: [f32; 2],
    pub axis_y: [f32; 2],
    pub pad: [f32; 2],
}

/// One mask, worked out for the shader (`GpuMask`): its opacity, exposure in stops, the white
/// balance as log2 gains of the camera's channels, the tone sliders as −1..1, then the shapes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GpuMask {
    pub count: u32,
    pub opacity: f32,
    pub exposure: f32,
    pub wb_log2: [f32; 3],
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub clarity: f32,
    pub saturation: f32,
    pub pad: [f32; 3],
    pub components: [GpuComponent; MAX_COMPONENTS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GpuMaskSet {
    pub count: u32,
    pub pad: [u32; 3],
    pub masks: [GpuMask; MAX_MASKS],
}

// develop.hlsl's MaskSet: 4 words, then 8 masks of 16 + 8 × 12 words.
const _: () = assert!(size_of::<GpuMaskSet>() == (4 + MAX_MASKS * (16 + MAX_COMPONENTS * 12)) * 4);

/// The Temperature slider at ±100 moves the white balance by this many kelvin from 5500 K (as
/// Lightroom's local slider, roughly), and Tint at ±100 by this many global tint units.
const KELVIN_AT_100: f32 = 2000.0;
const TINT_AT_100: f32 = 50.0;
const WB_BASE_KELVIN: f32 = 5500.0;

/// log2 of the camera-channel gains for local Temperature and Tint sliders (−100..100): the ratio
/// of the white balance multipliers of the moved white point to those of 5500 K, tint 0, so 0 and
/// 0 give no change, and +Temperature warms (more red, less blue), as the global slider does.
pub fn white_balance_log2(xyz_to_cam: &Mat3, temperature: f32, tint: f32) -> [f32; 3] {
    if temperature == 0.0 && tint == 0.0 {
        return [0.0; 3];
    }
    let base = temperature_to_multipliers(xyz_to_cam, WB_BASE_KELVIN, 0.0);
    let kelvin = (WB_BASE_KELVIN + KELVIN_AT_100 * temperature / 100.0).clamp(2000.0, 50000.0);
    let moved = temperature_to_multipliers(xyz_to_cam, kelvin, TINT_AT_100 * tint / 100.0);
    [0, 1, 2].map(|c| (moved[c] / base[c]).log2())
}

impl GpuComponent {
    fn new(c: &Component) -> GpuComponent {
        let mut g = GpuComponent { kind: c.kind, flags: (c.mode != 0) as u32, ..Default::default() };
        match c.kind {
            KIND_LINEAR => {
                let (a, b) = ([c.p[0], c.p[1]], [c.p[2], c.p[3]]);
                let d = [b[0] - a[0], b[1] - a[1]];
                let len2 = (d[0] * d[0] + d[1] * d[1]).max(1e-8);
                let grad = [d[0] / len2, d[1] / len2];
                g.axis_x = grad;
                g.offset = -(a[0] * grad[0] + a[1] * grad[1]);
            }
            KIND_RADIAL => {
                let (sin, cos) = c.p[4].sin_cos();
                let (rx, ry) = (c.p[2].max(1e-4), c.p[3].max(1e-4));
                g.centre = [c.p[0], c.p[1]];
                g.axis_x = [cos / rx, sin / rx];
                g.axis_y = [-sin / ry, cos / ry];
                g.inner = 1.0 - (c.p[5] / 100.0).clamp(0.005, 1.0);
                g.flags |= ((c.p[6] != 0.0) as u32) << 1;
            }
            _ => g.kind = KIND_NONE,
        }
        g
    }
}

impl GpuMaskSet {
    /// The masks worked out for the shader. `xyz_to_cam` is the photo's colour matrix, which the
    /// local white balance needs.
    pub fn new(masks: &Masks, xyz_to_cam: &Mat3) -> GpuMaskSet {
        let mut set = GpuMaskSet { count: masks.count.min(MAX_MASKS as u32), ..Default::default() };
        for (g, m) in set.masks.iter_mut().zip(masks.masks()) {
            let s = &m.sliders;
            *g = GpuMask {
                count: m.count.min(MAX_COMPONENTS as u32),
                opacity: m.amount / 100.0,
                exposure: s[slider::EXPOSURE],
                wb_log2: white_balance_log2(xyz_to_cam, s[slider::TEMPERATURE], s[slider::TINT]),
                contrast: s[slider::CONTRAST] / 100.0,
                highlights: s[slider::HIGHLIGHTS] / 100.0,
                shadows: s[slider::SHADOWS] / 100.0,
                whites: s[slider::WHITES] / 100.0,
                blacks: s[slider::BLACKS] / 100.0,
                clarity: s[slider::CLARITY] / 100.0,
                saturation: s[slider::SATURATION] / 100.0,
                ..Default::default()
            };
            for (gc, c) in g.components.iter_mut().zip(m.components()) {
                *gc = GpuComponent::new(c);
            }
        }
        set
    }
}

fn smoothstep01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A shape's weight at frame position `p`, 0–1 (develop.hlsl `componentWeight`).
pub fn component_weight(c: &GpuComponent, p: [f32; 2]) -> f32 {
    match c.kind {
        KIND_LINEAR => 1.0 - smoothstep01(p[0] * c.axis_x[0] + p[1] * c.axis_x[1] + c.offset),
        KIND_RADIAL => {
            let d = [p[0] - c.centre[0], p[1] - c.centre[1]];
            let q = [d[0] * c.axis_x[0] + d[1] * c.axis_x[1], d[0] * c.axis_y[0] + d[1] * c.axis_y[1]];
            let r = (q[0] * q[0] + q[1] * q[1]).sqrt();
            let w = 1.0 - smoothstep01((r - c.inner) / (1.0 - c.inner));
            if c.flags & 2 != 0 { 1.0 - w } else { w }
        }
        // Part B: the brush layer's value.
        _ => 0.0,
    }
}

/// A mask's weight at `p`: the soft union of its added shapes times what its subtracted shapes
/// leave, times its Amount (develop.hlsl `maskWeight`).
pub fn mask_weight(m: &GpuMask, p: [f32; 2]) -> f32 {
    let (mut add, mut sub) = (1.0f32, 1.0f32);
    for c in &m.components[..m.count as usize] {
        let w = component_weight(c, p);
        if c.flags & 1 != 0 {
            sub *= 1.0 - w;
        } else {
            add *= 1.0 - w;
        }
    }
    (1.0 - add) * sub * m.opacity
}

/// The masks' sliders weighted at one pixel and summed (develop.hlsl `MaskLocal`): stops of
/// exposure, log2 gains of the camera channels, and the −1..1 sliders to add to the global ones.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Local {
    pub exposure: f32,
    pub wb_log2: [f32; 3],
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub clarity: f32,
    pub saturation: f32,
}

impl GpuMaskSet {
    pub fn local(&self, p: [f32; 2]) -> Local {
        let mut l = Local::default();
        for m in &self.masks[..self.count as usize] {
            let w = mask_weight(m, p);
            if w <= 0.0 {
                continue;
            }
            l.exposure += w * m.exposure;
            for c in 0..3 {
                l.wb_log2[c] += w * m.wb_log2[c];
            }
            l.contrast += w * m.contrast;
            l.highlights += w * m.highlights;
            l.shadows += w * m.shadows;
            l.whites += w * m.whites;
            l.blacks += w * m.blacks;
            l.clarity += w * m.clarity;
            l.saturation += w * m.saturation;
        }
        l
    }

    /// Mask `index`'s weight at `p` (the overlay), 0 for a mask that isn't there.
    pub fn weight(&self, index: usize, p: [f32; 2]) -> f32 {
        if index < self.count as usize { mask_weight(&self.masks[index], p) } else { 0.0 }
    }
}

// ---- Text (for the sidecar) ----

fn number(v: f32) -> String {
    let rounded = (v * 1e5).round() / 1e5;
    if rounded == 0.0 { "0".into() } else { rounded.to_string() }
}

/// The masks as one line of text: masks joined by `;`, each a list of parts joined by `|`: first
/// the Amount and the sliders that are not zero (`amount=80 exposure=0.5`), then one part per
/// shape, `L+`/`L-` (linear) or `R+`/`R-` (radial) and its numbers. Empty for no masks.
pub fn to_text(masks: &Masks) -> String {
    let mut out = Vec::new();
    for m in masks.masks() {
        let mut parts = Vec::new();
        let mut head = vec![format!("amount={}", number(m.amount))];
        for (name, &v) in slider::NAMES.iter().zip(&m.sliders) {
            if v != 0.0 {
                head.push(format!("{name}={}", number(v)));
            }
        }
        parts.push(head.join(" "));
        for c in m.components() {
            let (letter, n) = match c.kind {
                KIND_LINEAR => ('L', 4),
                KIND_RADIAL => ('R', 7),
                _ => continue,
            };
            let sign = if c.mode != 0 { '-' } else { '+' };
            let nums: Vec<String> = c.p[..n].iter().map(|&v| number(v)).collect();
            parts.push(format!("{letter}{sign} {}", nums.join(" ")));
        }
        out.push(parts.join(" | "));
    }
    out.join(" ; ")
}

/// Reads [`to_text`]'s output. Anything it can't make sense of is left out (a mask or shape at a
/// time); the result is cleaned.
pub fn from_text(text: &str) -> Masks {
    let mut masks = Masks::default();
    for part in text.split(';') {
        if masks.count as usize >= MAX_MASKS || part.trim().is_empty() {
            continue;
        }
        let mut m = Mask::new();
        let mut pieces = part.split('|');
        for kv in pieces.next().unwrap_or("").split_whitespace() {
            let Some((name, value)) = kv.split_once('=') else { continue };
            let Ok(v) = value.parse::<f32>() else { continue };
            if name == "amount" {
                m.amount = v;
            } else if let Some(i) = slider::NAMES.iter().position(|n| *n == name) {
                m.sliders[i] = v;
            }
        }
        for shape in pieces {
            let mut words = shape.split_whitespace();
            let Some(tag) = words.next() else { continue };
            let kind = match tag.get(..1) {
                Some("L") => KIND_LINEAR,
                Some("R") => KIND_RADIAL,
                _ => continue,
            };
            let mode = if tag.ends_with('-') { SUBTRACT } else { ADD };
            let nums: Vec<f32> = words.filter_map(|w| w.parse().ok()).collect();
            let want = if kind == KIND_LINEAR { 4 } else { 7 };
            if nums.len() != want || m.count as usize >= MAX_COMPONENTS {
                continue;
            }
            let mut c = Component { kind, mode, p: [0.0; 8] };
            c.p[..want].copy_from_slice(&nums);
            m.components[m.count as usize] = c;
            m.count += 1;
        }
        masks.masks[masks.count as usize] = m;
        masks.count += 1;
    }
    masks.clean();
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    const OM5_XYZ_TO_CAM: Mat3 = [[1.2008, -0.523, -0.1123], [-0.2265, 1.0496, 0.2046], [0.0069, 0.0734, 0.4938]];

    fn linear(a: [f32; 2], b: [f32; 2], mode: u32) -> Component {
        Component { kind: KIND_LINEAR, mode, p: [a[0], a[1], b[0], b[1], 0.0, 0.0, 0.0, 0.0] }
    }

    fn radial(c: [f32; 2], r: [f32; 2], angle: f32, feather: f32, invert: bool, mode: u32) -> Component {
        Component { kind: KIND_RADIAL, mode, p: [c[0], c[1], r[0], r[1], angle, feather, invert as u32 as f32, 0.0] }
    }

    fn one(components: &[Component], amount: f32) -> Masks {
        let mut m = Mask::new();
        m.amount = amount;
        for (i, c) in components.iter().enumerate() {
            m.components[i] = *c;
        }
        m.count = components.len() as u32;
        let mut masks = Masks::default();
        masks.masks[0] = m;
        masks.count = 1;
        masks
    }

    fn weight(masks: &Masks, p: [f32; 2]) -> f32 {
        GpuMaskSet::new(masks, &IDENTITY).weight(0, p)
    }

    #[test]
    fn zeroed_is_no_masks() {
        let m = Masks::default();
        assert!(m.is_empty() && m.masks().is_empty() && !m.any_clarity());
        let set = GpuMaskSet::new(&m, &IDENTITY);
        assert_eq!(set.local([0.3, 0.3]), Local::default());
    }

    #[test]
    fn coordinates_round_trip_and_keep_ellipses_round() {
        let dims = (5184, 3888);
        assert_eq!(to_frame([1.0, 1.0], dims), [1.0, 0.75]);
        assert_eq!(to_frame([0.5, 0.5], dims), [0.5, 0.375]);
        for uv in [[0.0, 0.0], [0.25, 0.9], [1.0, 1.0]] {
            let back = from_frame(to_frame(uv, dims), dims);
            assert!((back[0] - uv[0]).abs() < 1e-6 && (back[1] - uv[1]).abs() < 1e-6);
        }
        // A circle of radius 0.2 long edges reaches the same number of pixels both ways.
        let circle = one(&[radial([0.5, 0.375], [0.2, 0.2], 0.0, 0.0, false, ADD)], 100.0);
        let px = |dx: f32, dy: f32| {
            let uv = [0.5 + dx / dims.0 as f32, 0.5 + dy / dims.1 as f32];
            weight(&circle, to_frame(uv, dims))
        };
        let r = 0.19 * dims.0 as f32;
        assert!(px(r, 0.0) > 0.99 && px(0.0, r) > 0.99);
        let r = 0.21 * dims.0 as f32;
        assert!(px(r, 0.0) < 0.01 && px(0.0, r) < 0.01);
    }

    #[test]
    fn linear_gradient_is_full_then_gone() {
        // Full at x = 0.2, gone at x = 0.6, constant along y.
        let m = one(&[linear([0.2, 0.1], [0.6, 0.1], ADD)], 100.0);
        assert_eq!(weight(&m, [0.0, 0.5]), 1.0);
        assert_eq!(weight(&m, [0.2, 0.9]), 1.0);
        assert_eq!(weight(&m, [0.6, 0.0]), 0.0);
        assert_eq!(weight(&m, [0.9, 0.3]), 0.0);
        assert!((weight(&m, [0.4, 0.0]) - 0.5).abs() < 1e-6, "smoothstep is 0.5 in the middle");
        assert_eq!(weight(&m, [0.4, 0.0]), weight(&m, [0.4, 0.8]));
        // Smoothstep: eased at both ends, falling all the way.
        let mut last = 1.0;
        for i in 0..=40 {
            let w = weight(&m, [0.2 + i as f32 * 0.01, 0.0]);
            assert!(w <= last + 1e-6);
            last = w;
        }
        assert!(weight(&m, [0.21, 0.0]) > 0.99 && weight(&m, [0.59, 0.0]) < 0.01);
        // Any direction: a tilted gradient is constant along its bands.
        let t = one(&[linear([0.0, 0.0], [0.3, 0.4], ADD)], 100.0);
        assert!((weight(&t, [0.15, 0.2]) - 0.5).abs() < 1e-6);
        assert!((weight(&t, [0.15 + 0.4, 0.2 - 0.3]) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn radial_inside_edge_outside_feather_and_invert() {
        let c = [0.5, 0.4];
        // A 0.3 × 0.15 ellipse, tilted a quarter turn (so the long radius runs along y).
        let hard = one(&[radial(c, [0.3, 0.15], std::f32::consts::FRAC_PI_2, 0.0, false, ADD)], 100.0);
        assert_eq!(weight(&hard, c), 1.0);
        assert!(weight(&hard, [0.5, 0.4 + 0.29]) > 0.99, "along y the radius is the first one");
        assert!(weight(&hard, [0.5 + 0.14, 0.4]) > 0.99, "across, the second");
        assert!(weight(&hard, [0.5 + 0.16, 0.4]) < 0.01);
        assert!(weight(&hard, [0.5, 0.4 + 0.31]) < 0.01);
        // Feather 100: falls from the centre to the edge; 50: full out to half the radius.
        let soft = one(&[radial(c, [0.2, 0.2], 0.0, 100.0, false, ADD)], 100.0);
        assert_eq!(weight(&soft, c), 1.0);
        let w_half = weight(&soft, [0.5 + 0.1, 0.4]);
        assert!((w_half - 0.5).abs() < 1e-5, "{w_half}");
        assert_eq!(weight(&soft, [0.5 + 0.2, 0.4]), 0.0);
        let half = one(&[radial(c, [0.2, 0.2], 0.0, 50.0, false, ADD)], 100.0);
        assert_eq!(weight(&half, [0.5 + 0.09, 0.4]), 1.0);
        assert_eq!(weight(&half, [0.5 + 0.1, 0.4]), 1.0);
        assert!((weight(&half, [0.5 + 0.15, 0.4]) - 0.5).abs() < 1e-5);
        // Inverted: the effect is outside.
        let inv = one(&[radial(c, [0.2, 0.2], 0.0, 50.0, true, ADD)], 100.0);
        for dx in [0.0, 0.05, 0.15, 0.25] {
            let p = [0.5 + dx, 0.4];
            assert!((weight(&half, p) + weight(&inv, p) - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn union_subtract_and_amount() {
        let a = radial([0.3, 0.5], [0.2, 0.2], 0.0, 100.0, false, ADD);
        let b = radial([0.5, 0.5], [0.2, 0.2], 0.0, 100.0, false, ADD);
        let (wa, wb) = (weight(&one(&[a], 100.0), [0.4, 0.5]), weight(&one(&[b], 100.0), [0.4, 0.5]));
        // The soft union never exceeds 1 and is at least either.
        let u = weight(&one(&[a, b], 100.0), [0.4, 0.5]);
        assert!((u - (1.0 - (1.0 - wa) * (1.0 - wb))).abs() < 1e-6 && u >= wa && u >= wb && u <= 1.0);
        // Subtracting a shape takes its weight away: full there means gone.
        let hole = radial([0.4, 0.5], [0.05, 0.05], 0.0, 0.0, false, SUBTRACT);
        let full = linear([2.0, 0.0], [3.0, 0.0], ADD); // full everywhere to the left of x = 2
        let m = one(&[full, hole], 100.0);
        assert_eq!(weight(&m, [0.4, 0.5]), 0.0);
        assert_eq!(weight(&m, [0.1, 0.1]), 1.0);
        // A partly subtracted pixel keeps the rest.
        let soft_hole = radial([0.4, 0.5], [0.2, 0.2], 0.0, 100.0, false, SUBTRACT);
        let w = weight(&one(&[full, soft_hole], 100.0), [0.5, 0.5]);
        let w_hole = weight(&one(&[radial([0.4, 0.5], [0.2, 0.2], 0.0, 100.0, false, ADD)], 100.0), [0.5, 0.5]);
        assert!((w - (1.0 - w_hole)).abs() < 1e-6);
        // A mask with only subtractions has nothing; Amount scales the whole.
        assert_eq!(weight(&one(&[hole], 100.0), [0.4, 0.5]), 0.0);
        assert_eq!(weight(&one(&[full], 40.0), [0.1, 0.1]), 0.4);
        assert_eq!(weight(&one(&[full], 0.0), [0.1, 0.1]), 0.0);
        // A mask with no shapes has no weight; an unknown kind (the brush, for now) too.
        assert_eq!(weight(&one(&[], 100.0), [0.1, 0.1]), 0.0);
        assert_eq!(weight(&one(&[Component { kind: KIND_BRUSH, ..Default::default() }], 100.0), [0.1, 0.1]), 0.0);
    }

    #[test]
    fn sliders_add_up_weighted() {
        let mut masks = Masks::default();
        for (i, (x, ev)) in [(0.3f32, 1.0f32), (0.7, -0.5)].into_iter().enumerate() {
            let mut m = Mask::new();
            m.sliders[slider::EXPOSURE] = ev;
            m.sliders[slider::CONTRAST] = 50.0;
            m.sliders[slider::SATURATION] = -20.0;
            m.components[0] = linear([x + 1.0, 0.0], [x + 2.0, 0.0], ADD); // full everywhere
            m.count = 1;
            masks.masks[i] = m;
        }
        masks.count = 2;
        let l = GpuMaskSet::new(&masks, &IDENTITY).local([0.5, 0.5]);
        assert!((l.exposure - 0.5).abs() < 1e-6 && (l.contrast - 1.0).abs() < 1e-6 && (l.saturation + 0.4).abs() < 1e-6);
        assert_eq!(l.wb_log2, [0.0; 3]);
        // Half-weight at the edge of a gradient halves the exposure.
        let mut m = Mask::new();
        m.sliders[slider::EXPOSURE] = 2.0;
        m.components[0] = linear([0.0, 0.0], [1.0, 0.0], ADD);
        m.count = 1;
        let mut masks = Masks::default();
        masks.masks[0] = m;
        masks.count = 1;
        let l = GpuMaskSet::new(&masks, &IDENTITY).local([0.5, 0.2]);
        assert!((l.exposure - 1.0).abs() < 1e-6);
    }

    #[test]
    fn local_white_balance_warms_and_is_neutral_at_zero() {
        assert_eq!(white_balance_log2(&OM5_XYZ_TO_CAM, 0.0, 0.0), [0.0; 3]);
        let warm = white_balance_log2(&OM5_XYZ_TO_CAM, 100.0, 0.0);
        assert!(warm[0] > 0.0 && warm[2] < 0.0, "more red, less blue: {warm:?}");
        let cool = white_balance_log2(&OM5_XYZ_TO_CAM, -100.0, 0.0);
        assert!(cool[0] < 0.0 && cool[2] > 0.0, "{cool:?}");
        // Roughly what the global slider does for the same move: +2000 K at 5500 K.
        let base = temperature_to_multipliers(&OM5_XYZ_TO_CAM, 5500.0, 0.0);
        let up = temperature_to_multipliers(&OM5_XYZ_TO_CAM, 7500.0, 0.0);
        assert!((warm[0] - (up[0] / base[0]).log2()).abs() < 1e-6);
        // Tint: +100 is magenta, so green is held back against red and blue.
        let magenta = white_balance_log2(&OM5_XYZ_TO_CAM, 0.0, 100.0);
        assert!(magenta[1] < magenta[0] && magenta[1] < magenta[2], "{magenta:?}");
    }

    #[test]
    fn clean_repairs_damage() {
        let mut m = one(
            &[
                radial([0.5, 0.5], [0.0, f32::NAN], f32::INFINITY, 900.0, false, 7),
                Component { kind: 9, ..Default::default() },
                linear([0.0, 0.0], [1.0, 0.0], ADD),
            ],
            900.0,
        );
        m.masks[0].sliders[slider::EXPOSURE] = 40.0;
        m.masks[0].sliders[slider::TINT] = f32::NAN;
        m.count = 99;
        m.clean();
        assert_eq!(m.count, MAX_MASKS as u32);
        let mask = &m.masks[0];
        assert_eq!((mask.amount, mask.sliders[slider::EXPOSURE], mask.sliders[slider::TINT]), (100.0, 4.0, 0.0));
        assert_eq!(mask.count, 2, "the unknown shape is dropped");
        let r = &mask.components[0];
        assert_eq!((r.mode, r.p[2], r.p[3], r.p[4], r.p[5]), (1, 1e-4, 1e-4, 0.0, 100.0));
        assert_eq!(mask.components[1].kind, KIND_LINEAR);
        assert_eq!(mask.components[2], Component::default());
    }

    #[test]
    fn text_round_trip() {
        let mut masks = Masks::default();
        let mut a = Mask::new();
        a.amount = 80.0;
        a.sliders[slider::EXPOSURE] = 0.65;
        a.sliders[slider::CLARITY] = -35.0;
        a.components[0] = linear([0.1, 0.2], [0.35, 0.6], ADD);
        a.components[1] = radial([0.5, 0.375], [0.25, 0.125], 0.5, 30.0, true, SUBTRACT);
        a.count = 2;
        let mut b = Mask::new();
        b.components[0] = radial([0.2, 0.3], [0.1, 0.1], 0.0, 50.0, false, ADD);
        b.count = 1;
        masks.masks[0] = a;
        masks.masks[1] = b;
        masks.count = 2;
        let text = to_text(&masks);
        assert!(text.contains("exposure=0.65") && text.contains("R- ") && !text.contains("tint"), "{text}");
        assert_eq!(from_text(&text), masks);
        assert_eq!(to_text(&Masks::default()), "");
        assert_eq!(from_text(""), Masks::default());
        // Damage costs the piece it is in and nothing else.
        let bad = text.replace("L+ 0.1 0.2 0.35 0.6", "L+ 0.1 x").replace("amount=80", "amount=zzz");
        let read = from_text(&bad);
        assert_eq!((read.count, read.masks[0].count, read.masks[0].amount), (2, 1, 100.0));
        assert_eq!(read.masks[1], b);
        assert_eq!(from_text("garbage ; | | ; amount=5").count, 3, "empty-looking masks stay, as masks with no shapes");
    }

    #[test]
    fn packed_size_matches_the_shader() {
        assert_eq!(size_of::<GpuComponent>(), 48);
        assert_eq!(size_of::<GpuMask>(), 448);
        assert_eq!(size_of::<GpuMaskSet>(), 3600);
    }
}
