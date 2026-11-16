// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Frames in between: the pictures a camera would have caught between two
//! of a file's own frames, for a slow motion that glides instead of
//! stuttering.
//!
//! Slowed to a quarter, a film's 24 frames a second become six, each shown
//! four times over. Made smooth, the three missing ones are worked out
//! from the two either side, as Twixtor and the phone apps' "smooth slow
//! motion" do, and the clip plays a picture per frame again.
//!
//! The working-out is RIFE (Practical-RIFE v4.6, MIT), exported to answer
//! with its motion rather than its picture: for the moment `t` between two
//! frames, where each pixel of that moment was in the frame before and in
//! the frame after, and how much of each to take. The network reads both
//! frames at [`flow_size`] - at most [`FLOW_SIDE`] on the long side, which
//! is where a phone's CPU spends its time - and the pictures are then
//! moved and mixed here at their own size, so a 4K file keeps its 4K
//! detail while its motion is found at a fraction of the cost. That is
//! what RIFE's own `scale` does inside the network; here it is outside, so
//! one exported model serves every size.
//!
//! [`blend`] is the other way to fill a gap: the two frames faded into
//! each other, with no model and no wait - ghosted where things move, as
//! the frame blending of the old editors was.
//!
//! Everything but [`Tweener`] is a pure function, tested without a model;
//! only the network needs the `infer` feature that brings the runtime.

use concat_core::Frame;
use concat_core::frame::BYTES_PER_PIXEL;

/// The longest side the motion is worked out at, in pixels: a 1080p frame
/// is read at 1024 by 576, a quarter of its pixels, which on a phone is the
/// difference between a second a frame and four. The pictures themselves
/// are moved at their own size.
pub const FLOW_SIDE: u32 = 1024;

/// What the network's sides must be a whole number of: its finest level
/// reads the frame an eighth the size, and that eighth halved twice more.
const MULTIPLE: u32 = 32;

/// The size the network reads a `width` by `height` frame at: the frame
/// itself, or shrunk until its long side is at most `side`, and each side
/// rounded to the network's multiple of 32. The shape is near the frame's
/// rather than exact; the motion is scaled back side by side.
pub fn flow_size(width: u32, height: u32, side: u32) -> (u32, u32) {
    let long = width.max(height).max(1) as f64;
    let shrink = (f64::from(side.max(MULTIPLE)) / long).min(1.0);
    let round = |length: u32| {
        let scaled = f64::from(length) * shrink / f64::from(MULTIPLE);
        (scaled.round().max(1.0) as u32) * MULTIPLE
    };
    (round(width), round(height))
}

/// The two frames faded into each other: `t` of the way from `a` to `b`.
/// Frames of two sizes give `a`.
pub fn blend(a: &Frame, b: &Frame, t: f32) -> Frame {
    if a.width() != b.width() || a.height() != b.height() || !eight_bit(a) || !eight_bit(b) {
        return a.clone();
    }
    let t = t.clamp(0.0, 1.0);
    let pixels = a
        .pixels()
        .iter()
        .zip(b.pixels())
        .map(|(&from, &to)| {
            (f32::from(from) + (f32::from(to) - f32::from(from)) * t + 0.5).clamp(0.0, 255.0) as u8
        })
        .collect();
    Frame::from_rgba(a.width(), a.height(), pixels).unwrap_or_else(|| a.clone())
}

/// Whether a frame is the eight-bit RGBA every picture here is read as.
fn eight_bit(frame: &Frame) -> bool {
    frame.pixels().len() == frame.width() as usize * frame.height() as usize * BYTES_PER_PIXEL
}

/// `frame`'s colour, without its alpha, at `width` by `height`, as the
/// network's planes: red, then green, then blue, each row-major, `0..=1`.
pub fn planes(frame: &Frame, width: u32, height: u32) -> Vec<f32> {
    let small = crate::enhance::area_resize(frame, width, height);
    let area = width as usize * height as usize;
    let mut out = vec![0f32; area * 3];
    for (index, pixel) in small.pixels().chunks_exact(BYTES_PER_PIXEL).enumerate() {
        for channel in 0..3 {
            out[channel * area + index] = f32::from(pixel[channel]) / 255.0;
        }
    }
    out
}

/// One field of the network's answer, `channels` planes of `width` by
/// `height`, read at a point of its own pixels: bilinear, between pixel
/// centres, held at the edges.
struct Field<'a> {
    values: &'a [f32],
    width: usize,
    height: usize,
}

