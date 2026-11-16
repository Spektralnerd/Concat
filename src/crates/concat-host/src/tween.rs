// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Smooth slow motion: a copy of the span a clip shows with frames made in
//! between its own, written once.
//!
//! A clip slowed to a quarter shows each of its file's frames four times
//! over, and it stutters. Made smooth, it is pointed at a copy of its span
//! with two to thirty-two frames for each of the file's - the original
//! frames where they were and new ones between - so the same speed plays
//! a fresh picture every time. The copy lasts as long as the span and
//! starts where the span does, so the clip's in-point, length, speed and
//! curve all mean what they meant: only the frames are denser.
//!
//! The frames between are made one of three ways ([`Method`]): by RIFE,
//! which moves things to where they were in between (see
//! [`concat_vision::tween`]); by fading the two frames into each other,
//! with no model and no wait; or by FFmpeg's own motion-compensated
//! interpolation, the classic way, slower and blockier than RIFE but with
//! nothing to download. Where the file holds the same frame twice, or cuts
//! to another shot, nothing is made between: the frame is held, rather
//! than a morph of two shots that were never one.
//!
//! As Enhance does, the job reads the span frame by frame and writes the
//! copy into the project's cache, with the span's sound beside it, under
//! a partial name until it is whole; the window then points the clip at
//! it. One job at a time through a [`SingleFlight`], with the model
//! fetched on first use and kept loaded. The copy is named after the
//! file's path, size and modification time, the span, the factor and the
//! method, so asking twice finds the copy already made.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use concat_core::Frame;
use concat_core::time::{FrameRate, Rational};
use concat_media::{
    DecodeOptions, Decoder, EncodeOptions, Encoder, FrameSink, FrameSource, RateMode, VideoCodec,
};
use concat_vision::tween::{self as vision, Tweener};
use concat_vision::{ModelId, models};

pub use crate::cutout::Progress;
use crate::jobs::SingleFlight;

/// How the frames in between are made.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Method {
    /// RIFE: things moved to where they were between the two frames.
    #[default]
    Motion,
    /// The two frames faded into each other.
    Blend,
    /// FFmpeg's `minterpolate`: motion estimated block by block.
    Classic,
}

impl Method {
    /// Every method, in the order a menu lists them.
    pub const ALL: [Method; 3] = [Method::Motion, Method::Blend, Method::Classic];

    /// The name a document and a copy's file name store.
    pub fn name(self) -> &'static str {
        match self {
            Method::Motion => "ai",
            Method::Blend => "blend",
            Method::Classic => "ffmpeg",
        }
    }

    /// The method a stored name means; anything else is RIFE's.
    pub fn parse(name: &str) -> Method {
        match name {
            "blend" => Method::Blend,
            "ffmpeg" => Method::Classic,
            _ => Method::Motion,
        }
    }

    /// Whether it needs the model downloaded.
    pub fn needs_model(self) -> bool {
        self == Method::Motion
    }
}

/// How many frames a copy may have for each of its file's: two, four or
/// eight. More is offered nowhere: a quarter speed is smooth at four, and
/// past eight the copy outgrows any use.
pub const FACTORS: [u32; 5] = [2, 4, 8, 16, 32];

/// The factor a request asks for, held to what is offered.
pub fn factor_of(asked: u32) -> u32 {
    FACTORS
        .iter()
        .copied()
        .min_by_key(|factor| factor.abs_diff(asked))
        .unwrap_or(4)
}

/// The most frames a second a copy is written at: MPEG-4's own limit, its
/// clock counting at most 65535 ticks a second. Nothing short of it is held
/// back - a slow-down to a sixteenth wants sixteen frames for each, and a
/// 60 fps file at that is 960 a second - only the time it takes grows.
pub const MAX_RATE: f64 = 65535.0;

/// How far apart two frames may be, as the mean difference of a small
/// picture of each, before they are taken for a cut rather than motion:
/// past it nothing is made between them.
const CUT: f32 = 0.2;

