//! The white balance picker (docs/phase2a.md): the user clicks a spot that should be neutral,
//! and this finds the temperature and tint that make it so.
//!
//! The click is a point on the view. It goes back to the photo the way the view got there:
//! through `geometry::view_map` (zoom, pan, crop, straighten, rotation) and then the lens warp
//! (`LensCorrection::source`), which lands on a sensor position. The colour is averaged from the
//! raw mosaic around it, on the CPU: each of the three colours' own samples in a small window,
//! as scene-linear camera values before any white balance. That needs no demosaic and no GPU
//! readback, and clipped samples are simply left out.

use crate::color::Mat3;
use crate::decode::RawFrame;
use crate::develop::{self, Edit};
use crate::geometry;
use crate::lens::Channel;
use crate::pipeline::Zoom;

/// Why a click gave no white balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickError {
    /// The click is off the photo (on the canvas around it).
    Outside,
    /// The spot is clipped (or too bright to trust).
    Clipped,
    /// The spot is black, or too dark to read a colour from.
    Dark,
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PickError::Outside => "Click on the photo to pick a white balance.",
            PickError::Clipped => "That spot is clipped. Pick something that isn't pure white.",
            PickError::Dark => "That spot is too dark to read a colour. Pick a brighter neutral.",
        })
    }
}

/// A pixel at or above this (0..1 between black and white level) is clipped.
const CLIPPED: f32 = 0.98;
/// A colour whose mean is below this is too dark to judge from.
const DARK: f32 = 0.005;
/// The window is about this many photo pixels across at 100%, and at least this many screen
/// pixels across when zoomed out; never over `MAX_SIDE` photo pixels.
const MIN_SIDE: f64 = 5.0;
const SCREEN_SIDE: f64 = 3.0;
const MAX_SIDE: f64 = 63.0;

/// A point of the view found on the photo.
#[derive(Debug, Clone, Copy)]
pub struct Spot {
    /// In the lens-corrected frame, as fractions (0..1) of the sensor image's width and height.
    pub uv: [f64; 2],
    /// On the sensor, in pixels.
    pub sensor: [f64; 2],
    /// Sensor pixels per view pixel.
    pub per_pixel: f64,
}

/// Where a point on the view (physical pixels of an `out`-sized view) lands on the photo: its
/// place in the corrected frame (texture coordinates, 0..1), its sensor position in pixels, and how
/// many sensor pixels one view pixel covers there. None if
/// the point is not on the photo.
pub fn sensor_point(frame: &RawFrame, edit: &Edit, zoom: Zoom, out: (u32, u32), view: (f32, f32)) -> Option<Spot> {
    let photo = if frame.orientation.swaps_axes() { (frame.height, frame.width) } else { (frame.width, frame.height) };
    let frame_size = geometry::frame_size(photo, edit.rotation);
    let turns = geometry::turns(frame.orientation, edit.rotation);
    let map = geometry::view_map((frame.width, frame.height), turns, frame_size, edit.angle, edit.crop, zoom, out);
    let (x, y) = view;
    if x < map.clip[0] || x > map.clip[2] || y < map.clip[1] || y > map.clip[3] {
        return None;
    }
    let uv = [0, 1].map(|i| (map.origin[i] + x * map.step_x[i] + y * map.step_y[i]) as f64);
    if uv.iter().any(|v| !(0.0..=1.0).contains(v)) {
        return None;
    }
    let (w, h) = (frame.width as f64, frame.height as f64);
    let per_pixel = (map.step_x[0] as f64 * w).hypot(map.step_x[1] as f64 * h);
    // The corrected frame's position, then where the sensor has it (as green; the window is wide
    // enough to swallow the colours' small offsets).
    let lens = frame.lens.and_then(|l| l.with_switches(edit.no_distortion == 0, edit.no_chromatic_aberration == 0));
    let at = match lens {
        Some(l) => {
            let s = l.source(Channel::Green, [((uv[0] - 0.5) * w) as f32, ((uv[1] - 0.5) * h) as f32], w as f32, h as f32);
            [w / 2.0 + s[0] as f64, h / 2.0 + s[1] as f64]
        }
        None => [uv[0] * w, uv[1] * h],
    };
    Some(Spot { uv, sensor: at, per_pixel })
}

/// The mean scene-linear camera RGB (black subtracted, 0..1 at the white level, no white
/// balance) in a `side` × `side` window centred on `at`, leaving out clipped samples.
pub fn average(frame: &RawFrame, at: [f64; 2], side: usize) -> Result<[f32; 3], PickError> {
    let (w, h) = (frame.width as i64, frame.height as i64);
    let half = side as i64 / 2;
    let (x0, y0) = (at[0].floor() as i64 - half, at[1].floor() as i64 - half);
    let (mut sum, mut count) = ([0.0f64; 3], [0u32; 3]);
    let (mut clipped, mut total) = (0u32, 0u32);
    for y in y0.max(0)..(y0 + side as i64).min(h) {
        for x in x0.max(0)..(x0 + side as i64).min(w) {
            let (row, col) = ((y & 1) as usize, (x & 1) as usize);
            let (black, color) = (frame.black[row][col], frame.cfa[row][col] as usize);
            let v = (frame.data[(y * w + x) as usize] as f32 - black).max(0.0) / (frame.white - black);
            total += 1;
            if v >= CLIPPED {
                clipped += 1;
            } else {
                sum[color] += v as f64;
                count[color] += 1;
            }
        }
    }
    // More than a tenth clipped, or a colour with nothing left: the spot isn't trustworthy.
    if total == 0 || clipped * 10 > total || count.contains(&0) {
        return Err(if total == 0 { PickError::Outside } else { PickError::Clipped });
    }
    let mean = [0, 1, 2].map(|c| (sum[c] / count[c] as f64) as f32);
    if mean.iter().any(|&m| m < DARK) {
        return Err(PickError::Dark);
    }
    Ok(mean)
}

