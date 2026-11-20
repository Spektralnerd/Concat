// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: a film's scenes - the frames its shots change at - found the way
//! PySceneDetect 0.7's `detect-content` finds them with its defaults, so the
//! list made here, the one `scenedetect` makes on a PC and the one the
//! Szenenschnitt apps make of the same film name the same frames.
//!
//! Every frame is read as PySceneDetect reads it through OpenCV: made BGR as
//! swscale's x86 converters make it (in the matrix and range the frame is
//! tagged with, as OpenCV 5 asks for, each pixel the chroma of the sample
//! it lies in), shrunk with OpenCV's bilinear `resize` in OpenCV's own
//! fixed-point arithmetic until its longer side is 256 pixels, and turned
//! into OpenCV's 8-bit HSV. A frame's score is how far its hue,
//! saturation and value moved from the frame before's - for each, the mean
//! of the pixels' differences - averaged over the three; a frame scoring
//! the threshold or more starts a new scene, unless the last cut was less
//! than the shortest scene ago, when the two are merged ([`Filter`]).
//!
//! While the film is read, three pictures of each scene are kept, at the
//! frames `scenedetect save-images` picks: the second, the middle and the
//! last - the middle one near the middle rather than on it, see [`Middle`].
//! They come out through the caller's sink as small RGB pictures as each
//! scene is closed, so a two-hour film never holds more than a handful of
//! frames.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::format::Pixel;
use ffmpeg_the_third::software::scaling;
use ffmpeg_the_third::util::frame::video::Video;

use crate::error::{Error, Result};
use crate::ffi;

/// What counts as a cut. The defaults are `detect-content`'s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// The score a frame must reach to start a scene: 27 by default; lower
    /// finds more cuts, higher fewer.
    pub threshold: f64,
    /// The shortest scene, in seconds: 0.6 by default. Zero lets every
    /// frame over the threshold cut.
    pub min_seconds: f64,
    /// A cut sooner than the shortest scene after the last one is merged
    /// with it (true, PySceneDetect's default) rather than dropped.
    pub merge: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threshold: 27.0,
            min_seconds: 0.6,
            merge: true,
        }
    }
}

impl Settings {
    /// The shortest scene in frames at `fps`, as PySceneDetect's command
    /// line turns "0.6s" into frames: rounded, halves to even, as Python's
    /// `round` does - 14 frames at 23.976, 15 at 25.
    pub fn min_frames(&self, fps: f64) -> u64 {
        if self.min_seconds.is_nan() || self.min_seconds <= 0.0 || fps.is_nan() || fps <= 0.0 {
            return 0;
        }
        (self.min_seconds * fps).round_ties_even() as u64
    }
}

/// What a whole film came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Frames a second, as the list counts them: the stream's average
    /// rate, which is the one OpenCV reports.
    pub fps: f64,
    /// Every frame read, the last scene's end.
    pub frames: u64,
    /// The first frame of every scene after the first, rising.
    pub cuts: Vec<u64>,
}

/// What the film has to say while it is read.
pub enum Event<'a> {
    /// How far the read has come.
    Progress {
        /// Frames read so far.
        frames: u64,
        /// About how many the film has; never fewer than `frames`.
        total: u64,
    },
    /// One of a scene's pictures.
    Picture {
        /// The scene, counted from 1.
        scene: usize,
        /// Which of its pictures: 1 the second frame, 2 the middle, 3
        /// the last.
        index: u8,
        /// The frame it shows.
        frame: u64,
        /// Its width in pixels.
        width: u32,
        /// Its height in pixels.
        height: u32,
        /// RGB, three bytes a pixel, rows packed.
        rgb: &'a [u8],
    },
}