/// What one copy covers.
#[derive(Clone, Debug)]
pub struct TweenRequest {
    /// The media file to read.
    pub media_path: String,
    /// How many frames for each of the file's; see [`FACTORS`].
    pub factor: u32,
    /// How the frames between are made.
    pub method: Method,
    /// Seconds into the file to start at and how many to cover: the span
    /// a clip shows, with a little either side.
    pub span: (f64, f64),
    /// Where the copy goes; see [`target_for`].
    pub target: PathBuf,
}

/// Where the smooth copy of `media`'s `span` lives under `project`: in the
/// cache, named by the file's path, size and modification time, the span
/// to the millisecond, the factor and the method. None for a file that
/// cannot be stat'ed, which cannot be read either.
pub fn target_for(
    project: &Path,
    media: &str,
    factor: u32,
    method: Method,
    span: (f64, f64),
) -> Option<PathBuf> {
    let meta = std::fs::metadata(media).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    media.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    (
        (span.0 * 1000.0).round() as u64,
        (span.1 * 1000.0).round() as u64,
    )
        .hash(&mut hasher);
    if let Ok(modified) = meta.modified()
        && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
    {
        since.as_secs().hash(&mut hasher);
    }
    Some(project.join("cache").join(format!(
        "tween-{:016x}-{}x-{}.mp4",
        hasher.finish(),
        factor_of(factor),
        method.name()
    )))
}

/// The factor a file at `fps` is copied at when `asked` is: the one asked
/// for, halved until the copy is at most [`MAX_RATE`] frames a second.
pub fn effective_factor(asked: u32, fps: f64) -> u32 {
    let mut factor = factor_of(asked);
    while factor > 2 && fps * f64::from(factor) > MAX_RATE + 1e-6 {
        factor /= 2;
    }
    factor
}

/// The smooth slow motion service: the model, loaded once, and the one-job
/// slot.
pub struct Tweeners {
    gate: Arc<SingleFlight>,
    /// The app's data directory, where the downloaded model lives.
    data: PathBuf,
    model: Mutex<Option<Arc<Tweener>>>,
}

impl Tweeners {
    /// A service with nothing loaded yet, keeping its model under `data`.
    pub fn new(data: &Path) -> Tweeners {
        Tweeners {
            gate: Arc::new(SingleFlight::new()),
            data: data.to_path_buf(),
            model: Mutex::new(None),
        }
    }

    /// Whether a copy is being made.
    pub fn is_busy(&self) -> bool {
        self.gate.is_busy()
    }

    /// Asks the running copy to stop after the frame in hand.
    pub fn cancel(&self) {
        self.gate.cancel();
    }

    /// Whether the model is on disk.
    pub fn installed(&self) -> bool {
        models::installed(&self.data, ModelId::Tween)
    }

    /// The model, fetched first when it is not on disk.
    fn loaded(
        &self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Arc<Tweener>, String> {
        if let Some(loaded) = self
            .model
            .lock()
            .map_err(|_| "model slot poisoned")?
            .as_ref()
        {
            return Ok(Arc::clone(loaded));
        }
        let file = models::model_file(&self.data, ModelId::Tween);
        if !self.installed() {
            crate::cutout::fetch(ModelId::Tween, &file, cancel, progress)?;
        }
        let tweener = Arc::new(Tweener::load(&file)?);
        *self.model.lock().map_err(|_| "model slot poisoned")? = Some(Arc::clone(&tweener));
        Ok(tweener)
    }

    /// Writes the copy `request` asks for, or finds it already written.
    /// Blocks until done, so run it on its own thread; `progress` is
    /// called as it goes. Returns where the copy is.
    pub fn tween(
        &self,
        request: &TweenRequest,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<PathBuf, String> {
        if request.target.is_file() {
            return Ok(request.target.clone());
        }
        let (start, duration) = request.span;
        if !start.is_finite() || !duration.is_finite() || duration <= 0.0 {
            return Err("nothing to make smooth".to_owned());
        }
        let job = self.gate.begin("tween")?;
        let cancel = job.cancel_handle();
        let tweener = if request.method.needs_model() {
            Some(self.loaded(&cancel, progress)?)
        } else {
            None
        };
        if let Some(folder) = request.target.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|error| format!("{}: {error}", folder.display()))?;
        }
        progress(Progress::Analysing(0.0));
        footage(tweener.as_deref(), request, &job, progress).map(|()| request.target.clone())
    }
}

