// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: a film's scenes on disk, beside the film.
//!
//! The list is PySceneDetect's own CSV, `<film>-Scenes.csv`, so a list
//! made here opens wherever `scenedetect list-scenes` output does, and one
//! made there - or by the Szenenschnitt apps, in their `<film>-szenen`
//! folder - opens here. The Szenenschnitt phone app's own scan, its
//! measurements of every frame (`szenen.dat`), is read too, and cut with
//! the settings it was last shown with: the same scenes, numbered the
//! same, as the app shows and its clips are named after. The pictures of every scene go into one ZIP,
//! `<film>-Szenen.zip`, stored rather than compressed (a JPEG does not
//! shrink): a film of a thousand scenes is three thousand pictures, and an
//! SD card is no place for them loose. The ZIP carries the cuts it was
//! made for (`concat-szenen.json`), so pictures of an older list are never
//! shown against a newer one.
//!
//! Where the film's folder takes no files, both go to the app's own data
//! folder instead, under a name made of the film's path.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

/// The manifest's name inside a pictures ZIP.
const MANIFEST: &str = "concat-szenen.json";

/// A film's scenes: where each starts, in frames, and where the last ends.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneList {
    /// The first frame of every scene, rising.
    pub starts: Vec<u64>,
    /// One past the last frame of the last scene.
    pub end: u64,
    /// Frames a second the frames are counted at.
    pub fps: f64,
}

impl SceneList {
    /// The scenes a detection found: one from the first frame to each cut,
    /// and the last to `frames`.
    pub fn from_cuts(cuts: &[u64], frames: u64, fps: f64) -> Self {
        let mut starts = Vec::with_capacity(cuts.len() + 1);
        starts.push(0);
        starts.extend(cuts.iter().copied().filter(|&cut| cut > 0 && cut < frames));
        starts.dedup();
        Self {
            starts,
            end: frames.max(1),
            fps,
        }
    }

    /// How many scenes there are.
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Scene `index`, from 0: its first frame and one past its last.
    pub fn scene(&self, index: usize) -> (u64, u64) {
        let start = self.starts[index];
        let end = self.starts.get(index + 1).copied().unwrap_or(self.end);
        (start, end.max(start + 1))
    }

    /// The frames the scenes after the first start at.
    pub fn cuts(&self) -> &[u64] {
        self.starts.get(1..).unwrap_or(&[])
    }

    /// Frame `frame` in seconds from the film's first frame.
    pub fn seconds(&self, frame: u64) -> f64 {
        frame as f64 / self.fps
    }
}

/// The settings a list was found with, kept beside its pictures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Frames a second the list counts.
    pub fps: f64,
    /// Every frame of the film.
    pub frames: u64,
    /// The scenes' starts after the first: what the pictures are of.
    pub cuts: Vec<u64>,
    /// The detection's threshold, or none for a list made elsewhere.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// The shortest scene in seconds, likewise.
    #[serde(default)]
    pub min_seconds: Option<f64>,
}

// ─── where things lie ─────────────────────────────────────────────────────

/// The film's name without its extension, as the files beside it begin.
pub fn stem(film: &Path) -> String {
    film.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "film".to_owned())
}

/// The list's place beside the film.
pub fn csv_beside(film: &Path) -> PathBuf {
    film.with_file_name(format!("{}-Scenes.csv", stem(film)))
}

/// The pictures' place beside the film.
pub fn zip_beside(film: &Path) -> PathBuf {
    film.with_file_name(format!("{}-Szenen.zip", stem(film)))
}

/// Where the list and pictures go when the film's folder takes no files:
/// a folder of the app's own, named for the film and its path.
pub fn fallback_dir(film: &Path) -> Option<PathBuf> {
    let dirs = crate::dirs::AppDirs::locate().ok()?;
    // FNV-1a over the path: the same folder for the same film every run.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in film.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(
        dirs.data
            .join("scenes")
            .join(format!("{}-{hash:016x}", stem(film))),
    )
}