impl Field<'_> {
    /// Plane `channel` at `(x, y)`, in the field's pixels.
    fn at(&self, channel: usize, x: f32, y: f32) -> f32 {
        let plane = &self.values[channel * self.width * self.height..];
        let x = x.clamp(0.0, (self.width - 1) as f32);
        let y = y.clamp(0.0, (self.height - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (ax, ay) = (x - x0 as f32, y - y0 as f32);
        let row0 = y0 * self.width;
        let row1 = y1 * self.width;
        let top = plane[row0 + x0] * (1.0 - ax) + plane[row0 + x1] * ax;
        let bottom = plane[row1 + x0] * (1.0 - ax) + plane[row1 + x1] * ax;
        top * (1.0 - ay) + bottom * ay
    }
}

/// `frame`'s colour at `(x, y)` in its own pixels, bilinear, the edge
/// pixels held past the edges, as RIFE's own warp samples.
fn sample(pixels: &[u8], width: usize, height: usize, x: f32, y: f32) -> [f32; 3] {
    let x = x.clamp(0.0, (width - 1) as f32);
    let y = y.clamp(0.0, (height - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
    let (ax, ay) = (x - x0 as f32, y - y0 as f32);
    let at =
        |px: usize, py: usize, c: usize| f32::from(pixels[(py * width + px) * BYTES_PER_PIXEL + c]);
    let mut out = [0f32; 3];
    for (c, value) in out.iter_mut().enumerate() {
        let top = at(x0, y0, c) * (1.0 - ax) + at(x1, y0, c) * ax;
        let bottom = at(x0, y1, c) * (1.0 - ax) + at(x1, y1, c) * ax;
        *value = top * (1.0 - ay) + bottom * ay;
    }
    out
}

/// The moment between `a` and `b` that the network's `flow` and `mask`
/// describe, at the frames' own size: every pixel taken from where the
/// motion says it was in `a` and in `b`, and the two mixed by the mask.
///
/// `flow` is four planes at `flow_width` by `flow_height`, in pixels of
/// that size: where each pixel of the moment was in `a` (x, then y), then
/// in `b`. `mask` is one plane, before its sigmoid: the share of `a`.
/// Both are read between their pixels and scaled to the frames, so the
/// network may have seen the frames smaller than they are.
pub fn compose(
    a: &Frame,
    b: &Frame,
    flow: &[f32],
    mask: &[f32],
    flow_width: u32,
    flow_height: u32,
) -> Result<Frame, String> {
    let (width, height) = (a.width() as usize, a.height() as usize);
    if a.width() != b.width() || a.height() != b.height() || width == 0 || height == 0 {
        return Err("the two frames are not one size".to_owned());
    }
    if !eight_bit(a) || !eight_bit(b) {
        return Err("frames in between are made from eight-bit pictures".to_owned());
    }
    let (fw, fh) = (flow_width as usize, flow_height as usize);
    if fw == 0 || fh == 0 || flow.len() < fw * fh * 4 || mask.len() < fw * fh {
        return Err("the motion is not the size it says".to_owned());
    }
    let motion = Field {
        values: flow,
        width: fw,
        height: fh,
    };
    let share = Field {
        values: mask,
        width: fw,
        height: fh,
    };
    // From a pixel of the frame to the field's pixels, centre to centre,
    // and the field's motion back to the frame's pixels.
    let (to_x, to_y) = (fw as f32 / width as f32, fh as f32 / height as f32);
    let (back_x, back_y) = (width as f32 / fw as f32, height as f32 / fh as f32);
    let (from, into) = (a.pixels(), b.pixels());
    let mut out = vec![255u8; width * height * BYTES_PER_PIXEL];
    let row_bytes = width * BYTES_PER_PIXEL;
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .clamp(1, 8);
    let rows_each = height.div_ceil(workers).max(1);
    std::thread::scope(|scope| {
        for (band, chunk) in out.chunks_mut(rows_each * row_bytes).enumerate() {
            let (motion, share) = (&motion, &share);
            scope.spawn(move || {
                for (offset, row) in chunk.chunks_mut(row_bytes).enumerate() {
                    let y = band * rows_each + offset;
                    let fy = (y as f32 + 0.5) * to_y - 0.5;
                    for x in 0..width {
                        let fx = (x as f32 + 0.5) * to_x - 0.5;
                        let (xf, yf) = (x as f32, y as f32);
                        let pa = sample(
                            from,
                            width,
                            height,
                            xf + motion.at(0, fx, fy) * back_x,
                            yf + motion.at(1, fx, fy) * back_y,
                        );
                        let pb = sample(
                            into,
                            width,
                            height,
                            xf + motion.at(2, fx, fy) * back_x,
                            yf + motion.at(3, fx, fy) * back_y,
                        );
                        let m = 1.0 / (1.0 + (-share.at(0, fx, fy)).exp());
                        let pixel = &mut row[x * BYTES_PER_PIXEL..(x + 1) * BYTES_PER_PIXEL];
                        for c in 0..3 {
                            pixel[c] =
                                (pa[c] * m + pb[c] * (1.0 - m) + 0.5).clamp(0.0, 255.0) as u8;
                        }
                    }
                }
            });
        }
    });
    Frame::from_rgba(a.width(), a.height(), out).ok_or_else(|| "a frame the wrong size".to_owned())
}

/// The network, loaded and ready: one pair of frames at a time, every core
/// on it.
#[cfg(feature = "infer")]
pub struct Tweener {
    model: std::sync::Mutex<crate::runtime::Model>,
    side: u32,
}

#[cfg(feature = "infer")]
impl Tweener {
    /// Loads the exported network from `file`, on the CPU: its warping
    /// is a grid sample, which the phones' accelerators do not run, and a
    /// network split between them and the CPU spent its time crossing.
    pub fn load(file: &std::path::Path) -> Result<Tweener, String> {
        let bytes = std::fs::read(file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        let cores = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        // A phone's fast cores, not its slow ones too: a run spread over
        // all eight waits on the slowest.
        let threads = if cfg!(any(target_os = "android", target_os = "ios")) {
            cores.clamp(1, 4)
        } else {
            cores.clamp(1, 8)
        };
        let model = crate::runtime::Model::from_bytes_on_cpu(&bytes, threads)?;
        Ok(Tweener {
            model: std::sync::Mutex::new(model),
            side: FLOW_SIDE,
        })
    }

    /// The same network reading the motion at most `side` pixels long:
    /// smaller is faster and coarser.
    pub fn with_side(mut self, side: u32) -> Tweener {
        self.side = side.max(MULTIPLE);
        self
    }

    /// The frames at each of `times` between `a` and `b`, in order, each
    /// `0..=1` of the way: the two frames read once, the network run once
    /// a moment.
    pub fn between(&self, a: &Frame, b: &Frame, times: &[f32]) -> Result<Vec<Frame>, String> {
        if a.width() != b.width() || a.height() != b.height() {
            return Err("the two frames are not one size".to_owned());
        }
        let (fw, fh) = flow_size(a.width(), a.height(), self.side);
        let (first, second) = (planes(a, fw, fh), planes(b, fw, fh));
        let dims = vec![1, 3, fh as usize, fw as usize];
        let mut model = self
            .model
            .lock()
            .map_err(|_| "the frame model is poisoned")?;
        let mut frames = Vec::with_capacity(times.len());
        for &t in times {
            let answer = model.run(
                vec![
                    crate::runtime::Input {
                        name: "img0",
                        dims: dims.clone(),
                        data: first.clone().into(),
                    },
                    crate::runtime::Input {
                        name: "img1",
                        dims: dims.clone(),
                        data: second.clone().into(),
                    },
                    crate::runtime::Input {
                        name: "timestep",
                        dims: vec![1, 1, 1, 1],
                        data: vec![t.clamp(0.0, 1.0)].into(),
                    },
                ],
                &["flow", "mask"],
            )?;
            let (flow, mask) = (&answer[0], &answer[1]);
            if flow.dims != [1, 4, fh as usize, fw as usize]
                || mask.dims != [1, 1, fh as usize, fw as usize]
            {
                return Err(format!(
                    "the frame model answered {:?} and {:?} for {fw} by {fh}",
                    flow.dims, mask.dims
                ));
            }
            frames.push(compose(a, b, &flow.data, &mask.data, fw, fh)?);
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame whose red rises across it and whose green rises down it.
    fn ramp(width: u32, height: u32) -> Frame {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[(x * 10) as u8, (y * 10) as u8, 50, 255]);
            }
        }
        Frame::from_rgba(width, height, pixels).expect("a frame")
    }

    fn solid(width: u32, height: u32, rgb: [u8; 3]) -> Frame {
        let pixels = (0..width * height)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect();
        Frame::from_rgba(width, height, pixels).expect("a frame")
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * frame.width() + x) * 4) as usize;
        frame.pixels()[at..at + 4].try_into().expect("four bytes")
    }

    #[test]
    fn the_network_reads_a_frame_at_most_its_side_long_in_multiples_of_32() {
        assert_eq!(flow_size(1920, 1080, 1024), (1024, 576));
        assert_eq!(flow_size(3840, 2160, 1024), (1024, 576));
        assert_eq!(flow_size(1080, 1920, 1024), (576, 1024));
        assert_eq!(flow_size(704, 384, 1024), (704, 384));
        assert_eq!(flow_size(711, 400, 1024), (704, 416));
        assert_eq!(flow_size(40, 20, 1024), (32, 32));
    }

    #[test]
    fn a_blend_is_the_share_of_the_way_from_one_frame_to_the_other() {
        let (a, b) = (solid(4, 4, [0, 100, 200]), solid(4, 4, [200, 100, 0]));
        assert_eq!(pixel(&blend(&a, &b, 0.0), 1, 1), [0, 100, 200, 255]);
        assert_eq!(pixel(&blend(&a, &b, 1.0), 1, 1), [200, 100, 0, 255]);
        assert_eq!(pixel(&blend(&a, &b, 0.25), 1, 1), [50, 100, 150, 255]);
    }

    #[test]
    fn with_no_motion_the_mask_picks_the_frame() {
        let (a, b) = (solid(8, 6, [10, 20, 30]), solid(8, 6, [210, 220, 230]));
        let (fw, fh) = (4, 3);
        let still = vec![0f32; fw * fh * 4];
        let all_a = compose(&a, &b, &still, &vec![40.0; fw * fh], 4, 3).expect("composes");
        assert_eq!(pixel(&all_a, 5, 2), [10, 20, 30, 255]);
        let all_b = compose(&a, &b, &still, &vec![-40.0; fw * fh], 4, 3).expect("composes");
        assert_eq!(pixel(&all_b, 5, 2), [210, 220, 230, 255]);
        let even = compose(&a, &b, &still, &vec![0.0; fw * fh], 4, 3).expect("composes");
        assert_eq!(pixel(&even, 5, 2), [110, 120, 130, 255]);
    }

    #[test]
    fn motion_is_read_at_the_networks_size_and_moved_at_the_frames() {
        // The network saw the frame at half its width: a motion of one of
        // its pixels to the right is two of the frame's.
        let a = ramp(16, 8);
        let b = solid(16, 8, [0, 0, 0]);
        let (fw, fh) = (8usize, 8usize);
        let mut flow = vec![0f32; fw * fh * 4];
        flow[..fw * fh].fill(1.0);
        let moved = compose(&a, &b, &flow, &vec![40.0; fw * fh], 8, 8).expect("composes");
        // The pixel at x = 3 shows what was at x = 5 in `a`.
        assert_eq!(pixel(&moved, 3, 2)[0], 50);
        assert_eq!(pixel(&moved, 3, 2)[1], 20);
        // Past the right edge the edge is held.
        assert_eq!(pixel(&moved, 15, 2)[0], 150);
    }

    #[test]
    fn frames_of_two_sizes_are_refused() {
        let (a, b) = (solid(4, 4, [0, 0, 0]), solid(6, 4, [0, 0, 0]));
        assert!(compose(&a, &b, &[0.0; 64], &[0.0; 16], 4, 4).is_err());
        assert_eq!(pixel(&blend(&a, &b, 0.5), 0, 0), [0, 0, 0, 255]);
    }

    /// The exported network on two pictures of a square moving right, when
    /// `CONCAT_TWEEN_MODEL` names the model file: the frame halfway has it
    /// halfway.
    #[cfg(feature = "infer")]
    #[test]
    fn the_network_puts_a_moving_square_halfway() {
        let Some(path) = std::env::var_os("CONCAT_TWEEN_MODEL") else {
            return;
        };
        let square = |left: u32| {
            let mut pixels = Vec::new();
            for y in 0..96u32 {
                for x in 0..160u32 {
                    let inside = (32..64).contains(&y) && (left..left + 24).contains(&x);
                    let v = if inside { 240 } else { 20 };
                    pixels.extend_from_slice(&[v, v, v, 255]);
                }
            }
            Frame::from_rgba(160, 96, pixels).expect("a frame")
        };
        let tweener = Tweener::load(std::path::Path::new(&path)).expect("loads");
        let frames = tweener
            .between(&square(40), &square(72), &[0.5])
            .expect("runs");
        // Halfway the square spans 56..80: its middle is bright and the
        // places it left and has not reached are dark.
        assert!(
            pixel(&frames[0], 68, 48)[0] > 200,
            "{:?}",
            pixel(&frames[0], 68, 48)
        );
        assert!(
            pixel(&frames[0], 44, 48)[0] < 60,
            "{:?}",
            pixel(&frames[0], 44, 48)
        );
        assert!(
            pixel(&frames[0], 92, 48)[0] < 60,
            "{:?}",
            pixel(&frames[0], 92, 48)
        );
    }
}
