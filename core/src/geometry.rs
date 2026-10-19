//! Crop, straighten and rotate (docs/crop.md): how the view maps output pixels to the texture,
//! and the crop's rules.
//!
//! The *frame* is the photo after the camera's orientation and the edit's quarter turns, W × H
//! pixels. The photo is turned by `angle` beneath it: a point p of the frame shows the photo at
//! c + R(−angle)·(p − c), c being the centre and R a clockwise turn, so a positive angle turns
//! the picture clockwise. The crop is a rectangle in the frame, as fractions of its size.

use crate::decode::Orientation;
use crate::develop::{Edit, FULL_CROP};
use crate::pipeline::Zoom;

pub const MAX_ANGLE: f32 = 45.0;

/// In crop mode the frame is fitted with this fraction of the view free on every side, so there's room
/// outside the box to grab and turn.
pub const FRAME_MARGIN: f64 = 0.09;

/// The smallest crop, as a fraction of the frame in either direction.
const MIN_SIZE: f64 = 0.002;

/// Left, top, right and bottom, as fractions of the frame.
pub type Rect = [f64; 4];

/// Quarter turns clockwise from the texture to the frame: the camera's and the edit's.
pub fn turns(camera: Orientation, rotation: u32) -> u32 {
    let camera = match camera {
        Orientation::Normal => 0,
        Orientation::Rotate90 => 1,
        Orientation::Rotate180 => 2,
        Orientation::Rotate270 => 3,
    };
    (camera + rotation) % 4
}

/// The frame's size: the photo's size after the camera's orientation, turned by `rotation`.
pub fn frame_size(photo: (u32, u32), rotation: u32) -> (u32, u32) {
    if rotation % 2 == 1 { (photo.1, photo.0) } else { photo }
}

/// The crop's rectangle in frame pixels: x, y, width, height.
pub fn window(frame: (u32, u32), crop: [f32; 4]) -> [f64; 4] {
    let (w, h) = (frame.0 as f64, frame.1 as f64);
    let [l, t, r, b] = crop.map(|v| v as f64);
    [l * w, t * h, (r - l) * w, (b - t) * h]
}

/// Puts an edit's geometry in range: what a hand-edited or damaged sidecar might hold.
pub fn clean(edit: &mut Edit) {
    edit.rotation %= 4;
    edit.angle = if edit.angle.is_finite() { edit.angle.clamp(-MAX_ANGLE, MAX_ANGLE) } else { 0.0 };
    let [l, t, r, b] = edit.crop;
    // A turned photo reaches past the frame (see `reach`), so a crop may too, but never further
    // than a whole frame out.
    let ok = edit.crop.iter().all(|v| v.is_finite() && (-1.0..=2.0).contains(v)) && r - l >= MIN_SIZE as f32 && b - t >= MIN_SIZE as f32;
    if !ok {
        edit.crop = FULL_CROP;
    }
}

/// Where the view puts what, for the develop pass.
pub struct ViewMap {
    /// Texture coordinates (0..1) at output pixel (0, 0), and the change per pixel right and down.
    pub origin: [f32; 2],
    pub step_x: [f32; 2],
    pub step_y: [f32; 2],
    /// The pyramid level to sample.
    pub lod: f32,
    /// The picture's rectangle on the output, in pixels: left, top, right, bottom. Outside it
    /// is empty canvas (the rest of the photo past a crop, say).
    pub clip: [f32; 4],
    /// The rectangle the app maps crop fractions on: the frame's in crop mode (smaller than `clip`
    /// when the photo is turned), the same as `clip` otherwise.
    pub frame_rect: [f32; 4],
}