/// The Szenenschnitt apps' folders for the film that are there:
/// `<film>-szenen` beside it, and the phone app's own under Movies, where
/// it goes when the film's folder takes no files.
fn szenenschnitt_dirs(film: &Path) -> Vec<PathBuf> {
    let name = format!("{}-szenen", stem(film));
    let mut dirs = vec![film.with_file_name(&name)];
    if cfg!(target_os = "android") {
        dirs.push(Path::new("/storage/emulated/0/Movies/Szenenschnitt").join(&name));
    }
    dirs.retain(|dir| dir.is_dir());
    dirs
}

/// Every place a list of the film's scenes may lie: beside it, in the
/// Szenenschnitt folders (with the phone app's export for the PC and its
/// own scan, `szenen.dat`), and in the app's own folder.
pub fn list_places(film: &Path) -> Vec<PathBuf> {
    let name = format!("{}-Scenes.csv", stem(film));
    let mut places = vec![csv_beside(film)];
    for szenen in szenenschnitt_dirs(film) {
        places.push(szenen.join(&name));
        places.push(szenen.join("pc").join(&name));
        places.push(szenen.join(SCAN));
    }
    if let Some(dir) = fallback_dir(film) {
        places.push(dir.join(&name));
    }
    places
}

/// The newest list of the film's scenes there is, and where it lies.
pub fn find_list(film: &Path) -> Option<(PathBuf, SceneList)> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = list_places(film)
        .into_iter()
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().find_map(|(_, path)| {
        let read = if path.file_name().is_some_and(|name| name == SCAN) {
            read_szenenschnitt(path.parent().unwrap_or(Path::new(".")))
        } else {
            std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| read_csv(&text))
        };
        match read {
            Ok(list) => Some((path, list)),
            Err(error) => {
                log::warn!("scenes: {} is no scene list: {error}", path.display());
                None
            }
        }
    })
}

// ─── the Szenenschnitt phone app's scan ───────────────────────────────────

/// The phone app's measurements of every frame.
const SCAN: &str = "szenen.dat";

/// How the phone app cuts its measurements: `detect` in its project file.
struct Knobs {
    adaptive: bool,
    threshold: f64,
    min_seconds: f64,
    suppress: bool,
    weights: [f64; 3],
    adaptive_threshold: f64,
    min_content: f64,
    window: usize,
}

impl Knobs {
    /// The project file's `detect`, each knob its default where missing.
    fn of(detect: Option<&serde_json::Value>) -> Self {
        let number = |key: &str, default: f64| {
            detect
                .and_then(|detect| detect.get(key))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(default)
        };
        let word = |key: &str| {
            detect
                .and_then(|detect| detect.get(key))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            adaptive: word("mode") == "adaptive",
            threshold: number("threshold", 27.0),
            min_seconds: number("min_len", 0.6),
            suppress: word("filter") == "suppress",
            weights: [number("wh", 1.0), number("ws", 1.0), number("wl", 1.0)],
            adaptive_threshold: number("adaptive_threshold", 3.0),
            min_content: number("min_content", 15.0),
            window: number("window", 2.0).max(1.0) as usize,
        }
    }
}

