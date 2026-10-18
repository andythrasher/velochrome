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
    let ok = edit.crop.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) && r - l >= MIN_SIZE as f32 && b - t >= MIN_SIZE as f32;
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
}

/// The affine map from output pixels to texture coordinates, covering zoom, pan, the camera's
/// orientation, the quarter turns, straightening and the crop. `texture` is the texture's size,
/// `turns` from [`turns`], and `frame` the frame's size. `Zoom` is relative to the crop (to the
/// whole frame for `Zoom::Frame`), except `Region`, which is in frame pixels.
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
    let [wx, wy, ww, wh] = match zoom {
        Zoom::Frame => [0.0, 0.0, fw, fh],
        _ => window(frame, crop),
    };
    let (scale, center) = match zoom {
        Zoom::Fit => ((out_w / ww).min(out_h / wh), (wx + ww / 2.0, wy + wh / 2.0)),
        Zoom::Frame => {
            let room = 1.0 - 2.0 * FRAME_MARGIN;
            ((out_w * room / ww).min(out_h * room / wh), (wx + ww / 2.0, wy + wh / 2.0))
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
    let clip = match zoom {
        Zoom::Region { .. } => [-1e9, -1e9, 1e9, 1e9],
        _ => {
            let at = |p: f64, c: f64, half: f64| (half + (p - c) * scale) as f32;
            [at(wx, center.0, out_w / 2.0), at(wy, center.1, out_h / 2.0), at(wx + ww, center.0, out_w / 2.0), at(wy + wh, center.1, out_h / 2.0)]
        }
    };
    let f = |v: [f64; 2]| [v[0] as f32, v[1] as f32];
    ViewMap {
        origin: f(o),
        step_x: f([x[0] - o[0], x[1] - o[1]]),
        step_y: f([y[0] - o[0], y[1] - o[1]]),
        lod: lod as f32,
        clip,
    }
}

// ---- Keeping the crop inside the photo ----

/// Whether the rectangle lies inside the frame and inside the photo turned by `angle` degrees.
pub fn fits(frame: (f64, f64), angle: f64, r: Rect) -> bool {
    let (w, h) = frame;
    let slack = 1e-6;
    if r[0] < -slack || r[1] < -slack || r[2] > 1.0 + slack || r[3] > 1.0 + slack {
        return false;
    }
    let (sin, cos) = angle.to_radians().sin_cos();
    let tolerance = 1e-4; // pixels
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
    let ok = |r: Rect| fits(frame, angle, r);
    let base = if ok(base) { base } else { [0.5 - MIN_SIZE, 0.5 - MIN_SIZE, 0.5 + MIN_SIZE, 0.5 + MIN_SIZE] };
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
            let (sx, sy) = (desired[0] - base[0], desired[1] - base[1]);
            let moved = |dx: f64, dy: f64| [base[0] + dx, base[1] + dy, base[2] + dx, base[3] + dy];
            let dx = furthest(0.0, sx, &|t| ok(moved(t, 0.0)));
            let dy = furthest(0.0, sy, &|t| ok(moved(dx, t)));
            moved(dx, dy)
        }
        Constrain::Edges => {
            let mut r = base;
            for i in 0..4 {
                if desired[i] == base[i] {
                    continue;
                }
                // Not past the opposite edge, and not outside the frame.
                let want = match i {
                    0 => desired[0].min(r[2] - MIN_SIZE),
                    1 => desired[1].min(r[3] - MIN_SIZE),
                    2 => desired[2].max(r[0] + MIN_SIZE),
                    _ => desired[3].max(r[1] + MIN_SIZE),
                }
                .clamp(0.0, 1.0);
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

/// `desired` made smaller about its centre until it fits. If the centre itself is outside the
/// photo, the centre moves toward the frame's first.
fn shrink(frame: (f64, f64), angle: f64, desired: Rect) -> Rect {
    let ok = |r: Rect| fits(frame, angle, r);
    let (hw, hh) = ((desired[2] - desired[0]) / 2.0, (desired[3] - desired[1]) / 2.0);
    let (mut cx, mut cy) = ((desired[0] + desired[2]) / 2.0, (desired[1] + desired[3]) / 2.0);
    let around = |cx: f64, cy: f64, s: f64| [cx - s * hw, cy - s * hh, cx + s * hw, cy + s * hh];
    if !ok(around(cx, cy, 0.0)) {
        let (mut good, mut bad) = (0.0, 1.0);
        let (x0, y0) = (cx, cy);
        for _ in 0..40 {
            let t = (good + bad) / 2.0;
            let (x, y) = (x0 + t * (0.5 - x0), y0 + t * (0.5 - y0));
            if ok(around(x, y, 0.0)) { bad = t } else { good = t }
        }
        // `bad` is the smallest move that lands inside.
        cx = x0 + bad * (0.5 - x0);
        cy = y0 + bad * (0.5 - y0);
    }
    if ok(around(cx, cy, 1.0)) {
        return around(cx, cy, 1.0);
    }
    let (mut good, mut bad) = (0.0, 1.0);
    for _ in 0..40 {
        let s = (good + bad) / 2.0;
        if ok(around(cx, cy, s)) { good = s } else { bad = s }
    }
    around(cx, cy, good)
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
        assert!(fits(FRAME, 10.0, r));
        assert!(((r[2] - r[0]) - size.0).abs() < 1e-9 && ((r[3] - r[1]) - size.1).abs() < 1e-9);
        // The crop was already tight at the start, so the slide cannot go far.
        assert!(r[0] - base[0] < 0.2, "{r:?}");
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
        assert!(fits(FRAME, 10.0, r) && r[2] < 1.0 && r[2] > 0.5, "{r:?}");
    }

    #[test]
    fn scaling_about_an_anchor_keeps_the_anchor_and_the_shape() {
        let anchor = [0.2, 0.2];
        // A corner drag far outside.
        let r = constrain_crop(FRAME, 0.0, [0.2, 0.2, 0.5, 0.4], [0.2, 0.2, 1.5, 0.8], Constrain::Scale(anchor));
        assert!(fits(FRAME, 0.0, r));
        assert!((r[0] - 0.2).abs() < 1e-9 && (r[1] - 0.2).abs() < 1e-9);
        assert!((r[2] - 1.0).abs() < 1e-6 || (r[3] - 1.0).abs() < 1e-6, "{r:?}");
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
        let m = view_map((400, 200), 0, (400, 200), 0.0, crop, Zoom::Frame, (400, 200));
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
        let m = view_map((200, 200), 0, (200, 200), 90.0, FULL_CROP, Zoom::Frame, (200, 200));
        let [u, v] = texture_at(&m, m.clip[2], 100.0);
        assert!((u - 0.5).abs() < 1e-5 && v.abs() < 1e-5, "{u} {v}");
        // The centre stays put at any angle.
        for angle in [-17.0, 4.0, 33.0] {
            let m = view_map((300, 200), 0, (300, 200), angle, FULL_CROP, Zoom::Frame, (300, 200));
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