/// The copy's encoder: FFmpeg's own MPEG-4, which every build carries and
/// which loads no driver, at near-transparent quality. A phone's H.264 chip
/// is not asked: at a hundred and more frames a second its rate control
/// starves each frame, and some chips refuse the rate outright. MPEG-4 needs
/// its rate to the whole frame - 95.904 is written as 96 - a thousandth
/// shorter, which over a clip's few seconds is a few milliseconds nobody
/// sees.
fn encoder_for(path: &Path, width: u32, height: u32, rate: FrameRate) -> Result<Encoder, String> {
    let whole = FrameRate::from_int(rate.fps().as_f64().round().clamp(1.0, MAX_RATE) as u32);
    Encoder::create(
        path,
        width,
        height,
        whole,
        &EncodeOptions {
            codec: VideoCodec::Mpeg4,
            preset: "medium".to_owned(),
            crf: 16,
            rate_mode: RateMode::Vbr,
            bitrate_kbps: 0,
            ten_bit: false,
            color_range: concat_media::ColorRange::Limited,
            hardware: false,
            threads: 2,
            software_only: true,
        },
    )
    .map_err(|error| error.to_string())
}

/// A frame's look in a few hundred pixels: what two frames are compared by
/// to tell motion from a cut.
fn glance(frame: &Frame) -> Vec<f32> {
    let small = concat_vision::enhance::area_resize(frame, 32, 18);
    small
        .pixels()
        .chunks_exact(concat_core::frame::BYTES_PER_PIXEL)
        .map(|pixel| {
            (0.2126 * f32::from(pixel[0])
                + 0.7152 * f32::from(pixel[1])
                + 0.0722 * f32::from(pixel[2]))
                / 255.0
        })
        .collect()
}

/// Whether two frames' glances are far enough apart to be a cut.
fn is_cut(a: &[f32], b: &[f32]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return true;
    }
    let mean = a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32;
    mean > CUT
}

/// The frames between `a` and `b`, `factor - 1` of them, by `method`: the
/// frame held where the two are the same picture or a cut apart.
fn between(
    tweener: Option<&Tweener>,
    method: Method,
    a: &Frame,
    b: &Frame,
    glances: (&[f32], &[f32]),
    factor: u32,
) -> Result<Vec<Frame>, String> {
    let times: Vec<f32> = (1..factor).map(|k| k as f32 / factor as f32).collect();
    if a.pixels() == b.pixels() || is_cut(glances.0, glances.1) {
        return Ok(times.iter().map(|_| a.clone()).collect());
    }
    match (method, tweener) {
        (Method::Motion, Some(tweener)) => tweener.between(a, b, &times),
        _ => Ok(times.iter().map(|&t| vision::blend(a, b, t)).collect()),
    }
}