/// The phone app's scan in `dir` as the list it shows: every frame's
/// measurements (`szenen.dat`: a 128-byte head with the frame rate, then
/// 24 bytes a frame - its time, then the hue's, saturation's and value's
/// change and its brightness as floats) cut with the settings in its
/// project file, the way the app cuts them.
fn read_szenenschnitt(dir: &Path) -> Result<SceneList, String> {
    const HEAD: usize = 128;
    const RECORD: usize = 24;
    let data = std::fs::read(dir.join(SCAN)).map_err(|error| error.to_string())?;
    if data.len() < HEAD || !data.starts_with(b"SZS") {
        return Err("not a Szenenschnitt scan".to_owned());
    }
    let int = |at: usize| i32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let float = |at: usize| {
        f64::from(f32::from_le_bytes([
            data[at],
            data[at + 1],
            data[at + 2],
            data[at + 3],
        ]))
    };
    let (num, den) = (int(8), int(12).max(1));
    let fps = if num > 0 {
        f64::from(num) / f64::from(den)
    } else {
        25.0
    };
    let frames = (data.len() - HEAD) / RECORD;
    if frames == 0 {
        return Err("an empty scan".to_owned());
    }
    let project: Option<serde_json::Value> =
        std::fs::read_to_string(dir.join("szenenschnitt-handy.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
    let knobs = Knobs::of(project.as_ref().and_then(|project| project.get("detect")));
    let sum: f64 = knobs.weights.iter().map(|weight| weight.abs()).sum();
    let scores: Vec<f64> = (0..frames)
        .map(|frame| {
            if frame == 0 || sum <= 0.0 {
                return 0.0;
            }
            let at = HEAD + frame * RECORD + 8;
            (float(at) * knobs.weights[0]
                + float(at + 4) * knobs.weights[1]
                + float(at + 8) * knobs.weights[2])
                / sum
        })
        .collect();
    let min_len = concat_media::scenes::Settings {
        threshold: knobs.threshold,
        min_seconds: knobs.min_seconds,
        merge: !knobs.suppress,
    }
    .min_frames(fps);
    let cuts = if knobs.adaptive {
        adaptive_cuts(&scores, &knobs, min_len)
    } else {
        let mut filter = concat_media::scenes::Filter::new(min_len, !knobs.suppress);
        scores
            .iter()
            .enumerate()
            .filter_map(|(frame, &score)| filter.feed(frame as u64, score >= knobs.threshold))
            .filter(|&cut| cut > 0)
            .collect()
    };
    Ok(SceneList::from_cuts(&cuts, frames as u64, fps))
}

/// PySceneDetect's adaptive detector over the scores, as the phone app
/// runs it: a frame cuts where its score stands out from the `window`
/// frames either side of it by `adaptive_threshold` times, and is at least
/// `min_content` itself.
fn adaptive_cuts(scores: &[f64], knobs: &Knobs, min_len: u64) -> Vec<u64> {
    let window = knobs.window;
    let need = 2 * window + 1;
    let mut cuts = Vec::new();
    let mut last_cut = 0usize;
    for frame in need.saturating_sub(1)..scores.len() {
        let target = frame - window;
        let target_score = scores[target];
        let around: f64 = (frame + 1 - need..=frame)
            .filter(|&at| at != target)
            .map(|at| scores[at])
            .sum::<f64>()
            / (2 * window) as f64;
        let ratio = if around.abs() >= 0.00001 {
            (target_score / around).min(255.0)
        } else if target_score >= knobs.min_content {
            255.0
        } else {
            0.0
        };
        if ratio >= knobs.adaptive_threshold
            && target_score >= knobs.min_content
            && (target - last_cut) as u64 >= min_len
        {
            last_cut = target;
            cuts.push(target as u64);
        }
    }
    cuts
}

// ─── the list ─────────────────────────────────────────────────────────────

/// A time as PySceneDetect writes one: `HH:MM:SS.mmm`.
fn timecode(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let hours = (seconds / 3600.0) as u64;
    let rest = seconds - hours as f64 * 3600.0;
    let minutes = (rest / 60.0) as u64;
    let mut secs = (rest - minutes as f64 * 60.0).max(0.0);
    secs = (secs * 1000.0).round() / 1000.0;
    let (mut hours, mut minutes) = (hours, minutes);
    if secs >= 60.0 {
        secs = 0.0;
        minutes += 1;
        if minutes >= 60 {
            minutes = 0;
            hours += 1;
        }
    }
    format!("{hours:02}:{minutes:02}:{secs:06.3}")
}

/// The list as `scenedetect list-scenes` writes it.
pub fn csv_text(list: &SceneList) -> String {
    let mut text = String::from("Timecode List:");
    for &cut in list.cuts() {
        text.push(',');
        text.push_str(&timecode(list.seconds(cut)));
    }
    text.push('\n');
    text.push_str(
        "Scene Number,Start Frame,Start Timecode,Start Time (seconds),End Frame,End Timecode,\
         End Time (seconds),Length (frames),Length (timecode),Length (seconds)\n",
    );
    for index in 0..list.len() {
        let (start, end) = list.scene(index);
        let (a, b, length) = (
            list.seconds(start),
            list.seconds(end),
            list.seconds(end - start),
        );
        text.push_str(&format!(
            "{},{},{},{a:.3},{end},{},{b:.3},{},{},{length:.3}\n",
            index + 1,
            start + 1,
            timecode(a),
            timecode(b),
            end - start,
            timecode(length),
        ));
    }
    text
}

/// Reads a scene list in PySceneDetect's CSV: the scenes' frames, by the
/// header's own column names, and the frame rate the times were counted
/// at.
pub fn read_csv(text: &str) -> Result<SceneList, String> {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    let header = loop {
        let Some(line) = lines.next() else {
            return Err("no header".to_owned());
        };
        if line.starts_with("Scene Number") {
            break line;
        }
    };
    let columns: Vec<&str> = header.split(',').map(str::trim).collect();
    let column = |name: &str| columns.iter().position(|&column| column == name);
    let start_frame = column("Start Frame").ok_or("no Start Frame column")?;
    let end_frame = column("End Frame").ok_or("no End Frame column")?;
    let start_time = column("Start Time (seconds)");
    let mut starts = Vec::new();
    let mut end = 0u64;
    // The frame rate from the last scene's start: the most digits there are.
    let mut rate = None;
    for line in lines {
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        let Some(first) = cells
            .get(start_frame)
            .and_then(|cell| cell.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(last) = cells
            .get(end_frame)
            .and_then(|cell| cell.parse::<u64>().ok())
        else {
            continue;
        };
        let start = first.saturating_sub(1);
        if starts.last().is_some_and(|&before| start <= before) {
            continue;
        }
        if let Some(seconds) = start_time
            .and_then(|at| cells.get(at))
            .and_then(|cell| cell.parse::<f64>().ok())
            && start > 0
            && seconds > 0.0
        {
            rate = Some(start as f64 / seconds);
        }
        starts.push(start);
        end = end.max(last);
    }
    if starts.is_empty() {
        return Err("no scenes".to_owned());
    }
    let end = end.max(starts.last().copied().unwrap_or(0) + 1);
    Ok(SceneList {
        starts,
        end,
        fps: rate.unwrap_or(0.0),
    })
}

/// Writes `text` to `path` whole or not at all: to a file beside it
/// first, renamed over it once written.
fn write_whole(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let part = part_of(path);
    let written = (|| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&part)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&part, path)
    })();
    written.map_err(|error| {
        let _ = std::fs::remove_file(&part);
        format!("{}: {error}", path.display())
    })
}