/// The scenes of the film at `path`, read through once: the list, and on
/// the way three pictures of every scene `pictures` pixels wide (none for
/// `None`). `None` when `cancel` was raised before the end.
pub fn detect(
    path: &Path,
    settings: &Settings,
    pictures: Option<u32>,
    sink: &mut dyn FnMut(Event<'_>),
    cancel: &AtomicBool,
) -> Result<Option<Found>> {
    let mut film = Film::open(path)?;
    let fps = film.fps;
    let mut filter = Filter::new(settings.min_frames(fps), settings.merge);
    let mut analyzer = Analyzer::new(film.width, film.height);
    let mut keeper = match pictures {
        Some(width) => Some(Keeper::new(Painter::new(
            film.display,
            film.rotation,
            width,
        ))),
        None => None,
    };
    let threshold = settings.threshold;
    let mut cuts = Vec::new();
    let mut frames = 0u64;
    let total = film.total;
    let mut said = Instant::now();
    let read = film.each_frame(cancel, |frame| {
        let index = frames;
        frames += 1;
        let score = analyzer.score(frame).unwrap_or(0.0);
        let above = score >= threshold;
        let cut = filter.feed(index, above).filter(|&cut| cut > 0);
        if let Some(keeper) = keeper.as_mut() {
            keeper.frame(index, frame, above, cut, sink);
        }
        if let Some(cut) = cut {
            cuts.push(cut);
        }
        if said.elapsed().as_millis() >= 250 {
            said = Instant::now();
            sink(Event::Progress {
                frames,
                total: total.max(frames),
            });
        }
        Ok(())
    })?;
    if read == Read::Cancelled {
        return Ok(None);
    }
    if let Some(keeper) = keeper.as_mut() {
        keeper.finish(frames, sink);
    }
    sink(Event::Progress {
        frames,
        total: frames,
    });
    Ok(Some(Found { fps, frames, cuts }))
}

/// The pictures of scenes already known - from a list made elsewhere -
/// read through the film once: for every scene `[start, end)` of
/// `scenes`, numbered from 1 in order, the three pictures [`detect`]
/// keeps, `width` pixels wide. False when `cancel` was raised before the
/// end.
pub fn pictures(
    path: &Path,
    scenes: &[(u64, u64)],
    width: u32,
    sink: &mut dyn FnMut(Event<'_>),
    cancel: &AtomicBool,
) -> Result<bool> {
    let mut film = Film::open(path)?;
    let mut painter = Painter::new(film.display, film.rotation, width);
    // Every frame wanted, in order, with the pictures it makes.
    let mut wanted: Vec<(u64, usize, u8)> = Vec::new();
    for (number, &(start, end)) in scenes.iter().enumerate() {
        for (index, frame) in picture_frames(start, end).into_iter().enumerate() {
            wanted.push((frame, number + 1, index as u8 + 1));
        }
    }
    wanted.sort_unstable();
    let last = wanted.last().map_or(0, |&(frame, _, _)| frame);
    let mut next = 0;
    let mut frames = 0u64;
    let total = film.total;
    let mut said = Instant::now();
    let read = film.each_frame(cancel, |frame| {
        let index = frames;
        frames += 1;
        while next < wanted.len() && wanted[next].0 == index {
            let (_, scene, picture) = wanted[next];
            let (width, height, rgb) = painter.paint(frame)?;
            sink(Event::Picture {
                scene,
                index: picture,
                frame: index,
                width,
                height,
                rgb,
            });
            next += 1;
        }
        if said.elapsed().as_millis() >= 250 {
            said = Instant::now();
            sink(Event::Progress {
                frames,
                total: total.max(frames),
            });
        }
        if index >= last || next >= wanted.len() {
            return Err(StopOr::Stop);
        }
        Ok(())
    })?;
    Ok(read != Read::Cancelled)
}

/// Every frame's score, the first's zero: what PySceneDetect's stats file
/// calls `content_val`, for holding the two side by side.
pub fn scores(path: &Path, cancel: &AtomicBool) -> Result<Vec<f64>> {
    let mut film = Film::open(path)?;
    let mut analyzer = Analyzer::new(film.width, film.height);
    let mut scores = Vec::new();
    film.each_frame(cancel, |frame| {
        scores.push(analyzer.score(frame).unwrap_or(0.0));
        Ok(())
    })?;
    Ok(scores)
}

/// The frames `scenedetect save-images` saves of the scene `[start, end)`
/// with its defaults (three pictures, a frame's margin), as PySceneDetect
/// 0.7 picks them: the scene cut in three, the first picture a frame into
/// the first third, the second the middle of the scene, the third the last
/// frame - at frame counts, each the nearest frame, halves to even.
pub fn picture_frames(start: u64, end: u64) -> [u64; 3] {
    if end <= start + 1 {
        return [start; 3];
    }
    let length = (end - start) as f64;
    let third = length / 3.0;
    let start_f = start as f64;
    let first = (start_f + 1.0).min(start_f + third);
    let middle = start_f + length / 2.0;
    let last = (end as f64 - 1.0).max(start_f + 2.0 * third);
    let frame = |at: f64| (at.round_ties_even() as u64).clamp(start, end - 1);
    [frame(first), frame(middle), frame(last)]
}

// ─── reading the film ─────────────────────────────────────────────────────

/// How a read through the film ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// At the film's end.
    Ended,
    /// The caller's cancel was raised.
    Cancelled,
    /// The frame handler had all it wanted.
    Stopped,
}

/// The film opened for reading every frame in order, on every core, in
/// software: the frames PySceneDetect counts, as FFmpeg's own decoder
/// makes them.
struct Film {
    path: std::path::PathBuf,
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    index: usize,
    fps: f64,
    width: u32,
    height: u32,
    /// About how many frames the film has, or 0.
    total: u64,
    /// The size the film is shown at: its pixels made square, turned
    /// upright.
    display: (u32, u32),
    /// Degrees the picture is turned for showing.
    rotation: i64,
}

impl Film {
    fn open(path: &Path) -> Result<Self> {
        ffi::init();
        let mut input =
            ffmpeg::format::input(path).map_err(|error| ffi::fail("open", path, error))?;
        let (index, context, fps, total, display, rotation) =
            {
                let stream = input.streams().best(ffmpeg::media::Type::Video).ok_or(
                    Error::NoVideoStream {
                        path: path.to_path_buf(),
                    },
                )?;
                let fps = frame_rate(&input, &stream);
                let frames = stream.frames();
                let total = if frames > 0 {
                    frames as u64
                } else {
                    let seconds = if stream.duration() > 0 {
                        stream.duration() as f64 * f64::from(stream.time_base())
                    } else if input.duration() > 0 {
                        input.duration() as f64 / f64::from(ffmpeg::sys::AV_TIME_BASE)
                    } else {
                        0.0
                    };
                    (seconds * fps).round().max(0.0) as u64
                };
                let rotation = ffi::rotation(&stream);
                let (width, height) = {
                    let parameters = stream.parameters();
                    // SAFETY: a live stream's codec parameters; plain integers.
                    unsafe {
                        let raw = parameters.as_ptr();
                        ((*raw).width.max(1) as u32, (*raw).height.max(1) as u32)
                    }
                };
                let display = ffi::displayed(width, height, ffi::sample_aspect(&stream), rotation);
                let context = ffmpeg::codec::Context::from_parameters(stream.parameters())
                    .map_err(|error| ffi::fail("codec parameters", path, error))?;
                (stream.index(), context, fps, total, display, rotation)
            };
        // Only the picture is read; the demuxer skips the rest.
        // SAFETY: the context's streams are valid while it lives; `discard`
        // is a plain field read before each packet is returned.
        unsafe {
            let raw = input.as_mut_ptr();
            for other in 0..(*raw).nb_streams as usize {
                if other != index {
                    let stream = *(*raw).streams.add(other);
                    (*stream).discard = ffmpeg::sys::AVDiscard::ALL;
                }
            }
        }
        let mut context = context;
        // Every core, frame and slice threads, as the `ffmpeg` tool and
        // OpenCV open a decoder.
        // SAFETY: the context is not opened yet; plain integers.
        unsafe {
            let raw = context.as_mut_ptr();
            (*raw).thread_count = 0;
            (*raw).thread_type = ffmpeg::sys::FF_THREAD_FRAME | ffmpeg::sys::FF_THREAD_SLICE;
        }
        ffi::begin();
        let decoder = context
            .decoder()
            .video()
            .map_err(|error| ffi::fail_said("open decoder", path, error))?;
        let (width, height) = (decoder.width().max(1), decoder.height().max(1));
        Ok(Self {
            path: path.to_path_buf(),
            input,
            decoder,
            index,
            fps,
            width,
            height,
            total,
            display,
            rotation,
        })
    }

    /// Hands every frame to `each`, in order, until the film ends,
    /// `cancel` is raised or `each` says it has had enough or fails.
    fn each_frame(
        &mut self,
        cancel: &AtomicBool,
        mut each: impl FnMut(&Video) -> std::result::Result<(), StopOr>,
    ) -> Result<Read> {
        let mut frame = Video::empty();
        let mut ended = false;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Read::Cancelled);
            }
            if !ended {
                let mut packet = ffmpeg::Packet::empty();
                match packet.read(&mut self.input) {
                    Ok(()) if packet.stream() == self.index => {
                        // A damaged packet is the codec's to conceal, as a
                        // player does: the frame count carries on.
                        if let Err(error) = self.decoder.send_packet(&packet)
                            && !ffi::is_again(&error)
                        {
                            log::debug!("scenes: a packet the decoder refused: {error}");
                        }
                    }
                    Ok(()) => continue,
                    Err(ffmpeg::Error::Eof) => {
                        ended = true;
                        let _ = self.decoder.send_eof();
                    }
                    Err(error) => return Err(ffi::fail("read", &self.path, error)),
                }
            }
            loop {
                match self.decoder.receive_frame(&mut frame) {
                    Ok(()) => match each(&frame) {
                        Ok(()) => {}
                        Err(StopOr::Stop) => return Ok(Read::Stopped),
                        Err(StopOr::Error(error)) => return Err(error),
                    },
                    Err(ffmpeg::Error::Eof) => return Ok(Read::Ended),
                    // Drained: past the end there is nothing more to come.
                    Err(error) if ffi::is_again(&error) && ended => return Ok(Read::Ended),
                    Err(error) if ffi::is_again(&error) => break,
                    Err(error) => {
                        // A frame the codec cannot make is skipped, as
                        // OpenCV skips it; past the end, that is the end.
                        log::debug!("scenes: a frame that would not decode: {error}");
                        if ended {
                            return Ok(Read::Ended);
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// What stops a frame's handler: having had enough, or a failure.
enum StopOr {
    Stop,
    Error(Error),
}

impl From<Error> for StopOr {
    fn from(error: Error) -> Self {
        StopOr::Error(error)
    }
}

/// The stream's frame rate as OpenCV reports it: the average rate, or
/// FFmpeg's guess where a stream gives none, or 25.
fn frame_rate(
    input: &ffmpeg::format::context::Input,
    stream: &ffmpeg::format::stream::Stream<'_>,
) -> f64 {
    let usable = |rate: ffmpeg::Rational| {
        (rate.numerator() > 0 && rate.denominator() > 0).then(|| f64::from(rate))
    };
    usable(stream.avg_frame_rate())
        .or_else(|| {
            // SAFETY: both pointers are live for the call; the guess only
            // reads them.
            let guessed = unsafe {
                ffmpeg::sys::av_guess_frame_rate(
                    input.as_ptr().cast_mut(),
                    stream.as_ptr().cast_mut(),
                    std::ptr::null_mut(),
                )
            };
            usable(guessed.into())
        })
        .unwrap_or(25.0)
}

// ─── the score ────────────────────────────────────────────────────────────

/// The size PySceneDetect analyses a `width` by `height` frame at: shrunk
/// until the longer side is 256 pixels (`compute_downscale_factor` on the
/// larger side), each side rounded as Python rounds, halves to even. A
/// frame no larger is analysed as it is.
pub fn analysis_size(width: u32, height: u32) -> (u32, u32) {
    let longer = width.max(height);
    if longer < 256 {
        return (width.max(1), height.max(1));
    }
    let factor = f64::from(longer) / 256.0;
    if factor <= 1.0 {
        return (width.max(1), height.max(1));
    }
    let side = |pixels: u32| ((f64::from(pixels) / factor).round_ties_even() as u32).max(1);
    (side(width), side(height))
}

/// One output sample of OpenCV's bilinear shrink along an axis: the two
/// input samples it is made of and their weights, which add to 2048.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Tap {
    near: usize,
    far: usize,
    near_weight: i32,
    far_weight: i32,
}

/// `cv::resize`'s INTER_LINEAR taps for `source` samples shrunk to
/// `target`, as its eight-bit path computes them: each sample's centre
/// mapped back in single precision, floored, and the two weights rounded
/// to eleven bits. Columns past the edge take the edge alone
/// (`clamp_far`); rows keep their weights and read the edge row twice, as
/// OpenCV's row cache does.
fn taps(source: usize, target: usize, clamp_far: bool) -> Vec<Tap> {
    let scale = 1.0 / (target as f64 / source as f64);
    (0..target)
        .map(|out| {
            let at = ((out as f64 + 0.5) * scale - 0.5) as f32;
            let mut near = at.floor() as i64;
            let mut weight = at - near as f32;
            if near < 0 {
                near = 0;
                weight = 0.0;
            }
            if clamp_far && near >= source as i64 - 1 {
                near = source as i64 - 1;
                weight = 0.0;
            }
            let near = near.clamp(0, source as i64 - 1) as usize;
            let far = (near + 1).min(source - 1);
            Tap {
                near,
                far,
                near_weight: ((1.0 - weight) * 2048.0).round_ties_even() as i32,
                far_weight: (weight * 2048.0).round_ties_even() as i32,
            }
        })
        .collect()
}

/// OpenCV's eight-bit BGR to HSV: hue in 0..180, as `cv2.COLOR_BGR2HSV`
/// gives it, through the same tables and rounding.
struct Hsv {
    sdiv: [i32; 256],
    hdiv: [i32; 256],
}

impl Hsv {
    fn new() -> Self {
        let mut sdiv = [0; 256];
        let mut hdiv = [0; 256];
        for i in 1..256 {
            sdiv[i] = ((255 << 12) as f64 / i as f64).round_ties_even() as i32;
            hdiv[i] = ((180 << 12) as f64 / (6.0 * i as f64)).round_ties_even() as i32;
        }
        Self { sdiv, hdiv }
    }

    #[inline]
    fn of(&self, b: i32, g: i32, r: i32) -> (u8, u8, u8) {
        let v = b.max(g).max(r);
        let vmin = b.min(g).min(r);
        let diff = v - vmin;
        let vr = if v == r { -1 } else { 0 };
        let vg = if v == g { -1 } else { 0 };
        let s = (diff * self.sdiv[v as usize] + (1 << 11)) >> 12;
        let mut h =
            (vr & (g - b)) + (!vr & ((vg & (b - r + 2 * diff)) + (!vg & (r - g + 4 * diff))));
        h = (h * self.hdiv[diff as usize] + (1 << 11)) >> 12;
        if h < 0 {
            h += 180;
        }
        (h.clamp(0, 255) as u8, s as u8, v as u8)
    }
}

#[inline]
fn clamp_byte(value: i32) -> i32 {
    value.clamp(0, 255)
}

/// YUV to BGR as swscale's x86 converters compute it, which is what OpenCV
/// hands PySceneDetect on a PC: the matrix's coefficients in thirteen-bit
/// fixed point, each term floored on its own (`pmulhw`), the sum clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Matrix {
    y_offset: i32,
    y: i32,
    ub: i32,
    ug: i32,
    vg: i32,
    vr: i32,
}

impl Matrix {
    /// swscale's coefficients for the colour space numbered `space` (an
    /// `AVColorSpace`) in the full (JPEG) range or the limited one, made
    /// as `ff_yuv2rgb_c_init_tables` makes them; BT.601 for a space it
    /// has none for.
    fn of(space: usize, full: bool) -> Self {
        // ff_yuv2rgb_coeffs: Cr to red, Cb to blue, Cb and Cr to green.
        const TABLE: [[i64; 4]; 11] = [
            [104_597, 132_201, 25675, 53279],
            [117_489, 138_438, 13975, 34925],
            [104_597, 132_201, 25675, 53279],
            [104_597, 132_201, 25675, 53279],
            [104_448, 132_798, 24759, 53109],
            [104_597, 132_201, 25675, 53279],
            [104_597, 132_201, 25675, 53279],
            [117_579, 136_230, 16907, 35559],
            [0, 0, 0, 0],
            [110_013, 140_363, 12277, 42626],
            [110_013, 140_363, 12277, 42626],
        ];
        let [crv, cbu, cgu, cgv] = TABLE
            .get(space)
            .copied()
            .filter(|row| row[0] != 0)
            .unwrap_or(TABLE[5]);
        let (mut crv, mut cbu, mut cgu, mut cgv) = (crv, cbu, -cgu, -cgv);
        let (cy, oy) = if full {
            // Integer division, towards zero, as C divides.
            crv = crv * 224 / 255;
            cbu = cbu * 224 / 255;
            cgu = cgu * 224 / 255;
            cgv = cgv * 224 / 255;
            (1i64 << 16, 0i64)
        } else {
            ((1i64 << 16) * 255 / 219, 16i64 << 16)
        };
        let round = |fixed: i64| ((fixed + (1 << 15)) >> 16) as i32;
        Self {
            y_offset: round(oy * 8),
            y: round(cy * 8192),
            ub: round(cbu * 8192),
            ug: round(cgu * 8192),
            vg: round(cgv * 8192),
            vr: round(crv * 8192),
        }
    }

    /// The matrix `frame` is tagged with, as OpenCV 5's swscale reads it.
    fn of_frame(frame: &Video) -> Self {
        // SAFETY: a decoded frame's own field, a plain integer.
        let space = unsafe { (*frame.as_ptr()).colorspace.0 as i32 };
        let full = frame.color_range() == ffmpeg::color::Range::JPEG
            || matches!(
                frame.format(),
                Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P
            );
        Self::of(usize::try_from(space).unwrap_or(2), full)
    }

    #[inline]
    fn bgr(&self, y: i32, u: i32, v: i32) -> [i32; 3] {
        let y = ((y * 8 - self.y_offset) * self.y) >> 16;
        let (u, v) = ((u - 128) * 8, (v - 128) * 8);
        [
            clamp_byte(y + ((u * self.ub) >> 16)),
            clamp_byte(y + (((u * self.ug) >> 16) + ((v * self.vg) >> 16))),
            clamp_byte(y + ((v * self.vr) >> 16)),
        ]
    }
}

/// Where a frame's pixels are read from, as BGR.
trait Pixels {
    fn bgr(&self, x: usize, y: usize) -> [i32; 3];
}

/// Eight-bit planar YUV, chroma halved across (`cw`) and down (`ch`) or not.
struct Planar<'a> {
    luma: &'a [u8],
    blue: &'a [u8],
    red: &'a [u8],
    strides: [usize; 3],
    cw: u32,
    ch: u32,
    matrix: Matrix,
}

impl Pixels for Planar<'_> {
    #[inline]
    fn bgr(&self, x: usize, y: usize) -> [i32; 3] {
        let luma = i32::from(self.luma[y * self.strides[0] + x]);
        let at = |plane: &[u8], stride: usize| {
            i32::from(plane[(y >> self.ch) * stride + (x >> self.cw)])
        };
        let u = at(self.blue, self.strides[1]);
        let v = at(self.red, self.strides[2]);
        self.matrix.bgr(luma, u, v)
    }
}

/// Eight-bit YUV with the two chroma planes woven together, U first (NV12)
/// or V first (NV21): what a phone's hardware decoder hands back.
struct Woven<'a> {
    luma: &'a [u8],
    chroma: &'a [u8],
    strides: [usize; 2],
    v_first: bool,
    matrix: Matrix,
}