/// The span read frame by frame and written denser, then the span's sound
/// beside it. Written under partial names and moved into place only when
/// whole, so a copy that is there is a copy that is complete.
fn footage(
    tweener: Option<&Tweener>,
    request: &TweenRequest,
    job: &crate::jobs::Job,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), String> {
    let info = concat_media::probe(&request.media_path).map_err(|error| error.to_string())?;
    let video = info
        .video
        .as_ref()
        .ok_or_else(|| "no video stream".to_owned())?;
    let rate = video.frame_rate;
    let fps = rate.fps().as_f64();
    let factor = effective_factor(request.factor, fps);
    let dense = FrameRate::new(rate.fps() * Rational::new(i64::from(factor), 1));
    let (start, duration) = request.span;
    let end = start + duration;
    // About how many of the file's frames the span holds, for the readout.
    let expected = (duration * fps).max(1.0);
    // Even sides, which the encoder wants: a stray pixel at the right or
    // bottom is cropped rather than the whole copy refused.
    let width = video.width & !1;
    let height = video.height & !1;
    if width == 0 || height == 0 {
        return Err("the picture is too small to make smooth".to_owned());
    }

    let partial_video = request.target.with_extension("part.mp4");
    let partial_muxed = request.target.with_extension("muxed.part.mp4");
    let options = DecodeOptions::default()
        .in_software()
        .threaded(2)
        .starting_at(Rational::approximate(start).unwrap_or(Rational::ZERO));
    let mut decoder =
        Decoder::open(&request.media_path, &options).map_err(|error| error.to_string())?;
    let mut encoder = encoder_for(&partial_video, width, height, dense)?;
    let started = std::time::Instant::now();
    let encoded = (|| {
        let mut read = 0u64;
        let mut written = 0u64;
        let mut model_time = std::time::Duration::ZERO;
        // The frame before, its glance, and whether it is the last one in
        // the span: each frame is written with the frames after it, once
        // the next one is known.
        let mut before: Option<(Frame, Vec<f32>)> = None;
        // FFmpeg's interpolation, for the classic method: a stream of the
        // file's frames in at its rate and the copy's out at its own.
        let mut classic = match request.method {
            Method::Classic => Some(
                concat_media::Stream::new(
                    &format!(
                        "minterpolate=fps={}/{}:mi_mode=mci:mc_mode=aobmc:me_mode=bidir:vsbmc=1:scd=fdiff",
                        dense.fps().numerator(),
                        dense.fps().denominator()
                    ),
                    width,
                    height,
                    (rate.fps().numerator(), rate.fps().denominator()),
                )
                .map_err(|error| error.to_string())?,
            ),
            _ => None,
        };
        let mut last_made: Option<Frame> = None;
        loop {
            if job.cancelled() {
                return Err("tween cancelled".to_owned());
            }
            let next = decoder.next_frame().map_err(|error| error.to_string())?;
            let past = match (&next, decoder.position()) {
                (None, _) => true,
                (Some(_), Some(at)) => at.as_f64() >= end - 1e-6,
                (Some(_), None) => read as f64 >= expected,
            };
            if let Some(stream) = classic.as_mut() {
                // FFmpeg makes the frames: every frame of the span goes in,
                // and the first past it too, for the last stretch; what
                // comes out up to the span's own count is the copy.
                let Some(frame) = next else { break };
                let frame = crate::enhance::crop(&frame, width, height);
                let made = stream
                    .push(&frame, read as i64)
                    .map_err(|error| error.to_string())?;
                if !(past && read > 0) {
                    read += 1;
                }
                let limit = read * u64::from(factor);
                for made in made {
                    if written < limit {
                        encoder
                            .write_frame(&made)
                            .map_err(|error| error.to_string())?;
                        written += 1;
                        last_made = Some(made);
                    }
                }
                progress(Progress::Analysing(
                    (read as f64 / expected).min(0.99) as f32
                ));
                if past && read > 0 {
                    break;
                }
                continue;
            }
            let next = next.map(|frame| crate::enhance::crop(&frame, width, height));
            if let Some((frame, look)) = before.take() {
                // The frame before, then what lies between it and this one:
                // made, or the frame held where the file ends here.
                encoder
                    .write_frame(&frame)
                    .map_err(|error| error.to_string())?;
                written += 1;
                let made = match &next {
                    Some(after) => {
                        let after_look = glance(after);
                        let begun = std::time::Instant::now();
                        let frames = between(
                            tweener,
                            request.method,
                            &frame,
                            after,
                            (&look, &after_look),
                            factor,
                        )?;
                        model_time += begun.elapsed();
                        frames
                    }
                    None => (1..factor).map(|_| frame.clone()).collect(),
                };
                for tween in &made {
                    if job.cancelled() {
                        return Err("tween cancelled".to_owned());
                    }
                    encoder
                        .write_frame(tween)
                        .map_err(|error| error.to_string())?;
                    written += 1;
                }
                progress(Progress::Analysing(
                    (read as f64 / expected).min(0.99) as f32
                ));
            }
            // The first frame past the span's end is read only to make the
            // frames before it; it is not part of the copy.
            let Some(frame) = next else { break };
            if past && read > 0 {
                break;
            }
            let look = glance(&frame);
            before = Some((frame, look));
            read += 1;
        }
        if let Some(stream) = classic.as_mut() {
            let limit = read * u64::from(factor);
            for made in stream.finish().map_err(|error| error.to_string())? {
                if written < limit {
                    encoder
                        .write_frame(&made)
                        .map_err(|error| error.to_string())?;
                    written += 1;
                    last_made = Some(made);
                }
            }
            // A copy as long as the span, whatever FFmpeg held back.
            while written < limit {
                let Some(held) = last_made.as_ref() else {
                    break;
                };
                encoder
                    .write_frame(held)
                    .map_err(|error| error.to_string())?;
                written += 1;
            }
        }
        log::info!(
            "tween: {read} frames of the file into {written} at {factor}x by {} in {:.1}s, {:.2}s a frame between",
            request.method.name(),
            started.elapsed().as_secs_f64(),
            model_time.as_secs_f64() / (read.max(1) * u64::from(factor - 1).max(1)) as f64
        );
        if written == 0 {
            return Err("the span holds no frames".to_owned());
        }
        encoder.finish().map_err(|error| error.to_string())
    })();
    // A finished encoder keeps its output open until it is dropped, and
    // Windows will not rename a file something holds.
    drop(encoder);
    drop(decoder);
    if let Err(error) = encoded {
        let _ = std::fs::remove_file(&partial_video);
        return Err(error);
    }
    let finished = if info.audio.is_some() {
        // The span's sound for the same seconds, so the copy's sound starts
        // where its picture does.
        let partial_audio = request.target.with_extension("part.m4a");
        let clip = concat_media::audio::AudioClip {
            path: PathBuf::from(&request.media_path),
            stream: None,
            start: 0.0,
            duration,
            source_start: start,
            speed: 1.0,
            tempo: Vec::new(),
            preserve_pitch: true,
            volume: 1.0,
            volume_curve: concat_core::animate::Track::new(Vec::new()),
            fade_in: 0.0,
            fade_out: 0.0,
            filter_chain: String::new(),
        };
        let muxed = concat_media::audio::mix_to_file(&[clip], duration, &partial_audio)
            .and_then(|()| concat_media::audio::mux(&partial_video, &partial_audio, &partial_muxed))
            .map_err(|error| error.to_string());
        let _ = std::fs::remove_file(&partial_audio);
        let _ = std::fs::remove_file(&partial_video);
        muxed.and_then(|()| {
            std::fs::rename(&partial_muxed, &request.target)
                .map_err(|error| format!("{}: {error}", request.target.display()))
        })
    } else {
        std::fs::rename(&partial_video, &request.target)
            .map_err(|error| format!("{}: {error}", request.target.display()))
    };
    if finished.is_err() {
        let _ = std::fs::remove_file(&partial_muxed);
    }
    finished
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_factor_is_two_to_thirty_two_and_never_past_the_rate_cap() {
        assert_eq!(factor_of(3), 2);
        assert_eq!(factor_of(4), 4);
        assert_eq!(factor_of(16), 16);
        assert_eq!(factor_of(100), 32);
        assert_eq!(effective_factor(8, 24.0), 8);
        assert_eq!(effective_factor(16, 25.0), 16);
        assert_eq!(effective_factor(32, 60.0), 32);
        assert_eq!(effective_factor(16, 240.0), 16);
        // Past MPEG-4's clock only: 4000 fps at 32 is 128000, so 16.
        assert_eq!(effective_factor(32, 4000.0), 16);
        assert_eq!(effective_factor(2, 100_000.0), 2);
    }

    #[test]
    fn a_method_is_kept_by_its_name() {
        for method in Method::ALL {
            assert_eq!(Method::parse(method.name()), method);
        }
        assert_eq!(Method::parse("something new"), Method::Motion);
        assert!(Method::Motion.needs_model());
        assert!(!Method::Blend.needs_model());
    }

    #[test]
    fn the_copy_is_named_after_the_file_the_span_and_the_settings() {
        let dir = std::env::temp_dir().join(format!("concat-tween-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let media = dir.join("clip.mp4");
        std::fs::write(&media, b"not really").expect("write");
        let media = media.to_string_lossy().into_owned();
        let target =
            |factor, method, span| target_for(&dir, &media, factor, method, span).expect("named");
        let first = target(4, Method::Motion, (1.0, 2.0));
        assert!(first.starts_with(dir.join("cache")));
        assert!(first.to_string_lossy().ends_with("-4x-ai.mp4"), "{first:?}");
        assert_eq!(first, target(4, Method::Motion, (1.0, 2.0)));
        assert_ne!(first, target(8, Method::Motion, (1.0, 2.0)));
        assert_ne!(first, target(4, Method::Blend, (1.0, 2.0)));
        assert_ne!(first, target(4, Method::Motion, (1.5, 2.0)));
        assert!(target_for(&dir, "/no/such/file.mp4", 4, Method::Motion, (0.0, 1.0)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cut_is_told_from_motion() {
        let flat = |v: f32| vec![v; 32 * 18];
        assert!(!is_cut(&flat(0.4), &flat(0.45)));
        assert!(is_cut(&flat(0.1), &flat(0.8)));
        assert!(is_cut(&flat(0.1), &[]));
    }

    /// A file of `frames` frames at `fps`, each a grey a step lighter than
    /// the one before, written by the encoder copies are written with.
    fn steps(dir: &Path, frames: u32, fps: u32) -> PathBuf {
        let path = dir.join("steps.mp4");
        let rate = FrameRate::from_int(fps);
        let mut encoder = encoder_for(&path, 64, 32, rate).expect("an encoder");
        for index in 0..frames {
            let v = (40 + index * 8).min(250) as u8;
            let pixels = (0..64 * 32).flat_map(|_| [v, v, v, 255]).collect();
            let frame = Frame::from_rgba(64, 32, pixels).expect("a frame");
            encoder.write_frame(&frame).expect("writes");
        }
        encoder.finish().expect("finishes");
        path
    }

    #[test]
    fn a_blended_copy_has_factor_times_the_frames_over_the_same_span() {
        let dir = std::env::temp_dir().join(format!("concat-tween-blend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let media = steps(&dir, 24, 24);
        let media = media.to_string_lossy().into_owned();
        let target = target_for(&dir, &media, 4, Method::Blend, (0.25, 0.5)).expect("named");
        let tweeners = Tweeners::new(&dir);
        let request = TweenRequest {
            media_path: media.clone(),
            factor: 4,
            method: Method::Blend,
            span: (0.25, 0.5),
            target: target.clone(),
        };
        let mut seen = Vec::new();
        let made = tweeners
            .tween(&request, &mut |progress| {
                if let Progress::Analysing(fraction) = progress {
                    seen.push(fraction);
                }
            })
            .expect("makes the copy");
        assert_eq!(made, target);
        assert!(target.is_file());
        assert!(!seen.is_empty());
        let info = concat_media::probe(&target).expect("probes");
        let video = info.video.expect("a picture");
        assert!(
            (video.frame_rate.fps().as_f64() - 96.0).abs() < 0.5,
            "{:?}",
            video.frame_rate
        );
        // Half a second at 24 frames is twelve frames, at four times 48.
        let mut decoder =
            Decoder::open(&target, &DecodeOptions::default().in_software()).expect("opens");
        let mut greys = Vec::new();
        while let Some(frame) = decoder.next_frame().expect("decodes") {
            greys.push(frame.pixels()[0]);
        }
        assert!((46..=50).contains(&greys.len()), "{} frames", greys.len());
        // The greys climb a quarter step a frame: never back, and the
        // frames between are between.
        assert!(
            greys.windows(2).all(|pair| pair[1] + 3 >= pair[0]),
            "{greys:?}"
        );
        assert!(greys[1] > greys[0] && greys[1] < greys[4], "{greys:?}");
        // Asked again, it is found rather than made.
        assert_eq!(
            tweeners.tween(&request, &mut |_| {}).expect("found"),
            target
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
