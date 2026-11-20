// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Proxies: a smaller copy of a large file, written once into the
//! project's cache, that playback and the filmstrips read instead of the
//! original.
//!
//! A 4K file decodes at a few frames a second on a laptop without
//! hardware decode; its quarter-size copy decodes at dozens. So a file
//! larger than HD gets one, written on the scheduler's proxy lane - one
//! at a time, on two threads, so the machine stays usable meanwhile - and
//! adopted into the reader pool as the file's stand-in. A request that
//! asks for the proxy reads it; the paused monitor and the export never
//! do, and see the original. The copy is named after the file's path,
//! size and modification time, so a file replaced on disk gets a fresh
//! one and the old copy is swept with the rest of the cache.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use concat_core::frame::Signal;
use concat_media::decode::{DecodeOptions, Decoder, FrameSource};
use concat_media::{ColorRange, EncodeOptions, Encoder, FrameSink, Priority, RateMode, VideoCodec};

/// A file with more pixels than this gets a proxy: anything larger than
/// full HD.
const PIXELS_WITHOUT_PROXY: u64 = 1920 * 1080;
/// Fork: with the fast preview on, the line is lower: every file with
/// more pixels than a quarter of full HD gets one, full HD included -
/// a phone scrubbing through a long film decodes the copy far quicker.
const PIXELS_WITHOUT_FAST_PROXY: u64 = 960 * 540;
/// The proxy's long side is the original's divided by this, and never
/// shorter than [`SHORTEST_LONG_SIDE`].
const DIVISOR: u32 = 4;
const SHORTEST_LONG_SIDE: u32 = 960;

/// Whether a `width` by `height` file is worth a proxy.
pub fn wanted(width: u32, height: u32) -> bool {
    u64::from(width) * u64::from(height) > PIXELS_WITHOUT_PROXY
}

/// Fork: whether a `width` by `height` file is worth a proxy for the fast
/// preview, which wants one for full HD too.
pub fn wanted_fast(width: u32, height: u32) -> bool {
    u64::from(width) * u64::from(height) > PIXELS_WITHOUT_FAST_PROXY
}

/// The proxy's size for a `width` by `height` original: the long side a
/// quarter of the original's, not under [`SHORTEST_LONG_SIDE`] and never
/// over the original, the aspect kept, both sides even.
pub fn size_for(width: u32, height: u32) -> (u32, u32) {
    let (width, height) = (width.max(2), height.max(2));
    let long = width.max(height);
    let target = (long / DIVISOR).max(SHORTEST_LONG_SIDE).min(long);
    let scale = f64::from(target) / f64::from(long);
    let side = |px: u32| (((f64::from(px) * scale).round() as u32) & !1).max(2);
    (side(width), side(height))
}

/// Where the proxy of `media` lives under `project`: named by the file's
/// path, size and modification time, and by the range it is read as,
/// since the copy is written from the corrected picture and a correction
/// changed is a different copy - and by whether the file is HDR, whose
/// proxy keeps its signal, so the tone-mapped copies earlier builds wrote
/// of one are written again. None for a file that cannot be stat'ed,
/// which cannot be read either.
pub fn path_for(
    project: &Path,
    media: &str,
    range: Option<ColorRange>,
    hdr: bool,
) -> Option<PathBuf> {
    let meta = std::fs::metadata(media).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    media.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    if let Some(range) = range {
        range.name().hash(&mut hasher);
    }
    if hdr {
        "hdr".hash(&mut hasher);
    }
    if let Ok(modified) = meta.modified()
        && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
    {
        since.as_secs().hash(&mut hasher);
    }
    Some(
        project
            .join("cache")
            .join("proxy")
            .join(format!("{:016x}.mp4", hasher.finish())),
    )
}

/// The proxy of `media` under `project`, read as `range`, when it has
/// been written - an HDR one or an SDR one, whichever there is: a
/// filmstrip reads either.
pub fn existing(project: &Path, media: &str, range: Option<ColorRange>) -> Option<PathBuf> {
    [true, false]
        .into_iter()
        .filter_map(|hdr| path_for(project, media, range, hdr))
        .find(|path| path.is_file())
}