impl Pixels for Woven<'_> {
    #[inline]
    fn bgr(&self, x: usize, y: usize) -> [i32; 3] {
        let luma = i32::from(self.luma[y * self.strides[0] + x]);
        let pair = (y >> 1) * self.strides[1] + (x >> 1) * 2;
        let (u, v) = if self.v_first {
            (self.chroma[pair + 1], self.chroma[pair])
        } else {
            (self.chroma[pair], self.chroma[pair + 1])
        };
        self.matrix.bgr(luma, i32::from(u), i32::from(v))
    }
}

/// Packed BGR, three bytes a pixel: any other format, made so by swscale
/// as OpenCV makes it.
struct Packed<'a> {
    data: &'a [u8],
    stride: usize,
}

impl Pixels for Packed<'_> {
    #[inline]
    fn bgr(&self, x: usize, y: usize) -> [i32; 3] {
        let at = y * self.stride + x * 3;
        [
            i32::from(self.data[at]),
            i32::from(self.data[at + 1]),
            i32::from(self.data[at + 2]),
        ]
    }
}

/// Scores frames: each against the one before, as [`ContentDetector`]
/// does.
///
/// [`ContentDetector`]: https://www.scenedetect.com/docs/latest/api/detectors.html
struct Analyzer {
    width: usize,
    height: usize,
    /// The analysis size.
    columns: usize,
    rows: usize,
    across: Vec<Tap>,
    down: Vec<Tap>,
    /// Exactly half each way: OpenCV averages blocks of four instead.
    halves: bool,
    hsv: Hsv,
    /// Hue, saturation and value planes of this frame and the last.
    planes: [Vec<u8>; 2],
    current: usize,
    have_last: bool,
    /// Any other format, made BGR through swscale first, as OpenCV does.
    scaler: Option<(Pixel, u32, u32, scaling::Context)>,
    packed: Video,
}