/// The name a file is written under until it is whole.
fn part_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// Writes the list to `path`.
pub fn write_csv(path: &Path, list: &SceneList) -> Result<(), String> {
    write_whole(path, csv_text(list).as_bytes())
}

// ─── the pictures ─────────────────────────────────────────────────────────

/// A picture's name in the ZIP, as Szenenschnitt names its export:
/// `<film>-Scene-007-02.jpg`.
fn picture_name(stem: &str, scene: usize, index: u8) -> String {
    format!("{stem}-Scene-{scene:03}-{index:02}.jpg")
}

/// The scene and picture a ZIP entry's name says it is.
fn picture_of(name: &str) -> Option<(usize, u8)> {
    let rest = name.strip_suffix(".jpg")?;
    let at = rest.rfind("-Scene-")?;
    let (scene, index) = rest[at + "-Scene-".len()..].split_once('-')?;
    Some((scene.parse().ok()?, index.parse().ok()?))
}

/// Pictures of a film's scenes, from wherever they lie.
pub struct Pictures {
    kind: Kind,
    /// Where they lie, for saying so.
    pub source: PathBuf,
}

enum Kind {
    /// A ZIP made here: each scene's three pictures, by name.
    Zip {
        archive: Mutex<zip::ZipArchive<BufReader<File>>>,
        entries: HashMap<(usize, u8), usize>,
    },
    /// The Szenenschnitt app's folder of frames, `f0001234.jpg` frame 1234
    /// counted from 0: a scene shows the one nearest its middle.
    Frames { dir: PathBuf, frames: Vec<u64> },
}