/// The affine map from output pixels to texture coordinates, covering zoom, pan, the camera's
/// orientation, the quarter turns, straightening and the crop. `texture` is the texture's size,
/// `turns` from [`turns`], and `frame` the frame's size. `Zoom` is relative to the crop (to the
/// whole frame for `Zoom::Frame`, wherever its centre is), except `Region`, which is in frame pixels.
pub fn view_map(
    texture: (u32, u32),
    turns: u32,
    frame: (u32, u32),
    angle: f32,
    crop: [f32; 4],
    zoom: Zoom,
    out: (u32, u32),
) -> ViewMap {
    // In double precision: the shader adds a step per pixel, so a step off by one part in 10^7
    // is a visible fraction of a pixel by the far side of a big export tile.
    let (fw, fh) = (frame.0 as f64, frame.1 as f64);
    let (out_w, out_h) = (out.0 as f64, out.1 as f64);
    // In crop mode the view takes in the whole turned photo, which reaches past the frame.
    let (ex, ey) = reach((fw, fh), angle as f64);
    let [wx, wy, ww, wh] = match zoom {
        Zoom::Frame { .. } => [(0.5 - ex) * fw, (0.5 - ey) * fh, 2.0 * ex * fw, 2.0 * ey * fh],
        _ => window(frame, crop),
    };
    let (scale, center) = match zoom {
        Zoom::Fit => ((out_w / ww).min(out_h / wh), (wx + ww / 2.0, wy + wh / 2.0)),
        Zoom::Frame { x, y } => {
            let room = 1.0 - 2.0 * FRAME_MARGIN;
            ((out_w * room / ww).min(out_h * room / wh), (x as f64 * fw, y as f64 * fh))
        }
        Zoom::Actual { x, y } => (1.0, (wx + x.clamp(0.0, 1.0) as f64 * ww, wy + y.clamp(0.0, 1.0) as f64 * wh)),
        Zoom::Region { x, y, scale } => (scale as f64, (x as f64 + out_w / scale as f64 / 2.0, y as f64 + out_h / scale as f64 / 2.0)),
    };
    let (sin, cos) = (angle as f64).to_radians().sin_cos();
    // Output pixel → frame → turned photo (0..1) → texture (0..1).
    let map = |px: f64, py: f64| -> [f64; 2] {
        let dx = center.0 + (px - out_w / 2.0) / scale - fw / 2.0;
        let dy = center.1 + (py - out_h / 2.0) / scale - fh / 2.0;
        let u = (dx * cos + dy * sin + fw / 2.0) / fw;
        let v = (-dx * sin + dy * cos + fh / 2.0) / fh;
        match turns % 4 {
            0 => [u, v],
            1 => [v, 1.0 - u],
            2 => [1.0 - u, 1.0 - v],
            _ => [1.0 - v, u],
        }
    };
    let o = map(0.0, 0.0);
    let x = map(1.0, 0.0);
    let y = map(0.0, 1.0);
    // Texture pixels per output pixel, along the frame's width.
    let texture_w = if turns % 2 == 1 { texture.1 } else { texture.0 } as f64;
    let lod = (texture_w / fw / scale).log2().max(0.0);
    let at = |p: f64, c: f64, half: f64| (half + (p - c) * scale) as f32;
    let on_output = |[x, y, w, h]: [f64; 4]| [at(x, center.0, out_w / 2.0), at(y, center.1, out_h / 2.0), at(x + w, center.0, out_w / 2.0), at(y + h, center.1, out_h / 2.0)];
    let clip = match zoom {
        Zoom::Region { .. } => [-1e9, -1e9, 1e9, 1e9],
        _ => on_output([wx, wy, ww, wh]),
    };
    // What crop fractions are fractions of: the frame in crop mode, the crop otherwise.
    let frame_rect = match zoom {
        Zoom::Frame { .. } => on_output([0.0, 0.0, fw, fh]),
        _ => clip,
    };
    let f = |v: [f64; 2]| [v[0] as f32, v[1] as f32];
    ViewMap {
        origin: f(o),
        step_x: f([x[0] - o[0], x[1] - o[1]]),
        step_y: f([y[0] - o[0], y[1] - o[1]]),
        lod: lod as f32,
        clip,
        frame_rect,
    }
}

// ---- Keeping the crop inside the photo ----

/// How far the turned photo reaches from the frame's centre, as fractions of the frame's width and
/// height: its bounding box is 0.5 ± these. More than 0.5 once it's turned, so a crop may lie partly
/// outside 0..1.
pub fn reach(frame: (f64, f64), angle: f64) -> (f64, f64) {
    let (w, h) = frame;
    let (sin, cos) = angle.to_radians().sin_cos();
    let (sin, cos) = (sin.abs(), cos.abs());
    ((w * cos + h * sin) / (2.0 * w), (w * sin + h * cos) / (2.0 * h))
}

/// Whether the rectangle lies inside the photo turned by `angle` degrees.
pub fn fits(frame: (f64, f64), angle: f64, r: Rect) -> bool {
    fits_within(frame, angle, r, 1e-4)
}

/// [`fits`] allowing the box to be `tolerance` pixels outside. Only the turned photo bounds it:
/// turned, its corners reach past the frame, and the box may follow them there.
fn fits_within(frame: (f64, f64), angle: f64, r: Rect, tolerance: f64) -> bool {
    let (w, h) = frame;
    let (sin, cos) = angle.to_radians().sin_cos();
    [(r[0], r[1]), (r[2], r[1]), (r[0], r[3]), (r[2], r[3])].iter().all(|&(fx, fy)| {
        let (dx, dy) = (fx * w - w / 2.0, fy * h - h / 2.0);
        let (qx, qy) = (dx * cos + dy * sin, -dx * sin + dy * cos);
        qx.abs() <= w / 2.0 + tolerance && qy.abs() <= h / 2.0 + tolerance
    })
}

/// How [`constrain_crop`] treats a desired rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Constrain {
    /// Keep the desired rectangle's centre and shape, and make it smaller until it fits (the
    /// angle changed, or the aspect ratio did).
    Shrink,
    /// Move the base rectangle by the desired one's offset, as far as it goes: it slides along an edge.
    Slide,
    /// Move the edges where the desired rectangle differs from the base, each as far as it goes.
    Edges,
    /// Scale the desired rectangle about this point (fractions), keeping its shape, until it fits.
    Scale([f64; 2]),
    /// The largest rectangle of this aspect ratio (width over height, in pixels) inside the
    /// desired one, with its centre, made to fit.
    Aspect(f64),
}