impl Analyzer {
    fn new(width: u32, height: u32) -> Self {
        let (columns, rows) = analysis_size(width, height);
        let (width, height) = (width.max(1) as usize, height.max(1) as usize);
        let (columns, rows) = (columns as usize, rows as usize);
        let scaled = columns != width || rows != height;
        let halves = scaled && columns * 2 == width && rows * 2 == height;
        let identity = |count: usize| {
            (0..count)
                .map(|at| Tap {
                    near: at,
                    far: at,
                    near_weight: 2048,
                    far_weight: 0,
                })
                .collect::<Vec<_>>()
        };
        let (across, down) = if scaled {
            (taps(width, columns, true), taps(height, rows, false))
        } else {
            (identity(width), identity(height))
        };
        let size = columns * rows * 3;
        Self {
            width,
            height,
            columns,
            rows,
            across,
            down,
            halves,
            hsv: Hsv::new(),
            planes: [vec![0; size], vec![0; size]],
            current: 0,
            have_last: false,
            scaler: None,
            packed: Video::empty(),
        }
    }

    /// The frame's score against the last frame, or `None` for the first.
    fn score(&mut self, frame: &Video) -> Option<f64> {
        self.read(frame);
        let n = self.columns * self.rows;
        let score = self.have_last.then(|| {
            let (now, last) = (&self.planes[self.current], &self.planes[self.current ^ 1]);
            let distance = |plane: usize| {
                let range = plane * n..(plane + 1) * n;
                let sum: u64 = now[range.clone()]
                    .iter()
                    .zip(&last[range])
                    .map(|(&a, &b)| u64::from(a.abs_diff(b)))
                    .sum();
                sum as f64 / n as f64
            };
            // `sum(component * weight) / sum(abs(weight))` with the default
            // weights 1, 1, 1 and 0 for edges. The components are numpy's
            // floats, which Python's `sum` adds plainly, left to right -
            // not with the compensation it gives its own floats.
            (distance(0) + distance(1) + distance(2) + 0.0) / 3.0
        });
        self.current ^= 1;
        self.have_last = true;
        score
    }