/// Sees to it that `media` - `width` by `height`, read as `range` - has
/// a proxy when it is worth one (with `fast`, the fast preview's lower
/// line: see [`wanted_fast`]): an existing copy is adopted into the
/// reader pool at once; otherwise one is queued to be written on the
/// proxy lane and adopted when done. Asking again while it is being
/// written is free. Returns whether the file has, or will have, a proxy.
pub fn ensure(
    project: &Path,
    media: &str,
    width: u32,
    height: u32,
    range: Option<ColorRange>,
    hdr: bool,
    fast: bool,
) -> bool {
    let worth = if fast {
        wanted_fast(width, height)
    } else {
        wanted(width, height)
    };
    if !worth {
        return false;
    }
    let Some(target) = path_for(project, media, range, hdr) else {
        return false;
    };
    let pool = crate::scheduler().pool();
    if target.is_file() {
        if pool.proxy_of(Path::new(media)).as_deref() != Some(target.as_path()) {
            pool.adopt_proxy(Path::new(media), target);
        }
        return true;
    }
    let writing = writing();
    {
        let mut writing = writing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !writing.insert(target.clone()) {
            return true;
        }
    }
    let source = media.to_owned();
    let size = size_for(width, height);
    crate::scheduler().submit(Priority::Proxy, move || {
        match write(&source, &target, size, range) {
            Ok(()) => {
                crate::scheduler()
                    .pool()
                    .adopt_proxy(Path::new(&source), target.clone());
                log::info!("{source}: proxy written to {}", target.display());
            }
            Err(error) => log::warn!("{source}: no proxy: {error}"),
        }
        writing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&target);
    });
    true
}

/// The proxies being written now, by target, so one is never queued
/// twice and a sweep leaves it for its writer to finish.
fn writing() -> &'static Mutex<HashSet<PathBuf>> {
    static WRITING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    WRITING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Fork: the proxy being written this moment: its file, how many of its
/// frames are written and how many there are (0 when its length is not
/// known), and since when.
struct Writing {
    source: String,
    done: u64,
    total: u64,
    since: Instant,
}

fn current() -> &'static Mutex<Option<Writing>> {
    static CURRENT: OnceLock<Mutex<Option<Writing>>> = OnceLock::new();
    CURRENT.get_or_init(|| Mutex::new(None))
}

/// Fork: how the proxies are coming along, for a line on the screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// The file whose copy is being written, if one is under way yet.
    pub source: Option<String>,
    /// How far along it is, `0..=1`; 0 when its length is not known.
    pub fraction: f32,
    /// Seconds it has taken so far.
    pub elapsed: f32,
    /// Copies waiting their turn after it.
    pub waiting: usize,
}

/// Fork: the proxies being written and waiting, or none when there are
/// none.
pub fn progress() -> Option<Progress> {
    let queued = writing()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .len();
    if queued == 0 {
        return None;
    }
    let current = current()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Some(match current.as_ref() {
        Some(writing) => Progress {
            source: Some(writing.source.clone()),
            fraction: if writing.total > 0 {
                (writing.done as f64 / writing.total as f64).min(1.0) as f32
            } else {
                0.0
            },
            elapsed: writing.since.elapsed().as_secs_f32(),
            waiting: queued.saturating_sub(1),
        },
        None => Progress {
            source: None,
            fraction: 0.0,
            elapsed: 0.0,
            waiting: queued,
        },
    })
}