impl Pictures {
    /// The pictures of `list`'s scenes there are: a ZIP made for exactly
    /// this list, beside the film or in the app's folder, else the
    /// Szenenschnitt app's frames.
    pub fn open(film: &Path, list: &SceneList) -> Option<Pictures> {
        let mut zips = vec![zip_beside(film)];
        if let Some(dir) = fallback_dir(film) {
            zips.push(dir.join(format!("{}-Szenen.zip", stem(film))));
        }
        for path in zips {
            if let Some(pictures) = Self::open_zip(&path, list) {
                return Some(pictures);
            }
        }
        szenenschnitt_dirs(film).into_iter().find_map(|szenen| {
            let dir = szenen.join("bilder");
            let mut frames: Vec<u64> = std::fs::read_dir(&dir)
                .ok()?
                .filter_map(|entry| {
                    let name = entry.ok()?.file_name();
                    let name = name.to_str()?;
                    name.strip_prefix('f')?.strip_suffix(".jpg")?.parse().ok()
                })
                .collect();
            if frames.is_empty() {
                return None;
            }
            frames.sort_unstable();
            Some(Pictures {
                source: dir.clone(),
                kind: Kind::Frames { dir, frames },
            })
        })
    }

    fn open_zip(path: &Path, list: &SceneList) -> Option<Pictures> {
        let file = File::open(path).ok()?;
        let mut archive = zip::ZipArchive::new(BufReader::new(file)).ok()?;
        let manifest: Manifest = {
            let mut entry = archive.by_name(MANIFEST).ok()?;
            let mut text = String::new();
            entry.read_to_string(&mut text).ok()?;
            serde_json::from_str(&text).ok()?
        };
        if manifest.cuts != list.cuts() {
            log::info!(
                "scenes: the pictures in {} are of another list",
                path.display()
            );
            return None;
        }
        let entries = (0..archive.len())
            .filter_map(|at| {
                let name = archive.name_for_index(at)?.ok()?;
                Some((picture_of(&name)?, at))
            })
            .collect();
        Some(Pictures {
            source: path.to_path_buf(),
            kind: Kind::Zip {
                archive: Mutex::new(archive),
                entries,
            },
        })
    }

    /// Picture `index` (1 the start, 2 the middle, 3 the end) of scene
    /// `scene` of `list`, counted from 0, as JPEG.
    pub fn read(&self, list: &SceneList, scene: usize, index: u8) -> Option<Vec<u8>> {
        match &self.kind {
            Kind::Zip { archive, entries } => {
                let at = *entries.get(&(scene + 1, index))?;
                let mut archive = archive
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut entry = archive.by_index(at).ok()?;
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut bytes).ok()?;
                Some(bytes)
            }
            Kind::Frames { dir, frames } => {
                let (start, end) = list.scene(scene);
                let want = match index {
                    1 => start + 1,
                    3 => end.saturating_sub(1),
                    _ => start + (end - start) / 2,
                };
                let at = frames.partition_point(|&frame| frame < want);
                let near = [at.checked_sub(1), Some(at)]
                    .into_iter()
                    .flatten()
                    .filter_map(|at| frames.get(at).copied())
                    .filter(|&frame| frame >= start && frame < end)
                    .min_by_key(|&frame| frame.abs_diff(want))?;
                std::fs::read(dir.join(format!("f{near:07}.jpg"))).ok()
            }
        }
    }

    /// Whether these are pictures made here, three a scene, rather than
    /// borrowed frames.
    pub fn complete(&self) -> bool {
        matches!(self.kind, Kind::Zip { .. })
    }
}

/// Writes the pictures of a film's scenes into a ZIP as they are made,
/// under a name of its own until [`PictureWriter::finish`].
pub struct PictureWriter {
    zip: zip::ZipWriter<BufWriter<File>>,
    part: PathBuf,
    path: PathBuf,
    stem: String,
}