    /// Fills this frame's planes.
    fn read(&mut self, frame: &Video) {
        let format = frame.format();
        let same_size =
            frame.width() as usize == self.width && frame.height() as usize == self.height;
        let matrix = Matrix::of_frame(frame);
        let planar = |cw: u32, ch: u32| Planar {
            luma: frame.data(0),
            blue: frame.data(1),
            red: frame.data(2),
            strides: [frame.stride(0), frame.stride(1), frame.stride(2)],
            cw,
            ch,
            matrix,
        };
        match format {
            Pixel::YUV420P | Pixel::YUVJ420P if same_size => self.fill(&planar(1, 1)),
            Pixel::YUV422P | Pixel::YUVJ422P if same_size => self.fill(&planar(1, 0)),
            Pixel::YUV444P | Pixel::YUVJ444P if same_size => self.fill(&planar(0, 0)),
            Pixel::NV12 | Pixel::NV21 if same_size => {
                let woven = Woven {
                    luma: frame.data(0),
                    chroma: frame.data(1),
                    strides: [frame.stride(0), frame.stride(1)],
                    v_first: format == Pixel::NV21,
                    matrix,
                };
                self.fill(&woven);
            }
            _ => {
                // Ten bits, RGB, a size changed mid-stream: BGR at the
                // film's own size first, as OpenCV's capture makes it.
                let key = (format, frame.width(), frame.height());
                if self
                    .scaler
                    .as_ref()
                    .is_none_or(|(f, w, h, _)| (*f, *w, *h) != key)
                {
                    self.scaler = scaling::Context::get(
                        format,
                        frame.width(),
                        frame.height(),
                        Pixel::BGR24,
                        self.width as u32,
                        self.height as u32,
                        scaling::Flags::BICUBIC,
                    )
                    .ok()
                    .map(|mut scaler| {
                        set_colours(&mut scaler, frame);
                        (format, frame.width(), frame.height(), scaler)
                    });
                }
                let Some((_, _, _, scaler)) = self.scaler.as_mut() else {
                    return;
                };
                if scaler.run(frame, &mut self.packed).is_err() {
                    return;
                }
                let packed = std::mem::replace(&mut self.packed, Video::empty());
                {
                    let source = Packed {
                        data: packed.data(0),
                        stride: packed.stride(0),
                    };
                    self.fill(&source);
                }
                self.packed = packed;
            }
        }
    }