/// Removes the proxies under `project` that `keep` does not name: the
/// copies of media no longer in the project, or read as a range they no
/// longer are. Nothing else sweeps this folder, and a proxy is a
/// quarter-size copy of every file over HD the project ever held
/// (audit 2026-09-23, #15). One being written is left for its writer.
/// Returns how many files went.
pub fn sweep(project: &Path, keep: &HashSet<PathBuf>) -> usize {
    let dir = project.join("cache").join("proxy");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let writing = writing()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "mp4")
            && !keep.contains(&path)
            && !writing.contains(&path)
            && std::fs::remove_file(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Writes the proxy of `source`, read as `range`, at `size` to `target`:
/// the working codec (see [`VideoCodec::WORKING`]), two threads, in
/// software, into a temporary name
/// that is moved into place only when whole, so a copy that is there is a
/// copy that is complete. The copy is tagged video range and converted to
/// match, so it is read as it is and needs no correction of its own. An
/// HDR source's copy keeps its signal - HEVC in ten bits, HLG or PQ as the
/// source is, on the hardware encoder where there is one - so an HDR
/// timeline plays it as the original draws when paused.
pub fn write(
    source: &str,
    target: &Path,
    (width, height): (u32, u32),
    range: Option<ColorRange>,
) -> Result<(), String> {
    let info = concat_media::probe(source).map_err(|error| error.to_string())?;
    let (rate, signal) = info
        .video
        .as_ref()
        .map(|video| (video.frame_rate, video.signal))
        .ok_or_else(|| "no video stream".to_owned())?;
    let hdr = matches!(signal, Signal::Hlg | Signal::Pq);
    // Fork: the phone stays awake while a copy is written - a whole film's
    // takes a while - and the screen can say how far it has got.
    let _awake = crate::awake::Awake::hold();
    let total = info
        .duration
        .map_or(0, |duration| rate.frames_in(duration).max(0) as u64);
    *current()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Writing {
        source: source.to_owned(),
        done: 0,
        total,
        since: Instant::now(),
    });
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            *current()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }
    let _done = Done;
    if let Some(folder) = target.parent() {
        std::fs::create_dir_all(folder)
            .map_err(|error| format!("{}: {error}", folder.display()))?;
    }
    // Written beside the target under a name the muxer still knows the
    // format of, and moved into place only when whole.
    let partial = target.with_extension("part.mp4");
    // A phone has cores to spare for it: four of its eight decode, which
    // is what the copy waits on, and the chip encodes.
    let threads = if cfg!(target_os = "android") { 4 } else { 2 };
    let options = DecodeOptions::default()
        .scaled_to(width, height)
        .in_software()
        .threaded(threads)
        .in_range(range)
        .deep(hdr);
    let mut decoder = Decoder::open(source, &options).map_err(|error| error.to_string())?;
    let encoding = EncodeOptions {
        codec: if hdr {
            VideoCodec::Hevc
        } else {
            VideoCodec::WORKING
        },
        preset: "veryfast".to_owned(),
        crf: 23,
        // A proxy is a working copy: the CRF says how good, and no
        // bitrate target ever applies to it.
        rate_mode: RateMode::Vbr,
        bitrate_kbps: 0,
        ten_bit: false,
        color_range: ColorRange::Limited,
        // HEVC in ten bits costs the CPU far more than H.264 does; the
        // media engine does it for nothing where there is one.
        hardware: hdr,
        threads: 2,
        software_only: false,
    };
    let mut encoder = if hdr {
        Encoder::create_hdr(&partial, width, height, rate, &encoding, signal)
    } else {
        Encoder::create(&partial, width, height, rate, &encoding)
    }
    .map_err(|error| error.to_string())?;
    let result = (|| {
        let mut done: u64 = 0;
        while let Some(frame) = decoder.next_frame().map_err(|error| error.to_string())? {
            encoder
                .write_frame(&frame)
                .map_err(|error| error.to_string())?;
            done += 1;
            if done.is_multiple_of(24)
                && let Some(writing) = current()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
            {
                writing.done = done;
            }
        }
        encoder.finish().map_err(|error| error.to_string())
    })();
    // Windows will not rename or delete a file something still holds open,
    // and a finished encoder keeps its output open until it is dropped.
    drop(encoder);
    drop(decoder);
    match result {
        Ok(()) => std::fs::rename(&partial, target)
            .map_err(|error| format!("{}: {error}", target.display())),
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the fast preview, a full HD file gets its copy too - half its
    /// size - and the copy's progress shows while it is written and is
    /// gone once it is done.
    #[test]
    fn a_full_hd_file_gets_a_fast_preview_copy_and_its_progress() {
        use concat_core::frame::Frame;
        use concat_core::time::FrameRate;
        let dir = std::env::temp_dir().join(format!("concat-proxy-fast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let media = dir.join("film.mp4");
        {
            let mut encoder = Encoder::create(
                &media,
                1920,
                1080,
                FrameRate::from_int(24),
                &EncodeOptions {
                    codec: VideoCodec::Mpeg4,
                    hardware: false,
                    ..EncodeOptions::default()
                },
            )
            .expect("encodes");
            for index in 0..48u8 {
                let mut frame = Frame::black(1920, 1080);
                frame.fill([index * 5, 20, 200, 255]);
                encoder.write_frame(&frame).expect("writes");
            }
            encoder.finish().expect("finishes");
        }
        let project = dir.join("project");
        let source = media.to_string_lossy().into_owned();
        assert!(
            !ensure(&project, &source, 1920, 1080, None, false, false),
            "not without the fast preview"
        );
        assert!(ensure(&project, &source, 1920, 1080, None, false, true));
        assert!(progress().is_some(), "said while it is on its way");
        crate::scheduler().drain();
        assert_eq!(progress(), None, "and gone once it is done");
        let proxy = existing(&project, &source, None).expect("the copy was written");
        let video = concat_media::probe(&proxy)
            .expect("probes")
            .video
            .expect("has pictures");
        assert_eq!((video.width, video.height), (960, 540));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fast preview's line takes in full HD and the 1480 by 1080 of a
    /// film kept in its full frame, but not a small file.
    #[test]
    fn the_fast_preview_wants_copies_of_full_hd_too() {
        assert!(!wanted(1920, 1080));
        assert!(wanted_fast(1920, 1080));
        assert!(wanted_fast(1480, 1080));
        assert!(!wanted_fast(960, 540));
        assert!(!wanted_fast(640, 360));
        assert_eq!(size_for(1920, 1080), (960, 540));
        assert_eq!(size_for(1480, 1080), (960, 700));
    }

    /// An HDR file's proxy keeps its signal: HEVC, HLG as the file is,
    /// its grey coming back as the same signal - under a name of its own,
    /// so the tone-mapped copy an earlier build wrote is written again.
    #[test]
    fn an_hdr_file_gets_an_hdr_proxy() {
        use concat_core::frame::{Depth, Frame};
        use concat_core::time::FrameRate;
        if !VideoCodec::Hevc.available() {
            eprintln!("no HEVC encoder in the linked FFmpeg; skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("concat-proxy-hdr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let media = dir.join("iphone.mp4");
        let value = (0.6f64 * 65_535.0).round() as u16;
        {
            let encoder = Encoder::create_hdr(
                &media,
                640,
                360,
                FrameRate::THIRTY,
                &EncodeOptions {
                    codec: VideoCodec::Hevc,
                    preset: "ultrafast".to_owned(),
                    hardware: false,
                    ..EncodeOptions::default()
                },
                Signal::Hlg,
            );
            let mut encoder = match encoder {
                Ok(encoder) => encoder,
                Err(error) if VideoCodec::Hevc.ten_bit_only_on_hardware() => {
                    eprintln!("no ten-bit HEVC encoder opens here ({error}); skipped");
                    return;
                }
                Err(error) => panic!("encodes: {error}"),
            };
            let pixel: Vec<u8> = [value, value, value, u16::MAX]
                .iter()
                .flat_map(|channel| channel.to_le_bytes())
                .collect();
            let frame = Frame::from_rgba64(640, 360, pixel.repeat(640 * 360), Signal::Hlg)
                .expect("a deep frame");
            for _ in 0..6 {
                encoder.write_frame(&frame).expect("writes");
            }
            encoder.finish().expect("finishes");
        }
        let source = media.to_string_lossy().into_owned();
        let project = dir.join("project");
        let target = path_for(&project, &source, None, true).expect("a path");
        assert_ne!(
            Some(&target),
            path_for(&project, &source, None, false).as_ref()
        );
        write(&source, &target, (320, 180), None).expect("writes the proxy");
        let info = concat_media::probe(&target).expect("probes");
        let video = info.video.expect("has pictures");
        assert_eq!((video.codec.as_str(), video.signal), ("hevc", Signal::Hlg));
        let mut decoder =
            Decoder::open(&target, &DecodeOptions::default().deep(true)).expect("opens");
        let frame = decoder.next_frame().expect("decodes").expect("a frame");
        assert_eq!(frame.depth(), Depth::Sixteen);
        let at = ((90 * 320 + 160) * 8) as usize;
        let got = u16::from_le_bytes([frame.pixels()[at], frame.pixels()[at + 1]]);
        assert!(
            got.abs_diff(value) < 1_000,
            "the HLG grey {value} came back {got}"
        );
        assert_eq!(existing(&project, &source, None), Some(target));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_files_larger_than_hd_get_a_proxy() {
        assert!(!wanted(1920, 1080));
        assert!(!wanted(1280, 720));
        assert!(wanted(3840, 2160));
        assert!(wanted(2560, 1440));
        assert!(!wanted(0, 0));
    }

    #[test]
    fn a_proxy_is_a_quarter_of_the_long_side_but_never_tiny_or_larger() {
        assert_eq!(size_for(3840, 2160), (960, 540));
        assert_eq!(size_for(7680, 4320), (1920, 1080));
        // A tall file keeps its way up.
        assert_eq!(size_for(2160, 3840), (540, 960));
        // A file too small for a quarter gets the floor, never upscaled.
        assert_eq!(size_for(2560, 1440), (960, 540));
        assert_eq!(size_for(640, 480), (640, 480));
        // Odd sizes come out even, and nothing is ever under two.
        let (w, h) = size_for(3841, 2161);
        assert!(w % 2 == 0 && h % 2 == 0);
        assert_eq!(size_for(1, 1), (2, 2));
    }

    /// A proxy written from a file larger than HD is a quarter of it, is
    /// adopted by the pool, and serves a moving request while the paused
    /// one still reads the original.
    #[test]
    fn a_written_proxy_is_read_for_a_moving_picture_only() {
        use concat_core::frame::Frame;
        use concat_core::time::FrameRate;
        let dir = std::env::temp_dir().join(format!("concat-proxy-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let media = dir.join("big.mp4");
        {
            let mut encoder = Encoder::create(
                &media,
                2560,
                1440,
                FrameRate::THIRTY,
                &EncodeOptions {
                    preset: "ultrafast".to_owned(),
                    hardware: false,
                    ..EncodeOptions::default()
                },
            )
            .expect("encodes");
            for index in 0..6u8 {
                let mut frame = Frame::black(2560, 1440);
                frame.fill([index * 40, 20, 200, 255]);
                encoder.write_frame(&frame).expect("writes");
            }
            encoder.finish().expect("finishes");
        }
        let project = dir.join("project");
        let source = media.to_string_lossy().into_owned();
        assert!(
            ensure(&project, &source, 2560, 1440, None, false, false),
            "worth a proxy"
        );
        crate::scheduler().drain();
        let proxy = existing(&project, &source, None).expect("the proxy was written");
        let info = concat_media::probe(&proxy).expect("probes");
        let video = info.video.expect("has pictures");
        assert_eq!((video.width, video.height), (960, 540));
        let pool = crate::scheduler().pool();
        assert_eq!(
            pool.proxy_of(Path::new(&source)).as_deref(),
            Some(proxy.as_path())
        );
        // Second time round: nothing to write, still adopted.
        assert!(ensure(&project, &source, 2560, 1440, None, false, false));
        let time = FrameRate::THIRTY.time_of_frame(2);
        let moving = pool
            .frame(&concat_media::FrameRequest::new(&source, time, 480, 270).from_proxy(true))
            .expect("reads the proxy");
        assert_eq!((moving.width(), moving.height()), (480, 270));
        let paused = pool
            .frame(&concat_media::FrameRequest::new(&source, time, 480, 270))
            .expect("reads the original");
        assert_eq!((paused.width(), paused.height()), (480, 270));
        // A file not worth a proxy is left alone.
        assert!(!ensure(&project, &source, 1280, 720, None, false, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_proxy_path_follows_the_file_and_changes_when_the_file_does() {
        let dir = std::env::temp_dir().join(format!("concat-proxy-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let media = dir.join("clip.mp4");
        std::fs::write(&media, b"one").expect("writes");
        let project = dir.join("project");
        let first = path_for(&project, &media.to_string_lossy(), None, false).expect("a path");
        assert!(first.starts_with(project.join("cache").join("proxy")));
        assert!(existing(&project, &media.to_string_lossy(), None).is_none());
        // Read as another range, the file is another proxy too: the copy
        // holds the corrected picture.
        let full = path_for(
            &project,
            &media.to_string_lossy(),
            Some(ColorRange::Full),
            false,
        )
        .expect("a path");
        assert_ne!(first, full, "a file read as full range is another proxy");
        std::fs::write(&media, b"one more byte").expect("writes");
        let second = path_for(&project, &media.to_string_lossy(), None, false).expect("a path");
        assert_ne!(first, second, "a changed file is another proxy");
        assert!(
            path_for(
                &project,
                &dir.join("missing.mp4").to_string_lossy(),
                None,
                false
            )
            .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sweep_keeps_what_is_named_and_what_is_being_written() {
        let scratch =
            std::env::temp_dir().join(format!("concat-proxy-sweep-{}", std::process::id()));
        let dir = scratch.join("cache").join("proxy");
        std::fs::create_dir_all(&dir).expect("a proxy folder");
        let kept = dir.join("1111111111111111.mp4");
        let stale = dir.join("2222222222222222.mp4");
        let busy = dir.join("3333333333333333.mp4");
        let partial = dir.join("4444444444444444.mp4.part");
        for file in [&kept, &stale, &busy, &partial] {
            std::fs::write(file, b"x").expect("writes");
        }
        writing()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(busy.clone());
        let keep: HashSet<PathBuf> = [kept.clone()].into_iter().collect();
        assert_eq!(sweep(&scratch, &keep), 1);
        assert!(kept.is_file() && busy.is_file() && partial.is_file());
        assert!(!stale.is_file());
        writing()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&busy);
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