/// The valid rectangle nearest to what the user asked for. `base` is a rectangle that fits (the
/// crop before this drag step), `desired` what the drag asks for, possibly past the photo.
pub fn constrain_crop(frame: (f64, f64), angle: f64, base: Rect, desired: Rect, how: Constrain) -> Rect {
    // The same few hundredths of a pixel as for the base below: a box wedged against the photo by a
    // straighten is a hair outside the exact limit once it has been through `f32`, and judged to the
    // exact limit every move away from there would fail and freeze it.
    let ok = |r: Rect| fits_within(frame, angle, r, 0.05);
    // A box that was valid and has been through `f32` (as the app and the sidecar hold it) is off by
    // about 1e-4 pixels, so the base only has to be valid to a few hundredths of a pixel. One that
    // is not valid at all is recovered by shrinking it as straightening would, never by collapsing it.
    let base = if fits_within(frame, angle, base, 0.05) { base } else { shrink(frame, angle, base) };
    // The furthest value along good → bad that still passes (good must pass).
    let furthest = |good: f64, bad: f64, pass: &dyn Fn(f64) -> bool| {
        if pass(bad) {
            return bad;
        }
        let (mut g, mut b) = (good, bad);
        for _ in 0..40 {
            let m = (g + b) / 2.0;
            if pass(m) { g = m } else { b = m }
        }
        g
    };
    match how {
        Constrain::Shrink => shrink(frame, angle, desired),
        Constrain::Aspect(ratio) => {
            let (wpx, hpx) = ((desired[2] - desired[0]) * frame.0, (desired[3] - desired[1]) * frame.1);
            let (wpx, hpx) = if wpx / hpx > ratio { (hpx * ratio, hpx) } else { (wpx, wpx / ratio) };
            let (cx, cy) = ((desired[0] + desired[2]) / 2.0, (desired[1] + desired[3]) / 2.0);
            let (hw, hh) = (wpx / frame.0 / 2.0, hpx / frame.1 / 2.0);
            shrink(frame, angle, [cx - hw, cy - hh, cx + hw, cy + hh])
        }
        Constrain::Slide => {
            // The base's box, at the place nearest the desired one where it fits. Moving x and then
            // y separately would stall against a turned edge, which only a move along it can follow.
            let (hw, hh) = ((base[2] - base[0]) / 2.0, (base[3] - base[1]) / 2.0);
            let want = [(desired[0] + desired[2]) / 2.0 - 0.5, (desired[1] + desired[3]) / 2.0 - 0.5];
            match nearest_centre(frame, angle, hw, hh, 1.0, [want[0] * frame.0, want[1] * frame.1]) {
                Some(p) => {
                    let (cx, cy) = (0.5 + p[0] / frame.0, 0.5 + p[1] / frame.1);
                    [cx - hw, cy - hh, cx + hw, cy + hh]
                }
                None => base,
            }
        }
        Constrain::Edges => {
            let mut r = base;
            for i in 0..4 {
                if desired[i] == base[i] {
                    continue;
                }
                // Not past the opposite edge, and not outside the turned photo's bounding box.
                let (ex, ey) = reach(frame, angle);
                let e = if i % 2 == 0 { ex } else { ey };
                let want = match i {
                    0 => desired[0].min(r[2] - MIN_SIZE),
                    1 => desired[1].min(r[3] - MIN_SIZE),
                    2 => desired[2].max(r[0] + MIN_SIZE),
                    _ => desired[3].max(r[1] + MIN_SIZE),
                }
                .clamp(0.5 - e, 0.5 + e);
                r[i] = furthest(base[i], want, &|v| {
                    let mut t = r;
                    t[i] = v;
                    ok(t)
                });
            }
            r
        }
        Constrain::Scale(anchor) => {
            let at = |s: f64| {
                [
                    anchor[0] + s * (desired[0] - anchor[0]),
                    anchor[1] + s * (desired[1] - anchor[1]),
                    anchor[0] + s * (desired[2] - anchor[0]),
                    anchor[1] + s * (desired[3] - anchor[1]),
                ]
            };
            at(furthest(0.0, 1.0, &|s| ok(at(s))))
        }
    }
}

/// `desired` made smaller until it fits, keeping its shape. It shrinks about its own centre when
/// that leaves a box of reasonable size; when the centre is near the photo's edge, or outside the
/// photo (a box in a corner of the frame, which straightening cuts off), the centre moves to the
/// nearest place where the largest box that fits has room, so a box is never reduced to a speck.
fn shrink(frame: (f64, f64), angle: f64, desired: Rect) -> Rect {
    let (w, h) = frame;
    let (hw, hh) = ((desired[2] - desired[0]) / 2.0, (desired[3] - desired[1]) / 2.0);
    let (cx, cy) = ((desired[0] + desired[2]) / 2.0, (desired[1] + desired[3]) / 2.0);
    let around = |cx: f64, cy: f64, s: f64| [cx - s * hw, cy - s * hh, cx + s * hw, cy + s * hh];
    // The box of scale s centred at (x, y), in pixels from the frame's centre, is inside the frame and
    // inside the turned photo when each of these holds: a·(x, y) ≤ b0 − s·k. They come in opposite
    // pairs about the origin, so the centre has room exactly while every b0 − s·k is not negative.
    if hw * w <= 0.0 || hh * h <= 0.0 {
        return desired;
    }
    let planes = box_planes(frame, angle, hw, hh);
    // The biggest scale there is room for anywhere, and the biggest with the desired centre.
    let anywhere = planes.iter().map(|&(_, b0, k)| b0 / k).fold(1.0, f64::min);
    let p0 = [(cx - 0.5) * w, (cy - 0.5) * h];
    let here = planes.iter().map(|&(n, b0, k)| (b0 - (n[0] * p0[0] + n[1] * p0[1])) / k).fold(anywhere, f64::min).max(0.0);
    if here >= 0.6 * anywhere {
        return around(cx, cy, here);
    }
    // Too small that way: the biggest box, as near the desired centre as it goes.
    let s = anywhere * (1.0 - 1e-9);
    match nearest_centre(frame, angle, hw, hh, s, p0) {
        Some(p) => around(0.5 + p[0] / w, 0.5 + p[1] / h, s),
        None => around(0.5, 0.5, s),
    }
}

/// The box with half-size (`hw`, `hh`) (fractions of the frame) scaled by s and centred at (x, y),
/// in pixels from the frame's centre, is inside the turned photo when each of these holds:
/// n·(x, y) ≤ b0 − s·k. They come in opposite pairs about the origin, so the centre has room
/// exactly while every b0 − s·k is not negative.
fn box_planes(frame: (f64, f64), angle: f64, hw: f64, hh: f64) -> [([f64; 2], f64, f64); 4] {
    let (w, h) = frame;
    let (sin, cos) = angle.to_radians().sin_cos();
    let (a, b) = (hw * w, hh * h);
    [
        ([cos, sin], w / 2.0, a * cos.abs() + b * sin.abs()),
        ([-cos, -sin], w / 2.0, a * cos.abs() + b * sin.abs()),
        ([-sin, cos], h / 2.0, a * sin.abs() + b * cos.abs()),
        ([sin, -cos], h / 2.0, a * sin.abs() + b * cos.abs()),
    ]
}