    /// Shrinks the picture `pixels` reads into this frame's HSV planes.
    fn fill(&mut self, pixels: &impl Pixels) {
        let n = self.columns * self.rows;
        let plane = &mut self.planes[self.current];
        let (hue, rest) = plane.split_at_mut(n);
        let (saturation, value) = rest.split_at_mut(n);
        let hsv = &self.hsv;
        for row in 0..self.rows {
            let down = self.down[row];
            for column in 0..self.columns {
                let at = row * self.columns + column;
                let [b, g, r] = if self.halves {
                    let (x, y) = (column * 2, row * 2);
                    let corners = [
                        pixels.bgr(x, y),
                        pixels.bgr(x + 1, y),
                        pixels.bgr(x, y + 1),
                        pixels.bgr(x + 1, y + 1),
                    ];
                    let mut out = [0; 3];
                    for (channel, out) in out.iter_mut().enumerate() {
                        let sum: i32 = corners.iter().map(|corner| corner[channel]).sum();
                        *out = (sum + 2) >> 2;
                    }
                    out
                } else {
                    let across = self.across[column];
                    let near_row = (
                        pixels.bgr(across.near, down.near),
                        pixels.bgr(across.far, down.near),
                    );
                    let far_row = (
                        pixels.bgr(across.near, down.far),
                        pixels.bgr(across.far, down.far),
                    );
                    let mut out = [0; 3];
                    for (channel, out) in out.iter_mut().enumerate() {
                        let top = near_row.0[channel] * across.near_weight
                            + near_row.1[channel] * across.far_weight;
                        let bottom = far_row.0[channel] * across.near_weight
                            + far_row.1[channel] * across.far_weight;
                        *out = clamp_byte(
                            (((down.near_weight * (top >> 4)) >> 16)
                                + ((down.far_weight * (bottom >> 4)) >> 16)
                                + 2)
                                >> 2,
                        );
                    }
                    out
                };
                let (h, s, v) = hsv.of(b, g, r);
                hue[at] = h;
                saturation[at] = s;
                value[at] = v;
            }
        }
    }
}

// ─── the cuts ─────────────────────────────────────────────────────────────

/// PySceneDetect's `FlashFilter`, counted in frames: a cut must come at
/// least `min_len` frames after the last frame over the threshold. Merged,
/// a run of quick cuts becomes one at its last; suppressed, the quick ones
/// are dropped.
#[derive(Debug, Clone)]
pub struct Filter {
    min_len: u64,
    merge: bool,
    last_above: Option<u64>,
    merge_enabled: bool,
    merge_triggered: bool,
    merge_start: u64,
}

impl Filter {
    /// A filter for scenes of at least `min_len` frames.
    pub fn new(min_len: u64, merge: bool) -> Self {
        Self {
            min_len,
            merge,
            last_above: None,
            merge_enabled: false,
            merge_triggered: false,
            merge_start: 0,
        }
    }

    /// Frame `frame` scored over the threshold (`above`) or not: the cut
    /// this makes, if any - this frame, or in a merge an earlier one.
    pub fn feed(&mut self, frame: u64, above: bool) -> Option<u64> {
        if self.min_len == 0 {
            return above.then_some(frame);
        }
        let last = *self.last_above.get_or_insert(frame);
        let length_met = frame.saturating_sub(last) >= self.min_len;
        if !self.merge {
            if above && length_met {
                self.last_above = Some(frame);
                return Some(frame);
            }
            return None;
        }
        if above {
            self.last_above = Some(frame);
        }
        let last_above = self.last_above.unwrap_or(frame);
        if self.merge_triggered {
            if length_met && !above && last_above.saturating_sub(self.merge_start) >= self.min_len {
                self.merge_triggered = false;
                return Some(last_above);
            }
            return None;
        }
        if !above {
            return None;
        }
        if length_met {
            self.merge_enabled = true;
            return Some(frame);
        }
        if self.merge_enabled {
            self.merge_triggered = true;
            self.merge_start = frame;
        }
        None
    }
}

// ─── the pictures ─────────────────────────────────────────────────────────

/// Makes a scene's pictures: frames shrunk to RGB, shown the right way up
/// and the right shape.
struct Painter {
    width: u32,
    height: u32,
    rotation: i64,
    scaler: Option<(Pixel, u32, u32, scaling::Context)>,
    scaled: Video,
    rgb: Vec<u8>,
}

impl Painter {
    /// Pictures `width` pixels across (before any quarter turn) of a film
    /// shown `display` large, turned `rotation` degrees for showing.
    fn new(display: (u32, u32), rotation: i64, width: u32) -> Self {
        let (shown_width, shown_height) = display;
        // Pictures are made unturned, then turned: the unturned shape is
        // the shown one with its sides swapped back for a quarter turn.
        let (across, down) = if rotation.rem_euclid(180) == 90 {
            (shown_height, shown_width)
        } else {
            (shown_width, shown_height)
        };
        let width = width.clamp(16, across.max(16));
        let height =
            ((f64::from(width) * f64::from(down) / f64::from(across.max(1))).round() as u32).max(2);
        Self {
            width: width & !1,
            height: height & !1,
            rotation: rotation.rem_euclid(360),
            scaler: None,
            scaled: Video::empty(),
            rgb: Vec::new(),
        }
    }