impl PictureWriter {
    /// A writer of the ZIP that will be `path`, of the film `film`.
    pub fn create(path: &Path, film: &Path) -> Result<Self, String> {
        let part = part_of(path);
        let file = (|| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            File::create(&part)
        })()
        .map_err(|error| format!("{}: {error}", part.display()))?;
        Ok(Self {
            zip: zip::ZipWriter::new(BufWriter::with_capacity(1 << 20, file)),
            part,
            path: path.to_path_buf(),
            stem: stem(film),
        })
    }

    /// Adds picture `index` of scene `scene` (from 1), a JPEG.
    pub fn add(&mut self, scene: usize, index: u8, jpeg: &[u8]) -> Result<(), String> {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        self.zip
            .start_file(picture_name(&self.stem, scene, index), options)
            .map_err(|error| format!("{}: {error}", self.part.display()))?;
        self.zip
            .write_all(jpeg)
            .map_err(|error| format!("{}: {error}", self.part.display()))
    }

    /// Ends the ZIP with its manifest and puts it in its place.
    pub fn finish(mut self, manifest: &Manifest) -> Result<PathBuf, String> {
        let fail = |error: &dyn std::fmt::Display, part: &Path| {
            let _ = std::fs::remove_file(part);
            format!("{}: {error}", part.display())
        };
        let text = serde_json::to_string(manifest).map_err(|error| fail(&error, &self.part))?;
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        if let Err(error) = self.zip.start_file(MANIFEST, options) {
            return Err(fail(&error, &self.part));
        }
        if let Err(error) = self.zip.write_all(text.as_bytes()) {
            return Err(fail(&error, &self.part));
        }
        let mut file = self
            .zip
            .finish()
            .map_err(|error| fail(&error, &self.part))?;
        file.flush().map_err(|error| fail(&error, &self.part))?;
        drop(file);
        std::fs::rename(&self.part, &self.path).map_err(|error| fail(&error, &self.part))?;
        Ok(self.path)
    }

    /// Throws the unfinished ZIP away.
    pub fn abandon(self) {
        let part = self.part.clone();
        drop(self);
        let _ = std::fs::remove_file(part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_reads_back_as_written() {
        let list = SceneList::from_cuts(&[91, 114, 174], 732, 24.0);
        let text = csv_text(&list);
        assert!(text.starts_with("Timecode List:,00:00:03.792,00:00:04.750,00:00:07.250\n"));
        assert!(
            text.contains("\n1,1,00:00:00.000,0.000,91,00:00:03.792,3.792,91,00:00:03.792,3.792\n")
        );
        assert!(text.contains(
            "\n4,175,00:00:07.250,7.250,732,00:00:30.500,30.500,558,00:00:23.250,23.250\n"
        ));
        let read = read_csv(&text).unwrap();
        assert_eq!(read.starts, list.starts);
        assert_eq!(read.end, 732);
        assert!((read.fps - 24.0).abs() < 1e-9);
    }

    #[test]
    fn pyscenedetects_own_list_is_read() {
        // As `scenedetect list-scenes` 0.7.3 wrote it for a test clip.
        let text = "Timecode List:,00:00:04.004,00:00:10.052\n\
            Scene Number,Start Frame,Start Timecode,Start Time (seconds),End Frame,End Timecode,End Time (seconds),Length (frames),Length (timecode),Length (seconds)\n\
            1,1,00:00:00.000,0.000,96,00:00:04.004,4.004,96,00:00:04.004,4.004\n\
            2,97,00:00:04.004,4.004,241,00:00:10.052,10.052,145,00:00:06.048,6.048\n\
            3,242,00:00:10.052,10.052,731,00:00:30.489,30.489,490,00:00:20.437,20.437\n";
        let list = read_csv(text).unwrap();
        assert_eq!(list.starts, vec![0, 96, 241]);
        assert_eq!(list.end, 731);
        assert_eq!(list.scene(1), (96, 241));
        assert!((list.fps - 24000.0 / 1001.0).abs() < 0.01);
    }

    #[test]
    fn the_phone_apps_scan_is_cut_with_its_own_settings() {
        let dir = std::env::temp_dir().join(format!("concat-szs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let film = dir.join("Film.mkv");
        let szenen = dir.join("Film-szenen");
        std::fs::create_dir_all(&szenen).unwrap();
        // 25 fps; a hard cut at 40 and at 41 (merged), and one at 90.
        let mut data = vec![0u8; 128];
        data[..4].copy_from_slice(b"SZS1");
        data[8..12].copy_from_slice(&25i32.to_le_bytes());
        data[12..16].copy_from_slice(&1i32.to_le_bytes());
        for frame in 0..120u64 {
            let change: f32 = match frame {
                40 | 41 | 90 => 60.0,
                _ => 2.0,
            };
            data.extend_from_slice(&(frame as i64 * 40).to_le_bytes());
            for _ in 0..3 {
                data.extend_from_slice(&change.to_le_bytes());
            }
            data.extend_from_slice(&100f32.to_le_bytes());
        }
        std::fs::write(szenen.join(SCAN), &data).unwrap();
        let (path, list) = find_list(&film).unwrap();
        assert_eq!(path, szenen.join(SCAN));
        assert_eq!(list.starts, vec![0, 40, 90]);
        assert_eq!(list.end, 120);
        assert!((list.fps - 25.0).abs() < 1e-9);
        // Its project file says suppress, and a higher threshold.
        std::fs::write(
            szenen.join("szenenschnitt-handy.json"),
            r#"{"detect": {"mode": "content", "threshold": 80, "min_len": 0.6, "filter": "suppress"}}"#,
        )
        .unwrap();
        let (_, list) = find_list(&film).unwrap();
        assert_eq!(list.starts, vec![0]);
        // Its frames in bilder/ stand in for pictures.
        std::fs::create_dir_all(szenen.join("bilder")).unwrap();
        std::fs::write(szenen.join("bilder").join("f0000045.jpg"), b"pic").unwrap();
        let three = SceneList::from_cuts(&[40, 90], 120, 25.0);
        let pictures = Pictures::open(&film, &three).unwrap();
        assert!(!pictures.complete());
        assert_eq!(pictures.read(&three, 1, 2).as_deref(), Some(&b"pic"[..]));
        assert_eq!(pictures.read(&three, 0, 2), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timecodes_carry_into_the_next_minute() {
        assert_eq!(timecode(59.9996), "00:01:00.000");
        assert_eq!(timecode(3725.25), "01:02:05.250");
    }

    #[test]
    fn picture_names_read_back() {
        assert_eq!(picture_of(&picture_name("Film-x", 7, 2)), Some((7, 2)));
        assert_eq!(picture_of("Film-Scene-1184-03.jpg"), Some((1184, 3)));
        assert_eq!(picture_of("concat-szenen.json"), None);
    }

    #[test]
    fn pictures_are_found_only_for_their_own_list() {
        let dir = std::env::temp_dir().join(format!("concat-scenes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let film = dir.join("Film.mkv");
        let list = SceneList::from_cuts(&[10, 20], 30, 25.0);
        let mut writer = PictureWriter::create(&zip_beside(&film), &film).unwrap();
        writer.add(1, 2, b"one").unwrap();
        writer.add(3, 1, b"three").unwrap();
        let manifest = Manifest {
            fps: 25.0,
            frames: 30,
            cuts: vec![10, 20],
            threshold: Some(27.0),
            min_seconds: Some(0.6),
        };
        writer.finish(&manifest).unwrap();
        assert!(!part_of(&zip_beside(&film)).exists());
        let pictures = Pictures::open(&film, &list).unwrap();
        assert!(pictures.complete());
        assert_eq!(pictures.read(&list, 0, 2).as_deref(), Some(&b"one"[..]));
        assert_eq!(pictures.read(&list, 2, 1).as_deref(), Some(&b"three"[..]));
        assert_eq!(pictures.read(&list, 1, 2), None);
        let other = SceneList::from_cuts(&[10, 21], 30, 25.0);
        assert!(Pictures::open(&film, &other).is_none());
        // The list beside the film is the one found.
        write_csv(&csv_beside(&film), &list).unwrap();
        let (path, found) = find_list(&film).unwrap();
        assert_eq!(path, csv_beside(&film));
        assert_eq!(found.starts, list.starts);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