/// Where the box of [`box_planes`] at scale s fits, nearest to `p0` (pixels from the frame's
/// centre); None when it fits nowhere. The places it fits are a convex polygon, so the nearest is
/// `p0` itself, its foot on one of the polygon's lines, or a corner where two lines meet.
fn nearest_centre(frame: (f64, f64), angle: f64, hw: f64, hh: f64, s: f64, p0: [f64; 2]) -> Option<[f64; 2]> {
    let limits: Vec<([f64; 2], f64)> = box_planes(frame, angle, hw, hh).iter().map(|&(n, b0, k)| (n, b0 - s * k)).collect();
    let inside = |p: [f64; 2]| limits.iter().all(|&(n, l)| n[0] * p[0] + n[1] * p[1] <= l + 1e-3);
    let mut best: Option<([f64; 2], f64)> = None;
    let mut consider = |p: [f64; 2]| {
        if inside(p) {
            let d = (p[0] - p0[0]).powi(2) + (p[1] - p0[1]).powi(2);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((p, d));
            }
        }
    };
    consider(p0);
    for &(n, l) in &limits {
        let off = n[0] * p0[0] + n[1] * p0[1] - l;
        consider([p0[0] - off * n[0], p0[1] - off * n[1]]);
    }
    for i in 0..limits.len() {
        for j in i + 1..limits.len() {
            let ((n, l), (m, k)) = (limits[i], limits[j]);
            let det = n[0] * m[1] - n[1] * m[0];
            if det.abs() > 1e-12 {
                consider([(l * m[1] - k * n[1]) / det, (n[0] * k - m[0] * l) / det]);
            }
        }
    }
    best.map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: (f64, f64) = (6000.0, 4000.0);
    const FULL: Rect = [0.0, 0.0, 1.0, 1.0];

    fn near(a: Rect, b: Rect) {
        for i in 0..4 {
            assert!((a[i] - b[i]).abs() < 1e-6, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn the_whole_frame_fits_only_when_straight() {
        assert!(fits(FRAME, 0.0, FULL));
        assert!(!fits(FRAME, 1.0, FULL));
        // Small crops near the middle fit at any angle.
        assert!(fits(FRAME, 30.0, [0.45, 0.45, 0.55, 0.55]));
        assert!(!fits(FRAME, 0.0, [-0.1, 0.0, 0.5, 0.5]));
    }

    #[test]
    fn straightening_shrinks_the_crop_about_its_centre_and_keeps_its_shape() {
        for angle in [-45.0, -10.0, -0.5, 0.5, 3.0, 10.0, 30.0, 45.0] {
            let r = constrain_crop(FRAME, angle, FULL, FULL, Constrain::Shrink);
            assert!(fits(FRAME, angle, r), "{angle}: {r:?}");
            near([r[0] + r[2], r[1] + r[3], 0.0, 0.0], [1.0, 1.0, 0.0, 0.0]);
            let aspect = (r[2] - r[0]) * FRAME.0 / ((r[3] - r[1]) * FRAME.1);
            assert!((aspect - 1.5).abs() < 1e-6, "{angle}: aspect {aspect}");
            // Just enough: a little bigger would not fit.
            let grown = [r[0] - 1e-4, r[1] - 1e-4, r[2] + 1e-4, r[3] + 1e-4];
            assert!(!fits(FRAME, angle, grown) || angle.abs() < 1e-3, "{angle}: {r:?}");
        }
        // Straight stays whole.
        near(constrain_crop(FRAME, 0.0, FULL, FULL, Constrain::Shrink), FULL);
    }

    #[test]
    fn straightening_back_gives_the_box_back() {
        // The app shrinks the box as the user last set it, never the shrunk one, so any
        // walk of angles that ends at 0° ends at the original box.
        let intended = [0.1, 0.2, 0.7, 0.8];
        for angles in [[10.0, -5.0, 0.0], [30.0, 8.0, 0.0], [-44.0, 44.0, 0.0]] {
            let mut shown = intended;
            for angle in angles {
                shown = constrain_crop(FRAME, angle, shown, intended, Constrain::Shrink);
                assert!(fits(FRAME, angle, shown));
            }
            near(shown, intended);
        }
    }

    #[test]
    fn a_box_kept_in_f32_still_fits_and_slides_without_shrinking() {
        // Straighten, hold the box as `f32` the way the app does, then move it: it must stay the same
        // size (a box taken for invalid collapses to a sliver).
        for angle in [0.3, 2.0, 9.5, 17.0, -9.5, -33.0] {
            let shown = constrain_crop(FRAME, angle, FULL, FULL, Constrain::Shrink);
            let held = shown.map(|v| v as f32 as f64);
            assert!(fits_within(FRAME, angle, held, 0.05), "{angle}: {held:?}");
            let moved = constrain_crop(FRAME, angle, held, [held[0] - 0.1, held[1], held[2] - 0.1, held[3]], Constrain::Slide);
            assert!(((moved[2] - moved[0]) - (held[2] - held[0])).abs() < 1e-9, "{angle}: {moved:?}");
        }
    }

    #[test]
    fn a_straightened_box_held_in_f32_can_still_be_resized_and_moved() {
        // Straightening leaves the box touching the photo's edges. Held as `f32` it is a hair over
        // the exact limit, which must not freeze it: shrinking an edge takes the edge, and the box slides
        // away from the corner it is wedged in.
        for angle in [2.0, 9.5, 16.1, -9.5, -33.0] {
            let held = constrain_crop(FRAME, angle, FULL, FULL, Constrain::Shrink).map(|v| v as f32 as f64);
            let mut want = held;
            want[2] -= 0.1;
            let r = constrain_crop(FRAME, angle, held, want, Constrain::Edges);
            assert!((r[2] - want[2]).abs() < 1e-6, "{angle}: edges {r:?} for {want:?}");
            // Shrink first so there is room, then slide it about.
            let small = [0.4, 0.4, 0.6, 0.6];
            let moved = constrain_crop(FRAME, angle, small, [0.35, 0.4, 0.55, 0.6], Constrain::Slide);
            assert!((moved[0] - 0.35).abs() < 1e-6, "{angle}: slide {moved:?}");
            // And the held full-size box moves toward the middle of its slack, if it has any, without freezing.
            let _ = constrain_crop(FRAME, angle, held, [held[0] - 0.01, held[1], held[2] - 0.01, held[3]], Constrain::Slide);
        }
    }


    /// A small deterministic random source, so the property test needs no crate and fails the same way every time.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn range(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * self.next()
        }
    }
    #[test]
    fn no_drag_or_straighten_ever_collapses_the_box() {
        // Random frames, angles (a few of them the ones people use) and boxes held as `f32` the way the
        // app holds them, put through every kind of change. Whatever happens the result fits, a slide
        // keeps the size, an edge only moves the way it was asked, a scale keeps the shape and is not
        // smaller than asked, and straightening leaves a box of at least 60% of the biggest that fits.
        let mut rng = Lcg(2024);
        let frames = [(6000.0, 4000.0), (4000.0, 6000.0), (5184.0, 3456.0), (4608.0, 3456.0), (3000.0, 3000.0)];
        let fixed = [-15.0, -4.2, -4.0, 0.0, 4.0, 4.2, 15.0];
        let mut tried = 0;
        for n in 0..30000 {
            let frame = frames[(rng.next() * frames.len() as f64) as usize % frames.len()];
            let angle = if n % 3 == 0 { fixed[(rng.next() * fixed.len() as f64) as usize % fixed.len()] } else { rng.range(-45.0, 45.0) };
            let mut random_box = |rng: &mut Lcg| {
                let (x0, y0) = (rng.range(0.0, 0.9), rng.range(0.0, 0.9));
                [x0, y0, rng.range(x0 + 0.03, 1.0), rng.range(y0 + 0.03, 1.0)]
            };
            // Any box at all, in the frame: where a crop made at another angle (or none) leaves the intended one.
            let intended = random_box(&mut rng);
            let (iw, ih) = (intended[2] - intended[0], intended[3] - intended[1]);

            // Straightening: from the intended box.
            let shrunk = constrain_crop(frame, angle, intended, intended, Constrain::Shrink);
            assert!(fits(frame, angle, shrunk), "shrink doesn't fit: {angle} {frame:?} {intended:?} -> {shrunk:?}");
            let scale = (shrunk[2] - shrunk[0]) / iw;
            assert!(((shrunk[3] - shrunk[1]) / ih - scale).abs() < 1e-6, "shrink changed the shape: {angle} {frame:?} {intended:?} -> {shrunk:?}");
            let centred = {
                let (mut good, mut bad) = (0.0, 1.0);
                let at = |s: f64| [0.5 - s * iw / 2.0, 0.5 - s * ih / 2.0, 0.5 + s * iw / 2.0, 0.5 + s * ih / 2.0];
                if fits(frame, angle, at(1.0)) { 1.0 } else {
                    for _ in 0..50 {
                        let m = (good + bad) / 2.0;
                        if fits(frame, angle, at(m)) { good = m } else { bad = m }
                    }
                    good
                }
            };
            assert!(scale >= 0.6 * centred - 1e-6, "shrink left a speck: {angle} {frame:?} {intended:?} -> {shrunk:?} (room for {centred})");

            // Dragging: from a box that fits, held as `f32`.
            let base = shrunk.map(|v| v as f32 as f64);
            let (bw, bh) = (base[2] - base[0], base[3] - base[1]);
            if bw < 0.05 || bh < 0.05 {
                continue;
            }
            tried += 1;

            let d = [rng.range(-0.5, 0.5), rng.range(-0.5, 0.5)];
            let slid = constrain_crop(frame, angle, base, [base[0] + d[0], base[1] + d[1], base[2] + d[0], base[3] + d[1]], Constrain::Slide);
            assert!(fits_within(frame, angle, slid, 0.05), "slide doesn't fit: {angle} {frame:?} {base:?} {d:?} -> {slid:?}");
            assert!(((slid[2] - slid[0]) - bw).abs() < 1e-6 && ((slid[3] - slid[1]) - bh).abs() < 1e-6, "slide changed the size: {angle} {frame:?} {base:?} {d:?} -> {slid:?}");

            let i = (rng.next() * 4.0) as usize % 4;
            let mut want = base;
            want[i] += rng.range(-0.4, 0.4);
            let edged = constrain_crop(frame, angle, base, want, Constrain::Edges);
            assert!(fits_within(frame, angle, edged, 0.05), "edge doesn't fit: {angle} {frame:?} {base:?} {want:?} -> {edged:?}");
            for j in 0..4 {
                if j != i {
                    assert_eq!(edged[j], base[j], "edge {i} moved edge {j}");
                }
            }
            let moved = edged[i] - base[i];
            assert!(moved * (want[i] - base[i]) >= -1e-12, "edge {i} went the wrong way: {base:?} {want:?} -> {edged:?}");
            assert!(edged[2] - edged[0] >= MIN_SIZE - 1e-12 && edged[3] - edged[1] >= MIN_SIZE - 1e-12, "edge collapsed: {edged:?}");

            let f = rng.range(0.3, 1.8);
            let anchor = [rng.range(base[0], base[2]), rng.range(base[1], base[3])];
            let desired = [anchor[0] + f * (base[0] - anchor[0]), anchor[1] + f * (base[1] - anchor[1]), anchor[0] + f * (base[2] - anchor[0]), anchor[1] + f * (base[3] - anchor[1])];
            let scaled = constrain_crop(frame, angle, base, desired, Constrain::Scale(anchor));
            assert!(fits_within(frame, angle, scaled, 0.05), "scale doesn't fit: {angle} {frame:?} {base:?} {desired:?} -> {scaled:?}");
            let (sw, sh) = (scaled[2] - scaled[0], scaled[3] - scaled[1]);
            assert!((sw / bw - sh / bh).abs() < 1e-6, "scale changed the shape: {scaled:?}");
            assert!(sw >= bw.min(bw * f) - 1e-6, "scale collapsed: {angle} {frame:?} {base:?} {desired:?} -> {scaled:?}");
        }
        assert!(tried > 10000, "only {tried} cases got as far as the drags");
    }

    #[test]
    fn a_box_in_the_corner_survives_straightening_either_way() {
        // The recorded collapse: a crop in the frame's top-left corner, then the Straighten slider moved a
        // few degrees. The corner of the frame is cut off by the turned photo, the box's centre is outside
        // it, and the old search reduced the box to a speck on the photo's edge.
        for angle in [-15.0, -4.2, -4.0, 4.0, 4.2, 15.0] {
            for corner in [[0.0, 0.0, 0.3, 0.3], [0.7, 0.0, 1.0, 0.3], [0.0, 0.7, 0.3, 1.0], [0.7, 0.7, 1.0, 1.0]] {
                let r = constrain_crop(FRAME, angle, corner, corner, Constrain::Shrink);
                assert!(fits(FRAME, angle, r), "{angle} {corner:?}: {r:?}");
                assert!(r[2] - r[0] > 0.2, "{angle} {corner:?} became {r:?}");
            }
        }
    }

    #[test]
    fn square_frame_at_45_degrees() {
        // The biggest centred square in a square turned 45° has a side of 1/√2.
        let r = constrain_crop((1000.0, 1000.0), 45.0, FULL, FULL, Constrain::Shrink);
        let side = r[2] - r[0];
        assert!((side - 0.5_f64.sqrt()).abs() < 1e-6, "{side}");
    }

    #[test]
    fn a_crop_off_to_one_side_moves_in_when_its_centre_is_outside() {
        // At 40° the corner of the frame is outside the photo, so a crop sitting there has to move.
        let r = constrain_crop(FRAME, 40.0, [0.4, 0.4, 0.6, 0.6], [0.0, 0.0, 0.1, 0.1], Constrain::Shrink);
        assert!(fits(FRAME, 40.0, r), "{r:?}");
        assert!(r[2] > r[0] && r[3] > r[1]);
    }

    #[test]
    fn slide_stops_at_the_edge_and_keeps_its_size() {
        let base = [0.2, 0.2, 0.6, 0.6];
        let r = constrain_crop(FRAME, 0.0, base, [0.9, 0.2, 1.3, 0.6], Constrain::Slide);
        near(r, [0.6, 0.2, 1.0, 0.6]);
        // It slides along the top edge while pushing past the corner.
        let r = constrain_crop(FRAME, 0.0, base, [0.9, -0.3, 1.3, 0.1], Constrain::Slide);
        near(r, [0.6, 0.0, 1.0, 0.4]);
        // Inside the photo it goes where it is asked.
        near(constrain_crop(FRAME, 0.0, base, [0.3, 0.3, 0.7, 0.7], Constrain::Slide), [0.3, 0.3, 0.7, 0.7]);
    }

    #[test]
    fn slide_follows_a_turned_edge() {
        let base = constrain_crop(FRAME, 10.0, FULL, FULL, Constrain::Shrink);
        let size = (base[2] - base[0], base[3] - base[1]);
        let r = constrain_crop(FRAME, 10.0, base, [base[0] + 0.5, base[1] + 0.5, base[2] + 0.5, base[3] + 0.5], Constrain::Slide);
        assert!(fits_within(FRAME, 10.0, r, 0.05));
        assert!(((r[2] - r[0]) - size.0).abs() < 1e-9 && ((r[3] - r[1]) - size.1).abs() < 1e-9);
        // The crop was already tight at the start, so the slide cannot go far.
        assert!(r[0] - base[0] < 0.2, "{r:?}");
    }

    #[test]
    fn a_box_reaches_into_the_corners_that_turning_pushes_past_the_frame() {
        // Turned, the photo's corners reach outside the frame; a small box can follow them there.
        let (ex, ey) = reach(FRAME, 10.0);
        assert!(ex > 0.5 && ey > 0.5);
        let small = [0.45, 0.45, 0.55, 0.55];
        let r = constrain_crop(FRAME, 10.0, small, [-0.55, -0.55, -0.45, -0.45], Constrain::Slide);
        assert!(fits(FRAME, 10.0, r), "{r:?}");
        assert!(r[0] < 0.0 || r[1] < 0.0, "stopped at the frame: {r:?}");
        let r = constrain_crop(FRAME, 10.0, small, [0.45, 0.45, 2.0, 0.55], Constrain::Edges);
        assert!(fits_within(FRAME, 10.0, r, 0.05) && r[2] > 1.0, "the right edge stopped at the frame: {r:?}");
    }

    #[test]
    fn slide_runs_along_a_turned_edge_to_the_corner() {
        // A smaller box on a turned photo, held as f32 between steps as the app holds it. Pushed
        // right it stops against the turned edge; pushed on down and right from there it must keep
        // going along that edge (not stall because neither a pure x nor a pure y move fits), all
        // the way to where it can go no further.
        for angle in [8.0, -8.0, 4.2, -15.0] {
            let max = constrain_crop(FRAME, angle, FULL, FULL, Constrain::Shrink);
            let (hw, hh) = ((max[2] - max[0]) / 4.0, (max[3] - max[1]) / 4.0);
            let start = [0.5 - hw, 0.5 - hh, 0.5 + hw, 0.5 + hh];
            let f32s = |r: Rect| r.map(|v| v as f32 as f64);
            let by = |r: Rect, dx: f64, dy: f64| [r[0] + dx, r[1] + dy, r[2] + dx, r[3] + dy];
            let right = f32s(constrain_crop(FRAME, angle, start, by(start, 1.0, 0.0), Constrain::Slide));
            assert!(right[0] - start[0] > 0.1, "{angle}: barely moved right: {right:?}");
            // Step by step, as a drag does, toward the bottom-right corner.
            let mut r = right;
            for _ in 0..200 {
                let next = f32s(constrain_crop(FRAME, angle, r, by(r, 0.01, 0.01), Constrain::Slide));
                assert!(fits_within(FRAME, angle, next, 0.05), "{angle}: {next:?}");
                // Each step keeps the size it was given (rounding to f32 between steps aside).
                assert!(((next[2] - next[0]) - (r[2] - r[0])).abs() < 1e-6 && ((next[3] - next[1]) - (r[3] - r[1])).abs() < 1e-6, "{angle}: a step changed the size");
                r = next;
            }
            // In one go it lands at the same place: the nearest that fits, as far as it goes.
            let once = constrain_crop(FRAME, angle, right, by(right, 2.0, 2.0), Constrain::Slide);
            assert!((r[0] - once[0]).abs() < 1e-3 && (r[1] - once[1]).abs() < 1e-3, "{angle}: steps {r:?}, once {once:?}");
            // And it touches the photo there: a pixel further either way doesn't fit.
            let px = (1.0 / FRAME.0, 1.0 / FRAME.1);
            assert!(!fits_within(FRAME, angle, by(r, px.0, 0.0), 0.05) || !fits_within(FRAME, angle, by(r, 0.0, px.1), 0.05), "{angle}: {r:?} short of the corner");
        }
    }

    #[test]
    fn edges_stop_at_the_photo() {
        let base = [0.2, 0.2, 0.6, 0.6];
        // Dragging the right edge out past the frame.
        near(constrain_crop(FRAME, 0.0, base, [0.2, 0.2, 1.4, 0.6], Constrain::Edges), [0.2, 0.2, 1.0, 0.6]);
        // A corner past the corner: each edge stops on its own.
        near(constrain_crop(FRAME, 0.0, base, [-0.5, -0.5, 0.6, 0.6], Constrain::Edges), [0.0, 0.0, 0.6, 0.6]);
        // An edge dragged past the opposite one leaves a sliver, not an inverted box.
        let r = constrain_crop(FRAME, 0.0, base, [0.9, 0.2, 0.6, 0.6], Constrain::Edges);
        assert!(r[0] < r[2] && r[2] - r[0] >= MIN_SIZE - 1e-12, "{r:?}");
        // With the photo turned, the edge stops where the corner meets the photo's edge.
        let r = constrain_crop(FRAME, 10.0, [0.3, 0.1, 0.5, 0.9], [0.3, 0.1, 1.0, 0.9], Constrain::Edges);
        assert!(fits_within(FRAME, 10.0, r, 0.05) && r[2] < 1.0 && r[2] > 0.5, "{r:?}");
    }

    #[test]
    fn scaling_about_an_anchor_keeps_the_anchor_and_the_shape() {
        let anchor = [0.2, 0.2];
        // A corner drag far outside.
        let r = constrain_crop(FRAME, 0.0, [0.2, 0.2, 0.5, 0.4], [0.2, 0.2, 1.5, 0.8], Constrain::Scale(anchor));
        assert!(fits_within(FRAME, 0.0, r, 0.05));
        assert!((r[0] - 0.2).abs() < 1e-9 && (r[1] - 0.2).abs() < 1e-9);
        // At the photo's edge, give or take the few hundredths of a pixel a fit is judged to.
        assert!((r[2] - 1.0).abs() < 1e-5 || (r[3] - 1.0).abs() < 1e-5, "{r:?}");
    }

    #[test]
    fn aspect_picks_the_largest_box_of_that_shape() {
        let r = constrain_crop(FRAME, 0.0, FULL, FULL, Constrain::Aspect(1.0));
        near(r, [1.0 / 6.0, 0.0, 5.0 / 6.0, 1.0]);
        let r = constrain_crop(FRAME, 0.0, FULL, FULL, Constrain::Aspect(16.0 / 9.0));
        let aspect = (r[2] - r[0]) * FRAME.0 / ((r[3] - r[1]) * FRAME.1);
        assert!((aspect - 16.0 / 9.0).abs() < 1e-6 && fits(FRAME, 0.0, r));
        // A turned photo shrinks it too.
        let r = constrain_crop(FRAME, 5.0, FULL, FULL, Constrain::Aspect(1.0));
        assert!(fits(FRAME, 5.0, r));
    }

    #[test]
    fn clean_repairs_damage() {
        let mut e = Edit { rotation: 6, angle: 90.0, crop: [0.5, 0.0, 0.2, 1.0], ..Default::default() };
        clean(&mut e);
        assert_eq!((e.rotation, e.angle, e.crop), (2, 45.0, FULL_CROP));
        let mut e = Edit { angle: f32::NAN, crop: [0.1, 0.1, 0.9, 0.8], ..Default::default() };
        clean(&mut e);
        assert_eq!((e.angle, e.crop), (0.0, [0.1, 0.1, 0.9, 0.8]));
    }

    /// Output pixel → texture, by the book, for checking the affine map.
    fn texture_at(m: &ViewMap, x: f32, y: f32) -> [f32; 2] {
        [m.origin[0] + x * m.step_x[0] + y * m.step_y[0], m.origin[1] + x * m.step_x[1] + y * m.step_y[1]]
    }

    #[test]
    fn plain_fit_covers_the_texture() {
        let m = view_map((400, 200), 0, (400, 200), 0.0, FULL_CROP, Zoom::Fit, (800, 400));
        assert_eq!(texture_at(&m, 0.0, 0.0), [0.0, 0.0]);
        assert_eq!(texture_at(&m, 800.0, 400.0), [1.0, 1.0]);
        assert_eq!(m.clip, [0.0, 0.0, 800.0, 400.0]);
        // Wider view: the picture is centred with empty canvas either side.
        let m = view_map((400, 200), 0, (400, 200), 0.0, FULL_CROP, Zoom::Fit, (1000, 400));
        assert_eq!(m.clip, [100.0, 0.0, 900.0, 400.0]);
    }

    #[test]
    fn camera_orientation_and_quarter_turns_compose() {
        // A 400×200 texture shown turned 90° clockwise is a 200×400 frame; its top-left pixel is
        // the texture's bottom-left.
        let m = view_map((400, 200), 1, (200, 400), 0.0, FULL_CROP, Zoom::Fit, (200, 400));
        assert_eq!(texture_at(&m, 0.0, 0.0), [0.0, 1.0]);
        // The camera's 90° and one more turn of ours is a half turn.
        assert_eq!(turns(Orientation::Rotate90, 1), 2);
        assert_eq!(turns(Orientation::Rotate270, 1), 0);
        let m = view_map((400, 200), 2, (400, 200), 0.0, FULL_CROP, Zoom::Fit, (400, 200));
        assert_eq!(texture_at(&m, 0.0, 0.0), [1.0, 1.0]);
        assert_eq!(frame_size((6000, 4000), 1), (4000, 6000));
        assert_eq!(frame_size((6000, 4000), 2), (6000, 4000));
    }

    #[test]
    fn crop_is_what_fit_fits() {
        // The right half of a 400×200 photo fills a 200×200 view.
        let crop = [0.5, 0.0, 1.0, 1.0];
        let m = view_map((400, 200), 0, (400, 200), 0.0, crop, Zoom::Fit, (200, 200));
        assert_eq!(texture_at(&m, 0.0, 0.0), [0.5, 0.0]);
        assert_eq!(texture_at(&m, 200.0, 200.0), [1.0, 1.0]);
        assert_eq!(m.clip, [0.0, 0.0, 200.0, 200.0]);
        // In crop mode the whole frame is shown instead.
        let m = view_map((400, 200), 0, (400, 200), 0.0, crop, Zoom::Frame { x: 0.5, y: 0.5 }, (400, 200));
        assert_eq!(texture_at(&m, m.clip[0], m.clip[1]), [0.0, 0.0]);
        // With a margin all round: the frame's box is inside the view, centred.
        assert!(m.clip[0] > 0.0 && m.clip[1] > 0.0 && 400.0 - m.clip[2] - m.clip[0] < 1e-3 && 200.0 - m.clip[3] - m.clip[1] < 1e-3);
        // 100% centred on the crop's middle (0.5, 0.5 of the crop) is the frame's (0.75, 0.5).
        let m = view_map((400, 200), 0, (400, 200), 0.0, crop, Zoom::Actual { x: 0.5, y: 0.5 }, (100, 100));
        assert_eq!(texture_at(&m, 50.0, 50.0), [0.75, 0.5]);
    }

    #[test]
    fn straightening_turns_the_photo_clockwise_about_the_centre() {
        // At +90° (not allowed by the slider, but the maths holds) the picture's top edge ends
        // up on the right: the frame's right-middle shows the photo's top-middle.
        let m = view_map((200, 200), 0, (200, 200), 90.0, FULL_CROP, Zoom::Frame { x: 0.5, y: 0.5 }, (200, 200));
        let [u, v] = texture_at(&m, m.clip[2], 100.0);
        assert!((u - 0.5).abs() < 1e-5 && v.abs() < 1e-5, "{u} {v}");
        // The centre stays put at any angle.
        for angle in [-17.0, 4.0, 33.0] {
            let m = view_map((300, 200), 0, (300, 200), angle, FULL_CROP, Zoom::Frame { x: 0.5, y: 0.5 }, (300, 200));
            let [u, v] = texture_at(&m, 150.0, 100.0);
            assert!((u - 0.5).abs() < 1e-5 && (v - 0.5).abs() < 1e-5);
        }
    }

    #[test]
    fn region_is_in_frame_pixels_and_never_clipped() {
        let m = view_map((400, 200), 0, (400, 200), 0.0, [0.5, 0.5, 1.0, 1.0], Zoom::Region { x: 100.0, y: 50.0, scale: 1.0 }, (64, 64));
        assert_eq!(texture_at(&m, 0.0, 0.0), [0.25, 0.25]);
        assert!(m.clip[0] < -1e8 && m.clip[2] > 1e8);
    }
}