    /// `frame` as a picture: its size and its RGB rows.
    fn paint(&mut self, frame: &Video) -> std::result::Result<(u32, u32, &[u8]), StopOr> {
        let key = (frame.format(), frame.width(), frame.height());
        if self
            .scaler
            .as_ref()
            .is_none_or(|(f, w, h, _)| (*f, *w, *h) != key)
        {
            let mut scaler = scaling::Context::get(
                frame.format(),
                frame.width(),
                frame.height(),
                Pixel::RGB24,
                self.width,
                self.height,
                scaling::Flags::AREA,
            )
            .map_err(|error| {
                StopOr::Error(ffi::fail("scaler", Path::new("<scene picture>"), error))
            })?;
            set_colours(&mut scaler, frame);
            self.scaler = Some((key.0, key.1, key.2, scaler));
        }
        let Some((_, _, _, scaler)) = self.scaler.as_mut() else {
            return Err(StopOr::Stop);
        };
        scaler.run(frame, &mut self.scaled).map_err(|error| {
            StopOr::Error(ffi::fail("scale", Path::new("<scene picture>"), error))
        })?;
        let (width, height) = (self.width as usize, self.height as usize);
        let stride = self.scaled.stride(0);
        let data = self.scaled.data(0);
        self.rgb.clear();
        self.rgb.reserve(width * height * 3);
        match self.rotation {
            90 | 270 => {
                // A quarter turn: the picture's columns become its rows.
                for out_row in 0..width {
                    for out_column in 0..height {
                        let (x, y) = if self.rotation == 90 {
                            (out_row, height - 1 - out_column)
                        } else {
                            (width - 1 - out_row, out_column)
                        };
                        let at = y * stride + x * 3;
                        self.rgb.extend_from_slice(&data[at..at + 3]);
                    }
                }
                Ok((self.height, self.width, &self.rgb))
            }
            180 => {
                for y in (0..height).rev() {
                    for x in (0..width).rev() {
                        let at = y * stride + x * 3;
                        self.rgb.extend_from_slice(&data[at..at + 3]);
                    }
                }
                Ok((self.width, self.height, &self.rgb))
            }
            _ => {
                for y in 0..height {
                    self.rgb
                        .extend_from_slice(&data[y * stride..y * stride + width * 3]);
                }
                Ok((self.width, self.height, &self.rgb))
            }
        }
    }
}

/// Tells `scaler` the colours `frame` is tagged with - its matrix and its
/// range - which swscale would otherwise take for BT.601 in the limited
/// range; OpenCV 5 tells it the same.
fn set_colours(scaler: &mut scaling::Context, frame: &Video) {
    // SAFETY: a decoded frame's own field, a plain integer.
    let space = unsafe { (*frame.as_ptr()).colorspace.0 as i32 };
    let full = frame.color_range() == ffmpeg::color::Range::JPEG
        || matches!(
            frame.format(),
            Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P
        );
    // An unknown space is swscale's default, BT.601.
    let space = if space <= 0 || space == 2 {
        ffmpeg::sys::SWS_CS_DEFAULT
    } else {
        space
    };
    // SAFETY: `scaler` owns a live SwsContext; the coefficient tables are
    // static and the call only sets fields on it.
    unsafe {
        ffmpeg::sys::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            ffmpeg::sys::sws_getCoefficients(space),
            i32::from(full),
            ffmpeg::sys::sws_getCoefficients(ffmpeg::sys::SWS_CS_DEFAULT),
            1,
            0,
            1 << 16,
            1 << 16,
        );
    }
}

/// A reference to a decoded frame's pictures, not a copy: a frame kept
/// for a picture costs what the decoder's pool lends it.
fn share(frame: &Video) -> Video {
    let mut shared = Video::empty();
    // SAFETY: both pointers are the wrappers' own valid AVFrames for the
    // call; av_frame_ref adds references to the source's buffers and
    // copies its properties into the empty destination.
    let result = unsafe { ffmpeg::sys::av_frame_ref(shared.as_mut_ptr(), frame.as_ptr()) };
    if result < 0 { frame.clone() } else { shared }
}

/// A scene's middle, found without keeping the scene: frames kept at an
/// even spacing from its start that doubles whenever they fill
/// [`Middle::ROOM`], so the scene's middle is seldom further than an
/// eighth of the scene from one of them, whatever its length. Seldom: a
/// scene whose cut was merged, and so known only a shortest scene late,
/// has a gap where its first frames were read as the scene before's.
struct Middle {
    start: u64,
    spacing: u64,
    kept: Vec<(u64, Video)>,
}

impl Middle {
    const ROOM: usize = 8;

    fn new(start: u64) -> Self {
        Self {
            start,
            spacing: 1,
            kept: Vec::with_capacity(Self::ROOM + 1),
        }
    }

    fn offer(&mut self, index: u64, frame: &Video) {
        if index < self.start || !(index - self.start).is_multiple_of(self.spacing) {
            return;
        }
        self.kept.push((index, share(frame)));
        if self.kept.len() > Self::ROOM {
            self.spacing *= 2;
            let (start, spacing) = (self.start, self.spacing);
            self.kept
                .retain(|(index, _)| (index - start).is_multiple_of(spacing));
        }
    }

    /// The kept frame nearest `target` before `end`, and the frames from
    /// `end` on, given to the scene that starts there.
    fn split(mut self, target: u64, end: u64) -> (Option<(u64, Video)>, Middle) {
        let mut next = Middle::new(end);
        let later: Vec<(u64, Video)> = {
            let at = self.kept.partition_point(|(index, _)| *index < end);
            self.kept.split_off(at)
        };
        for (index, frame) in &later {
            next.offer(*index, frame);
        }
        let nearest = self
            .kept
            .into_iter()
            .min_by_key(|(index, _)| index.abs_diff(target));
        (nearest, next)
    }
}