/// The temperature and tint (within the sliders' ranges) that make camera RGB `cam` neutral.
pub fn neutral_for(cam: [f32; 3], xyz_to_cam: &Mat3) -> (f32, f32) {
    let wb = [cam[1] / cam[0], 1.0, cam[1] / cam[2]];
    let (kelvin, tint) = develop::multipliers_to_temperature(xyz_to_cam, wb);
    (kelvin.clamp(2000.0, 50000.0), tint.clamp(-150.0, 150.0))
}

/// The window's side in sensor pixels for a view that covers `per_pixel` of them per pixel.
pub fn window_side(per_pixel: f64) -> usize {
    (MIN_SIDE.max(SCREEN_SIDE * per_pixel).min(MAX_SIDE)).round() as usize
}

/// Picks a white balance from a click on the view: temperature (K) and tint.
pub fn pick(frame: &RawFrame, edit: &Edit, zoom: Zoom, out: (u32, u32), view: (f32, f32)) -> Result<(f32, f32), PickError> {
    let spot = sensor_point(frame, edit, zoom, out, view).ok_or(PickError::Outside)?;
    let cam = average(frame, spot.sensor, window_side(spot.per_pixel))?;
    Ok(neutral_for(cam, &frame.xyz_to_cam))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Orientation;
    use crate::lens::{LensCorrection, LensModel};

    const OM5_XYZ_TO_CAM: Mat3 = [[1.2008, -0.523, -0.1123], [-0.2265, 1.0496, 0.2046], [0.0069, 0.0734, 0.4938]];
    const AS_SHOT: [f32; 3] = [2.0, 1.0, 1.6];

    /// An RGGB mosaic of `w` × `h` where `paint(x, y)` gives the camera RGB (0..1, as shot: not
    /// yet white balanced) of each pixel.
    fn frame(w: u32, h: u32, paint: impl Fn(u32, u32) -> [f32; 3]) -> RawFrame {
        let cfa = [[0u8, 1], [1, 2]];
        let mut data = vec![0u16; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let c = paint(x, y)[cfa[(y & 1) as usize][(x & 1) as usize] as usize];
                data[(y * w + x) as usize] = (c * 4000.0).round() as u16 + 95;
            }
        }
        RawFrame {
            make: String::new(),
            model: String::new(),
            data,
            width: w,
            height: h,
            cfa,
            black: [[95.0; 2]; 2],
            white: 4095.0,
            wb: AS_SHOT,
            cam_to_working: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            xyz_to_cam: OM5_XYZ_TO_CAM,
            orientation: Orientation::Normal,
            lens: None,
            decode_time: Default::default(),
        }
    }

    /// A surface the camera's own white balance makes neutral: red and blue read lower by the multipliers.
    fn grey(level: f32) -> [f32; 3] {
        [level / AS_SHOT[0], level, level / AS_SHOT[2]]
    }

    fn as_shot() -> (f32, f32) {
        develop::multipliers_to_temperature(&OM5_XYZ_TO_CAM, AS_SHOT)
    }

    fn near(a: (f32, f32), b: (f32, f32)) {
        assert!((a.0 - b.0).abs() / b.0 < 0.01 && (a.1 - b.1).abs() < 1.0, "{a:?} vs {b:?}");
    }

    #[test]
    fn a_grey_patch_gives_the_as_shot_white_balance_back() {
        let f = frame(256, 192, |_, _| grey(0.3));
        let edit = Edit::default();
        for view in [(10.0, 10.0), (128.0, 96.0), (250.0, 180.0)] {
            near(pick(&f, &edit, Zoom::Fit, (256, 192), view).unwrap(), as_shot());
        }
        // Whatever the zoom, the crop or the turn of the view.
        near(pick(&f, &edit, Zoom::Actual { x: 0.3, y: 0.6 }, (100, 80), (50.0, 40.0)).unwrap(), as_shot());
        let turned = Edit { rotation: 1, angle: 7.0, crop: [0.2, 0.2, 0.8, 0.8], ..Default::default() };
        near(pick(&f, &turned, Zoom::Fit, (200, 300), (100.0, 150.0)).unwrap(), as_shot());
    }

    #[test]
    fn a_coloured_patch_is_picked_in_its_own_place() {
        // Neutral everywhere but a warm square in the left half; picking in it gives a different
        // (cooler) correction, and picking beside it gives as shot.
        let f = frame(256, 192, |x, y| if (40..80).contains(&x) && (40..80).contains(&y) { [0.35, 0.3, 0.1] } else { grey(0.3) });
        let edit = Edit::default();
        let warm = pick(&f, &edit, Zoom::Fit, (256, 192), (60.0, 60.0)).unwrap();
        assert!(warm.0 < as_shot().0 - 500.0, "{warm:?}");
        near(pick(&f, &edit, Zoom::Fit, (256, 192), (150.0, 60.0)).unwrap(), as_shot());
    }

    #[test]
    fn clicks_map_through_rotation_crop_and_zoom() {
        let f = frame(400, 200, |_, _| grey(0.3));
        let close = |p: [f64; 2], q: [f64; 2]| assert!((p[0] - q[0]).abs() < 0.01 && (p[1] - q[1]).abs() < 0.01, "{p:?} vs {q:?}");
        // Plain fit, twice as large as the sensor: the view point halves.
        let s = sensor_point(&f, &Edit::default(), Zoom::Fit, (800, 400), (200.0, 100.0)).unwrap();
        let (at, per) = (s.sensor, s.per_pixel);
        close(at, [100.0, 50.0]);
        assert!((per - 0.5).abs() < 1e-6);
        // A quarter turn clockwise: the view's top-left is the sensor's bottom-left.
        let turned = Edit { rotation: 1, ..Default::default() };
        let at = sensor_point(&f, &turned, Zoom::Fit, (200, 400), (0.0, 0.0)).unwrap().sensor;
        close(at, [0.0, 200.0]);
        // A crop of the right half: the view's top-left is the sensor's top-middle.
        let cropped = Edit { crop: [0.5, 0.0, 1.0, 1.0], ..Default::default() };
        let at = sensor_point(&f, &cropped, Zoom::Fit, (200, 200), (0.0, 0.0)).unwrap().sensor;
        close(at, [200.0, 0.0]);
        // 100%, centred on the crop's middle, which is the frame's (0.75, 0.5) = (300, 100).
        let s = sensor_point(&f, &cropped, Zoom::Actual { x: 0.5, y: 0.5 }, (100, 100), (50.0, 50.0)).unwrap();
        let (at, per) = (s.sensor, s.per_pixel);
        close(at, [300.0, 100.0]);
        assert!((per - 1.0).abs() < 1e-6);
        // Off the photo (the canvas around a fitted crop), there is nothing to pick.
        assert!(sensor_point(&f, &Edit::default(), Zoom::Fit, (800, 800), (400.0, 10.0)).is_none());
    }

    #[test]
    fn the_lens_warp_is_followed_and_its_switch_respected() {
        let mut f = frame(400, 200, |_, _| grey(0.3));
        f.lens = Some(LensCorrection {
            model: LensModel::Olympus,
            scale: 0.97,
            k: [0.05, 0.0, 0.0],
            ca_red: [0.0; 3],
            ca_blue: [0.0; 3],
            zoom: 1.0,
        });
        let corner = (790.0, 395.0);
        let with = sensor_point(&f, &Edit::default(), Zoom::Fit, (800, 400), corner).unwrap().sensor;
        let without = sensor_point(&f, &Edit { no_distortion: 1, ..Default::default() }, Zoom::Fit, (800, 400), corner).unwrap().sensor;
        assert!((without[0] - 395.0).abs() < 0.01, "{without:?}");
        assert!((with[0] - without[0]).abs() > 1.0, "{with:?} vs {without:?}");
        // The centre doesn't move.
        let c = sensor_point(&f, &Edit::default(), Zoom::Fit, (800, 400), (400.0, 200.0)).unwrap().sensor;
        assert!((c[0] - 200.0).abs() < 0.01 && (c[1] - 100.0).abs() < 0.01);
    }

    #[test]
    fn clipped_and_black_spots_are_refused() {
        let edit = Edit::default();
        let bright = frame(128, 128, |_, _| [1.0, 1.0, 1.0]);
        assert_eq!(pick(&bright, &edit, Zoom::Fit, (128, 128), (64.0, 64.0)), Err(PickError::Clipped));
        let black = frame(128, 128, |_, _| [0.0; 3]);
        assert_eq!(pick(&black, &edit, Zoom::Fit, (128, 128), (64.0, 64.0)), Err(PickError::Dark));
        // A spot with a little clipping in it still reads, from the rest.
        let speck = frame(128, 128, |x, y| if x == 64 && y == 64 { [1.0; 3] } else { grey(0.3) });
        near(pick(&speck, &edit, Zoom::Fit, (128, 128), (64.0, 64.0)).unwrap(), as_shot());
        assert_eq!(pick(&bright, &edit, Zoom::Fit, (128, 128), (500.0, 64.0)), Err(PickError::Outside));
    }

    #[test]
    fn window_grows_when_zoomed_out() {
        assert_eq!(window_side(1.0), 5); // 100%
        assert_eq!(window_side(0.5), 5);
        assert_eq!(window_side(4.0), 12); // Fit on a 6000-wide photo in 1500 pixels
        assert_eq!(window_side(100.0), 63);
    }
}