/// Keeps what each scene's pictures need while the film is read, and
/// paints them when the scene closes. A cut is known as the frame it
/// falls on is read, or - in a merge - up to a shortest scene later; so
/// the frames about every frame over the threshold are held ([`Edge`])
/// until it is plainly not a cut, and the middle by [`Middle`].
struct Keeper {
    painter: Painter,
    /// The last three frames read, oldest first.
    recent: VecDeque<(u64, Video)>,
    /// The frames about each frame over the threshold still able to cut.
    edges: Vec<Edge>,
    /// The scene being read: its number, its start, its frames so far.
    scene: usize,
    start: u64,
    first: Option<(u64, Video)>,
    middle: Middle,
}

/// The frames either side of a frame over the threshold: what a cut there
/// would want for the scene it closes and the one it opens.
struct Edge {
    at: u64,
    /// Frames `at - 2`, `at - 1`, `at` and `at + 1`, as far as they came.
    frames: Vec<(u64, Video)>,
}

impl Keeper {
    fn new(painter: Painter) -> Self {
        Self {
            painter,
            recent: VecDeque::with_capacity(4),
            edges: Vec::new(),
            scene: 1,
            start: 0,
            first: None,
            middle: Middle::new(0),
        }
    }

    /// Frame `index` was read, over the threshold or not, and made `cut`.
    fn frame(
        &mut self,
        index: u64,
        frame: &Video,
        above: bool,
        cut: Option<u64>,
        sink: &mut dyn FnMut(Event<'_>),
    ) {
        if self.recent.len() == 3 {
            self.recent.pop_front();
        }
        self.recent.push_back((index, share(frame)));
        // The frame after an edge, for the scene it would open.
        for edge in &mut self.edges {
            if edge.at + 1 == index {
                edge.frames.push((index, share(frame)));
            }
        }
        if above {
            let frames = self
                .recent
                .iter()
                .filter(|(at, _)| *at + 2 >= index)
                .map(|(at, frame)| (*at, share(frame)))
                .collect();
            self.edges.push(Edge { at: index, frames });
        }
        // The open scene's own frames: its second, and its middle.
        if index == self.start + 1 && self.first.is_none() {
            self.first = Some((index, share(frame)));
        }
        self.middle.offer(index, frame);
        if let Some(cut) = cut {
            self.close(cut, sink);
        }
        // Only the newest edges can still cut: in a merge the last frame
        // over the threshold, and this one.
        if self.edges.len() > 2 {
            let keep = self.edges.len() - 2;
            self.edges.drain(..keep);
        }
    }

    /// The scene being read ends where `cut` starts the next.
    fn close(&mut self, cut: u64, sink: &mut dyn FnMut(Event<'_>)) {
        let edge = self
            .edges
            .iter()
            .position(|edge| edge.at == cut)
            .map(|at| self.edges.remove(at));
        let wanted = picture_frames(self.start, cut);
        let middle = std::mem::replace(&mut self.middle, Middle::new(cut));
        let (nearest, next) = middle.split(wanted[1], cut);
        let pick = |want: u64, first: &Option<(u64, Video)>, edge: &Option<Edge>| {
            first
                .iter()
                .chain(edge.iter().flat_map(|edge| edge.frames.iter()))
                .find(|(at, _)| *at == want)
                .map(|(at, frame)| (*at, share(frame)))
        };
        let first = pick(wanted[0], &self.first, &edge)
            .or_else(|| nearest.as_ref().map(|(at, frame)| (*at, share(frame))));
        let last = pick(wanted[2], &None, &edge)
            .or_else(|| nearest.as_ref().map(|(at, frame)| (*at, share(frame))));
        self.paint(1, first, sink);
        self.paint(2, nearest, sink);
        self.paint(3, last, sink);
        // The scene the cut opens: its second frame may already be here.
        self.scene += 1;
        self.start = cut;
        self.first = edge.as_ref().and_then(|edge| {
            edge.frames
                .iter()
                .find(|(at, _)| *at == cut + 1)
                .map(|(at, frame)| (*at, share(frame)))
        });
        self.middle = next;
        // Edges inside the scene just closed can cut no more.
        self.edges.retain(|edge| edge.at > cut);
    }

    /// The film ended at `frames`: the last scene closes there.
    fn finish(&mut self, frames: u64, sink: &mut dyn FnMut(Event<'_>)) {
        if frames <= self.start {
            return;
        }
        let wanted = picture_frames(self.start, frames);
        let middle = std::mem::replace(&mut self.middle, Middle::new(frames));
        let (nearest, _) = middle.split(wanted[1], frames);
        let from_recent = |want: u64, recent: &VecDeque<(u64, Video)>| {
            recent
                .iter()
                .find(|(at, _)| *at == want)
                .map(|(at, frame)| (*at, share(frame)))
        };
        let first = self
            .first
            .take()
            .filter(|(at, _)| *at == wanted[0])
            .or_else(|| from_recent(wanted[0], &self.recent))
            .or_else(|| nearest.as_ref().map(|(at, frame)| (*at, share(frame))));
        let last = from_recent(wanted[2], &self.recent)
            .or_else(|| nearest.as_ref().map(|(at, frame)| (*at, share(frame))));
        self.paint(1, first, sink);
        self.paint(2, nearest, sink);
        self.paint(3, last, sink);
    }

    fn paint(&mut self, index: u8, frame: Option<(u64, Video)>, sink: &mut dyn FnMut(Event<'_>)) {
        let Some((at, frame)) = frame else {
            return;
        };
        match self.painter.paint(&frame) {
            Ok((width, height, rgb)) => sink(Event::Picture {
                scene: self.scene,
                index,
                frame: at,
                width,
                height,
                rgb,
            }),
            Err(StopOr::Error(error)) => log::warn!("scenes: a picture failed: {error}"),
            Err(StopOr::Stop) => {}
        }
    }
}

#[cfg(test)]
#[path = "scenes_tests.rs"]
mod tests;
